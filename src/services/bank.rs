use std::time::Duration;

use serde_json::json;

use crate::{config::Config, state::AppState};

/// Charge the bank once. Network/5xx failures are retried, but every attempt carries the same
/// `Idempotency-Key`, so the bank records a single charge however many attempts were needed.
pub async fn charge(state: &AppState, cfg: &Config, order_id: i32, amount: f64, key: &str) -> Option<String> {
    for attempt in 0..3u64 {
        let res = state
            .http
            .post(format!("{}/charge", cfg.bank_url))
            .header("Idempotency-Key", key)
            .timeout(Duration::from_secs(8))
            .json(&json!({ "order_id": order_id, "amount": amount }))
            .send()
            .await;
        if let Ok(r) = res {
            if r.status().as_u16() == 200 {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    if let Some(s) = v.get("bank_ref").and_then(|x| x.as_str()) {
                        return Some(s.to_string());
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
    }
    None
}
