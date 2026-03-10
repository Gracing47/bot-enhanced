use std::collections::HashMap;
use std::env;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dotenvy::dotenv;
use ethers::prelude::*;
use eyre::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures::{SinkExt, StreamExt};
use tracing::{error, info, warn};

// ── ABIs ──────────────────────────────────────────────────────────────────────

abigen!(
    IERC20,
    r#"[
        function balanceOf(address owner) external view returns (uint256)
        function decimals() external view returns (uint8)
    ]"#
);

abigen!(
    IFtsoV2,
    r#"[
        function getFeedById(bytes21 feedId) external view returns (uint256 value, int8 decimals, uint64 timestamp)
    ]"#
);

// ── Adressen auf Flare Mainnet ────────────────────────────────────────────────

const USDCE_ADDR:  &str = "0xFbDa5F676cB37624f28265A144A48B0d6e87d3b6"; // 6 dec
const USDT0_ADDR:  &str = "0xe7cd86e13AC4309349F30B3435a9d337750fC82D"; // 6 dec
const FXRP_ADDR:   &str = "0xAd552A648C74D49E10027AB8a618A3ad4901c5bE";  // 6 dec
const WFLR_ADDR:   &str = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d"; // 18 dec
const FTSO_CONSUMER: &str = "0x7BDE3Df0624114eDB3A67dFe6753e62f4e7c1d20";

// ── Shared State ──────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct BotState {
    // Wallet balances: addr -> (flr_native, usdce, usdt0, fxrp, wflr)
    balances: HashMap<String, WalletBalance>,
    baseline: HashMap<String, WalletBalance>,   // Snapshot bei Start
    baseline_set: bool,
    pnl_session_usd: f64,                        // seit Start
    trades_total: u64,
    last_trades: Vec<TradeEvent>,                // max 5
    near_misses_kinetic: Vec<NearMiss>,          // max 10
    enosys_fxrp_troves: u32,
    enosys_wflr_troves: u32,
    morpho_borrowers: u32,
    kinetic_borrowers: u32,
    flr_price: f64,
    xrp_price: f64,
    last_error: Option<String>,
}

#[derive(Default, Clone)]
struct WalletBalance {
    flr_native:  f64,
    usdce:       f64,
    usdt0:       f64,
    fxrp:        f64,
    wflr:        f64,
}

impl WalletBalance {
    fn total_usd(&self, flr_price: f64, xrp_price: f64) -> f64 {
        self.usdce
            + self.usdt0
            + self.fxrp * xrp_price
            + (self.flr_native + self.wflr) * flr_price
    }
}

#[derive(Clone)]
struct TradeEvent {
    timestamp: String,
    bot: String,
    profit_usd: f64,
    tx_hash: String,
}

#[derive(Clone)]
struct NearMiss {
    borrower: String,
    shortfall_usd: f64,
    borrow_balance: f64,
}

type State = Arc<Mutex<BotState>>;

// ── Wallet-Adressen aus PRIVATE_KEYS ableiten ─────────────────────────────────

fn derive_addresses(pks_str: &str) -> Vec<String> {
    pks_str.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|key| key.parse::<LocalWallet>().ok())
        .map(|w| format!("{:#x}", w.address()))
        .collect()
}

// ── RPC: Preise via FTSOv2 ───────────────────────────────────────────────────

async fn fetch_prices(provider: Arc<Provider<Http>>) -> (f64, f64) {
    let ftso = IFtsoV2::new(FTSO_CONSUMER.parse::<Address>().unwrap(), provider);

    // FLR/USD feed ID: 0x01464c522f55534400000000000000000000000000 (bytes21)
    let flr_feed: [u8; 21] = [0x01,0x46,0x4c,0x52,0x2f,0x55,0x53,0x44,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00];
    // XRP/USD feed ID: 0x015852502f55534400000000000000000000000000
    let xrp_feed: [u8; 21] = [0x01,0x58,0x52,0x50,0x2f,0x55,0x53,0x44,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00];

    let flr_price = match ftso.get_feed_by_id(flr_feed).call().await {
        Ok((value, decimals, _)) => {
            let d = decimals as i32;
            value.as_u128() as f64 / 10f64.powi(d)
        }
        Err(_) => 0.0,
    };

    let xrp_price = match ftso.get_feed_by_id(xrp_feed).call().await {
        Ok((value, decimals, _)) => {
            let d = decimals as i32;
            value.as_u128() as f64 / 10f64.powi(d)
        }
        Err(_) => 0.0,
    };

    (flr_price, xrp_price)
}

// ── RPC: Wallet Balances ──────────────────────────────────────────────────────

async fn fetch_balances(
    addrs: &[String],
    provider: Arc<Provider<Http>>,
    flr_price: f64,
    xrp_price: f64,
) -> HashMap<String, WalletBalance> {
    let mut result = HashMap::new();

    let usdce = IERC20::new(USDCE_ADDR.parse::<Address>().unwrap(), provider.clone());
    let usdt0 = IERC20::new(USDT0_ADDR.parse::<Address>().unwrap(), provider.clone());
    let fxrp  = IERC20::new(FXRP_ADDR.parse::<Address>().unwrap(),  provider.clone());
    let wflr  = IERC20::new(WFLR_ADDR.parse::<Address>().unwrap(),  provider.clone());

    for addr_str in addrs {
        let addr: Address = match addr_str.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };

        let flr_raw = provider.get_balance(addr, None).await.unwrap_or_default();
        let usdce_raw = usdce.balance_of(addr).call().await.unwrap_or_default();
        let usdt0_raw = usdt0.balance_of(addr).call().await.unwrap_or_default();
        let fxrp_raw  = fxrp.balance_of(addr).call().await.unwrap_or_default();
        let wflr_raw  = wflr.balance_of(addr).call().await.unwrap_or_default();

        let bal = WalletBalance {
            flr_native: format_units(flr_raw, 18),
            usdce:      format_units(usdce_raw, 6),
            usdt0:      format_units(usdt0_raw, 6),
            fxrp:       format_units(fxrp_raw, 6),
            wflr:       format_units(wflr_raw, 18),
        };

        result.insert(addr_str.clone(), bal);
    }

    result
}

fn format_units(raw: U256, decimals: u32) -> f64 {
    let divisor = 10f64.powi(decimals as i32);
    raw.as_u128() as f64 / divisor
}

// ── Log-Parser: journalctl --follow ──────────────────────────────────────────

async fn log_watcher(state: State, webhook_url: String) {
    let mut cmd = Command::new("journalctl")
        .args([
            "-fu", "kojo-flare",
            "-u", "kinetic-engine",
            "-u", "morpho-engine",
            "-u", "enosys-engine",
            "--no-pager",
            "--output=cat",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("journalctl spawn failed");

    let stdout = cmd.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        process_log_line(&line, &state, &webhook_url).await;
    }
}

async fn process_log_line(line: &str, state: &State, webhook_url: &str) {
    // TX bestätigt → Trade-Event
    if line.contains("TX:") || line.contains("Liquidation TX sent:") || line.contains("✅ Enosys TX:") {
        if let Some(hash) = extract_tx_hash(line) {
            let bot = if line.contains("kojo_flare") || line.contains("ARB") { "kojo-flare" }
                else if line.contains("kinetic") { "kinetic" }
                else if line.contains("enosys") { "enosys" }
                else { "morpho" };

            let event = TradeEvent {
                timestamp: now_str(),
                bot: bot.to_string(),
                profit_usd: 0.0,
                tx_hash: hash.clone(),
            };

            let mut s = state.lock().await;
            s.trades_total += 1;
            s.last_trades.insert(0, event);
            s.last_trades.truncate(5);
            drop(s);

            // Sofort-Alert
            let msg = format!("⚡ **TRADE EXECUTED** | Bot: `{}` | TX: `{}`", bot, &hash[..16]);
            send_discord(webhook_url, &msg).await;
        }
    }

    // Kinetic Shortfall
    if line.contains("SHORTFALL") && line.contains("borrower=") {
        let borrower = extract_between(line, "borrower=", " ").unwrap_or("?");
        let shortfall = extract_f64_after(line, "shortfall_usd~=").unwrap_or(0.0);
        let balance   = extract_f64_after(line, "borrow_balance=").unwrap_or(0.0);

        let miss = NearMiss {
            borrower: borrower[..borrower.len().min(10)].to_string() + "...",
            shortfall_usd: shortfall,
            borrow_balance: balance,
        };

        let mut s = state.lock().await;
        s.near_misses_kinetic.retain(|m| m.borrower != miss.borrower);
        s.near_misses_kinetic.insert(0, miss);
        s.near_misses_kinetic.truncate(10);
    }

    // Enosys Trove Count
    if line.contains("Branch FXRP:") && line.contains("troves") {
        if let Some(n) = extract_u32_before(line, " troves") {
            state.lock().await.enosys_fxrp_troves = n;
        }
    }
    if line.contains("Branch WFLR:") && line.contains("troves") {
        if let Some(n) = extract_u32_before(line, " troves") {
            state.lock().await.enosys_wflr_troves = n;
        }
    }

    // FTSO Preis
    if line.contains("FTSO") && line.contains("FLR:") {
        if let Some(flr) = extract_f64_after(line, "FLR: $") {
            state.lock().await.flr_price = flr;
        }
        if let Some(xrp) = extract_f64_after(line, "XRP: $") {
            state.lock().await.xrp_price = xrp;
        }
    }
    // Morpho Block log
    if line.contains("Morpho Engine") && line.contains("Borrowers:") {
        if let Some(n) = extract_u32_after(line, "Borrowers: ") {
            state.lock().await.morpho_borrowers = n;
        }
    }

    // Kinetic Stats
    if line.contains("tracked_borrowers=") {
        if let Some(n) = extract_u32_after(line, "tracked_borrowers=") {
            state.lock().await.kinetic_borrowers = n;
        }
    }

    // Error / Crash → Alert
    if line.contains("ERROR") || line.contains("❌") {
        let trimmed = if line.len() > 200 { &line[..200] } else { line };
        state.lock().await.last_error = Some(trimmed.to_string());
    }
}

// ── Heartbeat: Discord Webhook POST alle N Sekunden ───────────────────────────

async fn heartbeat(
    state: State,
    addrs: Vec<String>,
    owner_addr: String,
    provider: Arc<Provider<Http>>,
    webhook_url: String,
    interval_secs: u64,
) {
    let mut ticker = interval(Duration::from_secs(interval_secs));
    ticker.tick().await; // erster Tick sofort

    loop {
        ticker.tick().await;

        // Preise direkt per FTSOv2 RPC holen (immer aktuell)
        let (ftso_flr, ftso_xrp) = fetch_prices(provider.clone()).await;
        let flr_price;
        let xrp_price;
        {
            let mut s = state.lock().await;
            if ftso_flr > 0.0 { s.flr_price = ftso_flr; }
            if ftso_xrp > 0.0 { s.xrp_price = ftso_xrp; }
            flr_price = if s.flr_price > 0.0 { s.flr_price } else { 0.0094 };
            xrp_price = if s.xrp_price > 0.0 { s.xrp_price } else { 1.38 };
        }

        let mut all_addrs = addrs.clone();
        if !owner_addr.is_empty() && !all_addrs.contains(&owner_addr) {
            all_addrs.push(owner_addr.clone());
        }

        let balances = fetch_balances(&all_addrs, provider.clone(), flr_price, xrp_price).await;

        // Baseline setzen beim ersten Mal
        let mut s = state.lock().await;
        if !s.baseline_set {
            s.baseline = balances.clone();
            s.baseline_set = true;
        }
        s.balances = balances.clone();

        // PnL: Summe(aktuell) - Summe(baseline)
        let total_now: f64 = balances.values().map(|b| b.total_usd(flr_price, xrp_price)).sum();
        let total_base: f64 = s.baseline.values().map(|b| b.total_usd(flr_price, xrp_price)).sum();
        s.pnl_session_usd = total_now - total_base;

        let pnl = s.pnl_session_usd;
        let trades = s.trades_total;
        let near_misses = s.near_misses_kinetic.clone();
        let last_trades = s.last_trades.clone();
        let fxrp_troves = s.enosys_fxrp_troves;
        let wflr_troves = s.enosys_wflr_troves;
        let morpho_borrowers = s.morpho_borrowers;
        let kinetic_borrowers = s.kinetic_borrowers;
        let last_error = s.last_error.clone();
        drop(s);

        // Discord Nachricht bauen
        let ts = now_str();
        let pnl_emoji = if pnl >= 0.0 { "📈" } else { "📉" };
        let pnl_sign  = if pnl >= 0.0 { "+" } else { "" };

        let mut wallet_lines = String::new();
        let mut total_usd_all = 0f64;
        for (i, addr) in addrs.iter().enumerate() {
            if let Some(b) = balances.get(addr) {
                let usd = b.total_usd(flr_price, xrp_price);
                total_usd_all += usd;
                wallet_lines.push_str(&format!(
                    "  **Bot {}** `{}…` | **{:.2} FLR** (${:.2}) | USDC.e ${:.2} | USDT0 ${:.2} | FXRP {:.2} | ≈**${:.2}**\n",
                    i + 1, &addr[2..10],
                    b.flr_native, b.flr_native * flr_price,
                    b.usdce, b.usdt0, b.fxrp,
                    usd
                ));
            }
        }
        if !owner_addr.is_empty() {
            if let Some(b) = balances.get(&owner_addr) {
                let usd = b.total_usd(flr_price, xrp_price);
                total_usd_all += usd;
                wallet_lines.push_str(&format!(
                    "  **Owner** `{}…` | **{:.2} FLR** (${:.2}) | USDC.e ${:.2} | USDT0 ${:.2} | FXRP {:.2} | ≈**${:.2}**\n",
                    &owner_addr[2..10],
                    b.flr_native, b.flr_native * flr_price,
                    b.usdce, b.usdt0, b.fxrp,
                    usd
                ));
            }
        }
        wallet_lines.push_str(&format!("  **TOTAL:** ≈**${:.2}**\n", total_usd_all));

        // Near-Miss Section
        let near_miss_section = if near_misses.is_empty() {
            "  ✅ keine aktiven Shortfalls\n".to_string()
        } else {
            let top3: Vec<String> = near_misses.iter().take(3).map(|m| {
                format!("  ⚠️ `{}` shortfall~${:.2} borrow={:.0}\n",
                    m.borrower, m.shortfall_usd, m.borrow_balance)
            }).collect();
            top3.join("")
        };

        // Letzte Trades Section
        let trades_section = if last_trades.is_empty() {
            "  noch keine Trades diese Session\n".to_string()
        } else {
            last_trades.iter().take(3).map(|t| {
                format!("  ✅ [{}] `{}` TX: `{}`\n", t.bot, t.timestamp, &t.tx_hash[..14])
            }).collect::<Vec<_>>().join("")
        };

        let error_section = match &last_error {
            Some(e) => format!("\n🔴 **LETZTER ERROR:** `{}`\n", &e[..e.len().min(100)]),
            None    => String::new(),
        };

        let msg = format!(
"📊 **KOJO STATUS** — {}
**FLR:** ${:.5}  |  **XRP:** ${:.4}
═══════════════════════════════
💰 **WALLETS**
{}
{} **PnL (Session):** {}{:.4} USD  |  **Trades:** {}
═══════════════════════════════
🔴 **NEAR-MISS Kinetic** ({} Borrower)
{}
🟡 **ENOSYS** | FXRP: {} Troves | WFLR: {} Troves
🟣 **MORPHO** | {} Borrower
═══════════════════════════════
🕐 **Letzte Trades**
{}{}",
            ts,
            flr_price, xrp_price,
            wallet_lines,
            pnl_emoji, pnl_sign, pnl, trades,
            kinetic_borrowers,
            near_miss_section,
            fxrp_troves, wflr_troves,
            morpho_borrowers,
            trades_section,
            error_section,
        );

        send_discord(&webhook_url, &msg).await;
        info!("Heartbeat gesendet (PnL {}{:.4} USD, {} Trades)", pnl_sign, pnl, trades);
    }
}

// ── DeepSeek Daily Summary ────────────────────────────────────────────────────

async fn deepseek_daily(webhook_url: String, deepseek_key: String) {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let hour_utc = (now_secs % 86400) / 3600;

        if hour_utc == 8 {
            info!("Starte DeepSeek Daily Summary...");
            match generate_daily_summary(&deepseek_key).await {
                Ok(summary) => {
                    let msg = format!("🧠 **KOJO DAILY AI REPORT**\n{}", summary);
                    send_discord(&webhook_url, &msg).await;
                }
                Err(e) => error!("DeepSeek Daily Summary Fehler: {}", e),
            }
            tokio::time::sleep(Duration::from_secs(82800)).await;
        }
    }
}

async fn generate_daily_summary(api_key: &str) -> Result<String> {
    let output = tokio::process::Command::new("journalctl")
        .args([
            "-u", "kojo-flare",
            "-u", "kinetic-engine",
            "-u", "morpho-engine",
            "-u", "enosys-engine",
            "--since", "24 hours ago",
            "--no-pager",
            "--output=cat",
        ])
        .output()
        .await?;

    let raw_logs = String::from_utf8_lossy(&output.stdout);
    let logs: String = raw_logs.chars().take(80_000).collect();

    let filtered: String = logs.lines()
        .filter(|l| {
            l.contains("TX:") || l.contains("SHORTFALL") || l.contains("LIQUIDAT")
            || l.contains("ERROR") || l.contains("❌") || l.contains("STATS")
            || l.contains("FTSO") || l.contains("DEX OPP")
        })
        .take(200)
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Du bist ein MEV-Bot Analyst für das Kojo Team auf Flare Mainnet.\n\
        Analysiere diese Bot-Logs der letzten 24 Stunden kurz und präzise (max 400 Wörter):\n\
        - Wie viele Trades wurden ausgeführt?\n\
        - Welche Near-Misses gab es (Kinetic Shortfalls, Enosys nahe MCR)?\n\
        - Welche Fehler sind aufgetreten?\n\
        - Was ist die Hauptempfehlung für das Team?\n\n\
        LOGS:\n{}",
        filtered
    );

    deepseek_ask(api_key, &prompt).await
}

// ── DeepSeek: Frage beantworten ───────────────────────────────────────────────

async fn deepseek_ask(api_key: &str, prompt: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.deepseek.com/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&json!({
            "model": "deepseek-chat",
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 600,
            "temperature": 0.3
        }))
        .timeout(Duration::from_secs(45))
        .send()
        .await?;

    let data: Value = resp.json().await?;
    let answer = data["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("(keine Antwort von DeepSeek)")
        .to_string();

    Ok(answer)
}

// ── Build State-Snapshot als Text für DeepSeek-Kontext ───────────────────────

async fn build_state_context(state: &State) -> String {
    let s = state.lock().await;
    let pnl_sign = if s.pnl_session_usd >= 0.0 { "+" } else { "" };

    let near_miss_txt = if s.near_misses_kinetic.is_empty() {
        "  keine aktiven Shortfalls".to_string()
    } else {
        s.near_misses_kinetic.iter().take(5).map(|m| {
            format!("  borrower={} shortfall=${:.2} borrow={:.0}", m.borrower, m.shortfall_usd, m.borrow_balance)
        }).collect::<Vec<_>>().join("\n")
    };

    let trades_txt = if s.last_trades.is_empty() {
        "  noch keine Trades diese Session".to_string()
    } else {
        s.last_trades.iter().take(5).map(|t| {
            format!("  [{}] {} TX: {}", t.bot, t.timestamp, &t.tx_hash[..16.min(t.tx_hash.len())])
        }).collect::<Vec<_>>().join("\n")
    };

    format!(
        "=== KOJO BOT STATUS (Live) ===\n\
        FLR Preis: ${:.5} | XRP Preis: ${:.4}\n\
        PnL Session: {}{:.4} USD | Trades Total: {}\n\
        Kinetic Borrower: {} | Morpho Borrower: {}\n\
        Enosys FXRP Troves: {} | WFLR Troves: {}\n\
        Letzter Error: {}\n\n\
        Near-Misses:\n{}\n\n\
        Letzte Trades:\n{}\n",
        s.flr_price, s.xrp_price,
        pnl_sign, s.pnl_session_usd, s.trades_total,
        s.kinetic_borrowers, s.morpho_borrowers,
        s.enosys_fxrp_troves, s.enosys_wflr_troves,
        s.last_error.as_deref().unwrap_or("keiner"),
        near_miss_txt,
        trades_txt,
    )
}

// ── Discord Gateway Bot: !ask Command Handler ─────────────────────────────────

async fn discord_gateway(
    bot_token: String,
    channel_id: String,
    state: State,
    deepseek_key: String,
) {
    let gateway_url = "wss://gateway.discord.gg/?v=10&encoding=json";
    let mut reconnect_delay = 5u64;

    loop {
        info!("Discord Gateway: verbinde zu {}...", gateway_url);

        let ws_result = connect_async(gateway_url).await;
        let (mut ws, _) = match ws_result {
            Ok(r) => r,
            Err(e) => {
                warn!("Discord Gateway connect Fehler: {} — retry in {}s", e, reconnect_delay);
                tokio::time::sleep(Duration::from_secs(reconnect_delay)).await;
                reconnect_delay = (reconnect_delay * 2).min(120);
                continue;
            }
        };
        reconnect_delay = 5;

        // Heartbeat interval (wird vom Server in HELLO gesetzt)
        let mut heartbeat_interval_ms: u64 = 41250;
        let mut sequence: Option<u64> = None;
        let mut identified = false;

        loop {
            // Heartbeat-Timer und WebSocket-Nachrichten parallel
            let hb_sleep = tokio::time::sleep(Duration::from_millis(heartbeat_interval_ms));
            tokio::pin!(hb_sleep);

            tokio::select! {
                _ = &mut hb_sleep => {
                    // Heartbeat senden
                    let hb_payload = match sequence {
                        Some(seq) => json!({"op": 1, "d": seq}),
                        None      => json!({"op": 1, "d": null}),
                    };
                    if let Err(e) = ws.send(Message::Text(hb_payload.to_string())).await {
                        warn!("Discord Heartbeat send error: {}", e);
                        break;
                    }
                }

                msg_result = ws.next() => {
                    let msg = match msg_result {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            warn!("Discord WS error: {}", e);
                            break;
                        }
                        None => {
                            warn!("Discord WS closed");
                            break;
                        }
                    };

                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => {
                            warn!("Discord WS: Close frame erhalten");
                            break;
                        }
                        _ => continue,
                    };

                    let payload: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Sequence aktualisieren
                    if let Some(s) = payload["s"].as_u64() {
                        sequence = Some(s);
                    }

                    let op = payload["op"].as_u64().unwrap_or(99);

                    match op {
                        // HELLO → identify + heartbeat interval
                        10 => {
                            heartbeat_interval_ms = payload["d"]["heartbeat_interval"]
                                .as_u64()
                                .unwrap_or(41250);
                            info!("Discord Gateway: HELLO empfangen (heartbeat={}ms)", heartbeat_interval_ms);

                            if !identified {
                                let identify = json!({
                                    "op": 2,
                                    "d": {
                                        "token": bot_token,
                                        "intents": 33280,  // GUILD_MESSAGES + MESSAGE_CONTENT
                                        "properties": {
                                            "os": "linux",
                                            "browser": "kojo",
                                            "device": "kojo"
                                        }
                                    }
                                });
                                if let Err(e) = ws.send(Message::Text(identify.to_string())).await {
                                    warn!("Discord Identify send error: {}", e);
                                    break;
                                }
                                identified = true;
                                info!("Discord Gateway: IDENTIFY gesendet");
                            }
                        }

                        // HEARTBEAT_ACK
                        11 => {}

                        // DISPATCH (Ereignisse)
                        0 => {
                            let t = payload["t"].as_str().unwrap_or("");

                            if t == "READY" {
                                info!("Discord Gateway: READY — Bot ist online ✅");
                            }

                            if t == "MESSAGE_CREATE" {
                                let msg_channel = payload["d"]["channel_id"].as_str().unwrap_or("");
                                let content = payload["d"]["content"].as_str().unwrap_or("");
                                let author_bot = payload["d"]["author"]["bot"].as_bool().unwrap_or(false);

                                // Nur im richtigen Channel + kein Bot-Echo
                                if msg_channel == channel_id && !author_bot {
                                    if let Some(question) = content.strip_prefix("!ask ") {
                                        let question = question.trim().to_string();
                                        if question.is_empty() {
                                            continue;
                                        }

                                        info!("Discord !ask: \"{}\"", question);

                                        // State-Kontext sammeln
                                        let state_ctx = build_state_context(&state).await;

                                        // Letzte Log-Zeilen (letzten 30 Min)
                                        let recent_logs = get_recent_logs(30).await;

                                        let full_prompt = format!(
                                            "Du bist Kojo AI, der intelligente Assistent des Kojo MEV Bot Teams auf Flare Mainnet.\n\
                                            Antworte präzise und kurz (max 300 Wörter) auf Deutsch oder Englisch je nach Frage.\n\n\
                                            {}\n\
                                            === LETZTE LOGS (30 Min) ===\n{}\n\n\
                                            === FRAGE ===\n{}",
                                            state_ctx, recent_logs, question
                                        );

                                        let deepseek_key2 = deepseek_key.clone();
                                        let channel_id2 = channel_id.clone();
                                        let bot_token2 = bot_token.clone();

                                        tokio::spawn(async move {
                                            // Typing indicator senden
                                            let typing_url = format!(
                                                "https://discord.com/api/v10/channels/{}/typing",
                                                channel_id2
                                            );
                                            let _ = reqwest::Client::new()
                                                .post(&typing_url)
                                                .header("Authorization", format!("Bot {}", bot_token2))
                                                .header("Content-Length", "0")
                                                .send()
                                                .await;

                                            // DeepSeek befragen
                                            let answer = match deepseek_ask(&deepseek_key2, &full_prompt).await {
                                                Ok(a) => a,
                                                Err(e) => format!("❌ DeepSeek Fehler: {}", e),
                                            };

                                            // Antwort in Discord-Channel posten
                                            let reply = format!("🧠 **Kojo AI:** {}", answer);
                                            let trimmed = if reply.len() > 1950 {
                                                format!("{}...", &reply[..1950])
                                            } else {
                                                reply
                                            };

                                            let msg_url = format!(
                                                "https://discord.com/api/v10/channels/{}/messages",
                                                channel_id2
                                            );
                                            if let Err(e) = reqwest::Client::new()
                                                .post(&msg_url)
                                                .header("Authorization", format!("Bot {}", bot_token2))
                                                .json(&json!({"content": trimmed}))
                                                .send()
                                                .await
                                            {
                                                warn!("Discord reply send error: {}", e);
                                            }
                                        });
                                    }
                                }
                            }
                        }

                        _ => {}
                    }
                }
            }
        }

        warn!("Discord Gateway: Verbindung getrennt — reconnect in {}s", reconnect_delay);
        tokio::time::sleep(Duration::from_secs(reconnect_delay)).await;
        reconnect_delay = (reconnect_delay * 2).min(120);
    }
}

async fn get_recent_logs(minutes: u64) -> String {
    let since = format!("{} minutes ago", minutes);
    let output = match tokio::process::Command::new("journalctl")
        .args([
            "-u", "kojo-flare",
            "-u", "kinetic-engine",
            "-u", "morpho-engine",
            "-u", "enosys-engine",
            "--since", &since,
            "--no-pager",
            "--output=cat",
        ])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return String::new(),
    };

    let raw = String::from_utf8_lossy(&output.stdout);
    // Nur relevante Zeilen, max 3000 Zeichen
    let filtered: String = raw.lines()
        .filter(|l| {
            l.contains("TX:") || l.contains("SHORTFALL") || l.contains("LIQUIDAT")
            || l.contains("ERROR") || l.contains("❌") || l.contains("STATS")
            || l.contains("FTSO") || l.contains("DEX OPP") || l.contains("Branch")
            || l.contains("Borrower") || l.contains("price=")
        })
        .collect::<Vec<_>>()
        .join("\n");

    filtered.chars().take(3000).collect()
}

// ── Discord ───────────────────────────────────────────────────────────────────

async fn send_discord(url: &str, content: &str) {
    if url.is_empty() { return; }
    let trimmed = if content.len() > 1950 {
        format!("{}...", &content[..1950])
    } else {
        content.to_string()
    };

    let client = reqwest::Client::new();
    if let Err(e) = client
        .post(url)
        .json(&json!({ "content": trimmed }))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        warn!("Discord send error: {}", e);
    }
}

// ── Hilfsfunktionen ───────────────────────────────────────────────────────────

fn now_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let s = secs % 86400;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    format!("{:02}:{:02}:{:02} UTC", h, m, sec)
}

fn extract_tx_hash(line: &str) -> Option<String> {
    if let Some(idx) = line.find("0x") {
        let rest = &line[idx..];
        let end = rest.find(|c: char| !c.is_ascii_hexdigit() && c != 'x')
            .unwrap_or(rest.len());
        let hash = &rest[..end];
        if hash.len() >= 10 {
            return Some(hash.to_string());
        }
    }
    None
}

fn extract_between<'a>(s: &'a str, after: &str, before: &str) -> Option<&'a str> {
    let start = s.find(after)? + after.len();
    let sub = &s[start..];
    let end = sub.find(before).unwrap_or(sub.len());
    Some(&sub[..end])
}

fn extract_f64_after(s: &str, marker: &str) -> Option<f64> {
    let start = s.find(marker)? + marker.len();
    let sub = &s[start..];
    let end = sub.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(sub.len());
    sub[..end].parse().ok()
}

fn extract_u32_before(s: &str, marker: &str) -> Option<u32> {
    let end = s.find(marker)?;
    let sub = &s[..end];
    sub.split_whitespace().last()?.parse().ok()
}

fn extract_u32_after(s: &str, marker: &str) -> Option<u32> {
    let start = s.find(marker)? + marker.len();
    let sub = &s[start..];
    let end = sub.find(|c: char| !c.is_ascii_digit()).unwrap_or(sub.len());
    sub[..end].parse().ok()
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt::init();

    info!("Kojo Monitor v2.0 — Discord Heartbeat + DeepSeek Daily + !ask Command");

    let rpc_url = env::var("RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:9650/ext/bc/C/rpc".to_string());
    let pks = env::var("PRIVATE_KEYS").expect("PRIVATE_KEYS required");
    let webhook = env::var("DISCORD_MONITOR_WEBHOOK_URL")
        .unwrap_or_else(|_| env::var("DISCORD_WEBHOOK_URL").unwrap_or_default());
    let deepseek_key = env::var("DEEPSEEK_API_KEY").unwrap_or_default();
    let bot_token = env::var("DISCORD_BOT_TOKEN").unwrap_or_default();
    let channel_id = env::var("DISCORD_CHANNEL_ID")
        .unwrap_or_else(|_| "1476898395951206442".to_string());
    let heartbeat_secs: u64 = env::var("MONITOR_HEARTBEAT_SECS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    let owner_addr = env::var("OWNER_WALLET").unwrap_or_default();

    let provider = Arc::new(Provider::<Http>::try_from(rpc_url)?);
    let bot_addrs = derive_addresses(&pks);

    info!("Tracking {} bot wallets", bot_addrs.len());
    for (i, addr) in bot_addrs.iter().enumerate() {
        info!("  Bot {}: {}", i + 1, addr);
    }
    if !owner_addr.is_empty() {
        info!("  Owner: {}", owner_addr);
    }

    if webhook.is_empty() {
        warn!("DISCORD_MONITOR_WEBHOOK_URL nicht gesetzt — Discord Output deaktiviert");
    }

    let state: State = Arc::new(Mutex::new(BotState::default()));

    // Startup-Nachricht
    send_discord(&webhook, "🚀 **Kojo Monitor v2.0 gestartet** — Heartbeat + !ask Command aktiv").await;

    // Task 1: Log-Watcher (journalctl --follow)
    let state1 = state.clone();
    let wh1 = webhook.clone();
    tokio::spawn(async move {
        log_watcher(state1, wh1).await;
    });

    // Task 2: Heartbeat (Wallet-Balances + Status-Post)
    let state2 = state.clone();
    let addrs2 = bot_addrs.clone();
    let owner2 = owner_addr.clone();
    let prov2 = provider.clone();
    let wh2 = webhook.clone();
    tokio::spawn(async move {
        heartbeat(state2, addrs2, owner2, prov2, wh2, heartbeat_secs).await;
    });

    // Task 3: DeepSeek Daily Summary (täglich 08:00 UTC)
    if !deepseek_key.is_empty() {
        let wh3 = webhook.clone();
        let key3 = deepseek_key.clone();
        tokio::spawn(async move {
            deepseek_daily(wh3, key3).await;
        });
    } else {
        info!("DEEPSEEK_API_KEY nicht gesetzt — Daily Summary deaktiviert");
    }

    // Task 4: Discord Gateway Bot (!ask Command Handler)
    if !bot_token.is_empty() && !deepseek_key.is_empty() {
        let state4 = state.clone();
        let token4 = bot_token.clone();
        let chan4 = channel_id.clone();
        let key4 = deepseek_key.clone();
        tokio::spawn(async move {
            discord_gateway(token4, chan4, state4, key4).await;
        });
        info!("Discord Gateway Bot gestartet — Channel: {}", channel_id);
    } else {
        warn!("DISCORD_BOT_TOKEN oder DEEPSEEK_API_KEY fehlt — !ask Command deaktiviert");
    }

    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
