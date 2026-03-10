// KOJO FLARE — FTSOv2 Oracle + FAssets Liquidation Bot (2026-02-25)
// Version: 2026-02-25.1 (FastUpdaterSubmitClient wrapper, SinkExt, to_big_endian, serde_json)
//
// Primäre Strategien:
//   1. FAssets Liquidation: FTSO Preis-Update → CR < 130% → Liquidation (10%+ guaranteed premium)
//   2. DEX Arbitrage: SparkDEX V3.1 Pool-Spread zwischen Fee-Tiers
//
// Architecture:
//   - eth_subscribe("newHeads") via WebSocket (Flare ~1.8s Blockzeit)
//   - FTSOv2 Fast Updates: alle ~1.8s neuer Preis-Feed (Block-Latenz)
//   - Lokaler go-flare Node = Sub-Millisekunden Latenz zum Netzwerk
//   - SparkLend FlashLoan (0% fee via ApprovedFlashBorrower) via FlashBotFlare Contract
//
// Deployment:
//   PRIVATE_KEY=0x... FLASHBOT_FLARE=0x... ENABLE_LIVE_TRADING=true ./kojo_flare

use ethers::prelude::*;
use eyre::Result;
use futures::{future::join_all, SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{info, error, warn, debug};
use std::env;
use dotenvy::dotenv;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

// ── RPC Config ──────────────────────────────────────────────────────────────

const RPC_URLS: &[&str] = &[
    "http://127.0.0.1:9650/ext/bc/C/rpc",          // primary: local go-flare (<1ms)
    "https://flare-api.flare.network/ext/bc/C/rpc", // fallback 1
    "https://flare.drpc.org",                       // fallback 2
];

const WS_URLS: &[&str] = &[
    "ws://127.0.0.1:9650/ext/bc/C/ws",              // primary: local go-flare WS
    "wss://flare-api.flare.network/ext/bc/C/ws",    // fallback
];

// ── Flare Mainnet Adressen (Chain ID: 14) ────────────────────────────────────

// Tokens — verifiziert via Fork-Tests
const WFLR:  &str = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d";
const USDCE: &str = "0xFbDa5F676cB37624f28265A144A48B0d6e87d3b6"; // USDC.e
const SFLR:  &str = "0x12e605bc104e93B45e1aD99F9e555f659051c2BB"; // Staked FLR

// SparkDEX V3.1 Factory — verifiziert
const SPARKDEX_FACTORY: &str = "0x8A2578d23d4C532cC9A98FaD91C0523f5efDE652";

// FTSOv2 — FastUpdater Contract (Flare Mainnet)
// Aufruf: fetchCurrentFeeds(feedIds) → Preise fuer FLR, BTC, XRP etc.
// Feed IDs: 0x01464c522f55534400000000000000000000000000 = FLR/USD
//           0x014254432f55534400000000000000000000000000 = BTC/USD
//           0x015852502f55534400000000000000000000000000 = XRP/USD
const FTSO_FAST_UPDATER: &str = "0x70e8870ef234EcD665F96Da4c669dc12c1e1c116"; // Flare Mainnet

// ── DEX Pool Config ──────────────────────────────────────────────────────────
//
// PLACEHOLDER POOLS — via Factory.getPool() beim ersten Start ermitteln!
// RUN_POOL_DISCOVERY=true → entdeckt echte Adressen

const POOLS: &[(&str, &str, &str, u32)] = &[
    ("WFLR/USDCE", "SparkDEX-0.3%",  "0x0000000000000000000000000000000000000001", 3000),
    ("WFLR/USDCE", "SparkDEX-0.05%", "0x0000000000000000000000000000000000000002", 500),
    ("WFLR/SFLR",  "SparkDEX-0.05%", "0x0000000000000000000000000000000000000003", 500),
    ("WFLR/SFLR",  "SparkDEX-0.3%",  "0x0000000000000000000000000000000000000004", 3000),
];

// ── FAssets Agent Config ──────────────────────────────────────────────────────
//
// Agents zum Monitoring — FXRP/FBTC Vaults auf Flare
// Format: (symbol, asset_manager, fasset_token, collateral_token)
// TODO: Nach Deployment echte Agent-Vault-Adressen via FAssets Explorer hinzufügen

const FASSET_MARKETS: &[(&str, &str, &str, &str)] = &[
    // FXRP — FAssets XRP (häufigste Liquidationen wegen XRP-Volatilität)
    // (symbol, asset_manager, fasset_token, collateral_token_WFLR)
    ("FXRP",
     "0x0000000000000000000000000000000000000010", // PLACEHOLDER — IAssetManager FXRP
     "0x0000000000000000000000000000000000000011", // PLACEHOLDER — FXRP Token
     WFLR),
    // FBTC — FAssets BTC
    ("FBTC",
     "0x0000000000000000000000000000000000000012", // PLACEHOLDER — IAssetManager FBTC
     "0x0000000000000000000000000000000000000013", // PLACEHOLDER — FBTC Token
     WFLR),
];

// ── CEX Price State (Binance + Bybit) ───────────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct CexPrice {
    last: f64,
    ts_ms: u64,
    source: &'static str,
}

#[derive(Clone, Debug, Default)]
struct CexPrices {
    flr_usdt: Option<CexPrice>,
    xrp_usdt: Option<CexPrice>,
    btc_usdt: Option<CexPrice>,
}

type SharedCexPrices = Arc<RwLock<CexPrices>>;

#[derive(Clone, Debug)]
struct FastFeedDelta {
    symbol: &'static str,
    price: f64,
}

#[derive(Clone, Debug, Default)]
struct FastUpdatePayload {
    block: u64,
    reward_epoch: u32,
    deltas: Vec<FastFeedDelta>,
}

type SharedFastUpdatePayload = Arc<RwLock<Option<FastUpdatePayload>>>;

// Binance WS payloads
#[derive(Debug, Deserialize)]
struct BinanceStream<T> {
    stream: String,
    data: T,
}

#[derive(Debug, Deserialize)]
struct BinanceTicker {
    #[serde(rename = "c")]
    last_price: String,
    #[serde(default)]
    #[serde(rename = "E")]
    event_time: Option<u64>,
}

// Bybit WS payloads
#[derive(Debug, Deserialize)]
struct BybitTickerMsg {
    topic: String,
    #[serde(default)]
    data: Option<Vec<BybitTicker>>,
}

#[derive(Debug, Deserialize)]
struct BybitTicker {
    symbol: String,
    #[serde(rename = "lastPrice")]
    last_price: String,
    #[serde(default)]
    ts: Option<u64>,
}

// ── ABI Definitions ──────────────────────────────────────────────────────────

// SparkDEX V3.1 Pool — Standard UniV3 slot0
abigen!(IUniswapV3Pool, r#"[
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked)
]"#);

// SparkDEX V3.1 Factory
abigen!(IUniswapV3Factory, r#"[
    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool)
]"#);

// FTSOv2 FastUpdater — fetchCurrentFeeds für Preis-Feeds
abigen!(IFastUpdater, r#"[
    function fetchCurrentFeeds(bytes21[] feedIds) external view returns (uint256[] memory values, int8[] memory decimals, uint64 timestamp)
]"#);

// FTSOv2 FastUpdater — Meta-Information für Fast Updates (Sortition & Belohnungen)
abigen!(IFastUpdaterView, r#"[
    function currentRewardEpochId() external view returns (uint24)
    function currentScoreCutoff() external view returns (uint256)
    function currentSortitionWeight(address _signingPolicyAddress) external view returns (uint256)
    function submissionWindow() external view returns (uint8)
    function numberOfUpdatesInBlock(uint256 _blockNumber) external view returns (uint256)
]"#);

// FTSOv2 FastUpdater — submitUpdates (signierte Transaktion)
// Calldata wird manuell gebaut; kein abigen wegen komplexem nested tuple.
// Wrapper nur für address() + client().send_transaction()
#[derive(Clone)]
struct FastUpdaterSubmitClient<M> {
    address: Address,
    client: Arc<M>,
}

impl<M> FastUpdaterSubmitClient<M> {
    fn new(address: Address, client: Arc<M>) -> Self {
        Self { address, client }
    }
    fn address(&self) -> Address {
        self.address
    }
    fn client(&self) -> &Arc<M> {
        &self.client
    }
}

// FlashBotFlare Contract — SimpleArb + FAssets Liquidation
abigen!(IFlashBotFlare, r#"[
    {
        "type": "function",
        "name": "executeSimpleArb",
        "inputs": [{
            "name": "p",
            "type": "tuple",
            "components": [
                {"name": "tokenBorrow",       "type": "address"},
                {"name": "tokenIntermediate", "type": "address"},
                {"name": "buyFee",            "type": "uint24"},
                {"name": "sellFee",           "type": "uint24"},
                {"name": "borrowAmount",      "type": "uint256"},
                {"name": "minProfit",         "type": "uint256"}
            ]
        }],
        "outputs": [],
        "stateMutability": "nonpayable"
    },
    {
        "type": "function",
        "name": "executeFAssetsLiquidation",
        "inputs": [{
            "name": "p",
            "type": "tuple",
            "components": [
                {"name": "assetManager",       "type": "address"},
                {"name": "agentVault",         "type": "address"},
                {"name": "fassetToken",        "type": "address"},
                {"name": "liquidateAmountUBA", "type": "uint256"},
                {"name": "collateralToken",    "type": "address"},
                {"name": "swapFee",            "type": "uint24"},
                {"name": "minProfit",          "type": "uint256"}
            ]
        }],
        "outputs": [],
        "stateMutability": "nonpayable"
    }
]"#);

// ── Preis-Feed Definitionen (FTSOv2 Feed IDs) ────────────────────────────────

fn flr_usd_feed_id() -> [u8; 21] {
    // "FLR/USD" encoded: 0x01 + "FLR/USD" padded to 20 bytes
    let mut id = [0u8; 21];
    id[0] = 0x01;
    let name = b"FLR/USD";
    id[1..name.len()+1].copy_from_slice(name);
    id
}

fn xrp_usd_feed_id() -> [u8; 21] {
    let mut id = [0u8; 21];
    id[0] = 0x01;
    let name = b"XRP/USD";
    id[1..name.len()+1].copy_from_slice(name);
    id
}

fn btc_usd_feed_id() -> [u8; 21] {
    let mut id = [0u8; 21];
    id[0] = 0x01;
    let name = b"BTC/USD";
    id[1..name.len()+1].copy_from_slice(name);
    id
}

// ── FTSOv2 Preis-Snapshot ────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct FtsoSnapshot {
    flr_usd:   f64,
    xrp_usd:   f64,
    btc_usd:   f64,
    timestamp: u64,
    block:     u64,
}

// ── DEX Price (für Arb-Detection) ────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Price {
    pair:      String,
    dex_name:  String,
    pool_addr: String,
    price:     f64,
    fee:       u32,
    fee_bps:   u32,
}

// ── FAssets Liquidation Opportunity ──────────────────────────────────────────

#[derive(Clone, Debug)]
struct LiquidationOpp {
    symbol:        String,
    asset_manager: String,
    agent_vault:   String,
    fasset_token:  String,
    collateral:    String, // WFLR
    cr_pct:        f64,    // Current Collateral Ratio in %
    est_profit_usd: f64,
}

// ── RPC Rotator ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct RpcRotator {
    providers: Vec<Arc<Provider<Http>>>,
    index:     usize,
}

impl RpcRotator {
    async fn new() -> Result<Self> {
        let mut providers = Vec::new();
        for url in RPC_URLS {
            if let Ok(p) = Provider::<Http>::try_from(*url) {
                providers.push(Arc::new(p));
            }
        }
        if providers.is_empty() {
            return Err(eyre::eyre!("Kein RPC erreichbar"));
        }
        info!("HTTP RPCs: {} endpoints geladen", providers.len());
        Ok(RpcRotator { providers, index: 0 })
    }

    fn get(&self) -> Arc<Provider<Http>> {
        self.providers[self.index].clone()
    }

    fn rotate(&mut self) {
        self.index = (self.index + 1) % self.providers.len();
    }
}

// ── FTSOv2 Preis-Fetch ────────────────────────────────────────────────────────

async fn fetch_ftso_prices(provider: Arc<Provider<Http>>, block: u64) -> Option<FtsoSnapshot> {
    let addr: Address = FTSO_FAST_UPDATER.parse().ok()?;
    let ftso = IFastUpdater::new(addr, provider);

    let feed_ids: Vec<[u8; 21]> = vec![
        flr_usd_feed_id(),
        xrp_usd_feed_id(),
        btc_usd_feed_id(),
    ];

    // Bytes21 als Bytes21 übergeben
    let feeds: Vec<ethers::types::Bytes> = feed_ids.iter()
        .map(|id| ethers::types::Bytes::from(id.to_vec()))
        .collect();

    let result = timeout(
        Duration::from_secs(2),
        ftso.fetch_current_feeds(
            feeds.iter().map(|f| {
                let mut arr = [0u8; 21];
                arr.copy_from_slice(&f[..21]);
                arr
            }).collect()
        )
    ).await.ok()?.ok()?;

    let (values, decimals, timestamp) = result;
    if values.len() < 3 { return None; }

    let to_f64 = |val: U256, dec: i8| -> f64 {
        let v = val.as_u128() as f64;
        if dec >= 0 {
            v / 10f64.powi(dec as i32)
        } else {
            v * 10f64.powi(-dec as i32)
        }
    };

    let flr = to_f64(values[0], decimals[0]);
    let xrp = to_f64(values[1], decimals[1]);
    let btc = to_f64(values[2], decimals[2]);

    // Sanity checks
    if flr < 0.001 || flr > 10.0 { return None; }
    if xrp < 0.01  || xrp > 100.0 { return None; }
    if btc < 1000.0 || btc > 1_000_000.0 { return None; }

    Some(FtsoSnapshot {
        flr_usd: flr,
        xrp_usd: xrp,
        btc_usd: btc,
        timestamp: timestamp,
        block,
    })
}

// ── FTSOv2 Fast-Update Meta (Sortition-Fenster & Belohnungs-Status) ─────────────
//
// Primärer Fokus des Bots im aktuellen Pivot:
//   - Pro neuem Block sortitionsrelevante Kennzahlen loggen.
//   - Sichtbar machen, wann und wie viele Fast Updates im aktuellen Block bereits
//     eingereicht wurden (numberOfUpdatesInBlock).
async fn log_fast_update_state(
    provider: Arc<Provider<Http>>,
    block: u64,
    signing_addr: Address,
) {
    let addr: Address = FTSO_FAST_UPDATER.parse().ok().unwrap_or_default();
    if addr == Address::zero() {
        return;
    }

    let fast = IFastUpdaterView::new(addr, provider);

    // Alle Calls sind best-effort; Fehler werden nur geloggt.
    let epoch = fast.current_reward_epoch_id().await;
    let cutoff = fast.current_score_cutoff().await;
    let weight = fast.current_sortition_weight(signing_addr).await;
    let window = fast.submission_window().await;
    let updates = fast.number_of_updates_in_block(U256::from(block)).await;

    if let (Ok(epoch), Ok(cutoff), Ok(weight), Ok(window), Ok(updates)) =
        (epoch, cutoff, weight, window, updates)
    {
        info!(
            "FAST-UPDATES | block={} epoch={} cutoff={} weight={} window={} updates_in_block={}",
            block, epoch, cutoff, weight, window, updates
        );
    } else {
        debug!("FAST-UPDATES | konnte Metadaten fuer block {} nicht laden", block);
    }
}

// ── CEX Price Helpers ───────────────────────────────────────────────────────────

async fn update_cex_price(
    shared: &SharedCexPrices,
    symbol: &str,
    source: &'static str,
    price: f64,
    ts_ms: u64,
) {
    let mut guard = shared.write().await;
    let slot = match symbol {
        "FLRUSDT" | "FLR/USDT" => &mut guard.flr_usdt,
        "XRPUSDT" | "XRP/USDT" => &mut guard.xrp_usdt,
        "BTCUSDT" | "BTC/USDT" => &mut guard.btc_usdt,
        _ => return,
    };
    *slot = Some(CexPrice { last: price, ts_ms, source });
}

async fn get_latest_cex_prices(shared: &SharedCexPrices) -> Option<(f64, f64, f64)> {
    let guard = shared.read().await;
    let flr = guard.flr_usdt.as_ref()?.last;
    let xrp = guard.xrp_usdt.as_ref()?.last;
    let btc = guard.btc_usdt.as_ref()?.last;
    Some((flr, xrp, btc))
}

// Binance WebSocket: FLRUSDT, XRPUSDT, BTCUSDT
async fn run_binance_ws(shared: SharedCexPrices) {
    let url = Url::parse(
        // Binance.US endpoint (binance.com blocked in USA/NJ; .us endpoint is accessible)
        "wss://stream.binance.us:9443/stream?streams=flrusdt@ticker/xrpusdt@ticker/btcusdt@ticker",
    )
    .expect("invalid binance ws url");

    loop {
        match connect_async(url.clone()).await {
            Ok((mut ws, _)) => {
                info!("BINANCE WS verbunden");
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(txt)) => {
                            if let Ok(parsed) =
                                serde_json::from_str::<BinanceStream<BinanceTicker>>(&txt)
                            {
                                let symbol = parsed.stream.split('@').next().unwrap_or_default();
                                let px: f64 = match parsed.data.last_price.parse() {
                                    Ok(v) => v,
                                    Err(_) => continue,
                                };
                                let ts = parsed.data.event_time.unwrap_or(0);
                                update_cex_price(&shared, &symbol.to_uppercase(), "BINANCE", px, ts)
                                    .await;
                            }
                        }
                        Ok(Message::Ping(p)) => {
                            let _ = ws.send(Message::Pong(p)).await;
                        }
                        Ok(Message::Close(_)) => {
                            warn!("BINANCE WS geschlossen");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                warn!("BINANCE WS Fehler: {}", e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

// Bybit WebSocket: FLRUSDT, XRPUSDT, BTCUSDT
async fn run_bybit_ws(shared: SharedCexPrices) {
    let url =
        Url::parse("wss://stream.bybit.com/v5/public/spot").expect("invalid bybit ws url");

    loop {
        match connect_async(url.clone()).await {
            Ok((mut ws, _)) => {
                info!("BYBIT WS verbunden");

                // subscribe to tickers
                let sub = serde_json::json!({
                    "op": "subscribe",
                    "args": [
                        "tickers.BTCUSDT",
                        "tickers.XRPUSDT",
                        "tickers.FLRUSDT"
                    ]
                })
                .to_string();
                let _ = ws.send(Message::Text(sub)).await;

                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(txt)) => {
                            if let Ok(parsed) =
                                serde_json::from_str::<BybitTickerMsg>(&txt)
                            {
                                if parsed.topic.starts_with("tickers") {
                                    if let Some(list) = parsed.data {
                                        for t in list {
                                            let px: f64 = match t.last_price.parse() {
                                                Ok(v) => v,
                                                Err(_) => continue,
                                            };
                                            let ts = t.ts.unwrap_or(0);
                                            update_cex_price(
                                                &shared,
                                                &t.symbol,
                                                "BYBIT",
                                                px,
                                                ts,
                                            )
                                            .await;
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(p)) => {
                            let _ = ws.send(Message::Pong(p)).await;
                        }
                        Ok(Message::Close(_)) => {
                            warn!("BYBIT WS geschlossen");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                warn!("BYBIT WS Fehler: {}", e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

// ── Fast-Update Payload Preparation (Skelett) ───────────────────────────────────

async fn prepare_fast_update_payload(
    provider: Arc<Provider<Http>>,
    block: u64,
    signing_addr: Address,
    cex_prices: &SharedCexPrices,
    shared_payload: &SharedFastUpdatePayload,
) {
    let addr: Address = FTSO_FAST_UPDATER.parse().ok().unwrap_or_default();
    if addr == Address::zero() {
        return;
    }

    let fast = IFastUpdaterView::new(addr, provider);

    let epoch = match fast.current_reward_epoch_id().await {
        Ok(e) => e,
        Err(e) => {
            debug!("FAST-UPDATES | epoch read error: {}", e);
            return;
        }
    };
    let cutoff = match fast.current_score_cutoff().await {
        Ok(c) => c,
        Err(e) => {
            debug!("FAST-UPDATES | cutoff read error: {}", e);
            return;
        }
    };
    let weight = match fast.current_sortition_weight(signing_addr).await {
        Ok(w) => w,
        Err(e) => {
            debug!("FAST-UPDATES | weight read error: {}", e);
            return;
        }
    };

    // Platzhalter-Heuristik: in der echten Implementierung muss der Provider-Score
    // entsprechend der FastUpdates-Spezifikation berechnet werden. Hier nutzen wir
    // weight >= cutoff als simplen Proxy, um einen Kandidaten-Block zu markieren.
    if weight < cutoff {
        debug!(
            "FAST-UPDATES | weight {} < cutoff {} — kein Submit-Kandidat in block {}",
            weight, cutoff, block
        );
        return;
    }

    // Aktuelle CEX-Preise lesen
    let (flr, xrp, btc) = match get_latest_cex_prices(cex_prices).await {
        Some(v) => v,
        None => {
            debug!(
                "FAST-UPDATES | keine vollstaendigen CEX-Preise fuer block {} vorhanden",
                block
            );
            return;
        }
    };

    let mut deltas = Vec::new();
    deltas.push(FastFeedDelta { symbol: "FLR/USD", price: flr });
    deltas.push(FastFeedDelta { symbol: "XRP/USD", price: xrp });
    deltas.push(FastFeedDelta { symbol: "BTC/USD", price: btc });

    let payload = FastUpdatePayload {
        block,
        reward_epoch: epoch as u32,
        deltas,
    };

    {
        let mut guard = shared_payload.write().await;
        *guard = Some(payload);
    }

    info!(
        "FAST-UPDATES | Payload vorbereitet fuer block {} (epoch {}) mit 3 deltas",
        block, epoch
    );
}

// ── VRF Proof Reader (Shared Memory /dev/shm) ───────────────────────────────

/// Liest den neuesten VRF-Proof (SortitionCredential) via Shared Memory (tmpfs).
/// Erwartet exakt 160 Bytes (5 x 32-Byte Big-Endian U256):
/// [replicate(32), gamma_x(32), gamma_y(32), c(32), s(32)]
fn read_vrf_proof_shm() -> Result<(U256, U256, U256, U256, U256)> {
    // Platform-unabhängiger Pfad für lokales Testen
    #[cfg(target_os = "windows")]
    let path = "C:\\Windows\\Temp\\flare_vrf_proof.bin";
    #[cfg(not(target_os = "windows"))]
    let path = "/dev/shm/flare_vrf_proof.bin";
    
    // Non-blocking read aus dem tmpfs
    let data = std::fs::read(path).map_err(|e| eyre::eyre!("SHM read failed: {}", e))?;
    
    if data.len() < 160 {
        return Err(eyre::eyre!("SHM file zu kurz: {} bytes", data.len()));
    }

    let replicate = U256::from_big_endian(&data[0..32]);
    let gamma_x   = U256::from_big_endian(&data[32..64]);
    let gamma_y   = U256::from_big_endian(&data[64..96]);
    let c         = U256::from_big_endian(&data[96..128]);
    let s         = U256::from_big_endian(&data[128..160]);

    Ok((replicate, gamma_x, gamma_y, c, s))
}

// ── submitUpdates als signierte Transaktion senden ─────────────────────────────
//
// Nimmt den vorbereiteten Payload, baut das FastUpdates-Struct (mit Platzhalter-
// SortitionCredential), signiert und sendet die Tx an IFastUpdater.
// HINWEIS: Bis ein gültiges SortitionCredential (z. B. vom Flare Entity/go-client)
// geliefert wird, wird die Tx on-chain revertieren. Der Ablauf (Payload → Encode →
// Sign → Send) ist damit finalisiert.

fn encode_deltas_as_bytes(payload: &FastUpdatePayload) -> Vec<u8> {
    use ethers::abi::{encode, Token};
    let decimals = 8u32;
    let scale = 10u64.pow(decimals);
    let flr = (payload.deltas.iter().find(|d| d.symbol == "FLR/USD").map(|d| d.price).unwrap_or(0.0) * scale as f64) as u64;
    let xrp = (payload.deltas.iter().find(|d| d.symbol == "XRP/USD").map(|d| d.price).unwrap_or(0.0) * scale as f64) as u64;
    let btc = (payload.deltas.iter().find(|d| d.symbol == "BTC/USD").map(|d| d.price).unwrap_or(0.0) * scale as f64) as u64;
    encode(&[Token::Array(vec![
        Token::Uint(U256::from(flr)),
        Token::Uint(U256::from(xrp)),
        Token::Uint(U256::from(btc)),
    ])])
}

async fn send_fast_update_submission<M: Middleware + 'static>(
    fast_updater: &FastUpdaterSubmitClient<M>,
    payload: FastUpdatePayload,
    wallet: &LocalWallet,
) -> Result<Option<TransactionReceipt>>
where
    M::Error: 'static,
{
    use ethers::abi::{encode, Token};
    use ethers::utils::keccak256;

    let sortition_block = U256::from(payload.block);
    
    // VRF-Proof via Shared Memory auslesen
    let (replicate, gamma_x, gamma_y, c, s) = match read_vrf_proof_shm() {
        Ok(proof) => proof,
        Err(e) => {
            warn!("FAST-UPDATES | VRF Proof fehlt oder ungueltig. Ueberspringe TX. Error: {}", e);
            return Ok(None);
        }
    };

    let deltas_bytes = encode_deltas_as_bytes(&payload);
    let deltas_token = Token::Bytes(deltas_bytes.clone());

    // Nachricht für Signatur: Autorisation dieses Senders für diesen Block
    let msg = ethers::utils::keccak256(
        ethers::abi::encode(&[
            Token::String("FastUpdates".into()),
            Token::Uint(sortition_block),
            Token::Address(wallet.address()),
        ]),
    );
    let sig = wallet.sign_hash(ethers::types::H256::from(msg))?;
    let v = sig.v;
    let mut r_bytes = [0u8; 32];
    sig.r.to_big_endian(&mut r_bytes);
    let mut s_bytes = [0u8; 32];
    sig.s.to_big_endian(&mut s_bytes);

    let sortition_credential = Token::Tuple(vec![
        Token::Uint(replicate),
        Token::Tuple(vec![Token::Uint(gamma_x), Token::Uint(gamma_y)]),
        Token::Uint(c),
        Token::Uint(s),
    ]);
    let signature = Token::Tuple(vec![
        Token::Uint((v as u64).into()),
        Token::FixedBytes(r_bytes.to_vec()),
        Token::FixedBytes(s_bytes.to_vec()),
    ]);
    let updates = Token::Tuple(vec![
        Token::Uint(sortition_block),
        sortition_credential,
        deltas_token,
        signature,
    ]);

    let calldata = encode(&[updates]);
    let selector = &keccak256(
        "submitUpdates((uint256,(uint256,(uint256,uint256),uint256,uint256),bytes,(uint8,bytes32,bytes32)))",
    )[0..4];
    let full_calldata: Vec<u8> = selector.iter().copied().chain(calldata.into_iter()).collect();

    let tx = TransactionRequest::default()
        .to(fast_updater.address())
        .data(ethers::types::Bytes::from(full_calldata))
        .gas(500_000u64)
        .gas_price(50_000_000_000u64); // 50 Gwei

    let pending = fast_updater.client().send_transaction(tx, None).await?;
    match pending.await? {
        Some(receipt) => {
            info!(
                "FAST-UPDATES | submitUpdates TX bestätigt block {} hash {:?} gas_used {:?}",
                receipt.block_number.unwrap_or_default(),
                receipt.transaction_hash,
                receipt.gas_used
            );
            Ok(Some(receipt))
        }
        None => {
            warn!("FAST-UPDATES | submitUpdates TX keine Receipt");
            Ok(None)
        }
    }
}

// ── FAssets CR-Check ──────────────────────────────────────────────────────────
//
// Einfache Heuristik: wenn FLR/USD-Preis deutlich fällt oder FAsset-Asset-Preis
// deutlich steigt, können Agenten unter 130% CR fallen.
//
// TODO: Für echte Liquidationen müssen Agent-Vaults via FAssets Subgraph/API
//       abgefragt werden. Hier ist der Framework für die spätere Integration.

fn check_fassets_opportunities(
    ftso: &FtsoSnapshot,
    prev_ftso: &Option<FtsoSnapshot>,
    min_profit_usd: f64,
) -> Vec<LiquidationOpp> {
    let mut opps = Vec::new();

    let Some(prev) = prev_ftso else { return opps; };

    // FLR Preisänderung seit letztem Block
    let flr_change_pct = (ftso.flr_usd - prev.flr_usd) / prev.flr_usd * 100.0;
    let xrp_change_pct = (ftso.xrp_usd - prev.xrp_usd) / prev.xrp_usd * 100.0;
    let btc_change_pct = (ftso.btc_usd - prev.btc_usd) / prev.btc_usd * 100.0;

    if ftso.block % 10 == 0 {
        info!("FTSO | FLR: ${:.5} ({:+.2}%) | XRP: ${:.4} ({:+.2}%) | BTC: ${:.0} ({:+.2}%)",
            ftso.flr_usd, flr_change_pct, ftso.xrp_usd, xrp_change_pct, ftso.btc_usd, btc_change_pct);
    }

    // Wenn FLR stark fällt (>2%) oder FAsset-Preis stark steigt (>1%):
    // → Agenten könnten unter CR 130% fallen → Liquidation opportunity
    let fxrp_stress = flr_change_pct < -2.0 || xrp_change_pct > 1.0;
    let fbtc_stress = flr_change_pct < -2.0 || btc_change_pct > 1.0;

    if fxrp_stress {
        warn!("STRESS SIGNAL FXRP: FLR {:+.2}% | XRP {:+.2}% — potenzielle Liquidationen!",
            flr_change_pct, xrp_change_pct);

        // Für jedes FXRP-Markt-Paar aus der Config:
        for (symbol, asset_manager, fasset_token, collateral) in FASSET_MARKETS {
            if *symbol != "FXRP" { continue; }
            if asset_manager.ends_with("000010") { continue; } // Skip Placeholder

            // Hier kommt später der echte CR-Check via IAssetManager.getAgentInfo()
            // Für jetzt: Signal loggen, Agent-Vault-List muss manuell gepflegt werden
            opps.push(LiquidationOpp {
                symbol: symbol.to_string(),
                asset_manager: asset_manager.to_string(),
                agent_vault: String::new(), // muss befüllt werden via Agent-Discovery
                fasset_token: fasset_token.to_string(),
                collateral: collateral.to_string(),
                cr_pct: 0.0, // unbekannt ohne on-chain CR-Query
                est_profit_usd: min_profit_usd * 1.5, // konservative Schätzung
            });
        }
    }

    if fbtc_stress {
        warn!("STRESS SIGNAL FBTC: FLR {:+.2}% | BTC {:+.2}% — potenzielle Liquidationen!",
            flr_change_pct, btc_change_pct);
    }

    opps
}

// ── Pool Discovery via Factory ────────────────────────────────────────────────

async fn discover_pools(provider: Arc<Provider<Http>>) {
    let factory_addr: Address = SPARKDEX_FACTORY.parse().unwrap();
    let factory = IUniswapV3Factory::new(factory_addr, provider.clone());

    let pairs: &[(&str, &str, u32)] = &[
        (WFLR, USDCE, 500),
        (WFLR, USDCE, 3000),
        (WFLR, SFLR,  500),
        (WFLR, SFLR,  3000),
    ];

    info!("=== SparkDEX V3.1 Pool Discovery ===");
    for (t0, t1, fee) in pairs {
        let a: Address = t0.parse().unwrap();
        let b: Address = t1.parse().unwrap();
        match timeout(Duration::from_secs(5), factory.get_pool(a, b, *fee as u32)).await {
            Ok(Ok(addr)) if addr != Address::zero() => {
                info!("  POOL: {}/{} {}bps → {:?}", t0, t1, fee/100, addr);
                info!("    → In POOLS[] eintragen und Placeholder ersetzen!");
            }
            Ok(Ok(_)) => warn!("  NICHT DEPLOYED: {}/{} {}bps", t0, t1, fee/100),
            _ => warn!("  QUERY FEHLGESCHLAGEN: {}/{} {}bps", t0, t1, fee/100),
        }
    }
    info!("=== Pool Discovery Ende ===");
}

// ── DEX Price Fetch ───────────────────────────────────────────────────────────

async fn fetch_price(
    provider: Arc<Provider<Http>>,
    pair: &'static str,
    dex_name: &'static str,
    pool_addr: &'static str,
    fee: u32,
) -> Option<Price> {
    // Skip Placeholders
    if pool_addr.ends_with("000001") || pool_addr.ends_with("000002")
        || pool_addr.ends_with("000003") || pool_addr.ends_with("000004") {
        return None;
    }

    let addr: Address = pool_addr.parse().ok()?;
    let pool = IUniswapV3Pool::new(addr, provider);

    let sqrt_raw: U256 = timeout(Duration::from_secs(2), pool.slot_0().call())
        .await.ok()?.ok()?.0;

    let sqrt = sqrt_raw.as_u128() as f64 / 2_u128.pow(96) as f64;

    let price = match pair {
        "WFLR/USDCE" => sqrt.powi(2) * 1e12, // 18dec vs 6dec
        "WFLR/SFLR"  => sqrt.powi(2),
        _            => sqrt.powi(2),
    };

    let valid = match pair {
        "WFLR/USDCE" => price > 0.001 && price < 1.0,
        "WFLR/SFLR"  => price > 0.9   && price < 1.1,
        _            => price > 0.0,
    };

    if !valid { return None; }

    Some(Price {
        pair:      pair.to_string(),
        dex_name:  dex_name.to_string(),
        pool_addr: pool_addr.to_string(),
        price,
        fee,
        fee_bps: fee / 100,
    })
}

async fn fetch_all_prices(rotator: &RpcRotator) -> Vec<Price> {
    let provider = rotator.get();
    let futures = POOLS.iter().map(|(pair, dex, addr, fee)| {
        fetch_price(provider.clone(), pair, dex, addr, *fee)
    });
    join_all(futures).await.into_iter().flatten().collect()
}

// ── Execute SimpleArb ─────────────────────────────────────────────────────────

async fn execute_simple_arb<M: Middleware + 'static>(
    flashbot: &IFlashBotFlare<M>,
    buy: &Price,
    sell: &Price,
    net_bps: u32,
    max_trade_usd: u64,
    min_profit_usd: f64,
    flr_price_usd: f64,
) -> bool {
    let (token_borrow, token_intermediate) = match buy.pair.as_str() {
        "WFLR/USDCE" => (USDCE, WFLR),
        "WFLR/SFLR"  => (SFLR,  WFLR),
        _            => (USDCE, WFLR),
    };

    let flr_usd = if flr_price_usd > 0.001 { flr_price_usd } else { 0.03 };
    let borrow_amount = match token_borrow {
        t if t == USDCE => U256::from(max_trade_usd * 1_000_000),
        _ => U256::from((max_trade_usd as f64 / flr_usd) as u64) * U256::exp10(18),
    };

    let arb_params = (
        token_borrow.parse::<Address>().unwrap(),
        token_intermediate.parse::<Address>().unwrap(),
        buy.fee as u32,
        sell.fee as u32,
        borrow_amount,
        U256::from((min_profit_usd * 1_000_000.0) as u64),
    );

    info!("ARB EXECUTE: {} | Buy {} @ {:.8} | Sell {} @ {:.8} | Net: {} bps",
        buy.pair, buy.dex_name, buy.price, sell.dex_name, sell.price, net_bps);

    // Hohe Priority Fee da Public Mempool (keine Sequencer auf Flare!)
    match flashbot
        .execute_simple_arb(arb_params)
        .gas_price(U256::from(50_000_000_000_u64)) // 50 Gwei — aggressiv für Backrun
        .send()
        .await
    {
        Ok(pending) => {
            info!("TX: {:?}", pending.tx_hash());
            match pending.await {
                Ok(Some(r)) => {
                    info!("CONFIRMED block #{} gas={}", r.block_number.unwrap_or_default(), r.gas_used.unwrap_or_default());
                    true
                }
                Ok(None)  => { warn!("No receipt"); false }
                Err(e)    => { error!("REVERTED: {}", e); false }
            }
        }
        Err(e) => { error!("SEND FAILED: {}", e); false }
    }
}

// ── Execute FAssets Liquidation ───────────────────────────────────────────────

async fn execute_fassets_liq<M: Middleware + 'static>(
    flashbot: &IFlashBotFlare<M>,
    opp: &LiquidationOpp,
    liq_amount_uba: U256,
    min_profit_usd: f64,
) -> bool {
    if opp.agent_vault.is_empty() {
        warn!("FAssets Liq: kein Agent Vault konfiguriert fuer {}", opp.symbol);
        return false;
    }

    let liq_params = (
        opp.asset_manager.parse::<Address>().unwrap(),
        opp.agent_vault.parse::<Address>().unwrap(),
        opp.fasset_token.parse::<Address>().unwrap(),
        liq_amount_uba,
        opp.collateral.parse::<Address>().unwrap(),
        3000u32,   // 0.3% fee fuer Collateral -> FAsset swap auf SparkDEX
        U256::from((min_profit_usd * 1_000_000.0) as u64),
    );

    info!("FASSETS LIQ: {} | Agent: {} | Amount: {} UBA",
        opp.symbol, opp.agent_vault, liq_amount_uba);

    // Sehr hohe Priority Fee — Backrun im selben Block wie FTSO-Update!
    match flashbot
        .execute_f_assets_liquidation(liq_params)
        .gas_price(U256::from(100_000_000_000_u64)) // 100 Gwei — Backrun priority
        .send()
        .await
    {
        Ok(pending) => {
            info!("FASSETS LIQ TX: {:?}", pending.tx_hash());
            match pending.await {
                Ok(Some(r)) => {
                    info!("LIQ CONFIRMED block #{} gas={}", r.block_number.unwrap_or_default(), r.gas_used.unwrap_or_default());
                    true
                }
                Ok(None)  => { warn!("No receipt"); false }
                Err(e)    => { error!("LIQ REVERTED: {}", e); false }
            }
        }
        Err(e) => { error!("LIQ SEND FAILED: {}", e); false }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()))
        .init();

    let pk             = env::var("PRIVATE_KEY").expect("PRIVATE_KEY required");
    let contract_addr  = env::var("FLASHBOT_FLARE")
        .unwrap_or_else(|_| "0x0000000000000000000000000000000000000000".to_string());
    let min_profit_bps = env::var("MIN_PROFIT_BPS").ok().and_then(|s| s.parse().ok()).unwrap_or(10u32);
    let min_profit_usd = env::var("MIN_PROFIT_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(0.10f64);
    let max_trade_usd  = env::var("MAX_TRADE_SIZE_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(1000u64);
    let live_trading   = env::var("ENABLE_LIVE_TRADING").map(|s| s == "true").unwrap_or(false);
    let log_every      = env::var("LOG_EVERY_N_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(10u64);
    let run_discovery  = env::var("RUN_POOL_DISCOVERY").map(|s| s == "true").unwrap_or(false);
    let enable_fast_submit = env::var("ENABLE_FAST_UPDATE_SUBMIT").map(|s| s == "true").unwrap_or(false);

    let wallet = pk.parse::<LocalWallet>()?.with_chain_id(14u64);
    let signing_addr = wallet.address();
    let mut rotator = RpcRotator::new().await?;
    let provider = rotator.get();
    let client = Arc::new(SignerMiddleware::new(provider.as_ref().clone(), wallet.clone()));
    let flashbot = IFlashBotFlare::new(contract_addr.parse::<Address>()?, client.clone());

    let fast_updater_addr: Address = FTSO_FAST_UPDATER.parse().expect("invalid FastUpdater address");
    let fast_updater = FastUpdaterSubmitClient::new(fast_updater_addr, client.clone());

    // Gemeinsamer CEX-Preis-State + vorbereiteter Fast-Update-Payload
    let cex_prices: SharedCexPrices = Arc::new(RwLock::new(CexPrices::default()));
    let fast_update_payload: SharedFastUpdatePayload = Arc::new(RwLock::new(None));

    // Hochgradig konkurrierte CEX WebSocket Listener
    {
        let shared = cex_prices.clone();
        tokio::spawn(async move {
            run_binance_ws(shared.clone()).await;
        });
        let shared2 = cex_prices.clone();
        tokio::spawn(async move {
            run_bybit_ws(shared2).await;
        });
    }

    info!("╔══════════════════════════════════════════════════════════════════════╗");
    info!("║  KOJO FLARE — FTSOv2 Oracle + FAssets Liquidation                 ║");
    info!("║  Version: 2026-02-25.1 | Chain: Flare Mainnet (14) | God Mode     ║");
    info!("╠══════════════════════════════════════════════════════════════════════╣");
    info!("║  Wallet:   {:?}  ║", signing_addr);
    info!("║  Contract: {}                         ║", contract_addr);
    if contract_addr == "0x0000000000000000000000000000000000000000" {
        info!("║  (FTSO-only: FLASHBOT_FLARE nicht gesetzt — Arb/Liq inaktiv)   ║");
    }
    info!("║  Min: {} bps / ${:.2} | Max Trade: ${} | Live: {}          ║",
        min_profit_bps, min_profit_usd, max_trade_usd,
        if live_trading { "JA - WARNUNG!" } else { "NEIN (dry-run)" });
    info!("║  FastUpdate Submit: {}                              ║",
        if enable_fast_submit { "JA" } else { "NEIN (ENABLE_FAST_UPDATE_SUBMIT=true)" });
    info!("║  FTSO: {} (FastUpdater)               ║", FTSO_FAST_UPDATER);
    info!("╚══════════════════════════════════════════════════════════════════════╝");

    if run_discovery {
        discover_pools(rotator.get()).await;
    }

    if !live_trading {
        info!("DRY-RUN MODUS: Setze ENABLE_LIVE_TRADING=true fuer echte Trades");
    }

    let mut prev_ftso: Option<FtsoSnapshot> = None;
    let mut last_trade    = Instant::now() - Duration::from_secs(3600);
    let mut opps_dex      = 0u64;
    let mut opps_fassets  = 0u64;
    let mut trades_ok     = 0u64;
    let mut blocks_seen   = 0u64;

    // WS verbinden
    let mut ws_provider: Option<Provider<Ws>> = None;
    for url in WS_URLS {
        match Provider::<Ws>::connect(url).await {
            Ok(p) => {
                info!("WS verbunden: {}", url);
                ws_provider = Some(p);
                break;
            }
            Err(e) => warn!("WS {} fehlgeschlagen: {}", url, e),
        }
    }

    match ws_provider {
        // ── EVENT-DRIVEN via WebSocket (newHeads) ───────────────────────────
        Some(ws) => {
            let mut stream = ws.subscribe_blocks().await?;
            info!("Subscribed zu newHeads — Flare ~1.8s Blockzeit — SUB-MILLISEKUNDEN LATENZ");

            while let Some(block) = stream.next().await {
                let block_num = block.number.unwrap_or_default().as_u64();
                blocks_seen += 1;

                // ── FTSOv2 Fast Updates: Sortition-/Belohnungs-Status (PRIMÄR) ─
                log_fast_update_state(rotator.get(), block_num, signing_addr).await;

                // Kandidaten-Payload fuer submitUpdates vorbereiten, falls Sortition passt
                prepare_fast_update_payload(
                    rotator.get(),
                    block_num,
                    signing_addr,
                    &cex_prices,
                    &fast_update_payload,
                )
                .await;

                // Payload sofort als signierte Tx senden, wenn aktiviert
                if enable_fast_submit {
                    let payload_opt = { fast_update_payload.write().await.take() };
                    if let Some(payload) = payload_opt {
                        match send_fast_update_submission(&fast_updater, payload, &wallet).await {
                            Ok(Some(_)) => {}
                            Ok(None) => {}
                            Err(e) => warn!("FAST-UPDATES | submit TX Fehler: {}", e),
                        }
                    }
                }

                // ── FTSOv2 Preis-Update (SEKUNDÄR — Monitoring / Heuristiken) ─
                let ftso = fetch_ftso_prices(rotator.get(), block_num).await;

                // ── FAssets CR-Check (auf FTSO-Preis-Änderung reagieren) ───
                if let Some(ref f) = ftso {
                    let liq_opps = check_fassets_opportunities(f, &prev_ftso, min_profit_usd);

                    for opp in &liq_opps {
                        opps_fassets += 1;
                        info!("FASSETS OPP #{}: {} | CR: {:.1}% | Est: ${:.2}",
                            opps_fassets, opp.symbol, opp.cr_pct, opp.est_profit_usd);

                        if !live_trading {
                            info!("DRY-RUN: Wuerde FAssets Liquidation ausfuehren");
                            continue;
                        }

                        if last_trade.elapsed() < Duration::from_secs(2) { continue; }

                        // Liquidations-Betrag: 1000 USD Wert als Startgrösse
                        let liq_amount = U256::from(1000u64 * 1_000_000); // USDC.e-skaliert

                        let ok = execute_fassets_liq(&flashbot, opp, liq_amount, min_profit_usd).await;
                        if ok {
                            trades_ok += 1;
                            last_trade = Instant::now();
                        }
                    }

                    prev_ftso = Some(f.clone());
                }

                // ── DEX Spread Check (sekundäre Strategie) ─────────────────
                let prices = fetch_all_prices(&rotator).await;

                if blocks_seen % log_every == 0 && !prices.is_empty() {
                    if let Some(ref f) = ftso {
                        info!("BLOCK #{} | FTSO: FLR=${:.5} XRP=${:.4} BTC=${:.0} | DEX Pools: {}",
                            block_num, f.flr_usd, f.xrp_usd, f.btc_usd, prices.len());
                    }
                }

                if prices.len() >= 2 && last_trade.elapsed() >= Duration::from_secs(3) {
                    let mut best: Option<(usize, usize, u32)> = None;
                    for i in 0..prices.len() {
                        for j in i+1..prices.len() {
                            if prices[i].pair != prices[j].pair { continue; }
                            let mid   = (prices[i].price + prices[j].price) / 2.0;
                            let gross = ((prices[i].price - prices[j].price).abs() / mid * 10000.0) as u32;
                            let fees  = prices[i].fee_bps + prices[j].fee_bps;
                            let net   = if gross > fees { gross - fees } else { 0 };
                            if net >= min_profit_bps {
                                if best.as_ref().map_or(true, |(_, _, s)| net > *s) {
                                    best = Some((i, j, net));
                                }
                            }
                        }
                    }

                    if let Some((i, j, net_bps)) = best {
                        opps_dex += 1;
                        let flr_price = ftso.as_ref().map(|f| f.flr_usd).unwrap_or(0.03);
                        let est = (max_trade_usd as f64 * net_bps as f64 / 10000.0) - 0.02;

                        info!("DEX OPP #{} [Block #{}]: {} | {} @ {:.8} <> {} @ {:.8} | Net: {} bps | Est: ${:.3}",
                            opps_dex, block_num, prices[i].pair,
                            prices[i].dex_name, prices[i].price,
                            prices[j].dex_name, prices[j].price, net_bps, est);

                        if est >= min_profit_usd && live_trading && last_trade.elapsed() >= Duration::from_secs(3) {
                            let (buy, sell) = if prices[i].price < prices[j].price {
                                (&prices[i], &prices[j])
                            } else {
                                (&prices[j], &prices[i])
                            };
                            let ok = execute_simple_arb(
                                &flashbot, buy, sell, net_bps,
                                max_trade_usd, min_profit_usd, flr_price,
                            ).await;
                            if ok {
                                trades_ok += 1;
                                last_trade = Instant::now();
                            }
                        } else if !live_trading {
                            debug!("DRY-RUN: Wuerde DEX Arb ausfuehren");
                        }
                    }
                }

                if blocks_seen % 50 == 0 { rotator.rotate(); }

                if blocks_seen % 100 == 0 {
                    info!("STATS: {} Bloecke | DEX Opps: {} | FAssets Opps: {} | Trades: {} OK",
                        blocks_seen, opps_dex, opps_fassets, trades_ok);
                }
            }
        }

        // ── POLLING FALLBACK ─────────────────────────────────────────────────
        None => {
            warn!("Kein WS verfuegbar — Polling mit 1s Intervall (Flare ~1.8s Bloecke)");
            let mut cycle = 0u64;

            loop {
                cycle += 1;
                let block_num = rotator.get().get_block_number().await.unwrap_or_default().as_u64();

                // Primär: Fast-Update Meta-Infos pro Block loggen
                log_fast_update_state(rotator.get(), block_num, signing_addr).await;

                prepare_fast_update_payload(
                    rotator.get(),
                    block_num,
                    signing_addr,
                    &cex_prices,
                    &fast_update_payload,
                )
                .await;

                if enable_fast_submit {
                    let payload_opt = { fast_update_payload.write().await.take() };
                    if let Some(payload) = payload_opt {
                        if let Err(e) = send_fast_update_submission(&fast_updater, payload, &wallet).await {
                            warn!("FAST-UPDATES | submit TX Fehler (polling): {}", e);
                        }
                    }
                }

                let ftso = fetch_ftso_prices(rotator.get(), block_num).await;

                if let Some(ref f) = ftso {
                    let liq_opps = check_fassets_opportunities(f, &prev_ftso, min_profit_usd);
                    for opp in &liq_opps {
                        opps_fassets += 1;
                        warn!("FASSETS OPP #{}: {} | Est: ${:.2}", opps_fassets, opp.symbol, opp.est_profit_usd);
                        if live_trading && last_trade.elapsed() >= Duration::from_secs(2) {
                            let liq_amount = U256::from(1000u64 * 1_000_000);
                            if execute_fassets_liq(&flashbot, opp, liq_amount, min_profit_usd).await {
                                trades_ok += 1;
                                last_trade = Instant::now();
                            }
                        }
                    }
                    prev_ftso = Some(f.clone());
                }

                let prices = fetch_all_prices(&rotator).await;

                if prices.len() >= 2 && last_trade.elapsed() >= Duration::from_secs(3) {
                    let mut best: Option<(usize, usize, u32)> = None;
                    for i in 0..prices.len() {
                        for j in i+1..prices.len() {
                            if prices[i].pair != prices[j].pair { continue; }
                            let mid   = (prices[i].price + prices[j].price) / 2.0;
                            let gross = ((prices[i].price - prices[j].price).abs() / mid * 10000.0) as u32;
                            let fees  = prices[i].fee_bps + prices[j].fee_bps;
                            let net   = if gross > fees { gross - fees } else { 0 };
                            if net >= min_profit_bps {
                                if best.as_ref().map_or(true, |(_, _, s)| net > *s) { best = Some((i, j, net)); }
                            }
                        }
                    }
                    if let Some((i, j, net_bps)) = best {
                        opps_dex += 1;
                        let flr_price = ftso.as_ref().map(|f| f.flr_usd).unwrap_or(0.03);
                        info!("DEX OPP #{}: {} net {} bps", opps_dex, prices[i].pair, net_bps);
                        if live_trading {
                            let (buy, sell) = if prices[i].price < prices[j].price { (&prices[i], &prices[j]) } else { (&prices[j], &prices[i]) };
                            if execute_simple_arb(&flashbot, buy, sell, net_bps, max_trade_usd, min_profit_usd, flr_price).await {
                                trades_ok += 1;
                                last_trade = Instant::now();
                            }
                        }
                    }
                }

                if cycle % 50 == 0 { rotator.rotate(); }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
    }

    Ok(())
}
