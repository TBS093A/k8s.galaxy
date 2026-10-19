# Changelog

All notable changes to k8s-galaxy. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/). A release moves the entries below
under a dated heading, `## X.Y.Z - YYYY-MM-DD` (checked by the release workflow).

## Unreleased

### Added

- Geo map, galaxy and radar views of nodes and pods; node locations from labels, zones and
  cloud provider APIs (Hetzner, AWS, GCP, Azure).
- Resource list by namespace: pods, Deployments, ReplicaSets, DaemonSets, StatefulSets, Helm releases.
- Rollouts view (native and Argo Rollouts) with restart, pause/resume, rollback to any revision
  (StatefulSets and DaemonSets through ControllerRevisions), promote, abort, retry — with
  warnings before acting (Argo CD self-heal, rollbackWindow, skipped revisions).
- Traffic view: Ingress, Gateway API routes and gateways, cert-manager certificates and issuers.
- RBAC view: subjects and roles rated by risk, who-can queries.
- Argo CD view: sync, health, drift, last operation; sync and refresh actions.
- Storage view: PV/PVC with in-cluster vs external location, provisioned vs used, `du`
  measurement of hostPath/local volumes, a storage map of node disks and external storage.
- CPU/memory from metrics-server, requests vs allocatable without it.
- Cluster picker for every kubeconfig in `~/.kube` and `$KUBECONFIG`, with infrastructure
  detection (managed EKS/GKE/AKS/…, cloud VMs, bare metal, local, hybrid) and the distribution.
- Cleanup view: leftover volumes, pods, jobs, ACME solvers, broken routes, failing certificates,
  unused config, selector-less services and empty namespaces, with deletes behind a confirmation.
- Watch API for nodes, pods, events and workloads: snapshots read local caches (default refresh 2 s).
- `--dry-run` (server-side validation of every action) and `--read-only`.
- CI/CD: CI, Security (secrets, SAST, SCA, DAST), post-merge builds and tag releases.
