// KINETIC ENGINE — Bot 2: Compound V2 Liquidation via SparkDEX V4 Flash Swaps
// Version: 2026-03-01.1  (TICKET-018: alle 7 kToken-Märkte — kUSDT0/kUSDT/kSFLR/kWETH/kFLRETH)
// Chain:   Flare Mainnet (Chain ID 14)
//
// FLOW (executed atomically per block):
//   1. Borrow-Rate Safeguard  — if borrowRatePerTimestamp >= 4.5e12 on ANY kToken,
//                               skip the whole block (saves gas on guaranteed reverts).
//                               NOTE: Kinetic Flare uses borrowRatePerTimestamp()
//                               (not borrowRatePerBlock()) — Flare is timestamp-based.
//   2. Discover Borrowers     — scan Borrow events on kUSDC.E (and kFLR if configured)
//                               and maintain an in-memory HashSet of tracked accounts.
//   3. Shortfall Scan         — getAccountLiquidity() for every tracked account;
//                               filter to those with shortfall > 0.
//   4. Fee-Aware Eval         — off-chain math:
//                               liquidation_premium > flash_fee + gas_cost?
//   5. Execute                — broadcast KineticLiquidator.liquidate(LiqParams)
//                               if ENABLE_LIVE_TRADING=true.
//
// ENV vars (load from .env):
//   PRIVATE_KEY              — EOA private key (must be owner() of KineticLiquidator)
//   KINETIC_LIQUIDATOR_ADDR  — Deployed KineticLiquidator contract address
//   V4_WFLR_USDCE_POOL       — SparkDEX V4 WFLR/USDC.e pool (default: hardcoded)
//   KINETIC_KFLR             — kFLR kToken address (optional; defaults to hardcoded mainnet address)
//                              Verified on-chain: 0xb84F771305d10607Dd086B2f89712c0CeD379407
//                              NOTE: old env var name KINETIC_KWFLR is also accepted (backward compat)
//   ENABLE_LIVE_TRADING      — "true" to broadcast real transactions
//   MIN_PROFIT_USD           — Minimum net profit in USD per liquidation (default: 0.50)
//   MAX_REPAY_USD            — Hard cap on repayAmount per trade in USD   (default: 5000)
//   LOG_LEVEL                — tracing filter string                       (default: "info")

use ethers::prelude::*;
use ethers::utils::keccak256;
use eyre::Result;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};
use std::env;
use std::fs;
use dotenvy::dotenv;
use std::io::Write;
use reqwest::Client;
use serde_json::json;

// ── Cache ────────────────────────────────────────────────────────────────────

fn load_borrower_cache() -> HashSet<Address> {
    let mut set = HashSet::new();
    if let Ok(data) = fs::read_to_string("borrowers.json") {
        for line in data.lines() {
            let s = line.trim().trim_matches('"').trim_matches(',').trim();
            if let Ok(addr) = s.parse::<Address>() {
                set.insert(addr);
            }
        }
    }
    set
}

fn save_borrower_cache(borrowers: &HashSet<Address>) {
    let lines: Vec<String> = borrowers.iter().map(|a| format!("{:?}", a)).collect();
    let _ = fs::write("borrowers.json", lines.join("\n"));
}

// ── RPC Endpoints ─────────────────────────────────────────────────────────────

const RPC_URLS: &[&str] = &[
    "http://127.0.0.1:9650/ext/bc/C/rpc",           // primary: local go-flare (<1 ms)
    "https://flare-api.flare.network/ext/bc/C/rpc", // fallback 1
    "https://flare.drpc.org",                        // fallback 2
];

const WS_URLS: &[&str] = &[
    "ws://127.0.0.1:9650/ext/bc/C/ws",
    "wss://flare-api.flare.network/ext/bc/C/ws",
];

// ── Verified Flare Mainnet Addresses ──────────────────────────────────────────

const WFLR:  &str = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d";
const USDCE: &str = "0xFbDa5F676cB37624f28265A144A48B0d6e87d3b6";

// SparkDEX V4 (Algebra Integral) — verified on Flare Mainnet explorer
const V4_WFLR_USDCE_POOL: &str = "0x9af682206Ee6f6651cb09ca0DECd169B2E861D06";

// Kinetic (Compound V2 fork) — verified via getAllMarkets() on Flare Mainnet
// IMPORTANT: Previous values were WRONG (had no code / wrong symbol). These are verified.
const KINETIC_UNITROLLER: &str = "0x8041680Fb73E1Fe5F851e76233DCDfA0f2D2D7c8"; // getAllMarkets() confirmed
const KINETIC_KUSDCE:     &str = "0xDEeBaBe05BDA7e8C1740873abF715f16164C29B8"; // symbol() = "kUSDC.E", underlying USDC.e (6 dec)
const KINETIC_KFLR:       &str = "0xb84F771305d10607Dd086B2f89712c0CeD379407"; // symbol() = "kFLR",    underlying WFLR (18 dec)
const KINETIC_KUSDT:      &str = "0x1e5bBC19E0B17D7d38F318C79401B3D16F2b93bb"; // symbol() = "kUSDT",   underlying USDT (6 dec)
const KINETIC_KSFLR:      &str = "0x291487beC339c2fE5D83DD45F0a15EFC9Ac45656"; // symbol() = "kSFLR",   underlying sFLR (18 dec)
const KINETIC_KWETH:      &str = "0x5C2400019017AE61F811D517D088Df732642DbD0"; // symbol() = "kWETH",   underlying WETH (18 dec)
const KINETIC_KFLRETH:    &str = "0x40eE5dfe1D4a957cA8AC4DD4ADaf8A8fA76b1C16"; // symbol() = "kFLRETH", underlying flrETH (18 dec)
const KINETIC_KUSDT0:     &str = "0x76809aBd690B77488Ffb5277e0a8300a7e77B779"; // symbol() = "kUSDT0",  underlying USDT0 (6 dec)

// Underlying tokens for new markets (verified on-chain 2026-03-01)
const USDT:   &str = "0x0B38e83B86d491735fEaa0a791F65c2B99535396"; // USDT (6 dec)
const SFLR:   &str = "0x12e605bc104e93B45e1aD99F9e555f659051c2BB"; // sFLR (18 dec)
const WETH:   &str = "0x1502FA4be69d526124D453619276FacCab275d3D"; // WETH (18 dec)
const FLRETH: &str = "0x26A1faB310bd080542DC864647d05985360B16A5"; // flrETH (18 dec)
const USDT0:  &str = "0xe7cd86e13AC4309349F30B3435a9d337750fC82D"; // USDT0 (6 dec)

// FTSOv2 Addresses — Flare Mainnet
// FastUpdater is provider-only (submitUpdates); Consumer reads use FtsoV2 (getFeedsById)
const FTSO_FAST_UPDATER: &str = "0xdBF71d7840934EB82FA10173103D4e9fd4054dd1"; // provider endpoint
const FTSO_V2_PUBLIC: &str = "0x7BDE3Df0624114eDB3A67dFe6753e62f4e7c1d20"; // consumer endpoint

// ── Safety & Fee Constants ─────────────────────────────────────────────────────
//
// Kinetic Flare uses borrowRatePerTimestamp() — rates are per second, not per block.
// borrowRateMaxMantissa from Compound V2 calibrated for Ethereum 12s blocks = 5e12.
// On Flare's 1-second timestamps Kinetic likely retains a similar cap value.
// We conservatively skip at 90% (4.5e12) to avoid accrueInterest() reverts.
const BORROW_RATE_SAFEGUARD: u64 = 4_500_000_000_000;

// Conservative Algebra V4 flash fee estimate: 0.05% = 500 / 1_000_000.
// Used only for off-chain profitability filtering; the contract enforces the real fee.
const FLASH_FEE_PPM: u128 = 500;

// Gas estimate for a full liquidation call (flash + liquidateBorrow + redeem + swap + repay).
const GAS_ESTIMATE: u64 = 500_000;

// ── RPC Retry & Pacing ──────────────────────────────────────────────────────
const MAX_RPC_RETRIES: u32 = 10;
const RPC_PACE_MS: u64 = 0;  // FULL THROTTLE
const RPC_BACKOFF_BASE_MS: u64 = 0; // NO BACKOFF
const RPC_BACKOFF_CAP_MS: u64 = 0;

// ── Wallet & Client Rotator ───────────────────────────────────────────────
struct WalletRotator {
    clients: Vec<Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>>,
    index: usize,
}

impl WalletRotator {
    fn new(pks_str: &str, provider: Arc<Provider<Http>>, chain_id: u64) -> Result<Self> {
        let mut clients = Vec::new();
        let keys = pks_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty());
        for key in keys {
            let wallet = key.parse::<LocalWallet>()?.with_chain_id(chain_id);
            let client = Arc::new(SignerMiddleware::new(provider.clone(), wallet));
            clients.push(client);
        }
        if clients.is_empty() {
            return Err(eyre::eyre!("No valid private keys provided"));
        }
        Ok(Self { clients, index: 0 })
    }

    fn get_next(&mut self) -> Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>> {
        let client = self.clients[self.index].clone();
        self.index = (self.index + 1) % self.clients.len();
        client
    }
}
//
// Default: 50 Gwei Priority Tip for liquidations, configurable via env:
//   KINETIC_PRIORITY_FEE_WEI=50000000000
fn kinetic_priority_tip_wei() -> U256 {
    let default_tip: u64 = 50_000_000_000;
    let tip = env::var("KINETIC_PRIORITY_FEE_WEI")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default_tip);
    U256::from(tip)
}

// ── Discord Webhook ─────────────────────────────────────────────────────────

async fn notify_discord_kinetic_trade(
    profit_usd: f64,
    gas_used: U256,
    tx_hash: H256,
) {
    let url = match env::var("DISCORD_WEBHOOK_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => return,
    };

    let client = reqwest::Client::new();
    let content = format!(
        "🎯 KINETIC LIQ CONFIRMED | Profit: ${:.4} | Gas used: {} | Tx: 0x{:x}",
        profit_usd,
        gas_used,
        tx_hash
    );

    let _ = client
        .post(url)
        .json(&json!({ "content": content }))
        .send()
        .await;
}

// ── ABI Definitions ───────────────────────────────────────────────────────────

// Kinetic Unitroller / Comptroller (Compound V2 fork)
abigen!(IKineticComptroller, r#"[
    function getAccountLiquidity(address account) external view returns (uint256 err, uint256 liquidity, uint256 shortfall)
    function closeFactorMantissa() external view returns (uint256)
    function liquidationIncentiveMantissa() external view returns (uint256)
]"#);

// Kinetic kToken (CErc20Immutable — Compound V2 fork, Flare-adapted)
// NOTE: Kinetic on Flare uses borrowRatePerTimestamp() instead of borrowRatePerBlock()
// because Flare is timestamp-based. Verified on-chain: borrowRatePerBlock() reverts with 0x.
abigen!(IKToken, r#"[
    function borrowRatePerTimestamp() external view returns (uint256)
    function getAccountSnapshot(address account) external view returns (uint256 err, uint256 cTokenBalance, uint256 borrowBalance, uint256 exchangeRate)
]"#);

// KineticLiquidator.sol — our deployed contract
// LiqParams struct fields must match the Solidity struct exactly.
abigen!(IKineticLiquidator, r#"[
    {
        "type": "function",
        "name": "liquidate",
        "inputs": [{
            "name": "p",
            "type": "tuple",
            "components": [
                {"name": "flashPool",        "type": "address"},
                {"name": "borrower",         "type": "address"},
                {"name": "repayAmount",      "type": "uint256"},
                {"name": "kTokenBorrow",     "type": "address"},
                {"name": "kTokenCollateral", "type": "address"},
                {"name": "tokenBorrow",      "type": "address"},
                {"name": "tokenCollateral",  "type": "address"},
                {"name": "minProfit",        "type": "uint256"},
                {"name": "swapDeadline",     "type": "uint256"}
            ]
        }],
        "outputs": [],
        "stateMutability": "nonpayable"
    }
]"#);

// FTSOv2 Public Consumer — getFeedsById for FLR/USD
abigen!(IFtsoV2, r#"[
    function getFeedsById(bytes21[] feedIds) external view returns (uint256[] memory values, int8[] memory decimals, uint64 timestamp)
]"#);

// ── Domain Types ──────────────────────────────────────────────────────────────

/// Cached Kinetic protocol parameters (refresh every ~2 min to catch governance changes).
#[derive(Clone, Debug)]
struct ComptrollerConfig {
    close_factor:          U256, // 0.5e18 = 50%: max fraction of debt we can repay
    liquidation_incentive: U256, // 1.1e18 = 10% bonus — verified on Flare Mainnet
}

impl Default for ComptrollerConfig {
    fn default() -> Self {
        Self {
            close_factor:          U256::from(500_000_000_000_000_000_u64),    // 0.5e18
            liquidation_incentive: U256::from(1_100_000_000_000_000_000_u128), // 1.1e18 — verified
        }
    }
}

/// An account confirmed to have positive shortfall, plus the borrow details needed
/// to build the LiqParams struct for our contract call.
#[derive(Clone, Debug)]
struct ShortfallAccount {
    borrower:           Address,
    shortfall_usd_e18:  U256,   // Compound's internal USD shortfall (1e18 = $1)
    k_token_borrow:     Address,
    borrow_balance:     U256,   // repayable amount (capped at closeFactor), underlying units
    token_borrow:       Address,
    k_token_collateral: Address,
    token_collateral:   Address,
}

/// A profitable liquidation opportunity, ready for execution.
#[derive(Clone, Debug)]
struct LiqOpportunity {
    account:        ShortfallAccount,
    repay_amount:   U256, // underlying units of tokenBorrow (≤ borrow_balance, ≤ max_repay cap)
    min_profit:     U256, // underlying units of tokenBorrow (covers gas + buffer for contract)
    est_profit_usd: f64,
    flash_pool:     Address,
    swap_deadline:  U256, // UNIX seconds: block.timestamp + 60
}

/// EIP-1559 gas parameters fetched from the latest block.
#[derive(Clone, Debug)]
struct GasParams {
    base_fee_wei: U256,
    priority_fee: U256,
}

/// Mutable engine state threaded through the per-block processing function.
struct EngineState {
    known_borrowers:   HashSet<Address>,
    cfg:               ComptrollerConfig,
    last_cfg_refresh:  Instant,
    last_trade:        Instant,
    blocks_seen:       u64,
    shortfalls_seen:   u64,
    trades_ok:         u64,
}

impl EngineState {
    fn new(initial_cfg: ComptrollerConfig) -> Self {
        EngineState {
            known_borrowers:  HashSet::new(),
            cfg:              initial_cfg,
            last_cfg_refresh: Instant::now(),
            last_trade:       Instant::now() - Duration::from_secs(3600),
            blocks_seen:      0,
            shortfalls_seen:  0,
            trades_ok:        0,
        }
    }
}

// ── RPC Rotator ───────────────────────────────────────────────────────────────

fn is_retryable_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("throttl")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection")
        || lower.contains("reset by peer")
        || lower.contains("broken pipe")
        || lower.contains("eof")
}

#[derive(Clone)]
struct RpcRotator {
    providers:          Vec<Arc<Provider<Http>>>,
    index:              usize,
    consecutive_errors: u32,
    last_request:       Instant,
}

impl RpcRotator {
    async fn new() -> Result<Self> {
        let mut providers = Vec::new();
        
        let urls: Vec<String> = if let Ok(custom) = env::var("CUSTOM_RPC_URLS") {
            custom.split(',').map(|s| s.trim().to_string()).collect()
        } else {
            RPC_URLS.iter().map(|&s| s.to_string()).collect()
        };

        for url in &urls {
            if let Ok(p) = Provider::<Http>::try_from(url.as_str()) {
                providers.push(Arc::new(p));
            }
        }
        if providers.is_empty() {
            return Err(eyre::eyre!("No HTTP RPC available"));
        }
        info!("HTTP RPC: {} endpoints loaded", providers.len());
        Ok(RpcRotator {
            providers,
            index: 0,
            consecutive_errors: 0,
            last_request: Instant::now() - Duration::from_secs(1),
        })
    }

    fn get(&self) -> Arc<Provider<Http>> {
        self.providers[self.index].clone()
    }

    fn rotate(&mut self) {
        self.index = (self.index + 1) % self.providers.len();
    }

    fn rotate_on_error(&mut self) {
        self.consecutive_errors += 1;
        let old = self.index;
        self.rotate();
        warn!(
            "RPC rotated #{} → #{} (consecutive errors: {})",
            old, self.index, self.consecutive_errors
        );
    }

    fn mark_success(&mut self) {
        self.consecutive_errors = 0;
    }

    fn backoff_ms(&self) -> u64 {
        (RPC_BACKOFF_BASE_MS * 2u64.pow(self.consecutive_errors.min(4)))
            .min(RPC_BACKOFF_CAP_MS)
    }

    async fn pace(&mut self) {
        let pace = if self.consecutive_errors > 0 {
            Duration::from_millis(RPC_PACE_MS * 3)
        } else {
            Duration::from_millis(RPC_PACE_MS)
        };
        let elapsed = self.last_request.elapsed();
        if elapsed < pace {
            tokio::time::sleep(pace - elapsed).await;
        }
        self.last_request = Instant::now();
    }
}

// ── Safety: 1-Second Bug Safeguard ───────────────────────────────────────────
//
// Kinetic uses Compound V2's JumpRateModel with borrowRateMaxMantissa = 5e12,
// calibrated for Ethereum's 12-second blocks. On Flare's 1-second blocks the
// same mantissa value is reached at 1/12 the utilization → accrueInterest()
// will revert. We conservatively skip at 90% of max (4.5e12).
//
// Returns true  → safe to proceed with liquidations
// Returns false → skip this block entirely
async fn check_borrow_rate_safeguard(
    rotator: &mut RpcRotator,
    k_tokens: &[Address],
) -> bool {
    for &addr in k_tokens {
        rotator.pace().await;
        let mut got_result = false;
        for attempt in 0..=MAX_RPC_RETRIES {
            let ktoken = IKToken::new(addr, rotator.get());
            match timeout(Duration::from_secs(2), ktoken.borrow_rate_per_timestamp().call()).await {
                Ok(Ok(rate)) => {
                    rotator.mark_success();
                    let rate_u64 = rate.min(U256::from(u64::MAX)).as_u64();
                    debug!("borrowRatePerTimestamp {:?} = {}", addr, rate_u64);
                    if rate_u64 >= BORROW_RATE_SAFEGUARD {
                        warn!(
                            "SAFEGUARD TRIGGERED: borrowRatePerTimestamp={} >= {} on {:?} — block skipped",
                            rate_u64, BORROW_RATE_SAFEGUARD, addr
                        );
                        return false;
                    }
                    got_result = true;
                    break;
                }
                Ok(Err(e)) => {
                    let err_s = format!("{}", e);
                    if is_retryable_error(&err_s) && attempt < MAX_RPC_RETRIES {
                        rotator.rotate_on_error();
                        tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                    } else {
                        warn!("borrowRatePerTimestamp {:?}: {} (non-retryable)", addr, e);
                        break;
                    }
                }
                Err(_) => {
                    if attempt < MAX_RPC_RETRIES {
                        rotator.rotate_on_error();
                        tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                    } else {
                        warn!("borrowRatePerTimestamp {:?}: timeout after {} retries", addr, MAX_RPC_RETRIES);
                    }
                }
            }
        }
        if !got_result {
            warn!("borrowRatePerTimestamp {:?}: could not verify — assuming safe", addr);
        }
    }
    true
}

// ── Gas: EIP-1559 Parameters ──────────────────────────────────────────────────

async fn fetch_gas_params(rotator: &mut RpcRotator) -> GasParams {
    let fallback = GasParams {
        base_fee_wei: U256::from(25_000_000_000_u64), // 25 Gwei baseline
        priority_fee: kinetic_priority_tip_wei(),      // configurable priority tip
    };
    for attempt in 0..=MAX_RPC_RETRIES {
        rotator.pace().await;
        match timeout(Duration::from_secs(2), rotator.get().get_block(BlockNumber::Latest)).await {
            Ok(Ok(Some(block))) => {
                rotator.mark_success();
                return GasParams {
                    base_fee_wei: block.base_fee_per_gas.unwrap_or(fallback.base_fee_wei),
                    priority_fee: fallback.priority_fee,
                };
            }
            Ok(Err(e)) => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                } else {
                    warn!("fetch_gas_params failed after retries: {}", e);
                }
            }
            _ => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                }
            }
        }
    }
    fallback
}

// ── Price: FLR/USD via FTSOv2 FastUpdater ─────────────────────────────────────

fn flr_usd_feed_id() -> [u8; 21] {
    let mut id = [0u8; 21];
    id[0] = 0x01;
    let name = b"FLR/USD";
    id[1..name.len() + 1].copy_from_slice(name);
    id
}

async fn fetch_flr_price_usd(rotator: &mut RpcRotator) -> f64 {
    // Primary source: FTSOv2 Public Consumer (getFeedsById — block-latency feed)
    let addr: Address = match FTSO_V2_PUBLIC.parse() {
        Ok(a) => a,
        Err(_) => {
            // Hard fallback when address cannot be parsed at all.
            let fb = std::env::var("FALLBACK_FLR_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(0.02);
            warn!("FTSO_V2_PUBLIC address invalid — using FALLBACK_FLR_USD=${:.4}", fb);
            return fb;
        }
    };

    for attempt in 0..=MAX_RPC_RETRIES {
        rotator.pace().await;
        let ftso = IFtsoV2::new(addr, rotator.get());
        match timeout(
            Duration::from_secs(2),
            ftso.get_feeds_by_id(vec![flr_usd_feed_id()]).call(),
        )
        .await
        {
            Ok(Ok((values, decimals, _))) => {
                if values.is_empty() {
                    if attempt < MAX_RPC_RETRIES {
                        rotator.rotate_on_error();
                        continue;
                    }
                } else {
                    rotator.mark_success();
                    let v = values[0].as_u128() as f64;
                    let d = decimals[0];
                    let price = if d >= 0 {
                        v / 10f64.powi(d as i32)
                    } else {
                        v * 10f64.powi(-d as i32)
                    };
                    return if price > 0.001 && price < 10.0 { price } else { 0.02 };
                }
            }
            Ok(Err(e)) => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                } else {
                    debug!("fetch_flr_price_usd: FastUpdater error after retries: {} — falling back", e);
                }
            }
            Err(_) => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                } else {
                    debug!("fetch_flr_price_usd: FastUpdater timeout after retries — falling back");
                }
            }
        }
    }

    // Bulletproof fallback: static FLR/USD from env or conservative default.
    let fb = std::env::var("FALLBACK_FLR_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(0.02);
    warn!("fetch_flr_price_usd: using FALLBACK_FLR_USD=${:.4}", fb);
    fb
}

// ── Kinetic: Comptroller Config Refresh ──────────────────────────────────────

async fn fetch_comptroller_config(
    rotator: &mut RpcRotator,
) -> Option<ComptrollerConfig> {
    let addr: Address = KINETIC_UNITROLLER.parse().ok()?;

    for attempt in 0..=MAX_RPC_RETRIES {
        rotator.pace().await;
        let comp = IKineticComptroller::new(addr, rotator.get());

        let cf = match timeout(Duration::from_secs(3), comp.close_factor_mantissa().call()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                } else {
                    warn!("closeFactorMantissa failed: {}", e);
                }
                continue;
            }
            Err(_) => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                }
                continue;
            }
        };

        rotator.pace().await;
        let comp2 = IKineticComptroller::new(addr, rotator.get());
        let li = match timeout(Duration::from_secs(3), comp2.liquidation_incentive_mantissa().call()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                } else {
                    warn!("liquidationIncentiveMantissa failed: {}", e);
                }
                continue;
            }
            Err(_) => {
                if attempt < MAX_RPC_RETRIES {
                    rotator.rotate_on_error();
                    tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                }
                continue;
            }
        };

        rotator.mark_success();
        return Some(ComptrollerConfig {
            close_factor:          cf,
            liquidation_incentive: li,
        });
    }
    None
}

// ── Kinetic: Discover Borrowers via On-Chain Borrow Events ───────────────────
//
// Compound V2 Borrow event (no indexed fields):
//   Borrow(address borrower, uint256 borrowAmount, uint256 accountBorrows, uint256 totalBorrows)
// ABI-encoded data layout: borrower at data[12..32], rest follow in 32-byte slots.

async fn discover_borrowers(
    rotator:       &mut RpcRotator,
    k_token_addrs: &[Address],
    from_block:    u64,
    to_block:      u64,
) -> Vec<Address> {
    let topic0 = H256::from(keccak256(
        "Borrow(address,uint256,uint256,uint256)",
    ));
    let mut found = Vec::new();

    for &addr in k_token_addrs {
        let filter = Filter::new()
            .address(addr)
            .topic0(topic0)
            .from_block(from_block)
            .to_block(to_block);

        for attempt in 0..=MAX_RPC_RETRIES {
            rotator.pace().await;
            match timeout(Duration::from_secs(6), rotator.get().get_logs(&filter)).await {
                Ok(Ok(logs)) => {
                    rotator.mark_success();
                    for log in &logs {
                        if log.data.len() >= 32 {
                            let borrower = Address::from_slice(&log.data[12..32]);
                            found.push(borrower);
                        }
                    }
                    debug!("Borrow events [{:?}]: {} new", addr, logs.len());
                    break;
                }
                Ok(Err(e)) => {
                    if attempt < MAX_RPC_RETRIES && is_retryable_error(&format!("{}", e)) {
                        rotator.rotate_on_error();
                        tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                    } else {
                        warn!("get_logs {:?}: {}", addr, e);
                        break;
                    }
                }
                Err(_) => {
                    if attempt < MAX_RPC_RETRIES {
                        rotator.rotate_on_error();
                        tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                    } else {
                        warn!("get_logs timeout {:?} after {} retries", addr, MAX_RPC_RETRIES);
                        break;
                    }
                }
            }
        }
    }
    found
}

// ── Kinetic: Scan All Tracked Borrowers for Shortfall ─────────────────────────
//
// For each borrower with shortfall > 0, check their borrow balance on each
// monitored kToken market. Returns a list sorted by descending shortfall
// (largest opportunity first).
//
// Market pairs are fixed for v1 (USDC.e debt ↔ WFLR collateral, WFLR debt ↔ USDC.e collateral).
// Assumption: the primary collateral for USDC.e borrowers is WFLR and vice versa.

async fn scan_shortfalls(
    rotator:       &mut RpcRotator,
    borrowers:     &HashSet<Address>,
    k_token_pairs: &[(Address, Address, Address, Address)],
    cfg:           &ComptrollerConfig,
) -> Vec<ShortfallAccount> {
    let comp_addr: Address = match KINETIC_UNITROLLER.parse() {
        Ok(a) => a,
        Err(_) => return vec![],
    };
    let usdce_addr: Address = match USDCE.parse() {
        Ok(a) => a,
        Err(_) => return vec![],
    };
    let e18 = U256::exp10(18);
    let mut results = Vec::new();

    for &borrower in borrowers {
        // getAccountLiquidity with retry + pacing
        rotator.pace().await;
        let mut shortfall_opt = None;
        for attempt in 0..=MAX_RPC_RETRIES {
            let comp = IKineticComptroller::new(comp_addr, rotator.get());
            match timeout(Duration::from_secs(3), comp.get_account_liquidity(borrower).call()).await {
                Ok(Ok((err, _liquidity, shortfall))) if err.is_zero() && !shortfall.is_zero() => {
                    rotator.mark_success();
                    shortfall_opt = Some(shortfall);
                    break;
                }
                Ok(Ok(_)) => {
                    rotator.mark_success();
                    break;
                }
                Ok(Err(e)) => {
                    if attempt < MAX_RPC_RETRIES && is_retryable_error(&format!("{}", e)) {
                        rotator.rotate_on_error();
                        tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                    } else {
                        if attempt == MAX_RPC_RETRIES {
                            warn!("getAccountLiquidity {:?}: {} after retries", borrower, e);
                        }
                        break;
                    }
                }
                Err(_) => {
                    if attempt < MAX_RPC_RETRIES {
                        rotator.rotate_on_error();
                        tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                    } else {
                        warn!("getAccountLiquidity {:?}: timeout after retries", borrower);
                        break;
                    }
                }
            }
        }

        let shortfall = match shortfall_opt {
            Some(s) => s,
            None => continue,
        };

        for &(k_borrow, token_borrow, k_collateral, token_collateral) in k_token_pairs {
            if k_collateral == Address::zero() {
                continue;
            }

            rotator.pace().await;
            let mut borrow_balance_opt = None;
            for attempt in 0..=MAX_RPC_RETRIES {
                let kt = IKToken::new(k_borrow, rotator.get());
                match timeout(Duration::from_secs(3), kt.get_account_snapshot(borrower).call()).await {
                    Ok(Ok((err, _cbal, borrow_bal, _exrate)))
                        if err.is_zero() && !borrow_bal.is_zero() =>
                    {
                        rotator.mark_success();
                        borrow_balance_opt = Some(borrow_bal);
                        break;
                    }
                    Ok(Ok(_)) => {
                        rotator.mark_success();
                        break;
                    }
                    Ok(Err(e)) => {
                        if attempt < MAX_RPC_RETRIES && is_retryable_error(&format!("{}", e)) {
                            rotator.rotate_on_error();
                            tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                        } else {
                            break;
                        }
                    }
                    Err(_) => {
                        if attempt < MAX_RPC_RETRIES {
                            rotator.rotate_on_error();
                            tokio::time::sleep(Duration::from_millis(rotator.backoff_ms())).await;
                        } else {
                            break;
                        }
                    }
                }
            }

            let borrow_balance = match borrow_balance_opt {
                Some(b) => b,
                None => continue,
            };

            // Convert snapshot borrow balance to underlying units.
            // Kinetic kUSDC.e may return 8 decimals (cToken); underlying USDC.e is 6.
            // Set env KINETIC_KUSDCE_SNAPSHOT_DECIMALS=6 if chain already returns 6 (no conversion).
            let snapshot_dec = env::var("KINETIC_KUSDCE_SNAPSHOT_DECIMALS")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(8);
            let borrow_underlying = if token_borrow == usdce_addr && snapshot_dec == 8 {
                borrow_balance / U256::from(100u32) // 8 dec → 6 dec
            } else {
                borrow_balance
            };

            let repay_max = borrow_underlying * cfg.close_factor / e18;
            if repay_max.is_zero() {
                continue;
            }

            debug!(
                "REPAY_RAW borrower={:?} token_borrow={:?} borrow_chain={} borrow_underlying={} repay_max={}",
                borrower, token_borrow, borrow_balance, borrow_underlying, repay_max
            );

            results.push(ShortfallAccount {
                borrower,
                shortfall_usd_e18: shortfall,
                k_token_borrow: k_borrow,
                borrow_balance:  repay_max,
                token_borrow,
                k_token_collateral: k_collateral,
                token_collateral,
            });
        }
    }

    results.sort_by(|a, b| b.shortfall_usd_e18.cmp(&a.shortfall_usd_e18));
    results
}

// ── Fee-Aware Off-Chain Profitability Evaluation ──────────────────────────────
//
// Accept a shortfall account only if:
//   net_profit_USD = liquidation_premium_USD - flash_fee_USD - gas_cost_USD
//                  >= min_profit_usd
//
// liquidation_premium = repayAmount × (liquidationIncentive − 1)  (in underlying)
// flash_fee           = repayAmount × FLASH_FEE_PPM / 1_000_000   (conservative)
// gas_cost            = GAS_ESTIMATE × (baseFee + priorityFee) × flrPrice

fn evaluate_opportunity(
    account:        &ShortfallAccount,
    cfg:            &ComptrollerConfig,
    gas:            &GasParams,
    flr_price_usd:  f64,
    flash_pool:     Address,
    min_profit_usd: f64,
    max_repay_usd:  f64,
) -> Option<LiqOpportunity> {
    let e18 = U256::exp10(18);

    // Determine decimal scale and USD price for tokenBorrow.
    let is_usdce = account.token_borrow
        == USDCE.parse::<Address>().unwrap_or_default();
    let (token_dec, token_price_usd): (u32, f64) = if is_usdce {
        (6, 1.0)
    } else {
        // Assume WFLR (18 dec); price from FTSO
        if flr_price_usd < 0.0001 {
            return None;
        }
        (18, flr_price_usd)
    };

    // Hard cap: never repay more than max_repay_usd in a single call.
    let max_repay_raw: U256 = if token_dec == 6 {
        U256::from((max_repay_usd * 1e6) as u64)
    } else {
        let max_tok = max_repay_usd / token_price_usd;
        // Split 1e18 as 1e9 × 1e9 to avoid u64 overflow
        U256::from((max_tok * 1e9) as u64) * U256::from(1_000_000_000_u64)
    };

    let repay_amount = account.borrow_balance.min(max_repay_raw);
    if repay_amount.is_zero() {
        return None;
    }

    // Liquidation premium in underlying token units.
    // premium = repayAmount × (incentive − 1e18) / 1e18
    if cfg.liquidation_incentive <= e18 {
        return None;
    }
    let incentive_excess = cfg.liquidation_incentive - e18;
    let gross_premium = repay_amount * incentive_excess / e18;

    // Flash fee (0.05% conservative upper bound).
    let flash_fee = repay_amount * U256::from(FLASH_FEE_PPM) / U256::from(1_000_000_u64);

    if flash_fee >= gross_premium {
        return None;
    }
    let net_premium_raw = gross_premium - flash_fee;

    // Convert net premium → USD.
    let net_premium_usd = if token_dec == 6 {
        net_premium_raw.as_u128() as f64 / 1e6
    } else {
        net_premium_raw.as_u128() as f64 / 1e18 * token_price_usd
    };

    // Gas cost in USD: GAS_ESTIMATE × (baseFee + priorityFee) in wei × FLR/USD.
    let gas_per_unit   = gas.base_fee_wei + gas.priority_fee;
    let gas_cost_wei   = U256::from(GAS_ESTIMATE) * gas_per_unit;
    let gas_cost_flr   = gas_cost_wei.as_u128() as f64 / 1e18;
    let gas_cost_usd   = gas_cost_flr * flr_price_usd;

    let est_profit_usd = net_premium_usd - gas_cost_usd;
    
    // Repay amount in USD for spread calculation
    let repay_usd = if token_dec == 6 {
        repay_amount.as_u128() as f64 / 1e6
    } else {
        repay_amount.as_u128() as f64 / 1e18 * token_price_usd
    };
    
    let spread_pct = if repay_usd > 0.0 { (est_profit_usd / repay_usd) * 100.0 } else { 0.0 };

    // --- SIGNAL LOGGING ---
    // Log near-misses that have a reasonable profit or spread, even if below MIN_PROFIT_USD.
    let signal_threshold_usd = std::env::var("DEBUG_LOG_SENSITIVITY")
        .or_else(|_| std::env::var("SIGNAL_THRESHOLD_USD"))
        .ok().and_then(|s| s.parse().ok()).unwrap_or(0.10);
        
    let signal_threshold_pct = 0.1; // 0.1%

    if est_profit_usd >= signal_threshold_usd || spread_pct >= signal_threshold_pct {
        let borrow_sym = if is_usdce { "USDC.e" } else { "WFLR" };
        let col_sym = if account.token_collateral == USDCE.parse::<Address>().unwrap_or_default() { "USDC.e" } else { "WFLR" };
        
        info!(
            "[SIGNAL] Pair: {}/{} | Spread: {:.2}% | Est. Profit: ${:.4} (Repay: ${:.4} raw={} | Min Req: ${:.2})",
            borrow_sym, col_sym, spread_pct, est_profit_usd, repay_usd, repay_amount, min_profit_usd
        );
        let _ = std::io::stdout().flush();
    }

    if est_profit_usd < min_profit_usd {
        debug!(
            "Unprofitable: {:?} est=${:.4} < min=${:.2} (premium={:.4} flash_fee={:.4} gas={:.4})",
            account.borrower, est_profit_usd, min_profit_usd,
            net_premium_usd + gas_cost_usd, // restore gross before gas
            (flash_fee.as_u128() as f64 / if token_dec == 6 { 1e6 } else { 1e18 }) * token_price_usd,
            gas_cost_usd,
        );
        return None;
    }

    // min_profit passed to the contract (in underlying units) = gas cost equivalent.
    // This ensures the on-chain check also rejects if the swap gives less than expected.
    let min_profit_contract: U256 = if token_dec == 6 {
        U256::from((gas_cost_usd * 1e6) as u64 + 1)
    } else {
        let min_tok = gas_cost_usd / token_price_usd;
        U256::from((min_tok * 1e9) as u64 + 1) * U256::from(1_000_000_000_u64)
    };

    // Deadline: current UNIX time + 60 seconds.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Some(LiqOpportunity {
        account:        account.clone(),
        repay_amount,
        min_profit:     min_profit_contract,
        est_profit_usd,
        flash_pool,
        swap_deadline:  U256::from(now_secs + 60),
    })
}

// ── Execute: Broadcast KineticLiquidator.liquidate() ─────────────────────────

async fn execute_liquidation<M: Middleware + 'static>(
    contract: &IKineticLiquidator<M>,
    opp:      &LiqOpportunity,
    gas:      &GasParams,
) -> bool {
    // Build the LiqParams tuple matching the Solidity struct field order.
    let params = (
        opp.flash_pool,
        opp.account.borrower,
        opp.repay_amount,
        opp.account.k_token_borrow,
        opp.account.k_token_collateral,
        opp.account.token_borrow,
        opp.account.token_collateral,
        opp.min_profit,
        opp.swap_deadline,
    );

    // Gas price: 2 × baseFee + priorityFee — covers one full base-fee doubling.
    // Using legacy gas_price() since ethers FunctionCall does not expose EIP-1559
    // setters directly; Flare accepts legacy transactions alongside EIP-1559.
    let gas_price = gas.base_fee_wei * 2 + gas.priority_fee;

    info!(
        "LIQUIDATE: borrower={:?} repay={} est_profit=${:.4} pool={:?}",
        opp.account.borrower, opp.repay_amount, opp.est_profit_usd, opp.flash_pool
    );

    // Chain directly — avoids borrow-checker E0597 across the .await yield point.
    match contract
        .liquidate(params)
        .gas(GAS_ESTIMATE * 2)
        .gas_price(gas_price)
        .send()
        .await {
        Ok(pending_tx) => {
            let tx_hash = pending_tx.tx_hash();
            info!("TX broadcast: {:?}", tx_hash);
            match pending_tx.await {
                Ok(Some(receipt)) => {
                    let block  = receipt.block_number.unwrap_or_default();
                    let g_used = receipt.gas_used.unwrap_or_default();
                    let status: u64 = receipt.status
                        .map(|s: U64| s.as_u64())
                        .unwrap_or(0);
                    if status == 1 {
                        info!("CONFIRMED block={} gas_used={}", block, g_used);
                        tokio::spawn(notify_discord_kinetic_trade(
                            opp.est_profit_usd,
                            g_used,
                            tx_hash,
                        ));
                        true
                    } else {
                        error!("REVERTED block={} gas_used={}", block, g_used);
                        false
                    }
                }
                Ok(None)  => { warn!("No receipt (dropped?)"); false }
                Err(e)    => { error!("Receipt error: {}", e); false }
            }
        }
        Err(e) => { error!("TX send failed: {}", e); false }
    }
}

// ── Per-Block Processing Logic ────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn process_block(
    block_num:      u64,
    state:          &mut EngineState,
    rotator:        &mut RpcRotator,
    contract:       &IKineticLiquidator<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>,
    k_token_addrs:  &[Address],  // all kTokens to watch for Borrow events + rate checks
    k_token_pairs:  &[(Address, Address, Address, Address)], // execution market pairs
    flash_pool:     Address,
    live_trading:   bool,
    min_profit_usd: f64,
    max_repay_usd:  f64,
) {
    state.blocks_seen += 1;

    // ── STEP 1: Borrow-Rate Safeguard ─────────────────────────────────────────
    if !check_borrow_rate_safeguard(rotator, k_token_addrs).await {
        if state.blocks_seen % 10 == 0 {
            warn!("Safeguard active — interest rate critical, liquidations suspended");
        }
        // Still discover new borrowers so we're ready when rates normalize.
        let new = discover_borrowers(rotator, k_token_addrs, block_num, block_num).await;
        let mut new_found = false;
        for b in new {
            if state.known_borrowers.insert(b) {
                new_found = true;
            }
        }
        if new_found {
            save_borrower_cache(&state.known_borrowers);
        }
        return;
    }

    // ── STEP 2: Discover Borrowers (incremental — current block only) ─────────
    let new_borrowers =
        discover_borrowers(rotator, k_token_addrs, block_num, block_num).await;
    let mut new_found = false;
    for b in new_borrowers {
        if state.known_borrowers.insert(b) {
            debug!("New borrower tracked: {:?}", b);
            new_found = true;
        }
    }
    if new_found {
        save_borrower_cache(&state.known_borrowers);
    }

    // ── STEP 3: Periodic Comptroller Config Refresh (every 2 minutes) ─────────
    if state.last_cfg_refresh.elapsed() > Duration::from_secs(120) {
        if let Some(fresh) = fetch_comptroller_config(rotator).await {
            info!(
                "Comptroller refresh: closeFactor={} liquidationIncentive={}",
                fresh.close_factor, fresh.liquidation_incentive
            );
            state.cfg = fresh;
        }
        state.last_cfg_refresh = Instant::now();
    }

    // ── STEP 4: Shortfall Scan ────────────────────────────────────────────────
    if state.known_borrowers.is_empty() {
        debug!("block={} — no tracked borrowers yet", block_num);
        return;
    }

    let shortfall_accounts =
        scan_shortfalls(rotator, &state.known_borrowers, k_token_pairs, &state.cfg).await;

    if shortfall_accounts.is_empty() {
        debug!("block={} — no shortfalls among {} borrowers", block_num, state.known_borrowers.len());
        return;
    }

    // ── STEP 5: FLR Price + Gas Params ───────────────────────────────────────
    let flr_price = fetch_flr_price_usd(rotator).await;
    let gas       = fetch_gas_params(rotator).await;

    debug!(
        "block={} FLR=${:.5} baseFee={} Gwei — {} shortfalls",
        block_num,
        flr_price,
        gas.base_fee_wei.as_u64() / 1_000_000_000,
        shortfall_accounts.len()
    );

    // ── STEP 6: Evaluate & Execute ────────────────────────────────────────────
    for account in &shortfall_accounts {
        state.shortfalls_seen += 1;
        info!(
            "SHORTFALL #{}: borrower={:?} shortfall_usd~={:.2} borrow_balance={}",
            state.shortfalls_seen,
            account.borrower,
            account.shortfall_usd_e18.as_u128() as f64 / 1e18,
            account.borrow_balance,
        );

        match evaluate_opportunity(
            account, &state.cfg, &gas, flr_price, flash_pool, min_profit_usd, max_repay_usd,
        ) {
            None => {
                info!("  → rejected by profitability check");
            }
            Some(ref opp) => {
                info!(
                    "  → PROFITABLE: est_profit=${:.4} repay={} min_profit={}",
                    opp.est_profit_usd, opp.repay_amount, opp.min_profit
                );

                if !live_trading {
                    info!("  → DRY-RUN: would call KineticLiquidator.liquidate()");
                    continue;
                }

                // Cooldown: max one tx per 3 seconds to avoid overlapping nonces.
                if state.last_trade.elapsed() < Duration::from_secs(3) {
                    debug!("  → cooldown active — skipping");
                    continue;
                }

                if execute_liquidation(contract, opp, &gas).await {
                    state.trades_ok += 1;
                    state.last_trade = Instant::now();
                }
            }
        }
    }

    // Rotate RPC endpoint every 100 blocks to balance load.
    if state.blocks_seen % 100 == 0 {
        rotator.rotate();
        info!(
            "STATS: blocks={} shortfalls={} liquidations_ok={} tracked_borrowers={}",
            state.blocks_seen,
            state.shortfalls_seen,
            state.trades_ok,
            state.known_borrowers.len(),
        );
        let _ = std::io::stdout().flush();
    }

    // Heartbeat every 300 blocks (~5 mins)
    if state.blocks_seen % 300 == 0 {
        info!("[HEARTBEAT] Engine Active | Scanning {} borrowers | Latency focus: NY4", state.known_borrowers.len());
        let _ = std::io::stdout().flush();
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()))
        .init();

    // ── Environment ───────────────────────────────────────────────────────────
    let pk = env::var("PRIVATE_KEY").expect("PRIVATE_KEY required");
    let contract_s = env::var("KINETIC_LIQUIDATOR_ADDR")
        .expect("KINETIC_LIQUIDATOR_ADDR required (run DeployKineticLiquidator.s.sol first)");
    let pool_s = env::var("V4_WFLR_USDCE_POOL")
        .unwrap_or_else(|_| V4_WFLR_USDCE_POOL.to_string());
    // Accept KINETIC_KFLR (preferred) or legacy KINETIC_KWFLR; default to hardcoded mainnet address.
    let kflr_s = env::var("KINETIC_KFLR")
        .or_else(|_| env::var("KINETIC_KWFLR"))
        .unwrap_or_else(|_| KINETIC_KFLR.to_string());
    let live_trading   = env::var("ENABLE_LIVE_TRADING").map(|s| s == "true").unwrap_or(false);
    let min_profit_usd: f64 = env::var("MIN_PROFIT_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(0.0001);
    let max_repay_usd: f64  = env::var("MAX_REPAY_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(100.0);

    // ── Wallet & Client ───────────────────────────────────────────────────────
    let pks = env::var("PRIVATE_KEYS").unwrap_or_else(|_| env::var("PRIVATE_KEY").expect("PRIVATE_KEYS or PRIVATE_KEY required"));
    let mut rotator = RpcRotator::new().await?;
    let mut wallet_rotator = WalletRotator::new(&pks, rotator.get(), 14u64)?;
    
    // Default owner logging (first wallet)
    let first_wallet = pks.split(',').next().unwrap().parse::<LocalWallet>()?.with_chain_id(14u64);
    let owner = first_wallet.address();

    let contract_addr: Address = contract_s.parse()?;
    let flash_pool: Address = pool_s.parse()?;

    // ── Market Pairs ──────────────────────────────────────────────────────────
    // k_token_pairs: (kBorrow, tokenBorrow, kCollateral, tokenCollateral)
    // v1 assumption: USDC.e borrowers have WFLR as primary collateral, and vice versa.
    let kusdce_addr: Address = KINETIC_KUSDCE.parse()?;
    let wflr_addr:   Address = WFLR.parse()?;
    let usdce_addr:  Address = USDCE.parse()?;

    let mut k_token_pairs: Vec<(Address, Address, Address, Address)> = Vec::new();
    let mut k_token_addrs: Vec<Address> = vec![kusdce_addr];

    // kFLR market: underlying is native FLR; WFLR address used for swap routing
    // (KineticLiquidator.sol handles FLR→WFLR wrapping before the SparkDEX swap).
    if let Ok(kflr_addr) = kflr_s.parse::<Address>() {
        if kflr_addr != Address::zero() {
            k_token_addrs.push(kflr_addr);
            // kUSDC.E debt → seize kFLR collateral (receive FLR, wrap to WFLR for swap)
            k_token_pairs.push((kusdce_addr, usdce_addr, kflr_addr, wflr_addr));
            // kFLR debt → seize kUSDC.E collateral
            k_token_pairs.push((kflr_addr, wflr_addr, kusdce_addr, usdce_addr));
            info!("kFLR market enabled: {:?}", kflr_addr);
        }
    }

    // TICKET-018: alle 7 kToken-Märkte (on-chain verifiziert 2026-03-01)
    // Neue Märkte nur für Borrower-Discovery (k_token_addrs) +
    // Liquidations-Pairs gegen kUSDC.E und kFLR als Collateral (größte Liquidität).
    let usdt0_addr:  Address = USDT0.parse().expect("USDT0 parse");
    let usdt_addr:   Address = USDT.parse().expect("USDT parse");
    let sflr_addr:   Address = SFLR.parse().expect("SFLR parse");
    let weth_addr:   Address = WETH.parse().expect("WETH parse");
    let flreth_addr: Address = FLRETH.parse().expect("FLRETH parse");

    let kusdt0_addr:  Address = KINETIC_KUSDT0.parse().expect("kUSDT0 parse");
    let kusdt_addr:   Address = KINETIC_KUSDT.parse().expect("kUSDT parse");
    let ksflr_addr:   Address = KINETIC_KSFLR.parse().expect("kSFLR parse");
    let kweth_addr:   Address = KINETIC_KWETH.parse().expect("kWETH parse");
    let kflreth_addr: Address = KINETIC_KFLRETH.parse().expect("kFLRETH parse");

    // Alle 5 neuen Märkte für Borrower-Event-Scanning registrieren
    k_token_addrs.push(kusdt0_addr);
    k_token_addrs.push(kusdt_addr);
    k_token_addrs.push(ksflr_addr);
    k_token_addrs.push(kweth_addr);
    k_token_addrs.push(kflreth_addr);
    info!("TICKET-018: {} kToken-Märkte registriert für Borrower-Scan", k_token_addrs.len());

    // Execution-Pairs für neue Märkte:
    // Strategie: Debt in neuem Token → Collateral in kFLR oder kUSDC.E (höchste Liquidität)
    // Nur Pairs mit echten Borrows:
    //   kUSDT0  (4M USDT0 borrowed)  — HOCH PRIO
    //   kUSDT   (6.4k USDT)          — mittel
    //   kSFLR   (—)                  — tracking only erstmal
    //   kWETH   (0.58 WETH)          — mittel
    //   kFLRETH (3.16 tokens)        — mittel

    if let Ok(kflr_addr) = kflr_s.parse::<Address>() {
        if kflr_addr != Address::zero() {
            // kUSDT0 debt → seize kFLR collateral (HOCH PRIO: 4M USDT0 ausgeliehen!)
            k_token_pairs.push((kusdt0_addr, usdt0_addr, kflr_addr, wflr_addr));
            // kUSDT debt → seize kFLR collateral
            k_token_pairs.push((kusdt_addr, usdt_addr, kflr_addr, wflr_addr));
            // kWETH debt → seize kFLR collateral
            k_token_pairs.push((kweth_addr, weth_addr, kflr_addr, wflr_addr));
            // kFLRETH debt → seize kFLR collateral
            k_token_pairs.push((kflreth_addr, flreth_addr, kflr_addr, wflr_addr));
            // kSFLR debt → seize kFLR collateral (sFLR/WFLR ratio ~0.56)
            k_token_pairs.push((ksflr_addr, sflr_addr, kflr_addr, wflr_addr));
        }
    }
    // kUSDT0/kUSDT/kWETH debt → seize kUSDC.E collateral (fallback)
    k_token_pairs.push((kusdt0_addr, usdt0_addr, kusdce_addr, usdce_addr));
    k_token_pairs.push((kusdt_addr,  usdt_addr,  kusdce_addr, usdce_addr));
    k_token_pairs.push((kweth_addr,  weth_addr,  kusdce_addr, usdce_addr));

    if k_token_pairs.is_empty() {
        warn!("kFLR market address parse failed — tracking kUSDC.E borrowers only (execution needs kFLR configured)");
        k_token_pairs.push((kusdce_addr, usdce_addr, Address::zero(), Address::zero()));
    }

    // ── Banner ────────────────────────────────────────────────────────────────
    info!("╔══════════════════════════════════════════════════════════════════════╗");
    info!("║  KINETIC ENGINE — Bot 2: Compound V2 Liquidation                  ║");
    info!("║  Version: 2026-03-01.1 | Chain: Flare Mainnet (14) — 7 Märkte     ║");
    info!("╠══════════════════════════════════════════════════════════════════════╣");
    info!("║  Owner:       {:?}  ║", owner);
    info!("║  Contract:    {}  ║", contract_s);
    info!("║  Flash Pool:  {}  ║", pool_s);
    info!("║  kUSDC.E:     {}  ║", KINETIC_KUSDCE);
    info!("║  kFLR:        {}  ║", kflr_s);
    info!("║  Min Profit:  ${:.2} | Max Repay: ${:.0}  ║", min_profit_usd, max_repay_usd);
    info!("║  Live:        {}  ║",
        if live_trading { "YES ← REAL MONEY" } else { "NO  (dry-run — set ENABLE_LIVE_TRADING=true)" });
    info!("╚══════════════════════════════════════════════════════════════════════╝");

    // ── Bootstrap Comptroller Config ──────────────────────────────────────────
    let init_cfg = match fetch_comptroller_config(&mut rotator).await {
        Some(c) => {
            info!("Comptroller: closeFactor={} liquidationIncentive={}", c.close_factor, c.liquidation_incentive);
            c
        }
        None => {
            warn!("Could not fetch comptroller config — using hardcoded defaults");
            ComptrollerConfig::default()
        }
    };

    let mut state = EngineState::new(init_cfg);

    // ── Seed Borrower Set from Historical Borrow Events ───────────────────────
    let cached = load_borrower_cache();
    for b in cached {
        state.known_borrowers.insert(b);
    }
    info!("Loaded {} borrowers from local cache", state.known_borrowers.len());

    let tip = rotator.get().get_block_number().await?.as_u64();
    let from = if state.known_borrowers.is_empty() {
        tip.saturating_sub(1_500_000) // ~17 days at 1-second blocks (enough to cover recent Kinetic activity)
    } else {
        tip.saturating_sub(50_000) // just catch up from last run
    };

    info!("Seeding borrowers from block {} to {} ...", from, tip);
    
    // Chunked discovery to avoid RPC timeouts
    let mut current_from = from;
    let chunk_size = 50_000;
    while current_from < tip {
        let current_to = (current_from + chunk_size).min(tip);
        info!("Scanning chunk {} to {} ...", current_from, current_to);
        let seed = discover_borrowers(&mut rotator, &k_token_addrs, current_from, current_to).await;
        for b in seed {
            state.known_borrowers.insert(b);
        }
        current_from = current_to + 1;
    }
    
    info!("Seeded {} unique borrowers", state.known_borrowers.len());
    save_borrower_cache(&state.known_borrowers);

    // ── WS Connection ─────────────────────────────────────────────────────────
    let mut ws_provider: Option<Provider<Ws>> = None;
    
    let mut ws_urls: Vec<String> = Vec::new();
    if let Ok(custom) = env::var("CUSTOM_WS_URLS") {
        ws_urls = custom.split(',').map(|s| s.trim().to_string()).collect();
    } else {
        ws_urls = WS_URLS.iter().map(|&s| s.to_string()).collect();
    }

    for url in &ws_urls {
        match Provider::<Ws>::connect(url.as_str()).await {
            Ok(p)  => { info!("WS connected: {}", url); ws_provider = Some(p); break; }
            Err(e) => warn!("WS {} failed: {}", url, e),
        }
    }

    match ws_provider {
        // ── EVENT-DRIVEN via WebSocket (newHeads) ─────────────────────────────
        Some(ws) => {
            let mut stream = ws.subscribe_blocks().await?;
            info!("Subscribed to newHeads — Flare ~1s blocks");

            while let Some(block) = stream.next().await {
                let block_num = block.number.unwrap_or_default().as_u64();
                
                // Get next wallet client for this block's potential executions
                let client = wallet_rotator.get_next();
                let contract = IKineticLiquidator::new(contract_addr, client);
                
                process_block(
                    block_num, &mut state, &mut rotator, &contract,
                    &k_token_addrs, &k_token_pairs,
                    flash_pool, live_trading, min_profit_usd, max_repay_usd,
                )
                .await;
            }

            warn!("WS stream ended — exiting");
        }

        // ── POLLING FALLBACK (1s interval) ────────────────────────────────────
        None => {
            warn!("No WS available — polling every 1s (Flare ~1s blocks)");
            let mut last_block = 0u64;
            loop {
                let block_num = rotator
                    .get()
                    .get_block_number()
                    .await
                    .unwrap_or_default()
                    .as_u64();

                if block_num > last_block {
                    last_block = block_num;
                    
                    // Get next wallet client for this block's potential executions
                    let client = wallet_rotator.get_next();
                    let contract = IKineticLiquidator::new(contract_addr, client);
                    
                    process_block(
                        block_num, &mut state, &mut rotator, &contract,
                        &k_token_addrs, &k_token_pairs,
                        flash_pool, live_trading, min_profit_usd, max_repay_usd,
                    )
                    .await;
                }
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
        }
    }

    Ok(())
}
