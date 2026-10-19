//! Cloud provider inventories: where each VM lives, straight from the provider API.
//!
//! Hetzner is queried over REST (`HCLOUD_TOKEN`); AWS, GCP and Azure go through their
//! CLIs (`aws`, `gcloud`, `az`) so every auth flow they support (SSO, ADC, `az login`…) just works.
//! Instances are matched to nodes by providerID, then name, then IP.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::process::Command;

use crate::model::NodeInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    Hetzner,
    Aws,
    Gcp,
    Azure,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Provider::Hetzner => "hetzner",
            Provider::Aws => "aws",
            Provider::Gcp => "gcp",
            Provider::Azure => "azure",
        })
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "hetzner" | "hcloud" => Ok(Provider::Hetzner),
            "aws" | "ec2" => Ok(Provider::Aws),
            "gcp" | "gce" | "google" => Ok(Provider::Gcp),
            "azure" | "az" => Ok(Provider::Azure),
            other => Err(format!("unknown provider `{other}` (hetzner, aws, gcp, azure)")),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CloudInstance {
    pub provider: Option<Provider>,
    /// Provider-native id: hcloud server id, EC2 instance id, GCE instance id, Azure resource id.
    pub id: String,
    /// Server name plus aliases (EC2 private DNS name, Azure computer name, GCE name).
    pub names: Vec<String>,
    /// Region-level key, e.g. `fsn1`, `eu-west-1`, `europe-west3`, `westeurope`.
    pub location: String,
    pub zone: String,
    pub ips: Vec<String>,
    /// Exact coordinates when the provider publishes them (Hetzner does).
    pub coords: Option<(f64, f64)>,
    pub city: Option<String>,
}

#[derive(Clone)]
pub struct CloudConfig {
    /// Explicit provider list; `None` = auto-detect.
    pub providers: Option<Vec<Provider>>,
    pub hcloud_token: Option<String>,
    pub aws_regions: Vec<String>,
    pub gcp_project: Option<String>,
}

impl CloudConfig {
    /// Auto mode: Hetzner when a token is available, others when node providerIDs point at them.
    /// AWS and GCE providerIDs already carry the zone, so those are only queried when asked for.
    fn active(&self, nodes: &[NodeInfo]) -> Vec<Provider> {
        if let Some(explicit) = &self.providers {
            return explicit.clone();
        }
        let mut out = vec![];
        if self.hcloud_token.is_some() {
            out.push(Provider::Hetzner);
        }
        if nodes.iter().any(|n| n.provider_id.starts_with("azure://")) {
            out.push(Provider::Azure);
        }
        out
    }
}

/// Query every active provider concurrently. Failures are reported, never fatal.
pub async fn inventory(cfg: &CloudConfig, nodes: &[NodeInfo]) -> (Vec<CloudInstance>, Vec<(Provider, String)>) {
    let providers = cfg.active(nodes);
    let futures = providers.iter().map(|&p| async move {
        let res = match p {
            Provider::Hetzner => match &cfg.hcloud_token {
                Some(token) => hetzner(token).await,
                None => Err(anyhow::anyhow!("HCLOUD_TOKEN not set (or use --hcloud-env-file)")),
            },
            Provider::Aws => aws(&cfg.aws_regions).await,
            Provider::Gcp => gcp(cfg.gcp_project.as_deref()).await,
            Provider::Azure => azure().await,
        };
        (p, res)
    });
    let mut instances = vec![];
    let mut errors = vec![];
    for (p, res) in futures::future::join_all(futures).await {
        match res {
            Ok(mut list) => {
                list.iter_mut().for_each(|i| i.provider = Some(p));
                instances.extend(list);
            }
            Err(e) => errors.push((p, format!("{e:#}"))),
        }
    }
    (instances, errors)
}

/// Location straight from a providerID, no API call: AWS and GCE embed the zone.
pub fn from_provider_id(pid: &str) -> Option<CloudInstance> {
    if let Some(rest) = pid.strip_prefix("aws://") {
        // aws:///eu-west-1a/i-0abc
        let parts: Vec<_> = rest.trim_start_matches('/').split('/').collect();
        if let [zone, id] = parts[..] {
            return Some(CloudInstance {
                provider: Some(Provider::Aws),
                id: id.into(),
                location: region_of_zone(zone),
                zone: zone.into(),
                ..Default::default()
            });
        }
    }
    if let Some(rest) = pid.strip_prefix("gce://") {
        // gce://project/europe-west3-a/instance-name
        let parts: Vec<_> = rest.split('/').collect();
        if let [_, zone, name] = parts[..] {
            return Some(CloudInstance {
                provider: Some(Provider::Gcp),
                names: vec![name.into()],
                location: region_of_zone(zone),
                zone: zone.into(),
                ..Default::default()
            });
        }
    }
    None
}

/// Find the instance backing a node: providerID, then name, then any shared IP.
pub fn match_node<'a>(node: &NodeInfo, instances: &'a [CloudInstance]) -> Option<&'a CloudInstance> {
    let pid = node.provider_id.to_ascii_lowercase();
    if !pid.is_empty() {
        let by_id = instances.iter().find(|i| {
            let id = i.id.to_ascii_lowercase();
            !id.is_empty()
                && match i.provider {
                    Some(Provider::Hetzner) => pid == format!("hcloud://{id}"),
                    Some(Provider::Aws) => pid.starts_with("aws://") && pid.ends_with(&format!("/{id}")),
                    Some(Provider::Gcp) => {
                        pid.starts_with("gce://") && i.names.iter().any(|n| pid.ends_with(&format!("/{}", n.to_ascii_lowercase())))
                    }
                    // VMs: azure:///subscriptions/.../virtualMachines/x == id;
                    // VMSS instances: providerID extends the scale set id with /virtualMachines/N.
                    Some(Provider::Azure) => pid.trim_start_matches("azure://").starts_with(&id),
                    None => false,
                }
        });
        if by_id.is_some() {
            return by_id;
        }
    }
    let name = norm(&node.name);
    let short = name.split('.').next().unwrap_or(&name).to_string();
    if let Some(i) = instances.iter().find(|i| i.names.iter().any(|n| norm(n) == name || norm(n) == short)) {
        return Some(i);
    }
    instances.iter().find(|i| i.ips.iter().any(|ip| node.addresses.contains(ip)))
}

/// `web.node.01` and `web-node-01` are the same machine.
fn norm(s: &str) -> String {
    s.to_ascii_lowercase().replace(['.', '_'], "-")
}

/// `eu-west-1a` -> `eu-west-1`, `europe-west3-a` -> `europe-west3`, `fsn1-dc14` -> `fsn1`.
pub fn region_of_zone(zone: &str) -> String {
    if let Some((head, tail)) = zone.rsplit_once('-')
        && (tail.starts_with("dc") || tail.len() == 1)
    {
        return head.to_string();
    }
    let b = zone.as_bytes();
    if b.len() > 2 && b[b.len() - 1].is_ascii_lowercase() && b[b.len() - 2].is_ascii_digit() {
        return zone[..zone.len() - 1].to_string();
    }
    zone.to_string()
}

/// Read `HCLOUD_TOKEN` from a dotenv-style file without exporting anything else.
pub fn token_from_env_file(path: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text.lines().find_map(|line| {
        let line = line.trim().trim_start_matches("export ").trim();
        let value = line.strip_prefix("HCLOUD_TOKEN=")?;
        Some(value.trim_matches(['"', '\'']).to_string()).filter(|v| !v.is_empty())
    }))
}

// ---------------------------------------------------------------- Hetzner

async fn hetzner(token: &str) -> Result<Vec<CloudInstance>> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let mut out = vec![];
    let mut page = 1;
    loop {
        let resp = client
            .get(format!("https://api.hetzner.cloud/v1/servers?per_page=50&page={page}"))
            .bearer_auth(token)
            .send()
            .await
            .context("Hetzner API")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("Hetzner API response")?;
        if !status.is_success() {
            bail!("Hetzner API {status}: {}", str_at(&body, &["error", "message"]));
        }
        out.extend(parse_hetzner(&body));
        match body.pointer("/meta/pagination/next_page").and_then(Value::as_u64) {
            Some(next) => page = next,
            None => return Ok(out),
        }
    }
}

pub fn parse_hetzner(body: &Value) -> Vec<CloudInstance> {
    arr(body, "servers")
        .iter()
        .map(|s| {
            // `location` is top-level since Hetzner retired `datacenter` (2025-12); older payloads nest it.
            let loc = if s["location"].is_object() { &s["location"] } else { &s["datacenter"]["location"] };
            let mut ips: Vec<String> = vec![];
            ips.extend(s.pointer("/public_net/ipv4/ip").and_then(Value::as_str).map(str::to_string));
            ips.extend(arr(s, "private_net").iter().filter_map(|n| n["ip"].as_str().map(str::to_string)));
            CloudInstance {
                provider: Some(Provider::Hetzner),
                id: s["id"].as_u64().map(|i| i.to_string()).unwrap_or_default(),
                names: vec![str_at(s, &["name"])],
                location: str_at(loc, &["name"]),
                zone: Some(str_at(s, &["datacenter", "name"])).filter(|z| !z.is_empty()).unwrap_or_else(|| str_at(loc, &["name"])),
                ips,
                coords: loc["latitude"].as_f64().zip(loc["longitude"].as_f64()),
                city: loc["city"].as_str().map(str::to_string),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- AWS

async fn aws(regions: &[String]) -> Result<Vec<CloudInstance>> {
    let regions: Vec<Option<&str>> =
        if regions.is_empty() { vec![None] } else { regions.iter().map(|r| Some(r.as_str())).collect() };
    let mut out = vec![];
    for region in regions {
        let mut args = vec!["ec2", "describe-instances", "--output", "json"];
        if let Some(r) = region {
            args.extend(["--region", r]);
        }
        out.extend(parse_aws(&cli_json("aws", &args).await?));
    }
    Ok(out)
}

pub fn parse_aws(body: &Value) -> Vec<CloudInstance> {
    arr(body, "Reservations")
        .iter()
        .flat_map(|r| arr(r, "Instances"))
        .map(|i| {
            let zone = str_at(i, &["Placement", "AvailabilityZone"]);
            let mut names = vec![];
            names.extend(
                arr(i, "Tags")
                    .iter()
                    .find(|t| t["Key"] == "Name")
                    .and_then(|t| t["Value"].as_str().map(str::to_string)),
            );
            names.extend(i["PrivateDnsName"].as_str().filter(|s| !s.is_empty()).map(str::to_string));
            let ips = ["PrivateIpAddress", "PublicIpAddress"]
                .iter()
                .filter_map(|k| i[*k].as_str().map(str::to_string))
                .collect();
            CloudInstance {
                provider: Some(Provider::Aws),
                id: str_at(i, &["InstanceId"]),
                names,
                location: region_of_zone(&zone),
                zone,
                ips,
                ..Default::default()
            }
        })
        .collect()
}

// ---------------------------------------------------------------- GCP

async fn gcp(project: Option<&str>) -> Result<Vec<CloudInstance>> {
    let mut args = vec!["compute", "instances", "list", "--format=json"];
    let flag;
    if let Some(p) = project {
        flag = format!("--project={p}");
        args.push(&flag);
    }
    Ok(parse_gcp(&cli_json("gcloud", &args).await?))
}

pub fn parse_gcp(body: &Value) -> Vec<CloudInstance> {
    body.as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|i| {
            // zone is a URL: .../projects/p/zones/europe-west3-a
            let zone = str_at(i, &["zone"]).rsplit('/').next().unwrap_or_default().to_string();
            let mut ips = vec![];
            for nic in arr(i, "networkInterfaces") {
                ips.extend(nic["networkIP"].as_str().map(str::to_string));
                ips.extend(arr(nic, "accessConfigs").iter().filter_map(|a| a["natIP"].as_str().map(str::to_string)));
            }
            CloudInstance {
                provider: Some(Provider::Gcp),
                id: str_at(i, &["id"]),
                names: vec![str_at(i, &["name"])],
                location: region_of_zone(&zone),
                zone,
                ips,
                ..Default::default()
            }
        })
        .collect()
}

// ---------------------------------------------------------------- Azure

async fn azure() -> Result<Vec<CloudInstance>> {
    let (vms, vmss) = tokio::join!(
        cli_json("az", &["vm", "list", "--show-details", "--output", "json"]),
        cli_json("az", &["vmss", "list", "--output", "json"]),
    );
    let mut out = parse_azure_vms(&vms?);
    // AKS nodes live in scale sets; not every account has them, so that half is best effort.
    if let Ok(vmss) = vmss {
        out.extend(parse_azure_vmss(&vmss));
    }
    Ok(out)
}

pub fn parse_azure_vms(body: &Value) -> Vec<CloudInstance> {
    body.as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|vm| {
            let split = |k: &str| -> Vec<String> {
                vm[k].as_str().unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
            };
            let mut names = vec![str_at(vm, &["name"])];
            names.extend(vm.pointer("/osProfile/computerName").and_then(Value::as_str).map(str::to_string));
            let location = str_at(vm, &["location"]);
            let zone = arr(vm, "zones").first().and_then(Value::as_str).map(|z| format!("{location}-{z}")).unwrap_or_default();
            CloudInstance {
                provider: Some(Provider::Azure),
                id: str_at(vm, &["id"]),
                names,
                location,
                zone,
                ips: [split("privateIps"), split("publicIps")].concat(),
                ..Default::default()
            }
        })
        .collect()
}

pub fn parse_azure_vmss(body: &Value) -> Vec<CloudInstance> {
    body.as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|s| CloudInstance {
            provider: Some(Provider::Azure),
            id: str_at(s, &["id"]),
            // AKS node names are the scale set name + instance suffix: aks-pool-123-vmss000000.
            names: vec![],
            location: str_at(s, &["location"]),
            ..Default::default()
        })
        .collect()
}

// ---------------------------------------------------------------- helpers

async fn cli_json(cmd: &str, args: &[&str]) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        Command::new(cmd).args(args).kill_on_drop(true).output(),
    )
    .await
    .with_context(|| format!("{cmd} timed out"))?
    .with_context(|| format!("running `{cmd}` (is it installed?)"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("{cmd}: {}", err.lines().find(|l| !l.trim().is_empty()).unwrap_or("failed"));
    }
    serde_json::from_slice(&output.stdout).with_context(|| format!("parsing {cmd} output"))
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn str_at(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for k in path {
        cur = &cur[*k];
    }
    match cur {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, pid: &str, addrs: &[&str]) -> NodeInfo {
        let mut n = crate::demo::Sim::new().snapshot().nodes.remove(0);
        n.name = name.into();
        n.provider_id = pid.into();
        n.addresses = addrs.iter().map(|s| s.to_string()).collect();
        n
    }

    #[test]
    fn hetzner_parse_and_match_by_name_and_ip() {
        let body = json!({"servers": [{
            "id": 4242, "name": "web-node-01",
            "datacenter": {"name": "fsn1-dc14", "location": {"name": "fsn1", "city": "Falkenstein", "latitude": 50.47612, "longitude": 12.370071}},
            "public_net": {"ipv4": {"ip": "1.2.3.4"}},
            "private_net": [{"ip": "10.0.0.2"}]
        }], "meta": {"pagination": {"next_page": null}}});
        let list = parse_hetzner(&body);
        assert_eq!(list[0].location, "fsn1");
        assert_eq!(list[0].coords, Some((50.47612, 12.370071)));
        assert!(match_node(&node("web.node.01", "", &[]), &list).is_some());
        assert!(match_node(&node("other", "", &["10.0.0.2"]), &list).is_some());
        assert!(match_node(&node("other", "hcloud://4242", &[]), &list).is_some());
        assert!(match_node(&node("other", "", &["10.9.9.9"]), &list).is_none());

        // Current API shape: no datacenter, location at the top level.
        let body = json!({"servers": [{"id": 1, "name": "n", "datacenter": null,
            "location": {"name": "hel1", "city": "Helsinki", "latitude": 60.17, "longitude": 24.94}}]});
        let list = parse_hetzner(&body);
        assert_eq!((list[0].location.as_str(), list[0].zone.as_str()), ("hel1", "hel1"));
        assert_eq!(list[0].city.as_deref(), Some("Helsinki"));
    }

    #[test]
    fn aws_parse_and_provider_id() {
        let body = json!({"Reservations": [{"Instances": [{
            "InstanceId": "i-0abc", "Placement": {"AvailabilityZone": "eu-central-1b"},
            "PrivateIpAddress": "10.1.2.3", "PrivateDnsName": "ip-10-1-2-3.eu-central-1.compute.internal",
            "Tags": [{"Key": "Name", "Value": "worker-a"}]
        }]}]});
        let list = parse_aws(&body);
        assert_eq!(list[0].location, "eu-central-1");
        assert!(match_node(&node("ip-10-1-2-3.eu-central-1.compute.internal", "", &[]), &list).is_some());
        assert!(match_node(&node("x", "aws:///eu-central-1b/i-0abc", &[]), &list).is_some());
        let direct = from_provider_id("aws:///eu-west-1a/i-0def").unwrap();
        assert_eq!((direct.location.as_str(), direct.zone.as_str()), ("eu-west-1", "eu-west-1a"));
    }

    #[test]
    fn gcp_parse_and_provider_id() {
        let body = json!([{
            "id": "123", "name": "gke-pool-1-abcd",
            "zone": "https://www.googleapis.com/compute/v1/projects/p/zones/europe-west3-a",
            "networkInterfaces": [{"networkIP": "10.156.0.2", "accessConfigs": [{"natIP": "34.1.1.1"}]}]
        }]);
        let list = parse_gcp(&body);
        assert_eq!(list[0].location, "europe-west3");
        assert!(match_node(&node("x", "gce://p/europe-west3-a/gke-pool-1-abcd", &[]), &list).is_some());
        assert_eq!(from_provider_id("gce://p/us-central1-f/n").unwrap().location, "us-central1");
    }

    #[test]
    fn azure_vm_and_vmss_match() {
        let vms = json!([{
            "id": "/subscriptions/s/resourceGroups/RG/providers/Microsoft.Compute/virtualMachines/node-1",
            "name": "node-1", "location": "westeurope", "zones": ["2"],
            "privateIps": "10.240.0.4", "publicIps": ""
        }]);
        let vmss = json!([{
            "id": "/subscriptions/s/resourceGroups/MC_rg/providers/Microsoft.Compute/virtualMachineScaleSets/aks-pool-1-vmss",
            "name": "aks-pool-1-vmss", "location": "polandcentral"
        }]);
        let mut list = parse_azure_vms(&vms);
        list.extend(parse_azure_vmss(&vmss));
        let pid = "azure:///subscriptions/s/resourceGroups/mc_rg/providers/Microsoft.Compute/virtualMachineScaleSets/aks-pool-1-vmss/virtualMachines/0";
        assert_eq!(match_node(&node("aks-pool-1-vmss000000", pid, &[]), &list).unwrap().location, "polandcentral");
        assert_eq!(match_node(&node("node-1", "", &[]), &list).unwrap().zone, "westeurope-2");
    }

    #[test]
    fn env_file_token() {
        let dir = std::env::temp_dir().join(format!("kg-test-{}", std::process::id()));
        std::fs::write(&dir, "FOO=1\nexport HCLOUD_TOKEN=\"abc\"\n").unwrap();
        assert_eq!(token_from_env_file(&dir).unwrap().as_deref(), Some("abc"));
        std::fs::remove_file(dir).ok();
    }
}
