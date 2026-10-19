//! Simulated rollouts, traffic and RBAC for `--demo`, with working rollout actions.

use crate::gitops::{AppAction, AppInfo, AppResource};
use crate::ops::*;

struct DemoRollout {
    info: RolloutInfo,
    /// Ticks until the next automatic step.
    timer: u32,
}

struct DemoApp {
    info: AppInfo,
    /// Ticks until the running operation (or self-heal) finishes.
    timer: u32,
    /// Sync of this app keeps failing (broken manifest in Git).
    broken: bool,
}

pub struct OpsSim {
    apps: Vec<DemoApp>,
    cleanup: Vec<Finding>,
    rollouts: Vec<DemoRollout>,
    net: NetInfo,
    rbac: RbacInfo,
}

const CANARY_STEPS: &[&str] =
    &["setWeight 10%", "pause 30s", "setWeight 30%", "analysis success-rate", "setWeight 60%", "pause ∞ (needs promote)", "setWeight 100%"];

fn rev(revision: i64, hash: &str, image: &str, replicas: i32, ready: i32, age: &str, role: Option<&'static str>) -> Revision {
    Revision { revision, name: hash.into(), image: image.into(), replicas, ready, age: age.into(), role }
}

#[allow(clippy::too_many_arguments)]
fn ro(
    kind: RolloutKind,
    ns: &str,
    name: &str,
    strategy: &str,
    desired: i32,
    updated: i32,
    ready: i32,
    phase: Phase,
    image: &str,
) -> RolloutInfo {
    RolloutInfo {
        kind,
        namespace: ns.into(),
        name: name.into(),
        strategy: strategy.into(),
        revision: 1,
        desired,
        updated,
        ready,
        available: ready,
        paused: phase == Phase::Paused,
        phase,
        message: String::new(),
        image: image.into(),
        history: vec![],
        steps: vec![],
        step: None,
        weight: None,
        rollback_window: None,
    }
}

impl OpsSim {
    pub fn new() -> Self {
        use Phase::*;
        use RolloutKind::*;
        let rolling = "RollingUpdate surge 25% unavail 25%";
        let mut rollouts = vec![];

        let mut canary = ro(Argo, "shop", "checkout-api", "Canary via nginx", 4, 1, 4, Progressing, "registry.local/checkout-api:2.8.0");
        canary.revision = 12;
        canary.steps = CANARY_STEPS.iter().map(|s| s.to_string()).collect();
        canary.step = Some(0);
        canary.weight = Some(10);
        canary.history = vec![
            rev(12, "7d9c4f6b58", "registry.local/checkout-api:2.8.0", 1, 1, "40s", Some("canary")),
            rev(11, "5f8b7c9d44", "registry.local/checkout-api:2.7.3", 4, 4, "2d", Some("stable")),
            rev(10, "6c4d8e2f19", "registry.local/checkout-api:2.7.2", 0, 0, "6d", None),
        ];
        rollouts.push(DemoRollout { info: canary, timer: 3 });

        let mut bg = ro(
            Argo,
            "search",
            "search-api",
            "BlueGreen active=search-api preview=search-api-preview",
            2,
            2,
            2,
            Paused,
            "registry.local/search-api:0.9.1",
        );
        bg.revision = 5;
        bg.message = "BlueGreenPause: waiting for promotion of preview".into();
        bg.history = vec![
            rev(5, "84f6c2d1a9", "registry.local/search-api:0.9.1", 2, 2, "12m", Some("preview")),
            rev(4, "3b7e9a5c20", "registry.local/search-api:0.9.0", 2, 2, "3d", Some("active")),
        ];
        rollouts.push(DemoRollout { info: bg, timer: 0 });

        let mut aborted = ro(Argo, "shop", "payments", "Canary (replica weighted)", 2, 0, 2, Aborted, "registry.local/payments:2026.9");
        aborted.revision = 8;
        aborted.steps = vec!["setWeight 25%".into(), "analysis error-rate".into(), "setWeight 100%".into()];
        aborted.step = Some(0);
        aborted.message = "RolloutAborted: metric \"error-rate\" assessed Failed due to failed (3) > failureLimit (2)".into();
        aborted.history = vec![
            rev(8, "c1d2e3f4a5", "registry.local/payments:2026.9", 0, 0, "25m", Some("canary")),
            rev(7, "9a8b7c6d5e", "registry.local/payments:2026.8", 2, 2, "11d", Some("stable")),
        ];
        rollouts.push(DemoRollout { info: aborted, timer: 0 });

        let mut grafana = ro(Deployment, "monitoring", "grafana", rolling, 3, 1, 2, Progressing, "grafana/grafana:12.2.0");
        grafana.revision = 7;
        grafana.message = "ReplicaSet \"grafana-6d8f9c7b5\" is progressing.".into();
        grafana.history = vec![
            rev(7, "6d8f9c7b5", "grafana/grafana:12.2.0", 1, 1, "20s", None),
            rev(6, "58c7d9f4b", "grafana/grafana:12.1.1", 2, 2, "9d", None),
            rev(5, "7f9e4c2a1", "grafana/grafana:12.1.0", 0, 0, "30d", None),
        ];
        rollouts.push(DemoRollout { info: grafana, timer: 2 });

        let mut argocd = ro(Deployment, "argocd", "argocd-server", rolling, 2, 1, 1, Degraded, "quay.io/argoproj/argocd:v3.6.0-rc1");
        argocd.revision = 14;
        argocd.message = "ReplicaSet \"argocd-server-5c9d\" has timed out progressing.".into();
        argocd.history = vec![
            rev(14, "5c9d7f8b6", "quay.io/argoproj/argocd:v3.6.0-rc1", 1, 0, "14m", None),
            rev(13, "4b8c6e7a5", "quay.io/argoproj/argocd:v3.5.3", 1, 1, "21d", None),
        ];
        rollouts.push(DemoRollout { info: argocd, timer: 0 });

        let mut nginx =
            ro(Deployment, "ingress", "ingress-nginx", rolling, 3, 3, 3, Complete, "registry.k8s.io/ingress-nginx/controller:v1.13.2");
        nginx.revision = 9;
        nginx.history = vec![
            rev(9, "79b6d5c8f", "registry.k8s.io/ingress-nginx/controller:v1.13.2", 3, 3, "4d", None),
            rev(8, "6f5c4b3a2", "registry.k8s.io/ingress-nginx/controller:v1.13.1", 0, 0, "33d", None),
            rev(7, "5e4d3c2b1", "registry.k8s.io/ingress-nginx/controller:v1.12.4", 0, 0, "71d", None),
        ];
        rollouts.push(DemoRollout { info: nginx, timer: 0 });

        let mut cilium =
            ro(DaemonSet, "kube-system", "cilium", "RollingUpdate unavail 1", 7, 5, 6, Progressing, "quay.io/cilium/cilium:v1.18.2");
        cilium.revision = 4;
        cilium.history = vec![
            rev(4, "7c9d4b5f8", "quay.io/cilium/cilium:v1.18.2", 5, 5, "3m", None),
            rev(3, "6b8c3a4e7", "quay.io/cilium/cilium:v1.18.1", 2, 2, "19d", None),
            rev(2, "5a7b2f3d6", "quay.io/cilium/cilium:v1.17.6", 0, 0, "62d", None),
        ];
        rollouts.push(DemoRollout { info: cilium, timer: 3 });
        rollouts.push(DemoRollout {
            info: ro(DaemonSet, "monitoring", "node-exporter", "RollingUpdate unavail 1", 7, 7, 7, Complete, "prom/node-exporter:v1.9.1"),
            timer: 0,
        });
        let mut pg = ro(StatefulSet, "shop", "postgres", "RollingUpdate partition 1", 3, 1, 3, Progressing, "postgres:17.6");
        pg.message = "postgres-6b8d9f → postgres-7c9e1a".into();
        pg.revision = 7;
        pg.history = vec![rev(7, "7c9e1a", "postgres:17.6", 1, 1, "4m", None), rev(6, "6b8d9f", "postgres:17.5", 2, 2, "40d", None)];
        rollouts.push(DemoRollout { info: pg, timer: 0 });
        for (ns, name, image) in [
            ("monitoring", "prometheus", "prom/prometheus:v3.6.0"),
            ("monitoring", "loki", "grafana/loki:3.5.5"),
            ("search", "qdrant", "qdrant/qdrant:v1.15.4"),
        ] {
            rollouts.push(DemoRollout { info: ro(StatefulSet, ns, name, "RollingUpdate", 1, 1, 1, Complete, image), timer: 0 });
        }
        for (ns, name, n) in [
            ("kube-system", "coredns", 2),
            ("ci", "docker-builder", 2),
            ("shop", "storefront", 2),
            ("catalog", "catalog-api", 2),
            ("argocd", "argocd-repo-server", 2),
        ] {
            let mut r = ro(Deployment, ns, name, rolling, n, n, n, Complete, &format!("registry.local/{name}:1.4.2"));
            r.revision = 3;
            r.history = vec![
                rev(3, "5d4c3b2a1", &r.image.clone(), n, n, "8d", None),
                rev(2, "4c3b2a190", "registry.local/old:1.4.1", 0, 0, "40d", None),
            ];
            rollouts.push(DemoRollout { info: r, timer: 0 });
        }

        OpsSim { apps: demo_apps(), cleanup: demo_cleanup(), rollouts, net: demo_net(), rbac: demo_rbac() }
    }

    pub fn cleanup(&self) -> Vec<Finding> {
        self.cleanup.clone()
    }

    pub fn delete(&mut self, f: &Finding) {
        self.cleanup.retain(|x| x.key() != f.key());
    }

    pub fn apps(&self) -> Vec<AppInfo> {
        self.apps.iter().map(|a| a.info.clone()).collect()
    }

    pub fn perform_app(&mut self, namespace: &str, name: &str, action: AppAction) -> Result<(), String> {
        let a = self
            .apps
            .iter_mut()
            .find(|a| a.info.namespace == namespace && a.info.name == name)
            .ok_or_else(|| format!("application {namespace}/{name} not found"))?;
        match action {
            AppAction::Sync | AppAction::SyncPrune => {
                if a.info.running() {
                    return Err(format!("an operation is already running on {name}"));
                }
                a.info.op_phase = Some("Running".into());
                a.info.op_message = "waiting for completion of hook and resources".into();
                a.info.op_by = "k8s-galaxy".into();
                a.info.op_when = "0s".into();
                a.timer = 3;
            }
            AppAction::Refresh | AppAction::HardRefresh => {}
        }
        Ok(())
    }

    /// One poll tick: rollouts move on by themselves.
    pub fn step(&mut self) {
        for a in &mut self.apps {
            if a.timer == 0 {
                continue;
            }
            a.timer -= 1;
            if a.timer > 0 {
                continue;
            }
            let app = &mut a.info;
            if a.broken {
                app.op_phase = Some("Failed".into());
                app.op_message = "one or more objects failed to apply, reason: Deployment.apps \"search-api\" is invalid: spec.template.spec.containers[0].image: Required value".into();
                continue;
            }
            app.op_phase = Some("Succeeded".into());
            app.op_message = "successfully synced (all tasks run)".into();
            app.sync = "Synced".into();
            app.revision = "c0ffee1".into();
            for r in &mut app.resources {
                r.sync = "Synced".into();
                if r.health == "Missing" {
                    r.health = "Healthy".into();
                }
            }
            if app.health == "Missing" || app.health == "Progressing" {
                app.health = "Healthy".into();
            }
        }
        for d in &mut self.rollouts {
            let r = &mut d.info;
            if r.paused || matches!(r.phase, Phase::Complete | Phase::Aborted | Phase::Degraded) {
                continue;
            }
            if d.timer > 0 {
                d.timer -= 1;
                continue;
            }
            d.timer = 2;
            match r.kind {
                RolloutKind::Argo if !r.steps.is_empty() => {
                    let next = r.step.map_or(0, |s| s + 1);
                    if next >= r.steps.len() {
                        complete_argo(r);
                        continue;
                    }
                    r.step = Some(next);
                    let step = r.steps[next].clone();
                    if let Some(w) = step.strip_prefix("setWeight ").and_then(|w| w.trim_end_matches('%').parse::<i32>().ok()) {
                        r.weight = Some(w);
                        r.updated = ((r.desired * w) as f64 / 100.0).ceil() as i32;
                        if let Some(c) = r.history.iter_mut().find(|h| h.role == Some("canary")) {
                            (c.replicas, c.ready) = (r.updated, r.updated);
                        }
                    }
                    if step.contains('∞') {
                        r.paused = true;
                        r.phase = Phase::Paused;
                        r.message = "CanaryPauseStep: waiting for promote".into();
                    } else {
                        r.message = format!("step {}/{}: {step}", next + 1, r.steps.len());
                    }
                }
                _ => {
                    r.updated = (r.updated + 1).min(r.desired);
                    r.ready = r.desired.min(r.ready + 1);
                    r.available = r.ready;
                    if let Some(new) = r.history.first_mut() {
                        new.replicas = r.updated;
                        new.ready = r.updated;
                    }
                    if let Some(old) = r.history.get_mut(1) {
                        old.replicas = (r.desired - r.updated).max(0);
                        old.ready = old.replicas;
                    }
                    if r.updated == r.desired && r.ready == r.desired {
                        r.phase = Phase::Complete;
                        r.message = format!("rollout to revision {} complete", r.revision);
                    }
                }
            }
        }
    }

    pub fn rollouts(&self) -> Vec<RolloutInfo> {
        self.rollouts.iter().map(|d| d.info.clone()).collect()
    }

    pub fn net(&self) -> NetInfo {
        self.net.clone()
    }

    pub fn rbac(&self) -> RbacInfo {
        self.rbac.clone()
    }

    pub fn perform(&mut self, a: &Action) -> Result<String, String> {
        let d = self
            .rollouts
            .iter_mut()
            .find(|d| d.info.kind == a.target && d.info.namespace == a.namespace && d.info.name == a.name)
            .ok_or_else(|| format!("{}/{} not found", a.namespace, a.name))?;
        let r = &mut d.info;
        use ActionKind::*;
        match a.kind {
            Restart => {
                r.revision += 1;
                r.history.insert(0, rev(r.revision, "a1b2c3d4e5", &r.image, 0, 0, "0s", None));
                r.history.truncate(4);
                r.updated = 0;
                r.phase = Phase::Progressing;
                r.paused = false;
                r.message = "restart requested".into();
                if r.kind == RolloutKind::Argo && !r.steps.is_empty() {
                    r.step = None;
                    r.weight = Some(0);
                }
                d.timer = 1;
            }
            Pause => {
                r.paused = true;
                r.phase = Phase::Paused;
            }
            Resume => {
                r.paused = false;
                r.phase = Phase::Progressing;
            }
            Undo => {
                let prev = r.undo_target(a.to_revision).cloned().ok_or("no previous revision")?;
                r.revision = r.history.iter().map(|h| h.revision).max().unwrap_or(r.revision) + 1;
                r.image = prev.image.clone();
                r.history.retain(|h| h.revision != prev.revision);
                r.history.insert(0, Revision { revision: r.revision, age: "0s".into(), ..prev });
                r.updated = 0;
                r.phase = Phase::Progressing;
                r.paused = false;
                r.message = "rolling back".into();
                d.timer = 1;
            }
            Promote => {
                if r.strategy.starts_with("BlueGreen") {
                    complete_argo(r);
                } else {
                    r.paused = false;
                    r.phase = Phase::Progressing;
                    d.timer = 0;
                }
            }
            PromoteFull => complete_argo(r),
            Abort => {
                r.phase = Phase::Aborted;
                r.weight = Some(0);
                r.updated = 0;
                r.message = "RolloutAborted: aborted by user".into();
                if let Some(c) = r.history.iter_mut().find(|h| h.role == Some("canary")) {
                    (c.replicas, c.ready) = (0, 0);
                }
            }
            Retry => {
                r.phase = Phase::Progressing;
                r.paused = false;
                r.step = None;
                r.message = "retrying".into();
                d.timer = 1;
            }
        }
        Ok(format!("{} {} {}/{} (demo)", a.kind.label(), a.target.as_str(), a.namespace, a.name))
    }
}

fn complete_argo(r: &mut RolloutInfo) {
    r.phase = Phase::Complete;
    r.paused = false;
    r.updated = r.desired;
    r.ready = r.desired;
    r.available = r.desired;
    r.weight = if r.steps.is_empty() { None } else { Some(100) };
    if !r.steps.is_empty() {
        r.step = Some(r.steps.len() - 1);
    }
    r.message = format!("rollout to revision {} complete", r.revision);
    let current = r.revision;
    for h in &mut r.history {
        if h.revision == current {
            h.role = Some(if r.strategy.starts_with("BlueGreen") { "active" } else { "stable" });
            (h.replicas, h.ready) = (r.desired, r.desired);
        } else {
            h.role = None;
            (h.replicas, h.ready) = (0, 0);
        }
    }
}

fn backend(host: &str, path: &str, svc: &str, port: &str, endpoints: Option<(usize, usize)>) -> Backend {
    Backend { host: host.into(), path: path.into(), service: svc.into(), port: port.into(), endpoints }
}

#[allow(clippy::too_many_arguments)]
fn cert(
    ns: &str,
    name: &str,
    secret: &str,
    dns: &[&str],
    issuer: &str,
    ready: bool,
    days: Option<i64>,
    reason: &str,
    message: &str,
) -> CertInfo {
    CertInfo {
        namespace: ns.into(),
        name: name.into(),
        secret: secret.into(),
        dns_names: dns.iter().map(|d| d.to_string()).collect(),
        issuer: issuer.into(),
        ready,
        reason: reason.into(),
        message: message.into(),
        not_after: days.map(|d| format!("in {d} days")),
        renewal: days.map(|d| format!("in {} days", d - 30)),
        days_left: days,
        challenges: vec![],
        broken_days: if ready { None } else { Some(9) },
    }
}

fn demo_net() -> NetInfo {
    let le = "ClusterIssuer/letsencrypt-prod";
    let mut kibana = cert(
        "monitoring",
        "kibana-tls",
        "kibana-tls",
        &["kibana.example.com"],
        le,
        false,
        None,
        "DoesNotExist",
        "Issuing certificate as Secret does not exist",
    );
    kibana.challenges = vec![
        "http-01 kibana.example.com: pending — Waiting for HTTP-01 challenge propagation: wrong status code '404', expected '200'".into(),
    ];
    let certs = vec![
        cert(
            "argocd",
            "argocd-server-tls",
            "argocd-server-tls",
            &["argocd.example.com"],
            le,
            true,
            Some(61),
            "Ready",
            "Certificate is up to date and has not expired",
        ),
        cert(
            "monitoring",
            "grafana-tls",
            "grafana-tls",
            &["grafana.example.com"],
            le,
            true,
            Some(5),
            "Ready",
            "Certificate is up to date and has not expired",
        ),
        kibana,
        cert(
            "infra",
            "wildcard-example",
            "wildcard-tls",
            &["*.example.com", "example.com"],
            "ClusterIssuer/letsencrypt-dns",
            true,
            Some(74),
            "Ready",
            "Certificate is up to date and has not expired",
        ),
        cert(
            "shop",
            "api-tls",
            "api-tls",
            &["api.example.com"],
            le,
            true,
            Some(33),
            "Ready",
            "Certificate is up to date and has not expired",
        ),
        cert(
            "search",
            "qdrant-internal",
            "qdrant-internal-tls",
            &["qdrant.search.svc"],
            "Issuer/internal-ca",
            true,
            Some(340),
            "Ready",
            "Certificate is up to date and has not expired",
        ),
    ];
    let gateway = GatewayInfo {
        namespace: "infra".into(),
        name: "public-gw".into(),
        class: "cilium".into(),
        listeners: vec![
            Listener {
                name: "https".into(),
                protocol: "HTTPS".into(),
                port: 443,
                hostname: "*.example.com".into(),
                tls_secret: Some("wildcard-tls".into()),
                attached: 2,
            },
            Listener {
                name: "http".into(),
                protocol: "HTTP".into(),
                port: 80,
                hostname: "*.example.com".into(),
                tls_secret: None,
                attached: 1,
            },
            Listener {
                name: "grpc".into(),
                protocol: "HTTPS".into(),
                port: 8443,
                hostname: "grpc.example.com".into(),
                tls_secret: Some("wildcard-tls".into()),
                attached: 1,
            },
        ],
        addresses: vec!["49.13.77.10".into()],
        programmed: true,
        message: "Gateway successfully scheduled".into(),
    };
    let ing = |ns: &str, name: &str, hosts: &[&str], backends: Vec<Backend>, tls: Option<&str>| RouteInfo {
        kind: RouteKind::Ingress,
        namespace: ns.into(),
        name: name.into(),
        class: "nginx".into(),
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
        backends,
        tls: tls.map(|t| vec![(t.to_string(), hosts.iter().map(|h| h.to_string()).collect())]).unwrap_or_default(),
        address: "10.0.0.2,10.0.0.3".into(),
        problems: vec![],
    };
    let routes = vec![
        ing(
            "argocd",
            "argocd-server",
            &["argocd.example.com"],
            vec![backend("argocd.example.com", "/", "argocd-server", "443", Some((1, 2)))],
            Some("argocd-server-tls"),
        ),
        ing(
            "monitoring",
            "grafana",
            &["grafana.example.com"],
            vec![backend("grafana.example.com", "/", "grafana", "3000", Some((3, 3)))],
            Some("grafana-tls"),
        ),
        ing(
            "monitoring",
            "kibana",
            &["kibana.example.com"],
            vec![backend("kibana.example.com", "/", "kibana-kb-http", "5601", None)],
            Some("kibana-tls"),
        ),
        ing(
            "shop",
            "checkout",
            &["api.example.com"],
            vec![
                backend("api.example.com", "/", "checkout-api", "8000", Some((4, 4))),
                backend("api.example.com", "/ws", "checkout-api-ws", "8001", Some((2, 2))),
            ],
            Some("api-tls"),
        ),
        RouteInfo {
            kind: RouteKind::HttpRoute,
            namespace: "shop".into(),
            name: "shop-frontend".into(),
            class: "infra/public-gw:https".into(),
            hosts: vec!["app.example.com".into()],
            backends: vec![
                backend("app.example.com", "/", "storefront", "80 w90", Some((2, 2))),
                backend("app.example.com", "/", "storefront-canary", "80 w10", Some((1, 1))),
            ],
            tls: vec![("wildcard-tls".into(), vec!["*.example.com".into()])],
            address: "49.13.77.10".into(),
            problems: vec![],
        },
        RouteInfo {
            kind: RouteKind::HttpRoute,
            namespace: "catalog".into(),
            name: "catalog".into(),
            class: "infra/public-gw:https".into(),
            hosts: vec!["catalog.example.com".into()],
            backends: vec![
                backend("catalog.example.com", "/api", "catalog-api", "8080", Some((2, 2))),
                backend("catalog.example.com", "/", "catalog-web", "80", None),
            ],
            tls: vec![("wildcard-tls".into(), vec!["*.example.com".into()])],
            address: "49.13.77.10".into(),
            problems: vec!["ResolvedRefs: BackendNotFound — Service \"catalog-web\" not found".into()],
        },
        RouteInfo {
            kind: RouteKind::GrpcRoute,
            namespace: "search".into(),
            name: "qdrant-grpc".into(),
            class: "infra/public-gw:grpc".into(),
            hosts: vec!["grpc.example.com".into()],
            backends: vec![backend("grpc.example.com", "qdrant.Points", "qdrant", "6334", Some((1, 1)))],
            tls: vec![("wildcard-tls".into(), vec!["grpc.example.com".into()])],
            address: "49.13.77.10".into(),
            problems: vec![],
        },
    ];
    let issuer = |cluster: bool, ns: &str, name: &str, kind: &str, ready: bool, message: &str| IssuerInfo {
        cluster,
        namespace: ns.into(),
        name: name.into(),
        kind: kind.into(),
        ready,
        message: message.into(),
    };
    let issuers = vec![
        issuer(
            true,
            "",
            "letsencrypt-prod",
            "acme acme-v02.api.letsencrypt.org [http01]",
            true,
            "The ACME account was registered with the ACME server",
        ),
        issuer(
            true,
            "",
            "letsencrypt-dns",
            "acme acme-v02.api.letsencrypt.org [dns01]",
            true,
            "The ACME account was registered with the ACME server",
        ),
        issuer(
            true,
            "",
            "letsencrypt-staging",
            "acme acme-staging-v02.api.letsencrypt.org [http01]",
            true,
            "The ACME account was registered with the ACME server",
        ),
        issuer(false, "search", "internal-ca", "ca", true, "Signing CA verified"),
    ];
    let mut tls_secrets: std::collections::BTreeSet<(String, String)> =
        certs.iter().filter(|c| c.ready).map(|c| (c.namespace.clone(), c.secret.clone())).collect();
    // Listener certs live in the gateway namespace but are referenced from routes in other namespaces.
    for ns in ["shop", "catalog", "search"] {
        tls_secrets.insert((ns.into(), "wildcard-tls".into()));
    }
    let mut certs = certs;
    for (ns, route) in [("shop", "shop-frontend"), ("catalog", "catalog"), ("search", "qdrant-grpc")] {
        let _ = route;
        let mut c = certs[3].clone();
        c.namespace = ns.into();
        c.name = "wildcard-example (infra)".into();
        certs.push(c);
    }
    NetInfo { routes, gateways: vec![gateway], certs, issuers, tls_secrets }
}

fn rule(groups: &[&str], resources: &[&str], verbs: &[&str]) -> Rule {
    let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
    Rule { api_groups: v(groups), resources: v(resources), verbs: v(verbs), names: vec![], urls: vec![] }
}

fn demo_rbac() -> RbacInfo {
    let role = |cluster: bool, ns: &str, name: &str, rules: Vec<Rule>| RoleInfo {
        cluster,
        namespace: ns.into(),
        name: name.into(),
        rules,
        aggregated: false,
    };
    let subj = |kind: &str, name: &str, ns: &str| Subject { kind: kind.into(), name: name.into(), namespace: ns.into() };
    let binding = |cluster: bool, ns: &str, name: &str, role_cluster: bool, role: &str, subjects: Vec<Subject>| BindingInfo {
        cluster,
        namespace: ns.into(),
        name: name.into(),
        role_cluster,
        role: role.into(),
        subjects,
    };
    let read = ["get", "list", "watch"];
    let roles = vec![
        role(true, "", "cluster-admin", vec![rule(&["*"], &["*"], &["*"])]),
        role(
            true,
            "",
            "admin",
            vec![
                rule(&["", "apps"], &["*"], &["*"]),
                rule(&["rbac.authorization.k8s.io"], &["roles", "rolebindings"], &["create", "update", "patch", "delete"]),
            ],
        ),
        role(
            true,
            "",
            "edit",
            vec![
                rule(
                    &["", "apps", "batch"],
                    &["pods", "deployments", "services", "configmaps", "jobs"],
                    &["get", "list", "watch", "create", "update", "patch", "delete"],
                ),
                rule(&[""], &["pods/exec"], &["create"]),
            ],
        ),
        role(true, "", "view", vec![rule(&["", "apps"], &["pods", "deployments", "services", "configmaps"], &read)]),
        role(true, "", "prometheus", vec![rule(&[""], &["nodes", "nodes/metrics", "services", "endpoints", "pods"], &read)]),
        role(
            true,
            "",
            "argo-rollouts",
            vec![
                rule(&["argoproj.io"], &["rollouts", "rollouts/status", "analysisruns"], &["*"]),
                rule(&["apps"], &["replicasets"], &["get", "list", "watch", "create", "update", "patch", "delete"]),
            ],
        ),
        role(
            true,
            "",
            "cert-manager-controller-certificates",
            vec![
                rule(&["cert-manager.io"], &["certificates", "certificaterequests"], &["*"]),
                rule(&[""], &["secrets"], &["get", "list", "watch", "create", "update", "delete"]),
            ],
        ),
        role(false, "shop", "secrets-reader", vec![rule(&[""], &["secrets"], &["get", "list"])]),
        role(false, "ci", "deployer", vec![rule(&["apps"], &["deployments"], &["get", "patch", "update"])]),
    ];
    let bindings = vec![
        binding(true, "", "cluster-admin", true, "cluster-admin", vec![subj("Group", "system:masters", "")]),
        binding(true, "", "admins", true, "cluster-admin", vec![subj("User", "admin@example.com", "")]),
        binding(true, "", "jenkins-admin", true, "cluster-admin", vec![subj("ServiceAccount", "jenkins", "ci")]),
        binding(
            true,
            "",
            "argocd-application-controller",
            true,
            "cluster-admin",
            vec![subj("ServiceAccount", "argocd-application-controller", "argocd")],
        ),
        binding(true, "", "prometheus", true, "prometheus", vec![subj("ServiceAccount", "prometheus", "monitoring")]),
        binding(true, "", "argo-rollouts", true, "argo-rollouts", vec![subj("ServiceAccount", "argo-rollouts", "argo-rollouts")]),
        binding(
            true,
            "",
            "cert-manager-controller-certificates",
            true,
            "cert-manager-controller-certificates",
            vec![subj("ServiceAccount", "cert-manager", "cert-manager")],
        ),
        binding(true, "", "devs-view", true, "view", vec![subj("Group", "developers", "")]),
        binding(false, "shop", "devs-edit", true, "edit", vec![subj("Group", "developers", ""), subj("User", "alice@example.com", "")]),
        binding(false, "shop", "default-secrets", false, "secrets-reader", vec![subj("ServiceAccount", "default", "shop")]),
        binding(false, "ci", "old-deployer", false, "deployer", vec![subj("ServiceAccount", "deployer", "ci-legacy")]),
        binding(false, "search", "search-admin", true, "admin", vec![subj("User", "alice@example.com", "")]),
        binding(true, "", "system:kube-proxy", true, "system:node-proxier", vec![subj("User", "system:kube-proxy", "")]),
    ];
    let service_accounts = [
        ("ci", "jenkins"),
        ("argocd", "argocd-application-controller"),
        ("monitoring", "prometheus"),
        ("argo-rollouts", "argo-rollouts"),
        ("cert-manager", "cert-manager"),
        ("shop", "default"),
    ]
    .iter()
    .map(|(n, s)| (n.to_string(), s.to_string()))
    .collect();
    RbacInfo { roles, bindings, service_accounts }
}

fn res(kind: &str, ns: &str, name: &str, sync: &str, health: &str) -> AppResource {
    AppResource {
        kind: kind.into(),
        namespace: ns.into(),
        name: name.into(),
        sync: sync.into(),
        health: health.into(),
        message: String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn app(
    project: &str,
    name: &str,
    sync: &str,
    health: &str,
    source: &str,
    dest_ns: &str,
    auto: bool,
    resources: Vec<AppResource>,
) -> AppInfo {
    AppInfo {
        namespace: "argocd".into(),
        name: name.into(),
        project: project.into(),
        sync: sync.into(),
        health: health.into(),
        health_message: String::new(),
        revision: "f804b20".into(),
        sources: vec![source.into()],
        destination: format!("in-cluster → ns {dest_ns}"),
        policy: if auto { "auto (prune, selfHeal)".into() } else { "manual".into() },
        auto,
        self_heal: auto,
        op_phase: Some("Succeeded".into()),
        op_message: "successfully synced (all tasks run)".into(),
        op_by: "automated".into(),
        op_when: "2h".into(),
        resources,
        conditions: vec![],
    }
}

fn demo_apps() -> Vec<DemoApp> {
    let repo = "acme/platform-gitops";
    let ok = |kind: &str, ns: &str, name: &str| {
        res(
            kind,
            ns,
            name,
            "Synced",
            if matches!(kind, "Deployment" | "StatefulSet" | "Service" | "Ingress" | "PersistentVolumeClaim") { "Healthy" } else { "" },
        )
    };
    let mut list = vec![];
    let mut push = |info: AppInfo, timer: u32, broken: bool| list.push(DemoApp { info, timer, broken });

    push(
        app(
            "default",
            "app-of-apps",
            "Synced",
            "Healthy",
            &format!("{repo} kubernetes/argocd@master"),
            "argocd",
            true,
            ["infrastructure", "monitoring", "checkout-api", "storefront", "search", "catalog", "qdrant"]
                .iter()
                .map(|n| ok("Application", "argocd", n))
                .collect(),
        ),
        0,
        false,
    );
    push(
        app(
            "infrastructure",
            "infrastructure",
            "Synced",
            "Healthy",
            &format!("{repo} kubernetes/infrastructure@master"),
            "kube-system",
            true,
            vec![
                ok("Namespace", "", "ingress"),
                ok("Deployment", "ingress", "ingress-nginx"),
                ok("ClusterIssuer", "", "letsencrypt-prod"),
                ok("Deployment", "cert-manager", "cert-manager"),
            ],
        ),
        0,
        false,
    );

    let mut mon = app(
        "infrastructure",
        "monitoring",
        "OutOfSync",
        "Healthy",
        "prometheus-community.github.io chart kube-prometheus-stack@65.1.1",
        "monitoring",
        false,
        vec![
            res("ConfigMap", "monitoring", "grafana-dashboards", "OutOfSync", ""),
            res("PrometheusRule", "monitoring", "node-alerts", "OutOfSync", ""),
            ok("Deployment", "monitoring", "grafana"),
            ok("StatefulSet", "monitoring", "prometheus"),
            ok("Service", "monitoring", "grafana"),
        ],
    );
    mon.sources.push(format!("{repo} ref $values@master"));
    mon.op_when = "6d".into();
    push(mon, 0, false);

    let mut backend = app(
        "backend-apps",
        "checkout-api",
        "Synced",
        "Progressing",
        &format!("{repo} kubernetes/apps/checkout-api@master"),
        "shop",
        true,
        vec![
            ok("Deployment", "shop", "checkout-api"),
            ok("Service", "shop", "checkout-api"),
            res("Rollout", "shop", "checkout-api", "Synced", "Progressing"),
        ],
    );
    backend.op_phase = Some("Running".into());
    backend.op_message = "waiting for healthy state of argoproj.io/Rollout/checkout-api".into();
    backend.op_when = "40s".into();
    push(backend, 4, false);

    let mut frontend = app(
        "frontend-apps",
        "storefront",
        "OutOfSync",
        "Healthy",
        &format!("{repo} kubernetes/apps/storefront@master"),
        "shop",
        true,
        vec![
            res("Deployment", "shop", "storefront", "OutOfSync", "Healthy"),
            ok("Service", "shop", "storefront"),
            ok("Ingress", "shop", "storefront"),
        ],
    );
    frontend.health_message = "replicas changed by hand (kubectl scale) — self-heal will revert".into();
    push(frontend, 9, false);

    let mut argocd = app(
        "default",
        "argocd",
        "Synced",
        "Degraded",
        "argoproj.github.io chart argo-cd@7.6.12",
        "argocd",
        true,
        vec![
            res("Deployment", "argocd", "argocd-server", "Synced", "Degraded"),
            ok("StatefulSet", "argocd", "argocd-application-controller"),
        ],
    );
    argocd.resources[0].message = "Deployment \"argocd-server\" exceeded its progress deadline".into();
    argocd.health_message = "Deployment \"argocd-server\" exceeded its progress deadline".into();
    push(argocd, 0, false);

    let mut search = app(
        "backend-apps",
        "search",
        "OutOfSync",
        "Healthy",
        &format!("{repo} kubernetes/apps/search@master"),
        "search",
        false,
        vec![res("Deployment", "search", "search-api", "OutOfSync", "Healthy"), ok("Service", "search", "search-api")],
    );
    search.op_phase = Some("Failed".into());
    search.op_message = "one or more objects failed to apply, reason: Deployment.apps \"search-api\" is invalid: spec.template.spec.containers[0].image: Required value".into();
    search.op_by = "alice@example.com".into();
    search.op_when = "12m".into();
    search.conditions = vec!["SyncError: Failed sync attempt to 9b1e2c4: one or more objects failed to apply".into()];
    push(search, 0, true);

    let mut voiv = app(
        "backend-apps",
        "catalog",
        "OutOfSync",
        "Missing",
        &format!("{repo} kubernetes/apps/catalog@master"),
        "catalog",
        false,
        vec![
            res("Deployment", "catalog", "catalog-web", "OutOfSync", "Missing"),
            res("Service", "catalog", "catalog-web", "OutOfSync", "Missing"),
        ],
    );
    voiv.op_phase = None;
    voiv.op_message = String::new();
    voiv.op_when = String::new();
    push(voiv, 0, false);

    for (project, name, ns) in [("databases", "qdrant", "search"), ("databases", "postgres", "shop"), ("databases", "redis", "shop")] {
        push(
            app(
                project,
                name,
                "Synced",
                "Healthy",
                &format!("{repo} kubernetes/databases/{name}@master"),
                ns,
                true,
                vec![ok("StatefulSet", ns, name), ok("Service", ns, name), ok("PersistentVolumeClaim", ns, &format!("data-{name}-0"))],
            ),
            0,
            false,
        );
    }
    list
}

use crate::storage::{ClassInfo, DiskUsage, Location, Measured, StorageInfo, Target, VolumeInfo};

const GI: i64 = 1 << 30;

#[allow(clippy::too_many_arguments)]
fn vol(
    ns: Option<&str>,
    claim: Option<&str>,
    pv: Option<&str>,
    size: i64,
    class: &str,
    kind: &str,
    driver: &str,
    detail: &str,
    location: Location,
) -> VolumeInfo {
    VolumeInfo {
        namespace: ns.map(str::to_string),
        claim: claim.map(str::to_string),
        claim_phase: claim.map(|_| "Bound".into()),
        requested: claim.map(|_| size * GI),
        pv: pv.map(str::to_string),
        pv_phase: pv.map(|_| "Bound".into()),
        capacity: pv.map(|_| size * GI),
        reclaim: "Delete".into(),
        class: class.into(),
        access: vec!["ReadWriteOnce".into()],
        kind: kind.into(),
        driver: driver.into(),
        detail: detail.into(),
        location,
        node: None,
        mounted_by: vec![],
        usage: None,
        released_from: None,
    }
}

pub fn demo_storage() -> StorageInfo {
    use Location::*;
    let mut v = vec![];

    let mut pg = vol(
        Some("shop"),
        Some("data-postgres-0"),
        Some("pvc-3f1c9a2e"),
        50,
        "hcloud-volumes",
        "csi",
        "csi.hetzner.cloud",
        "104829331",
        External("Hetzner Cloud Volume".into()),
    );
    pg.mounted_by = vec![("shop/postgres-0".into(), "worker-fsn1-1".into())];
    pg.usage = Some(DiskUsage { used: 41 * GI, capacity: 49 * GI });
    v.push(pg);

    let mut redis = vol(
        Some("shop"),
        Some("redis-data"),
        Some("pvc-77b0d1c4"),
        5,
        "longhorn",
        "csi",
        "driver.longhorn.io",
        "pvc-77b0d1c4",
        InCluster("Longhorn".into()),
    );
    redis.mounted_by = vec![("shop/redis-master-0".into(), "worker-nbg1-1".into())];
    redis.usage = Some(DiskUsage { used: 4 * GI + 900 * (1 << 20), capacity: 5 * GI });
    v.push(redis);

    let mut prom = vol(
        Some("monitoring"),
        Some("prometheus-db"),
        Some("pv-prometheus"),
        100,
        "",
        "hostPath",
        "",
        "/data/prometheus",
        InCluster("node disk (hostPath) on homelab-waw-1".into()),
    );
    prom.node = Some("homelab-waw-1".into());
    prom.reclaim = "Retain".into();
    prom.mounted_by = vec![("monitoring/prometheus-0".into(), "homelab-waw-1".into())];
    v.push(prom);

    let mut loki = vol(
        Some("monitoring"),
        Some("loki-chunks"),
        Some("pvc-0be41f7a"),
        30,
        "ebs-gp3",
        "csi",
        "ebs.csi.aws.com",
        "vol-0a3f9b7c21e4d5f60",
        External("AWS EBS".into()),
    );
    loki.mounted_by = vec![("monitoring/loki-0".into(), "edge-ash-1".into())];
    loki.usage = Some(DiskUsage { used: 12 * GI, capacity: 30 * GI });
    v.push(loki);

    let mut qdrant = vol(
        Some("search"),
        Some("qdrant-storage"),
        Some("pvc-9d2a4e11"),
        20,
        "rook-ceph-block",
        "csi",
        "rook-ceph.rbd.csi.ceph.com",
        "0001-0009-rook-ceph-0000000000000001-9d2a",
        InCluster("Rook Ceph".into()),
    );
    qdrant.mounted_by = vec![("search/qdrant-0".into(), "worker-hel1-1".into())];
    qdrant.usage = Some(DiskUsage { used: 6 * GI, capacity: 20 * GI });
    v.push(qdrant);

    let mut media = vol(
        Some("catalog"),
        Some("media"),
        Some("pv-nas-media"),
        200,
        "",
        "nfs",
        "",
        "192.168.1.20:/volume1/media",
        External("NFS 192.168.1.20".into()),
    );
    media.access = vec!["ReadWriteMany".into()];
    media.reclaim = "Retain".into();
    media.mounted_by =
        vec![("catalog/catalog-api-5080-0".into(), "homelab-waw-1".into()), ("catalog/catalog-api-5080-1".into(), "homelab-waw-2".into())];
    media.usage = Some(DiskUsage { used: 140 * GI, capacity: 200 * GI });
    v.push(media);

    let mut cache = vol(
        Some("ci"),
        Some("docker-cache"),
        Some("pvc-5c1e8b90"),
        80,
        "local-path",
        "hostPath",
        "rancher.io/local-path",
        "/opt/local-path-provisioner/pvc-5c1e8b90_ci_docker-cache",
        InCluster("local-path (node disk) on homelab-waw-2".into()),
    );
    cache.node = Some("homelab-waw-2".into());
    cache.mounted_by = vec![("ci/docker-builder-a192-0".into(), "homelab-waw-2".into())];
    v.push(cache);

    let mut pending = vol(
        Some("shop"),
        Some("payments-export"),
        None,
        20,
        "hcloud-volumes",
        "",
        "csi.hetzner.cloud",
        "",
        External("Hetzner Cloud Volume".into()),
    );
    pending.claim_phase = Some("Pending".into());
    v.push(pending);

    let mut idle = vol(
        Some("search"),
        Some("search-api-tmp"),
        Some("pvc-a14f02c3"),
        2,
        "longhorn",
        "csi",
        "driver.longhorn.io",
        "pvc-a14f02c3",
        InCluster("Longhorn".into()),
    );
    idle.usage = None;
    v.push(idle);

    let mut released = vol(
        None,
        None,
        Some("pv-old-grafana"),
        10,
        "",
        "hostPath",
        "",
        "/data/grafana",
        InCluster("node disk (hostPath) on worker-fsn1-1".into()),
    );
    released.node = Some("worker-fsn1-1".into());
    released.pv_phase = Some("Released".into());
    released.reclaim = "Retain".into();
    released.released_from = Some("monitoring/grafana".into());
    v.push(released);

    let class = |name: &str, prov: &str, default: bool, location: Location| ClassInfo {
        name: name.into(),
        provisioner: prov.into(),
        default,
        reclaim: "Delete".into(),
        binding: "WaitForFirstConsumer".into(),
        expansion: true,
        location,
    };
    let classes = vec![
        class("hcloud-volumes", "csi.hetzner.cloud", true, External("Hetzner Cloud Volume".into())),
        class("longhorn", "driver.longhorn.io", false, InCluster("Longhorn".into())),
        class("local-path", "rancher.io/local-path", false, InCluster("local-path (node disk)".into())),
        class("rook-ceph-block", "rook-ceph.rbd.csi.ceph.com", false, InCluster("Rook Ceph".into())),
        class("ebs-gp3", "ebs.csi.aws.com", false, External("AWS EBS".into())),
    ];
    let node_fs = [
        ("homelab-waw-1", 410, 480),
        ("homelab-waw-2", 120, 480),
        ("worker-fsn1-1", 31, 160),
        ("worker-nbg1-1", 22, 160),
        ("worker-hel1-1", 18, 160),
        ("cp-fsn1-1", 14, 80),
        ("edge-ash-1", 9, 80),
    ]
    .iter()
    .map(|(n, u, c)| (n.to_string(), DiskUsage { used: u * GI, capacity: c * GI }))
    .collect();
    StorageInfo { volumes: v, classes, node_fs }
}

/// Simulated `du`/`df` on the node.
pub fn demo_measure(targets: &[Target]) -> crate::storage::Measurements {
    targets
        .iter()
        .map(|t| {
            let res = match t.pv.as_str() {
                "pv-prometheus" => Ok(Measured { used: 88 * GI, device: "/dev/nvme0n1p2".into(), fs_size: 480 * GI, fs_used: 410 * GI }),
                "pv-old-grafana" => Ok(Measured { used: 3 * GI, device: "/dev/sda1".into(), fs_size: 160 * GI, fs_used: 31 * GI }),
                _ => Ok(Measured { used: 23 * GI, device: "/dev/nvme0n1p2".into(), fs_size: 480 * GI, fs_used: 120 * GI }),
            };
            (t.pv.clone(), res)
        })
        .collect()
}

use crate::cleanup::Finding;
use crate::model::Health;

pub fn demo_cleanup() -> Vec<Finding> {
    let f =
        |kind, ns: &str, name: &str, category, severity, reason: &str, age: i64, deletable: bool, notes: Vec<(Health, String)>| Finding {
            kind,
            namespace: ns.into(),
            name: name.into(),
            category,
            severity,
            reason: reason.into(),
            age_days: Some(age),
            deletable,
            notes,
        };
    vec![
        f(
            "PersistentVolume",
            "",
            "pv-old-grafana",
            "Released volumes",
            Health::Warn,
            "claim monitoring/grafana was deleted; Retain kept the volume",
            41,
            true,
            vec![(
                Health::Bad,
                "deleting the PV does not delete the data: remove /data/grafana on worker-fsn1-1 by hand if it is not needed".into(),
            )],
        ),
        f(
            "PersistentVolumeClaim",
            "search",
            "search-api-tmp",
            "Unmounted claims",
            Health::Ok,
            "bound but no pod mounts it",
            12,
            true,
            vec![(Health::Bad, "reclaim policy Delete: deleting the claim DESTROYS the volume and its data".into())],
        ),
        f("Pod", "ci", "docker-builder-a192-7", "Finished pods", Health::Ok, "Evicted, not owned by a Job", 3, true, vec![]),
        f("Pod", "shop", "migrate-db-x81k", "Finished pods", Health::Ok, "Succeeded, not owned by a Job", 19, true, vec![]),
        f(
            "Job",
            "catalog",
            "reindex-2026-09",
            "Old jobs",
            Health::Ok,
            "completed 33 days ago, no ttlSecondsAfterFinished",
            33,
            true,
            vec![(Health::Ok, "its pods are deleted with it".into())],
        ),
        f(
            "Ingress",
            "monitoring",
            "cm-acme-http-solver-7xq2p",
            "ACME solver leftovers",
            Health::Warn,
            "challenge for kibana.example.com is still pending — the certificate is failing, fix that first",
            9,
            false,
            vec![],
        ),
        f(
            "Ingress",
            "shop",
            "cm-acme-http-solver-k2v9d",
            "ACME solver leftovers",
            Health::Ok,
            "no challenge for shop.example.com is active any more",
            120,
            true,
            vec![],
        ),
        f(
            "Ingress",
            "monitoring",
            "kibana",
            "Broken routes",
            Health::Warn,
            "kibana.example.com → service kibana-kb-http does not exist",
            300,
            true,
            vec![(Health::Warn, "if the app was removed, the Ingress (and its Certificate) can go too".into())],
        ),
        f(
            "Certificate",
            "monitoring",
            "kibana-tls",
            "Failing certificates",
            Health::Warn,
            "not ready for 9 days (DoesNotExist): kibana.example.com",
            9,
            true,
            vec![(Health::Ok, "its Secret kibana-tls is left in place; delete it too if the host is gone".into())],
        ),
        f(
            "ConfigMap",
            "shop",
            "checkout-api-config-old",
            "Possibly unused config",
            Health::Ok,
            "no pod mounts or reads it",
            64,
            true,
            vec![(Health::Warn, "controllers and apps can read ConfigMaps through the API — check before deleting".into())],
        ),
        f(
            "Secret",
            "catalog",
            "legacy-s3-credentials",
            "Possibly unused config",
            Health::Ok,
            "no pod mounts or reads it, no Ingress uses it",
            210,
            true,
            vec![(Health::Warn, "operators, Argo CD and registries read Secrets through the API — check before deleting".into())],
        ),
        f(
            "Service",
            "shop",
            "payments-legacy",
            "Services without pods",
            Health::Warn,
            "selector app=payments-legacy matches no pod",
            88,
            true,
            vec![],
        ),
        f(
            "Namespace",
            "",
            "sandbox-old",
            "Empty namespaces",
            Health::Ok,
            "no pods, services or volumes",
            150,
            false,
            vec![(Health::Ok, "not deletable from here: deleting a namespace deletes everything in it".into())],
        ),
    ]
}

/// A Hetzner bill for the demo nodes (ids match their `hcloud://` providerIDs).
pub fn demo_costs() -> Vec<crate::costs::Costs> {
    use crate::costs::{Costs, Item};
    let up: k8s_openapi::jiff::Timestamp =
        k8s_openapi::jiff::Timestamp::from_second(k8s_openapi::jiff::Timestamp::now().as_second() - 3 * 86_400 - 5 * 3600).unwrap();
    let server = |name: &str, ty: &str, loc: &str, h: f64, m: f64| Item {
        kind: "server",
        name: name.into(),
        detail: ty.into(),
        location: loc.into(),
        hourly: h,
        monthly: m,
        id: (40_000_000 + crate::fx::hash(name) % 9_000_000).to_string(),
        created: Some(up),
        running: true,
    };
    let other = |kind, name: &str, detail: &str, m: f64| Item {
        kind,
        name: name.into(),
        detail: detail.into(),
        location: "fsn1".into(),
        hourly: 0.0,
        monthly: m,
        id: String::new(),
        created: None,
        running: true,
    };
    let hetzner = Costs {
        provider: "Hetzner Cloud".into(),
        notes: vec![],
        currency: "EUR".into(),
        items: vec![
            server("cp-fsn1-1", "cx32", "fsn1", 0.0112, 6.80),
            server("worker-fsn1-1", "cx42", "fsn1", 0.0302, 18.85),
            server("worker-hel1-1", "cx42", "hel1", 0.0302, 18.85),
            server("worker-nbg1-1", "ccx23", "nbg1", 0.0446, 27.83),
            other("volume", "pvc-3f1c9a2e", "50 GB", 2.20),
            other("snapshot", "k8s-cluster-000 2026-10-01", "61.4 GB", 0.73),
            other("primary ip", "cp-fsn1-1", "49.13.77.21", 0.60),
            other("floating ip", "public", "49.13.77.10", 3.57),
            other("load balancer", "public-gw", "lb11", 6.41),
        ],
    };
    // The edge node in Ashburn is an EC2 instance with an EBS volume.
    let aws = Costs {
        provider: "AWS".into(),
        currency: "USD".into(),
        notes: vec![],
        items: vec![
            Item {
                kind: "server",
                name: "edge-ash-1".into(),
                detail: "m6i.large".into(),
                location: "us-east-1".into(),
                hourly: 0.096,
                monthly: 0.096 * 730.0,
                id: "edge-ash-1".into(),
                created: None,
                running: true,
            },
            Item {
                kind: "volume",
                name: "loki-chunks".into(),
                detail: "32 GB ebs-gp3".into(),
                location: "us-east-1".into(),
                hourly: 0.0,
                monthly: 32.0 * 0.08,
                id: String::new(),
                created: None,
                running: true,
            },
        ],
    };
    vec![hetzner, aws]
}

use crate::jobs::{CronJobInfo, JobInfo};

fn ago_ts(secs: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() - secs).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn job(
    ns: &str,
    name: &str,
    cron: Option<&str>,
    status: &str,
    done: i32,
    of: i32,
    started: i64,
    took: Option<i64>,
    message: &str,
) -> JobInfo {
    JobInfo {
        namespace: ns.into(),
        name: name.into(),
        cronjob: cron.map(str::to_string),
        status: status.into(),
        active: if status == "Running" { 1 } else { 0 },
        succeeded: done,
        failed: if status == "Failed" { 6 } else { 0 },
        completions: of,
        started: Some(ago_ts(started)),
        finished: took.map(|t| ago_ts(started - t)),
        message: message.into(),
    }
}

pub struct JobSim {
    pub jobs: Vec<JobInfo>,
    pub crons: Vec<(String, String, String, Option<String>, bool, i64)>, // ns, name, schedule, tz, suspend, last run (s ago)
}

impl JobSim {
    pub fn new() -> Self {
        let jobs = vec![
            job("shop", "backup-postgres-29353500", Some("backup-postgres"), "Complete", 1, 1, 9 * 3600, Some(212), ""),
            job("shop", "backup-postgres-29352060", Some("backup-postgres"), "Complete", 1, 1, 33 * 3600, Some(198), ""),
            job(
                "catalog",
                "reindex-29354010",
                Some("reindex"),
                "Failed",
                0,
                1,
                1500,
                Some(410),
                "BackoffLimitExceeded: Job has reached the specified backoff limit",
            ),
            job("catalog", "reindex-29353980", Some("reindex"), "Complete", 1, 1, 3300, Some(380), ""),
            job("shop", "migrate-db-0ae9db8", None, "Complete", 1, 1, 6 * 86_400, Some(41), ""),
            job(
                "ci",
                "seed-test-data",
                None,
                "Failed",
                0,
                1,
                2 * 86_400,
                Some(95),
                "DeadlineExceeded: Job was active longer than specified deadline",
            ),
            job("search", "embeddings-backfill", None, "Running", 3, 8, 1800, None, ""),
        ];
        let crons = vec![
            ("shop".into(), "backup-postgres".into(), "0 3 * * *".into(), Some("Europe/Warsaw".into()), false, 9 * 3600),
            ("catalog".into(), "reindex".into(), "*/30 * * * *".into(), None, false, 1500),
            ("monitoring".into(), "cert-check".into(), "@hourly".into(), None, true, 5 * 86_400),
            ("ci".into(), "cleanup-images".into(), "0 4 * * 0".into(), None, false, 23 * 86_400),
        ];
        JobSim { jobs, crons }
    }

    pub fn cronjobs(&self) -> Vec<CronJobInfo> {
        self.crons
            .iter()
            .map(|(ns, name, schedule, tz, suspend, last)| {
                let newest =
                    self.jobs.iter().filter(|j| &j.namespace == ns && j.cronjob.as_deref() == Some(name)).max_by_key(|j| j.started);
                let mut c = CronJobInfo {
                    namespace: ns.clone(),
                    name: name.clone(),
                    schedule: schedule.clone(),
                    time_zone: tz.clone(),
                    suspend: *suspend,
                    active: self.jobs.iter().filter(|j| j.cronjob.as_deref() == Some(name) && j.status == "Running").count(),
                    last_schedule: Some(ago_ts(*last)),
                    last_success: self
                        .jobs
                        .iter()
                        .filter(|j| j.cronjob.as_deref() == Some(name) && j.status == "Complete")
                        .filter_map(|j| j.finished)
                        .max()
                        .or(Some(ago_ts(*last))),
                    last_status: newest.map(|j| j.status.clone()),
                    next: None,
                };
                if !c.suspend {
                    c.next = crate::jobs::Cron::parse(&c.schedule).ok().and_then(|cr| cr.next_after(jiff::Timestamp::now(), &c.zone()));
                }
                c
            })
            .collect()
    }

    pub fn step(&mut self) {
        // Manual runs finish after a little while.
        for j in self.jobs.iter_mut().filter(|j| j.status == "Running" && j.name.contains("-manual-")) {
            if j.duration_secs().unwrap_or(0) > 6 {
                j.status = "Complete".into();
                j.succeeded = j.completions;
                j.active = 0;
                j.finished = Some(jiff::Timestamp::now());
            }
        }
    }

    pub fn run_now(&mut self, ns: &str, cron: &str) -> Result<String, String> {
        let name = format!("{cron}-manual-{}", jiff::Timestamp::now().as_second() % 1_000_000);
        self.jobs.push(job(ns, &name, Some(cron), "Running", 0, 1, 0, None, ""));
        if let Some(c) = self.crons.iter_mut().find(|c| c.0 == ns && c.1 == cron) {
            c.5 = 0;
        }
        Ok(name)
    }

    pub fn suspend(&mut self, ns: &str, cron: &str, on: bool) {
        if let Some(c) = self.crons.iter_mut().find(|c| c.0 == ns && c.1 == cron) {
            c.4 = on;
        }
    }

    pub fn delete(&mut self, ns: &str, name: &str) {
        self.jobs.retain(|j| !(j.namespace == ns && j.name == name));
    }
}
