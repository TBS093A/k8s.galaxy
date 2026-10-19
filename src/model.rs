use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    Bad,
    Warn,
    Ok,
}

#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub name: String,
    pub ready: bool,
    pub roles: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub region: String,
    pub zone: String,
    pub cpu: String,
    pub memory: String,
    pub allocatable: crate::metrics::Usage,
    pub kubelet: String,
    /// e.g. "Ubuntu 22.04.4 LTS", "Talos (v1.7.5)", "Bottlerocket OS 1.20.0".
    pub os_image: String,
    pub internal_ip: String,
    pub provider_id: String,
    /// Every address the kubelet reports (InternalIP, ExternalIP, Hostname).
    pub addresses: Vec<String>,
    /// Backing cloud instance, e.g. `hetzner k8s-node-000 #4242`, once matched.
    pub cloud: Option<String>,
    /// (lon, lat) once resolved by `geo::Resolver`.
    pub geo: Option<(f64, f64)>,
    pub place: String,
}

impl NodeInfo {
    #[cfg(test)]
    pub fn test(name: &str) -> Self {
        NodeInfo {
            name: name.into(),
            ready: true,
            roles: vec![],
            labels: Default::default(),
            annotations: Default::default(),
            region: String::new(),
            zone: String::new(),
            cpu: String::new(),
            memory: String::new(),
            allocatable: Default::default(),
            kubelet: String::new(),
            os_image: String::new(),
            internal_ip: String::new(),
            provider_id: String::new(),
            addresses: vec![],
            cloud: None,
            geo: None,
            place: String::new(),
        }
    }

    pub fn health(&self) -> Health {
        if self.ready { Health::Ok } else { Health::Bad }
    }

    pub fn is_control_plane(&self) -> bool {
        self.roles.iter().any(|r| r == "control-plane" || r == "master")
    }
}

#[derive(Clone, Debug)]
pub struct PodInfo {
    pub name: String,
    pub namespace: String,
    pub node: Option<String>,
    pub phase: String,
    /// Waiting/terminated reason of the worst container, e.g. CrashLoopBackOff.
    pub reason: Option<String>,
    pub restarts: i32,
    pub ready: bool,
    pub containers: Vec<String>,
    /// Controlling owner, e.g. ("ReplicaSet", "api-5ff86cc56").
    pub owner: Option<(String, String)>,
    /// Sum of container requests.
    pub requests: crate::metrics::Usage,
}

const BAD_REASONS: &[&str] = &[
    "CrashLoopBackOff",
    "Error",
    "ImagePullBackOff",
    "ErrImagePull",
    "OOMKilled",
    "CreateContainerConfigError",
    "InvalidImageName",
];

impl PodInfo {
    pub fn health(&self) -> Health {
        let bad_reason = self.reason.as_deref().is_some_and(|r| BAD_REASONS.contains(&r));
        if self.phase == "Failed" || bad_reason {
            Health::Bad
        } else if self.phase == "Pending" || (self.phase == "Running" && !self.ready) {
            Health::Warn
        } else {
            Health::Ok
        }
    }

    pub fn status(&self) -> &str {
        self.reason.as_deref().unwrap_or(&self.phase)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadKind {
    Deployment,
    ReplicaSet,
    DaemonSet,
    StatefulSet,
}

impl WorkloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadKind::Deployment => "Deployment",
            WorkloadKind::ReplicaSet => "ReplicaSet",
            WorkloadKind::DaemonSet => "DaemonSet",
            WorkloadKind::StatefulSet => "StatefulSet",
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkloadInfo {
    pub kind: WorkloadKind,
    pub name: String,
    pub namespace: String,
    pub desired: i32,
    pub ready: i32,
    pub image: String,
    /// Controlling owner (a ReplicaSet's Deployment).
    pub owner: Option<(String, String)>,
}

impl WorkloadInfo {
    pub fn health(&self) -> Health {
        if self.desired > 0 && self.ready == 0 {
            Health::Bad
        } else if self.ready < self.desired {
            Health::Warn
        } else {
            Health::Ok
        }
    }

    /// Scaled-to-zero ReplicaSets left behind by rollouts.
    pub fn is_stale(&self) -> bool {
        self.kind == WorkloadKind::ReplicaSet && self.desired == 0 && self.ready == 0
    }
}

#[derive(Clone, Debug)]
pub struct HelmRelease {
    pub name: String,
    pub namespace: String,
    pub revision: i32,
    pub status: String,
    pub chart: String,
    pub app_version: String,
    pub updated: String,
}

impl HelmRelease {
    pub fn health(&self) -> Health {
        match self.status.as_str() {
            "deployed" => Health::Ok,
            "failed" => Health::Bad,
            _ => Health::Warn,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EventInfo {
    pub reason: String,
    pub object: String,
    pub message: String,
    pub count: i32,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cluster: String,
    pub nodes: Vec<NodeInfo>,
    pub pods: Vec<PodInfo>,
    pub warnings: Vec<EventInfo>,
    pub workloads: Vec<WorkloadInfo>,
    pub helm: Vec<HelmRelease>,
    pub rollouts: Vec<crate::ops::RolloutInfo>,
    pub net: crate::ops::NetInfo,
    pub rbac: crate::ops::RbacInfo,
    pub caps: crate::ops::Caps,
    pub apps: Vec<crate::gitops::AppInfo>,
    /// Live usage from metrics-server; None when it is not installed.
    pub metrics: Option<crate::metrics::Metrics>,
    pub infra: crate::infra::Infra,
    pub storage: crate::storage::StorageInfo,
    pub fetched_at: Instant,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn empty(cluster: String) -> Self {
        Self {
            cluster,
            nodes: vec![],
            pods: vec![],
            warnings: vec![],
            workloads: vec![],
            helm: vec![],
            rollouts: vec![],
            net: Default::default(),
            rbac: Default::default(),
            caps: Default::default(),
            apps: vec![],
            metrics: None,
            infra: Default::default(),
            storage: Default::default(),
            fetched_at: Instant::now(),
            error: None,
        }
    }

    pub fn alerts(&self) -> usize {
        self.nodes.iter().filter(|n| n.health() == Health::Bad).count()
            + self.pods.iter().filter(|p| p.health() == Health::Bad).count()
    }

    /// Pods controlled by a workload; Deployments go through their ReplicaSets.
    pub fn pods_of(&self, w: &WorkloadInfo) -> Vec<&PodInfo> {
        let owners: Vec<(&str, &str)> = match w.kind {
            WorkloadKind::Deployment => self
                .workloads
                .iter()
                .filter(|rs| {
                    rs.kind == WorkloadKind::ReplicaSet
                        && rs.namespace == w.namespace
                        && rs.owner.as_ref().is_some_and(|(k, n)| k == "Deployment" && *n == w.name)
                })
                .map(|rs| ("ReplicaSet", rs.name.as_str()))
                .collect(),
            kind => vec![(kind.as_str(), w.name.as_str())],
        };
        let mut pods: Vec<_> = self
            .pods
            .iter()
            .filter(|p| {
                p.namespace == w.namespace
                    && p.owner.as_ref().is_some_and(|(k, n)| owners.contains(&(k.as_str(), n.as_str())))
            })
            .collect();
        pods.sort_by(|a, b| (a.health(), &a.name).cmp(&(b.health(), &b.name)));
        pods
    }

    /// Pods on a node, worst health first.
    pub fn pods_on(&self, node: &str) -> Vec<&PodInfo> {
        let mut pods: Vec<_> = self.pods.iter().filter(|p| p.node.as_deref() == Some(node)).collect();
        pods.sort_by(|a, b| (a.health(), &a.namespace, &a.name).cmp(&(b.health(), &b.namespace, &b.name)));
        pods
    }
}
