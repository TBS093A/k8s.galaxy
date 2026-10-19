//! What the cluster costs on Hetzner Cloud: servers, volumes, snapshots, IPs and load balancers
//! with list prices (gross) from the API, how long the servers have been up, and what stays
//! billed when the cluster is powered down (snapshots).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use k8s_openapi::jiff::Timestamp;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// "server", "volume", "snapshot", "primary ip", "floating ip", "load balancer".
    pub kind: &'static str,
    pub name: String,
    /// Server type, size, address…
    pub detail: String,
    pub location: String,
    pub hourly: f64,
    pub monthly: f64,
    /// Server id, to match nodes.
    pub id: String,
    pub created: Option<Timestamp>,
    pub running: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Costs {
    pub currency: String,
    pub items: Vec<Item>,
}

impl Costs {
    pub fn monthly(&self) -> f64 {
        self.items.iter().map(|i| i.monthly).sum()
    }

    pub fn hourly(&self) -> f64 {
        self.items.iter().map(|i| if i.hourly > 0.0 { i.hourly } else { i.monthly / 730.0 }).sum()
    }

    /// What is still billed with every server deleted (the power-down state): everything but servers.
    pub fn monthly_when_down(&self) -> f64 {
        self.items.iter().filter(|i| i.kind != "server").map(|i| i.monthly).sum()
    }

    /// Oldest creation time of the servers: when the cluster was last brought up.
    pub fn up_since(&self) -> Option<Timestamp> {
        self.items.iter().filter(|i| i.kind == "server").filter_map(|i| i.created).min()
    }

    /// Cost of the servers since they were created (hourly price × hours, capped at the monthly price per month).
    pub fn spent_since_up(&self) -> f64 {
        let now = Timestamp::now().as_second();
        self.items
            .iter()
            .filter(|i| i.kind == "server")
            .filter_map(|i| i.created.map(|c| (i, (now - c.as_second()).max(0) as f64 / 3600.0)))
            .map(|(i, hours)| (i.hourly * hours).min(i.monthly * (hours / 730.0).ceil().max(1.0)))
            .sum()
    }

    pub fn server(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.kind == "server" && i.id == id)
    }

    pub fn symbol(&self) -> &str {
        match self.currency.as_str() {
            "EUR" | "" => "€",
            "USD" => "$",
            other => other,
        }
    }
}

fn f(v: &Value) -> f64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// Gross price for a location from a `prices` list.
fn price_at(prices: &[Value], location: &str) -> (f64, f64) {
    prices
        .iter()
        .find(|p| p["location"] == location)
        .or_else(|| prices.first())
        .map(|p| (f(&p["price_hourly"]["gross"]), f(&p["price_monthly"]["gross"])))
        .unwrap_or((0.0, 0.0))
}

async fn get(client: &reqwest::Client, token: &str, path: &str) -> Result<Vec<Value>> {
    let key = path.split('?').next().unwrap_or(path).trim_start_matches('/').to_string();
    let mut out = vec![];
    let mut page = 1;
    loop {
        let sep = if path.contains('?') { '&' } else { '?' };
        let resp = client
            .get(format!("https://api.hetzner.cloud/v1/{path}{sep}per_page=50&page={page}"))
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| format!("Hetzner API {key}"))?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            bail!("Hetzner API {key} {status}: {}", s(&body["error"]["message"]));
        }
        out.extend(arr(&body, &key).iter().cloned());
        match body.pointer("/meta/pagination/next_page").and_then(Value::as_u64) {
            Some(next) => page = next,
            None => break,
        }
    }
    Ok(out)
}

pub async fn hetzner(token: &str) -> Result<Costs> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let pricing: Value =
        client.get("https://api.hetzner.cloud/v1/pricing").bearer_auth(token).send().await.context("Hetzner API pricing")?.json().await?;
    let (servers, volumes, images, primary, floating, lbs) = tokio::try_join!(
        get(&client, token, "servers"),
        get(&client, token, "volumes"),
        get(&client, token, "images?type=snapshot"),
        get(&client, token, "primary_ips"),
        get(&client, token, "floating_ips"),
        get(&client, token, "load_balancers"),
    )?;
    Ok(parse(&pricing["pricing"], &servers, &volumes, &images, &primary, &floating, &lbs))
}

pub fn parse(
    pricing: &Value,
    servers: &[Value],
    volumes: &[Value],
    images: &[Value],
    primary: &[Value],
    floating: &[Value],
    lbs: &[Value],
) -> Costs {
    let mut items = vec![];
    let loc = |v: &Value| {
        let l = if v["location"].is_object() { &v["location"]["name"] } else { &v["datacenter"]["location"]["name"] };
        s(l)
    };
    for sv in servers {
        let location = loc(sv);
        let (hourly, monthly) = price_at(arr(&sv["server_type"], "prices"), &location);
        items.push(Item {
            kind: "server",
            name: s(&sv["name"]),
            detail: s(&sv["server_type"]["name"]),
            hourly,
            monthly,
            id: sv["id"].as_u64().map(|i| i.to_string()).unwrap_or_default(),
            created: sv["created"].as_str().and_then(|c| c.parse().ok()),
            running: sv["status"] == "running",
            location,
        });
    }
    let per_gb = |key: &str| f(&pricing[key]["price_per_gb_month"]["gross"]);
    for v in volumes {
        let size = v["size"].as_f64().unwrap_or(0.0);
        items.push(Item {
            kind: "volume",
            name: s(&v["name"]),
            detail: format!("{size:.0} GB"),
            location: loc(v),
            hourly: 0.0,
            monthly: size * per_gb("volume"),
            id: String::new(),
            created: None,
            running: true,
        });
    }
    for i in images {
        let size = i["image_size"].as_f64().unwrap_or(0.0);
        items.push(Item {
            kind: "snapshot",
            name: Some(s(&i["description"])).filter(|d| !d.is_empty()).unwrap_or_else(|| format!("#{}", i["id"])),
            detail: format!("{size:.1} GB"),
            location: String::new(),
            hourly: 0.0,
            monthly: size * per_gb("image"),
            id: String::new(),
            created: i["created"].as_str().and_then(|c| c.parse().ok()),
            running: true,
        });
    }
    for ip in primary {
        let location = loc(ip);
        let prices = arr(pricing, "primary_ips").iter().find(|p| p["type"] == ip["type"]).map(|p| arr(p, "prices")).unwrap_or(&[]);
        let (hourly, monthly) = price_at(prices, &location);
        items.push(Item {
            kind: "primary ip",
            name: s(&ip["name"]),
            detail: s(&ip["ip"]),
            location,
            hourly,
            monthly,
            id: String::new(),
            created: None,
            running: true,
        });
    }
    for ip in floating {
        let location = s(&ip["home_location"]["name"]);
        let prices = arr(pricing, "floating_ips").iter().find(|p| p["type"] == ip["type"]).map(|p| arr(p, "prices")).unwrap_or(&[]);
        let (_, monthly) = price_at(prices, &location);
        items.push(Item {
            kind: "floating ip",
            name: s(&ip["name"]),
            detail: s(&ip["ip"]),
            location,
            hourly: 0.0,
            monthly,
            id: String::new(),
            created: None,
            running: true,
        });
    }
    for lb in lbs {
        let location = loc(lb);
        let (hourly, monthly) = price_at(arr(&lb["load_balancer_type"], "prices"), &location);
        items.push(Item {
            kind: "load balancer",
            name: s(&lb["name"]),
            detail: s(&lb["load_balancer_type"]["name"]),
            location,
            hourly,
            monthly,
            id: String::new(),
            created: None,
            running: true,
        });
    }
    Costs { currency: s(&pricing["currency"]), items }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prices_and_totals() {
        let pricing = json!({"currency": "EUR", "volume": {"price_per_gb_month": {"gross": "0.0440"}},
            "image": {"price_per_gb_month": {"gross": "0.0119"}},
            "primary_ips": [{"type": "ipv4", "prices": [{"location": "fsn1", "price_hourly": {"gross": "0.0010"}, "price_monthly": {"gross": "0.6000"}}]}],
            "floating_ips": [{"type": "ipv4", "prices": [{"location": "fsn1", "price_monthly": {"gross": "3.5700"}}]}]});
        let server = json!({"id": 7, "name": "k8s-node-000", "status": "running", "created": "2026-10-18T10:00:00Z",
            "location": {"name": "fsn1"},
            "server_type": {"name": "cx42", "prices": [{"location": "fsn1", "price_hourly": {"gross": "0.0302"}, "price_monthly": {"gross": "18.8500"}}]}});
        let c = parse(
            &pricing,
            &[server],
            &[json!({"name": "data", "size": 50, "location": {"name": "fsn1"}})],
            &[json!({"id": 9, "description": "k8s-node-000 snapshot", "image_size": 20.0})],
            &[json!({"name": "ip", "type": "ipv4", "ip": "1.2.3.4", "location": {"name": "fsn1"}})],
            &[json!({"name": "dns", "type": "ipv4", "ip": "5.6.7.8", "home_location": {"name": "fsn1"}})],
            &[],
        );
        assert_eq!(c.symbol(), "€");
        assert_eq!(c.server("7").unwrap().detail, "cx42");
        assert!((c.monthly() - (18.85 + 50.0 * 0.044 + 20.0 * 0.0119 + 0.6 + 3.57)).abs() < 1e-9);
        assert!((c.monthly_when_down() - (c.monthly() - 18.85)).abs() < 1e-9);
        assert!(c.up_since().is_some());
        assert!(c.spent_since_up() > 0.0);
    }
}
