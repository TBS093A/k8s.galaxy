//! On-demand list prices for clusters on AWS, GCP and Azure: what the nodes (instance type ×
//! region), their block volumes and a managed control plane cost. Prices come from each
//! provider's pricing API and are cached for hours — they change rarely.
//!
//! - Azure: the public Retail Prices API (no login).
//! - AWS: the Pricing API through the `aws` CLI (needs `pricing:GetProducts`).
//! - GCP: the Cloud Billing Catalog API with a `gcloud auth print-access-token` token, plus
//!   `gcloud compute machine-types describe` for vCPUs and memory.
//!
//! Spot/preemptible discounts, savings plans, committed use, traffic and support are not included.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::process::Command;

use crate::costs::{Costs, Item};
use crate::infra::{Hosting, Infra};
use crate::model::NodeInfo;
use crate::storage::StorageInfo;

const TTL: Duration = Duration::from_secs(6 * 3600);
/// Fee for a managed control plane per hour (EKS standard support, GKE Standard).
const CONTROL_PLANE_HOURLY: f64 = 0.10;
const HOURS_PER_MONTH: f64 = 730.0;

#[derive(Default)]
pub struct PriceCache {
    /// key → (price, fetched): instance prices per hour, disk prices per GB-month or per disk-month.
    prices: BTreeMap<String, (f64, Instant)>,
    gcp_skus: Option<(Vec<Value>, Instant)>,
}

impl PriceCache {
    fn get(&self, key: &str) -> Option<f64> {
        self.prices.get(key).filter(|(_, t)| t.elapsed() < TTL).map(|(p, _)| *p)
    }

    fn put(&mut self, key: String, price: f64) {
        self.prices.insert(key, (price, Instant::now()));
    }
}

/// Which provider runs a node, from its providerID.
pub fn provider_of(n: &NodeInfo) -> Option<&'static str> {
    match n.provider_id.split("://").next()? {
        "aws" => Some("AWS"),
        "gce" => Some("GCP"),
        "azure" => Some("Azure"),
        _ => None,
    }
}

fn instance_type(n: &NodeInfo) -> Option<String> {
    n.labels.get("node.kubernetes.io/instance-type").or_else(|| n.labels.get("beta.kubernetes.io/instance-type")).cloned()
}

fn region(n: &NodeInfo) -> String {
    if !n.region.is_empty() {
        return n.region.clone();
    }
    n.labels.get("topology.kubernetes.io/region").cloned().unwrap_or_default()
}

/// Costs per provider found among the nodes (only AWS, GCP and Azure here; Hetzner has its own API).
pub async fn costs(nodes: &[NodeInfo], storage: &StorageInfo, infra: &Infra, cache: &mut PriceCache) -> Vec<(String, Result<Costs>)> {
    let mut out = vec![];
    for provider in ["AWS", "GCP", "Azure"] {
        let ours: Vec<&NodeInfo> = nodes.iter().filter(|n| provider_of(n) == Some(provider)).collect();
        if ours.is_empty() {
            continue;
        }
        let res = provider_costs(provider, &ours, storage, infra, cache).await;
        out.push((provider.to_string(), res));
    }
    out
}

async fn provider_costs(
    provider: &str,
    nodes: &[&NodeInfo],
    storage: &StorageInfo,
    infra: &Infra,
    cache: &mut PriceCache,
) -> Result<Costs> {
    let mut items = vec![];
    let mut errors = vec![];
    for n in nodes {
        let (Some(ty), reg) = (instance_type(n), region(n)) else {
            errors.push(format!("{}: no instance-type label", n.name));
            continue;
        };
        let price = match provider {
            "AWS" => aws_instance(&ty, &reg, cache).await,
            "GCP" => gcp_instance(&ty, &reg, n.zone.as_str(), cache).await,
            _ => azure_instance(&ty, &reg, cache).await,
        };
        match price {
            Ok(hourly) => items.push(Item {
                kind: "server",
                name: n.name.clone(),
                detail: ty,
                location: reg,
                hourly,
                monthly: hourly * HOURS_PER_MONTH,
                id: n.name.clone(),
                created: None,
                running: n.ready,
            }),
            Err(e) => errors.push(format!("{} ({ty}): {e:#}", n.name)),
        }
    }
    // Block volumes of this provider's CSI driver.
    let reg = nodes.first().map(|n| region(n)).unwrap_or_default();
    for v in &storage.volumes {
        let driver = v.driver.as_str();
        let ours = match provider {
            "AWS" => driver.contains("ebs.csi.aws.com") || driver.contains("aws-ebs"),
            "GCP" => driver.contains("pd.csi.storage.gke.io") || driver.contains("gce-pd"),
            _ => driver.contains("disk.csi.azure.com") || driver.contains("azure-disk"),
        };
        let (Some(pv), Some(size)) = (&v.pv, v.size()) else { continue };
        if !ours {
            continue;
        }
        let gb = size as f64 / 1e9;
        let monthly = match provider {
            "AWS" => aws_disk(&v.class, &reg, cache).await.map(|p| p * gb),
            "GCP" => gcp_disk(&v.class, &reg, cache).await.map(|p| p * gb),
            _ => azure_disk(&v.class, size, &reg, cache).await,
        };
        match monthly {
            Ok(m) => items.push(Item {
                kind: "volume",
                name: v.claim.clone().unwrap_or_else(|| pv.clone()),
                detail: format!("{:.0} GB {}", gb, v.class),
                location: reg.clone(),
                hourly: 0.0,
                monthly: m,
                id: String::new(),
                created: None,
                running: true,
            }),
            Err(e) => errors.push(format!("volume {pv}: {e:#}")),
        }
    }
    // Managed control plane fee (EKS, GKE Standard; AKS's free tier has none).
    if let Hosting::Managed(svc) = infra.hosting
        && ((svc == "EKS" && provider == "AWS") || (svc == "GKE" && provider == "GCP"))
    {
        items.push(Item {
            kind: "control plane",
            name: svc.to_string(),
            detail: "managed control plane".into(),
            location: reg,
            hourly: CONTROL_PLANE_HOURLY,
            monthly: CONTROL_PLANE_HOURLY * HOURS_PER_MONTH,
            id: String::new(),
            created: None,
            running: true,
        });
    }
    if items.is_empty() && !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(Costs { provider: provider.to_string(), currency: "USD".into(), items, notes: errors })
}

// ───────────────────────────── Azure ─────────────────────────────

async fn azure_query(filter: &str) -> Result<Vec<Value>> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let url = reqwest::Url::parse_with_params("https://prices.azure.com/api/retail/prices", &[("$filter", filter)])?;
    let body: Value = client.get(url).send().await.context("Azure Retail Prices API")?.json().await?;
    Ok(body["Items"].as_array().cloned().unwrap_or_default())
}

/// Linux, on-demand (no Windows licence, no Spot / Low Priority).
pub fn azure_pick_vm(items: &[Value]) -> Option<f64> {
    items
        .iter()
        .filter(|i| {
            let product = i["productName"].as_str().unwrap_or_default();
            let sku = i["skuName"].as_str().unwrap_or_default();
            !product.contains("Windows") && !sku.contains("Spot") && !sku.contains("Low Priority") && i["unitOfMeasure"] == "1 Hour"
        })
        .filter_map(|i| i["retailPrice"].as_f64())
        .reduce(f64::min)
}

async fn azure_instance(sku: &str, region: &str, cache: &mut PriceCache) -> Result<f64> {
    let key = format!("azure/vm/{region}/{sku}");
    if let Some(p) = cache.get(&key) {
        return Ok(p);
    }
    let filter = format!(
        "serviceName eq 'Virtual Machines' and armRegionName eq '{region}' and armSkuName eq '{sku}' and priceType eq 'Consumption'"
    );
    let price = azure_pick_vm(&azure_query(&filter).await?).ok_or_else(|| anyhow!("no on-demand Linux price for {sku} in {region}"))?;
    cache.put(key, price);
    Ok(price)
}

/// Managed disks are billed per size tier: the smallest tier that fits.
pub fn azure_tier(class: &str, size_bytes: i64) -> String {
    let gib = size_bytes as f64 / (1u64 << 30) as f64;
    let tiers =
        [(4, 1), (8, 2), (16, 3), (32, 4), (64, 6), (128, 10), (256, 15), (512, 20), (1024, 30), (2048, 40), (4096, 50), (8192, 60)];
    let n = tiers.iter().find(|(cap, _)| gib <= *cap as f64).map_or(80, |(_, n)| *n);
    let c = class.to_lowercase();
    let letter = if c.contains("premium") {
        'P'
    } else if c.contains("standard-hdd") || c.contains("standard_lrs") {
        'S'
    } else {
        'E'
    };
    format!("{letter}{n}")
}

async fn azure_disk(class: &str, size: i64, region: &str, cache: &mut PriceCache) -> Result<f64> {
    let tier = azure_tier(class, size);
    let key = format!("azure/disk/{region}/{tier}");
    if let Some(p) = cache.get(&key) {
        return Ok(p);
    }
    let filter =
        format!("serviceName eq 'Storage' and armRegionName eq '{region}' and skuName eq '{tier} LRS' and priceType eq 'Consumption'");
    let items = azure_query(&filter).await?;
    let price = items
        .iter()
        .filter(|i| i["meterName"].as_str() == Some(&format!("{tier} LRS Disk")) && i["unitOfMeasure"] == "1/Month")
        .filter_map(|i| i["retailPrice"].as_f64())
        .next()
        .ok_or_else(|| anyhow!("no price for a {tier} LRS disk in {region}"))?;
    cache.put(key, price);
    Ok(price)
}

// ───────────────────────────── AWS ─────────────────────────────

async fn aws_products(filters: &[(&str, &str)]) -> Result<Vec<Value>> {
    let mut cmd = Command::new("aws");
    cmd.args(["pricing", "get-products", "--region", "us-east-1", "--service-code", "AmazonEC2", "--output", "json", "--filters"]);
    for (field, value) in filters {
        cmd.arg(format!("Type=TERM_MATCH,Field={field},Value={value}"));
    }
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await.context("aws pricing: timeout")?.context("running aws")?;
    if !out.status.success() {
        bail!("aws pricing: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    // PriceList items are JSON documents encoded as strings.
    Ok(v["PriceList"].as_array().into_iter().flatten().filter_map(|s| serde_json::from_str(s.as_str()?).ok()).collect())
}

/// First on-demand USD price of a Pricing API product.
pub fn aws_on_demand(products: &[Value]) -> Option<f64> {
    products.iter().find_map(|p| {
        let od = p["terms"]["OnDemand"].as_object()?;
        od.values().find_map(|term| {
            term["priceDimensions"]
                .as_object()?
                .values()
                .find_map(|d| d["pricePerUnit"]["USD"].as_str()?.parse::<f64>().ok().filter(|p| *p > 0.0))
        })
    })
}

async fn aws_instance(ty: &str, region: &str, cache: &mut PriceCache) -> Result<f64> {
    let key = format!("aws/ec2/{region}/{ty}");
    if let Some(p) = cache.get(&key) {
        return Ok(p);
    }
    let products = aws_products(&[
        ("instanceType", ty),
        ("regionCode", region),
        ("operatingSystem", "Linux"),
        ("tenancy", "Shared"),
        ("preInstalledSw", "NA"),
        ("capacitystatus", "Used"),
    ])
    .await?;
    let price = aws_on_demand(&products).ok_or_else(|| anyhow!("no on-demand Linux price for {ty} in {region}"))?;
    cache.put(key, price);
    Ok(price)
}

async fn aws_disk(class: &str, region: &str, cache: &mut PriceCache) -> Result<f64> {
    let c = class.to_lowercase();
    let api = ["gp2", "io1", "io2", "st1", "sc1"].into_iter().find(|t| c.contains(t)).unwrap_or("gp3");
    let key = format!("aws/ebs/{region}/{api}");
    if let Some(p) = cache.get(&key) {
        return Ok(p);
    }
    let products = aws_products(&[("productFamily", "Storage"), ("regionCode", region), ("volumeApiName", api)]).await?;
    let price = aws_on_demand(&products).ok_or_else(|| anyhow!("no EBS {api} price in {region}"))?;
    cache.put(key, price);
    Ok(price)
}

// ───────────────────────────── GCP ─────────────────────────────

async fn gcloud_json(args: &[&str]) -> Result<Value> {
    let out = tokio::time::timeout(Duration::from_secs(30), Command::new("gcloud").args(args).arg("--format=json").output())
        .await
        .context("gcloud: timeout")?
        .context("running gcloud")?;
    if !out.status.success() {
        bail!("gcloud {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

async fn gcp_skus(cache: &mut PriceCache) -> Result<Vec<Value>> {
    if let Some((skus, t)) = &cache.gcp_skus
        && t.elapsed() < TTL
    {
        return Ok(skus.clone());
    }
    let token = Command::new("gcloud").args(["auth", "print-access-token"]).output().await.context("running gcloud")?;
    if !token.status.success() {
        bail!("gcloud auth print-access-token: {}", String::from_utf8_lossy(&token.stderr).trim());
    }
    let token = String::from_utf8_lossy(&token.stdout).trim().to_string();
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    // Compute Engine's service id in the Cloud Billing Catalog.
    let mut skus = vec![];
    let mut page = String::new();
    loop {
        let mut params = vec![("currencyCode", "USD"), ("pageSize", "5000")];
        if !page.is_empty() {
            params.push(("pageToken", page.as_str()));
        }
        let url = reqwest::Url::parse_with_params("https://cloudbilling.googleapis.com/v1/services/6F81-5844-456A/skus", &params)?;
        let body: Value = client.get(url).bearer_auth(&token).send().await.context("Cloud Billing Catalog API")?.json().await?;
        if let Some(e) = body["error"]["message"].as_str() {
            bail!("Cloud Billing Catalog API: {e}");
        }
        skus.extend(body["skus"].as_array().cloned().unwrap_or_default());
        match body["nextPageToken"].as_str() {
            Some(t) if !t.is_empty() => page = t.to_string(),
            _ => break,
        }
    }
    cache.gcp_skus = Some((skus.clone(), Instant::now()));
    Ok(skus)
}

fn sku_price(sku: &Value) -> Option<f64> {
    let rate = sku["pricingInfo"][0]["pricingExpression"]["tieredRates"].as_array()?.last()?;
    let p = &rate["unitPrice"];
    Some(p["units"].as_str().and_then(|u| u.parse::<f64>().ok()).unwrap_or(0.0) + p["nanos"].as_f64().unwrap_or(0.0) / 1e9)
}

/// On-demand per-vCPU-hour and per-GiB-hour price of a machine family in a region.
pub fn gcp_core_ram(skus: &[Value], family: &str, region: &str) -> Option<(f64, f64)> {
    let fam = family.to_uppercase();
    let matching = |what: &str| {
        skus.iter()
            .filter(|s| {
                let d = s["description"].as_str().unwrap_or_default();
                d.starts_with(&format!("{fam} "))
                    && d.contains(what)
                    && !d.contains("Custom")
                    && !d.contains("Sole Tenancy")
                    && !d.contains("Preemptible")
                    && !d.contains("Spot")
                    && s["category"]["usageType"] == "OnDemand"
                    && s["serviceRegions"].as_array().is_some_and(|r| r.iter().any(|x| x == region))
            })
            .find_map(sku_price)
    };
    Some((matching("Instance Core")?, matching("Instance Ram")?))
}

async fn gcp_instance(ty: &str, region: &str, zone: &str, cache: &mut PriceCache) -> Result<f64> {
    let key = format!("gcp/vm/{region}/{ty}");
    if let Some(p) = cache.get(&key) {
        return Ok(p);
    }
    let zone = if zone.is_empty() { format!("{region}-a") } else { zone.to_string() };
    let mt = gcloud_json(&["compute", "machine-types", "describe", ty, &format!("--zone={zone}")]).await?;
    let cpus = mt["guestCpus"].as_f64().ok_or_else(|| anyhow!("{ty}: no guestCpus"))?;
    let gib = mt["memoryMb"].as_f64().unwrap_or(0.0) / 1024.0;
    let family = ty.split('-').next().unwrap_or(ty);
    let skus = gcp_skus(cache).await?;
    let (core, ram) = gcp_core_ram(&skus, family, region).ok_or_else(|| anyhow!("no {family} core/ram SKU for {region}"))?;
    let price = cpus * core + gib * ram;
    cache.put(key, price);
    Ok(price)
}

async fn gcp_disk(class: &str, region: &str, cache: &mut PriceCache) -> Result<f64> {
    let c = class.to_lowercase();
    let what = if c.contains("ssd") {
        "SSD backed PD Capacity"
    } else if c.contains("standard") {
        "Storage PD Capacity"
    } else {
        "Balanced PD Capacity"
    };
    let key = format!("gcp/pd/{region}/{what}");
    if let Some(p) = cache.get(&key) {
        return Ok(p);
    }
    let skus = gcp_skus(cache).await?;
    let price = skus
        .iter()
        .filter(|s| {
            s["description"].as_str().is_some_and(|d| d.starts_with(what) && !d.contains("Regional"))
                && s["serviceRegions"].as_array().is_some_and(|r| r.iter().any(|x| x == region))
        })
        .find_map(sku_price)
        .ok_or_else(|| anyhow!("no '{what}' price in {region}"))?;
    cache.put(key, price);
    Ok(price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Network: the real Azure Retail Prices API. `cargo test azure_live -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn azure_live() {
        let mut cache = PriceCache::default();
        let vm = azure_instance("Standard_D4s_v5", "westeurope", &mut cache).await.unwrap();
        let disk = azure_disk("managed-csi", 100 << 30, "westeurope", &mut cache).await.unwrap();
        println!("D4s_v5 westeurope ${vm}/h, E10 disk ${disk}/month");
        assert!(vm > 0.05 && vm < 2.0 && disk > 1.0 && disk < 50.0);
    }

    #[test]
    fn azure_picks_linux_on_demand() {
        let items = vec![
            json!({"retailPrice": 0.23, "unitOfMeasure": "1 Hour", "productName": "Virtual Machines Dsv5 Series", "skuName": "Standard_D4s_v5"}),
            json!({"retailPrice": 0.0425, "unitOfMeasure": "1 Hour", "productName": "Virtual Machines Dsv5 Series", "skuName": "Standard_D4s_v5 Spot"}),
            json!({"retailPrice": 0.414, "unitOfMeasure": "1 Hour", "productName": "Virtual Machines Dsv5 Series Windows", "skuName": "Standard_D4s_v5"}),
        ];
        assert_eq!(azure_pick_vm(&items), Some(0.23));
        assert_eq!(azure_tier("managed-csi", 100 << 30), "E10");
        assert_eq!(azure_tier("managed-csi-premium", 300 << 30), "P20");
    }

    #[test]
    fn aws_reads_on_demand_terms() {
        let p = json!({"terms": {"OnDemand": {"X.Y": {"priceDimensions": {"X.Y.Z": {"pricePerUnit": {"USD": "0.0960000000"}}}}}}});
        assert_eq!(aws_on_demand(&[p]), Some(0.096));
    }

    #[test]
    fn gcp_core_and_ram_skus() {
        let sku = |d: &str, units: &str, nanos: i64| {
            json!({"description": d, "category": {"usageType": "OnDemand"}, "serviceRegions": ["europe-west1"],
                   "pricingInfo": [{"pricingExpression": {"tieredRates": [{"unitPrice": {"units": units, "nanos": nanos}}]}}]})
        };
        let skus = vec![
            sku("E2 Instance Core running in Belgium", "0", 21_811_590),
            sku("E2 Instance Ram running in Belgium", "0", 2_923_530),
            sku("Spot Preemptible E2 Instance Core running in Belgium", "0", 6_000_000),
        ];
        let (core, ram) = gcp_core_ram(&skus, "e2", "europe-west1").unwrap();
        assert!((core - 0.02181159).abs() < 1e-9 && (ram - 0.00292353).abs() < 1e-9);
    }
}
