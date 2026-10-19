//! Resolves where a node physically lives, from labels, annotations and CLI overrides.

use std::collections::HashMap;

use crate::cloud::{self, CloudInstance};
use crate::model::NodeInfo;

/// Explicit "lat,lon" on a node wins over everything.
pub const GEO_ANNOTATION: &str = "k8s.galaxy/geo";
/// A place key (e.g. `waw`, `fsn1`) looked up in the table / overrides.
pub const LOCATION_LABEL: &str = "k8s.galaxy/location";

const REGION_LABELS: &[&str] = &["topology.kubernetes.io/region", "failure-domain.beta.kubernetes.io/region"];
const ZONE_LABELS: &[&str] = &["topology.kubernetes.io/zone", "failure-domain.beta.kubernetes.io/zone"];

/// (key, lat, lon, city)
const PLACES: &[(&str, f64, f64, &str)] = &[
    // Hetzner
    ("fsn1", 50.48, 12.37, "Falkenstein"),
    ("nbg1", 49.45, 11.08, "Nuremberg"),
    ("hel1", 60.17, 24.94, "Helsinki"),
    ("ash", 39.04, -77.49, "Ashburn"),
    ("hil", 45.52, -122.99, "Hillsboro"),
    ("sin", 1.35, 103.82, "Singapore"),
    // homelab shortcuts
    ("waw", 52.23, 21.01, "Warsaw"),
    ("krk", 50.06, 19.94, "Kraków"),
    ("wro", 51.11, 17.03, "Wrocław"),
    ("gdn", 54.35, 18.65, "Gdańsk"),
    ("poz", 52.41, 16.93, "Poznań"),
    // AWS
    ("us-east-1", 39.04, -77.49, "N. Virginia"),
    ("us-east-2", 40.0, -82.9, "Ohio"),
    ("us-west-1", 37.35, -121.95, "N. California"),
    ("us-west-2", 45.84, -119.7, "Oregon"),
    ("ca-central-1", 45.5, -73.57, "Montreal"),
    ("sa-east-1", -23.55, -46.63, "São Paulo"),
    ("eu-west-1", 53.35, -6.26, "Dublin"),
    ("eu-west-2", 51.51, -0.13, "London"),
    ("eu-west-3", 48.86, 2.35, "Paris"),
    ("eu-central-1", 50.11, 8.68, "Frankfurt"),
    ("eu-central-2", 47.37, 8.54, "Zurich"),
    ("eu-north-1", 59.33, 18.07, "Stockholm"),
    ("eu-south-1", 45.46, 9.19, "Milan"),
    ("ap-south-1", 19.08, 72.88, "Mumbai"),
    ("ap-southeast-1", 1.35, 103.82, "Singapore"),
    ("ap-southeast-2", -33.87, 151.21, "Sydney"),
    ("ap-northeast-1", 35.69, 139.69, "Tokyo"),
    ("ap-northeast-2", 37.57, 126.98, "Seoul"),
    // GCP
    ("europe-west1", 50.45, 3.82, "St. Ghislain"),
    ("europe-west2", 51.51, -0.13, "London"),
    ("europe-west3", 50.11, 8.68, "Frankfurt"),
    ("europe-west4", 53.44, 6.83, "Eemshaven"),
    ("europe-central2", 52.23, 21.01, "Warsaw"),
    ("europe-north1", 60.57, 27.19, "Hamina"),
    ("us-central1", 41.26, -95.86, "Iowa"),
    ("us-east1", 33.2, -80.0, "S. Carolina"),
    ("us-east4", 39.04, -77.49, "N. Virginia"),
    ("us-west1", 45.6, -121.18, "Oregon"),
    ("asia-east1", 24.05, 120.52, "Taiwan"),
    ("asia-northeast1", 35.69, 139.69, "Tokyo"),
    // Azure
    ("westeurope", 52.37, 4.9, "Amsterdam"),
    ("northeurope", 53.35, -6.26, "Dublin"),
    ("polandcentral", 52.23, 21.01, "Warsaw"),
    ("germanywestcentral", 50.11, 8.68, "Frankfurt"),
    ("eastus", 37.37, -79.82, "Virginia"),
    ("westus2", 47.23, -119.85, "Washington"),
];

#[derive(Clone)]
pub struct Resolver {
    /// key (node name / region / zone / location) -> pinned place
    pub overrides: HashMap<String, Place>,
    /// Fallback for unlabeled nodes, e.g. a homelab.
    pub home: Option<(f64, f64)>,
}

impl Resolver {
    pub fn apply(&self, node: &mut NodeInfo, instances: &[CloudInstance]) {
        // The backing VM, from the provider API or the providerID itself.
        let inst = cloud::match_node(node, instances).cloned().or_else(|| cloud::from_provider_id(&node.provider_id));
        if let Some(i) = &inst {
            let provider = i.provider.map(|p| p.to_string()).unwrap_or_default();
            let name = i.names.first().filter(|n| !n.is_empty()).cloned().unwrap_or_default();
            let id = if i.id.is_empty() || i.id.starts_with('/') { String::new() } else { format!(" #{}", i.id) };
            node.cloud = Some(format!("{provider} {name}{id}").trim().to_string());
            if node.region.is_empty() {
                node.region = i.location.clone();
            }
            if node.zone.is_empty() {
                node.zone = i.zone.clone();
            }
        }
        let (geo, place) = self.resolve(node, inst.as_ref());
        node.geo = geo.map(|(lat, lon)| (lon, lat));
        node.place = place;
    }

    fn resolve(&self, node: &NodeInfo, inst: Option<&CloudInstance>) -> (Option<(f64, f64)>, String) {
        if let Some(ll) = node.annotations.get(GEO_ANNOTATION).and_then(|v| parse_lat_lon(v).ok()) {
            return (Some(ll), "annotated".into());
        }
        if let Some(p) = self.overrides.get(&node.name) {
            return (Some((p.lat, p.lon)), p.label.clone());
        }
        let mut keys: Vec<String> = vec![];
        keys.extend(node.labels.get(LOCATION_LABEL).cloned());
        keys.extend(REGION_LABELS.iter().filter_map(|l| node.labels.get(*l).cloned()));
        for zone in ZONE_LABELS.iter().filter_map(|l| node.labels.get(*l)) {
            keys.push(zone.clone());
            keys.push(cloud::region_of_zone(zone));
        }
        for key in &keys {
            if let Some(p) = self.overrides.get(key).cloned().or_else(|| known_place(key)) {
                return (Some((p.lat, p.lon)), p.label);
            }
        }
        if let Some(i) = inst {
            let key = &i.location;
            let provider = i.provider.map(|p| p.to_string()).unwrap_or_default();
            if let Some(p) = self.overrides.get(key).cloned().or_else(|| known_place(key)) {
                return (Some((p.lat, p.lon)), p.label);
            }
            if let Some(ll) = i.coords {
                let city = i.city.clone().unwrap_or_else(|| key.clone());
                return (Some(ll), format!("{city} ({key})"));
            }
            if !key.is_empty() {
                return (None, format!("{provider} {key} (unknown region)"));
            }
        }
        match self.home {
            Some(ll) => (Some(ll), "home".into()),
            None => (None, "unplaced".into()),
        }
    }
}

pub fn parse_lat_lon(s: &str) -> Result<(f64, f64), String> {
    let (lat, lon) = s.split_once(',').ok_or("expected LAT,LON")?;
    let lat: f64 = lat.trim().parse().map_err(|e| format!("lat: {e}"))?;
    let lon: f64 = lon.trim().parse().map_err(|e| format!("lon: {e}"))?;
    Ok((lat, lon))
}

#[derive(Clone, Debug)]
pub struct Place {
    pub lat: f64,
    pub lon: f64,
    pub label: String,
}

fn known_place(key: &str) -> Option<Place> {
    PLACES.iter().find(|p| p.0 == key).map(|&(_, lat, lon, city)| Place { lat, lon, label: format!("{city} ({key})") })
}

/// `KEY=LAT,LON` or `KEY=PLACE` (a table key like `fsn1`) for `--place`.
pub fn parse_place(s: &str) -> Result<(String, Place), String> {
    let (key, value) = s.split_once('=').ok_or("expected KEY=LAT,LON or KEY=PLACE")?;
    let place = match parse_lat_lon(value) {
        Ok((lat, lon)) => Place { lat, lon, label: key.to_string() },
        Err(_) => known_place(value).ok_or_else(|| format!("unknown place `{value}`, use LAT,LON"))?,
    };
    Ok((key.to_string(), place))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_normalize() {
        assert_eq!(cloud::region_of_zone("fsn1-dc14"), "fsn1");
        assert_eq!(cloud::region_of_zone("us-east-1a"), "us-east-1");
        assert_eq!(cloud::region_of_zone("europe-west1-b"), "europe-west1");
    }
}
