use std::fs;
use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use dotenvy::dotenv;
use ethers::prelude::*;
use ethers::utils::keccak256;
use eyre::Result;
use futures::StreamExt;
use tokio::time::timeout;
use tracing::{error, info, warn};

// ── Cache ────────────────────────────────────────────────────────────────────

fn load_borrower_cache() -> HashSet<Address> {
    let mut set = HashSet::new();
    if let Ok(data) = fs::read_to_string("morpho_borrowers.json") {
        for line in data.lines() {
            let s = line.trim().trim_matches('"').trim_matches(',').trim();
            if let Ok(addr) = s.parse::<Address>() { set.insert(addr); }
        }
    }
    set
}

fn save_borrower_cache(borrowers: &HashSet<Address>) {
    let lines: Vec<String> = borrowers.iter().map(|a| format!("{:?}", a)).collect();
    let _ = fs::write("morpho_borrowers.json", lines.join("\n"));
}

// ── ABI ───────────────────────────────────────────────────────────────────────

abigen!(
    IFtsoV2,
    r#"[
        function getFeedsById(bytes21[] feedIds) external view returns (uint256[] memory values, int8[] memory decimals, uint64 timestamp)
    ]"#
);

abigen!(
    IMorphoBlue,
    r#"[
        event SupplyCollateral(bytes32 indexed id, address indexed caller, address indexed onBehalf, uint256 assets)
        event BorrowCollateral(bytes32 indexed id, address indexed caller, address indexed onBehalf, uint256 assets)
        event Liquidate(bytes32 indexed id, address indexed caller, address indexed borrower, uint256 repaidAssets, uint256 repaidShares, uint256 seizedAssets, uint256 badDebtAssets, uint256 badDebtShares)
        function market(bytes32 id) external view returns (uint128 totalSupplyAssets, uint128 totalSupplyShares, uint128 totalBorrowAssets, uint128 totalBorrowShares, uint128 lastUpdate, uint128 fee)
        function position(bytes32 id, address user) external view returns (uint256 supplyShares, uint128 borrowShares, uint256 collateral)
        function idToMarketParams(bytes32 id) external view returns (address loanToken, address collateralToken, address oracle, address irm, uint256 lltv)
        function liquidate(bytes32 id, address borrower, uint256 seizedAssets, uint256 repaidShares, bytes calldata data) external returns (uint256, uint256)
        function feeRecipient() external view returns (address)
    ]"#
);

abigen!(
    IWalletRotatorToken,
    r#"[
        function approve(address spender, uint256 amount) external returns (bool)
    ]"#
);

abigen!(
    IMorphoLiquidator,
    r#"[
        function executeLiquidation(address borrower, uint128 repaidShares, uint256 minProfitUsdt0) external
    ]"#
);

// ── FTSO Feed IDs ─────────────────────────────────────────────────────────────

fn flr_usd_feed_id() -> [u8; 21] {
    let mut id = [0u8; 21];
    id[0] = 0x01;
    let name = b"FLR/USD";
    id[1..name.len() + 1].copy_from_slice(name);
    id
}

fn xrp_usd_feed_id() -> [u8; 21] {
    let mut id = [0u8; 21];
    id[0] = 0x01;
    let name = b"XRP/USD";
    id[1..name.len() + 1].copy_from_slice(name);
    id
}

// ── WalletRotator ─────────────────────────────────────────────────────────────

struct WalletRotator {
    clients: Vec<Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>>,
    index:   usize,
}

impl WalletRotator {
    fn new(pks_str: &str, provider: Arc<Provider<Http>>, chain_id: u64) -> Result<Self> {
        let mut clients = Vec::new();
        for key in pks_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let wallet = key.parse::<LocalWallet>()?.with_chain_id(chain_id);
            clients.push(Arc::new(SignerMiddleware::new(provider.clone(), wallet)));
        }
        if clients.is_empty() { return Err(eyre::eyre!("No valid keys")); }
        Ok(Self { clients, index: 0 })
    }

    fn get_next(&mut self) -> Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>> {
        let c = self.clients[self.index].clone();
        self.index = (self.index + 1) % self.clients.len();
        c
    }
}

// ── Config ──────────────────────────────────────────────────────────────────

const RPC_URLS: &[&str] = &[
    "http://127.0.0.1:9650/ext/bc/C/rpc",
    "https://flare-api.flare.network/ext/bc/C/rpc",
];

const WS_URLS: &[&str] = &[
    "ws://127.0.0.1:9650/ext/bc/C/ws",
    "wss://flare-api.flare.network/ext/bc/C/ws",
];

const FTSO_V2_PUBLIC:        &str = "0x7BDE3Df0624114eDB3A67dFe6753e62f4e7c1d20";
const FLASHBOT_FLARE:        &str = "0xF61CaBDD39FD8EDBca98C4bb705b6B3678874a02"; // v3
const MORPHO_BLUE_CORE:      &str = "0xf4346f5132e810f80a28487a79c7559d9797e8b0"; // ✅ on-chain verified 2026-03-01
const MORPHO_LIQUIDATOR_ADDR: &str = "0xdc640d3a1c1ef780024596d6006d388bdeee5576"; // ✅ v2 deployed 2026-03-01 (authorized: all 4 wallets)

// Active Morpho Blue Market IDs on Flare (from TX trace)
// loanToken=USDT0, collateral=FXRP, oracle=0x183f..., lltv=77%
const MARKET_ID_USDT0_FXRP: &str = "0x2f31ab3fc12d6d10d1de9e5c74053126f03ac1f80a2e6d69d36a411fef7d942f";

// ── RPC Rotator ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct RpcRotator {
    providers: Vec<Arc<Provider<Http>>>,
    index: usize,
}

impl RpcRotator {
    async fn new() -> Result<Self> {
        let mut providers = Vec::new();
        for url in RPC_URLS {
            if let Ok(p) = Provider::<Http>::try_from(*url) {
                providers.push(Arc::new(p));
            }
        }
        if providers.is_empty() { return Err(eyre::eyre!("No RPC")); }
        Ok(RpcRotator { providers, index: 0 })
    }

    fn get(&self) -> Arc<Provider<Http>> { self.providers[self.index].clone() }
    fn rotate(&mut self) { self.index = (self.index + 1) % self.providers.len(); }
}

// ── FTSO Price Fetch ──────────────────────────────────────────────────────────

async fn fetch_prices(rotator: &mut RpcRotator) -> (f64, f64) {
    let fallback_flr = env::var("FALLBACK_FLR_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(0.02);
    let addr: Address = match FTSO_V2_PUBLIC.parse() {
        Ok(a) => a,
        Err(_) => return (fallback_flr, 0.50),
    };
    for _ in 0..2 {
        let ftso = IFtsoV2::new(addr, rotator.get());
        match timeout(
            Duration::from_secs(2),
            ftso.get_feeds_by_id(vec![flr_usd_feed_id(), xrp_usd_feed_id()]).call(),
        ).await {
            Ok(Ok((values, decimals, _))) if values.len() >= 2 => {
                let parse = |v: U256, d: i8| -> f64 {
                    let f = v.low_u128() as f64;
                    if d >= 0 { f / 10f64.powi(d as i32) } else { f * 10f64.powi(-d as i32) }
                };
                let flr = parse(values[0], decimals[0]);
                let xrp = parse(values[1], decimals[1]);
                let flr = if flr > 0.001 && flr < 10.0 { flr } else { fallback_flr };
                let xrp = if xrp > 0.01 && xrp < 100.0 { xrp } else { 0.50 };
                return (flr, xrp);
            }
            _ => rotator.rotate(),
        }
    }
    warn!("FTSO fetch failed — using fallback FLR=${:.4}", fallback_flr);
    (fallback_flr, 0.50)
}

// ── Discover Borrowers via SupplyCollateral events ───────────────────────────

async fn discover_borrowers(
    rotator: &mut RpcRotator,
    morpho: Address,
    from_block: u64,
    to_block: u64,
) -> Vec<(Address, [u8; 32])> {
    // SupplyCollateral(bytes32 indexed id, address indexed caller, address indexed onBehalf, uint256 assets)
    let topic0 = H256::from(keccak256("SupplyCollateral(bytes32,address,address,uint256)"));
    let mut found = Vec::new();

    let filter = Filter::new()
        .address(morpho)
        .topic0(topic0)
        .from_block(from_block)
        .to_block(to_block);

    for _attempt in 0..3 {
        match timeout(Duration::from_secs(10), rotator.get().get_logs(&filter)).await {
            Ok(Ok(logs)) => {
                for log in &logs {
                    if log.topics.len() >= 4 {
                        let mut market_id = [0u8; 32];
                        market_id.copy_from_slice(&log.topics[1][..]);
                        let borrower = Address::from_slice(&log.topics[3][12..32]);
                        found.push((borrower, market_id));
                    }
                }
                break;
            }
            _ => rotator.rotate(),
        }
    }
    found
}

// ── Check position health ─────────────────────────────────────────────────────

async fn check_liquidatable(
    rotator: &mut RpcRotator,
    morpho: Address,
    market_id: [u8; 32],
    borrower: Address,
    xrp_price: f64,
    min_profit_usd: f64,
) -> Option<(f64, u128)> {
    let provider = rotator.get();
    let morpho_contract = IMorphoBlue::new(morpho, provider.clone());

    let pos = timeout(Duration::from_secs(3),
        morpho_contract.position(market_id, borrower).call()
    ).await.ok()?.ok()?;

    let borrow_shares = pos.1; // u128
    if borrow_shares == 0 { return None; }

    let collateral = pos.2; // u256 — in FXRP (6 decimals, NOT 18!)
    let collateral_fxrp = collateral.low_u128() as f64 / 1e6;

    let market = timeout(Duration::from_secs(3),
        morpho_contract.market(market_id).call()
    ).await.ok()?.ok()?;

    let total_borrow_assets = market.2 as f64;
    let total_borrow_shares = market.3 as f64;
    if total_borrow_shares == 0.0 { return None; }

    // Convert shares to assets
    let borrow_assets_usdt0 = borrow_shares as f64 * total_borrow_assets / total_borrow_shares;
    let borrow_usd = borrow_assets_usdt0 / 1e6; // USDT0 has 6 decimals

    // Collateral value in USD
    let collateral_usd = collateral_fxrp * xrp_price;

    // LLTV = 77% → liquidatable when collateral_usd < borrow_usd / 0.77
    let lltv = 0.77;
    let health_factor = if borrow_usd > 0.0 { collateral_usd * lltv / borrow_usd } else { f64::MAX };

    if health_factor < 1.0 {
        // Liquidation bonus ~5%
        let seizable_collateral_usd = borrow_usd * 1.05;
        let profit_usd = seizable_collateral_usd - borrow_usd;
        if profit_usd >= min_profit_usd {
            return Some((profit_usd, borrow_shares));
        }
    }
    None
}

// ── State ───────────────────────────────────────────────────────────────────

struct EngineState {
    known_borrowers: HashSet<Address>,
    blocks_seen:     u64,
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()))
        .init();

    let live_trading   = env::var("ENABLE_LIVE_TRADING").map(|s| s == "true").unwrap_or(false);
    let min_profit_usd: f64 = env::var("MIN_PROFIT_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(0.01);
    let pks = env::var("PRIVATE_KEYS").unwrap_or_else(|_| env::var("PRIVATE_KEY").expect("PRIVATE_KEYS required"));

    let mut rotator = RpcRotator::new().await?;
    let mut wallet_rotator = WalletRotator::new(&pks, rotator.get(), 14u64)?;

    let morpho: Address = MORPHO_BLUE_CORE.parse()?;
    let market_id_bytes = {    let s = MARKET_ID_USDT0_FXRP.trim_start_matches("0x");    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i+2], 16).unwrap()).collect::<Vec<u8>>()};
    let mut market_id = [0u8; 32];
    market_id.copy_from_slice(&market_id_bytes);

    info!("╔══════════════════════════════════════════════════════════════════════╗");
    info!("║  MORPHO ENGINE — Bot 5: Morpho Blue Liquidator                       ║");
    info!("║  Chain: Flare Mainnet (14)                                           ║");
    info!("╠══════════════════════════════════════════════════════════════════════╣");
    info!("║  Morpho Blue:    {} ✅          ║", MORPHO_BLUE_CORE);
    info!("║  Liquidator:     {} ✅  ║", MORPHO_LIQUIDATOR_ADDR);
    info!("║  Price Source:   FTSOv2 getFeedsById                                 ║");
    info!("║  Live Trading:   {}                                                   ║", if live_trading { "YES ← REAL MONEY" } else { "NO  (dry-run)" });
    info!("║  Min Profit:     ${:.4}                                              ║", min_profit_usd);
    info!("╚══════════════════════════════════════════════════════════════════════╝");

    let mut state = EngineState {
        known_borrowers: load_borrower_cache(),
        blocks_seen:     0,
    };
    info!("Loaded {} borrowers from cache", state.known_borrowers.len());

    // Seed from historical SupplyCollateral events
    if let Ok(tip) = rotator.get().get_block_number().await {
        let tip_u64 = tip.as_u64();
        let from = if state.known_borrowers.is_empty() {
            tip_u64.saturating_sub(200_000)
        } else {
            tip_u64.saturating_sub(10_000)
        };
        info!("Seeding borrowers from block {} to {} ...", from, tip_u64);
        let mut cur = from;
        while cur < tip_u64 {
            let end = (cur + 50_000).min(tip_u64);
            let seed = discover_borrowers(&mut rotator, morpho, cur, end).await;
            for (b, _) in seed { state.known_borrowers.insert(b); }
            cur = end + 1;
        }
        info!("Seeded {} unique borrowers", state.known_borrowers.len());
        save_borrower_cache(&state.known_borrowers);
    }

    // WS block stream
    let mut ws_provider: Option<Provider<Ws>> = None;
    for url in WS_URLS {
        if let Ok(p) = Provider::<Ws>::connect(*url).await {
            info!("WS connected: {}", url);
            ws_provider = Some(p);
            break;
        }
    }

    let ws = match ws_provider {
        Some(p) => p,
        None => { error!("No WS — engine halted."); return Ok(()); }
    };

    let mut stream = ws.subscribe_blocks().await?;
    while let Some(block) = stream.next().await {
        let block_num = block.number.unwrap_or_default().as_u64();
        state.blocks_seen += 1;

        let (flr_price, xrp_price) = fetch_prices(&mut rotator).await;

        // Discover new borrowers from this block
        let new_entries = discover_borrowers(&mut rotator, morpho, block_num, block_num).await;
        let mut new_found = false;
        for (b, _) in new_entries {
            if state.known_borrowers.insert(b) {
                info!("⚡ New Morpho borrower in Block {}: {:?}", block_num, b);
                new_found = true;
            }
        }
        if new_found { save_borrower_cache(&state.known_borrowers); }

        if state.blocks_seen % 10 == 0 {
            info!("BLOCK #{} | Morpho Engine | FTSO FLR: ${:.5} XRP: ${:.4} | Borrowers: {}",
                block_num, flr_price, xrp_price, state.known_borrowers.len());
        }

        // Check all known borrowers every block for liquidation opportunity
        let borrowers: Vec<Address> = state.known_borrowers.iter().cloned().collect();
        for borrower in borrowers {
            if let Some((profit_usd, borrow_shares)) = check_liquidatable(
                &mut rotator, morpho, market_id, borrower, xrp_price, min_profit_usd
            ).await {
                warn!("🚨 LIQUIDATABLE: {:?} | profit ~${:.4} | borrow_shares={}", borrower, profit_usd, borrow_shares);
                if live_trading {
                    let client = wallet_rotator.get_next();
                    let liquidator_addr: Address = MORPHO_LIQUIDATOR_ADDR.parse().expect("valid addr");
                    let liquidator = IMorphoLiquidator::new(liquidator_addr, client.clone());
                    // executeLiquidation: flashloan USDT0 from Morpho → liquidate → swap FXRP→USDT0 → profit
                    // minProfitUsdt0 = 1 USDT0 raw (1_000_000 = 1.0 USDT0 with 6 dec)
                    match liquidator
                        .execute_liquidation(borrower, borrow_shares, U256::from(1_000_000u64))
                        .send().await
                    {
                        Ok(pending) => {
                            info!("✅ Liquidation TX sent: {:?}", pending.tx_hash());
                            let _ = pending.await;
                        }
                        Err(e) => error!("❌ Liquidation TX failed: {}", e),
                    }
                }
            }
        }

        if state.blocks_seen % 50 == 0 { rotator.rotate(); }
    }

    Ok(())
}
