#![forbid(unsafe_code)]

mod app;
mod cloud;
#[cfg(test)]
mod dast;
mod demo;
mod demo_ops;
mod fx;
mod geo;
mod gitops;
mod infra;
mod kubeconfigs;
mod metrics;
mod model;
mod ops;
mod source;
mod storage;
mod ui;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::app::{Active, App};
use crate::model::Snapshot;
use crate::source::{Msg, Source};

/// Terminal command center for Kubernetes: geo map, galaxy and radar views of your cluster.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Kubeconfig file (defaults to $KUBECONFIG / ~/.kube/config).
    #[arg(long)]
    kubeconfig: Option<PathBuf>,
    /// Kubeconfig context (defaults to current-context, else the first one).
    #[arg(long)]
    context: Option<String>,
    /// Run against a simulated cluster.
    #[arg(long)]
    demo: bool,
    /// Poll interval in seconds.
    #[arg(long, default_value_t = 5)]
    interval: u64,
    /// Pin a node name / region / zone / location key: KEY=LAT,LON or KEY=PLACE, e.g. node-1=fsn1 (repeatable).
    #[arg(long = "place", value_parser = geo::parse_place)]
    places: Vec<(String, geo::Place)>,
    /// Coordinates for nodes without any location info, e.g. your homelab: LAT,LON.
    #[arg(long, value_parser = geo::parse_lat_lon, allow_hyphen_values = true)]
    home: Option<(f64, f64)>,
    /// Cloud providers to ask where VMs live: hetzner,aws,gcp,azure (comma separated).
    /// Default: auto — Hetzner when HCLOUD_TOKEN is available, Azure when nodes have azure:// providerIDs.
    #[arg(long, value_delimiter = ',')]
    cloud: Option<Vec<String>>,
    /// Dotenv file to read HCLOUD_TOKEN from (when it is not in the environment).
    #[arg(long)]
    hcloud_env_file: Option<PathBuf>,
    /// AWS regions to list instances in (repeatable / comma separated; default: CLI's configured region).
    #[arg(long = "aws-region", value_delimiter = ',')]
    aws_regions: Vec<String>,
    /// GCP project for `gcloud compute instances list` (default: gcloud's configured project).
    #[arg(long)]
    gcp_project: Option<String>,
    /// How often to refresh provider inventories, in seconds.
    #[arg(long, default_value_t = 300)]
    cloud_interval: u64,
    /// Disable rollout actions (restart, pause, undo, promote, abort...).
    #[arg(long)]
    read_only: bool,
    /// Send rollout actions as server-side dry-runs: validated by the API server, nothing changes.
    #[arg(long)]
    dry_run: bool,
    /// Frames per second.
    #[arg(long, default_value_t = 30)]
    fps: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Clusters from $KUBECONFIG, ~/.kube/config and every other kubeconfig in ~/.kube/.
    let clusters = kubeconfigs::discover();
    let (mut source, active, picker) = if args.demo {
        (Source::Demo(demo::Sim::new()), Active::Demo, false)
    } else if args.kubeconfig.is_some() || args.context.is_some() {
        let file = args.kubeconfig.as_ref().and_then(|f| f.canonicalize().ok());
        let active = clusters
            .iter()
            .position(|c| {
                file.as_ref().is_none_or(|f| c.file.canonicalize().ok().as_ref() == Some(f))
                    && args.context.as_ref().is_none_or(|ctx| *ctx == c.context)
                    && (args.context.is_some() || file.is_some())
            })
            .map_or(Active::Other, Active::Entry);
        // A cluster from the list keeps its list name; anything else is opened as given.
        let source = match active {
            Active::Entry(i) => Source::open(&source::Target::Kube(clusters[i].clone())).await?,
            _ => Source::connect(args.kubeconfig, args.context).await?,
        };
        (source, active, false)
    } else {
        match clusters.len() {
            0 => anyhow::bail!("no kubeconfig found in $KUBECONFIG or ~/.kube/ — pass --kubeconfig, or try --demo"),
            1 => (Source::open(&source::Target::Kube(clusters[0].clone())).await?, Active::Entry(0), false),
            // Several clusters: let the user pick (the default context is preselected).
            _ => (Source::Idle, Active::None, true),
        }
    };
    source.set_dry_run(args.dry_run);
    let providers = match args.cloud {
        None => None,
        Some(list) if list.iter().any(|p| p == "auto") => None,
        Some(list) if list.iter().any(|p| p == "none") => Some(vec![]),
        Some(list) => Some(list.iter().map(|p| p.parse()).collect::<Result<Vec<cloud::Provider>, _>>().map_err(anyhow::Error::msg)?),
    };
    let hcloud_token = match std::env::var("HCLOUD_TOKEN").ok().filter(|t| !t.is_empty()) {
        Some(t) => Some(t),
        None => match &args.hcloud_env_file {
            Some(path) => cloud::token_from_env_file(path)?,
            None => None,
        },
    };
    let cloud_poll = source::CloudPoll {
        config: cloud::CloudConfig { providers, hcloud_token, aws_regions: args.aws_regions, gcp_project: args.gcp_project },
        interval: Duration::from_secs(args.cloud_interval.max(30)),
    };
    let resolver = geo::Resolver { overrides: args.places.into_iter().collect(), home: args.home };

    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let cluster = source.cluster();
    tokio::spawn(source::run(source, resolver, Duration::from_secs(args.interval.max(1)), cloud_poll, cmd_rx, msg_tx));

    let mut app = App::new(Snapshot::empty(cluster), cmd_tx);
    app.set_clusters(clusters, active);
    if picker {
        app.open_picker();
    }
    app.read_only = args.read_only;
    app.dry_run = args.dry_run;
    let mut terminal = ratatui::init();
    let frame = Duration::from_millis(1000 / args.fps.clamp(1, 120));
    let result = (|| -> Result<()> {
        while !app.quit {
            while let Ok(msg) = msg_rx.try_recv() {
                match msg {
                    Msg::Snapshot(snap) => app.set_snapshot(*snap),
                    Msg::Logs { title, lines } => app.set_logs(title, lines),
                    Msg::ActionDone(res) => app.action_done(res),
                    Msg::Switched { cluster } => app.switched(cluster),
                    Msg::SwitchFailed(e) => app.switch_failed(e),
                    Msg::Probed { index, result } => app.set_probe(index, result),
                    Msg::Measured(res) => app.measured(res),
                }
            }
            app.tick();
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(frame)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                app.on_key(key);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}
