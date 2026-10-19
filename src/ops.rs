//! Operations data: rollouts (native + Argo Rollouts), traffic (Ingress, Gateway API, cert-manager)
//! and RBAC — plus the rollout actions (restart, pause, undo, promote, abort...).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use k8s_openapi::api::apps::v1::{ControllerRevision, DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::core::v1::{Secret, Service, ServiceAccount};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use k8s_openapi::jiff::Timestamp;
use kube::api::{DynamicObject, ListParams, Patch, PatchParams};
use kube::discovery::ApiResource;
use kube::{Api, Client};
use serde_json::{Value, json};

use crate::model::Health;

// ───────────────────────────── model ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RolloutKind {
    Deployment,
    StatefulSet,
    DaemonSet,
    Argo,
}

impl RolloutKind {
    pub fn short(self) -> &'static str {
        match self {
            RolloutKind::Deployment => "deploy",
            RolloutKind::StatefulSet => "sts",
            RolloutKind::DaemonSet => "ds",
            RolloutKind::Argo => "argo",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RolloutKind::Deployment => "Deployment",
            RolloutKind::StatefulSet => "StatefulSet",
            RolloutKind::DaemonSet => "DaemonSet",
            RolloutKind::Argo => "Rollout",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Complete,
    Progressing,
    Paused,
    Degraded,
    Aborted,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Complete => "Complete",
            Phase::Progressing => "Progressing",
            Phase::Paused => "Paused",
            Phase::Degraded => "Degraded",
            Phase::Aborted => "Aborted",
        }
    }

    pub fn health(self) -> Health {
        match self {
            Phase::Complete => Health::Ok,
            Phase::Progressing | Phase::Paused => Health::Warn,
            Phase::Degraded | Phase::Aborted => Health::Bad,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Revision {
    pub revision: i64,
    pub name: String,
    pub image: String,
    pub replicas: i32,
    pub ready: i32,
    pub age: String,
    /// Argo: "stable" / "canary" / "active" / "preview".
    pub role: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct RolloutInfo {
    pub kind: RolloutKind,
    pub namespace: String,
    pub name: String,
    /// "RollingUpdate 25%/25%", "Recreate", "Canary", "BlueGreen", "OnDelete".
    pub strategy: String,
    pub revision: i64,
    pub desired: i32,
    pub updated: i32,
    pub ready: i32,
    pub available: i32,
    pub paused: bool,
    pub phase: Phase,
    pub message: String,
    pub image: String,
    pub history: Vec<Revision>,
    /// Argo canary steps, rendered ("setWeight 20", "pause 30s", "pause ∞", "analysis").
    pub steps: Vec<String>,
    pub step: Option<usize>,
    pub weight: Option<i32>,
    /// Argo Rollouts `spec.rollbackWindow.revisions`: rollbacks within it skip the steps.
    pub rollback_window: Option<i64>,
}

impl RolloutInfo {
    /// Revision an undo goes to: the requested one, else the newest older than the current.
    pub fn undo_target(&self, to: Option<i64>) -> Option<&Revision> {
        match to {
            Some(t) => self.history.iter().find(|h| h.revision == t),
            None => self.history.iter().filter(|h| h.revision < self.revision).max_by_key(|h| h.revision),
        }
    }

    /// Revisions an undo can go to (everything but the current one), newest first.
    pub fn undo_candidates(&self) -> Vec<&Revision> {
        let mut v: Vec<&Revision> = self.history.iter().filter(|h| h.revision != self.revision).collect();
        v.sort_by_key(|h| std::cmp::Reverse(h.revision));
        v
    }

    pub fn health(&self) -> Health {
        self.phase.health()
    }

    pub fn actions(&self) -> Vec<ActionKind> {
        use ActionKind::*;
        match self.kind {
            RolloutKind::Deployment => vec![Restart, if self.paused { Resume } else { Pause }, Undo],
            RolloutKind::StatefulSet | RolloutKind::DaemonSet => vec![Restart, Undo],
            RolloutKind::Argo => {
                let mut a = vec![Restart, if self.paused { Resume } else { Pause }, Promote, PromoteFull];
                a.push(if self.phase == Phase::Aborted { Retry } else { Abort });
                a.push(Undo);
                a
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Restart,
    Pause,
    Resume,
    Undo,
    Promote,
    PromoteFull,
    Abort,
    Retry,
}

impl ActionKind {
    pub fn key(self) -> char {
        match self {
            ActionKind::Restart => 'R',
            ActionKind::Pause | ActionKind::Resume => 'p',
            ActionKind::Undo => 'u',
            ActionKind::Promote => 'P',
            ActionKind::PromoteFull => 'F',
            ActionKind::Abort => 'A',
            ActionKind::Retry => 'Y',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Restart => "restart",
            ActionKind::Pause => "pause",
            ActionKind::Resume => "resume",
            ActionKind::Undo => "undo",
            ActionKind::Promote => "promote",
            ActionKind::PromoteFull => "promote-full",
            ActionKind::Abort => "abort",
            ActionKind::Retry => "retry",
        }
    }

    pub fn explain(self, r: &RolloutInfo, to: Option<i64>) -> String {
        match self {
            ActionKind::Restart => "rolls every pod (new pod template annotation)".into(),
            ActionKind::Pause => "stops the rollout where it is".into(),
            ActionKind::Resume => "continues a paused rollout".into(),
            ActionKind::Undo => match r.undo_target(to) {
                Some(prev) => {
                    format!("rolls back to revision {} ({})", prev.revision, prev.image.rsplit('/').next().unwrap_or(&prev.image))
                }
                None => "rolls back to the previous revision".into(),
            },
            ActionKind::Promote => "skips the current pause / advances to the next step".into(),
            ActionKind::PromoteFull => "skips all remaining steps and analysis — 100% new version".into(),
            ActionKind::Abort => "scales the canary down, traffic back to stable".into(),
            ActionKind::Retry => "restarts an aborted rollout from the first step".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Action {
    pub kind: ActionKind,
    pub target: RolloutKind,
    pub namespace: String,
    pub name: String,
    /// Undo: the revision to go back to (None = the one before the current).
    pub to_revision: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    Ingress,
    HttpRoute,
    GrpcRoute,
}

impl RouteKind {
    pub fn short(self) -> &'static str {
        match self {
            RouteKind::Ingress => "ing",
            RouteKind::HttpRoute => "http",
            RouteKind::GrpcRoute => "grpc",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Backend {
    pub host: String,
    pub path: String,
    pub service: String,
    pub port: String,
    /// Ready / total endpoints; None = service does not exist.
    pub endpoints: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub struct RouteInfo {
    pub kind: RouteKind,
    pub namespace: String,
    pub name: String,
    /// Ingress class, or the parent Gateway(s) of a route.
    pub class: String,
    pub hosts: Vec<String>,
    pub backends: Vec<Backend>,
    /// (secret, hosts) — Ingress TLS sections; for routes, the TLS of the parent listeners.
    pub tls: Vec<(String, Vec<String>)>,
    pub address: String,
    /// Route conditions that are not happy (Accepted/ResolvedRefs false).
    pub problems: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Listener {
    pub name: String,
    pub protocol: String,
    pub port: i64,
    pub hostname: String,
    pub tls_secret: Option<String>,
    pub attached: i64,
}

#[derive(Clone, Debug)]
pub struct GatewayInfo {
    pub namespace: String,
    pub name: String,
    pub class: String,
    pub listeners: Vec<Listener>,
    pub addresses: Vec<String>,
    pub programmed: bool,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct CertInfo {
    pub namespace: String,
    pub name: String,
    pub secret: String,
    pub dns_names: Vec<String>,
    pub issuer: String,
    pub ready: bool,
    pub reason: String,
    pub message: String,
    pub not_after: Option<String>,
    pub renewal: Option<String>,
    pub days_left: Option<i64>,
    /// Pending ACME challenges: "http-01 kibana.example.com: pending — Waiting for HTTP-01 challenge propagation".
    pub challenges: Vec<String>,
}

impl CertInfo {
    pub fn health(&self) -> Health {
        match (self.ready, self.days_left) {
            (false, _) => Health::Bad,
            (true, Some(d)) if d < 7 => Health::Bad,
            (true, Some(d)) if d < 21 => Health::Warn,
            _ => Health::Ok,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IssuerInfo {
    pub cluster: bool,
    pub namespace: String,
    pub name: String,
    /// "acme https://acme-v02.api.letsencrypt.org/directory", "ca", "selfSigned", "vault"...
    pub kind: String,
    pub ready: bool,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct NetInfo {
    pub routes: Vec<RouteInfo>,
    pub gateways: Vec<GatewayInfo>,
    pub certs: Vec<CertInfo>,
    pub issuers: Vec<IssuerInfo>,
    /// TLS secrets that exist (namespace, name), to spot dangling references.
    pub tls_secrets: BTreeSet<(String, String)>,
}

impl NetInfo {
    pub fn cert_for(&self, namespace: &str, secret: &str) -> Option<&CertInfo> {
        self.certs.iter().find(|c| c.namespace == namespace && c.secret == secret)
    }

    pub fn route_health(&self, r: &RouteInfo) -> Health {
        let mut h = Health::Ok;
        if !r.problems.is_empty() || r.backends.iter().any(|b| matches!(b.endpoints, None | Some((0, _)))) {
            return Health::Bad;
        }
        if r.backends.iter().any(|b| matches!(b.endpoints, Some((ready, total)) if ready < total)) {
            h = Health::Warn;
        }
        for (secret, _) in &r.tls {
            match self.cert_for(&r.namespace, secret) {
                Some(c) => h = h.min(c.health()),
                None if !self.tls_secrets.contains(&(r.namespace.clone(), secret.clone())) => return Health::Bad,
                None => {}
            }
        }
        h
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub api_groups: Vec<String>,
    pub resources: Vec<String>,
    pub verbs: Vec<String>,
    pub names: Vec<String>,
    pub urls: Vec<String>,
}

impl Rule {
    fn from(r: PolicyRule) -> Self {
        Rule {
            api_groups: r.api_groups.unwrap_or_default(),
            resources: r.resources.unwrap_or_default(),
            verbs: r.verbs,
            names: r.resource_names.unwrap_or_default(),
            urls: r.non_resource_urls.unwrap_or_default(),
        }
    }

    pub fn allows(&self, verb: &str, resource: &str, group: &str) -> bool {
        let any = |list: &[String], v: &str| list.iter().any(|x| x == "*" || x == v);
        self.names.is_empty() && any(&self.verbs, verb) && any(&self.resources, resource) && any(&self.api_groups, group)
    }

    /// Why this rule is risky, if it is.
    pub fn risk(&self) -> Option<(Health, &'static str)> {
        let has = |list: &[String], v: &str| list.iter().any(|x| x == "*" || x == v);
        let res = |r: &str| has(&self.resources, r);
        let verb = |v: &str| has(&self.verbs, v);
        if verb("*") && res("*") {
            return Some((Health::Bad, "full wildcard (cluster-admin equivalent)"));
        }
        if verb("escalate") || verb("bind") || verb("impersonate") {
            return Some((Health::Bad, "escalate / bind / impersonate"));
        }
        if res("secrets") && (verb("get") || verb("list") || verb("watch")) && self.names.is_empty() {
            return Some((Health::Bad, "reads secrets"));
        }
        if (res("pods/exec") || res("pods/attach")) && (verb("create") || verb("get")) {
            return Some((Health::Bad, "exec into pods"));
        }
        if res("nodes/proxy") {
            return Some((Health::Bad, "nodes/proxy (kubelet API)"));
        }
        if ["roles", "clusterroles", "rolebindings", "clusterrolebindings"].iter().any(|r| res(r))
            && (verb("create") || verb("update") || verb("patch"))
        {
            return Some((Health::Bad, "edits RBAC"));
        }
        if res("pods") && verb("create") {
            return Some((Health::Warn, "creates pods (can mount any SA in namespace)"));
        }
        if verb("*") || res("*") {
            return Some((Health::Warn, "wildcard"));
        }
        if verb("delete") || verb("deletecollection") {
            return Some((Health::Warn, "deletes"));
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct RoleInfo {
    pub cluster: bool,
    pub namespace: String,
    pub name: String,
    pub rules: Vec<Rule>,
    pub aggregated: bool,
}

impl RoleInfo {
    pub fn risk(&self) -> Option<(Health, &'static str)> {
        self.rules.iter().filter_map(Rule::risk).min_by_key(|(h, _)| *h)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Subject {
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

impl Subject {
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "ServiceAccount" => format!("sa:{}/{}", self.namespace, self.name),
            "Group" => format!("group:{}", self.name),
            _ => format!("user:{}", self.name),
        }
    }

    pub fn is_system(&self) -> bool {
        self.name.starts_with("system:") || self.namespace == "kube-system"
    }
}

#[derive(Clone, Debug)]
pub struct BindingInfo {
    pub cluster: bool,
    pub namespace: String,
    pub name: String,
    pub role_cluster: bool,
    pub role: String,
    pub subjects: Vec<Subject>,
}

impl BindingInfo {
    /// Where the granted permissions apply.
    pub fn scope(&self) -> String {
        if self.cluster { "cluster-wide".into() } else { format!("ns {}", self.namespace) }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RbacInfo {
    pub roles: Vec<RoleInfo>,
    pub bindings: Vec<BindingInfo>,
    pub service_accounts: BTreeSet<(String, String)>,
}

impl RbacInfo {
    pub fn role_of(&self, b: &BindingInfo) -> Option<&RoleInfo> {
        self.roles.iter().find(|r| r.name == b.role && r.cluster == b.role_cluster && (r.cluster || r.namespace == b.namespace))
    }

    /// Every subject with its bindings.
    pub fn subjects(&self) -> BTreeMap<Subject, Vec<&BindingInfo>> {
        let mut out: BTreeMap<Subject, Vec<&BindingInfo>> = BTreeMap::new();
        for b in &self.bindings {
            for s in &b.subjects {
                out.entry(s.clone()).or_default().push(b);
            }
        }
        out
    }

    pub fn subject_risk(&self, bindings: &[&BindingInfo]) -> Option<(Health, &'static str)> {
        bindings
            .iter()
            .filter_map(|b| {
                let (h, why) = self.role_of(b)?.risk()?;
                // A namespaced grant of a "Bad" rule is less severe than the same grant cluster-wide.
                Some((if b.cluster { h } else { h.max(Health::Warn) }, why))
            })
            .min_by_key(|(h, _)| *h)
    }

    pub fn missing_sa(&self, s: &Subject) -> bool {
        s.kind == "ServiceAccount" && !self.service_accounts.contains(&(s.namespace.clone(), s.name.clone()))
    }

    /// Subjects allowed to `verb` `resource` in `group` (in `namespace`, or anywhere when None).
    pub fn who_can(&self, verb: &str, resource: &str, group: &str, namespace: Option<&str>) -> Vec<(Subject, String)> {
        let mut out = vec![];
        for b in &self.bindings {
            if !b.cluster && namespace.is_some_and(|ns| ns != b.namespace) {
                continue;
            }
            let Some(role) = self.role_of(b) else { continue };
            if role.rules.iter().any(|r| r.allows(verb, resource, group)) {
                for s in &b.subjects {
                    out.push((
                        s.clone(),
                        format!("{} → {}{} ({})", b.name, if b.role_cluster { "ClusterRole/" } else { "Role/" }, b.role, b.scope()),
                    ));
                }
            }
        }
        out.sort();
        out.dedup_by(|a, b| a.0 == b.0);
        out
    }
}

/// Which optional APIs the cluster has.
#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub argo_rollouts: bool,
    pub gateway_api: bool,
    pub cert_manager: bool,
    pub argocd: bool,
    pub metrics: bool,
}

// ───────────────────────────── discovery ─────────────────────────────

#[derive(Clone, Default)]
pub struct Crds {
    pub rollout: Option<ApiResource>,
    pub gateway: Option<ApiResource>,
    pub http_route: Option<ApiResource>,
    pub grpc_route: Option<ApiResource>,
    pub certificate: Option<ApiResource>,
    pub issuer: Option<ApiResource>,
    pub cluster_issuer: Option<ApiResource>,
    pub challenge: Option<ApiResource>,
    pub application: Option<ApiResource>,
    pub node_metrics: Option<ApiResource>,
    pub pod_metrics: Option<ApiResource>,
}

impl Crds {
    pub async fn discover(client: &Client) -> Crds {
        async fn kind(client: &Client, group: &str, kind: &str) -> Option<ApiResource> {
            let g = kube::discovery::group(client, group).await.ok()?;
            g.recommended_kind(kind).map(|(ar, _)| ar)
        }
        let (application, node_metrics, pod_metrics) = tokio::join!(
            kind(client, "argoproj.io", "Application"),
            kind(client, "metrics.k8s.io", "NodeMetrics"),
            kind(client, "metrics.k8s.io", "PodMetrics"),
        );
        let (rollout, gateway, http_route, grpc_route, certificate, issuer, cluster_issuer, challenge) = tokio::join!(
            kind(client, "argoproj.io", "Rollout"),
            kind(client, "gateway.networking.k8s.io", "Gateway"),
            kind(client, "gateway.networking.k8s.io", "HTTPRoute"),
            kind(client, "gateway.networking.k8s.io", "GRPCRoute"),
            kind(client, "cert-manager.io", "Certificate"),
            kind(client, "cert-manager.io", "Issuer"),
            kind(client, "cert-manager.io", "ClusterIssuer"),
            kind(client, "acme.cert-manager.io", "Challenge"),
        );
        Crds {
            rollout,
            gateway,
            http_route,
            grpc_route,
            certificate,
            issuer,
            cluster_issuer,
            challenge,
            application,
            node_metrics,
            pod_metrics,
        }
    }

    pub fn caps(&self) -> Caps {
        Caps {
            argo_rollouts: self.rollout.is_some(),
            gateway_api: self.gateway.is_some(),
            cert_manager: self.certificate.is_some(),
            argocd: self.application.is_some(),
            metrics: self.node_metrics.is_some() && self.pod_metrics.is_some(),
        }
    }
}

async fn list_dyn(client: &Client, ar: &Option<ApiResource>) -> Result<Vec<DynamicObject>> {
    match ar {
        None => Ok(vec![]),
        Some(ar) => Ok(Api::<DynamicObject>::all_with(client.clone(), ar)
            .list(&ListParams::default())
            .await
            .with_context(|| format!("listing {}", ar.plural))?
            .items),
    }
}

fn s(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    match cur {
        Value::String(x) => x.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn i(v: &Value, path: &[&str]) -> i64 {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur.as_i64().unwrap_or(0)
}

fn arr<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn condition<'a>(v: &'a Value, kind: &str) -> Option<&'a Value> {
    arr(v, &["status", "conditions"]).iter().find(|c| c["type"] == kind)
}

fn age(meta: &ObjectMeta) -> String {
    meta.creation_timestamp.as_ref().map(|t| ago(t.0)).unwrap_or_default()
}

pub fn ago(t: Timestamp) -> String {
    let secs = Timestamp::now().as_second() - t.as_second();
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub(crate) fn days_until(rfc3339: &str) -> Option<i64> {
    let t: Timestamp = rfc3339.parse().ok()?;
    Some((t.as_second() - Timestamp::now().as_second()).div_euclid(86_400))
}

fn ios(v: &Option<IntOrString>) -> String {
    match v {
        Some(IntOrString::Int(n)) => n.to_string(),
        Some(IntOrString::String(s)) => s.clone(),
        None => "25%".into(),
    }
}

fn controller_uid(meta: &ObjectMeta) -> Option<&str> {
    meta.owner_references.as_ref()?.iter().find(|o| o.controller == Some(true)).map(|o| o.uid.as_str())
}

// ───────────────────────────── rollouts ─────────────────────────────

pub fn rollouts(
    deps: &[Deployment],
    rss: &[ReplicaSet],
    stss: &[StatefulSet],
    dss: &[DaemonSet],
    crs: &[ControllerRevision],
    argo: &[DynamicObject],
) -> Vec<RolloutInfo> {
    let mut out = vec![];
    let history = |uid: &str, hash_label: &str| -> Vec<Revision> {
        let mut h: Vec<Revision> = rss
            .iter()
            .filter(|rs| controller_uid(&rs.metadata) == Some(uid))
            .map(|rs| {
                let ann = rs.metadata.annotations.clone().unwrap_or_default();
                let revision = ann
                    .get("deployment.kubernetes.io/revision")
                    .or_else(|| ann.get("rollout.argoproj.io/revision"))
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(0);
                let hash = rs.metadata.labels.as_ref().and_then(|l| l.get(hash_label)).cloned().unwrap_or_default();
                Revision {
                    revision,
                    name: if hash.is_empty() { rs.metadata.name.clone().unwrap_or_default() } else { hash },
                    image: rs
                        .spec
                        .as_ref()
                        .and_then(|s| s.template.as_ref())
                        .and_then(|t| t.spec.as_ref())
                        .and_then(|s| s.containers.first())
                        .and_then(|c| c.image.clone())
                        .unwrap_or_default(),
                    replicas: rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0),
                    ready: rs.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0),
                    age: age(&rs.metadata),
                    role: None,
                }
            })
            .collect();
        h.sort_by_key(|r| std::cmp::Reverse(r.revision));
        h
    };

    for d in deps {
        let spec = d.spec.clone().unwrap_or_default();
        let st = d.status.clone().unwrap_or_default();
        let desired = spec.replicas.unwrap_or(1);
        let strategy = match spec.strategy.as_ref().and_then(|s| s.type_.as_deref()) {
            Some("Recreate") => "Recreate".to_string(),
            _ => {
                let ru = spec.strategy.as_ref().and_then(|s| s.rolling_update.clone()).unwrap_or_default();
                format!("RollingUpdate surge {} unavail {}", ios(&ru.max_surge), ios(&ru.max_unavailable))
            }
        };
        let conditions = st.conditions.clone().unwrap_or_default();
        let progressing = conditions.iter().find(|c| c.type_ == "Progressing");
        let deadline = progressing.is_some_and(|c| c.reason.as_deref() == Some("ProgressDeadlineExceeded"));
        let (updated, ready, available) =
            (st.updated_replicas.unwrap_or(0), st.ready_replicas.unwrap_or(0), st.available_replicas.unwrap_or(0));
        let stale = st.observed_generation.unwrap_or(0) < d.metadata.generation.unwrap_or(0);
        let total = st.replicas.unwrap_or(0);
        let paused = spec.paused.unwrap_or(false);
        let phase = if deadline {
            Phase::Degraded
        } else if paused {
            Phase::Paused
        } else if !stale && updated == desired && ready == desired && available == desired && total == desired {
            Phase::Complete
        } else if desired > 0 && available == 0 && !stale {
            Phase::Degraded
        } else {
            Phase::Progressing
        };
        let uid = d.metadata.uid.clone().unwrap_or_default();
        let revision = d
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("deployment.kubernetes.io/revision"))
            .and_then(|r| r.parse().ok())
            .unwrap_or(0);
        out.push(RolloutInfo {
            kind: RolloutKind::Deployment,
            namespace: d.metadata.namespace.clone().unwrap_or_default(),
            name: d.metadata.name.clone().unwrap_or_default(),
            strategy,
            revision,
            desired,
            updated,
            ready,
            available,
            paused,
            phase,
            message: progressing.and_then(|c| c.message.clone()).unwrap_or_default(),
            image: spec.template.spec.as_ref().and_then(|s| s.containers.first()).and_then(|c| c.image.clone()).unwrap_or_default(),
            history: history(&uid, "pod-template-hash"),
            steps: vec![],
            step: None,
            weight: None,
            rollback_window: None,
        });
    }

    for s in stss {
        let spec = s.spec.clone().unwrap_or_default();
        let st = s.status.clone().unwrap_or_default();
        let desired = spec.replicas.unwrap_or(1);
        let us = spec.update_strategy.clone().unwrap_or_default();
        let strategy = match us.type_.as_deref() {
            Some("OnDelete") => "OnDelete".to_string(),
            _ => match us.rolling_update.and_then(|r| r.partition).filter(|p| *p > 0) {
                Some(p) => format!("RollingUpdate partition {p}"),
                None => "RollingUpdate".into(),
            },
        };
        let (updated, ready) = (st.updated_replicas.unwrap_or(0), st.ready_replicas.unwrap_or(0));
        let in_sync = st.current_revision.is_some() && st.current_revision == st.update_revision;
        let phase = if desired > 0 && ready == 0 {
            Phase::Degraded
        } else if in_sync && ready == desired {
            Phase::Complete
        } else {
            Phase::Progressing
        };
        out.push(RolloutInfo {
            kind: RolloutKind::StatefulSet,
            namespace: s.metadata.namespace.clone().unwrap_or_default(),
            name: s.metadata.name.clone().unwrap_or_default(),
            strategy,
            revision: cr_current(crs, s.metadata.uid.as_deref()),
            desired,
            updated,
            ready,
            available: st.available_replicas.unwrap_or(ready),
            paused: false,
            phase,
            message: match (&st.current_revision, &st.update_revision) {
                (Some(c), Some(u)) if c != u => format!("{c} → {u}"),
                (_, Some(u)) => u.clone(),
                _ => String::new(),
            },
            image: spec.template.spec.as_ref().and_then(|s| s.containers.first()).and_then(|c| c.image.clone()).unwrap_or_default(),
            history: cr_history(crs, s.metadata.uid.as_deref(), updated, desired),
            steps: vec![],
            step: None,
            weight: None,
            rollback_window: None,
        });
    }

    for d in dss {
        let spec = d.spec.clone().unwrap_or_default();
        let st = d.status.clone().unwrap_or_default();
        let desired = st.desired_number_scheduled;
        let updated = st.updated_number_scheduled.unwrap_or(0);
        let available = st.number_available.unwrap_or(0);
        let strategy = match spec.update_strategy.as_ref().and_then(|s| s.type_.as_deref()) {
            Some("OnDelete") => "OnDelete".to_string(),
            _ => {
                let ru = spec.update_strategy.as_ref().and_then(|s| s.rolling_update.clone()).unwrap_or_default();
                format!("RollingUpdate unavail {}", ios(&ru.max_unavailable).replace("25%", "1"))
            }
        };
        let phase = if desired > 0 && st.number_ready == 0 {
            Phase::Degraded
        } else if updated == desired && available == desired {
            Phase::Complete
        } else {
            Phase::Progressing
        };
        out.push(RolloutInfo {
            kind: RolloutKind::DaemonSet,
            namespace: d.metadata.namespace.clone().unwrap_or_default(),
            name: d.metadata.name.clone().unwrap_or_default(),
            strategy,
            revision: cr_current(crs, d.metadata.uid.as_deref()),
            desired,
            updated,
            ready: st.number_ready,
            available,
            paused: false,
            phase,
            message: String::new(),
            image: spec.template.spec.as_ref().and_then(|s| s.containers.first()).and_then(|c| c.image.clone()).unwrap_or_default(),
            history: cr_history(crs, d.metadata.uid.as_deref(), updated, desired),
            steps: vec![],
            step: None,
            weight: None,
            rollback_window: None,
        });
    }

    for r in argo {
        out.push(argo_rollout(r, &history));
    }
    out
}

fn cr_current(crs: &[ControllerRevision], uid: Option<&str>) -> i64 {
    crs.iter().filter(|cr| controller_uid(&cr.metadata) == uid).map(|cr| cr.revision).max().unwrap_or(0)
}

/// StatefulSet/DaemonSet history from their ControllerRevisions, newest first. The newest one is
/// what updated pods run; while a rollout is in progress the one before it runs the rest.
fn cr_history(crs: &[ControllerRevision], uid: Option<&str>, updated: i32, desired: i32) -> Vec<Revision> {
    let mut owned: Vec<&ControllerRevision> = crs.iter().filter(|cr| controller_uid(&cr.metadata) == uid).collect();
    owned.sort_by_key(|cr| std::cmp::Reverse(cr.revision));
    owned
        .iter()
        .enumerate()
        .map(|(i, cr)| {
            let data = cr.data.as_ref().map(|d| &d.0);
            let image = data
                .and_then(|d| d["spec"]["template"]["spec"]["containers"].as_array())
                .and_then(|c| c.first())
                .and_then(|c| c["image"].as_str())
                .unwrap_or_default()
                .to_string();
            let name = cr.metadata.name.clone().unwrap_or_default();
            let replicas = match i {
                0 => updated,
                1 => (desired - updated).max(0),
                _ => 0,
            };
            Revision {
                revision: cr.revision,
                name: name.rsplit('-').next().unwrap_or(&name).to_string(),
                image,
                replicas,
                ready: replicas,
                age: age(&cr.metadata),
                role: None,
            }
        })
        .collect()
}

fn argo_rollout(r: &DynamicObject, history: &dyn Fn(&str, &str) -> Vec<Revision>) -> RolloutInfo {
    let v = &r.data;
    let canary = &v["spec"]["strategy"]["canary"];
    let blue_green = &v["spec"]["strategy"]["blueGreen"];
    let steps: Vec<String> = arr(canary, &["steps"])
        .iter()
        .map(|st| {
            if let Some(w) = st["setWeight"].as_i64() {
                format!("setWeight {w}%")
            } else if st.get("pause").is_some() {
                match st["pause"]["duration"].as_str().map(str::to_string).or(st["pause"]["duration"].as_i64().map(|d| format!("{d}s"))) {
                    Some(d) => format!("pause {d}"),
                    None => "pause ∞ (needs promote)".into(),
                }
            } else if st.get("analysis").is_some() {
                format!("analysis {}", s(st, &["analysis", "templates"]).trim_matches('"'))
            } else if st.get("experiment").is_some() {
                "experiment".into()
            } else if let Some(n) = st["setCanaryScale"]["replicas"].as_i64() {
                format!("setCanaryScale {n}")
            } else {
                st.as_object().and_then(|o| o.keys().next().cloned()).unwrap_or_default()
            }
        })
        .collect();
    let status = &v["status"];
    let step = status["currentStepIndex"].as_i64().map(|n| n as usize);
    let weight = steps
        .iter()
        .take(step.map_or(steps.len(), |s| (s + 1).min(steps.len())))
        .filter_map(|s| s.strip_prefix("setWeight ")?.trim_end_matches('%').parse().ok())
        .next_back();
    let paused = v["spec"]["paused"].as_bool().unwrap_or(false) || !arr(status, &["pauseConditions"]).is_empty();
    let phase = if status["abort"].as_bool() == Some(true) {
        Phase::Aborted
    } else {
        match status["phase"].as_str() {
            Some("Healthy") => Phase::Complete,
            Some("Paused") => Phase::Paused,
            Some("Degraded") => Phase::Degraded,
            _ if paused => Phase::Paused,
            _ => Phase::Progressing,
        }
    };
    let strategy = if canary.is_object() {
        let traffic = canary["trafficRouting"].as_object().and_then(|o| o.keys().next().cloned());
        match traffic {
            Some(t) => format!("Canary via {t}"),
            None => "Canary (replica weighted)".into(),
        }
    } else if blue_green.is_object() {
        format!("BlueGreen active={} preview={}", s(blue_green, &["activeService"]), s(blue_green, &["previewService"]))
    } else {
        "Canary".into()
    };
    let uid = r.metadata.uid.clone().unwrap_or_default();
    let mut hist = history(&uid, "rollouts-pod-template-hash");
    let (stable, current) = (s(status, &["stableRS"]), s(status, &["currentPodHash"]));
    for h in &mut hist {
        h.role = if h.name == stable && h.name == current {
            Some("stable")
        } else if h.name == stable {
            Some(if blue_green.is_object() { "active" } else { "stable" })
        } else if h.name == current {
            Some(if blue_green.is_object() { "preview" } else { "canary" })
        } else {
            None
        };
    }
    RolloutInfo {
        kind: RolloutKind::Argo,
        namespace: r.metadata.namespace.clone().unwrap_or_default(),
        name: r.metadata.name.clone().unwrap_or_default(),
        strategy,
        revision: r
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("rollout.argoproj.io/revision"))
            .and_then(|x| x.parse().ok())
            .unwrap_or(0),
        desired: v["spec"]["replicas"].as_i64().unwrap_or(1) as i32,
        updated: i(status, &["updatedReplicas"]) as i32,
        ready: i(status, &["readyReplicas"]) as i32,
        available: i(status, &["availableReplicas"]) as i32,
        paused,
        phase,
        message: s(status, &["message"]),
        image: arr(&v["spec"]["template"]["spec"], &["containers"]).first().map(|c| s(c, &["image"])).unwrap_or_default(),
        history: hist,
        steps,
        step,
        weight,
        rollback_window: v["spec"]["rollbackWindow"]["revisions"].as_i64(),
    }
}

// ───────────────────────────── traffic ─────────────────────────────

pub async fn net(client: &Client, crds: &Crds) -> Result<NetInfo> {
    let lp = ListParams::default();
    let tls_lp = ListParams::default().fields("type=kubernetes.io/tls");
    let (ings, svcs, slices, secrets) = tokio::try_join!(
        async { Api::<Ingress>::all(client.clone()).list(&lp).await.context("listing ingresses") },
        async { Api::<Service>::all(client.clone()).list(&lp).await.context("listing services") },
        async { Api::<EndpointSlice>::all(client.clone()).list(&lp).await.context("listing endpointslices") },
        async { Api::<Secret>::all(client.clone()).list_metadata(&tls_lp).await.context("listing TLS secrets") },
    )?;
    let (gws, hrs, grs, certs, issuers, cissuers, challenges) = tokio::try_join!(
        list_dyn(client, &crds.gateway),
        list_dyn(client, &crds.http_route),
        list_dyn(client, &crds.grpc_route),
        list_dyn(client, &crds.certificate),
        list_dyn(client, &crds.issuer),
        list_dyn(client, &crds.cluster_issuer),
        list_dyn(client, &crds.challenge),
    )?;

    let services: BTreeSet<(String, String)> = svcs
        .items
        .iter()
        .map(|s| (s.metadata.namespace.clone().unwrap_or_default(), s.metadata.name.clone().unwrap_or_default()))
        .collect();
    let mut eps: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
    for sl in &slices.items {
        let Some(svc) = sl.metadata.labels.as_ref().and_then(|l| l.get("kubernetes.io/service-name")) else { continue };
        let e = eps.entry((sl.metadata.namespace.clone().unwrap_or_default(), svc.clone())).or_default();
        for ep in sl.endpoints.iter().flatten() {
            e.1 += 1;
            if ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true) {
                e.0 += 1;
            }
        }
    }
    let endpoints = |ns: &str, svc: &str| -> Option<(usize, usize)> {
        services
            .contains(&(ns.to_string(), svc.to_string()))
            .then(|| eps.get(&(ns.to_string(), svc.to_string())).copied().unwrap_or((0, 0)))
    };

    let mut info = NetInfo {
        tls_secrets: secrets
            .items
            .iter()
            .map(|s| (s.metadata.namespace.clone().unwrap_or_default(), s.metadata.name.clone().unwrap_or_default()))
            .collect(),
        ..Default::default()
    };

    for ing in ings.items {
        let ns = ing.metadata.namespace.clone().unwrap_or_default();
        let spec = ing.spec.unwrap_or_default();
        let ann = ing.metadata.annotations.unwrap_or_default();
        let mut backends = vec![];
        let mut hosts = vec![];
        for rule in spec.rules.unwrap_or_default() {
            let host = rule.host.unwrap_or_else(|| "*".into());
            hosts.push(host.clone());
            for p in rule.http.map(|h| h.paths).unwrap_or_default() {
                if let Some(svc) = p.backend.service {
                    let port = svc.port.map(|p| p.number.map(|n| n.to_string()).or(p.name).unwrap_or_default()).unwrap_or_default();
                    backends.push(Backend {
                        host: host.clone(),
                        path: p.path.unwrap_or_else(|| "/".into()),
                        endpoints: endpoints(&ns, &svc.name),
                        service: svc.name,
                        port,
                    });
                }
            }
        }
        if let Some(svc) = spec.default_backend.and_then(|b| b.service) {
            backends.push(Backend {
                host: "*".into(),
                path: "(default)".into(),
                endpoints: endpoints(&ns, &svc.name),
                port: svc.port.and_then(|p| p.number.map(|n| n.to_string()).or(p.name)).unwrap_or_default(),
                service: svc.name,
            });
        }
        let address = ing
            .status
            .and_then(|s| s.load_balancer)
            .and_then(|lb| lb.ingress)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| i.ip.or(i.hostname))
            .collect::<Vec<_>>()
            .join(",");
        info.routes.push(RouteInfo {
            kind: RouteKind::Ingress,
            namespace: ns,
            name: ing.metadata.name.unwrap_or_default(),
            class: spec
                .ingress_class_name
                .or_else(|| ann.get("kubernetes.io/ingress.class").cloned())
                .unwrap_or_else(|| "<default>".into()),
            hosts,
            backends,
            tls: spec.tls.unwrap_or_default().into_iter().filter_map(|t| Some((t.secret_name?, t.hosts.unwrap_or_default()))).collect(),
            address,
            problems: vec![],
        });
    }

    for gw in &gws {
        let v = &gw.data;
        let listeners_status = arr(v, &["status", "listeners"]);
        let listeners = arr(v, &["spec", "listeners"])
            .iter()
            .map(|l| {
                let name = s(l, &["name"]);
                Listener {
                    attached: listeners_status.iter().find(|x| x["name"] == name.as_str()).map(|x| i(x, &["attachedRoutes"])).unwrap_or(0),
                    name,
                    protocol: s(l, &["protocol"]),
                    port: i(l, &["port"]),
                    hostname: s(l, &["hostname"]),
                    tls_secret: arr(l, &["tls", "certificateRefs"]).first().map(|r| s(r, &["name"])),
                }
            })
            .collect();
        let programmed = condition(v, "Programmed").or(condition(v, "Ready"));
        info.gateways.push(GatewayInfo {
            namespace: gw.metadata.namespace.clone().unwrap_or_default(),
            name: gw.metadata.name.clone().unwrap_or_default(),
            class: s(v, &["spec", "gatewayClassName"]),
            listeners,
            addresses: arr(v, &["status", "addresses"]).iter().map(|a| s(a, &["value"])).collect(),
            programmed: programmed.is_some_and(|c| c["status"] == "True"),
            message: programmed.map(|c| s(c, &["message"])).unwrap_or_default(),
        });
    }

    for (kind, list) in [(RouteKind::HttpRoute, &hrs), (RouteKind::GrpcRoute, &grs)] {
        for r in list {
            let v = &r.data;
            let ns = r.metadata.namespace.clone().unwrap_or_default();
            let hosts: Vec<String> = arr(v, &["spec", "hostnames"]).iter().filter_map(|h| h.as_str().map(str::to_string)).collect();
            let parents: Vec<(String, String, String)> = arr(v, &["spec", "parentRefs"])
                .iter()
                .map(|p| {
                    let pns = s(p, &["namespace"]);
                    (if pns.is_empty() { ns.clone() } else { pns }, s(p, &["name"]), s(p, &["sectionName"]))
                })
                .collect();
            let mut backends = vec![];
            for rule in arr(v, &["spec", "rules"]) {
                let path = arr(rule, &["matches"])
                    .first()
                    .map(|m| {
                        let p = s(m, &["path", "value"]);
                        if p.is_empty() { s(m, &["method", "service"]) } else { p }
                    })
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "/".into());
                for b in arr(rule, &["backendRefs"]) {
                    let bns = s(b, &["namespace"]);
                    let bns = if bns.is_empty() { ns.clone() } else { bns };
                    let svc = s(b, &["name"]);
                    let weight = b["weight"].as_i64().map(|w| format!(" w{w}")).unwrap_or_default();
                    backends.push(Backend {
                        host: hosts.first().cloned().unwrap_or_else(|| "*".into()),
                        path: path.clone(),
                        endpoints: endpoints(&bns, &svc),
                        service: if bns == ns { svc } else { format!("{bns}/{svc}") },
                        port: format!("{}{weight}", s(b, &["port"])),
                    });
                }
            }
            let mut problems = vec![];
            for p in arr(v, &["status", "parents"]) {
                for c in arr(p, &["conditions"]) {
                    if c["status"] == "False" {
                        problems.push(format!("{}: {} — {}", s(c, &["type"]), s(c, &["reason"]), s(c, &["message"])));
                    }
                }
            }
            let tls = parents
                .iter()
                .filter_map(|(pns, pname, section)| {
                    let gw = info.gateways.iter().find(|g| g.namespace == *pns && g.name == *pname)?;
                    let l = gw.listeners.iter().find(|l| l.tls_secret.is_some() && (section.is_empty() || l.name == *section))?;
                    // Gateway listener certs live in the gateway's namespace.
                    Some((l.tls_secret.clone()?, vec![l.hostname.clone()]))
                })
                .collect();
            let address = parents
                .iter()
                .filter_map(|(pns, pname, _)| info.gateways.iter().find(|g| g.namespace == *pns && g.name == *pname))
                .flat_map(|g| g.addresses.clone())
                .collect::<Vec<_>>()
                .join(",");
            info.routes.push(RouteInfo {
                kind,
                namespace: ns,
                name: r.metadata.name.clone().unwrap_or_default(),
                class: parents
                    .iter()
                    .map(|(n, g, sec)| if sec.is_empty() { format!("{n}/{g}") } else { format!("{n}/{g}:{sec}") })
                    .collect::<Vec<_>>()
                    .join(","),
                hosts,
                backends,
                tls,
                address,
                problems,
            });
        }
    }

    for c in &certs {
        let v = &c.data;
        let ready = condition(v, "Ready");
        let ns = c.metadata.namespace.clone().unwrap_or_default();
        let name = c.metadata.name.clone().unwrap_or_default();
        let not_after = Some(s(v, &["status", "notAfter"])).filter(|x| !x.is_empty());
        let cert_challenges = challenges
            .iter()
            .filter(|ch| {
                ch.metadata.namespace.as_deref() == Some(&ns)
                    && ch.metadata.owner_references.as_ref().is_some_and(|o| o.iter().any(|o| o.name.starts_with(&name)))
            })
            .map(|ch| {
                let cv = &ch.data;
                format!(
                    "{} {}: {} — {}",
                    s(cv, &["spec", "type"]).to_lowercase(),
                    s(cv, &["spec", "dnsName"]),
                    s(cv, &["status", "state"]),
                    s(cv, &["status", "reason"])
                )
            })
            .collect();
        info.certs.push(CertInfo {
            secret: s(v, &["spec", "secretName"]),
            dns_names: arr(v, &["spec", "dnsNames"]).iter().filter_map(|d| d.as_str().map(str::to_string)).collect(),
            issuer: {
                let kind = s(v, &["spec", "issuerRef", "kind"]);
                format!("{}/{}", if kind.is_empty() { "Issuer".into() } else { kind }, s(v, &["spec", "issuerRef", "name"]))
            },
            ready: ready.is_some_and(|r| r["status"] == "True"),
            reason: ready.map(|r| s(r, &["reason"])).unwrap_or_default(),
            message: ready.map(|r| s(r, &["message"])).unwrap_or_default(),
            days_left: not_after.as_deref().and_then(days_until),
            renewal: Some(s(v, &["status", "renewalTime"])).filter(|x| !x.is_empty()),
            not_after,
            challenges: cert_challenges,
            namespace: ns,
            name,
        });
    }

    for (cluster, list) in [(false, &issuers), (true, &cissuers)] {
        for is in list {
            let v = &is.data;
            let spec = &v["spec"];
            let kind = if spec["acme"].is_object() {
                let solvers: Vec<String> = arr(spec, &["acme", "solvers"])
                    .iter()
                    .filter_map(|s| s.as_object().and_then(|o| o.keys().find(|k| *k != "selector").cloned()))
                    .collect();
                let server = s(spec, &["acme", "server"]);
                let server = server.trim_start_matches("https://").trim_end_matches("/directory");
                format!("acme {server} [{}]", solvers.join(","))
            } else {
                spec.as_object().and_then(|o| o.keys().next().cloned()).unwrap_or_default()
            };
            let ready = condition(v, "Ready");
            info.issuers.push(IssuerInfo {
                cluster,
                namespace: is.metadata.namespace.clone().unwrap_or_default(),
                name: is.metadata.name.clone().unwrap_or_default(),
                kind,
                ready: ready.is_some_and(|r| r["status"] == "True"),
                message: ready.map(|r| s(r, &["message"])).unwrap_or_default(),
            });
        }
    }
    Ok(info)
}

// ───────────────────────────── RBAC ─────────────────────────────

pub async fn rbac(client: &Client) -> Result<RbacInfo> {
    let lp = ListParams::default();
    let (crs, roles, crbs, rbs, sas) = tokio::try_join!(
        async { Api::<ClusterRole>::all(client.clone()).list(&lp).await.context("listing clusterroles") },
        async { Api::<Role>::all(client.clone()).list(&lp).await.context("listing roles") },
        async { Api::<ClusterRoleBinding>::all(client.clone()).list(&lp).await.context("listing clusterrolebindings") },
        async { Api::<RoleBinding>::all(client.clone()).list(&lp).await.context("listing rolebindings") },
        async { Api::<ServiceAccount>::all(client.clone()).list_metadata(&lp).await.context("listing serviceaccounts") },
    )?;
    let mut info = RbacInfo::default();
    for r in crs.items {
        info.roles.push(RoleInfo {
            cluster: true,
            namespace: String::new(),
            name: r.metadata.name.unwrap_or_default(),
            rules: r.rules.unwrap_or_default().into_iter().map(Rule::from).collect(),
            aggregated: r.aggregation_rule.is_some(),
        });
    }
    for r in roles.items {
        info.roles.push(RoleInfo {
            cluster: false,
            namespace: r.metadata.namespace.unwrap_or_default(),
            name: r.metadata.name.unwrap_or_default(),
            rules: r.rules.unwrap_or_default().into_iter().map(Rule::from).collect(),
            aggregated: false,
        });
    }
    let subjects = |list: Option<Vec<k8s_openapi::api::rbac::v1::Subject>>, default_ns: &str| -> Vec<Subject> {
        list.unwrap_or_default()
            .into_iter()
            .map(|s| Subject {
                namespace: s.namespace.unwrap_or_else(|| if s.kind == "ServiceAccount" { default_ns.to_string() } else { String::new() }),
                kind: s.kind,
                name: s.name,
            })
            .collect()
    };
    for b in crbs.items {
        info.bindings.push(BindingInfo {
            cluster: true,
            namespace: String::new(),
            name: b.metadata.name.unwrap_or_default(),
            role_cluster: true,
            role: b.role_ref.name,
            subjects: subjects(b.subjects, ""),
        });
    }
    for b in rbs.items {
        let ns = b.metadata.namespace.unwrap_or_default();
        info.bindings.push(BindingInfo {
            cluster: false,
            name: b.metadata.name.unwrap_or_default(),
            role_cluster: b.role_ref.kind == "ClusterRole",
            role: b.role_ref.name,
            subjects: subjects(b.subjects, &ns),
            namespace: ns,
        });
    }
    info.service_accounts =
        sas.items.into_iter().map(|s| (s.metadata.namespace.unwrap_or_default(), s.metadata.name.unwrap_or_default())).collect();
    Ok(info)
}

// ───────────────────────────── actions ─────────────────────────────

pub async fn perform(client: &Client, crds: &Crds, a: &Action, dry_run: bool) -> Result<String> {
    let pp = PatchParams { field_manager: Some("k8s-galaxy".into()), dry_run, ..Default::default() };
    let now = Timestamp::now().to_string();
    let ns = a.namespace.as_str();
    use ActionKind::*;
    match a.target {
        RolloutKind::Argo => {
            let ar = crds.rollout.as_ref().ok_or_else(|| anyhow!("Argo Rollouts CRD not found"))?;
            let api = Api::<DynamicObject>::namespaced_with(client.clone(), ns, ar);
            let spec = |p: Value| merge(&api, &a.name, &pp, p, false);
            let status = |p: Value| merge(&api, &a.name, &pp, p, true);
            match a.kind {
                Restart => spec(json!({"spec": {"restartAt": now}})).await?,
                Pause => spec(json!({"spec": {"paused": true}})).await?,
                Resume => spec(json!({"spec": {"paused": false}})).await?,
                Promote => {
                    // Same as `kubectl argo rollouts promote`: unpause, or skip the current step when not paused.
                    let ro = api.get(&a.name).await?;
                    let paused = !arr(&ro.data, &["status", "pauseConditions"]).is_empty() || ro.data["spec"]["paused"] == true;
                    if paused {
                        spec(json!({"spec": {"paused": false}})).await?;
                        status(json!({"status": {"pauseConditions": null}})).await?;
                    } else {
                        let next = ro.data["status"]["currentStepIndex"].as_i64().unwrap_or(0) + 1;
                        status(json!({"status": {"currentStepIndex": next}})).await?;
                    }
                }
                PromoteFull => status(json!({"status": {"promoteFull": true}})).await?,
                Abort => status(json!({"status": {"abort": true}})).await?,
                Retry => status(json!({"status": {"abort": false}})).await?,
                Undo => {
                    let ro = api.get(&a.name).await?;
                    let tpl = previous_template(
                        client,
                        ns,
                        ro.metadata.uid.as_deref(),
                        &ro.metadata.annotations,
                        "rollout.argoproj.io/revision",
                        "rollouts-pod-template-hash",
                        a.to_revision,
                    )
                    .await?;
                    api.patch(&a.name, &pp, &Patch::Json::<()>(json_patch_template(tpl)?)).await?;
                }
            }
        }
        RolloutKind::Deployment => {
            let api = Api::<Deployment>::namespaced(client.clone(), ns);
            match a.kind {
                Restart => drop(api.patch(&a.name, &pp, &Patch::Merge(restart_patch(&now))).await?),
                Pause => drop(api.patch(&a.name, &pp, &Patch::Merge(json!({"spec": {"paused": true}}))).await?),
                Resume => drop(api.patch(&a.name, &pp, &Patch::Merge(json!({"spec": {"paused": false}}))).await?),
                Undo => {
                    let d = api.get(&a.name).await?;
                    let tpl = previous_template(
                        client,
                        ns,
                        d.metadata.uid.as_deref(),
                        &d.metadata.annotations,
                        "deployment.kubernetes.io/revision",
                        "pod-template-hash",
                        a.to_revision,
                    )
                    .await?;
                    api.patch(&a.name, &pp, &Patch::Json::<()>(json_patch_template(tpl)?)).await?;
                }
                k => bail!("{} is not supported for Deployments", k.label()),
            }
        }
        RolloutKind::StatefulSet => {
            let api = Api::<StatefulSet>::namespaced(client.clone(), ns);
            match a.kind {
                Restart => drop(api.patch(&a.name, &pp, &Patch::Merge(restart_patch(&now))).await?),
                // What `kubectl rollout undo` does: re-apply the template stored in the ControllerRevision.
                Undo => {
                    let uid = api.get(&a.name).await?.metadata.uid;
                    let data = revision_data(client, ns, uid.as_deref(), a.to_revision).await?;
                    api.patch(&a.name, &pp, &Patch::Strategic(data)).await?;
                }
                k => bail!("{} is not supported for StatefulSets", k.label()),
            }
        }
        RolloutKind::DaemonSet => {
            let api = Api::<DaemonSet>::namespaced(client.clone(), ns);
            match a.kind {
                Restart => drop(api.patch(&a.name, &pp, &Patch::Merge(restart_patch(&now))).await?),
                Undo => {
                    let uid = api.get(&a.name).await?.metadata.uid;
                    let data = revision_data(client, ns, uid.as_deref(), a.to_revision).await?;
                    api.patch(&a.name, &pp, &Patch::Strategic(data)).await?;
                }
                k => bail!("{} is not supported for DaemonSets", k.label()),
            }
        }
    }
    Ok(format!("{}{} {} {}/{}", if dry_run { "dry-run OK: " } else { "" }, a.kind.label(), a.target.as_str(), a.namespace, a.name))
}

async fn merge(api: &Api<DynamicObject>, name: &str, pp: &PatchParams, p: Value, status: bool) -> Result<()> {
    if status {
        api.patch_status(name, pp, &Patch::Merge(p)).await?;
    } else {
        api.patch(name, pp, &Patch::Merge(p)).await?;
    }
    Ok(())
}

fn restart_patch(now: &str) -> Value {
    json!({"spec": {"template": {"metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": now}}}}})
}

/// Pod template of the newest ReplicaSet older than the current revision (what `rollout undo` restores).
async fn previous_template(
    client: &Client,
    ns: &str,
    owner_uid: Option<&str>,
    annotations: &Option<BTreeMap<String, String>>,
    rev_key: &str,
    hash_label: &str,
    to: Option<i64>,
) -> Result<Value> {
    let current: i64 = annotations.as_ref().and_then(|a| a.get(rev_key)).and_then(|r| r.parse().ok()).unwrap_or(i64::MAX);
    let rss = Api::<ReplicaSet>::namespaced(client.clone(), ns).list(&ListParams::default()).await?;
    let prev = rss
        .items
        .into_iter()
        .filter(|rs| controller_uid(&rs.metadata) == owner_uid)
        .filter_map(|rs| {
            let rev: i64 = rs.metadata.annotations.as_ref()?.get(rev_key)?.parse().ok()?;
            let wanted = match to {
                Some(t) => rev == t && rev != current,
                None => rev < current,
            };
            wanted.then_some((rev, rs))
        })
        .max_by_key(|(rev, _)| *rev)
        .ok_or_else(|| match to {
            Some(t) => anyhow!("revision {t} is not kept any more"),
            None => anyhow!("no previous revision to roll back to"),
        })?;
    let mut tpl = serde_json::to_value(prev.1.spec.and_then(|s| s.template).ok_or_else(|| anyhow!("ReplicaSet has no template"))?)?;
    if let Some(labels) = tpl["metadata"]["labels"].as_object_mut() {
        labels.remove(hash_label);
    }
    Ok(tpl)
}

/// Stored template patch of a StatefulSet/DaemonSet ControllerRevision: the requested revision,
/// else the newest one older than the current (= newest) revision.
async fn revision_data(client: &Client, ns: &str, owner_uid: Option<&str>, to: Option<i64>) -> Result<Value> {
    let crs = Api::<ControllerRevision>::namespaced(client.clone(), ns).list(&ListParams::default()).await?;
    let mut owned: Vec<ControllerRevision> = crs.items.into_iter().filter(|cr| controller_uid(&cr.metadata) == owner_uid).collect();
    owned.sort_by_key(|cr| std::cmp::Reverse(cr.revision));
    let current = owned.first().map(|cr| cr.revision).ok_or_else(|| anyhow!("no ControllerRevisions found"))?;
    let pick = match to {
        Some(t) if t == current => bail!("revision {t} is the current one"),
        Some(t) => owned.into_iter().find(|cr| cr.revision == t).ok_or_else(|| anyhow!("revision {t} is not kept any more"))?,
        None => owned.into_iter().find(|cr| cr.revision < current).ok_or_else(|| anyhow!("no previous revision to roll back to"))?,
    };
    pick.data.map(|d| d.0).ok_or_else(|| anyhow!("ControllerRevision {} has no data", pick.metadata.name.unwrap_or_default()))
}

fn json_patch_template(tpl: Value) -> Result<json_patch::Patch> {
    Ok(serde_json::from_value(json!([{"op": "replace", "path": "/spec/template", "value": tpl}]))?)
}

/// Argo CD Application whose resources include this workload.
pub fn managing_app<'a>(r: &RolloutInfo, apps: &'a [crate::gitops::AppInfo]) -> Option<&'a crate::gitops::AppInfo> {
    apps.iter().find(|a| a.resources.iter().any(|x| x.kind == r.kind.as_str() && x.namespace == r.namespace && x.name == r.name))
}

/// Things to know before an action: will it stick, what else happens. `Health::Ok` = information.
pub fn action_warnings(r: &RolloutInfo, kind: ActionKind, to: Option<i64>, apps: &[crate::gitops::AppInfo]) -> Vec<(Health, String)> {
    let mut w = vec![];
    let app = managing_app(r, apps);
    if let Some(a) = app
        && a.running()
    {
        w.push((Health::Warn, format!("Argo CD is syncing app {} right now — it may overwrite this", a.name)));
    }
    match kind {
        ActionKind::Undo => {
            match r.undo_target(to) {
                None => w.push((Health::Bad, "no older revision is kept (revisionHistoryLimit: 0?) — nothing to roll back to".into())),
                Some(prev) => {
                    let skipped = r.history.iter().filter(|h| h.revision > prev.revision && h.revision < r.revision).count();
                    if skipped > 0 {
                        w.push((Health::Ok, format!("r{} → r{}, skipping {skipped} newer revision(s)", r.revision, prev.revision)));
                    }
                    if prev.revision > r.revision {
                        w.push((
                            Health::Warn,
                            format!("r{} is newer than the current r{} — this rolls forward, not back", prev.revision, r.revision),
                        ));
                    }
                    if prev.image == r.image {
                        w.push((
                            Health::Warn,
                            format!("r{} runs the same image ({}) — only other pod-template fields change", prev.revision, prev.image),
                        ));
                    }
                }
            }
            match app {
                Some(a) if a.self_heal => w.push((
                    Health::Bad,
                    format!(
                        "Argo CD app {} has self-heal: it puts the Git version back within minutes. For a lasting rollback revert the commit in Git (or pin targetRevision).",
                        a.name
                    ),
                )),
                Some(a) if a.auto => w.push((Health::Warn, format!("Argo CD app {} auto-syncs: the next Git change rolls forward again; the app shows OutOfSync until then", a.name))),
                Some(a) => w.push((Health::Warn, format!("Argo CD app {} will show OutOfSync; its next sync re-applies the Git version", a.name))),
                None => {}
            }
            if r.kind == RolloutKind::Argo && !r.steps.is_empty() && r.rollback_window.is_none() {
                w.push((
                    Health::Warn,
                    format!(
                        "no spec.rollbackWindow: the rollback runs all {} canary steps again (F promote-full skips them)",
                        r.steps.len()
                    ),
                ));
            }
        }
        ActionKind::Pause if app.is_some() => w.push((Health::Ok, "Argo CD will report the app as Suspended while paused".into())),
        ActionKind::PromoteFull => w.push((Health::Warn, "skips the remaining steps and analysis — no automatic safety net".into())),
        ActionKind::Abort => {
            w.push((Health::Ok, "traffic goes back to stable; the rollout stays Degraded until retry (Y) or a new revision".into()))
        }
        _ => {}
    }
    w
}

/// Standing notes for the details pane.
pub fn caveats(r: &RolloutInfo, apps: &[crate::gitops::AppInfo]) -> Vec<(Health, String)> {
    let mut c = vec![];
    match managing_app(r, apps) {
        Some(a) if a.self_heal => {
            c.push((Health::Ok, format!("managed by Argo CD app {} ({})", a.name, a.policy)));
            c.push((Health::Warn, "undo (u) is temporary: self-heal restores the Git version — roll back in Git".into()));
        }
        Some(a) => c.push((Health::Ok, format!("managed by Argo CD app {} ({})", a.name, a.policy))),
        None => {}
    }
    match r.kind {
        RolloutKind::StatefulSet | RolloutKind::DaemonSet => c.push((
            Health::Warn,
            format!("no rollback for {}s here (history is in ControllerRevisions) — change the image in Git", r.kind.as_str()),
        )),
        RolloutKind::Argo if !r.steps.is_empty() && r.rollback_window.is_none() => {
            c.push((Health::Ok, "no spec.rollbackWindow: a rollback re-runs the canary steps".into()))
        }
        _ => {}
    }
    if (r.kind == RolloutKind::Deployment || r.kind == RolloutKind::Argo) && !r.history.iter().any(|h| h.revision < r.revision) {
        c.push((Health::Warn, "no older revision kept — nothing to roll back to".into()))
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_risks() {
        let rule = |verbs: &[&str], res: &[&str]| Rule {
            api_groups: vec!["".into()],
            resources: res.iter().map(|s| s.to_string()).collect(),
            verbs: verbs.iter().map(|s| s.to_string()).collect(),
            names: vec![],
            urls: vec![],
        };
        assert_eq!(rule(&["*"], &["*"]).risk().unwrap().0, Health::Bad);
        assert_eq!(rule(&["list"], &["secrets"]).risk().unwrap().1, "reads secrets");
        assert!(rule(&["get", "list"], &["pods"]).risk().is_none());
        assert!(rule(&["get"], &["pods"]).allows("get", "pods", ""));
        assert!(!rule(&["get"], &["pods"]).allows("delete", "pods", ""));
    }

    #[test]
    fn undo_warnings() {
        let rev = |n: i64, image: &str| Revision {
            revision: n,
            name: format!("h{n}"),
            image: image.into(),
            replicas: 0,
            ready: 0,
            age: "1d".into(),
            role: None,
        };
        let r = RolloutInfo {
            kind: RolloutKind::Deployment,
            namespace: "shop".into(),
            name: "api".into(),
            strategy: String::new(),
            revision: 5,
            desired: 1,
            updated: 1,
            ready: 1,
            available: 1,
            paused: false,
            phase: Phase::Complete,
            message: String::new(),
            image: "api:5".into(),
            history: vec![rev(5, "api:5"), rev(4, "api:4"), rev(3, "api:3")],
            steps: vec![],
            step: None,
            weight: None,
            rollback_window: None,
        };
        assert_eq!(r.undo_target(None).unwrap().revision, 4);
        assert_eq!(r.undo_candidates().iter().map(|h| h.revision).collect::<Vec<_>>(), vec![4, 3]);
        let app: crate::gitops::AppInfo = {
            let obj: DynamicObject = serde_json::from_value(json!({
                "apiVersion": "argoproj.io/v1alpha1", "kind": "Application", "metadata": {"name": "shop", "namespace": "argocd"},
                "spec": {"source": {"repoURL": "https://git/x.git"}, "destination": {"namespace": "shop"},
                         "syncPolicy": {"automated": {"selfHeal": true}}},
                "status": {"resources": [{"kind": "Deployment", "namespace": "shop", "name": "api", "status": "Synced"}]}
            }))
            .unwrap();
            crate::gitops::parse(&obj)
        };
        let w = action_warnings(&r, ActionKind::Undo, Some(3), std::slice::from_ref(&app));
        assert!(w.iter().any(|(h, t)| *h == Health::Bad && t.contains("self-heal")));
        assert!(w.iter().any(|(_, t)| t.contains("skipping 1 newer")));
        assert!(action_warnings(&r, ActionKind::Undo, None, &[]).iter().all(|(h, _)| *h != Health::Bad));
    }

    #[test]
    fn days_left() {
        let in_ten = Timestamp::from_second(Timestamp::now().as_second() + 10 * 86_400 + 60).unwrap();
        assert_eq!(days_until(&in_ten.to_string()), Some(10));
    }
}
