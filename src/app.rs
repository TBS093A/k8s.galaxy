use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;

use crate::gitops::{AppAction, AppInfo};
use crate::kubeconfigs::{ClusterEntry, Probe};
use crate::source::Target;
use crate::metrics::Usage;
use crate::ops::{Action, ActionKind, BindingInfo, CertInfo, GatewayInfo, IssuerInfo, RoleInfo, RolloutInfo, RouteInfo, Subject};
use crate::model::{Health, HelmRelease, PodInfo, Snapshot, WorkloadInfo, WorkloadKind};
use crate::source::Cmd;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Map,
    Galaxy,
    Radar,
    Rollouts,
    Network,
    Access,
    Argo,
}

impl View {
    pub const ALL: [View; 7] = [View::Map, View::Galaxy, View::Radar, View::Rollouts, View::Network, View::Access, View::Argo];

    pub fn title(self) -> &'static str {
        match self {
            View::Map => "GEO MAP",
            View::Galaxy => "GALAXY",
            View::Radar => "RADAR",
            View::Rollouts => "ROLLOUT",
            View::Network => "NET",
            View::Access => "RBAC",
            View::Argo => "ARGO",
        }
    }

    /// Full-width operations views (no node sidebar).
    pub fn is_ops(self) -> bool {
        matches!(self, View::Rollouts | View::Network | View::Access | View::Argo)
    }

    fn ops_slot(self) -> usize {
        match self {
            View::Network => 1,
            View::Access => 2,
            View::Argo => 3,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NetTab {
    Routes,
    Gateways,
    Certs,
    Issuers,
}

impl NetTab {
    pub const ALL: [NetTab; 4] = [NetTab::Routes, NetTab::Gateways, NetTab::Certs, NetTab::Issuers];

    pub fn short(self) -> &'static str {
        match self {
            NetTab::Routes => "ROUTES",
            NetTab::Gateways => "GATEWAYS",
            NetTab::Certs => "CERTS",
            NetTab::Issuers => "ISSUERS",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RbacTab {
    Subjects,
    Roles,
}

impl RbacTab {
    pub const ALL: [RbacTab; 2] = [RbacTab::Subjects, RbacTab::Roles];

    pub fn short(self) -> &'static str {
        match self {
            RbacTab::Subjects => "SUBJECTS",
            RbacTab::Roles => "ROLES",
        }
    }
}

/// An entry of an operations list.
pub enum OpsRef<'a> {
    Rollout(&'a RolloutInfo),
    Route(&'a RouteInfo),
    Gateway(&'a GatewayInfo),
    Cert(&'a CertInfo),
    Issuer(&'a IssuerInfo),
    Subject(Subject, Vec<&'a BindingInfo>),
    Role(&'a RoleInfo),
    App(&'a AppInfo),
}

impl OpsRef<'_> {
    /// Section the entry is listed under.
    pub fn group(&self) -> String {
        match self {
            OpsRef::Rollout(r) => r.namespace.clone(),
            OpsRef::Route(r) => r.namespace.clone(),
            OpsRef::Gateway(g) => g.namespace.clone(),
            OpsRef::Cert(c) => c.namespace.clone(),
            OpsRef::Issuer(i) => if i.cluster { "(cluster)".into() } else { i.namespace.clone() },
            OpsRef::Subject(s, _) => match s.kind.as_str() {
                "ServiceAccount" => format!("sa {}", s.namespace),
                "Group" => "groups".into(),
                _ => "users".into(),
            },
            OpsRef::Role(r) => if r.cluster { "(cluster)".into() } else { r.namespace.clone() },
            OpsRef::App(a) => format!("project {}", a.project),
        }
    }

    pub fn name(&self) -> String {
        match self {
            OpsRef::Rollout(r) => format!("{}/{}", r.kind.short(), r.name),
            OpsRef::Route(r) => format!("{}/{}", r.kind.short(), r.name),
            OpsRef::Gateway(g) => g.name.clone(),
            OpsRef::Cert(c) => c.name.clone(),
            OpsRef::Issuer(i) => i.name.clone(),
            OpsRef::Subject(s, _) => s.name.clone(),
            OpsRef::Role(r) => r.name.clone(),
            OpsRef::App(a) => a.name.clone(),
        }
    }
}

/// An action waiting for y/n.
pub enum Pending {
    Rollout(Action),
    App { namespace: String, name: String, action: AppAction },
}

impl Pending {
    /// (verb, kind, namespace/name)
    pub fn title(&self) -> (String, &'static str, String) {
        match self {
            Pending::Rollout(a) => (a.kind.label().to_uppercase(), a.target.as_str(), format!("{}/{}", a.namespace, a.name)),
            Pending::App { namespace, name, action } => (action.label().to_uppercase(), "Application", format!("{namespace}/{name}")),
        }
    }

    pub fn danger(&self) -> bool {
        match self {
            Pending::Rollout(a) => matches!(a.kind, ActionKind::Abort | ActionKind::Undo | ActionKind::PromoteFull),
            Pending::App { action, .. } => *action == AppAction::SyncPrune,
        }
    }

    fn cmd(&self) -> Cmd {
        match self {
            Pending::Rollout(a) => Cmd::Action(a.clone()),
            Pending::App { namespace, name, action } => Cmd::AppAction { namespace: namespace.clone(), name: name.clone(), action: *action },
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProbeState {
    Pending,
    Ok(Probe),
    Err(String),
}

/// Which cluster is shown: an entry of the discovered list, the demo, or one given by flags
/// that is not in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Active {
    Entry(usize),
    Demo,
    Other,
    None,
}

pub struct Confirm {
    pub pending: Pending,
    pub explain: String,
}

pub struct Toast {
    pub text: String,
    pub ok: bool,
    pub at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nodes,
    Resources,
}

/// What the sidebar resource list shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResKind {
    Pods,
    Deployments,
    ReplicaSets,
    DaemonSets,
    StatefulSets,
    Helm,
}

impl ResKind {
    pub const ALL: [ResKind; 6] =
        [ResKind::Pods, ResKind::Deployments, ResKind::ReplicaSets, ResKind::DaemonSets, ResKind::StatefulSets, ResKind::Helm];

    pub fn short(self) -> &'static str {
        match self {
            ResKind::Pods => "POD",
            ResKind::Deployments => "DEP",
            ResKind::ReplicaSets => "RS",
            ResKind::DaemonSets => "DS",
            ResKind::StatefulSets => "STS",
            ResKind::Helm => "HELM",
        }
    }

    fn workload(self) -> Option<WorkloadKind> {
        match self {
            ResKind::Deployments => Some(WorkloadKind::Deployment),
            ResKind::ReplicaSets => Some(WorkloadKind::ReplicaSet),
            ResKind::DaemonSets => Some(WorkloadKind::DaemonSet),
            ResKind::StatefulSets => Some(WorkloadKind::StatefulSet),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Res<'a> {
    Pod(&'a PodInfo),
    Workload(&'a WorkloadInfo),
    Helm(&'a HelmRelease),
}

impl<'a> Res<'a> {
    pub fn namespace(&self) -> &'a str {
        match self {
            Res::Pod(p) => &p.namespace,
            Res::Workload(w) => &w.namespace,
            Res::Helm(h) => &h.namespace,
        }
    }

    pub fn name(&self) -> &'a str {
        match self {
            Res::Pod(p) => &p.name,
            Res::Workload(w) => &w.name,
            Res::Helm(h) => &h.name,
        }
    }

    pub fn health(&self) -> Health {
        match self {
            Res::Pod(p) => p.health(),
            Res::Workload(w) => w.health(),
            Res::Helm(h) => h.health(),
        }
    }
}

/// A line of the resource list: namespace section header or an entry.
pub enum Row<'a> {
    Header { namespace: &'a str, total: usize, bad: usize, warn: usize },
    Item(Res<'a>),
}

/// Map camera: center (lon, lat) and zoom (1 = whole world wide).
#[derive(Clone, Copy)]
pub struct Camera {
    pub lon: f64,
    pub lat: f64,
    pub zoom: f64,
}

impl Camera {
    pub const WORLD: Camera = Camera { lon: 10.0, lat: 15.0, zoom: 1.0 };

    fn approach(&mut self, target: Camera, k: f64) {
        self.lon += (target.lon - self.lon) * k;
        self.lat += (target.lat - self.lat) * k;
        self.zoom += (target.zoom - self.zoom) * k;
    }
}

pub struct LogView {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub loading: bool,
    /// Pod logs (vs an info popup): only logs get the LOGS prefix and auto-scroll to the end.
    pub is_log: bool,
}

pub struct App {
    pub snap: Snapshot,
    pub view: View,
    pub focus: Focus,
    pub node_idx: usize,
    pub res_idx: usize,
    pub res_kind: ResKind,
    /// Selected entry of each operations view (rollouts, network, access).
    pub ops_idx: [usize; 4],
    pub net_tab: NetTab,
    pub rbac_tab: RbacTab,
    /// Show system:* subjects and roles in the RBAC view.
    pub show_system: bool,
    pub confirm: Option<Confirm>,
    pub toast: Option<Toast>,
    /// Who-can query being typed in the RBAC view.
    pub query: Option<String>,
    pub read_only: bool,
    pub dry_run: bool,
    /// Clusters found in ~/.kube and $KUBECONFIG, their probe results, and the picker cursor
    /// (rows: the clusters, then the demo).
    pub clusters: Vec<ClusterEntry>,
    pub probes: Vec<ProbeState>,
    pub picker: Option<usize>,
    pub active: Active,
    /// Label of the cluster being connected to.
    pub switching: Option<String>,
    pub cam: Camera,
    pub cam_target: Camera,
    /// Map canvas inner size (cols, rows) from the last frame, for fit-to-nodes.
    pub map_size: (u16, u16),
    pub logs: Option<LogView>,
    pub quit: bool,
    /// False until the first snapshot (good or failed) arrives.
    pub synced: bool,
    fitted_once: bool,
    started: Instant,
    cmds: UnboundedSender<Cmd>,
}

impl App {
    pub fn new(snap: Snapshot, cmds: UnboundedSender<Cmd>) -> Self {
        Self {
            snap,
            view: View::Map,
            focus: Focus::Nodes,
            node_idx: 0,
            res_idx: 0,
            res_kind: ResKind::Pods,
            ops_idx: [0; 4],
            net_tab: NetTab::Routes,
            rbac_tab: RbacTab::Subjects,
            show_system: false,
            confirm: None,
            toast: None,
            query: None,
            read_only: false,
            dry_run: false,
            clusters: vec![],
            probes: vec![],
            picker: None,
            active: Active::None,
            switching: None,
            cam: Camera::WORLD,
            cam_target: Camera::WORLD,
            map_size: (120, 40),
            logs: None,
            quit: false,
            synced: false,
            fitted_once: false,
            started: Instant::now(),
            cmds,
        }
    }

    /// Seconds since start — the clock every animation runs on.
    pub fn t(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn tick(&mut self) {
        self.cam.approach(self.cam_target, 0.12);
    }

    pub fn set_snapshot(&mut self, snap: Snapshot) {
        let selected = self.selected_node().map(|n| n.name.clone());
        let selected_res = self.selected_res_any().map(|r| (r.namespace().to_string(), r.name().to_string()));
        let selected_ops = self.selected_ops().map(|o| (o.group(), o.name()));
        let mut snap = snap;
        // Until RBAC is loaded the installer cannot be told apart; the probe already knows.
        if snap.rbac.bindings.is_empty()
            && let Active::Entry(i) = self.active
            && let Some(ProbeState::Ok(p)) = self.probes.get(i)
        {
            snap.infra = p.infra.clone();
        }
        self.snap = snap;
        if self.view.is_ops() {
            let items = self.ops_items();
            let slot = self.view.ops_slot();
            self.ops_idx[slot] = selected_ops
                .and_then(|key| items.iter().position(|o| (o.group(), o.name()) == key))
                .unwrap_or(self.ops_idx[slot])
                .min(items.len().saturating_sub(1));
        }
        self.synced = true;
        self.node_idx = selected
            .and_then(|name| self.snap.nodes.iter().position(|n| n.name == name))
            .unwrap_or(self.node_idx)
            .min(self.snap.nodes.len().saturating_sub(1));
        // Keep the cursor on the same object even when health re-sorts the list.
        let items = self.resources();
        self.res_idx = selected_res
            .and_then(|(ns, name)| items.iter().position(|r| r.namespace() == ns && r.name() == name))
            .unwrap_or(self.res_idx)
            .min(items.len().saturating_sub(1));
        if !self.fitted_once && self.snap.nodes.iter().any(|n| n.geo.is_some()) {
            self.fitted_once = true;
            self.fit();
        }
    }

    pub fn set_logs(&mut self, title: String, lines: Result<Vec<String>, String>) {
        let lines = lines.unwrap_or_else(|e| vec![format!("error: {e}")]);
        let scroll = lines.len().saturating_sub(1);
        self.logs = Some(LogView { title, lines, scroll, loading: false, is_log: true });
    }

    pub fn selected_node(&self) -> Option<&crate::model::NodeInfo> {
        self.snap.nodes.get(self.node_idx)
    }

    /// Live usage of a node, when metrics-server is there.
    pub fn node_usage(&self, node: &str) -> Option<Usage> {
        self.snap.metrics.as_ref()?.nodes.get(node).copied()
    }

    pub fn pod_usage(&self, p: &PodInfo) -> Option<Usage> {
        self.snap.metrics.as_ref()?.pods.get(&(p.namespace.clone(), p.name.clone())).copied()
    }

    /// Sum of requests of the pods scheduled on a node (finished pods excluded).
    pub fn node_requests(&self, node: &str) -> Usage {
        let mut u = Usage::default();
        for p in self.snap.pods_on(node) {
            if p.phase != "Succeeded" && p.phase != "Failed" {
                u += p.requests;
            }
        }
        u
    }

    pub fn selected_pods(&self) -> Vec<&PodInfo> {
        self.selected_node().map(|n| self.snap.pods_on(&n.name)).unwrap_or_default()
    }

    /// Entries of the current list kind, sorted by namespace, then worst health, then name.
    /// Pods are those of the selected node; everything else is cluster-wide.
    pub fn resources(&self) -> Vec<Res<'_>> {
        let mut items: Vec<Res> = match self.res_kind {
            ResKind::Pods => self.selected_pods().into_iter().map(Res::Pod).collect(),
            ResKind::Helm => self.snap.helm.iter().map(Res::Helm).collect(),
            kind => {
                let wk = kind.workload();
                self.snap
                    .workloads
                    .iter()
                    .filter(|w| Some(w.kind) == wk && !w.is_stale())
                    .map(Res::Workload)
                    .collect()
            }
        };
        items.sort_by(|a, b| (a.namespace(), a.health(), a.name()).cmp(&(b.namespace(), b.health(), b.name())));
        items
    }

    pub fn rows(&self) -> Vec<Row<'_>> {
        let items = self.resources();
        let mut rows = vec![];
        let mut i = 0;
        while i < items.len() {
            let ns = items[i].namespace();
            let group: Vec<_> = items[i..].iter().take_while(|r| r.namespace() == ns).copied().collect();
            rows.push(Row::Header {
                namespace: ns,
                total: group.len(),
                bad: group.iter().filter(|r| r.health() == Health::Bad).count(),
                warn: group.iter().filter(|r| r.health() == Health::Warn).count(),
            });
            i += group.len();
            rows.extend(group.into_iter().map(Row::Item));
        }
        rows
    }

    /// ReplicaSets hidden because rollouts scaled them to zero.
    pub fn stale_replicasets(&self) -> usize {
        self.snap.workloads.iter().filter(|w| w.is_stale()).count()
    }

    fn selected_res_any(&self) -> Option<Res<'_>> {
        self.resources().get(self.res_idx).copied()
    }

    pub fn selected_res(&self) -> Option<Res<'_>> {
        if self.focus == Focus::Resources { self.selected_res_any() } else { None }
    }

    pub fn selected_pod(&self) -> Option<&PodInfo> {
        match self.selected_res() {
            Some(Res::Pod(p)) => Some(p),
            _ => None,
        }
    }

    /// Pods to highlight in the galaxy/radar: the pod itself, a workload's pods, a release's namespace.
    pub fn highlighted_pods(&self) -> Vec<(&str, &str)> {
        match self.selected_res() {
            None => vec![],
            Some(Res::Pod(p)) => vec![(&p.namespace, &p.name)],
            Some(Res::Workload(w)) => self.snap.pods_of(w).into_iter().map(|p| (p.namespace.as_str(), p.name.as_str())).collect(),
            Some(Res::Helm(h)) => self
                .snap
                .pods
                .iter()
                .filter(|p| p.namespace == h.namespace)
                .map(|p| (p.namespace.as_str(), p.name.as_str()))
                .collect(),
        }
    }

    /// Entries of the current operations view, grouped (sorted) by section, worst health first.
    pub fn ops_items(&self) -> Vec<OpsRef<'_>> {
        let net = &self.snap.net;
        let rbac = &self.snap.rbac;
        let mut items: Vec<(Health, OpsRef)> = match self.view {
            View::Rollouts => self.snap.rollouts.iter().map(|r| (r.health(), OpsRef::Rollout(r))).collect(),
            View::Network => match self.net_tab {
                NetTab::Routes => net.routes.iter().map(|r| (net.route_health(r), OpsRef::Route(r))).collect(),
                NetTab::Gateways => net
                    .gateways
                    .iter()
                    .map(|g| (if g.programmed { Health::Ok } else { Health::Bad }, OpsRef::Gateway(g)))
                    .collect(),
                // Listener certs mirrored into route namespaces are shown once, at their source.
                NetTab::Certs => net.certs.iter().filter(|c| !c.name.ends_with(')')).map(|c| (c.health(), OpsRef::Cert(c))).collect(),
                NetTab::Issuers => net
                    .issuers
                    .iter()
                    .map(|i| (if i.ready { Health::Ok } else { Health::Bad }, OpsRef::Issuer(i)))
                    .collect(),
            },
            View::Access => match self.rbac_tab {
                RbacTab::Subjects => rbac
                    .subjects()
                    .into_iter()
                    .filter(|(s, _)| self.show_system || !s.is_system())
                    .map(|(s, bindings)| {
                        let mut h = rbac.subject_risk(&bindings).map_or(Health::Ok, |(h, _)| h);
                        if rbac.missing_sa(&s) {
                            h = h.min(Health::Warn);
                        }
                        (h, OpsRef::Subject(s, bindings))
                    })
                    .collect(),
                RbacTab::Roles => rbac
                    .roles
                    .iter()
                    .filter(|r| self.show_system || !r.name.starts_with("system:"))
                    .map(|r| (r.risk().map_or(Health::Ok, |(h, _)| h), OpsRef::Role(r)))
                    .collect(),
            },
            View::Argo => self.snap.apps.iter().map(|a| (a.health(), OpsRef::App(a))).collect(),
            _ => vec![],
        };
        items.sort_by_key(|(h, o)| (o.group(), *h, o.name()));
        items.into_iter().map(|(_, o)| o).collect()
    }

    pub fn ops_health(&self, o: &OpsRef) -> Health {
        let rbac = &self.snap.rbac;
        match o {
            OpsRef::Rollout(r) => r.health(),
            OpsRef::Route(r) => self.snap.net.route_health(r),
            OpsRef::Gateway(g) => if g.programmed { Health::Ok } else { Health::Bad },
            OpsRef::Cert(c) => c.health(),
            OpsRef::Issuer(i) => if i.ready { Health::Ok } else { Health::Bad },
            OpsRef::Subject(s, b) => {
                let h = rbac.subject_risk(b).map_or(Health::Ok, |(h, _)| h);
                if rbac.missing_sa(s) { h.min(Health::Warn) } else { h }
            }
            OpsRef::Role(r) => r.risk().map_or(Health::Ok, |(h, _)| h),
            OpsRef::App(a) => a.health(),
        }
    }

    pub fn ops_selected_idx(&self) -> usize {
        self.ops_idx[self.view.ops_slot()]
    }

    pub fn selected_ops(&self) -> Option<OpsRef<'_>> {
        if !self.view.is_ops() {
            return None;
        }
        self.ops_items().into_iter().nth(self.ops_selected_idx())
    }

    pub fn set_clusters(&mut self, clusters: Vec<ClusterEntry>, active: Active) {
        self.probes = vec![ProbeState::Pending; clusters.len()];
        self.clusters = clusters;
        self.active = active;
    }

    /// Open the cluster picker and (re-)probe every cluster.
    pub fn open_picker(&mut self) {
        let start = match self.active {
            Active::Entry(i) => i,
            Active::Demo => self.clusters.len(),
            _ => self.clusters.iter().position(|c| c.default).unwrap_or(0),
        };
        self.picker = Some(start);
        self.probe_all();
    }

    fn probe_all(&mut self) {
        self.probes = vec![ProbeState::Pending; self.clusters.len()];
        let _ = self.cmds.send(Cmd::Probe(self.clusters.iter().cloned().enumerate().collect()));
    }

    pub fn set_probe(&mut self, index: usize, result: Result<Probe, String>) {
        if let Some(p) = self.probes.get_mut(index) {
            *p = match result {
                Ok(p) => ProbeState::Ok(p),
                Err(e) => ProbeState::Err(e),
            };
        }
    }

    /// The source switched clusters: start from a clean slate.
    pub fn switched(&mut self, cluster: String) {
        self.snap = Snapshot::empty(cluster.clone());
        self.synced = false;
        self.fitted_once = false;
        self.node_idx = 0;
        self.res_idx = 0;
        self.ops_idx = [0; 4];
        self.focus = Focus::Nodes;
        self.logs = None;
        self.confirm = None;
        self.query = None;
        self.cam = Camera::WORLD;
        self.cam_target = Camera::WORLD;
        self.switching = None;
        self.toast = Some(Toast { text: format!("connected to {cluster}"), ok: true, at: Instant::now() });
    }

    pub fn switch_failed(&mut self, err: String) {
        self.switching = None;
        self.toast = Some(Toast { text: format!("could not connect: {err}"), ok: false, at: Instant::now() });
        if self.picker.is_none() {
            self.open_picker();
        }
    }

    fn picker_key(&mut self, key: KeyEvent) {
        let Some(idx) = self.picker else { return };
        let rows = self.clusters.len() + 1;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.picker = Some((idx + 1) % rows),
            KeyCode::Up | KeyCode::Char('k') => self.picker = Some((idx + rows - 1) % rows),
            KeyCode::Char('r') => self.probe_all(),
            KeyCode::Enter => {
                let (target, active, label) = match self.clusters.get(idx) {
                    Some(e) => (Target::Kube(e.clone()), Active::Entry(idx), e.label.clone()),
                    None => (Target::Demo, Active::Demo, "demo".to_string()),
                };
                if active == self.active {
                    self.picker = None;
                    return;
                }
                let _ = self.cmds.send(Cmd::Switch(target));
                self.active = active;
                self.switching = Some(label.clone());
                self.picker = None;
                self.toast = Some(Toast { text: format!("connecting to {label}…"), ok: true, at: Instant::now() });
            }
            // With nothing connected there is nothing behind the picker to go back to.
            KeyCode::Esc | KeyCode::Char('q') if self.active == Active::None => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => self.picker = None,
            _ => {}
        }
    }

    pub fn action_done(&mut self, res: Result<String, String>) {
        let (text, ok) = match res {
            Ok(t) => (t, true),
            Err(e) => (e, false),
        };
        self.toast = Some(Toast { text, ok, at: Instant::now() });
    }

    fn request_action(&mut self, kind: ActionKind) {
        let Some(OpsRef::Rollout(r)) = self.selected_ops() else { return };
        if !r.actions().contains(&kind) {
            let available: Vec<_> = r.actions().iter().map(|a| a.label()).collect();
            self.toast = Some(Toast {
                text: format!("{} not available for {} — try: {}", kind.label(), r.kind.as_str(), available.join(", ")),
                ok: false,
                at: Instant::now(),
            });
            return;
        }
        if self.read_only {
            self.toast = Some(Toast { text: "read-only mode (--read-only): actions disabled".into(), ok: false, at: Instant::now() });
            return;
        }
        let explain = kind.explain(r);
        let action = Action { kind, target: r.kind, namespace: r.namespace.clone(), name: r.name.clone() };
        self.confirm = Some(Confirm { pending: Pending::Rollout(action), explain });
    }

    fn request_app_action(&mut self, action: AppAction) {
        let Some(OpsRef::App(a)) = self.selected_ops() else { return };
        if self.read_only {
            self.toast = Some(Toast { text: "read-only mode (--read-only): actions disabled".into(), ok: false, at: Instant::now() });
            return;
        }
        let pending = Pending::App { namespace: a.namespace.clone(), name: a.name.clone(), action };
        let mut explain = action.explain().to_string();
        if matches!(action, AppAction::Sync | AppAction::SyncPrune) {
            let drift = a.drift().len();
            explain = format!("{explain} — {drift} resource(s) out of sync");
            if a.running() {
                explain.push_str("; an operation is running right now, the sync will be refused");
            }
        }
        // Refresh only re-reads Git: no need to ask.
        if matches!(action, AppAction::Refresh | AppAction::HardRefresh) {
            let _ = self.cmds.send(pending.cmd());
            self.toast = Some(Toast { text: format!("{}…", action.label()), ok: true, at: Instant::now() });
            return;
        }
        self.confirm = Some(Confirm { pending, explain });
    }

    fn cycle_tab(&mut self, step: isize) {
        let rot = |i: usize, n: usize| (i as isize + step).rem_euclid(n as isize) as usize;
        match self.view {
            View::Network => {
                let i = NetTab::ALL.iter().position(|t| *t == self.net_tab).unwrap_or(0);
                self.net_tab = NetTab::ALL[rot(i, NetTab::ALL.len())];
            }
            View::Access => {
                let i = RbacTab::ALL.iter().position(|t| *t == self.rbac_tab).unwrap_or(0);
                self.rbac_tab = RbacTab::ALL[rot(i, RbacTab::ALL.len())];
            }
            _ => return,
        }
        self.ops_idx[self.view.ops_slot()] = 0;
    }

    /// `verb resource[.group] [namespace]`, e.g. `get secrets`, `create pods/exec shop`, `patch deployments.apps`.
    fn run_query(&mut self, q: &str) {
        let parts: Vec<&str> = q.split_whitespace().collect();
        let (verb, resource) = match parts.as_slice() {
            [v, r, ..] => (*v, *r),
            _ => {
                self.toast = Some(Toast { text: "who-can: type `verb resource [namespace]`, e.g. `get secrets`".into(), ok: false, at: Instant::now() });
                return;
            }
        };
        let (resource, group) = match resource.split_once('.') {
            Some((r, g)) => (r, g),
            None => (resource, ""),
        };
        let namespace = parts.get(2).copied();
        let who = self.snap.rbac.who_can(verb, resource, group, namespace);
        let mut lines = vec![
            format!("who can {verb} {resource}{}{}", if group.is_empty() { String::new() } else { format!(".{group}") }, namespace.map(|n| format!(" in {n}")).unwrap_or_default()),
            "(from RoleBindings/ClusterRoleBindings in the cluster; resourceNames-restricted rules are skipped)".into(),
            String::new(),
        ];
        if who.is_empty() {
            lines.push("nobody (besides system:masters, which bypasses RBAC)".into());
        }
        for (s, via) in who {
            let tag = if s.is_system() { "  [system]" } else { "" };
            lines.push(format!("{:<48} via {via}{tag}", s.label()));
        }
        self.logs = Some(LogView { title: format!("who-can {q}"), lines, scroll: 0, loading: false, is_log: false });
    }

    /// Key handling for the operations views; returns false when the key is not theirs.
    fn on_ops_key(&mut self, key: KeyEvent) -> bool {
        if let Some(q) = &mut self.query {
            match key.code {
                KeyCode::Esc => self.query = None,
                KeyCode::Enter => {
                    let q = std::mem::take(q);
                    self.query = None;
                    self.run_query(&q);
                }
                KeyCode::Backspace => drop(q.pop()),
                KeyCode::Char(c) => q.push(c),
                _ => {}
            }
            return true;
        }
        let len = self.ops_items().len();
        let slot = self.view.ops_slot();
        let mv = |idx: &mut usize, d: isize| {
            if len > 0 {
                *idx = (*idx as isize + d).rem_euclid(len as isize) as usize;
            }
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => mv(&mut self.ops_idx[slot], 1),
            KeyCode::Up | KeyCode::Char('k') => mv(&mut self.ops_idx[slot], -1),
            KeyCode::PageDown => mv(&mut self.ops_idx[slot], 10),
            KeyCode::PageUp => mv(&mut self.ops_idx[slot], -10),
            KeyCode::Char('t') => self.cycle_tab(1),
            KeyCode::Char('T') => self.cycle_tab(-1),
            KeyCode::Esc => self.view = View::Map,
            KeyCode::Enter => {
                if let Some(o) = self.selected_ops() {
                    let title = format!("{} {}", o.group(), o.name());
                    let lines = crate::ui::ops::detail_lines(self, &o, 120)
                        .into_iter()
                        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                        .collect();
                    self.logs = Some(LogView { title, lines, scroll: 0, loading: false, is_log: false });
                }
            }
            KeyCode::Char('s') if self.view == View::Access => {
                self.show_system = !self.show_system;
                self.ops_idx[slot] = 0;
            }
            KeyCode::Char('/') if self.view == View::Access => self.query = Some(String::new()),
            KeyCode::Char(c) if self.view == View::Argo => match AppAction::ALL.iter().find(|a| a.key() == c) {
                Some(a) => self.request_app_action(*a),
                None => return false,
            },
            KeyCode::Char(c) if self.view == View::Rollouts => {
                let kind = match c {
                    'R' => ActionKind::Restart,
                    'p' => match self.selected_ops() {
                        Some(OpsRef::Rollout(r)) if r.paused => ActionKind::Resume,
                        _ => ActionKind::Pause,
                    },
                    'u' => ActionKind::Undo,
                    'P' => ActionKind::Promote,
                    'F' => ActionKind::PromoteFull,
                    'A' => ActionKind::Abort,
                    'Y' => ActionKind::Retry,
                    _ => return false,
                };
                self.request_action(kind);
            }
            _ => return false,
        }
        true
    }

    fn cycle_kind(&mut self, step: isize) {
        let i = ResKind::ALL.iter().position(|k| *k == self.res_kind).unwrap_or(0) as isize;
        self.res_kind = ResKind::ALL[(i + step).rem_euclid(ResKind::ALL.len() as isize) as usize];
        self.res_idx = 0;
    }

    /// Zoom so every placed node is visible.
    fn fit(&mut self) {
        let pts: Vec<_> = self.snap.nodes.iter().filter_map(|n| n.geo).collect();
        if pts.is_empty() {
            self.cam_target = Camera::WORLD;
            return;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (180.0f64, -180.0f64, 90.0f64, -90.0f64);
        for (x, y) in pts {
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        let (w, h) = (self.map_size.0.max(1) as f64, self.map_size.1.max(1) as f64 * 2.0);
        let span_x = (x1 - x0).max(6.0) * 1.5;
        let span_y = (y1 - y0).max(3.0) * 1.5 * w / h;
        let span = span_x.max(span_y);
        self.cam_target = Camera { lon: (x0 + x1) / 2.0, lat: (y0 + y1) / 2.0, zoom: (360.0 / span).clamp(1.0, 40.0) };
    }

    fn zoom_by(&mut self, k: f64) {
        let mut target = self.cam_target;
        if let Some((lon, lat)) = self.selected_node().and_then(|n| n.geo) {
            target.lon = lon;
            target.lat = lat;
        }
        target.zoom = (target.zoom * k).clamp(1.0, 60.0);
        self.cam_target = target;
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        let step = 360.0 / self.cam_target.zoom * 0.1;
        self.cam_target.lon = (self.cam_target.lon + dx * step).clamp(-180.0, 180.0);
        self.cam_target.lat = (self.cam_target.lat + dy * step).clamp(-80.0, 85.0);
    }

    fn move_selection(&mut self, delta: isize) {
        let (idx, len) = match self.focus {
            Focus::Nodes => (&mut self.node_idx, self.snap.nodes.len()),
            Focus::Resources => {
                let len = self.resources().len();
                (&mut self.res_idx, len)
            }
        };
        if len > 0 {
            *idx = (*idx as isize + delta).rem_euclid(len as isize) as usize;
        }
        if self.focus == Focus::Nodes {
            if self.res_kind == ResKind::Pods {
                self.res_idx = 0;
            }
            if self.cam_target.zoom > 1.5
                && let Some((lon, lat)) = self.selected_node().and_then(|n| n.geo)
            {
                self.cam_target.lon = lon;
                self.cam_target.lat = lat;
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        if let Some(logs) = &mut self.logs {
            let page = 20;
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => self.logs = None,
                KeyCode::Down | KeyCode::Char('j') => logs.scroll = (logs.scroll + 1).min(logs.lines.len().saturating_sub(1)),
                KeyCode::Up | KeyCode::Char('k') => logs.scroll = logs.scroll.saturating_sub(1),
                KeyCode::PageDown => logs.scroll = (logs.scroll + page).min(logs.lines.len().saturating_sub(1)),
                KeyCode::PageUp => logs.scroll = logs.scroll.saturating_sub(page),
                KeyCode::Char('g') => logs.scroll = 0,
                KeyCode::Char('G') => logs.scroll = logs.lines.len().saturating_sub(1),
                _ => {}
            }
            return;
        }
        if let Some(c) = &self.confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let _ = self.cmds.send(c.pending.cmd());
                    self.toast = Some(Toast { text: format!("sending {}…", c.pending.title().0.to_lowercase()), ok: true, at: Instant::now() });
                    self.confirm = None;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => self.confirm = None,
                _ => {}
            }
            return;
        }
        if self.view.is_ops() && self.on_ops_key(key) {
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Esc if self.focus == Focus::Resources => self.focus = Focus::Nodes,
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => self.open_picker(),
            KeyCode::Char('1') => self.view = View::Map,
            KeyCode::Char('2') => self.view = View::Galaxy,
            KeyCode::Char('3') => self.view = View::Radar,
            KeyCode::Char('4') => self.view = View::Rollouts,
            KeyCode::Char('5') => self.view = View::Network,
            KeyCode::Char('6') => self.view = View::Access,
            KeyCode::Char('7') => self.view = View::Argo,
            KeyCode::Tab => {
                let i = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
                self.view = View::ALL[(i + 1) % View::ALL.len()];
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Right | KeyCode::Enter if self.focus == Focus::Nodes => {
                self.focus = Focus::Resources;
                self.res_idx = 0;
            }
            KeyCode::Left if self.focus == Focus::Resources => self.focus = Focus::Nodes,
            KeyCode::Char('t') => self.cycle_kind(1),
            KeyCode::Char('T') => self.cycle_kind(-1),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('l') => self.open_logs(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom_by(1.6),
            KeyCode::Char('-') => self.zoom_by(1.0 / 1.6),
            KeyCode::Char('0') => self.cam_target = Camera::WORLD,
            KeyCode::Char('f') => self.fit(),
            KeyCode::Char('H') => self.pan(-1.0, 0.0),
            KeyCode::Char('L') => self.pan(1.0, 0.0),
            KeyCode::Char('K') => self.pan(0.0, 1.0),
            KeyCode::Char('J') => self.pan(0.0, -1.0),
            KeyCode::Char('r') => {
                let _ = self.cmds.send(Cmd::Refresh);
            }
            _ => {}
        }
    }

    /// Enter on the list: pod logs, a workload's pods, or a Helm release summary.
    fn open_selected(&mut self) {
        let popup = match self.selected_res() {
            None => return,
            Some(Res::Pod(_)) => return self.open_logs(),
            Some(Res::Workload(w)) => {
                let pods = self.snap.pods_of(w);
                let mut lines = vec![
                    format!("{} {}/{}", w.kind.as_str(), w.namespace, w.name),
                    format!("ready    {}/{}", w.ready, w.desired),
                    format!("image    {}", w.image),
                ];
                if let Some((k, n)) = &w.owner {
                    lines.push(format!("owner    {k}/{n}"));
                }
                lines.push(String::new());
                lines.push(format!("{} pods", pods.len()));
                lines.extend(pods.iter().map(|p| {
                    format!(
                        "  {:<20} {:<52} {:<16} r:{}",
                        p.status(),
                        p.name,
                        p.node.as_deref().unwrap_or("<unscheduled>"),
                        p.restarts
                    )
                }));
                (format!("{} {}", w.kind.as_str(), w.name), lines)
            }
            Some(Res::Helm(h)) => {
                let ns_pods: Vec<_> = self.snap.pods.iter().filter(|p| p.namespace == h.namespace).collect();
                let bad = ns_pods.iter().filter(|p| p.health() == Health::Bad).count();
                let workloads: Vec<_> =
                    self.snap.workloads.iter().filter(|w| w.namespace == h.namespace && !w.is_stale() && w.kind != WorkloadKind::ReplicaSet).collect();
                let mut lines = vec![
                    format!("release      {}", h.name),
                    format!("namespace    {}", h.namespace),
                    format!("chart        {}", h.chart),
                    format!("app version  {}", h.app_version),
                    format!("revision     {}", h.revision),
                    format!("status       {}", h.status),
                    format!("updated      {}", h.updated),
                    String::new(),
                    format!("namespace has {} pods ({} failing) and {} workloads:", ns_pods.len(), bad, workloads.len()),
                ];
                lines.extend(workloads.iter().map(|w| format!("  {:<12} {:<48} {}/{}", w.kind.as_str(), w.name, w.ready, w.desired)));
                (format!("helm {}", h.name), lines)
            }
        };
        let (title, lines) = popup;
        self.logs = Some(LogView { title, lines, scroll: 0, loading: false, is_log: false });
    }

    fn open_logs(&mut self) {
        let Some(pod) = self.selected_pod() else { return };
        let cmd = Cmd::Logs {
            namespace: pod.namespace.clone(),
            pod: pod.name.clone(),
            container: pod.containers.first().cloned(),
        };
        let title = format!("{}/{}", pod.namespace, pod.name);
        let _ = self.cmds.send(cmd);
        self.logs = Some(LogView { title, lines: vec![], scroll: 0, loading: true, is_log: true });
    }
}
