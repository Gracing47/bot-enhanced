use std::env;
use std::sync::Arc;
use std::time::Duration;

use dotenvy::dotenv;
use ethers::prelude::*;
use eyre::Result;
use tracing::{error, info, warn};

// ── ABI Definitionen -- Liquity V2 (Enosys Loans on Flare) -------------------
// Verifiziert on-chain 2026-03-01. Troves haben uint256 IDs, kein address-basiertes V1 Interface.

abigen!(
    ITroveManagerV2,
    r#"[
        function priceFeed() external view returns (address)
        function MCR() external view returns (uint256)
        function getCurrentICR(uint256 _troveId, uint256 _price) external view returns (uint256)
        function getTroveStatus(uint256 _troveId) external view returns (uint8)
        function liquidate(uint256 _troveId) external
    ]"#
);

abigen!(
    IMultiTroveGetterV2,
    r#"[{"inputs":[{"internalType":"uint256","name":"_collIndex","type":"uint256"},{"internalType":"int256","name":"_startIdx","type":"int256"},{"internalType":"uint256","name":"_count","type":"uint256"}],"name":"getMultipleSortedTroves","outputs":[{"internalType":"uint256[11][]","name":"","type":"uint256[11][]"}],"stateMutability":"view","type":"function"}]"#
);

abigen!(
    IPriceFeed,
    r#"[
        function lastGoodPrice() external view returns (uint256)
    ]"#
);

// ── Signer / Rotation --------------------------------------------------------

pub struct WalletRotator {
    pub clients: Vec<Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>>>,
    pub index:   usize,
}

impl WalletRotator {
    pub fn new(pks_str: &str, provider: Arc<Provider<Http>>, chain_id: u64) -> Result<Self> {
        let mut clients = Vec::new();
        let keys: Vec<&str> = pks_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        for key in keys {
            let wallet = key.parse::<LocalWallet>()?.with_chain_id(chain_id);
            let client = Arc::new(SignerMiddleware::new(provider.clone(), wallet));
            clients.push(client);
        }
        if clients.is_empty() { return Err(eyre::eyre!("No valid keys")); }
        Ok(Self { clients, index: 0 })
    }

    pub fn get_next(&mut self) -> Arc<SignerMiddleware<Arc<Provider<Http>>, LocalWallet>> {
        let client = self.clients[self.index].clone();
        self.index = (self.index + 1) % self.clients.len();
        client
    }
}

// ── ABI: FlashBotFlare -------------------------------------------------------

abigen!(
    IFlashBotFlare,
    r#"[
        function callContract(address target, bytes payload, address tokenAddress) external payable
    ]"#
);

// ── Config -------------------------------------------------------------------
// Version: 2026-03-01.1 (TICKET-017: Liquity V2 uint256 troveId interface)

const FXRP_TROVE_MANAGER: &str = "0xc46e7d0538494FEb82b460b9723dAba0508C8Fb1";
const WFLR_TROVE_MANAGER: &str = "0xB6cB0c5301D4E6e227Ba490cee7b92EB954ac06D";
const MULTI_TROVE_GETTER:  &str = "0xb80C59BBCEB205bCA25Eea2e4221717f23b339d3";
const FLASHBOT_FLARE:      &str = "0xF61CaBDD39FD8EDBca98C4bb705b6B3678874a02";
const WFLR_TOKEN:          &str = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d";

// MCR = 110% in 1e18
const MCR: u128 = 1_100_000_000_000_000_000u128;

#[derive(Debug, Clone)]
struct BranchConfig {
    name:          &'static str,
    trove_manager: Address,
    coll_index:    U256,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt::init();

    info!("Enosys Multi-Branch Liquidator v2026-03-01.1 (Liquity V2 interface)");

    let rpc_url = env::var("RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:9650/ext/bc/C/rpc".to_string());
    info!("Connecting to RPC: {}", rpc_url);
    let provider = Provider::<Http>::try_from(rpc_url)?;
    let client = Arc::new(provider);

    let pks = env::var("PRIVATE_KEYS").expect("PRIVATE_KEYS required");
    let mut wallet_rotator = WalletRotator::new(&pks, client.clone(), 14u64)?;

    let fb_addr: Address = FLASHBOT_FLARE.parse()?;
    let getter_addr: Address = MULTI_TROVE_GETTER.parse()?;
    let getter = IMultiTroveGetterV2::new(getter_addr, Arc::clone(&client));

    let branches = vec![
        BranchConfig {
            name:          "FXRP",
            trove_manager: FXRP_TROVE_MANAGER.parse()?,
            coll_index:    U256::zero(),
        },
        BranchConfig {
            name:          "WFLR",
            trove_manager: WFLR_TROVE_MANAGER.parse()?,
            coll_index:    U256::one(),
        },
    ];

    // Startup: priceFeed + current price pro Branch loggen
    for branch in &branches {
        let itm = ITroveManagerV2::new(branch.trove_manager, Arc::clone(&client));
        match itm.price_feed().call().await {
            Ok(feed_addr) => {
                let feed = IPriceFeed::new(feed_addr, Arc::clone(&client));
                let price = feed.last_good_price().call().await.unwrap_or_default();
                info!("Branch {}: troveManager={:?} priceFeed={:?} price={}",
                    branch.name, branch.trove_manager, feed_addr, price);
            }
            Err(e) => error!("Branch {}: priceFeed() failed: {}", branch.name, e),
        }
    }

    loop {
        for branch in &branches {
            match scan_branch(&getter, branch, &mut wallet_rotator, fb_addr, &client).await {
                Ok(_) => {}
                Err(e) => error!("Error scanning {} Branch: {}", branch.name, e),
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

async fn scan_branch(
    getter:  &IMultiTroveGetterV2<Provider<Http>>,
    branch:  &BranchConfig,
    wallets: &mut WalletRotator,
    fb_addr: Address,
    client:  &Arc<Provider<Http>>,
) -> Result<()> {
    // startIdx=-1 = schlechteste Troves zuerst (lowest ICR)
    let troves = getter
        .get_multiple_sorted_troves(branch.coll_index, I256::from(-1i64), U256::from(60))
        .call()
        .await?;

    if troves.is_empty() {
        return Ok(());
    }

    // Preis einmal holen
    let itm = ITroveManagerV2::new(branch.trove_manager, Arc::clone(client));
    let feed_addr = itm.price_feed().call().await?;
    let feed = IPriceFeed::new(feed_addr, Arc::clone(client));
    let price = feed.last_good_price().call().await?;
    let mcr = U256::from(MCR);

    let mut liquidatable = 0u32;
    for trove_data in &troves {
        // Liquity V2 CombinedTroveData: [0]=id, [1]=entireDebt, [2]=entireColl
        let trove_id    = trove_data[0];
        let entire_debt = trove_data[1];

        if entire_debt.is_zero() { continue; }

        // getCurrentICR on-chain (akkurat, kein floating point)
        let icr = match itm.get_current_icr(trove_id, price).call().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        if icr < mcr {
            liquidatable += 1;
            let id_str = trove_id.to_string();
            let id_short = if id_str.len() > 8 {
                format!("{}...{}", &id_str[..4], &id_str[id_str.len()-4..])
            } else {
                id_str.clone()
            };
            warn!(
                "!!! LIQUIDATABLE: id={} | ICR={}% | debt={} | {} Branch",
                id_short,
                icr / U256::from(10_000_000_000_000_000u64),
                entire_debt,
                branch.name
            );

            let signer = wallets.get_next();
            let itm_signer = ITroveManagerV2::new(branch.trove_manager, Arc::clone(&signer));
            let call = itm_signer.liquidate(trove_id);
            let payload = call.calldata()
                .ok_or_else(|| eyre::eyre!("calldata fehlt"))?;

            info!("Liquidate trove {} via callContract (branch={})...", id_short, branch.name);

            let flashbot = IFlashBotFlare::new(fb_addr, Arc::clone(&signer));
            match flashbot
                .call_contract(branch.trove_manager, payload, WFLR_TOKEN.parse()?)
                .send()
                .await
            {
                Ok(pending) => {
                    info!("TX: {:?}", pending.tx_hash());
                    let _ = pending.await;
                }
                Err(e) => error!("TX failed: {}", e),
            }
        }
    }

    if liquidatable == 0 {
        info!("Branch {}: {} troves, none liquidatable (price={})",
            branch.name, troves.len(), price);
    }

    Ok(())
}
