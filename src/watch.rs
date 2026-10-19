//! Watch API: every frequently changing list (nodes, pods, events, workloads) is a `watch`
//! stream feeding a local cache. Snapshots read the caches — no request per refresh — and the
//! API server only sends what changed.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{ControllerRevision, DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use k8s_openapi::serde::de::DeserializeOwned;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client, Resource};

use crate::source::AbortOnDrop;

type Errors = Arc<Mutex<BTreeMap<&'static str, String>>>;

pub struct Watches {
    pub nodes: Store<Node>,
    pub pods: Store<Pod>,
    pub events: Store<Event>,
    pub deployments: Store<Deployment>,
    pub replicasets: Store<ReplicaSet>,
    pub daemonsets: Store<DaemonSet>,
    pub statefulsets: Store<StatefulSet>,
    pub revisions: Store<ControllerRevision>,
    /// Latest error per resource (cleared by the next good event); the watch retries by itself.
    errors: Errors,
    _tasks: Vec<AbortOnDrop>,
}

/// Spawn a watch + reflector for one resource kind.
fn watch<K>(client: &Client, name: &'static str, errors: &Errors, tasks: &mut Vec<AbortOnDrop>) -> Store<K>
where
    K: Resource + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let (store, writer) = reflector::store();
    let stream = reflector::reflector(writer, watcher(Api::<K>::all(client.clone()), watcher::Config::default()).default_backoff());
    let errors = errors.clone();
    tasks.push(AbortOnDrop(tokio::spawn(async move {
        stream
            .for_each(|ev| {
                let mut e = errors.lock().unwrap();
                match ev {
                    Ok(_) => drop(e.remove(name)),
                    Err(err) => drop(e.insert(name, err.to_string())),
                }
                futures::future::ready(())
            })
            .await;
    })));
    store
}

impl Watches {
    pub fn start(client: &Client) -> Self {
        let errors = Errors::default();
        let mut tasks = vec![];
        Watches {
            nodes: watch(client, "nodes", &errors, &mut tasks),
            pods: watch(client, "pods", &errors, &mut tasks),
            events: watch(client, "events", &errors, &mut tasks),
            deployments: watch(client, "deployments", &errors, &mut tasks),
            replicasets: watch(client, "replicasets", &errors, &mut tasks),
            daemonsets: watch(client, "daemonsets", &errors, &mut tasks),
            statefulsets: watch(client, "statefulsets", &errors, &mut tasks),
            revisions: watch(client, "controllerrevisions", &errors, &mut tasks),
            errors,
            _tasks: tasks,
        }
    }

    /// Wait (bounded) until nodes and pods have their first full list.
    pub async fn ready(&self, limit: Duration) -> bool {
        tokio::time::timeout(limit, async {
            let _ = self.nodes.wait_until_ready().await;
            let _ = self.pods.wait_until_ready().await;
        })
        .await
        .is_ok()
    }

    pub fn errors(&self) -> Vec<(&'static str, String)> {
        self.errors.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect()
    }
}

/// Owned copies of a cache's objects (the snapshot code consumes them).
pub fn items<K>(store: &Store<K>) -> Vec<K>
where
    K: Resource + Clone + 'static,
    K::DynamicType: Eq + std::hash::Hash + Clone,
{
    store.state().iter().map(|a| (**a).clone()).collect()
}
