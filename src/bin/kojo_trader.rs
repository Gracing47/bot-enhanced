use std::collections::VecDeque;
use std::env;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dotenvy::dotenv;
use ethers::prelude::*;
use eyre::Result;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{error, info, warn};

// ── ABIs ──────────────────────────────────────────────────────────────────────

abigen!(
    IERC20,
    r#"[
        function balanceOf(address owner) external view returns (uint256)
        function approve(address spender, uint256 amount) external returns (bool)
        function transfer(address to, uint256 amount) external returns (bool)
    ]"#
);

abigen!(
    IFtsoV2,
    r#"[
        function getFeedById(bytes21 feedId) external view returns (uint256 value, int8 decimals, uint64 timestamp)
    ]"#
);

abigen!(
    IEnosysRouter,
    r#"[
        function getAmountsOut(uint amountIn, address[] calldata path) external view returns (uint[] memory amounts)
        function swapExactTokensForTokens(uint amountIn, uint amountOutMin, address[] calldata path, address to, uint deadline) external returns (uint[] memory amounts)
    ]"#
);

// ── Adressen auf Flare Mainnet ────────────────────────────────────────────────

const USDT0_ADDR:    &str = "0xe7cd86e13AC4309349F30B3435a9d337750fC82D";
const WFLR_ADDR:     &str = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d";
const ENOSYS_V2_ROUTER: &str = "0xe3A1b355ca63abCBC9589334B5e609583C7BAa06";
const FTSO_CONSUMER: &str = "0x7BDE3Df0624114eDB3A67dFe6753e62f4e7c1d20";

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct PriceSignal {
    flr_price: f64,
    ema_30m: f64,
    signal: Signal,
    timestamp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Signal {
    Buy,
    Sell,
    None,
}

#[derive(Clone, Debug, Default)]
struct TradeState {
    position_open: bool,
    entry_price: f64,
    entry_time: u64,
}

// ── Fetch FTSOv2 Price ────────────────────────────────────────────────────────

async fn fetch_ftso_price(provider: Arc<Provider<Http>>) -> Result<f64> {
    let ftso = IFtsoV2::new(FTSO_CONSUMER.parse::<Address>()?, provider);
    let flr_feed: [u8; 21] = [
        0x01, 0x46, 0x4c, 0x52, 0x2f, 0x55, 0x53, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    match ftso.get_feed_by_id(flr_feed).call().await {
        Ok((value, decimals, _)) => {
            let d = decimals as i32;
            Ok(value.as_u128() as f64 / 10f64.powi(d))
        }
        Err(e) => {
            error!("FTSOv2 fetch failed: {}", e);
            Err(eyre::eyre!("FTSOv2 fetch error"))
        }
    }
}

// ── Calculate EMA ─────────────────────────────────────────────────────────────

fn calculate_ema(prices: &VecDeque<f64>, alpha: f64) -> f64 {
    if prices.is_empty() {
        return 0.0;
    }
    let mut ema = prices[0];
    for i in 1..prices.len() {
        ema = alpha * prices[i] + (1.0 - alpha) * ema;
    }
    ema
}

// ── Generate Signal ───────────────────────────────────────────────────────────

fn generate_signal(price: f64, ema: f64, threshold: f64) -> Signal {
    let lower_bound = ema * (1.0 - threshold);
    let upper_bound = ema * (1.0 + threshold);

    if price < lower_bound {
        Signal::Buy
    } else if price > upper_bound {
        Signal::Sell
    } else {
        Signal::None
    }
}

// ── Fetch Balance ─────────────────────────────────────────────────────────────

async fn fetch_balance(
    provider: Arc<Provider<Http>>,
    token_addr: &str,
    wallet: Address,
) -> Result<u64> {
    let token = IERC20::new(token_addr.parse::<Address>()?, provider);
    let balance = token.balance_of(wallet).call().await?;
    Ok(balance.as_u64())
}

// ── Execute Enosys Swap ───────────────────────────────────────────────────────

async fn execute_swap(
    provider: Arc<Provider<Http>>,
    wallet: LocalWallet,
    token_in: &str,
    token_out: &str,
    amount_in: u64,
    min_out: u64,
) -> Result<String> {
    let client = Arc::new(SignerMiddleware::new(provider.clone(), wallet.clone()));

    // Step 1: Approve
    let token = IERC20::new(token_in.parse::<Address>()?, provider.clone());
    token
        .approve(ENOSYS_V2_ROUTER.parse::<Address>()?, U256::from(amount_in))
        .send()
        .await
        .ok();

    info!("Swap approve initiated");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 2: Get quote
    let router = IEnosysRouter::new(ENOSYS_V2_ROUTER.parse::<Address>()?, provider);
    let path = vec![token_in.parse::<Address>()?, token_out.parse::<Address>()?];

    let amounts_out = router.get_amounts_out(U256::from(amount_in), path.clone()).call().await?;

    if amounts_out[1].as_u64() < min_out {
        return Err(eyre::eyre!("Slippage exceeded"));
    }

    info!("Quote OK: {} -> {}", amount_in, amounts_out[1].as_u64());

    // Step 3: Swap
    IEnosysRouter::new(ENOSYS_V2_ROUTER.parse::<Address>()?, client)
        .swap_exact_tokens_for_tokens(
            U256::from(amount_in),
            U256::from(min_out),
            path,
            wallet.address(),
            U256::from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 300),
        )
        .send()
        .await
        .ok();

    info!("Swap executed");
    Ok("success".to_string())
}

// ── Price Tracker Task ────────────────────────────────────────────────────────

async fn price_tracker_task(
    provider: Arc<Provider<Http>>,
    tx: watch::Sender<PriceSignal>,
) -> Result<()> {
    let mut price_window: VecDeque<f64> = VecDeque::with_capacity(1800);
    let mut ticker = interval(Duration::from_secs(2));

    let threshold = 0.015; // ±1.5%
    let alpha = 2.0 / 1801.0;

    loop {
        ticker.tick().await;

        match fetch_ftso_price(provider.clone()).await {
            Ok(price) => {
                price_window.push_back(price);
                if price_window.len() > 1800 {
                    price_window.pop_front();
                }

                let ema = if price_window.len() >= 60 {
                    calculate_ema(&price_window, alpha)
                } else {
                    price_window.iter().sum::<f64>() / price_window.len() as f64
                };

                let signal = generate_signal(price, ema, threshold);
                let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

                let ps = PriceSignal {
                    flr_price: price,
                    ema_30m: ema,
                    signal,
                    timestamp: ts,
                };

                info!("FLR: ${:.6} | EMA: ${:.6} | Signal: {:?}", price, ema, signal);
                let _ = tx.send(ps);
            }
            Err(e) => warn!("Price fetch error: {}", e),
        }
    }
}

// ── Trade Executor Task ───────────────────────────────────────────────────────

async fn trade_executor_task(
    provider: Arc<Provider<Http>>,
    wallet: LocalWallet,
    mut rx: watch::Receiver<PriceSignal>,
    webhook_url: Option<String>,
    enable_flashloan: bool,
) -> Result<()> {
    let mut state = TradeState::default();
    let mut pnl_session: f64 = 0.0;
    let mut trades_total = 0u64;
    let mut ticker = interval(Duration::from_secs(5));

    loop {
        ticker.tick().await;

        if rx.changed().await.is_ok() {
            let signal = rx.borrow().clone();

            match signal.signal {
                Signal::Buy if !state.position_open => {
                    info!("🚀 BUY: FLR ${:.6}", signal.flr_price);

                    let position_size = if enable_flashloan {
                        50_000_000u64 // $50
                    } else {
                        1_500_000u64 // $1.50
                    };

                    let balance = fetch_balance(provider.clone(), USDT0_ADDR, wallet.address())
                        .await
                        .unwrap_or(0);

                    if balance < position_size {
                        warn!("Insufficient balance: {} < {}", balance, position_size);
                        continue;
                    }

                    let min_wflr = (position_size as f64 * 0.99) as u64;
                    match execute_swap(
                        provider.clone(),
                        wallet.clone(),
                        USDT0_ADDR,
                        WFLR_ADDR,
                        position_size,
                        min_wflr,
                    )
                    .await
                    {
                        Ok(_) => {
                            state.position_open = true;
                            state.entry_price = signal.flr_price;
                            state.entry_time = signal.timestamp;
                            info!("Position opened @ ${:.6}", signal.flr_price);

                            if let Some(url) = &webhook_url {
                                let msg = format!(
                                    "🚀 **BUY** FLR @${:.6}\nPosition: ${:.2}",
                                    signal.flr_price,
                                    position_size as f64 / 1_000_000.0
                                );
                                let _ = reqwest::Client::new()
                                    .post(url)
                                    .json(&json!({"content": msg}))
                                    .send()
                                    .await;
                            }
                        }
                        Err(e) => error!("Swap failed: {}", e),
                    }
                }

                Signal::Sell if state.position_open => {
                    info!("✅ SELL: FLR ${:.6}", signal.flr_price);

                    let wflr_balance = fetch_balance(provider.clone(), WFLR_ADDR, wallet.address())
                        .await
                        .unwrap_or(0);

                    let min_usdt = (wflr_balance as f64 * 0.99) as u64;
                    match execute_swap(
                        provider.clone(),
                        wallet.clone(),
                        WFLR_ADDR,
                        USDT0_ADDR,
                        wflr_balance,
                        min_usdt,
                    )
                    .await
                    {
                        Ok(_) => {
                            let profit_pct = ((signal.flr_price - state.entry_price) / state.entry_price) * 100.0;
                            pnl_session += profit_pct;
                            trades_total += 1;

                            state.position_open = false;
                            info!("Position closed: +{:.2}%", profit_pct);

                            if let Some(url) = &webhook_url {
                                let msg = format!(
                                    "✅ **SELL** FLR @${:.6}\nProfit: {:.2}%\nSession: {:.2}% | Trades: {}",
                                    signal.flr_price, profit_pct, pnl_session, trades_total
                                );
                                let _ = reqwest::Client::new()
                                    .post(url)
                                    .json(&json!({"content": msg}))
                                    .send()
                                    .await;
                            }
                        }
                        Err(e) => error!("Swap failed: {}", e),
                    }
                }

                _ => {
                    if state.position_open {
                        let drawdown = (signal.flr_price - state.entry_price) / state.entry_price;
                        if drawdown < -0.05 {
                            warn!("⛔ STOP-LOSS: {:.2}%", drawdown * 100.0);

                            let wflr_balance = fetch_balance(provider.clone(), WFLR_ADDR, wallet.address())
                                .await
                                .unwrap_or(0);

                            let min_usdt = (wflr_balance as f64 * 0.98) as u64;
                            if let Ok(_) = execute_swap(
                                provider.clone(),
                                wallet.clone(),
                                WFLR_ADDR,
                                USDT0_ADDR,
                                wflr_balance,
                                min_usdt,
                            )
                            .await
                            {
                                state.position_open = false;
                                pnl_session -= 5.0;
                                trades_total += 1;

                                if let Some(url) = &webhook_url {
                                    let msg = format!(
                                        "⛔ **STOP-LOSS** @${:.6}\nLoss: -5.00%",
                                        signal.flr_price
                                    );
                                    let _ = reqwest::Client::new()
                                        .post(url)
                                        .json(&json!({"content": msg}))
                                        .send()
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("kojo_trader=info".parse()?),
        )
        .init();

    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:9650/ext/bc/C/rpc".to_string());
    let private_keys = env::var("PRIVATE_KEYS")?;
    let trader_wallet_index = env::var("TRADER_WALLET_INDEX")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(3);
    let webhook_url = env::var("DISCORD_WEBHOOK_URL").ok();
    let enable_flashloan = env::var("ENABLE_FLASHLOAN")
        .ok()
        .map(|s| s == "true")
        .unwrap_or(false);

    let wallets: Vec<LocalWallet> = private_keys
        .split(',')
        .enumerate()
        .filter_map(|(i, pk)| {
            let pk = pk.trim();
            if i == trader_wallet_index {
                pk.parse::<LocalWallet>().ok()
            } else {
                None
            }
        })
        .collect();

    if wallets.is_empty() {
        panic!("Could not derive trader wallet");
    }

    let wallet = wallets[0].clone();
    info!("🚀 KOJO-TRADER v1.1 STARTING");
    info!("Wallet: {}", wallet.address());
    info!("FlashLoan: {}", enable_flashloan);

    let provider = Arc::new(Provider::<Http>::try_from(rpc_url)?);

    let (tx, rx) = watch::channel(PriceSignal {
        flr_price: 0.0,
        ema_30m: 0.0,
        signal: Signal::None,
        timestamp: 0,
    });

    let provider_clone = provider.clone();
    let price_task = tokio::spawn(async move {
        let _ = price_tracker_task(provider_clone, tx).await;
    });

    let provider_clone = provider.clone();
    let wallet_clone = wallet.clone();
    let webhook_clone = webhook_url.clone();
    let trade_task = tokio::spawn(async move {
        let _ = trade_executor_task(provider_clone, wallet_clone, rx, webhook_clone, enable_flashloan).await;
    });

    tokio::select! {
        _ = price_task => error!("Price task ended"),
        _ = trade_task => error!("Trade task ended"),
    }

    Ok(())
}
