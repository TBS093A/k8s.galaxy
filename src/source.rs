//! Background data feed: polls the cluster (or the demo simulator) and pushes snapshots to the UI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Event, Node, Pod, PodSpec, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{ListParams, LogParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::cloud::{self, CloudConfig, CloudInstance};
use crate::demo::Sim;
use crate::geo::Resolver;
use crate::gitops::{self, AppAction, AppInfo};
use crate::infra;
use crate::kubeconfigs::{self, ClusterEntry, Probe};
use crate::metrics::{self, Metrics, Usage};
use crate::model::{EventInfo, HelmRelease, NodeInfo, PodInfo, Snapshot, WorkloadInfo, WorkloadKind};
use crate::ops::{self, Action, Crds, NetInfo, RbacInfo};

/// Traffic, RBAC and Helm change rarely and are bigger lists: they refresh in a background task
/// on this clock, so a slow API server never holds up the live view.
const SLOW_INTERVAL: Duration = Duration::from_secs(20);
/// Argo CD Applications and metrics: same background task, faster clock (sync status should move quickly).
const LIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Optional CRDs (Argo Rollouts, Gateway API, cert-manager) can be installed while we run.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(300);

/// What the background task last fetched.
#[derive(Default)]
pub struct SlowState {
    net: NetInfo,
    rbac: RbacInfo,
    helm: Vec<HelmRelease>,
    storage: crate::storage::StorageInfo,
    cleanup: Vec<crate::cleanup::Finding>,
    pdbs: Vec<crate::model::PdbInfo>,
    apps: Vec<AppInfo>,
    metrics: Option<Metrics>,
    /// Errors of the 20 s lane and of the 5 s lane (apps, metrics), kept apart so each lane replaces its own.
    errors: Vec<EventInfo>,
    live_errors: Vec<EventInfo>,
    crds: Crds,
}

type Shared = Arc<Mutex<SlowState>>;

pub enum Cmd {
    Refresh,
    Action(Action),
    AppAction {
        namespace: String,
        name: String,
        action: AppAction,
    },
    Logs {
        namespace: String,
        pod: String,
        container: Option<String>,
    },
    /// Connect to another cluster (the current one is dropped).
    Switch(Target),
    /// Probe these clusters (index in the app's list, entry).
    Probe(Vec<(usize, ClusterEntry)>),
    /// `du`/`df` of hostPath/local volumes on their nodes.
    Measure(Vec<crate::storage::Target>),
    /// Delete a cleanup candidate.
    Delete(crate::cleanup::Finding),
    Cordon {
        node: String,
        on: bool,
    },
    Drain(crate::nodeops::DrainPlan),
}

#[derive(Clone, Debug)]
pub enum Target {
    Demo,
    Kube(ClusterEntry),
}

pub enum Msg {
    Snapshot(Box<Snapshot>),
    ActionDone(Result<String, String>),
    Logs {
        title: String,
        lines: Result<Vec<String>, String>,
    },
    /// Now showing this cluster (fresh state follows).
    Switched {
        cluster: String,
    },
    SwitchFailed(String),
    Probed {
        index: usize,
        result: Result<Probe, String>,
    },
    Measured(Result<crate::storage::Measurements, String>),
}

/// Background task handle that stops the task when the source it belongs to is dropped.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[allow(clippy::large_enum_variant)] // one long-lived value
pub enum Source {
    Kube {
        client: Client,
        cluster: String,
        /// API server URL and version, for telling managed services from self-managed clusters.
        server: String,
        version: String,
        slow: Shared,
        wake: Arc<Notify>,
        dry_run: bool,
        /// Watch streams + caches of the frequently changing lists.
        watches: crate::watch::Watches,
        /// Set after the first snapshot has waited for the caches to fill.
        primed: bool,
        _task: AbortOnDrop,
    },
    Demo(Sim),
    /// No cluster chosen yet (the cluster picker is open).
    Idle,
}

impl Source {
    pub async fn connect(kubeconfig: Option<PathBuf>, context: Option<String>) -> Result<Self> {
        let kc = match &kubeconfig {
            Some(path) => Kubeconfig::read_from(path).with_context(|| format!("reading {}", path.display()))?,
            None => Kubeconfig::read().context("reading kubeconfig ($KUBECONFIG / ~/.kube/config)")?,
        };
        let cluster = context
            .clone()
            .or_else(|| kc.current_context.clone())
            .or_else(|| kc.contexts.first().map(|c| c.name.clone()))
            .unwrap_or_else(|| "cluster".into());
        let server = kc
            .contexts
            .iter()
            .find(|c| c.name == cluster)
            .and_then(|c| c.context.as_ref())
            .and_then(|c| kc.clusters.iter().find(|cl| cl.name == c.cluster))
            .and_then(|cl| cl.cluster.as_ref())
            .and_then(|cl| cl.server.clone())
            .unwrap_or_default();
        let opts = KubeConfigOptions { context: Some(cluster.clone()), ..Default::default() };
        let config = Config::from_custom_kubeconfig(kc, &opts).await.context("building kube config")?;
        let client = Client::try_from(config)?;
        Ok(Self::from_client(client, cluster, server).await)
    }

    /// A cluster from the discovered list, shown under its list label.
    pub async fn open(target: &Target) -> Result<Self> {
        match target {
            Target::Demo => Ok(Source::Demo(Sim::new())),
            Target::Kube(e) => {
                let client = kubeconfigs::client_for(&e.file, &e.context, false).await?;
                Ok(Self::from_client(client, e.label.clone(), e.server.clone()).await)
            }
        }
    }

    async fn from_client(client: Client, cluster: String, server: String) -> Self {
        let version = tokio::time::timeout(Duration::from_secs(8), client.apiserver_version())
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|v| v.git_version)
            .unwrap_or_default();
        let slow = Shared::default();
        let wake = Arc::new(Notify::new());
        let task = AbortOnDrop(tokio::spawn(slow_loop(client.clone(), slow.clone(), wake.clone())));
        let watches = crate::watch::Watches::start(&client);
        Source::Kube { client, cluster, server, version, slow, wake, dry_run: false, watches, primed: false, _task: task }
    }

    fn server_version(&self) -> (String, String) {
        match self {
            Source::Kube { server, version, .. } => (server.clone(), version.clone()),
            Source::Demo(_) => ("https://demo.local:6443".into(), "v1.34.1".into()),
            Source::Idle => Default::default(),
        }
    }

    fn dry_run(&self) -> bool {
        matches!(self, Source::Kube { dry_run: true, .. })
    }

    pub fn cluster(&self) -> String {
        match self {
            Source::Kube { cluster, .. } => cluster.clone(),
            Source::Demo(_) => "demo-galaxy".into(),
            Source::Idle => "no cluster selected".into(),
        }
    }

    async fn snapshot(&mut self) -> Result<Snapshot> {
        match self {
            Source::Idle => Ok(Snapshot::empty("no cluster selected".into())),
            Source::Demo(sim) => {
                sim.step();
                Ok(sim.snapshot())
            }
            Source::Kube { client, cluster, slow, watches, primed, .. } => {
                let rollout_crd = slow.lock().unwrap().crds.rollout.clone();
                let lp = ListParams::default();
                // The first snapshot waits for the watch caches; later ones just read them.
                if !*primed {
                    watches.ready(Duration::from_secs(20)).await;
                    *primed = true;
                }
                let watch_errors = watches.errors();
                // Without nodes and pods there is nothing to show: report it like a failed poll.
                if let Some((what, err)) = watch_errors.iter().find(|(w, _)| matches!(*w, "nodes" | "pods"))
                    && watches.nodes.state().is_empty()
                {
                    anyhow::bail!("watching {what}: {err}");
                }
                use crate::watch::items;
                let (nodes, pods, events) = (items(&watches.nodes), items(&watches.pods), items(&watches.events));
                let (deps, rss, dss, stss, crs) = (
                    items(&watches.deployments),
                    items(&watches.replicasets),
                    items(&watches.daemonsets),
                    items(&watches.statefulsets),
                    items(&watches.revisions),
                );
                let argo = match &rollout_crd {
                    Some(ar) => {
                        Api::<kube::api::DynamicObject>::all_with(client.clone(), ar).list(&lp).await.map(|l| l.items).unwrap_or_default()
                    }
                    None => vec![],
                };
                let rollouts = ops::rollouts(&deps, &rss, &stss, &dss, &crs, &argo);
                let mut workloads = vec![];
                for d in deps {
                    let s = d.status.unwrap_or_default();
                    let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
                    let image = first_image(d.spec.map(|s| s.template.spec));
                    workloads.push(workload(WorkloadKind::Deployment, d.metadata, desired, s.ready_replicas.unwrap_or(0), image));
                }
                for r in rss {
                    let s = r.status.unwrap_or_default();
                    let desired = r.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
                    let image = first_image(r.spec.and_then(|s| s.template).map(|t| t.spec));
                    workloads.push(workload(WorkloadKind::ReplicaSet, r.metadata, desired, s.ready_replicas.unwrap_or(0), image));
                }
                for d in dss {
                    let s = d.status.unwrap_or_default();
                    let image = first_image(d.spec.map(|s| s.template.spec));
                    workloads.push(workload(WorkloadKind::DaemonSet, d.metadata, s.desired_number_scheduled, s.number_ready, image));
                }
                for st in stss {
                    let s = st.status.unwrap_or_default();
                    let desired = st.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
                    let image = first_image(st.spec.map(|s| s.template.spec));
                    workloads.push(workload(WorkloadKind::StatefulSet, st.metadata, desired, s.ready_replicas.unwrap_or(0), image));
                }
                let mut warnings: Vec<_> = events.into_iter().filter(|e| e.type_.as_deref() == Some("Warning")).collect();
                warnings.sort_by_key(|e| std::cmp::Reverse(e.last_timestamp.as_ref().map(|t| t.0.to_string())));
                let mut warnings: Vec<EventInfo> = warnings.into_iter().take(50).map(event_info).collect();
                let slow = slow.lock().unwrap();
                warnings.splice(0..0, slow.errors.iter().cloned());
                warnings.splice(
                    0..0,
                    watch_errors.into_iter().map(|(what, e)| EventInfo {
                        reason: "Watch".into(),
                        object: what.into(),
                        message: e,
                        count: 1,
                    }),
                );
                warnings.splice(0..0, slow.live_errors.iter().cloned());
                warnings.splice(0..0, cert_warnings(&slow.net));
                warnings.splice(0..0, volume_warnings(&slow.storage));
                Ok(Snapshot {
                    cluster: cluster.clone(),
                    nodes: nodes.into_iter().map(node_info).collect(),
                    pods: pods.into_iter().map(pod_info).collect(),
                    warnings,
                    workloads,
                    helm: slow.helm.clone(),
                    rollouts,
                    net: slow.net.clone(),
                    rbac: slow.rbac.clone(),
                    caps: slow.crds.caps(),
                    apps: slow.apps.clone(),
                    metrics: slow.metrics.clone(),
                    storage: slow.storage.clone(),
                    costs: vec![],
                    cleanup: slow.cleanup.clone(),
                    pdbs: slow.pdbs.clone(),
                    infra: Default::default(),
                    fetched_at: Instant::now(),
                    error: None,
                })
            }
        }
    }

    async fn measure(&mut self, targets: &[crate::storage::Target]) -> Result<crate::storage::Measurements, String> {
        match self {
            Source::Idle => Err("no cluster selected".into()),
            Source::Demo(sim) => Ok(sim.measure(targets)),
            Source::Kube { client, dry_run, .. } => crate::storage::measure(client, targets, *dry_run).await.map_err(|e| format!("{e:#}")),
        }
    }

    async fn cordon(&mut self, node: &str, on: bool) -> Result<String, String> {
        let what = format!("{} {node}", if on { "cordoned" } else { "uncordoned" });
        match self {
            Source::Idle => Err("no cluster selected".into()),
            Source::Demo(sim) => {
                sim.cordon(node, on);
                Ok(format!("{what} (demo)"))
            }
            Source::Kube { client, dry_run, .. } => crate::nodeops::cordon(client, node, on, *dry_run)
                .await
                .map(|_| if *dry_run { format!("dry-run OK: {what}") } else { what })
                .map_err(|e| format!("{e:#}")),
        }
    }

    async fn delete(&mut self, f: &crate::cleanup::Finding) -> Result<String, String> {
        let what =
            format!("deleted {} {}{}", f.kind, if f.namespace.is_empty() { String::new() } else { format!("{}/", f.namespace) }, f.name);
        match self {
            Source::Idle => Err("no cluster selected".into()),
            Source::Demo(sim) => {
                sim.delete(f);
                Ok(format!("{what} (demo)"))
            }
            Source::Kube { client, dry_run, .. } => crate::cleanup::delete(client, f, *dry_run)
                .await
                .map(|_| if *dry_run { format!("dry-run OK: {what}") } else { what })
                .map_err(|e| format!("{e:#}")),
        }
    }

    fn wake(&self) {
        if let Source::Kube { wake, .. } = self {
            wake.notify_one();
        }
    }

    async fn perform_app(&mut self, namespace: &str, name: &str, action: AppAction) -> Result<String, String> {
        let done = format!("{} Application {namespace}/{name}", action.label());
        match self {
            Source::Idle => Err("no cluster selected".into()),
            Source::Demo(sim) => sim.perform_app(namespace, name, action).map(|_| format!("{done} (demo)")),
            Source::Kube { client, slow, dry_run, .. } => {
                let ar = slow.lock().unwrap().crds.application.clone().ok_or("Argo CD Application CRD not found")?;
                gitops::perform(client, &ar, namespace, name, action, *dry_run)
                    .await
                    .map(|_| if *dry_run { format!("dry-run OK: {done}") } else { done })
                    .map_err(|e| format!("{e:#}"))
            }
        }
    }

    pub fn set_dry_run(&mut self, on: bool) {
        if let Source::Kube { dry_run, .. } = self {
            *dry_run = on;
        }
    }

    async fn perform(&mut self, action: &Action) -> Result<String, String> {
        match self {
            Source::Idle => Err("no cluster selected".into()),
            Source::Demo(sim) => sim.perform(action),
            Source::Kube { client, slow, dry_run, .. } => {
                let crds = slow.lock().unwrap().crds.clone();
                ops::perform(client, &crds, action, *dry_run).await.map_err(|e| format!("{e:#}"))
            }
        }
    }

    async fn logs(&self, namespace: &str, pod: &str, container: Option<String>) -> Result<Vec<String>, String> {
        match self {
            Source::Idle => Err("no cluster selected".into()),
            Source::Demo(sim) => Ok(sim.logs(pod)),
            Source::Kube { client, .. } => {
                let params = LogParams { tail_lines: Some(500), container, ..Default::default() };
                let text = Api::<Pod>::namespaced(client.clone(), namespace).logs(pod, &params).await.map_err(|e| e.to_string())?;
                Ok(text.lines().map(str::to_string).collect())
            }
        }
    }
}

pub struct CloudPoll {
    pub config: CloudConfig,
    pub interval: Duration,
}

pub async fn run(
    mut source: Source,
    resolver: Resolver,
    interval: Duration,
    cloud_poll: CloudPoll,
    mut cmds: UnboundedReceiver<Cmd>,
    tx: Sender<Msg>,
) {
    let mut last_good: Option<Snapshot> = None;
    let mut instances: Vec<CloudInstance> = vec![];
    let mut cloud_errors: Vec<EventInfo> = vec![];
    let mut cloud_at: Option<Instant> = None;
    // Pricing runs in its own task: CLI calls and price lists can take long, the view must not wait.
    let cost_state: Arc<Mutex<(Vec<crate::costs::Costs>, Vec<EventInfo>)>> = Default::default();
    let price_cache = Arc::new(tokio::sync::Mutex::new(crate::cloudprice::PriceCache::default()));
    loop {
        // Nothing to poll while no cluster is chosen: only serve commands (switch, probe).
        if !matches!(source, Source::Idle) {
            let snap = match tokio::time::timeout(Duration::from_secs(25), source.snapshot()).await {
                Ok(Ok(mut snap)) => {
                    // Provider inventories change rarely; refresh them on their own, slower clock.
                    if cloud_at.is_none_or(|t| t.elapsed() >= cloud_poll.interval) {
                        cloud_at = Some(Instant::now());
                        let (list, errors) = cloud::inventory(&cloud_poll.config, &snap.nodes).await;
                        instances = list;
                        cloud_errors = errors
                            .into_iter()
                            .map(|(p, e)| EventInfo {
                                reason: "CloudAPI".into(),
                                object: format!("provider/{p}"),
                                message: e.replace('\n', " "),
                                count: 1,
                            })
                            .collect();
                        // Prices change rarely too: refresh with the inventory (not for the demo).
                        // A run still in progress keeps the cache lock, so runs never overlap.
                        if !matches!(source, Source::Demo(_))
                            && let Ok(cache) = price_cache.clone().try_lock_owned()
                        {
                            let token = cloud_poll.config.hcloud_token.clone();
                            let mut nodes = snap.nodes.clone();
                            nodes.iter_mut().for_each(|n| resolver.apply(n, &instances));
                            let hints: Vec<String> = snap.rbac.bindings.iter().filter(|b| b.cluster).map(|b| b.name.clone()).collect();
                            let (server, version) = source.server_version();
                            let infra = infra::classify(&server, &version, &nodes, &hints);
                            let storage = snap.storage.clone();
                            let state = cost_state.clone();
                            tokio::spawn(async move {
                                let mut cache = cache;
                                let (costs, errors) = fetch_costs(token, &nodes, &storage, &infra, &mut cache).await;
                                *state.lock().unwrap() = (costs, errors);
                            });
                        }
                    }
                    snap.warnings.splice(0..0, cloud_errors.iter().cloned());
                    {
                        let (costs, errors) = &*cost_state.lock().unwrap();
                        snap.warnings.splice(0..0, errors.iter().cloned());
                        if snap.costs.is_empty() {
                            snap.costs = costs.clone();
                        }
                    }
                    snap.nodes.iter_mut().for_each(|n| resolver.apply(n, &instances));
                    snap.nodes.sort_by(|a, b| (&a.place, &a.name).cmp(&(&b.place, &b.name)));
                    let (server, version) = source.server_version();
                    let hints: Vec<String> = snap.rbac.bindings.iter().filter(|b| b.cluster).map(|b| b.name.clone()).collect();
                    snap.infra = infra::classify(&server, &version, &snap.nodes, &hints);
                    last_good = Some(snap.clone());
                    snap
                }
                failure => {
                    let err = match failure {
                        Ok(Err(e)) => format!("{e:#}"),
                        _ => "timeout talking to API server".into(),
                    };
                    let mut snap = last_good.clone().unwrap_or_else(|| Snapshot::empty(source.cluster()));
                    snap.error = Some(err);
                    snap
                }
            };
            if tx.send(Msg::Snapshot(Box::new(snap))).is_err() {
                return;
            }
        }

        // Wait for the next tick, serving commands meanwhile.
        let deadline = tokio::time::Instant::now() + interval;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                cmd = cmds.recv() => match cmd {
                    None => return,
                    Some(Cmd::Refresh) => {
                        source.wake();
                        break;
                    }
                    Some(Cmd::Switch(target)) => {
                        let dry_run = source.dry_run();
                        match Source::open(&target).await {
                            Ok(mut new) => {
                                new.set_dry_run(dry_run);
                                source = new;
                                last_good = None;
                                instances.clear();
                                *cost_state.lock().unwrap() = Default::default();
                                cloud_errors.clear();
                                cloud_at = None;
                                if tx.send(Msg::Switched { cluster: source.cluster() }).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                if tx.send(Msg::SwitchFailed(format!("{e:#}"))).is_err() {
                                    return;
                                }
                            }
                        }
                        break;
                    }
                    Some(Cmd::Probe(entries)) => {
                        for (index, entry) in entries {
                            let (tx, cfg, resolver) = (tx.clone(), cloud_poll.config.clone(), resolver.clone());
                            tokio::spawn(async move {
                                let result = probe(&entry, &cfg, &resolver).await.map_err(|e| format!("{e:#}"));
                                let _ = tx.send(Msg::Probed { index, result });
                            });
                        }
                    }
                    Some(Cmd::Cordon { node, on }) => {
                        let res = source.cordon(&node, on).await;
                        if tx.send(Msg::ActionDone(res)).is_err() {
                            return;
                        }
                        break;
                    }
                    Some(Cmd::Drain(plan)) => match &mut source {
                        Source::Kube { client, dry_run, .. } => {
                            // A drain waits on PodDisruptionBudgets for minutes: run it on the side.
                            let (client, dry_run, tx) = (client.clone(), *dry_run, tx.clone());
                            tokio::spawn(async move {
                                let progress = |m: String| drop(tx.send(Msg::ActionDone(Ok(m))));
                                progress(format!("draining {}…", plan.node));
                                let res = crate::nodeops::drain(&client, &plan, dry_run, progress).await.map_err(|e| format!("{e:#}"));
                                let _ = tx.send(Msg::ActionDone(res));
                            });
                        }
                        Source::Demo(sim) => {
                            let res = sim.drain(&plan);
                            if tx.send(Msg::ActionDone(res)).is_err() {
                                return;
                            }
                        }
                        Source::Idle => {}
                    },
                    Some(Cmd::Delete(finding)) => {
                        let res = source.delete(&finding).await;
                        source.wake();
                        if tx.send(Msg::ActionDone(res)).is_err() {
                            return;
                        }
                    }
                    Some(Cmd::Measure(targets)) => {
                        let res = source.measure(&targets).await;
                        if tx.send(Msg::Measured(res)).is_err() {
                            return;
                        }
                    }
                    Some(Cmd::AppAction { namespace, name, action }) => {
                        let res = source.perform_app(&namespace, &name, action).await;
                        source.wake();
                        if tx.send(Msg::ActionDone(res)).is_err() {
                            return;
                        }
                    }
                    Some(Cmd::Action(action)) => {
                        let res = source.perform(&action).await;
                        if tx.send(Msg::ActionDone(res)).is_err() {
                            return;
                        }
                        break;
                    }
                    Some(Cmd::Logs { namespace, pod, container }) => {
                        let lines = source.logs(&namespace, &pod, container).await;
                        let title = format!("{namespace}/{pod}");
                        if tx.send(Msg::Logs { title, lines }).is_err() {
                            return;
                        }
                    }
                },
            }
        }
    }
}

/// PodDisruptionBudgets with how many disruptions each allows right now.
async fn pdbs(client: &Client) -> Result<Vec<crate::model::PdbInfo>> {
    use k8s_openapi::api::policy::v1::PodDisruptionBudget;
    let list =
        Api::<PodDisruptionBudget>::all(client.clone()).list(&ListParams::default()).await.context("listing poddisruptionbudgets")?;
    Ok(list
        .items
        .into_iter()
        .map(|p| crate::model::PdbInfo {
            namespace: p.metadata.namespace.unwrap_or_default(),
            name: p.metadata.name.unwrap_or_default(),
            selector: p.spec.and_then(|s| s.selector).and_then(|s| s.match_labels).unwrap_or_default(),
            allowed: p.status.and_then(|s| s.disruptions_allowed).unwrap_or(0),
        })
        .collect())
}

/// Hetzner's bill from its API, AWS/GCP/Azure nodes priced from the providers' price lists.
async fn fetch_costs(
    token: Option<String>,
    nodes: &[NodeInfo],
    storage: &crate::storage::StorageInfo,
    infra: &crate::infra::Infra,
    cache: &mut crate::cloudprice::PriceCache,
) -> (Vec<crate::costs::Costs>, Vec<EventInfo>) {
    let mut costs = vec![];
    let mut errors = vec![];
    let error = |who: &str, e: String| EventInfo { reason: "CloudAPI".into(), object: format!("{who}/costs"), message: e, count: 1 };
    if let Some(token) = token {
        match crate::costs::hetzner(&token).await {
            Ok(c) if !c.items.is_empty() => costs.push(c),
            Ok(_) => {}
            Err(e) => errors.push(error("hetzner", format!("{e:#}"))),
        }
    }
    for (provider, res) in crate::cloudprice::costs(nodes, storage, infra, cache).await {
        match res {
            Ok(c) => {
                errors.extend(c.notes.iter().map(|n| error(&provider.to_lowercase(), n.clone())));
                costs.push(c);
            }
            Err(e) => errors.push(error(&provider.to_lowercase(), format!("{e:#}"))),
        }
    }
    (costs, errors)
}

/// Reachability, version and hosting of one cluster. Nodes are matched against the cloud
/// inventories first, so self-managed clusters on cloud VMs without a cloud controller (no
/// providerID) are still recognised as such.
async fn probe(entry: &ClusterEntry, cfg: &CloudConfig, resolver: &Resolver) -> Result<Probe> {
    let (version, nodes, hints) =
        tokio::time::timeout(Duration::from_secs(12), kubeconfigs::probe_raw(entry)).await.map_err(|_| anyhow::anyhow!("timed out"))??;
    let mut nodes: Vec<NodeInfo> = nodes.into_iter().map(node_info).collect();
    let (instances, _) = tokio::time::timeout(Duration::from_secs(15), cloud::inventory(cfg, &nodes)).await.unwrap_or_default();
    nodes.iter_mut().for_each(|n| resolver.apply(n, &instances));
    Ok(Probe { nodes: nodes.len(), infra: infra::classify(&entry.server, &version, &nodes, &hints), version })
}

fn node_info(node: Node) -> NodeInfo {
    let labels: BTreeMap<String, String> = node.metadata.labels.unwrap_or_default();
    let annotations = node.metadata.annotations.unwrap_or_default();
    let status = node.status.unwrap_or_default();
    let ready = status.conditions.unwrap_or_default().iter().any(|c| c.type_ == "Ready" && c.status == "True");
    let roles = labels.keys().filter_map(|k| k.strip_prefix("node-role.kubernetes.io/")).map(str::to_string).collect();
    let capacity = status.capacity.unwrap_or_default();
    let alloc = status.allocatable.unwrap_or_default();
    let allocatable = Usage {
        cpu_m: alloc.get("cpu").map(|q| metrics::cpu_milli(&q.0)).unwrap_or(0),
        mem: alloc.get("memory").map(|q| metrics::bytes(&q.0)).unwrap_or(0),
    };
    let addresses = status.addresses.unwrap_or_default();
    let label = |keys: &[&str]| keys.iter().find_map(|k| labels.get(*k)).cloned().unwrap_or_default();
    NodeInfo {
        unschedulable: node.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false),
        name: node.metadata.name.unwrap_or_default(),
        ready,
        roles,
        region: label(&["topology.kubernetes.io/region", "failure-domain.beta.kubernetes.io/region"]),
        zone: label(&["topology.kubernetes.io/zone", "failure-domain.beta.kubernetes.io/zone"]),
        cpu: capacity.get("cpu").map(|q| q.0.clone()).unwrap_or_default(),
        memory: capacity.get("memory").map(|q| human_bytes(&q.0)).unwrap_or_default(),
        allocatable,
        os_image: status.node_info.as_ref().map(|i| i.os_image.clone()).unwrap_or_default(),
        kubelet: status.node_info.map(|i| i.kubelet_version).unwrap_or_default(),
        internal_ip: addresses.iter().find(|a| a.type_ == "InternalIP").map(|a| a.address.clone()).unwrap_or_default(),
        provider_id: node.spec.and_then(|s| s.provider_id).unwrap_or_default(),
        addresses: addresses.into_iter().map(|a| a.address).collect(),
        cloud: None,
        labels,
        annotations,
        geo: None,
        place: String::new(),
    }
}

fn pod_info(pod: Pod) -> PodInfo {
    let owner = controller(&pod.metadata);
    let spec = pod.spec.unwrap_or_default();
    let mut requests = Usage::default();
    for c in &spec.containers {
        if let Some(req) = c.resources.as_ref().and_then(|r| r.requests.as_ref()) {
            requests += Usage {
                cpu_m: req.get("cpu").map(|q| metrics::cpu_milli(&q.0)).unwrap_or(0),
                mem: req.get("memory").map(|q| metrics::bytes(&q.0)).unwrap_or(0),
            };
        }
    }
    let status = pod.status.unwrap_or_default();
    let statuses = status.container_statuses.unwrap_or_default();
    let reason = statuses
        .iter()
        .filter_map(|cs| cs.state.as_ref())
        .find_map(|s| {
            s.waiting
                .as_ref()
                .and_then(|w| w.reason.clone())
                .or_else(|| s.terminated.as_ref().and_then(|t| t.reason.clone()).filter(|r| r != "Completed"))
        })
        .or(status.reason);
    let volumes = spec.volumes.clone().unwrap_or_default();
    PodInfo {
        labels: pod.metadata.labels.clone().unwrap_or_default(),
        host_path: volumes.iter().any(|v| v.host_path.is_some()),
        empty_dir: volumes.iter().any(|v| v.empty_dir.is_some()),
        mirror: pod.metadata.annotations.as_ref().is_some_and(|a| a.contains_key("kubernetes.io/config.mirror")),
        name: pod.metadata.name.unwrap_or_default(),
        namespace: pod.metadata.namespace.unwrap_or_default(),
        node: spec.node_name,
        phase: status.phase.unwrap_or_else(|| "Unknown".into()),
        reason,
        restarts: statuses.iter().map(|c| c.restart_count).sum(),
        ready: !statuses.is_empty() && statuses.iter().all(|c| c.ready),
        containers: spec.containers.into_iter().map(|c| c.name).collect(),
        owner,
        requests,
    }
}

fn event_info(e: Event) -> EventInfo {
    let o = e.involved_object;
    EventInfo {
        reason: e.reason.unwrap_or_default(),
        object: format!("{}/{}", o.kind.unwrap_or_default().to_lowercase(), o.name.unwrap_or_default()),
        message: e.message.unwrap_or_default().replace('\n', " "),
        count: e.count.unwrap_or(1),
    }
}

/// `7927612Ki` -> `7.6Gi`; anything unparseable is passed through.
fn human_bytes(q: &str) -> String {
    let units = [("Ki", 1u64 << 10), ("Mi", 1 << 20), ("Gi", 1 << 30), ("Ti", 1 << 40)];
    let (num, mult) = units.iter().find_map(|(s, m)| q.strip_suffix(s).map(|n| (n, *m))).unwrap_or((q, 1));
    match num.parse::<f64>() {
        Ok(n) => format!("{:.1}Gi", n * mult as f64 / (1u64 << 30) as f64),
        Err(_) => q.to_string(),
    }
}

fn controller(meta: &ObjectMeta) -> Option<(String, String)> {
    meta.owner_references.as_ref()?.iter().find(|o| o.controller == Some(true)).map(|o| (o.kind.clone(), o.name.clone()))
}

fn first_image(spec: Option<Option<PodSpec>>) -> String {
    spec.flatten().and_then(|s| s.containers.into_iter().next()).and_then(|c| c.image).unwrap_or_default()
}

fn workload(kind: WorkloadKind, meta: ObjectMeta, desired: i32, ready: i32, image: String) -> WorkloadInfo {
    WorkloadInfo {
        kind,
        owner: controller(&meta),
        name: meta.name.unwrap_or_default(),
        namespace: meta.namespace.unwrap_or_default(),
        desired,
        ready,
        image,
    }
}

/// Background refresh of traffic, RBAC and Helm (and CRD discovery), written into `shared`.
async fn slow_loop(client: Client, shared: Shared, wake: Arc<Notify>) {
    let mut crds_at: Option<Instant> = None;
    let mut slow_at: Option<Instant> = None;
    let mut helm_cache: HelmPayloads = BTreeMap::new();
    let limit = Duration::from_secs(90);
    loop {
        if crds_at.is_none_or(|t| t.elapsed() >= DISCOVERY_INTERVAL) {
            crds_at = Some(Instant::now());
            let crds = Crds::discover(&client).await;
            shared.lock().unwrap().crds = crds;
        }
        let crds = shared.lock().unwrap().crds.clone();

        // Fast lane: Argo CD Applications and metrics.
        let (apps, metrics) = tokio::join!(
            async {
                match &crds.application {
                    Some(ar) => Some(tokio::time::timeout(Duration::from_secs(20), gitops::list(&client, ar)).await),
                    None => None,
                }
            },
            async {
                match (&crds.node_metrics, &crds.pod_metrics) {
                    (Some(n), Some(p)) => Some(tokio::time::timeout(Duration::from_secs(20), metrics::fetch(&client, n, p)).await),
                    _ => None,
                }
            },
        );
        {
            let mut live_errors = vec![];
            let mut st = shared.lock().unwrap();
            match apps {
                Some(Ok(Ok(a))) => st.apps = a,
                Some(Ok(Err(e))) => live_errors.push(lane_error("ArgoCD", "applications", format!("{e:#}"))),
                Some(Err(_)) => live_errors.push(lane_error("ArgoCD", "applications", "timed out after 20s".into())),
                None => st.apps.clear(),
            }
            match metrics {
                Some(Ok(Ok(m))) => st.metrics = Some(m),
                Some(Ok(Err(e))) => live_errors.push(lane_error("Metrics", "metrics.k8s.io", format!("{e:#}"))),
                Some(Err(_)) => live_errors.push(lane_error("Metrics", "metrics.k8s.io", "timed out after 20s".into())),
                None => st.metrics = None,
            }
            st.live_errors = live_errors;
        }

        // Slow lane: traffic, RBAC, Helm.
        if slow_at.is_none_or(|t| t.elapsed() >= SLOW_INTERVAL) {
            slow_at = Some(Instant::now());
            let (net, rbac, helm, storage) = tokio::join!(
                tokio::time::timeout(limit, ops::net(&client, &crds)),
                tokio::time::timeout(limit, ops::rbac(&client)),
                tokio::time::timeout(limit, helm_releases(&client, &mut helm_cache)),
                tokio::time::timeout(limit, crate::storage::fetch(&client)),
            );
            let mut errors = vec![];
            // The guard must be gone before the next await: scoped, returning what cleanup needs.
            let (net, storage) = {
                let mut st = shared.lock().unwrap();
                match net {
                    Ok(Ok(n)) => st.net = n,
                    Ok(Err(e)) => errors.push(lane_error("OpsList", "traffic", format!("{e:#}"))),
                    Err(_) => errors.push(lane_error("OpsList", "traffic", "timed out after 90s".into())),
                }
                match rbac {
                    Ok(Ok(r)) => st.rbac = r,
                    Ok(Err(e)) => errors.push(lane_error("OpsList", "rbac", format!("{e:#}"))),
                    Err(_) => errors.push(lane_error("OpsList", "rbac", "timed out after 90s".into())),
                }
                match storage {
                    Ok(Ok(s)) => st.storage = s,
                    Ok(Err(e)) => errors.push(lane_error("OpsList", "storage", format!("{e:#}"))),
                    Err(_) => errors.push(lane_error("OpsList", "storage", "timed out after 90s".into())),
                }
                match helm {
                    Ok(Ok(h)) => st.helm = h,
                    Ok(Err(e)) => errors.push(lane_error("HelmList", "secrets/owner=helm", format!("{e:#}"))),
                    Err(_) => errors.push(lane_error("HelmList", "secrets/owner=helm", "timed out after 90s".into())),
                }
                st.errors = errors;
                (st.net.clone(), st.storage.clone())
            };
            // Cleanup candidates build on the traffic and storage data just fetched.
            let found = tokio::time::timeout(limit, crate::cleanup::scan(&client, &net, &storage)).await;
            let pdbs = tokio::time::timeout(limit, pdbs(&client)).await;
            let mut st = shared.lock().unwrap();
            match found {
                Ok(Ok(f)) => st.cleanup = f,
                Ok(Err(e)) => st.errors.push(lane_error("OpsList", "cleanup", format!("{e:#}"))),
                Err(_) => st.errors.push(lane_error("OpsList", "cleanup", "timed out after 90s".into())),
            }
            match pdbs {
                Ok(Ok(p)) => st.pdbs = p,
                Ok(Err(e)) => st.errors.push(lane_error("OpsList", "poddisruptionbudgets", format!("{e:#}"))),
                Err(_) => st.errors.push(lane_error("OpsList", "poddisruptionbudgets", "timed out after 90s".into())),
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(LIVE_INTERVAL) => {}
            // A refresh request or an action: re-fetch everything now.
            _ = wake.notified() => slow_at = None,
        }
    }
}

fn lane_error(reason: &str, object: &str, message: String) -> EventInfo {
    EventInfo { reason: reason.into(), object: object.into(), message, count: 1 }
}

/// Decoded chart info per release revision — a revision's payload never changes, so it is fetched once.
type HelmPayloads = BTreeMap<(String, String, i32), (String, String, String)>;

/// Helm 3 keeps each release revision in a Secret (`owner=helm`). Labels carry name, revision and
/// status; the (large) gzipped payload is only fetched for revisions not seen before.
async fn helm_releases(client: &Client, cache: &mut HelmPayloads) -> Result<Vec<HelmRelease>> {
    let lp = ListParams::default().labels("owner=helm,status!=superseded");
    let secrets = Api::<Secret>::all(client.clone()).list_metadata(&lp).await.context("listing Helm release secrets")?;
    let mut latest: BTreeMap<(String, String), HelmRelease> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for s in secrets.items {
        let labels = s.metadata.labels.clone().unwrap_or_default();
        let namespace = s.metadata.namespace.clone().unwrap_or_default();
        let name = labels.get("name").cloned().unwrap_or_default();
        let revision = labels.get("version").and_then(|v| v.parse().ok()).unwrap_or(0);
        let key = (namespace.clone(), name.clone(), revision);
        if !cache.contains_key(&key) {
            let secret = Api::<Secret>::namespaced(client.clone(), &namespace)
                .get(s.metadata.name.as_deref().unwrap_or_default())
                .await
                .with_context(|| format!("reading Helm release {namespace}/{name}"))?;
            let info = match secret.data.as_ref().and_then(|d| d.get("release")).and_then(|b| decode_release(&b.0)) {
                Some(body) => {
                    let meta = &body["chart"]["metadata"];
                    (
                        format!("{}-{}", meta["name"].as_str().unwrap_or("?"), meta["version"].as_str().unwrap_or("?")),
                        meta["appVersion"].as_str().unwrap_or_default().to_string(),
                        body["info"]["last_deployed"].as_str().unwrap_or_default().chars().take(19).collect(),
                    )
                }
                None => Default::default(),
            };
            cache.insert(key.clone(), info);
        }
        let (chart, app_version, updated) = cache[&key].clone();
        seen.insert(key);
        let rel = HelmRelease {
            name: name.clone(),
            namespace: namespace.clone(),
            revision,
            status: labels.get("status").cloned().unwrap_or_default(),
            chart,
            app_version,
            updated,
        };
        let k = (namespace, name);
        if latest.get(&k).is_none_or(|r| r.revision < rel.revision) {
            latest.insert(k, rel);
        }
    }
    cache.retain(|k, _| seen.contains(k));
    Ok(latest.into_values().collect())
}

/// Release payload: base64 (inside the already-decoded Secret data) of gzipped JSON.
pub(crate) fn decode_release(raw: &[u8]) -> Option<serde_json::Value> {
    use base64::Engine;
    use std::io::Read;
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    let json = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out).ok()?;
        out
    } else {
        bytes
    };
    serde_json::from_slice(&json).ok()
}

/// Volumes that need attention, as ticker warnings (information-only notes are left out).
fn volume_warnings(storage: &crate::storage::StorageInfo) -> Vec<EventInfo> {
    storage
        .volumes
        .iter()
        .filter_map(|v| {
            let (h, msg) = v.problems(None).into_iter().next()?;
            (h != crate::model::Health::Ok).then(|| EventInfo {
                reason: "Volume".into(),
                object: match (&v.namespace, &v.claim) {
                    (Some(ns), Some(c)) => format!("pvc/{ns}/{c}"),
                    _ => format!("pv/{}", v.pv.clone().unwrap_or_default()),
                },
                message: msg,
                count: 1,
            })
        })
        .collect()
}

/// Certificates that failed or are about to expire, as ticker warnings.
fn cert_warnings(net: &NetInfo) -> Vec<EventInfo> {
    net.certs
        .iter()
        .filter_map(|c| {
            let message = match (c.ready, c.days_left) {
                (false, _) => format!("not ready: {} {}", c.reason, c.message),
                (true, Some(d)) if d < 14 => format!("expires in {d} days (renewal {})", c.renewal.as_deref().unwrap_or("?")),
                _ => return None,
            };
            Some(EventInfo { reason: "Certificate".into(), object: format!("certificate/{}/{}", c.namespace, c.name), message, count: 1 })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helm_payload_roundtrip() {
        use base64::Engine;
        use std::io::Write;
        let json = br#"{"chart":{"metadata":{"name":"argo-cd","version":"5.1.0","appVersion":"v2.6"}},"info":{"status":"deployed"}}"#;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(json).unwrap();
        let raw = base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap());
        let v = decode_release(raw.as_bytes()).unwrap();
        assert_eq!(v["chart"]["metadata"]["name"], "argo-cd");
    }
}
