//! Every cluster this user can reach: contexts from `$KUBECONFIG`, `~/.kube/config` and any other
//! kubeconfig file in `~/.kube/`, plus a quick probe of each (reachable? version? what it runs on?).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use k8s_openapi::api::rbac::v1::ClusterRoleBinding;
use kube::api::ListParams;
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};

use crate::infra::Infra;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterEntry {
    pub file: PathBuf,
    pub context: String,
    /// API server URL of the context's cluster.
    pub server: String,
    pub user: String,
    /// What the list shows: the file name, plus the context when a file holds several.
    pub label: String,
    /// The context kubectl would use by default.
    pub default: bool,
}

#[derive(Clone, Debug)]
pub struct Probe {
    pub version: String,
    pub nodes: usize,
    pub infra: Infra,
}

/// Files that are clearly not live kubeconfigs (backups, editor leftovers, caches).
fn skip(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    name.starts_with('.')
        || [".bak", ".old", ".orig", ".swp", ".tmp", ".lock", "~", ".backup"].iter().any(|s| name.ends_with(s))
        || name.contains(".bak.")
}

fn home_kube() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".kube"))
}

/// Kubeconfig files in the order kubectl would consider them, then the rest of `~/.kube/`.
pub fn candidate_files() -> Vec<PathBuf> {
    let mut files = vec![];
    if let Some(list) = std::env::var_os("KUBECONFIG") {
        files.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    if let Some(dir) = home_kube() {
        files.push(dir.join("config"));
        if let Ok(read) = std::fs::read_dir(&dir) {
            let mut extra: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.is_file() && !skip(p)).collect();
            extra.sort();
            files.extend(extra);
        }
    }
    let mut seen = BTreeSet::new();
    files.retain(|p| p.is_file() && seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())));
    files
}

pub(crate) fn stem(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("kubeconfig");
    for ext in [".kubeconfig", ".config", ".conf", ".yaml", ".yml"] {
        if let Some(s) = name.strip_suffix(ext) {
            return s.to_string();
        }
    }
    name.to_string()
}

/// All contexts of all kubeconfig files found.
pub fn discover() -> Vec<ClusterEntry> {
    let files = candidate_files();
    // The default context: current-context of the first file that sets one (kubectl's merge rule).
    let mut default: Option<(PathBuf, String)> = None;
    let mut out = vec![];
    for file in files {
        let Ok(kc) = Kubeconfig::read_from(&file) else { continue };
        if kc.contexts.is_empty() {
            continue;
        }
        let in_default_chain = std::env::var_os("KUBECONFIG")
            .map_or_else(|| home_kube().is_some_and(|d| d.join("config") == file), |list| std::env::split_paths(&list).any(|p| p == file));
        if default.is_none()
            && in_default_chain
            && let Some(cur) = &kc.current_context
        {
            default = Some((file.clone(), cur.clone()));
        }
        let several = kc.contexts.len() > 1;
        for ctx in &kc.contexts {
            let Some(c) = &ctx.context else { continue };
            let server = kc
                .clusters
                .iter()
                .find(|cl| cl.name == c.cluster)
                .and_then(|cl| cl.cluster.as_ref())
                .and_then(|cl| cl.server.clone())
                .unwrap_or_default();
            let base = if file.file_name().is_some_and(|n| n == "config") { ctx.name.clone() } else { stem(&file) };
            out.push(ClusterEntry {
                label: if several && base != ctx.name { format!("{base}/{}", ctx.name) } else { base },
                file: file.clone(),
                context: ctx.name.clone(),
                server,
                user: c.user.clone().unwrap_or_default(),
                default: false,
            });
        }
    }
    if let Some((file, ctx)) = default {
        for e in &mut out {
            e.default = e.file == file && e.context == ctx;
        }
    }
    out
}

pub async fn client_for(file: &Path, context: &str, quick: bool) -> Result<Client> {
    let kc = Kubeconfig::read_from(file).with_context(|| format!("reading {}", file.display()))?;
    let opts = KubeConfigOptions { context: Some(context.to_string()), ..Default::default() };
    let mut config = Config::from_custom_kubeconfig(kc, &opts).await.context("building kube config")?;
    if quick {
        config.connect_timeout = Some(Duration::from_secs(4));
        config.read_timeout = Some(Duration::from_secs(8));
    }
    Ok(Client::try_from(config)?)
}

/// Version, nodes and ClusterRoleBinding names of a cluster (read-only). Nodes are returned so
/// the caller can match them against cloud inventories before classifying; binding names hint at
/// the installer (kubespray). Missing RBAC access to bindings is not an error.
pub async fn probe_raw(entry: &ClusterEntry) -> Result<(String, Vec<Node>, Vec<String>)> {
    let client = client_for(&entry.file, &entry.context, true).await?;
    let (version, nodes, bindings) = tokio::join!(
        async { client.apiserver_version().await.context("GET /version") },
        async { Api::<Node>::all(client.clone()).list(&ListParams::default().limit(500)).await.context("listing nodes") },
        async { Api::<ClusterRoleBinding>::all(client.clone()).list_metadata(&ListParams::default()).await },
    );
    let hints = bindings.map(|b| b.items.into_iter().filter_map(|b| b.metadata.name).collect()).unwrap_or_default();
    Ok((version?.git_version, nodes?.items, hints))
}

/// Short form of an API server URL for the list: host:port.
pub fn short_server(server: &str) -> String {
    server.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string()
}

/// `~/…` instead of the home directory.
pub fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_backups_and_strips_extensions() {
        assert!(skip(Path::new("/h/.kube/hetzner.k8s.admin.conf.bak")));
        assert!(skip(Path::new("/h/.kube/config.bak.2024")));
        assert!(skip(Path::new("/h/.kube/.swp")));
        assert!(!skip(Path::new("/h/.kube/hetzner.k8s.admin.conf")));
        assert_eq!(stem(Path::new("/h/.kube/hetzner.k8s.admin.conf")), "hetzner.k8s.admin");
        assert_eq!(stem(Path::new("/h/.kube/local.ubuntu.cluster.config")), "local.ubuntu.cluster");
        assert_eq!(short_server("https://91.98.1.1:6443/"), "91.98.1.1:6443");
    }
}
