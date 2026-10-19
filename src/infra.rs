//! What a cluster runs on: a managed service (EKS, GKE, AKS…), self-managed nodes on cloud VMs,
//! bare metal (or VMs without any cloud integration), a local dev cluster, or a mix — plus the
//! distribution (kubeadm/kubespray, k3s, rke2, Talos, OpenShift…). Pure heuristics over the API
//! server URL, its version string and node labels / providerIDs / OS images.

use crate::model::NodeInfo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hosting {
    /// Control plane run by the provider.
    Managed(&'static str),
    /// Self-managed control plane on one provider's VMs.
    CloudVms(String),
    /// No cloud provider integration on any node.
    BareMetal,
    /// Developer cluster on this machine (kind, minikube, k3d, Docker Desktop…).
    Local(&'static str),
    /// Nodes of several kinds, e.g. Hetzner Cloud VMs + a homelab box.
    Hybrid(Vec<String>),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Infra {
    pub hosting: Hosting,
    pub distro: Option<&'static str>,
    /// Control-plane nodes are visible as Node objects (never the case on managed services).
    pub control_plane_visible: bool,
}

impl Default for Infra {
    fn default() -> Self {
        Infra { hosting: Hosting::Unknown, distro: None, control_plane_visible: false }
    }
}

impl Infra {
    /// Short badge for the header / cluster list, e.g. `managed · EKS`, `Hetzner Cloud VMs · kubespray`.
    pub fn badge(&self) -> String {
        let mut parts = vec![match &self.hosting {
            Hosting::Managed(p) => format!("managed {p}"),
            Hosting::CloudVms(p) => format!("{p} VMs"),
            Hosting::BareMetal => "bare metal".into(),
            Hosting::Local(p) => format!("local {p}"),
            Hosting::Hybrid(kinds) => format!("hybrid: {}", kinds.join(" + ")),
            Hosting::Unknown => "unknown".into(),
        }];
        if let Some(d) = self.distro
            && !matches!(self.hosting, Hosting::Managed(_) | Hosting::Local(_))
        {
            parts.push(d.to_string());
        }
        parts.join(" · ")
    }

    pub fn managed(&self) -> bool {
        matches!(self.hosting, Hosting::Managed(_))
    }
}

/// Managed offerings recognised from node labels.
const MANAGED_LABELS: &[(&str, &str)] = &[
    ("eks.amazonaws.com/nodegroup", "EKS"),
    ("eks.amazonaws.com/compute-type", "EKS"),
    ("alpha.eksctl.io/cluster-name", "EKS"),
    ("cloud.google.com/gke-nodepool", "GKE"),
    ("kubernetes.azure.com/cluster", "AKS"),
    ("kubernetes.azure.com/agentpool", "AKS"),
    ("doks.digitalocean.com/node-id", "DOKS"),
    ("lke.linode.com/pool-id", "LKE"),
    ("oci.oraclecloud.com/fault-domain", "OKE"),
    ("k8s.scaleway.com/pool-name", "Kapsule"),
    ("k8s.ovh.net/nodepool", "OVH MKS"),
    ("node.kubernetes.io/exoscale-sks-nodepool", "SKS"),
    ("ack.aliyun.com", "ACK"),
    ("tke.cloud.tencent.com/nodepool-id", "TKE"),
];

/// API server hosts of managed services.
const MANAGED_HOSTS: &[(&str, &str)] = &[
    (".eks.amazonaws.com", "EKS"),
    (".azmk8s.io", "AKS"),
    (".k8s.ondigitalocean.com", "DOKS"),
    (".linodelke.net", "LKE"),
    (".oraclecloud.com", "OKE"),
    (".k8s.scw.cloud", "Kapsule"),
    (".k8s.ovh.net", "OVH MKS"),
    (".sks-", "SKS"),
    ("container.googleapis.com", "GKE"),
];

fn provider_of_id(provider_id: &str) -> Option<&'static str> {
    let scheme = provider_id.split("://").next()?;
    Some(match scheme {
        "aws" => "AWS",
        "gce" => "GCP",
        "azure" => "Azure",
        "hcloud" => "Hetzner Cloud",
        "digitalocean" => "DigitalOcean",
        "linode" => "Linode",
        "oci" => "OCI",
        "scaleway" => "Scaleway",
        "openstack" => "OpenStack",
        "vsphere" => "vSphere",
        "equinixmetal" | "packet" => "Equinix Metal",
        "ovhcloud" => "OVHcloud",
        _ => return None,
    })
}

/// Provider of the cloud inventory match (`hetzner k8s-node-000 #42` → Hetzner Cloud).
fn provider_of_match(cloud: &str) -> Option<String> {
    let p = cloud.split_whitespace().next()?;
    Some(match p {
        "hetzner" => "Hetzner Cloud".into(),
        "aws" => "AWS".into(),
        "gcp" => "GCP".into(),
        "azure" => "Azure".into(),
        other => other.to_string(),
    })
}

fn local_kind(n: &NodeInfo) -> Option<&'static str> {
    if n.provider_id.starts_with("kind://") {
        Some("kind")
    } else if n.labels.contains_key("minikube.k8s.io/name") {
        Some("minikube")
    } else if n.name == "docker-desktop" {
        Some("Docker Desktop")
    } else if n.name.starts_with("k3d-") {
        Some("k3d")
    } else if n.name == "rancher-desktop" || n.name == "lima-rancher-desktop" {
        Some("Rancher Desktop")
    } else if n.name == "orbstack" {
        Some("OrbStack")
    } else {
        None
    }
}

/// What one node is, for the details panel: `EKS node m5.large`, `Hetzner Cloud VM cx42`, `bare metal`.
pub fn node_kind(n: &NodeInfo) -> String {
    let instance = n.labels.get("node.kubernetes.io/instance-type").or_else(|| n.labels.get("beta.kubernetes.io/instance-type"));
    let instance = instance.filter(|t| *t != "k3s").map(|t| format!(" {t}")).unwrap_or_default();
    if let Some((_, svc)) = MANAGED_LABELS.iter().find(|(l, _)| n.labels.contains_key(*l)) {
        let fargate = n.labels.get("eks.amazonaws.com/compute-type").is_some_and(|t| t == "fargate");
        return if fargate { "EKS Fargate".into() } else { format!("{svc} node{instance}") };
    }
    if let Some(l) = local_kind(n) {
        return format!("local {l}");
    }
    if let Some(p) = provider_of_id(&n.provider_id) {
        return if p == "Equinix Metal" { format!("{p} bare metal{instance}") } else { format!("{p} VM{instance}") };
    }
    if let Some(p) = n.cloud.as_deref().and_then(provider_of_match) {
        return format!("{p} VM{instance}");
    }
    "bare metal (no cloud provider)".into()
}

/// Node kind reduced to a hosting bucket.
fn bucket(n: &NodeInfo) -> String {
    if let Some((_, svc)) = MANAGED_LABELS.iter().find(|(l, _)| n.labels.contains_key(*l)) {
        return svc.to_string();
    }
    if let Some(l) = local_kind(n) {
        return format!("local {l}");
    }
    if let Some(p) = provider_of_id(&n.provider_id) {
        return p.to_string();
    }
    if let Some(p) = n.cloud.as_deref().and_then(provider_of_match) {
        return p;
    }
    "bare metal".into()
}

fn distro(version: &str, nodes: &[NodeInfo], hints: &[String]) -> Option<&'static str> {
    let any_label = |k: &str| nodes.iter().any(|n| n.labels.contains_key(k));
    let any_os = |s: &str| nodes.iter().any(|n| n.os_image.contains(s));
    if version.contains("+k3s") || any_label("k3s.io/hostname") || nodes.iter().any(|n| n.provider_id.starts_with("k3s://")) {
        return Some("k3s");
    }
    if version.contains("+rke2") || any_label("rke2.io/hostname") {
        return Some("rke2");
    }
    if any_os("Talos") {
        return Some("Talos");
    }
    if any_label("node.openshift.io/os_id") || any_os("Red Hat Enterprise Linux CoreOS") {
        return Some("OpenShift");
    }
    if any_label("microk8s.io/cluster") || version.contains("microk8s") {
        return Some("MicroK8s");
    }
    if version.contains("+vmware") {
        return Some("Tanzu");
    }
    if version.contains("-eks-") {
        return Some("EKS");
    }
    if version.contains("-gke.") {
        return Some("GKE");
    }
    if hints.iter().any(|h| h.contains("kubespray")) {
        return Some("kubespray");
    }
    if nodes.iter().any(|n| n.annotations.contains_key("kubeadm.alpha.kubernetes.io/cri-socket")) {
        return Some("kubeadm");
    }
    None
}

/// `server` is the API server URL from the kubeconfig, `version` its gitVersion, `hints` names of
/// ClusterRoleBindings (installers leave traces there, e.g. kubespray).
pub fn classify(server: &str, version: &str, nodes: &[NodeInfo], hints: &[String]) -> Infra {
    let host = server.trim_start_matches("https://").trim_start_matches("http://").split(['/', ':']).next().unwrap_or("").to_lowercase();
    let control_plane_visible = nodes.iter().any(|n| n.is_control_plane());
    let distro = distro(version, nodes, hints);

    let managed = MANAGED_HOSTS
        .iter()
        .find(|(h, _)| host.contains(h))
        .map(|(_, s)| *s)
        .or_else(|| (version.contains("-eks-")).then_some("EKS"))
        .or_else(|| (version.contains("-gke.")).then_some("GKE"))
        .or_else(|| nodes.iter().find_map(|n| MANAGED_LABELS.iter().find(|(l, _)| n.labels.contains_key(*l)).map(|(_, s)| *s)));
    // A managed service never shows its control plane; a cluster that does is self-managed even
    // if some node pools carry a provider's labels.
    if let Some(m) = managed
        && !control_plane_visible
    {
        return Infra { hosting: Hosting::Managed(m), distro, control_plane_visible };
    }

    if let Some(l) = nodes.iter().find_map(local_kind) {
        return Infra { hosting: Hosting::Local(l), distro, control_plane_visible };
    }
    if nodes.is_empty() && matches!(host.as_str(), "127.0.0.1" | "localhost" | "0.0.0.0" | "host.docker.internal") {
        return Infra { hosting: Hosting::Local("cluster"), distro, control_plane_visible };
    }

    let mut kinds: Vec<String> = nodes.iter().map(bucket).collect();
    kinds.sort();
    kinds.dedup();
    let hosting = match kinds.as_slice() {
        [] => Hosting::Unknown,
        [one] if one == "bare metal" => Hosting::BareMetal,
        [one] => Hosting::CloudVms(one.clone()),
        many => Hosting::Hybrid(many.to_vec()),
    };
    Infra { hosting, distro, control_plane_visible }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, labels: &[(&str, &str)], provider_id: &str, cloud: Option<&str>) -> NodeInfo {
        let mut n = NodeInfo::test(name);
        n.labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        n.provider_id = provider_id.into();
        n.cloud = cloud.map(str::to_string);
        n
    }

    #[test]
    fn managed_services() {
        let eks = node(
            "ip-10-0-1-5.ec2.internal",
            &[("eks.amazonaws.com/nodegroup", "ng-1"), ("node.kubernetes.io/instance-type", "m5.large")],
            "aws:///eu-west-1a/i-1",
            None,
        );
        let i = classify("https://ABC.gr7.eu-west-1.eks.amazonaws.com", "v1.30.2-eks-1552ad0", std::slice::from_ref(&eks), &[]);
        assert_eq!(i.hosting, Hosting::Managed("EKS"));
        assert_eq!(i.badge(), "managed EKS");
        assert_eq!(node_kind(&eks), "EKS node m5.large");
        let gke = node("gke-pool-1", &[("cloud.google.com/gke-nodepool", "pool-1")], "gce://p/europe-west1-b/gke-pool-1", None);
        assert_eq!(classify("https://34.1.2.3", "v1.31.1-gke.1678000", &[gke], &[]).hosting, Hosting::Managed("GKE"));
        assert_eq!(classify("https://x.hcp.westeurope.azmk8s.io:443", "v1.30.0", &[], &[]).hosting, Hosting::Managed("AKS"));
    }

    #[test]
    fn self_managed_and_bare_metal() {
        let mut cp = node("k8s.node.000", &[("node-role.kubernetes.io/control-plane", "")], "", Some("hetzner k8s-node-000 #1"));
        cp.roles = vec!["control-plane".into()];
        cp.annotations.insert("kubeadm.alpha.kubernetes.io/cri-socket".into(), "unix:///var/run/containerd/containerd.sock".into());
        let w = node("k8s.node.001", &[], "", Some("hetzner k8s-node-001 #2"));
        let i = classify("https://91.98.1.1:6443", "v1.26.3", &[cp.clone(), w], &["kubespray:system:node".into()]);
        assert_eq!(i.hosting, Hosting::CloudVms("Hetzner Cloud".into()));
        assert_eq!(i.badge(), "Hetzner Cloud VMs · kubespray");

        let metal = node("rack-1", &[], "", None);
        let i = classify("https://192.168.1.250:6443", "v1.31.0+k3s1", std::slice::from_ref(&metal), &[]);
        assert_eq!(i.badge(), "bare metal · k3s");

        let i = classify("https://91.98.1.1:6443", "v1.26.3", &[cp, metal], &[]);
        assert_eq!(i.hosting, Hosting::Hybrid(vec!["Hetzner Cloud".into(), "bare metal".into()]));

        let kind = node("dev-control-plane", &[], "kind://docker/dev/dev-control-plane", None);
        assert_eq!(classify("https://127.0.0.1:39143", "v1.33.1", &[kind], &[]).hosting, Hosting::Local("kind"));
    }
}
