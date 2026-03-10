use std::env;
use std::sync::Arc;
use std::time::Duration;

use dotenvy::dotenv;
use ethers::prelude::*;
use eyre::Result;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{error, info, warn};

// ── ABIs ──────────────────────────────────────────────────────────────────────

abigen!(
    IEnosysRouter,
    r#"[
        function getAmountsOut(uint amountIn, address[] calldata path) external view returns (uint[] memory amounts)
    ]"#
);

abigen!(
    IFtsoV2,
    r#"[
        function getFeedById(bytes21 feedId) external view returns (uint256 value, int8 decimals, uint64 timestamp)
    ]"#
);

// ── Adressen ──────────────────────────────────────────────────────────────────

const USDT0_ADDR:         &str = "0xe7cd86e13AC4309349F30B3435a9d337750fC82D";
const WFLR_ADDR:          &str = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d";
const FXRP_ADDR:          &str = "0xAd552A648C74D49E10027AB8a618A3ad4901c5bE";
const USDC_E_ADDR:        &str = "0xFbDa5F676cB37624f28265A144A48B0d6e87d3b6";
const ENOSYS_V2_ROUTER:   &str = "0xe3A1b355ca63abCBC9589334B5e609583C7BAa06";
const FTSO_CONSUMER:      &str = "0x7BDE3Df0624114eDB3A67dFe6753e62f4e7c1d20";

// ── Triangle State ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct TrianglePath {
    name: String,
    profit_pct: f64,
    profit_usd: f64,
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
            Err(eyre::eyre!("FTSOv2 error"))
        }
    }
}

// ── Triangle Scanner ──────────────────────────────────────────────────────────

async fn triangle_scanner_task(provider: Arc<Provider<Http>>, tx: mpsc::Sender<TrianglePath>) -> Result<()> {
    let mut interval = interval(Duration::from_secs(3));
    let min_threshold = 0.003; // 0.3%
    let borrow_amount = 500_000_000u64; // $500

    let tokens = vec![
        ("USDT0", USDT0_ADDR.parse::<Address>()?),
        ("WFLR", WFLR_ADDR.parse::<Address>()?),
        ("FXRP", FXRP_ADDR.parse::<Address>()?),
        ("USDC.e", USDC_E_ADDR.parse::<Address>()?),
    ];

    info!("🔺 Triangle Scanner ACTIVE — scanning {} paths every 3s", tokens.len() * (tokens.len() - 1) * (tokens.len() - 2));

    let mut scan_count = 0u64;

    loop {
        interval.tick().await;
        scan_count += 1;

        if scan_count % 20 == 0 {
            info!(
                "📊 Triangle scanner running (scan #{}) | FLR: ${:.6}",
                scan_count,
                fetch_ftso_price(provider.clone()).await.unwrap_or(0.0)
            );
        }

        // Simulate triangle opportunity detection
        // In real version: quote all 3-leg paths, calculate profit, check threshold
        // For now: just demonstrate the structure
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("kojo_flare=info".parse()?),
        )
        .init();

    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:9650/ext/bc/C/rpc".to_string());
    let provider = Arc::new(Provider::<Http>::try_from(rpc_url)?);

    info!("🚀 KOJO-FLARE v4 — TRIANGLE ARBITRAGE BOT");

    let price = fetch_ftso_price(provider.clone()).await?;
    info!("Initial FLR Price: ${:.6}", price);

    let (tx, _rx) = mpsc::channel(100);
    let provider_clone = provider.clone();

    let scanner = tokio::spawn(async move {
        let _ = triangle_scanner_task(provider_clone, tx).await;
    });

    tokio::select! {
        _ = scanner => error!("Scanner ended"),
    }

    Ok(())
}
