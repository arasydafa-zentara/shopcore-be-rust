//! Background notifications. Emails were only ever appended to an in-memory list that nothing read,
//! so they are just logged. `notify_url` callbacks go through sharded workers: every event for the same
//! order is handled by the same worker, which keeps the pending -> paid order.

use std::{net::IpAddr, time::Duration};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::Config;

const SHARDS: usize = 32;

#[derive(Debug)]
pub struct NotifyJob {
    pub url: String,
    pub body: Value,
}

#[derive(Clone)]
pub struct Notifier {
    shards: Vec<mpsc::Sender<NotifyJob>>,
}

impl Notifier {
    pub fn start(cfg: &Config) -> Notifier {
        let mut shards = Vec::new();
        for _ in 0..SHARDS {
            let (tx, mut rx) = mpsc::channel::<NotifyJob>(1024);
            let block_private = cfg.notify_block_private;
            tokio::spawn(async move {
                while let Some(job) = rx.recv().await {
                    // Hard ceiling per job so one slow or hostile receiver cannot hold a worker for long.
                    let _ = tokio::time::timeout(Duration::from_secs(10), deliver(job, block_private)).await;
                }
            });
            shards.push(tx);
        }
        Notifier { shards }
    }

    pub fn order_event(&self, order_id: i32, order_no: Option<i32>, status: &str, url: Option<&str>) {
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return;
        };
        let job = NotifyJob {
            url: url.to_string(),
            body: json!({ "order_id": order_id, "order_no": order_no, "status": status }),
        };
        let shard = &self.shards[order_id.unsigned_abs() as usize % SHARDS];
        if shard.try_send(job).is_err() {
            tracing::warn!(order_id, "notify queue full, event dropped");
        }
    }
}

pub fn email(subject: &str) {
    // The legacy outbox was never delivered anywhere; keep a log line, never the body or address.
    tracing::info!(subject, "email queued");
}

/// Addresses that are refused whatever the configuration: link-local ranges and the well-known cloud metadata
/// endpoints, including when they are written as IPv4-mapped / IPv4-compatible IPv6 addresses.
fn is_always_blocked(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_link_local() || o == [100, 100, 100, 200] || o == [192, 0, 0, 192]
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return is_always_blocked(&IpAddr::V4(v4));
            }
            let s = v6.segments();
            (s[0] & 0xffc0) == 0xfe80 || (s[0] == 0xfd00 && s[1] == 0x0ec2)
        }
    }
}

fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_unspecified() || v4.is_broadcast() || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(&IpAddr::V4(v4)) || v4.is_link_local();
            }
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

async fn deliver(job: NotifyJob, block_private: bool) {
    let Ok(url) = reqwest::Url::parse(&job.url) else {
        tracing::warn!("notify: invalid url");
        return;
    };
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
        tracing::warn!("notify: url rejected (scheme or credentials)");
        return;
    }
    let Some(host) = url.host_str().map(str::to_string) else {
        return;
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let lookup = tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((host.as_str(), port)));
    let addrs: Vec<std::net::SocketAddr> = match lookup.await {
        Ok(Ok(a)) => a.collect(),
        _ => {
            tracing::warn!("notify: dns lookup failed");
            return;
        }
    };
    if addrs.is_empty() || addrs.iter().any(|a| is_always_blocked(&a.ip())) {
        tracing::warn!("notify: target refused (link-local or unresolved)");
        return;
    }
    if block_private && addrs.iter().any(|a| is_private(&a.ip())) {
        tracing::warn!("notify: target refused (private address)");
        return;
    }
    // Pin the connection to the addresses we just validated (no second DNS lookup = no rebinding).
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .resolve_to_addrs(&host, &addrs)
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    for attempt in 0..2u64 {
        match client.post(url.clone()).json(&job.body).send().await {
            Ok(r) if r.status().as_u16() < 500 => return,
            _ => tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await,
        }
    }
    tracing::warn!("notify: delivery failed after retries");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_classification() {
        assert!(is_always_blocked(&"169.254.169.254".parse().unwrap()));
        assert!(is_always_blocked(&"::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_always_blocked(&"fd00:ec2::254".parse().unwrap()));
        assert!(is_always_blocked(&"fe80::1".parse().unwrap()));
        assert!(is_always_blocked(&"100.100.100.200".parse().unwrap()));
        assert!(!is_always_blocked(&"10.0.0.5".parse().unwrap()));
        assert!(!is_always_blocked(&"8.8.8.8".parse().unwrap()));
        assert!(is_private(&"10.0.0.5".parse().unwrap()));
        assert!(is_private(&"127.0.0.1".parse().unwrap()));
        assert!(is_private(&"::1".parse().unwrap()));
        assert!(!is_private(&"8.8.8.8".parse().unwrap()));
    }
}
