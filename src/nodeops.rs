//! Node maintenance: cordon / uncordon, and drain — cordon, then evict every pod through the
//! Eviction API (which honours PodDisruptionBudgets), like `kubectl drain`. Before a drain the
//! plan says what will happen to each kind of pod, so nothing is a surprise.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{DeleteParams, EvictParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use serde_json::json;

use crate::metrics::{Usage, fmt_cpu, fmt_mem};
use crate::model::{Health, PodInfo, Snapshot};

/// How long a drain keeps retrying evictions refused by a PodDisruptionBudget.
const PDB_WAIT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct DrainPlan {
    pub node: String,
    /// (namespace, name) of the pods to evict.
    pub evict: Vec<(String, String)>,
    pub skipped_daemonset: usize,
    pub skipped_static: usize,
    pub warnings: Vec<(Health, String)>,
}

fn names(pods: &[&PodInfo]) -> String {
    let mut v: Vec<String> = pods.iter().take(4).map(|p| format!("{}/{}", p.namespace, p.name)).collect();
    if pods.len() > 4 {
        v.push(format!("+{} more", pods.len() - 4));
    }
    v.join(", ")
}

pub fn plan(snap: &Snapshot, node: &str) -> DrainPlan {
    let on_node: Vec<&PodInfo> =
        snap.pods.iter().filter(|p| p.node.as_deref() == Some(node) && !matches!(p.phase.as_str(), "Succeeded" | "Failed")).collect();
    let daemon = |p: &PodInfo| p.owner.as_ref().is_some_and(|(k, _)| k == "DaemonSet");
    let evict: Vec<&PodInfo> = on_node.iter().copied().filter(|p| !daemon(p) && !p.mirror).collect();
    let mut w: Vec<(Health, String)> = vec![];

    let others: Vec<_> = snap.nodes.iter().filter(|n| n.name != node && n.ready && !n.unschedulable).collect();
    if others.is_empty() && !evict.is_empty() {
        w.push((Health::Bad, "no other ready, schedulable node: evicted pods stay Pending".into()));
    }
    if snap.nodes.iter().find(|n| n.name == node).is_some_and(|n| n.is_control_plane()) {
        w.push((Health::Warn, "control-plane node: its static pods (API server, etcd…) keep running, the rest moves".into()));
    }

    // PodDisruptionBudgets that allow no disruption block those evictions until they do.
    for pdb in snap.pdbs.iter().filter(|b| b.allowed == 0) {
        let blocked: Vec<&PodInfo> = evict.iter().copied().filter(|p| pdb.covers(p)).collect();
        if !blocked.is_empty() {
            w.push((
                Health::Bad,
                format!(
                    "PodDisruptionBudget {}/{} allows 0 disruptions: {} — the drain retries for {} min, then gives up on them",
                    pdb.namespace,
                    pdb.name,
                    names(&blocked),
                    PDB_WAIT.as_secs() / 60
                ),
            ));
        }
    }

    // Data on this node's disk.
    for v in snap.storage.volumes.iter().filter(|v| matches!(v.kind.as_str(), "hostPath" | "local")) {
        if !v.mounted_by.iter().any(|(_, n)| n == node) {
            continue;
        }
        let claim = format!("{}/{}", v.namespace.clone().unwrap_or_default(), v.name());
        w.push(if v.node.as_deref() == Some(node) {
            (Health::Bad, format!("{claim}: data pinned to this node ({}) — its pod stays Pending until you uncordon", v.detail))
        } else {
            (
                Health::Bad,
                format!("{claim}: data on this node's disk without node affinity — its pod starts elsewhere with an EMPTY directory"),
            )
        });
    }
    let host_path: Vec<&PodInfo> = evict.iter().copied().filter(|p| p.host_path).collect();
    if !host_path.is_empty() {
        w.push((Health::Warn, format!("hostPath volumes (data stays on this node): {}", names(&host_path))));
    }
    let bare: Vec<&PodInfo> = evict.iter().copied().filter(|p| p.owner.is_none()).collect();
    if !bare.is_empty() {
        w.push((Health::Bad, format!("pods without a controller are deleted for good: {}", names(&bare))));
    }
    let scratch: Vec<&PodInfo> = evict.iter().copied().filter(|p| p.empty_dir).collect();
    if !scratch.is_empty() {
        w.push((Health::Warn, format!("emptyDir data is lost: {}", names(&scratch))));
    }

    // Room elsewhere for what moves (requests vs allocatable).
    let mut need = Usage::default();
    for p in &evict {
        need += p.requests;
    }
    let mut free = Usage::default();
    for n in &others {
        let mut used = Usage::default();
        for p in snap.pods.iter().filter(|p| p.node.as_deref() == Some(&n.name)) {
            used += p.requests;
        }
        free += Usage { cpu_m: (n.allocatable.cpu_m - used.cpu_m).max(0), mem: (n.allocatable.mem - used.mem).max(0) };
    }
    if !others.is_empty() && (need.cpu_m > free.cpu_m || need.mem > free.mem) {
        w.push((
            Health::Warn,
            format!(
                "not enough room on the other nodes: moving {} CPU / {} requested, free {} / {}",
                fmt_cpu(need.cpu_m),
                fmt_mem(need.mem),
                fmt_cpu(free.cpu_m),
                fmt_mem(free.mem)
            ),
        ));
    }
    let skipped_daemonset = on_node.iter().filter(|p| daemon(p)).count();
    let skipped_static = on_node.iter().filter(|p| p.mirror).count();
    w.insert(
        0,
        (
            Health::Ok,
            format!(
                "cordons the node, evicts {} pods (honouring PodDisruptionBudgets), leaves {skipped_daemonset} DaemonSet and {skipped_static} static pods",
                evict.len()
            ),
        ),
    );
    w.sort_by_key(|(h, _)| *h);
    DrainPlan {
        node: node.to_string(),
        evict: evict.iter().map(|p| (p.namespace.clone(), p.name.clone())).collect(),
        skipped_daemonset,
        skipped_static,
        warnings: w,
    }
}

pub async fn cordon(client: &Client, node: &str, on: bool, dry_run: bool) -> Result<()> {
    let pp = PatchParams { field_manager: Some("k8s-galaxy".into()), dry_run, ..Default::default() };
    Api::<Node>::all(client.clone())
        .patch(node, &pp, &Patch::Merge(json!({"spec": {"unschedulable": on}})))
        .await
        .with_context(|| format!("{} {node}", if on { "cordoning" } else { "uncordoning" }))?;
    Ok(())
}

/// Cordon, then evict all planned pods concurrently, retrying evictions a PDB refuses (429).
/// `progress` gets short status lines while it runs.
pub async fn drain(client: &Client, plan: &DrainPlan, dry_run: bool, progress: impl Fn(String) + Sync) -> Result<String> {
    cordon(client, &plan.node, true, dry_run).await?;
    let started = Instant::now();
    let params = EvictParams {
        delete_options: Some(DeleteParams { dry_run, ..Default::default() }),
        post_options: PostParams { dry_run, ..Default::default() },
    };
    let runs = plan.evict.iter().map(|(ns, name)| {
        let api = Api::<Pod>::namespaced(client.clone(), ns);
        let params = &params;
        let progress = &progress;
        async move {
            loop {
                match api.evict(name, params).await {
                    Ok(_) => return Ok(()),
                    Err(kube::Error::Api(e)) if e.code == 404 => return Ok(()), // already gone
                    Err(kube::Error::Api(e)) if e.code == 429 && started.elapsed() < PDB_WAIT => {
                        progress(format!("waiting: a PodDisruptionBudget holds {ns}/{name}"));
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    Err(e) => return Err(format!("{ns}/{name}: {e}")),
                }
            }
        }
    });
    let results = futures::future::join_all(runs).await;
    let failed: Vec<String> = results.into_iter().filter_map(|r| r.err()).collect();
    let evicted = plan.evict.len() - failed.len();
    let prefix = if dry_run { "dry-run OK: " } else { "" };
    if failed.is_empty() {
        Ok(format!(
            "{prefix}drained {}: {evicted} pods evicted, {} DaemonSet and {} static pods left in place",
            plan.node, plan.skipped_daemonset, plan.skipped_static
        ))
    } else {
        anyhow::bail!("{prefix}drained {} partly: {evicted} evicted, {} not — {}", plan.node, failed.len(), failed.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_flags_pdbs_bare_pods_and_daemonsets() {
        let mut snap = crate::demo::Sim::new().snapshot();
        let node = snap.nodes[1].name.clone();
        // A bare pod on the node.
        let mut bare = snap.pods.iter().find(|p| p.node.as_deref() == Some(&node)).unwrap().clone();
        bare.name = "debug-shell".into();
        bare.owner = None;
        snap.pods.push(bare);
        let p = plan(&snap, &node);
        assert!(p.skipped_daemonset > 0);
        assert!(p.evict.iter().any(|(_, n)| n == "debug-shell"));
        assert!(p.warnings.iter().any(|(h, t)| *h == Health::Bad && t.contains("deleted for good")));
        assert_eq!(p.warnings.last().map(|(h, _)| *h), Some(Health::Ok));
    }
}
