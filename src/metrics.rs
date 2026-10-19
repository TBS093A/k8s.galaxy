//! Resource usage: live CPU/memory from metrics-server (`metrics.k8s.io`) when it is installed,
//! plus requests vs allocatable, which every cluster has.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use kube::api::{DynamicObject, ListParams};
use kube::discovery::ApiResource;
use kube::{Api, Client};

/// Millicores and bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub cpu_m: i64,
    pub mem: i64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        self.cpu_m += o.cpu_m;
        self.mem += o.mem;
    }
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub nodes: BTreeMap<String, Usage>,
    pub pods: BTreeMap<(String, String), Usage>,
}

/// CPU quantity → millicores: "250m", "1", "1.5", "123456789n", "500u".
pub fn cpu_milli(q: &str) -> i64 {
    let q = q.trim();
    let (num, unit) = split_unit(q);
    let v: f64 = num.parse().unwrap_or(0.0);
    let milli = match unit {
        "n" => v / 1e6,
        "u" => v / 1e3,
        "m" => v,
        "k" => v * 1e6,
        _ => v * 1e3,
    };
    milli.round() as i64
}

/// Memory quantity → bytes: "128Mi", "1Gi", "500M", "123456", "1e9".
pub fn bytes(q: &str) -> i64 {
    let q = q.trim();
    if let Ok(v) = q.parse::<f64>() {
        return v as i64;
    }
    let (num, unit) = split_unit(q);
    let v: f64 = num.parse().unwrap_or(0.0);
    let k = match unit {
        "Ki" => 1024f64,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "m" => 1e-3,
        _ => 1.0,
    };
    (v * k) as i64
}

fn split_unit(q: &str) -> (&str, &str) {
    let i = q.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(q.len());
    (&q[..i], &q[i..])
}

pub fn fmt_cpu(m: i64) -> String {
    if m >= 1000 { format!("{:.1}", m as f64 / 1000.0) } else { format!("{m}m") }
}

pub fn fmt_mem(b: i64) -> String {
    let mi = b as f64 / 1024.0 / 1024.0;
    if mi >= 1024.0 { format!("{:.1}Gi", mi / 1024.0) } else { format!("{:.0}Mi", mi) }
}

pub async fn fetch(client: &Client, nodes: &ApiResource, pods: &ApiResource) -> Result<Metrics> {
    let lp = ListParams::default();
    let (n, p) = tokio::try_join!(
        async { Api::<DynamicObject>::all_with(client.clone(), nodes).list(&lp).await.context("listing node metrics") },
        async { Api::<DynamicObject>::all_with(client.clone(), pods).list(&lp).await.context("listing pod metrics") },
    )?;
    let mut m = Metrics::default();
    for node in n.items {
        let u = &node.data["usage"];
        m.nodes.insert(
            node.metadata.name.unwrap_or_default(),
            Usage { cpu_m: cpu_milli(u["cpu"].as_str().unwrap_or("0")), mem: bytes(u["memory"].as_str().unwrap_or("0")) },
        );
    }
    for pod in p.items {
        let mut total = Usage::default();
        for c in pod.data["containers"].as_array().into_iter().flatten() {
            total += Usage {
                cpu_m: cpu_milli(c["usage"]["cpu"].as_str().unwrap_or("0")),
                mem: bytes(c["usage"]["memory"].as_str().unwrap_or("0")),
            };
        }
        m.pods.insert((pod.metadata.namespace.unwrap_or_default(), pod.metadata.name.unwrap_or_default()), total);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        assert_eq!(cpu_milli("250m"), 250);
        assert_eq!(cpu_milli("2"), 2000);
        assert_eq!(cpu_milli("1.5"), 1500);
        assert_eq!(cpu_milli("123456789n"), 123);
        assert_eq!(cpu_milli("1500u"), 2);
        assert_eq!(bytes("128Mi"), 128 * 1024 * 1024);
        assert_eq!(bytes("1Gi"), 1 << 30);
        assert_eq!(bytes("500M"), 500_000_000);
        assert_eq!(bytes("7918932Ki"), 7918932 * 1024);
        assert_eq!(bytes("1e3"), 1000);
        assert_eq!(fmt_mem(1536 * 1024 * 1024), "1.5Gi");
        assert_eq!(fmt_cpu(1250), "1.2");
    }
}
