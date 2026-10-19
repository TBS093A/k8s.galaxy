//! Fake but lively cluster for `--demo`: nodes across Hetzner regions plus a homelab,
//! pods that crash and recover, a flaky node.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::fx::Rng;
use crate::geo::LOCATION_LABEL;
use crate::metrics::{Metrics, Usage};
use crate::model::{EventInfo, HelmRelease, NodeInfo, PodInfo, Snapshot, WorkloadInfo, WorkloadKind};

const STATEFUL: &[&str] = &["prometheus", "loki", "postgres", "qdrant", "jenkins"];

fn kind_of(app: &str, replicas: usize, nodes: usize) -> WorkloadKind {
    if replicas == nodes {
        WorkloadKind::DaemonSet
    } else if STATEFUL.contains(&app) {
        WorkloadKind::StatefulSet
    } else {
        WorkloadKind::Deployment
    }
}

/// Deployments own pods through a ReplicaSet with a hash suffix.
fn owner_name(app: &str, kind: WorkloadKind) -> (String, String) {
    match kind {
        WorkloadKind::Deployment => ("ReplicaSet".into(), format!("{app}-{:x}", crate::fx::hash(app) & 0xfffffff)),
        k => (k.as_str().into(), app.into()),
    }
}

struct DemoNode {
    name: &'static str,
    location: &'static str,
    zone: &'static str,
    control_plane: bool,
    ready: bool,
}

pub struct Sim {
    rng: Rng,
    nodes: Vec<DemoNode>,
    pods: Vec<PodInfo>,
    warnings: Vec<EventInfo>,
    ops: crate::demo_ops::OpsSim,
    /// Per-pod load factor (usage / request), random-walked each step.
    load: BTreeMap<String, f64>,
}

/// Container requests by app: heavy hitters get more, the rest a stable pseudo-random size.
fn requests_of(app: &str) -> Usage {
    let (cpu_m, mi) = match app {
        "prometheus" => (500, 2048),
        "loki" => (250, 1024),
        "qdrant" => (1000, 4096),
        "checkout-api" => (500, 1024),
        "jenkins" | "docker-builder" => (750, 2048),
        "cilium" => (100, 512),
        _ => (50 + (crate::fx::hash(app) % 450) as i64, 64 + (crate::fx::hash(app) % 960) as i64),
    };
    Usage { cpu_m, mem: mi * 1024 * 1024 }
}

const APPS: &[(&str, &str, usize)] = &[
    ("kube-system", "coredns", 2),
    ("kube-system", "kube-proxy", 7),
    ("kube-system", "cilium", 7),
    ("monitoring", "prometheus", 1),
    ("monitoring", "grafana", 1),
    ("monitoring", "loki", 2),
    ("monitoring", "node-exporter", 7),
    ("argocd", "argocd-server", 1),
    ("argocd", "argocd-repo-server", 2),
    ("ingress", "ingress-nginx", 3),
    ("shop", "checkout-api", 3),
    ("shop", "storefront", 2),
    ("shop", "payments", 2),
    ("catalog", "catalog-api", 2),
    ("catalog", "postgres", 1),
    ("search", "search-api", 2),
    ("search", "qdrant", 1),
    ("ci", "jenkins", 1),
    ("ci", "docker-builder", 2),
];

impl Sim {
    pub fn new() -> Self {
        let nodes = vec![
            DemoNode { name: "cp-fsn1-1", location: "fsn1", zone: "fsn1-dc14", control_plane: true, ready: true },
            DemoNode { name: "worker-fsn1-1", location: "fsn1", zone: "fsn1-dc14", control_plane: false, ready: true },
            DemoNode { name: "worker-nbg1-1", location: "nbg1", zone: "nbg1-dc3", control_plane: false, ready: true },
            DemoNode { name: "worker-hel1-1", location: "hel1", zone: "hel1-dc2", control_plane: false, ready: true },
            DemoNode { name: "edge-ash-1", location: "ash", zone: "ash-dc1", control_plane: false, ready: true },
            DemoNode { name: "homelab-waw-1", location: "waw", zone: "", control_plane: false, ready: true },
            DemoNode { name: "homelab-waw-2", location: "waw", zone: "", control_plane: false, ready: true },
        ];
        let mut rng = Rng::new(0x6a1a_c7c5);
        let mut pods = vec![];
        for &(ns, app, replicas) in APPS {
            for i in 0..replicas {
                let node = if replicas == nodes.len() { nodes[i].name } else { rng.pick(&nodes[1..]).name };
                let suffix = format!("{:05x}", rng.next_u64() & 0xfffff);
                pods.push(PodInfo {
                    name: if replicas == nodes.len() { format!("{app}-{}", &suffix[..5]) } else { format!("{app}-{}-{i}", &suffix[..4]) },
                    namespace: ns.into(),
                    node: Some(node.into()),
                    phase: "Running".into(),
                    reason: None,
                    restarts: 0,
                    ready: true,
                    containers: vec![app.into()],
                    owner: Some(owner_name(app, kind_of(app, replicas, nodes.len()))),
                    requests: requests_of(app),
                });
            }
        }
        let load = pods.iter().map(|p| (p.name.clone(), 0.3 + crate::fx::unit(crate::fx::hash(&p.name)) * 0.7)).collect();
        Self { rng, nodes, pods, warnings: vec![], ops: crate::demo_ops::OpsSim::new(), load }
    }

    pub fn perform(&mut self, action: &crate::ops::Action) -> Result<String, String> {
        self.ops.perform(action)
    }

    pub fn perform_app(&mut self, namespace: &str, name: &str, action: crate::gitops::AppAction) -> Result<(), String> {
        self.ops.perform_app(namespace, name, action)
    }

    pub fn measure(&self, targets: &[crate::storage::Target]) -> crate::storage::Measurements {
        crate::demo_ops::demo_measure(targets)
    }

    pub fn step(&mut self) {
        self.ops.step();
        for v in self.load.values_mut() {
            *v = (*v + (self.rng.f64() - 0.5) * 0.25).clamp(0.05, 1.8);
        }
        let rng = &mut self.rng;
        if rng.chance(0.35) {
            let i = (rng.next_u64() % self.pods.len() as u64) as usize;
            let reason = *rng.pick(&["CrashLoopBackOff", "OOMKilled", "ImagePullBackOff"]);
            let p = &mut self.pods[i];
            if p.reason.is_none() {
                p.reason = Some(reason.into());
                p.ready = false;
                p.restarts += 1;
                self.warnings.insert(
                    0,
                    EventInfo {
                        reason: "BackOff".into(),
                        object: format!("pod/{}", p.name),
                        message: format!("Back-off restarting failed container {} ({reason})", p.containers[0]),
                        count: p.restarts,
                    },
                );
            }
        }
        for p in self.pods.iter_mut().filter(|p| p.reason.is_some()) {
            if rng.chance(0.2) {
                p.reason = None;
                p.ready = true;
            }
        }
        if rng.chance(0.06) {
            let n = &mut self.nodes[3];
            n.ready = !n.ready;
            if !n.ready {
                self.warnings.insert(
                    0,
                    EventInfo {
                        reason: "NodeNotReady".into(),
                        object: format!("node/{}", n.name),
                        message: "Kubelet stopped posting node status".into(),
                        count: 1,
                    },
                );
            }
        }
        self.warnings.truncate(50);
    }

    pub fn snapshot(&self) -> Snapshot {
        let nodes = self
            .nodes
            .iter()
            .map(|n| {
                let mut labels = BTreeMap::new();
                labels.insert(LOCATION_LABEL.to_string(), n.location.to_string());
                if !n.zone.is_empty() {
                    labels.insert("topology.kubernetes.io/zone".into(), n.zone.into());
                }
                NodeInfo {
                    name: n.name.into(),
                    ready: n.ready,
                    roles: vec![if n.control_plane { "control-plane" } else { "worker" }.into()],
                    labels,
                    annotations: BTreeMap::new(),
                    region: n.location.into(),
                    zone: n.zone.into(),
                    cpu: if n.control_plane { "4" } else { "8" }.into(),
                    memory: if n.control_plane { "8Gi" } else { "32Gi" }.into(),
                    allocatable: if n.control_plane {
                        Usage { cpu_m: 3800, mem: 7 << 30 }
                    } else if n.name == "homelab-waw-1" {
                        // A small box, so the demo has a node running hot.
                        Usage { cpu_m: 2000, mem: 6 << 30 }
                    } else {
                        Usage { cpu_m: 7800, mem: 30 << 30 }
                    },
                    kubelet: "v1.34.1".into(),
                    os_image: if n.location == "waw" { "Debian GNU/Linux 12 (bookworm)" } else { "Ubuntu 24.04.1 LTS" }.into(),
                    internal_ip: format!("10.0.0.{}", 10 + crate::fx::hash(n.name) % 200),
                    provider_id: match n.location {
                        "ash" => "aws:///us-east-1a/i-0e4b9c2d7f1a83b56".into(),
                        "waw" => String::new(),
                        _ => format!("hcloud://{}", 40_000_000 + crate::fx::hash(n.name) % 9_000_000),
                    },
                    addresses: vec![],
                    cloud: None,
                    geo: None,
                    place: String::new(),
                }
            })
            .collect();
        Snapshot {
            cluster: "demo-galaxy".into(),
            nodes,
            pods: self.pods.clone(),
            warnings: self.warnings.clone(),
            workloads: self.workloads(),
            rollouts: self.ops.rollouts(),
            net: self.ops.net(),
            rbac: self.ops.rbac(),
            caps: crate::ops::Caps { argo_rollouts: true, gateway_api: true, cert_manager: true, argocd: true, metrics: true },
            apps: self.ops.apps(),
            metrics: Some(self.metrics()),
            storage: crate::demo_ops::demo_storage(),
            infra: Default::default(),
            helm: self.helm(),
            fetched_at: Instant::now(),
            error: None,
        }
    }

    fn metrics(&self) -> Metrics {
        let mut m = Metrics::default();
        for p in &self.pods {
            let k = if p.ready { self.load.get(&p.name).copied().unwrap_or(0.5) } else { 0.02 };
            let u = Usage { cpu_m: (p.requests.cpu_m as f64 * k) as i64, mem: (p.requests.mem as f64 * (0.5 + k * 0.4)) as i64 };
            m.pods.insert((p.namespace.clone(), p.name.clone()), u);
            if let Some(node) = &p.node {
                // Kubelet, runtime and OS on top of the pods.
                let e = m.nodes.entry(node.clone()).or_insert(Usage { cpu_m: 180, mem: 900 << 20 });
                *e += u;
            }
        }
        // Real nodes cannot hand out more than they have.
        for n in &self.nodes {
            if let Some(u) = m.nodes.get_mut(n.name) {
                let alloc = if n.control_plane {
                    (3800, 7i64 << 30)
                } else if n.name == "homelab-waw-1" {
                    (2000, 6 << 30)
                } else {
                    (7800, 30 << 30)
                };
                u.cpu_m = u.cpu_m.min(alloc.0 * 97 / 100);
                u.mem = u.mem.min(alloc.1 * 96 / 100);
            }
        }
        m
    }

    fn workloads(&self) -> Vec<WorkloadInfo> {
        let mut out = vec![];
        for &(ns, app, replicas) in APPS {
            let kind = kind_of(app, replicas, self.nodes.len());
            let (owner_kind, owner) = owner_name(app, kind);
            let ready =
                self.pods.iter().filter(|p| p.ready && p.owner.as_ref().is_some_and(|(k, n)| *k == owner_kind && *n == owner)).count()
                    as i32;
            let base = WorkloadInfo {
                kind,
                name: app.into(),
                namespace: ns.into(),
                desired: replicas as i32,
                ready,
                image: format!("registry.local/{app}:1.{}", crate::fx::hash(app) % 9),
                owner: None,
            };
            if kind == WorkloadKind::Deployment {
                out.push(WorkloadInfo {
                    kind: WorkloadKind::ReplicaSet,
                    name: owner,
                    owner: Some(("Deployment".into(), app.into())),
                    ..base.clone()
                });
                // A scaled-down ReplicaSet from the previous rollout.
                out.push(WorkloadInfo {
                    kind: WorkloadKind::ReplicaSet,
                    name: format!("{app}-{:x}", crate::fx::hash(ns) & 0xfffffff),
                    desired: 0,
                    ready: 0,
                    owner: Some(("Deployment".into(), app.into())),
                    ..base.clone()
                });
            }
            out.push(base);
        }
        out
    }

    fn helm(&self) -> Vec<HelmRelease> {
        let failing = self.pods.iter().any(|p| p.namespace == "ingress" && p.reason.is_some());
        [
            ("argocd", "argo-cd", "7.6.12", "v2.12.6", 14),
            ("ingress", "ingress-nginx", "4.11.3", "1.11.3", 9),
            ("monitoring", "kube-prometheus-stack", "65.1.1", "v0.77.1", 22),
            ("monitoring", "loki", "6.18.0", "3.2.0", 5),
            ("kube-system", "cilium", "1.16.3", "1.16.3", 3),
            ("shop", "checkout-api", "0.4.2", "0ae9db8", 31),
            ("search", "qdrant", "1.12.1", "v1.12.1", 2),
        ]
        .iter()
        .map(|&(ns, chart, version, app_version, revision)| HelmRelease {
            name: if chart == "kube-prometheus-stack" { "monitoring".into() } else { chart.into() },
            namespace: ns.into(),
            revision,
            status: if ns == "ingress" && failing { "failed".into() } else { "deployed".into() },
            chart: format!("{chart}-{version}"),
            app_version: app_version.into(),
            updated: "2026-10-18 21:04:11".into(),
        })
        .collect()
    }

    pub fn logs(&self, pod: &str) -> Vec<String> {
        let mut rng = Rng::new(crate::fx::hash(pod));
        let msgs = [
            "INFO  request handled path=/api/v1/health status=200",
            "INFO  connected to postgres pool=8",
            "DEBUG cache hit ratio=0.93",
            "WARN  slow query took=1.2s table=orders",
            "INFO  reconciled 14 objects",
            "ERROR upstream timeout service=search-api",
        ];
        (0..120).map(|i| format!("2026-10-19T12:{:02}:{:02}Z {}", i / 60, i % 60, rng.pick(&msgs))).collect()
    }
}
