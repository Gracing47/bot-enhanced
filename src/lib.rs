/// LLM Client for Qwen 1.5 32B (via Ollama)
///
/// Provides AI-driven trading signals with automatic fallback to hard-coded logic
/// if LLM is unavailable. All LLM decisions logged for analysis.

use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{info, warn, error};

#[derive(Clone, Debug)]
pub struct QwenClient {
    api_url: String,
    client: Client,
    timeout_secs: u64,
    fallback_enabled: bool,
    model: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PoolSnapshot {
    pub token0: String,
    pub token1: String,
    pub price: f64,
    pub spread_pct: f64,
    pub volume_24h: f64,
    pub liquidity: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LLMSignal {
    pub signal: String,      // "BUY", "SELL", "HOLD"
    pub confidence: f64,     // 0.0-1.0
    pub reason: String,
    pub min_profit_pct: f64,
}

impl QwenClient {
    /// Create new QwenClient, test connection
    pub async fn new(api_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        // Test connection to Ollama API
        match client
            .get(&format!("{}api/tags", api_url))
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!("✅ Qwen LLM Available at {}", api_url);
                    Ok(Self {
                        api_url: api_url.to_string(),
                        client,
                        timeout_secs: 5,
                        fallback_enabled: true,
                        model: "qwen1.5-32b-chat-moe-equiv".to_string(),
                    })
                } else {
                    warn!("⚠️ Qwen LLM not responding (status {}), fallback mode", resp.status());
                    Ok(Self {
                        api_url: api_url.to_string(),
                        client,
                        timeout_secs: 5,
                        fallback_enabled: true,
                        model: "qwen1.5-32b-chat-moe-equiv".to_string(),
                    })
                }
            }
            Err(e) => {
                warn!("⚠️ Qwen LLM NOT Available: {}, fallback mode enabled!", e);
                Ok(Self {
                    api_url: api_url.to_string(),
                    client,
                    timeout_secs: 5,
                    fallback_enabled: true,
                    model: "qwen1.5-32b-chat-moe-equiv".to_string(),
                })
            }
        }
    }

    /// Analyze pool data and return trading signal
    pub async fn analyze_opportunity(
        &self,
        pool_data: &PoolSnapshot,
    ) -> Result<LLMSignal, Box<dyn std::error::Error>> {
        if !self.fallback_enabled {
            return Ok(self.fallback_signal(pool_data));
        }

        let prompt = format!(
            "Analyze DEX pool: {} / {}, spread {:.2}%, volume ${:.0}, liquidity ${:.0}. \
             Return ONLY JSON: {{\"signal\":\"BUY|SELL|HOLD\",\"confidence\":0.0-1.0,\"reason\":\"str\",\"min_profit_pct\":0.1-5.0}}",
            pool_data.token0, pool_data.token1,
            pool_data.spread_pct, pool_data.volume_24h, pool_data.liquidity
        );

        let req = json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "temperature": 0.5,
        });

        match self
            .client
            .post(&format!("{}api/generate", self.api_url))
            .json(&req)
            .timeout(Duration::from_secs(self.timeout_secs))
            .send()
            .await
        {
            Ok(response) => {
                if let Ok(body) = response.json::<Value>().await {
                    if let Some(response_text) = body["response"].as_str() {
                        if let Ok(signal) = serde_json::from_str::<LLMSignal>(response_text) {
                            info!("🤖 LLM: {} ({}% conf)", signal.signal, (signal.confidence * 100.0) as i32);
                            return Ok(signal);
                        }
                    }
                }
                Ok(self.fallback_signal(pool_data))
            }
            Err(_) => Ok(self.fallback_signal(pool_data)),
        }
    }

    fn fallback_signal(&self, pool_data: &PoolSnapshot) -> LLMSignal {
        let (signal, confidence) = if pool_data.spread_pct > 0.5 {
            ("BUY", 0.6)
        } else if pool_data.spread_pct > 0.3 {
            ("BUY", 0.5)
        } else if pool_data.spread_pct < -0.3 {
            ("SELL", 0.5)
        } else {
            ("HOLD", 0.4)
        };

        LLMSignal {
            signal: signal.to_string(),
            confidence,
            reason: "Fallback (LLM unavailable)".to_string(),
            min_profit_pct: 0.3,
        }
    }

    pub async fn health_check(&self) -> Result<String, Box<dyn std::error::Error>> {
        match self.client.get(&format!("{}api/tags", self.api_url)).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    Ok("✅ Qwen healthy".to_string())
                } else {
                    Ok(format!("❌ HTTP {}", resp.status()))
                }
            }
            Err(e) => Ok(format!("❌ {}", e)),
        }
    }

    pub fn set_fallback_enabled(&mut self, enabled: bool) {
        self.fallback_enabled = enabled;
    }
}
