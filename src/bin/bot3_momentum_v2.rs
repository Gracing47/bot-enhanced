use std::collections::VecDeque;
use std::env;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dotenvy::dotenv;
use ethers::prelude::*;
use eyre::Result;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::interval;
use tracing::{error, info, warn};

// ── ABIs ──────────────────────────────────────────────────────────────────────

abigen!(
    IERC20,
    r#"[
        function balanceOf(address owner) external view returns (uint256)
        function approve(address spender, uint256 amount) external returns (bool)
        function decimals() external view returns (uint8)
    ]"#
);

abigen!(
    IWFLR,
    r#"[
        function deposit() external payable
        function withdraw(uint256 amount) external
    ]"#
);

abigen!(
    IEnosysRouter,
    r#"[
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external returns (uint256[] amounts)
        function getAmountsOut(uint256 amountIn, address[] path) external view returns (uint256[] amounts)
    ]"#
);

abigen!(
    IFtsoV2,
    r#"[
        function getFeedById(bytes21 feedId) external view returns (uint256 value, int8 decimals, uint64 timestamp)
    ]"#
);

// ── Adressen Flare Mainnet (verifiziert) ──────────────────────────────────────

const WFLR_ADDR:     &str = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d";
const USDT0_ADDR:    &str = "0xe7cd86e13AC4309349F30B3435a9d337750fC82D";
const ENOSYS_V2:     &str = "0xe3A1b355ca63abCBC9589334B5e609583C7BAa06";
const FTSO_CONSUMER: &str = "0x7BDE3Df0624114eDB3A67dFe6753e62f4e7c1d20";

// FLR/USD FTSOv2 Feed ID (bytes21)
const FLR_FEED: [u8; 21] = [
    0x01,0x46,0x4c,0x52,0x2f,0x55,0x53,0x44,
    0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    0x00,0x00,0x00,0x00,0x00,
];

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Position {
    entry_price: f64,
    wflr_amount: U256,   // wie viel WFLR gekauft wurde
    usdt0_spent: U256,   // wie viel USDT0 ausgegeben
    entry_time:  u64,
}

#[derive(Default)]
struct TraderState {
    position:      Option<Position>,
    price_history: VecDeque<(u64, f64)>,  // (unix_secs, price)
    pnl_total:     f64,
    trades_total:  u32,
    trades_win:    u32,
    last_action:   String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_str() -> String {
    let ts = now_secs();
    // einfaches UTC-Format ohne externe Crate
    let s = ts % 86400;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    let days = ts / 86400;
    // 2026-01-01 = Tag 20454 seit Unix-Epoch
    let day_of_2026 = days.saturating_sub(20454);
    format!("2026-03-01+{}d {:02}:{:02}:{:02} UTC", day_of_2026, h, m, sec)
}

// ── EMA Berechnung ─────────────────────────────────────────────────────────────

fn ema_30min(history: &VecDeque<(u64, f64)>) -> Option<f64> {
    if history.len() < 10 {
        return None; // zu wenig Datenpunkte
    }
    let now = now_secs();
    let cutoff = now.saturating_sub(1800); // 30 Minuten
    let recent: Vec<f64> = history.iter()
        .filter(|(t, _)| *t >= cutoff)
        .map(|(_, p)| *p)
        .collect();
    if recent.is_empty() {
        return None;
    }
    // Einfacher Durchschnitt (SMA) als EMA-Näherung
    Some(recent.iter().sum::<f64>() / recent.len() as f64)
}

// ── FTSOv2 Preis holen ────────────────────────────────────────────────────────

async fn fetch_flr_price(provider: Arc<Provider<Http>>) -> Option<f64> {
    let ftso = IFtsoV2::new(FTSO_CONSUMER.parse::<Address>().ok()?, provider);
    match ftso.get_feed_by_id(FLR_FEED).call().await {
        Ok((value, decimals, _)) => {
            let d = decimals as i32;
            Some(value.as_u128() as f64 / 10f64.powi(d))
        }
        Err(e) => {
            warn!("FTSOv2 Fehler: {}", e);
            None
        }
    }
}

// ── DEX Quote ────────────────────────────────────────────────────────────────

async fn get_quote_wflr_per_usdt0(
    provider: Arc<Provider<Http>>,
    usdt0_amount: U256,
) -> Option<U256> {
    let router = IEnosysRouter::new(ENOSYS_V2.parse::<Address>().ok()?, provider);
    let path = vec![
        USDT0_ADDR.parse::<Address>().ok()?,
        WFLR_ADDR.parse::<Address>().ok()?,
    ];
    match router.get_amounts_out(usdt0_amount, path).call().await {
        Ok(amounts) if amounts.len() >= 2 => Some(amounts[1]),
        _ => None,
    }
}

// ── Trade: USDT0 → WFLR (buy FLR) ────────────────────────────────────────────

async fn buy_wflr(
    client: Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>,
    usdt0_amount: U256,
    min_wflr: U256,
    gas_price: U256,
) -> Result<Option<TxHash>> {
    let provider = client.provider().clone();
    let trader_addr = client.address();

    // 1. USDT0 approve
    let usdt0 = IERC20::new(USDT0_ADDR.parse::<Address>()?, provider.clone().into());
    let balance = usdt0.balance_of(trader_addr).call().await.unwrap_or_default();
    if balance < usdt0_amount {
        warn!("Nicht genug USDT0: {} < {}", balance, usdt0_amount);
        return Ok(None);
    }
    let approve_tx = usdt0.approve(ENOSYS_V2.parse::<Address>()?, usdt0_amount)
        .gas_price(gas_price);
    approve_tx.send().await?.await?;

    // 2. Swap USDT0 → WFLR
    let router = IEnosysRouter::new(ENOSYS_V2.parse::<Address>()?, provider.into());
    let path = vec![
        USDT0_ADDR.parse::<Address>()?,
        WFLR_ADDR.parse::<Address>()?,
    ];
    let deadline = U256::from(now_secs() + 120);
    let swap = router.swap_exact_tokens_for_tokens(
        usdt0_amount, min_wflr, path, trader_addr, deadline
    ).gas_price(gas_price);

    let receipt = swap.send().await?.await?;
    Ok(receipt.map(|r| r.transaction_hash))
}

// ── Trade: WFLR → USDT0 (sell FLR) ──────────────────────────────────────────

async fn sell_wflr(
    client: Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>,
    wflr_amount: U256,
    min_usdt0: U256,
    gas_price: U256,
) -> Result<Option<TxHash>> {
    let provider = client.provider().clone();
    let trader_addr = client.address();

    // 1. WFLR approve
    let wflr = IERC20::new(WFLR_ADDR.parse::<Address>()?, provider.clone().into());
    let approve_tx = wflr.approve(ENOSYS_V2.parse::<Address>()?, wflr_amount)
        .gas_price(gas_price);
    approve_tx.send().await?.await?;

    // 2. Swap WFLR → USDT0
    let router = IEnosysRouter::new(ENOSYS_V2.parse::<Address>()?, provider.into());
    let path = vec![
        WFLR_ADDR.parse::<Address>()?,
        USDT0_ADDR.parse::<Address>()?,
    ];
    let deadline = U256::from(now_secs() + 120);
    let swap = router.swap_exact_tokens_for_tokens(
        wflr_amount, min_usdt0, path, trader_addr, deadline
    ).gas_price(gas_price);

    let receipt = swap.send().await?.await?;
    Ok(receipt.map(|r| r.transaction_hash))
}

// ── Discord Webhook ───────────────────────────────────────────────────────────

async fn discord_post(webhook: &str, msg: &str) {
    let client = reqwest::Client::new();
    // Discord limit 2000 Zeichen
    let content = if msg.len() > 1900 { &msg[..1900] } else { msg };
    let _ = client.post(webhook)
        .json(&json!({"content": content}))
        .send().await;
}

// ── Preis-Tracking + Signal-Generierung ──────────────────────────────────────

async fn price_tracker(
    provider: Arc<Provider<Http>>,
    state: Arc<Mutex<TraderState>>,
    signal_tx: tokio::sync::watch::Sender<Option<TradeSignal>>,
    buy_threshold: f64,
    sell_threshold: f64,
) {
    let mut ticker = interval(Duration::from_secs(3));

    loop {
        ticker.tick().await;

        let price = match fetch_flr_price(provider.clone()).await {
            Some(p) if p > 0.0 => p,
            _ => continue,
        };

        let mut s = state.lock().await;
        let ts = now_secs();
        s.price_history.push_back((ts, price));

        // Rolling window: nur letzte 60 Minuten behalten
        let cutoff = ts.saturating_sub(3600);
        while s.price_history.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            s.price_history.pop_front();
        }

        let ema = match ema_30min(&s.price_history) {
            Some(e) => e,
            None => continue,
        };

        let deviation = (price - ema) / ema;
        let has_position = s.position.is_some();
        drop(s);

        let signal = if !has_position && deviation < -buy_threshold {
            // Kein Trade aktiv + Preis zu weit gefallen → BUY
            info!("BUY Signal: FLR=${:.6} EMA=${:.6} dev={:.2}%",
                price, ema, deviation * 100.0);
            Some(TradeSignal::Buy { price, ema })
        } else if has_position && deviation > sell_threshold {
            // Position offen + Preis gestiegen → SELL (Profit)
            info!("SELL Signal (profit): FLR=${:.6} EMA=${:.6} dev={:.2}%",
                price, ema, deviation * 100.0);
            Some(TradeSignal::Sell { price, ema, reason: "profit" })
        } else {
            None
        };

        if signal.is_some() {
            let _ = signal_tx.send(signal);
        }
    }
}

#[derive(Debug, Clone)]
enum TradeSignal {
    Buy  { price: f64, ema: f64 },
    Sell { price: f64, ema: f64, reason: &'static str },
}

// ── Stop-Loss Monitor ─────────────────────────────────────────────────────────

async fn stop_loss_monitor(
    state: Arc<Mutex<TraderState>>,
    signal_tx: tokio::sync::watch::Sender<Option<TradeSignal>>,
    stop_loss_pct: f64,
) {
    let mut ticker = interval(Duration::from_secs(5));

    loop {
        ticker.tick().await;

        let s = state.lock().await;
        if let Some(ref pos) = s.position {
            // aktueller Preis aus history
            if let Some((_, current_price)) = s.price_history.back().copied() {
                let loss = (current_price - pos.entry_price) / pos.entry_price;
                if loss < -stop_loss_pct {
                    warn!("STOP-LOSS getriggert: entry=${:.6} current=${:.6} loss={:.2}%",
                        pos.entry_price, current_price, loss * 100.0);
                    drop(s);
                    let _ = signal_tx.send(Some(TradeSignal::Sell {
                        price: current_price,
                        ema: current_price,
                        reason: "stop-loss",
                    }));
                }
            }
        }
    }
}

// ── Trade Executor ────────────────────────────────────────────────────────────

async fn trade_executor(
    client: Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>,
    state: Arc<Mutex<TraderState>>,
    mut signal_rx: tokio::sync::watch::Receiver<Option<TradeSignal>>,
    position_size_usdt0: U256,  // USDT0 pro Trade (6 decimals)
    webhook: String,
) {
    loop {
        // Warte auf Signal-Änderung
        if signal_rx.changed().await.is_err() {
            break;
        }

        let signal = signal_rx.borrow().clone();
        let signal = match signal {
            Some(s) => s,
            None => continue,
        };

        // Dynamisches Gas
        let gas_price = match client.provider().get_gas_price().await {
            Ok(g) => g * 150 / 100,  // +50% Surcharge
            Err(_) => U256::from(25_000_000_000u64),
        };

        match signal {
            TradeSignal::Buy { price, ema } => {
                // USDT0 Balance prüfen
                let usdt0 = IERC20::new(
                    USDT0_ADDR.parse::<Address>().unwrap(),
                    client.provider().clone().into(),
                );
                let balance = usdt0.balance_of(client.address()).call().await
                    .unwrap_or_default();

                if balance < position_size_usdt0 {
                    warn!("Nicht genug USDT0 für Trade: {} < {}", balance, position_size_usdt0);
                    continue;
                }

                // Quote holen (mit 2% Slippage) — braucht Arc<Provider<Http>>
                let raw_provider: Arc<Provider<Http>> = client.inner().clone();
                let expected_wflr = match get_quote_wflr_per_usdt0(
                    raw_provider, position_size_usdt0
                ).await {
                    Some(q) => q,
                    None => continue,
                };
                let min_wflr = expected_wflr * 98 / 100;

                info!("BUY: {} USDT0 → ≥{} WFLR @ FLR=${:.6}",
                    position_size_usdt0, min_wflr, price);

                match buy_wflr(client.clone(), position_size_usdt0, min_wflr, gas_price).await {
                    Ok(Some(tx)) => {
                        info!("BUY TX: {:?}", tx);
                        let msg = format!(
                            "🟢 **BOT3 MOMENTUM BUY**\nFLR=${:.6} (EMA=${:.6})\nUSDF0 spent: {:.2}\nTX: `{:?}`",
                            price, ema,
                            position_size_usdt0.as_u128() as f64 / 1e6,
                            tx
                        );
                        discord_post(&webhook, &msg).await;

                        let mut s = state.lock().await;
                        s.position = Some(Position {
                            entry_price: price,
                            wflr_amount: expected_wflr,
                            usdt0_spent: position_size_usdt0,
                            entry_time: now_secs(),
                        });
                        s.last_action = format!("BUY @ ${:.6}", price);
                    }
                    Ok(None) => warn!("BUY TX: kein Receipt"),
                    Err(e) => error!("BUY fehlgeschlagen: {}", e),
                }
            }

            TradeSignal::Sell { price, ema: _, reason } => {
                let pos = {
                    let s = state.lock().await;
                    match s.position.clone() {
                        Some(p) => p,
                        None => continue,
                    }
                };

                // Aktuelles WFLR-Balance (könnte durch Fees minimal anders sein)
                let wflr_token = IERC20::new(
                    WFLR_ADDR.parse::<Address>().unwrap(),
                    client.provider().clone().into(),
                );
                let wflr_balance = wflr_token.balance_of(client.address()).call().await
                    .unwrap_or(pos.wflr_amount);

                // Quote WFLR → USDT0
                let path = vec![
                    WFLR_ADDR.parse::<Address>().unwrap(),
                    USDT0_ADDR.parse::<Address>().unwrap(),
                ];
                let router = IEnosysRouter::new(
                    ENOSYS_V2.parse::<Address>().unwrap(),
                    client.provider().clone().into(),
                );
                let expected_usdt0 = match router.get_amounts_out(wflr_balance, path).call().await {
                    Ok(amounts) if amounts.len() >= 2 => amounts[1],
                    _ => continue,
                };
                let min_usdt0 = expected_usdt0 * 98 / 100;

                let pnl_usdt0 = expected_usdt0.as_u128() as i128 - pos.usdt0_spent.as_u128() as i128;
                let pnl_usd = pnl_usdt0 as f64 / 1e6;

                info!("SELL ({}): {} WFLR → ≥{} USDT0 | PnL={:.4}$",
                    reason, wflr_balance, min_usdt0, pnl_usd);

                match sell_wflr(client.clone(), wflr_balance, min_usdt0, gas_price).await {
                    Ok(Some(tx)) => {
                        info!("SELL TX: {:?}", tx);
                        let emoji = if pnl_usd >= 0.0 { "✅" } else { "🔴" };
                        let msg = format!(
                            "{} **BOT3 MOMENTUM SELL** ({})\nFLR=${:.6}\nPnL: **{:+.4}$**\nTX: `{:?}`",
                            emoji, reason, price, pnl_usd, tx
                        );
                        discord_post(&webhook, &msg).await;

                        let mut s = state.lock().await;
                        s.position = None;
                        s.pnl_total += pnl_usd;
                        s.trades_total += 1;
                        if pnl_usd > 0.0 { s.trades_win += 1; }
                        s.last_action = format!("SELL ({}) @ ${:.6} PnL={:+.4}$", reason, price, pnl_usd);
                    }
                    Ok(None) => warn!("SELL TX: kein Receipt"),
                    Err(e) => error!("SELL fehlgeschlagen: {}", e),
                }
            }
        }
    }
}

// ── Heartbeat ─────────────────────────────────────────────────────────────────

async fn heartbeat(
    client: Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>,
    state: Arc<Mutex<TraderState>>,
    webhook: String,
    interval_secs: u64,
) {
    let mut ticker = interval(Duration::from_secs(interval_secs));
    ticker.tick().await;

    loop {
        ticker.tick().await;

        let s = state.lock().await;
        let current_price = s.price_history.back().map(|(_, p)| *p).unwrap_or(0.0);
        let ema = ema_30min(&s.price_history).unwrap_or(0.0);
        let pnl = s.pnl_total;
        let trades = s.trades_total;
        let wins = s.trades_win;
        let last_action = s.last_action.clone();
        let position_info = match &s.position {
            Some(pos) => {
                let unrealized = (current_price - pos.entry_price) / pos.entry_price * 100.0;
                let held_secs = now_secs().saturating_sub(pos.entry_time);
                format!("📊 **Position OFFEN**\n  Entry: ${:.6} | Aktuell: ${:.6}\n  Unrealized: {:+.2}% | Gehalten: {}min\n",
                    pos.entry_price, current_price, unrealized, held_secs / 60)
            }
            None => "💤 Keine offene Position\n".to_string(),
        };
        drop(s);

        // Wallet Balances
        let trader_addr = client.address();
        let wflr = IERC20::new(WFLR_ADDR.parse::<Address>().unwrap(), client.provider().clone().into());
        let usdt0 = IERC20::new(USDT0_ADDR.parse::<Address>().unwrap(), client.provider().clone().into());
        let wflr_bal = wflr.balance_of(trader_addr).call().await.unwrap_or_default();
        let usdt0_bal = usdt0.balance_of(trader_addr).call().await.unwrap_or_default();
        let wflr_f = wflr_bal.as_u128() as f64 / 1e18;
        let usdt0_f = usdt0_bal.as_u128() as f64 / 1e6;
        let win_rate = if trades > 0 { wins * 100 / trades } else { 0 };

        let msg = format!(
            "📈 **BOT3 MOMENTUM STATUS** — {}\n══════════════════════════════\n💰 Wallet `{:#x}…`\n  WFLR: {:.2} | USDT0: ${:.2}\n\n🎯 FLR=${:.6} | EMA30={:.6}\n\n{}\n📊 **Stats:** {} Trades | {}% Win-Rate | PnL: **{:+.4}$**\n🔔 Letzte Aktion: {}\n══════════════════════════════",
            now_str(), trader_addr, wflr_f, usdt0_f,
            current_price, ema,
            position_info,
            trades, win_rate, pnl,
            last_action
        );

        discord_post(&webhook, &msg).await;
        info!("Heartbeat gesendet | FLR=${:.6} | PnL={:+.4}$", current_price, pnl);
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("bot3_momentum=info".parse()?)
        )
        .init();

    info!("Bot3 Momentum v2.0 — FTSOv2 Momentum Trading (FLR/USDT0)");

    let rpc_url    = env::var("RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:9650/ext/bc/C/rpc".to_string());
    let chain_id: u64 = env::var("CHAIN_ID").unwrap_or_else(|_| "14".to_string()).parse().unwrap_or(14);
    let pks_str    = env::var("PRIVATE_KEYS").expect("PRIVATE_KEYS fehlt");
    let webhook    = env::var("BOT3_BOT3_DISCORD_WEBHOOK_URL").unwrap_or_else(|_|
        env::var("BOT3_DISCORD_WEBHOOK_URL").expect("BOT3_DISCORD_WEBHOOK_URL fehlt")
    );

    // Wallet-Index (0-basiert, default=3 für Bot 4)
    let wallet_idx: usize = env::var("BOT3_WALLET_INDEX")
        .unwrap_or_else(|_| "3".to_string())
        .parse().unwrap_or(3);

    // Position-Size in USDT0 (6 decimals), default 5 USDT0
    let position_size_usdt0_raw: u64 = env::var("BOT3_POSITION_SIZE_USDT0")
        .unwrap_or_else(|_| "5000000".to_string())  // 5 USDT0
        .parse().unwrap_or(5_000_000);
    let position_size_usdt0 = U256::from(position_size_usdt0_raw);

    // Buy-Signal: 3% unter EMA, Sell-Signal: 3% über EMA
    let buy_threshold: f64 = env::var("BOT3_BUY_THRESHOLD")
        .unwrap_or_else(|_| "0.03".to_string()).parse().unwrap_or(0.03);
    let sell_threshold: f64 = env::var("BOT3_SELL_THRESHOLD")
        .unwrap_or_else(|_| "0.03".to_string()).parse().unwrap_or(0.03);
    let stop_loss_pct: f64 = env::var("BOT3_STOP_LOSS_PCT")
        .unwrap_or_else(|_| "0.05".to_string()).parse().unwrap_or(0.05);

    let heartbeat_secs: u64 = env::var("MONITOR_HEARTBEAT_SECS")
        .unwrap_or_else(|_| "600".to_string()).parse().unwrap_or(600);

    // Provider
    let provider = Arc::new(Provider::<Http>::try_from(rpc_url.as_str())?);

    // Wallet aus PRIVATE_KEYS (Index)
    let keys: Vec<&str> = pks_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if wallet_idx >= keys.len() {
        eyre::bail!("BOT3_WALLET_INDEX={} aber nur {} Keys in PRIVATE_KEYS", wallet_idx, keys.len());
    }
    let wallet = keys[wallet_idx].parse::<LocalWallet>()?.with_chain_id(chain_id);
    let trader_addr = wallet.address();
    info!("Trader Wallet: {:#x}", trader_addr);

    let client = Arc::new(SignerMiddleware::new(provider.clone(), wallet));

    // Initialer Balance-Check
    let usdt0_token = IERC20::new(USDT0_ADDR.parse::<Address>()?, Arc::clone(&provider));
    let wflr_token  = IERC20::new(WFLR_ADDR.parse::<Address>()?,  Arc::clone(&provider));
    let usdt0_bal = usdt0_token.balance_of(trader_addr).call().await.unwrap_or_default();
    let wflr_bal  = wflr_token.balance_of(trader_addr).call().await.unwrap_or_default();
    info!("Balance: WFLR={:.4} | USDT0={:.4}",
        wflr_bal.as_u128() as f64 / 1e18,
        usdt0_bal.as_u128() as f64 / 1e6);

    if usdt0_bal < position_size_usdt0 {
        warn!("Warnung: USDT0 Balance ({:.4}) kleiner als Position Size ({:.4})",
            usdt0_bal.as_u128() as f64 / 1e6,
            position_size_usdt0.as_u128() as f64 / 1e6);
        warn!("Falls Bot4 nur FLR hat: FLR zuerst auf Enosys zu USDT0 swappen oder BOT3_WALLET_INDEX anpassen!");
    }

    // Shared State
    let state = Arc::new(Mutex::new(TraderState {
        last_action: "Start".to_string(),
        ..Default::default()
    }));

    // Signal-Kanal
    let (signal_tx, signal_rx) = tokio::sync::watch::channel(None::<TradeSignal>);

    info!("Strategie: Buy wenn FLR {:.1}% unter EMA30 | Sell wenn {:.1}% über EMA30 | Stop-Loss {:.1}%",
        buy_threshold * 100.0, sell_threshold * 100.0, stop_loss_pct * 100.0);
    info!("Position Size: {:.2} USDT0 | Heartbeat: {}s", position_size_usdt0_raw as f64 / 1e6, heartbeat_secs);

    // Startup Discord Nachricht
    discord_post(&webhook, &format!(
        "🚀 **KOJO-TRADER v1.0 gestartet**\nWallet: `{:#x}`\nStrategie: FLR/USDT0 Momentum (FTSOv2 1.8s Signal)\nBuy: -{:.1}% EMA | Sell: +{:.1}% EMA | Stop-Loss: -{:.1}%\nPosition Size: {:.2} USDT0",
        trader_addr, buy_threshold * 100.0, sell_threshold * 100.0, stop_loss_pct * 100.0,
        position_size_usdt0_raw as f64 / 1e6
    )).await;

    // Tasks starten
    let state1 = state.clone();
    let state2 = state.clone();
    let state3 = state.clone();
    let state4 = state.clone();
    let provider1 = provider.clone();
    let signal_tx2 = signal_tx.clone();
    let client2 = client.clone();
    let webhook2 = webhook.clone();

    tokio::select! {
        _ = tokio::spawn(async move {
            price_tracker(provider1, state1, signal_tx, buy_threshold, sell_threshold).await;
        }) => {}
        _ = tokio::spawn(async move {
            stop_loss_monitor(state2, signal_tx2, stop_loss_pct).await;
        }) => {}
        _ = tokio::spawn(async move {
            trade_executor(client2, state3, signal_rx, position_size_usdt0, webhook2).await;
        }) => {}
        _ = tokio::spawn(async move {
            heartbeat(client, state4, webhook, heartbeat_secs).await;
        }) => {}
    }

    Ok(())
}
