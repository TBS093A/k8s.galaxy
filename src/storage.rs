//! Persistent volumes: claims, volumes and classes, where the data physically lives (inside the
//! cluster — node disks, Longhorn, Rook Ceph… — or outside — cloud block storage, NFS…), how much
//! is provisioned and how much is actually used (kubelet volume stats, or `du` on the node for
//! hostPath/local volumes, which the kubelet does not measure).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use k8s_openapi::api::core::v1::{Node, PersistentVolume, PersistentVolumeClaim, Pod};
use k8s_openapi::api::storage::v1::StorageClass;
use kube::api::{DeleteParams, ListParams, LogParams, PostParams};
use kube::{Api, Client};
use serde_json::json;

use crate::metrics::bytes;
use crate::model::Health;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// Data on cluster nodes or a storage system running in the cluster.
    InCluster(String),
    /// Data on something the cluster does not run: cloud block/file storage, an NFS server…
    External(String),
    Unknown,
}

impl Location {
    pub fn short(&self) -> &'static str {
        match self {
            Location::InCluster(_) => "in-cluster",
            Location::External(_) => "external",
            Location::Unknown => "unknown",
        }
    }

    pub fn what(&self) -> &str {
        match self {
            Location::InCluster(s) | Location::External(s) => s,
            Location::Unknown => "?",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    pub used: i64,
    pub capacity: i64,
}

/// Result of measuring a hostPath/local volume on its node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measured {
    pub used: i64,
    /// Filesystem holding the directory: device, size, used.
    pub device: String,
    pub fs_size: i64,
    pub fs_used: i64,
}

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    /// Claim side (None for a PV nobody claims).
    pub namespace: Option<String>,
    pub claim: Option<String>,
    pub claim_phase: Option<String>,
    pub requested: Option<i64>,
    /// Volume side (None for a claim still Pending).
    pub pv: Option<String>,
    pub pv_phase: Option<String>,
    pub capacity: Option<i64>,
    pub reclaim: String,
    pub class: String,
    pub access: Vec<String>,
    /// "hostPath", "local", "nfs", "csi", "iscsi"…
    pub kind: String,
    /// Driver / provisioner, e.g. "ebs.csi.aws.com", "rancher.io/local-path".
    pub driver: String,
    /// Path, server:path or volume handle.
    pub detail: String,
    pub location: Location,
    /// Node the data is pinned to (nodeAffinity on kubernetes.io/hostname).
    pub node: Option<String>,
    /// Pods mounting the claim: (namespace/name, node).
    pub mounted_by: Vec<(String, String)>,
    /// Live usage from the kubelet (CSI and other volumes that report stats).
    pub usage: Option<DiskUsage>,
    /// Released claim name kept on a Retain volume (`spec.claimRef`).
    pub released_from: Option<String>,
}

impl VolumeInfo {
    /// Name shown in lists: the claim, else the volume.
    pub fn name(&self) -> String {
        self.claim.clone().or_else(|| self.pv.clone()).unwrap_or_default()
    }

    pub fn group(&self) -> String {
        self.namespace.clone().unwrap_or_else(|| "(no claim)".into())
    }

    /// Provisioned size: the bound volume's capacity, else what the claim asks for.
    pub fn size(&self) -> Option<i64> {
        self.capacity.or(self.requested)
    }

    /// Node to measure a hostPath/local volume on: its affinity, else where it is mounted.
    pub fn measure_node(&self) -> Option<String> {
        self.node.clone().or_else(|| self.mounted_by.first().map(|(_, n)| n.clone()))
    }

    pub fn measurable(&self) -> bool {
        matches!(self.kind.as_str(), "hostPath" | "local") && self.pv.is_some() && self.measure_node().is_some()
    }

    pub fn health(&self, measured: Option<&Measured>) -> Health {
        let mut h = Health::Ok;
        for (sev, _) in self.problems(measured) {
            h = h.min(sev);
        }
        h
    }

    /// What is wrong or worth knowing, worst first.
    pub fn problems(&self, measured: Option<&Measured>) -> Vec<(Health, String)> {
        let mut p = vec![];
        if self.claim_phase.as_deref() == Some("Pending") {
            p.push((Health::Bad, "claim Pending: no volume bound (no matching PV / provisioner failing)".into()));
        }
        if self.claim_phase.as_deref() == Some("Lost") || self.pv_phase.as_deref() == Some("Failed") {
            p.push((Health::Bad, format!("volume {}", self.pv_phase.clone().unwrap_or_else(|| "Lost".into()))));
        }
        if self.pv_phase.as_deref() == Some("Released") {
            p.push((
                Health::Warn,
                format!(
                    "Released: claim {} is gone, data kept ({} policy) — reuse it or delete the volume and its data",
                    self.released_from.as_deref().unwrap_or("?"),
                    self.reclaim
                ),
            ));
        }
        if self.kind == "hostPath" && self.node.is_none() {
            p.push((Health::Warn, "hostPath without node affinity: a pod scheduled to another node sees an empty directory".into()));
        }
        let used = self.usage.map(|u| (u.used, u.capacity)).or_else(|| measured.map(|m| (m.used, self.size().unwrap_or(0))));
        if let Some((used, cap)) = used
            && cap > 0
        {
            let pct = used as f64 * 100.0 / cap as f64;
            if pct >= 95.0 {
                p.push((Health::Bad, format!("{pct:.0}% full")));
            } else if pct >= 85.0 {
                p.push((Health::Warn, format!("{pct:.0}% full")));
            }
            if matches!(self.kind.as_str(), "hostPath") && pct > 100.0 {
                p.push((Health::Warn, "hostPath does not enforce the size: it can fill the node's disk".into()));
            }
        }
        if let Some(m) = measured
            && m.fs_size > 0
            && m.fs_used as f64 / m.fs_size as f64 > 0.85
        {
            p.push((
                Health::Warn,
                format!("filesystem {} on the node is {:.0}% full", m.device, m.fs_used as f64 * 100.0 / m.fs_size as f64),
            ));
        }
        if self.claim.is_some() && self.mounted_by.is_empty() && self.claim_phase.as_deref() == Some("Bound") {
            p.push((Health::Ok, "not mounted by any pod".into()));
        }
        p.sort_by_key(|(h, _)| *h);
        p
    }
}

#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub name: String,
    pub provisioner: String,
    pub default: bool,
    pub reclaim: String,
    pub binding: String,
    pub expansion: bool,
    pub location: Location,
}

#[derive(Clone, Debug, Default)]
pub struct StorageInfo {
    pub volumes: Vec<VolumeInfo>,
    pub classes: Vec<ClassInfo>,
    /// Node root filesystem usage from the kubelet (hostPath volumes share it unless mounted elsewhere).
    pub node_fs: BTreeMap<String, DiskUsage>,
}

/// Where data of a driver / provisioner lives. `node_ips` lets an NFS server on a node count as in-cluster.
pub fn locate(kind: &str, driver: &str, detail: &str, node: Option<&str>, node_ips: &BTreeSet<String>) -> Location {
    let on = |what: &str| match node {
        Some(n) => format!("{what} on {n}"),
        None => what.to_string(),
    };
    match kind {
        "hostPath" => return Location::InCluster(on("node disk (hostPath)")),
        "local" => return Location::InCluster(on("node disk (local)")),
        "nfs" => {
            let server = detail.split(':').next().unwrap_or("");
            return if node_ips.contains(server) {
                Location::InCluster(format!("NFS server on a node ({server})"))
            } else {
                Location::External(format!("NFS {server}"))
            };
        }
        "iscsi" => return Location::External("iSCSI target".into()),
        "fc" => return Location::External("Fibre Channel SAN".into()),
        "rbd" | "cephfs" => return Location::External("Ceph (in-tree, external cluster)".into()),
        _ => {}
    }
    let d = driver.to_lowercase();
    let inside: &[(&str, &str)] = &[
        ("driver.longhorn.io", "Longhorn"),
        ("rook-ceph", "Rook Ceph"),
        ("openebs.io", "OpenEBS"),
        ("topolvm", "TopoLVM"),
        ("rancher.io/local-path", "local-path (node disk)"),
        ("local-path", "local-path (node disk)"),
        ("directpv.min.io", "DirectPV"),
        ("linstor.csi.linbit.com", "LINSTOR"),
        ("hostpath.csi.k8s.io", "csi-hostpath (node disk)"),
        ("mayastor", "OpenEBS Mayastor"),
        ("seaweedfs", "SeaweedFS"),
        ("juicefs", "JuiceFS"),
        ("portworx", "Portworx"),
        ("kubernetes.io/no-provisioner", "local volumes (node disk)"),
    ];
    if let Some((_, n)) = inside.iter().find(|(k, _)| d.contains(k)) {
        return Location::InCluster(match node {
            Some(node) if n.contains("node disk") => format!("{n} on {node}"),
            _ => n.to_string(),
        });
    }
    let outside: &[(&str, &str)] = &[
        ("ebs.csi.aws.com", "AWS EBS"),
        ("kubernetes.io/aws-ebs", "AWS EBS"),
        ("efs.csi.aws.com", "AWS EFS"),
        ("fsx.csi.aws.com", "AWS FSx"),
        ("s3.csi.aws.com", "AWS S3"),
        ("pd.csi.storage.gke.io", "GCE Persistent Disk"),
        ("kubernetes.io/gce-pd", "GCE Persistent Disk"),
        ("filestore.csi.storage.gke.io", "GCP Filestore"),
        ("disk.csi.azure.com", "Azure Disk"),
        ("kubernetes.io/azure-disk", "Azure Disk"),
        ("file.csi.azure.com", "Azure Files"),
        ("csi.hetzner.cloud", "Hetzner Cloud Volume"),
        ("dobs.csi.digitalocean.com", "DigitalOcean Volume"),
        ("linodebs.csi.linode.com", "Linode Block Storage"),
        ("blockvolume.csi.oraclecloud.com", "OCI Block Volume"),
        ("csi.scaleway.com", "Scaleway Block Storage"),
        ("cinder.csi.openstack.org", "OpenStack Cinder"),
        ("csi.vsphere.vmware.com", "vSphere disk"),
        ("nfs.csi.k8s.io", "NFS"),
        ("smb.csi.k8s.io", "SMB share"),
        ("csi.trident.netapp.io", "NetApp"),
        ("rbd.csi.ceph.com", "Ceph RBD (external)"),
        ("cephfs.csi.ceph.com", "CephFS (external)"),
        ("ru.yandex.s3.csi", "S3 (csi-s3)"),
        ("csi.storage.k8s.io", "external CSI"),
    ];
    if let Some((_, n)) = outside.iter().find(|(k, _)| d.contains(k)) {
        return Location::External(n.to_string());
    }
    Location::Unknown
}

fn hostname_affinity(pv: &PersistentVolume) -> Option<String> {
    let terms = &pv.spec.as_ref()?.node_affinity.as_ref()?.required.as_ref()?.node_selector_terms;
    terms
        .iter()
        .flat_map(|t| t.match_expressions.iter().flatten())
        .find(|e| e.key == "kubernetes.io/hostname" && e.operator == "In")
        .and_then(|e| e.values.as_ref()?.first().cloned())
}

/// Backend kind, driver and detail of a PV.
fn backend(pv: &PersistentVolume) -> (String, String, String) {
    let Some(s) = pv.spec.as_ref() else { return ("?".into(), String::new(), String::new()) };
    let provisioner = pv.metadata.annotations.as_ref().and_then(|a| a.get("pv.kubernetes.io/provisioned-by")).cloned().unwrap_or_default();
    if let Some(h) = &s.host_path {
        return ("hostPath".into(), provisioner, h.path.clone());
    }
    if let Some(l) = &s.local {
        return ("local".into(), provisioner, l.path.clone());
    }
    if let Some(n) = &s.nfs {
        return ("nfs".into(), provisioner, format!("{}:{}", n.server, n.path));
    }
    if let Some(c) = &s.csi {
        let mut detail = c.volume_handle.clone();
        if c.driver == "nfs.csi.k8s.io"
            && let Some(attrs) = &c.volume_attributes
            && let Some(server) = attrs.get("server")
        {
            detail = format!("{server}:{}", attrs.get("share").cloned().unwrap_or_default());
        }
        return ("csi".into(), c.driver.clone(), detail);
    }
    if let Some(i) = &s.iscsi {
        return ("iscsi".into(), provisioner, format!("{} {}", i.target_portal, i.iqn));
    }
    if s.rbd.is_some() {
        return ("rbd".into(), provisioner, String::new());
    }
    if s.cephfs.is_some() {
        return ("cephfs".into(), provisioner, String::new());
    }
    if s.fc.is_some() {
        return ("fc".into(), provisioner, String::new());
    }
    if let Some(e) = &s.aws_elastic_block_store {
        return ("csi".into(), "kubernetes.io/aws-ebs".into(), e.volume_id.clone());
    }
    if let Some(g) = &s.gce_persistent_disk {
        return ("csi".into(), "kubernetes.io/gce-pd".into(), g.pd_name.clone());
    }
    if let Some(a) = &s.azure_disk {
        return ("csi".into(), "kubernetes.io/azure-disk".into(), a.disk_name.clone());
    }
    ("other".into(), provisioner, String::new())
}

fn qty(map: Option<&BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>>, key: &str) -> Option<i64> {
    map?.get(key).map(|q| bytes(&q.0))
}

pub async fn fetch(client: &Client) -> Result<StorageInfo> {
    let lp = ListParams::default();
    let (pvs, pvcs, classes, pods, nodes) = tokio::try_join!(
        async { Api::<PersistentVolume>::all(client.clone()).list(&lp).await.context("listing persistentvolumes") },
        async { Api::<PersistentVolumeClaim>::all(client.clone()).list(&lp).await.context("listing persistentvolumeclaims") },
        async { Api::<StorageClass>::all(client.clone()).list(&lp).await.context("listing storageclasses") },
        async { Api::<Pod>::all(client.clone()).list(&lp).await.context("listing pods") },
        async { Api::<Node>::all(client.clone()).list(&lp).await.context("listing nodes") },
    )?;
    let node_ips: BTreeSet<String> = nodes
        .items
        .iter()
        .flat_map(|n| n.status.as_ref().and_then(|s| s.addresses.clone()).unwrap_or_default())
        .map(|a| a.address)
        .collect();
    let node_names: Vec<String> = nodes.items.iter().filter_map(|n| n.metadata.name.clone()).collect();

    // Kubelet volume stats + node filesystem, per node (errors just mean no live usage).
    let mut usage: BTreeMap<(String, String), DiskUsage> = BTreeMap::new();
    let mut node_fs = BTreeMap::new();
    let stats = futures::future::join_all(node_names.iter().map(|n| {
        let client = client.clone();
        async move {
            let req = http::Request::get(format!("/api/v1/nodes/{n}/proxy/stats/summary")).body(vec![]).ok()?;
            let v: serde_json::Value = tokio::time::timeout(Duration::from_secs(10), client.request(req)).await.ok()?.ok()?;
            Some((n.clone(), v))
        }
    }))
    .await;
    for (node, v) in stats.into_iter().flatten() {
        let fs = &v["node"]["fs"];
        if let (Some(used), Some(cap)) = (fs["usedBytes"].as_i64(), fs["capacityBytes"].as_i64()) {
            node_fs.insert(node, DiskUsage { used, capacity: cap });
        }
        for pod in v["pods"].as_array().into_iter().flatten() {
            for vol in pod["volume"].as_array().into_iter().flatten() {
                let r = &vol["pvcRef"];
                if let (Some(ns), Some(name), Some(used)) = (r["namespace"].as_str(), r["name"].as_str(), vol["usedBytes"].as_i64()) {
                    usage.insert((ns.into(), name.into()), DiskUsage { used, capacity: vol["capacityBytes"].as_i64().unwrap_or(0) });
                }
            }
        }
    }

    let mut mounts: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    for p in &pods.items {
        let ns = p.metadata.namespace.clone().unwrap_or_default();
        let node = p.spec.as_ref().and_then(|s| s.node_name.clone()).unwrap_or_default();
        for v in p.spec.as_ref().and_then(|s| s.volumes.clone()).unwrap_or_default() {
            if let Some(c) = v.persistent_volume_claim {
                mounts
                    .entry((ns.clone(), c.claim_name))
                    .or_default()
                    .push((format!("{ns}/{}", p.metadata.name.clone().unwrap_or_default()), node.clone()));
            }
        }
    }

    let mut out = vec![];
    let mut claimed_pvs = BTreeSet::new();
    for c in &pvcs.items {
        let ns = c.metadata.namespace.clone().unwrap_or_default();
        let name = c.metadata.name.clone().unwrap_or_default();
        let spec = c.spec.clone().unwrap_or_default();
        let pv = spec
            .volume_name
            .clone()
            .filter(|v| !v.is_empty())
            .and_then(|v| pvs.items.iter().find(|p| p.metadata.name.as_deref() == Some(&v)));
        if let Some(pv) = pv {
            claimed_pvs.insert(pv.metadata.name.clone().unwrap_or_default());
        }
        let mut v = volume(pv, &node_ips);
        v.namespace = Some(ns.clone());
        v.claim = Some(name.clone());
        v.claim_phase = c.status.as_ref().and_then(|s| s.phase.clone());
        v.requested = qty(spec.resources.as_ref().and_then(|r| r.requests.as_ref()), "storage");
        if v.class.is_empty() {
            v.class = spec.storage_class_name.clone().unwrap_or_default();
        }
        if pv.is_none() {
            v.access = spec.access_modes.clone().unwrap_or_default();
            // Not bound yet: the class says where it would go.
            if let Some(sc) = classes.items.iter().find(|s| s.metadata.name.as_deref() == Some(v.class.as_str())) {
                v.driver = sc.provisioner.clone();
                v.location = locate("csi", &sc.provisioner, "", None, &node_ips);
            }
        }
        v.mounted_by = mounts.get(&(ns.clone(), name.clone())).cloned().unwrap_or_default();
        v.usage = usage.get(&(ns, name)).copied();
        out.push(v);
    }
    for pv in &pvs.items {
        if claimed_pvs.contains(pv.metadata.name.as_deref().unwrap_or_default()) {
            continue;
        }
        out.push(volume(Some(pv), &node_ips));
    }

    let default_annotation = "storageclass.kubernetes.io/is-default-class";
    let classes = classes
        .items
        .iter()
        .map(|sc| ClassInfo {
            name: sc.metadata.name.clone().unwrap_or_default(),
            provisioner: sc.provisioner.clone(),
            default: sc.metadata.annotations.as_ref().and_then(|a| a.get(default_annotation)).is_some_and(|v| v == "true"),
            reclaim: sc.reclaim_policy.clone().unwrap_or_else(|| "Delete".into()),
            binding: sc.volume_binding_mode.clone().unwrap_or_else(|| "Immediate".into()),
            expansion: sc.allow_volume_expansion.unwrap_or(false),
            location: locate("csi", &sc.provisioner, "", None, &node_ips),
        })
        .collect();
    Ok(StorageInfo { volumes: out, classes, node_fs })
}

fn volume(pv: Option<&PersistentVolume>, node_ips: &BTreeSet<String>) -> VolumeInfo {
    let mut v = VolumeInfo {
        namespace: None,
        claim: None,
        claim_phase: None,
        requested: None,
        pv: None,
        pv_phase: None,
        capacity: None,
        reclaim: String::new(),
        class: String::new(),
        access: vec![],
        kind: String::new(),
        driver: String::new(),
        detail: String::new(),
        location: Location::Unknown,
        node: None,
        mounted_by: vec![],
        usage: None,
        released_from: None,
    };
    let Some(pv) = pv else { return v };
    let spec = pv.spec.clone().unwrap_or_default();
    let (kind, driver, detail) = backend(pv);
    v.node = hostname_affinity(pv);
    v.location = locate(&kind, &driver, &detail, v.node.as_deref(), node_ips);
    v.pv = pv.metadata.name.clone();
    v.pv_phase = pv.status.as_ref().and_then(|s| s.phase.clone());
    v.capacity = qty(spec.capacity.as_ref(), "storage");
    v.reclaim = spec.persistent_volume_reclaim_policy.clone().unwrap_or_default();
    v.class = spec.storage_class_name.clone().unwrap_or_default();
    v.access = spec.access_modes.clone().unwrap_or_default();
    if v.pv_phase.as_deref() == Some("Released") {
        v.released_from =
            spec.claim_ref.as_ref().map(|c| format!("{}/{}", c.namespace.clone().unwrap_or_default(), c.name.clone().unwrap_or_default()));
    }
    v.kind = kind;
    v.driver = driver;
    v.detail = detail;
    v
}

/// Per volume (PV name): what `du`/`df` found, or why it could not.
pub type Measurements = Vec<(String, Result<Measured, String>)>;

/// One measurement target: volume name, node, host directory.
#[derive(Clone, Debug)]
pub struct Target {
    pub pv: String,
    pub node: String,
    pub path: String,
}

const MEASURE_NS: &str = "default";
const MEASURE_IMAGE: &str = "busybox:1.36";

/// `du` + `df` of hostPath/local directories: one short-lived pod per node, the directories mounted
/// read-only, deleted afterwards. With `dry_run` the pods are only validated by the API server.
pub async fn measure(client: &Client, targets: &[Target], dry_run: bool) -> Result<Measurements> {
    let mut by_node: BTreeMap<&str, Vec<&Target>> = BTreeMap::new();
    for t in targets {
        by_node.entry(t.node.as_str()).or_default().push(t);
    }
    let api = Api::<Pod>::namespaced(client.clone(), MEASURE_NS);
    let runs = by_node.into_iter().map(|(node, list)| {
        let api = api.clone();
        async move {
            let res = measure_node(&api, node, &list, dry_run).await;
            match res {
                Ok(_) if dry_run => vec![],
                Ok(found) => list
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        (
                            t.pv.clone(),
                            found.get(&i).cloned().ok_or_else(|| "no output for this directory (missing on the node?)".to_string()),
                        )
                    })
                    .collect::<Vec<_>>(),
                Err(e) => list.iter().map(|t| (t.pv.clone(), Err(format!("{e:#}")))).collect(),
            }
        }
    });
    let results: Vec<_> = futures::future::join_all(runs).await.into_iter().flatten().collect();
    Ok(results)
}

async fn measure_node(api: &Api<Pod>, node: &str, list: &[&Target], dry_run: bool) -> Result<BTreeMap<usize, Measured>> {
    let mounts: Vec<_> =
        (0..list.len()).map(|i| json!({"name": format!("v{i}"), "mountPath": format!("/v/{i}"), "readOnly": true})).collect();
    let volumes: Vec<_> =
        list.iter().enumerate().map(|(i, t)| json!({"name": format!("v{i}"), "hostPath": {"path": t.path, "type": "Directory"}})).collect();
    // One line per directory: index, used KiB, then df's device, size and used KiB.
    let script = (0..list.len())
        .map(|i| format!("echo \"{i} $(du -sk /v/{i} 2>/dev/null | cut -f1) $(df -kP /v/{i} | tail -1 | awk '{{print $1, $2, $3}}')\""))
        .collect::<Vec<_>>()
        .join("; ");
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "generateName": "k8s-galaxy-du-",
            "labels": {"app.kubernetes.io/managed-by": "k8s-galaxy", "app.kubernetes.io/component": "volume-usage"}
        },
        "spec": {
            "nodeName": node,
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "tolerations": [{"operator": "Exists"}],
            "containers": [{
                "name": "du",
                "image": MEASURE_IMAGE,
                "command": ["sh", "-c", script],
                "resources": {"requests": {"cpu": "10m", "memory": "16Mi"}, "limits": {"memory": "64Mi"}},
                "securityContext": {"readOnlyRootFilesystem": true, "allowPrivilegeEscalation": false},
                "volumeMounts": mounts
            }],
            "volumes": volumes
        }
    }))?;
    let pp = PostParams { dry_run, field_manager: Some("k8s-galaxy".into()) };
    let created = api.create(&pp, &pod).await.with_context(|| format!("creating the measuring pod on {node}"))?;
    if dry_run {
        return Ok(BTreeMap::new());
    }
    let name = created.metadata.name.clone().ok_or_else(|| anyhow!("pod without a name"))?;
    let outcome = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let p = api.get(&name).await?;
            match p.status.as_ref().and_then(|s| s.phase.as_deref()) {
                Some("Succeeded") => break,
                Some("Failed") => bail!("measuring pod failed on {node}"),
                _ => {}
            }
            // A directory that does not exist keeps the pod in ContainerCreating forever.
            let waiting = p
                .status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .and_then(|c| c.first())
                .and_then(|c| c.state.as_ref())
                .and_then(|s| s.waiting.as_ref())
                .and_then(|w| w.message.clone());
            if tokio::time::Instant::now() > deadline {
                bail!("measuring pod on {node} did not finish: {}", waiting.unwrap_or_else(|| "timeout".into()));
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        let logs = api.logs(&name, &LogParams::default()).await?;
        Ok(parse_measure(&logs))
    }
    .await;
    // Always clean up, whatever happened.
    let _ = api.delete(&name, &DeleteParams::default()).await;
    outcome
}

fn parse_measure(out: &str) -> BTreeMap<usize, Measured> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let [i, used, device, size, fs_used] = f.as_slice() else { return None };
            Some((
                i.parse().ok()?,
                Measured {
                    used: used.parse::<i64>().ok()? * 1024,
                    device: device.to_string(),
                    fs_size: size.parse::<i64>().ok()? * 1024,
                    fs_used: fs_used.parse::<i64>().ok()? * 1024,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations() {
        let ips: BTreeSet<String> = ["10.0.0.2".to_string()].into();
        assert_eq!(locate("hostPath", "", "/k8s/pv/x", Some("n1"), &ips), Location::InCluster("node disk (hostPath) on n1".into()));
        assert_eq!(locate("csi", "ebs.csi.aws.com", "vol-1", None, &ips).short(), "external");
        assert_eq!(locate("csi", "driver.longhorn.io", "pvc-1", None, &ips), Location::InCluster("Longhorn".into()));
        assert_eq!(locate("csi", "rook-ceph.rbd.csi.ceph.com", "", None, &ips), Location::InCluster("Rook Ceph".into()));
        assert_eq!(locate("csi", "csi.hetzner.cloud", "123", None, &ips), Location::External("Hetzner Cloud Volume".into()));
        assert_eq!(locate("nfs", "", "10.0.0.2:/exports", None, &ips).short(), "in-cluster");
        assert_eq!(locate("nfs", "", "192.168.1.9:/exports", None, &ips), Location::External("NFS 192.168.1.9".into()));
    }

    #[test]
    fn parses_du_df() {
        let m = parse_measure("0 2048 /dev/sda1 157196532 32847512\n1  /dev/sda1 157196532 32847512\n");
        assert_eq!(m[&0].used, 2048 * 1024);
        assert_eq!(m[&0].device, "/dev/sda1");
        assert!(!m.contains_key(&1));
    }
}
