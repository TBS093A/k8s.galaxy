//! Dynamic tests: render every view at many terminal sizes (popups included) and feed hostile
//! input to every parser, checking that nothing panics. The default run is quick; `DAST=1`
//! (with `PROPTEST_CASES`, as the security workflow does) runs the deep profile.

use proptest::prelude::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::{App, Confirm, LogView, Pending, View};
use crate::model::Snapshot;

fn deep() -> bool {
    std::env::var_os("DAST").is_some()
}

fn demo_app() -> App {
    let mut snap: Snapshot = crate::demo::Sim::new().snapshot();
    let resolver = crate::geo::Resolver { overrides: Default::default(), home: None };
    snap.nodes.iter_mut().for_each(|n| resolver.apply(n, &[]));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    // The receiver is dropped: commands sent by key handling just go nowhere.
    let mut app = App::new(Snapshot::empty("test".into()), tx);
    app.set_snapshot(snap);
    app
}

fn sizes() -> Vec<(u16, u16)> {
    let (ws, hs): (Vec<u16>, Vec<u16>) = if deep() {
        ((1..=260).step_by(3).collect(), (1..=90).step_by(2).collect())
    } else {
        (vec![1, 2, 7, 20, 46, 80, 120, 200], vec![1, 3, 9, 24, 52])
    };
    ws.iter().flat_map(|w| hs.iter().map(move |h| (*w, *h))).collect()
}

fn render_all(app: &mut App) {
    for (w, h) in sizes() {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
    }
}

#[test]
fn every_view_renders_at_every_size() {
    let mut app = demo_app();
    for view in View::ALL {
        app.view = view;
        render_all(&mut app);
    }
}

#[test]
fn popups_render_at_every_size() {
    let mut app = demo_app();
    app.logs = Some(LogView {
        title: "x".repeat(300),
        lines: vec!["é".repeat(500), String::new(), "\t\u{1b}[31mred".into()],
        scroll: 99,
        loading: false,
        is_log: true,
    });
    render_all(&mut app);
    app.logs = None;
    app.confirm = Some(Confirm {
        pending: Pending::Measure(vec![]),
        explain: "y".repeat(400),
        warnings: vec![(crate::model::Health::Bad, "z".repeat(700)), (crate::model::Health::Ok, String::new())],
    });
    render_all(&mut app);
    app.confirm = None;
    app.open_picker();
    render_all(&mut app);
}

#[test]
fn an_empty_cluster_renders() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Snapshot::empty(String::new()), tx);
    for view in View::ALL {
        app.view = view;
        render_all(&mut app);
    }
}

proptest! {
    #[test]
    fn quantities_never_panic(s in ".{0,40}") {
        let _ = crate::metrics::cpu_milli(&s);
        let _ = crate::metrics::bytes(&s);
    }

    #[test]
    fn helm_payloads_never_panic(raw in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = crate::source::decode_release(&raw);
    }

    #[test]
    fn du_output_never_panics(s in "(?s).{0,300}") {
        let _ = crate::storage::parse_measure(&s);
    }

    #[test]
    fn dates_never_panic(s in ".{0,40}") {
        let _ = crate::ops::days_until(&s);
    }

    #[test]
    fn classification_never_panics(server in ".{0,60}", version in ".{0,30}", label in ".{0,30}", provider in ".{0,40}") {
        let mut n = crate::model::NodeInfo::test("n");
        n.labels.insert(label, String::new());
        n.provider_id = provider;
        let i = crate::infra::classify(&server, &version, &[n.clone()], &[]);
        let _ = i.badge();
        let _ = crate::infra::node_kind(&n);
    }

    #[test]
    fn kubeconfig_names_never_panic(s in ".{0,80}") {
        let _ = crate::kubeconfigs::short_server(&s);
        let _ = crate::kubeconfigs::stem(std::path::Path::new(&s));
    }
}
