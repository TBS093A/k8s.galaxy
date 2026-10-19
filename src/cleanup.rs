//! Cleanup: objects that are probably left over — released volumes, finished pods, ACME solver
//! leftovers, broken routes, long-failing certificates, unused config — with the reason, and a
//! delete action behind a confirmation.

use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, PersistentVolume, PersistentVolumeClaim, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::jiff::Timestamp;
use kube::api::{DeleteParams, ListParams, PropagationPolicy};
use kube::{Api, Client};

use crate::model::Health;
use crate::ops::NetInfo;
use crate::storage::StorageInfo;

#[derive(Clone, Debug)]
pub struct Finding {
    /// Kubernetes kind: "PersistentVolume", "Pod", "Ingress"…
    pub kind: &'static str,
    pub namespace: String,
    pub name: String,
    /// Section in the list: "Released volumes", "Finished pods"…
    pub category: &'static str,
    pub severity: Health,
    pub reason: String,
    pub age_days: Option<i64>,
    /// Safe enough to offer `d` (delete) for.
    pub deletable: bool,
    /// What else to know before deleting (Bad = data loss).
    pub notes: Vec<(Health, String)>,
}

impl Finding {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.kind, self.namespace, self.name)
    }
}

const SYSTEM_NAMESPACES: &[&str] = &["kube-system", "kube-public", "kube-node-lease"];

fn days_since(meta: &ObjectMeta) -> Option<i64> {
    let t = meta.creation_timestamp.as_ref()?.0;
    Some((Timestamp::now().as_second() - t.as_second()) / 86_400)
}

fn ns_of(meta: &ObjectMeta) -> String {
    meta.namespace.clone().unwrap_or_default()
}

fn name_of(meta: &ObjectMeta) -> String {
    meta.name.clone().unwrap_or_default()
}

/// What pods reference: ConfigMaps and Secrets, per namespace.
#[derive(Default)]
struct Refs {
    configmaps: BTreeSet<(String, String)>,
    secrets: BTreeSet<(String, String)>,
}

fn pod_refs(pods: &[Pod], refs: &mut Refs) {
    for p in pods {
        let ns = ns_of(&p.metadata);
        let Some(spec) = &p.spec else { continue };
        for s in spec.image_pull_secrets.iter().flatten() {
            refs.secrets.insert((ns.clone(), s.name.clone()));
        }
        for v in spec.volumes.iter().flatten() {
            if let Some(c) = &v.config_map {
                refs.configmaps.insert((ns.clone(), c.name.clone()));
            }
            if let Some(s) = &v.secret
                && let Some(n) = &s.secret_name
            {
                refs.secrets.insert((ns.clone(), n.clone()));
            }
            for src in v.projected.iter().flat_map(|p| p.sources.iter().flatten()) {
                if let Some(c) = &src.config_map {
                    refs.configmaps.insert((ns.clone(), c.name.clone()));
                }
                if let Some(s) = &src.secret {
                    refs.secrets.insert((ns.clone(), s.name.clone()));
                }
            }
        }
        let containers = spec.containers.iter().chain(spec.init_containers.iter().flatten());
        for c in containers {
            for e in c.env_from.iter().flatten() {
                if let Some(c) = &e.config_map_ref {
                    refs.configmaps.insert((ns.clone(), c.name.clone()));
                }
                if let Some(s) = &e.secret_ref {
                    refs.secrets.insert((ns.clone(), s.name.clone()));
                }
            }
            for e in c.env.iter().flatten() {
                let Some(from) = &e.value_from else { continue };
                if let Some(c) = &from.config_map_key_ref {
                    refs.configmaps.insert((ns.clone(), c.name.clone()));
                }
                if let Some(s) = &from.secret_key_ref {
                    refs.secrets.insert((ns.clone(), s.name.clone()));
                }
            }
        }
    }
}

pub async fn scan(client: &Client, net: &NetInfo, storage: &StorageInfo) -> Result<Vec<Finding>> {
    let lp = ListParams::default();
    let (pods, cms, secrets, svcs, jobs, ingresses, namespaces) = tokio::try_join!(
        async { Api::<Pod>::all(client.clone()).list(&lp).await.context("listing pods") },
        async { Api::<ConfigMap>::all(client.clone()).list_metadata(&lp).await.context("listing configmaps") },
        // Metadata only: Secret values never leave the API server.
        async { Api::<Secret>::all(client.clone()).list_metadata(&lp).await.context("listing secrets") },
        async { Api::<Service>::all(client.clone()).list(&lp).await.context("listing services") },
        async { Api::<Job>::all(client.clone()).list(&lp).await.context("listing jobs") },
        async { Api::<Ingress>::all(client.clone()).list(&lp).await.context("listing ingresses") },
        async { Api::<Namespace>::all(client.clone()).list_metadata(&lp).await.context("listing namespaces") },
    )?;
    Ok(findings(&pods.items, &cms.items, &secrets.items, &svcs.items, &jobs.items, &ingresses.items, &namespaces.items, net, storage))
}

#[allow(clippy::too_many_arguments)]
pub fn findings(
    pods: &[Pod],
    configmaps: &[kube::core::PartialObjectMeta<ConfigMap>],
    secrets: &[kube::core::PartialObjectMeta<Secret>],
    services: &[Service],
    jobs: &[Job],
    ingresses: &[Ingress],
    namespaces: &[kube::core::PartialObjectMeta<Namespace>],
    net: &NetInfo,
    storage: &StorageInfo,
) -> Vec<Finding> {
    let mut out = vec![];
    let finding = |kind, namespace: String, name: String, category, severity, reason: String| Finding {
        kind,
        namespace,
        name,
        category,
        severity,
        reason,
        age_days: None,
        deletable: true,
        notes: vec![],
    };

    // Volumes.
    for v in &storage.volumes {
        match (v.pv_phase.as_deref(), &v.pv, &v.claim) {
            (Some("Released"), Some(pv), _) => {
                let mut f = finding(
                    "PersistentVolume",
                    String::new(),
                    pv.clone(),
                    "Released volumes",
                    Health::Warn,
                    format!("claim {} was deleted; {} kept the volume", v.released_from.clone().unwrap_or_default(), v.reclaim),
                );
                f.notes.push((
                    Health::Bad,
                    match v.kind.as_str() {
                        "hostPath" | "local" => format!(
                            "deleting the PV does not delete the data: remove {} on {} by hand if it is not needed",
                            v.detail,
                            v.node.clone().unwrap_or_else(|| "its node".into())
                        ),
                        _ => format!(
                            "with reclaim policy {} the backing storage ({}) stays — delete it at the provider if not needed",
                            v.reclaim,
                            v.location.what()
                        ),
                    },
                ));
                out.push(f);
            }
            (Some("Available"), Some(pv), None) => out.push(finding(
                "PersistentVolume",
                String::new(),
                pv.clone(),
                "Unclaimed volumes",
                Health::Ok,
                "Available, no claim has bound it".into(),
            )),
            (_, _, Some(claim)) if v.claim_phase.as_deref() == Some("Bound") && v.mounted_by.is_empty() => {
                let mut f = finding(
                    "PersistentVolumeClaim",
                    v.namespace.clone().unwrap_or_default(),
                    claim.clone(),
                    "Unmounted claims",
                    Health::Ok,
                    "bound but no pod mounts it".into(),
                );
                f.notes.push(if v.reclaim == "Delete" {
                    (Health::Bad, "reclaim policy Delete: deleting the claim DESTROYS the volume and its data".into())
                } else {
                    (Health::Warn, format!("reclaim policy {}: the volume and its data stay, as a Released PV", v.reclaim))
                });
                out.push(f);
            }
            _ => {}
        }
    }

    // Finished pods (Jobs keep theirs for logs; their owners decide).
    for p in pods {
        let phase = p.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or_default();
        let owned_by_job = p.metadata.owner_references.iter().flatten().any(|o| o.kind == "Job");
        if !matches!(phase, "Succeeded" | "Failed") || owned_by_job {
            continue;
        }
        let reason = p.status.as_ref().and_then(|s| s.reason.clone()).unwrap_or_else(|| phase.to_string());
        let mut f =
            finding("Pod", ns_of(&p.metadata), name_of(&p.metadata), "Finished pods", Health::Ok, format!("{reason}, not owned by a Job"));
        f.age_days = days_since(&p.metadata);
        out.push(f);
    }

    // Jobs finished long ago without a TTL (CronJobs prune their own history).
    for j in jobs {
        let owned_by_cron = j.metadata.owner_references.iter().flatten().any(|o| o.kind == "CronJob");
        let ttl = j.spec.as_ref().and_then(|s| s.ttl_seconds_after_finished).is_some();
        let done =
            j.status.as_ref().and_then(|s| s.completion_time.as_ref()).map(|t| (Timestamp::now().as_second() - t.0.as_second()) / 86_400);
        if let Some(days) = done
            && days >= 7
            && !owned_by_cron
            && !ttl
        {
            let mut f = finding(
                "Job",
                ns_of(&j.metadata),
                name_of(&j.metadata),
                "Old jobs",
                Health::Ok,
                format!("completed {days} days ago, no ttlSecondsAfterFinished"),
            );
            f.age_days = Some(days);
            f.notes.push((Health::Ok, "its pods are deleted with it".into()));
            out.push(f);
        }
    }

    // ACME HTTP-01 solvers: cert-manager removes them when the challenge ends; one without an
    // active challenge is a leftover, one with a stuck challenge points at a failing certificate.
    let challenge_hosts: BTreeSet<String> = net
        .certs
        .iter()
        .flat_map(|c| c.challenges.iter())
        .filter_map(|ch| ch.split_whitespace().nth(1).map(|h| h.trim_end_matches(':').to_string()))
        .collect();
    let solver = |meta: &ObjectMeta| meta.labels.as_ref().is_some_and(|l| l.contains_key("acme.cert-manager.io/http01-solver"));
    for ing in ingresses.iter().filter(|i| solver(&i.metadata) || name_of(&i.metadata).starts_with("cm-acme-http-solver-")) {
        let hosts: Vec<String> =
            ing.spec.as_ref().and_then(|s| s.rules.clone()).unwrap_or_default().into_iter().filter_map(|r| r.host).collect();
        let active = hosts.iter().any(|h| challenge_hosts.contains(h));
        let mut f = finding(
            "Ingress",
            ns_of(&ing.metadata),
            name_of(&ing.metadata),
            "ACME solver leftovers",
            if active { Health::Warn } else { Health::Ok },
            if active {
                format!("challenge for {} is still pending — the certificate is failing, fix that first", hosts.join(","))
            } else {
                format!("no challenge for {} is active any more", hosts.join(","))
            },
        );
        f.age_days = days_since(&ing.metadata);
        f.deletable = !active;
        out.push(f);
    }

    // Routes to Services that do not exist.
    for r in net.routes.iter().filter(|r| r.kind == crate::ops::RouteKind::Ingress) {
        let missing: Vec<&str> = r.backends.iter().filter(|b| b.endpoints.is_none()).map(|b| b.service.as_str()).collect();
        if !missing.is_empty() && !r.name.starts_with("cm-acme-http-solver-") {
            let mut f = finding(
                "Ingress",
                r.namespace.clone(),
                r.name.clone(),
                "Broken routes",
                Health::Warn,
                format!("{} → service {} does not exist", r.hosts.join(","), missing.join(",")),
            );
            f.notes.push((Health::Warn, "if the app was removed, the Ingress (and its Certificate) can go too".into()));
            out.push(f);
        }
    }

    // Certificates failing for a week or more.
    for c in net.certs.iter().filter(|c| !c.ready && !c.name.ends_with(')')) {
        let days = c.broken_days.unwrap_or(0);
        if days < 7 {
            continue;
        }
        let mut f = finding(
            "Certificate",
            c.namespace.clone(),
            c.name.clone(),
            "Failing certificates",
            Health::Warn,
            format!("not ready for {days} days ({}): {}", c.reason, c.dns_names.join(",")),
        );
        f.age_days = Some(days);
        f.notes.push((Health::Ok, format!("its Secret {} is left in place; delete it too if the host is gone", c.secret)));
        out.push(f);
    }

    // Unused ConfigMaps and Secrets (best effort: only pods and Ingress TLS count as users).
    let mut refs = Refs::default();
    pod_refs(pods, &mut refs);
    for i in ingresses {
        for t in i.spec.as_ref().and_then(|s| s.tls.clone()).unwrap_or_default() {
            if let Some(s) = t.secret_name {
                refs.secrets.insert((ns_of(&i.metadata), s));
            }
        }
    }
    for c in &net.certs {
        refs.secrets.insert((c.namespace.clone(), c.secret.clone()));
    }
    let skip = |meta: &ObjectMeta| {
        let ns = ns_of(meta);
        let labels = meta.labels.clone().unwrap_or_default();
        let annotations = meta.annotations.clone().unwrap_or_default();
        // Managed by Helm or Argo CD: part of a release/app, recreated if deleted — not a leftover.
        let managed = labels.get("app.kubernetes.io/managed-by").is_some_and(|m| m == "Helm")
            || annotations.contains_key("meta.helm.sh/release-name")
            || annotations.contains_key("argocd.argoproj.io/tracking-id");
        // Read through the API by sidecars and operators: Grafana dashboards/datasources, Jenkins JCasC, leader locks.
        let watched = labels.keys().any(|k| k.contains("grafana_") || k.contains("jenkins-config") || k.contains("jcasc"))
            || ["-rbac-cm", "-leader-election", "-lock", "-jcasc-config"]
                .iter()
                .any(|s| meta.name.as_deref().unwrap_or_default().ends_with(s));
        SYSTEM_NAMESPACES.contains(&ns.as_str()) || meta.owner_references.as_ref().is_some_and(|o| !o.is_empty()) || managed || watched
    };
    for cm in configmaps {
        let (ns, name) = (ns_of(&cm.metadata), name_of(&cm.metadata));
        if skip(&cm.metadata)
            || name == "kube-root-ca.crt"
            || name.ends_with("-leader-election")
            || refs.configmaps.contains(&(ns.clone(), name.clone()))
        {
            continue;
        }
        let mut f = finding("ConfigMap", ns, name, "Possibly unused config", Health::Ok, "no pod mounts or reads it".into());
        f.age_days = days_since(&cm.metadata);
        f.notes.push((Health::Warn, "controllers and apps can read ConfigMaps through the API — check before deleting".into()));
        out.push(f);
    }
    for s in secrets {
        let (ns, name) = (ns_of(&s.metadata), name_of(&s.metadata));
        let helm = name.starts_with("sh.helm.release.");
        let sa_token = s.metadata.annotations.as_ref().is_some_and(|a| a.contains_key("kubernetes.io/service-account.name"));
        if skip(&s.metadata) || helm || sa_token || refs.secrets.contains(&(ns.clone(), name.clone())) {
            continue;
        }
        let mut f =
            finding("Secret", ns, name, "Possibly unused config", Health::Ok, "no pod mounts or reads it, no Ingress uses it".into());
        f.age_days = days_since(&s.metadata);
        f.notes.push((Health::Warn, "operators, Argo CD and registries read Secrets through the API — check before deleting".into()));
        out.push(f);
    }

    // Services whose selector matches no pod.
    for svc in services {
        let ns = ns_of(&svc.metadata);
        let Some(sel) = svc.spec.as_ref().and_then(|s| s.selector.clone()).filter(|s| !s.is_empty()) else { continue };
        if SYSTEM_NAMESPACES.contains(&ns.as_str()) || name_of(&svc.metadata).starts_with("cm-acme-http-solver-") {
            continue;
        }
        let matches = pods
            .iter()
            .any(|p| ns_of(&p.metadata) == ns && p.metadata.labels.as_ref().is_some_and(|l| sel.iter().all(|(k, v)| l.get(k) == Some(v))));
        if !matches {
            let sel: Vec<String> = sel.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let mut f = finding(
                "Service",
                ns,
                name_of(&svc.metadata),
                "Services without pods",
                Health::Warn,
                format!("selector {} matches no pod", sel.join(",")),
            );
            f.age_days = days_since(&svc.metadata);
            out.push(f);
        }
    }

    // Empty namespaces: shown, never offered for deletion from here.
    let busy: BTreeSet<String> = pods
        .iter()
        .map(|p| ns_of(&p.metadata))
        .chain(storage.volumes.iter().filter_map(|v| v.namespace.clone()))
        .chain(services.iter().map(|s| ns_of(&s.metadata)))
        .collect();
    for n in namespaces {
        let name = name_of(&n.metadata);
        if busy.contains(&name) || SYSTEM_NAMESPACES.contains(&name.as_str()) || name == "default" {
            continue;
        }
        let mut f = finding("Namespace", String::new(), name, "Empty namespaces", Health::Ok, "no pods, services or volumes".into());
        f.age_days = days_since(&n.metadata);
        f.deletable = false;
        f.notes.push((Health::Ok, "not deletable from here: deleting a namespace deletes everything in it".into()));
        out.push(f);
    }

    out.sort_by(|a, b| (a.category, a.severity, &a.namespace, &a.name).cmp(&(b.category, b.severity, &b.namespace, &b.name)));
    out
}

/// Delete one object (foreground propagation, so owned objects go too).
pub async fn delete(client: &Client, f: &Finding, dry_run: bool) -> Result<()> {
    let dp = DeleteParams { dry_run, propagation_policy: Some(PropagationPolicy::Background), ..Default::default() };
    let ns = f.namespace.as_str();
    macro_rules! del {
        ($t:ty, $api:expr) => {{
            let api: Api<$t> = $api;
            api.delete(&f.name, &dp).await.map(|_| ())
        }};
    }
    let res = match f.kind {
        "PersistentVolume" => del!(PersistentVolume, Api::all(client.clone())),
        "PersistentVolumeClaim" => del!(PersistentVolumeClaim, Api::namespaced(client.clone(), ns)),
        "Pod" => del!(Pod, Api::namespaced(client.clone(), ns)),
        "Job" => del!(Job, Api::namespaced(client.clone(), ns)),
        "Ingress" => del!(Ingress, Api::namespaced(client.clone(), ns)),
        "ConfigMap" => del!(ConfigMap, Api::namespaced(client.clone(), ns)),
        "Secret" => del!(Secret, Api::namespaced(client.clone(), ns)),
        "Service" => del!(Service, Api::namespaced(client.clone(), ns)),
        "Certificate" => {
            let ar = kube::discovery::ApiResource {
                group: "cert-manager.io".into(),
                version: "v1".into(),
                api_version: "cert-manager.io/v1".into(),
                kind: "Certificate".into(),
                plural: "certificates".into(),
            };
            Api::<kube::api::DynamicObject>::namespaced_with(client.clone(), ns, &ar).delete(&f.name, &dp).await.map(|_| ())
        }
        other => return Err(anyhow!("deleting {other} is not supported here")),
    };
    res.with_context(|| format!("deleting {} {}", f.kind, f.name))
}
