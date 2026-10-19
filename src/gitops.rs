//! Argo CD Applications: sync / health / drift, last operation, and the sync + refresh actions.

use anyhow::{Context, Result, anyhow, bail};
use kube::api::{DynamicObject, ListParams, Patch, PatchParams};
use kube::discovery::ApiResource;
use kube::{Api, Client};
use serde_json::{Value, json};

use crate::model::Health;

#[derive(Clone, Debug)]
pub struct AppResource {
    pub kind: String,
    pub namespace: String,
    pub name: String,
    /// Synced / OutOfSync / Unknown.
    pub sync: String,
    /// Healthy / Progressing / Degraded / Suspended / Missing / Unknown, empty for resources without health.
    pub health: String,
    pub message: String,
}

impl AppResource {
    pub fn drifted(&self) -> bool {
        self.sync == "OutOfSync"
    }
}

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub namespace: String,
    pub name: String,
    pub project: String,
    pub sync: String,
    pub health: String,
    pub health_message: String,
    /// Synced revision(s), shortened.
    pub revision: String,
    /// "repo path@target" per source.
    pub sources: Vec<String>,
    pub destination: String,
    /// "auto (prune, selfHeal)" or "manual".
    pub policy: String,
    pub auto: bool,
    pub op_phase: Option<String>,
    pub op_message: String,
    pub op_by: String,
    pub op_when: String,
    pub resources: Vec<AppResource>,
    pub conditions: Vec<String>,
}

impl AppInfo {
    pub fn health(&self) -> Health {
        let op_failed = matches!(self.op_phase.as_deref(), Some("Failed") | Some("Error"));
        if op_failed || matches!(self.health.as_str(), "Degraded" | "Missing") || self.conditions.iter().any(|c| c.contains("Error")) {
            Health::Bad
        } else if self.sync != "Synced" || self.health != "Healthy" || self.op_phase.as_deref() == Some("Running") {
            Health::Warn
        } else {
            Health::Ok
        }
    }

    pub fn drift(&self) -> Vec<&AppResource> {
        self.resources.iter().filter(|r| r.drifted()).collect()
    }

    pub fn running(&self) -> bool {
        self.op_phase.as_deref() == Some("Running")
    }
}

fn s(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur.as_str().map(str::to_string).unwrap_or_default()
}

fn short_repo(url: &str) -> String {
    let u = url.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<&str> = u.rsplitn(3, '/').collect();
    match parts.as_slice() {
        [repo, owner, _] if !owner.is_empty() && !owner.ends_with(':') => format!("{owner}/{repo}"),
        [repo, ..] => repo.to_string(),
        [] => u.to_string(),
    }
}

fn source_label(src: &Value) -> String {
    let repo = short_repo(&s(src, &["repoURL"]));
    let target = s(src, &["targetRevision"]);
    let what = if let Some(chart) = src["chart"].as_str() {
        format!("chart {chart}")
    } else if let Some(r) = src["ref"].as_str() {
        format!("ref ${r}")
    } else {
        s(src, &["path"])
    };
    format!("{repo} {what}@{}", if target.is_empty() { "HEAD" } else { &target })
}

pub fn parse(obj: &DynamicObject) -> AppInfo {
    let v = &obj.data;
    let spec = &v["spec"];
    let status = &v["status"];
    let sources: Vec<String> = match spec["sources"].as_array() {
        Some(list) => list.iter().map(source_label).collect(),
        None => vec![source_label(&spec["source"])],
    };
    let revision = match status["sync"]["revisions"].as_array() {
        Some(list) if !list.is_empty() => {
            let mut revs: Vec<String> = list.iter().filter_map(|r| r.as_str()).map(|r| r.chars().take(7).collect()).collect();
            revs.dedup();
            revs.join(",")
        }
        _ => s(status, &["sync", "revision"]).chars().take(7).collect(),
    };
    let automated = &spec["syncPolicy"]["automated"];
    let auto = automated.is_object() && automated["enabled"].as_bool() != Some(false);
    let policy = if auto {
        let flags: Vec<&str> = ["prune", "selfHeal"].into_iter().filter(|f| automated[*f].as_bool() == Some(true)).collect();
        if flags.is_empty() { "auto".to_string() } else { format!("auto ({})", flags.join(", ")) }
    } else {
        "manual".to_string()
    };
    let op = &status["operationState"];
    let by = &op["operation"]["initiatedBy"];
    let op_by = if by["automated"].as_bool() == Some(true) {
        "automated".to_string()
    } else {
        let u = s(by, &["username"]);
        if u.is_empty() { "user".into() } else { u }
    };
    let when = s(op, &["finishedAt"]);
    let when = if when.is_empty() { s(op, &["startedAt"]) } else { when };
    let resources = status["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| AppResource {
            kind: s(r, &["kind"]),
            namespace: s(r, &["namespace"]),
            name: s(r, &["name"]),
            sync: s(r, &["status"]),
            health: s(r, &["health", "status"]),
            message: s(r, &["health", "message"]),
        })
        .collect();
    let dest = &spec["destination"];
    let server = s(dest, &["server"]);
    let server = if server.is_empty() { s(dest, &["name"]) } else { server };
    AppInfo {
        namespace: obj.metadata.namespace.clone().unwrap_or_default(),
        name: obj.metadata.name.clone().unwrap_or_default(),
        project: s(spec, &["project"]),
        sync: Some(s(status, &["sync", "status"])).filter(|x| !x.is_empty()).unwrap_or_else(|| "Unknown".into()),
        health: Some(s(status, &["health", "status"])).filter(|x| !x.is_empty()).unwrap_or_else(|| "Unknown".into()),
        health_message: s(status, &["health", "message"]),
        revision,
        sources,
        destination: format!(
            "{} → ns {}",
            if server == "https://kubernetes.default.svc" { "in-cluster".to_string() } else { server },
            s(dest, &["namespace"])
        ),
        policy,
        auto,
        op_phase: Some(s(op, &["phase"])).filter(|x| !x.is_empty()),
        op_message: s(op, &["message"]),
        op_by,
        op_when: when.parse().ok().map(crate::ops::ago).unwrap_or_default(),
        resources,
        conditions: status["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| format!("{}: {}", s(c, &["type"]), s(c, &["message"])))
            .collect(),
    }
}

pub async fn list(client: &Client, ar: &ApiResource) -> Result<Vec<AppInfo>> {
    let apps = Api::<DynamicObject>::all_with(client.clone(), ar)
        .list(&ListParams::default())
        .await
        .context("listing Argo CD applications")?;
    Ok(apps.items.iter().map(parse).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppAction {
    Sync,
    SyncPrune,
    Refresh,
    HardRefresh,
}

impl AppAction {
    pub fn label(self) -> &'static str {
        match self {
            AppAction::Sync => "sync",
            AppAction::SyncPrune => "sync --prune",
            AppAction::Refresh => "refresh",
            AppAction::HardRefresh => "hard refresh",
        }
    }

    pub fn key(self) -> char {
        match self {
            AppAction::Sync => 's',
            AppAction::SyncPrune => 'S',
            AppAction::Refresh => 'R',
            AppAction::HardRefresh => 'H',
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            AppAction::Sync => "applies the target revision from Git (resources no longer in Git are kept)",
            AppAction::SyncPrune => "applies the target revision and DELETES resources that are no longer in Git",
            AppAction::Refresh => "re-compares the live state with Git (no changes applied)",
            AppAction::HardRefresh => "re-renders manifests bypassing the repo-server cache, then compares",
        }
    }

    pub const ALL: [AppAction; 4] = [AppAction::Sync, AppAction::SyncPrune, AppAction::Refresh, AppAction::HardRefresh];
}

/// Sync = set `.operation` (what the Argo CD API server does for `argocd app sync`);
/// refresh = the `argocd.argoproj.io/refresh` annotation the controller watches.
pub async fn perform(client: &Client, ar: &ApiResource, namespace: &str, name: &str, action: AppAction, dry_run: bool) -> Result<()> {
    let api = Api::<DynamicObject>::namespaced_with(client.clone(), namespace, ar);
    let pp = PatchParams { field_manager: Some("k8s-galaxy".into()), dry_run, ..Default::default() };
    let patch = match action {
        AppAction::Refresh | AppAction::HardRefresh => {
            let level = if action == AppAction::HardRefresh { "hard" } else { "normal" };
            json!({"metadata": {"annotations": {"argocd.argoproj.io/refresh": level}}})
        }
        AppAction::Sync | AppAction::SyncPrune => {
            let app = api.get(name).await?;
            if app.data["operation"].is_object() || app.data["status"]["operationState"]["phase"] == "Running" {
                bail!("an operation is already running on {name}");
            }
            if app.data["spec"].is_null() {
                return Err(anyhow!("application {name} has no spec"));
            }
            json!({"operation": {
                "initiatedBy": {"username": "k8s-galaxy"},
                "info": [{"name": "Reason", "value": "sync from k8s-galaxy"}],
                "sync": {"prune": action == AppAction::SyncPrune, "syncStrategy": {"hook": {}}},
            }})
        }
    };
    api.patch(name, &pp, &Patch::Merge(patch)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_multi_source_app() {
        let obj: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "Application",
            "metadata": {"name": "gitea", "namespace": "argocd"},
            "spec": {"project": "default",
                "sources": [{"repoURL": "http://gitea/acme/platform-gitops.git", "path": "apps/gitea", "targetRevision": "master"},
                            {"repoURL": "https://charts.example.com", "chart": "gitea", "targetRevision": "10.1.0"}],
                "destination": {"server": "https://kubernetes.default.svc", "namespace": "development"},
                "syncPolicy": {"automated": {"prune": true, "selfHeal": true}}},
            "status": {"sync": {"status": "OutOfSync", "revisions": ["7a42213322f9", "7a42213322f9"]},
                "health": {"status": "Healthy"},
                "operationState": {"phase": "Succeeded", "message": "ok", "finishedAt": "2026-09-25T17:49:16Z",
                    "operation": {"initiatedBy": {"automated": true}}},
                "resources": [{"kind": "Service", "name": "gitea", "namespace": "development", "status": "OutOfSync", "health": {"status": "Healthy"}},
                              {"kind": "ConfigMap", "name": "gitea", "namespace": "development", "status": "Synced"}]}
        }))
        .unwrap();
        let a = parse(&obj);
        assert_eq!(a.sources, vec!["acme/platform-gitops apps/gitea@master", "charts.example.com chart gitea@10.1.0"]);
        assert_eq!(a.revision, "7a42213");
        assert_eq!(a.policy, "auto (prune, selfHeal)");
        assert_eq!(a.destination, "in-cluster → ns development");
        assert_eq!(a.drift().len(), 1);
        assert_eq!(a.health(), Health::Warn);
        assert_eq!(a.op_by, "automated");
    }
}
