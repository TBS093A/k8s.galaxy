//! Operations views: rollouts, traffic (routes, gateways, certificates, issuers) and RBAC.
//! List of entries in namespace sections on the left, details of the selected one on the right.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::health_rgb;
use crate::app::{App, NetTab, OpsRef, RbacTab, View};
use crate::fx::{self, Rgb};
use crate::model::Health;
use crate::ops::{BindingInfo, NetInfo, Phase, RbacInfo, RolloutInfo, RolloutKind, Rule, Subject};

const TEXT: Rgb = (200, 220, 230);

fn st(text: impl Into<String>, rgb: Rgb) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(fx::c(rgb)))
}

fn bold(text: impl Into<String>, rgb: Rgb) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(fx::c(rgb)).add_modifier(Modifier::BOLD))
}

fn pad(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count > n {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    } else {
        format!("{s}{}", " ".repeat(n - count))
    }
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![st(format!("{k:<10}"), fx::STEEL), st(v, TEXT)])
}

fn section(title: &str) -> Line<'static> {
    Line::from(vec![st("── ", fx::STEEL), bold(title.to_string(), fx::CYAN), st(" ──", fx::STEEL)])
}

fn accent(view: View) -> Rgb {
    match view {
        View::Rollouts => fx::MAGENTA,
        View::Network => fx::CYAN,
        View::Argo => fx::GREEN,
        _ => fx::AMBER,
    }
}

/// `▰▰▰▱▱` bar of `done` out of `total`, `width` cells.
fn bar(done: i32, total: i32, width: usize, rgb: Rgb, t: f64, moving: bool) -> Vec<Span<'static>> {
    let total = total.max(1);
    let filled = ((done.clamp(0, total) as f64 / total as f64) * width as f64).round() as usize;
    let mut spans = vec![];
    for i in 0..width {
        if i < filled {
            // A light sweep along the filled part while it is still moving.
            let k = if moving { 0.65 + 0.35 * (((t * 1.6 - i as f64 * 0.12).sin() + 1.0) / 2.0) } else { 1.0 };
            spans.push(Span::styled("▰", Style::new().fg(fx::fade(rgb, k))));
        } else {
            spans.push(Span::styled("▱", Style::new().fg(fx::fade(fx::STEEL, 0.6))));
        }
    }
    spans
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let [list_area, detail_area] = Layout::horizontal([Constraint::Percentage(54), Constraint::Fill(1)]).areas(area);
    list(f, list_area, app);
    detail(f, detail_area, app);
    if app.confirm.is_some() {
        confirm(f, area, app);
    }
}

fn list(f: &mut Frame, area: Rect, app: &App) {
    let rgb = accent(app.view);
    let t = app.t();
    let items = app.ops_items();
    let selected = app.ops_selected_idx();
    let mut rows: Vec<ListItem> = vec![];
    let mut selected_row = None;
    let mut i = 0;
    while i < items.len() {
        let group = items[i].group();
        let n = items[i..].iter().take_while(|o| o.group() == group).count();
        let bad = items[i..i + n].iter().filter(|o| app.ops_health(o) == Health::Bad).count();
        let warn = items[i..i + n].iter().filter(|o| app.ops_health(o) == Health::Warn).count();
        let mut header = vec![bold(format!("▾ {group} "), fx::CYAN), st(n.to_string(), fx::STEEL)];
        if bad > 0 {
            header.push(st(format!(" ×{bad}"), fx::RED));
        }
        if warn > 0 {
            header.push(st(format!(" ▲{warn}"), fx::AMBER));
        }
        rows.push(ListItem::new(Line::from(header)));
        for (k, o) in items[i..i + n].iter().enumerate() {
            if i + k == selected {
                selected_row = Some(rows.len());
            }
            rows.push(ListItem::new(row(app, o, t)));
        }
        i += n;
    }

    let mut title = vec![Span::styled(" ◆ ", Style::new().fg(fx::c(rgb))), bold(app.view.title(), rgb), Span::raw("  ")];
    let tabs: Vec<(&str, bool)> = match app.view {
        View::Network => NetTab::ALL.iter().map(|t| (t.short(), *t == app.net_tab)).collect(),
        View::Access => RbacTab::ALL.iter().map(|t| (t.short(), *t == app.rbac_tab)).collect(),
        _ => vec![],
    };
    for (name, on) in tabs {
        title.push(if on {
            Span::styled(format!(" {name} "), Style::new().fg(fx::c(fx::BG)).bg(fx::c(rgb)).add_modifier(Modifier::BOLD))
        } else {
            Span::styled(format!(" {name} "), Style::new().fg(fx::fade(rgb, 0.6)))
        });
    }
    title.extend(summary(app));
    title.push(Span::raw(" "));
    let hints = match app.view {
        View::Rollouts => {
            let mode = if app.read_only { "  READ-ONLY" } else if app.dry_run { "  DRY-RUN" } else { "" };
            format!(" j/k select · R restart · p pause/resume · u undo · P promote · F full · A abort · Y retry{mode} ")
        }
        View::Network => " j/k select · t tab · ⏎ full details ".into(),
        View::Argo => {
            let mode = if app.read_only { "  READ-ONLY" } else if app.dry_run { "  DRY-RUN" } else { "" };
            format!(" j/k select · s sync · S sync+prune · R refresh · H hard refresh · ⏎ details{mode} ")
        }
        _ => format!(" j/k · t tab · / who-can · s system:* {} · ⏎ details ", if app.show_system { "on" } else { "off" }),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(fx::c(rgb)))
        .title(Line::from(title))
        .title_bottom(Line::from(st(hints, fx::STEEL)));

    if rows.is_empty() {
        f.render_widget(Paragraph::new(empty_hint(app)).wrap(Wrap { trim: false }).block(block), area);
    } else {
        let mut state = ListState::default().with_selected(selected_row);
        let list = List::new(rows)
            .block(block)
            .highlight_style(Style::new().bg(fx::fade(rgb, 0.16)).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶");
        f.render_stateful_widget(list, area, &mut state);
    }

    if let Some(q) = &app.query {
        let line_area = Rect { x: area.x + 1, y: area.bottom().saturating_sub(2), width: area.width.saturating_sub(2), height: 1 };
        f.render_widget(Clear, line_area);
        let cursor = if (t * 2.0).fract() < 0.5 { "█" } else { " " };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" who can › ", Style::new().fg(fx::c(fx::BG)).bg(fx::c(fx::AMBER))),
                st(format!(" {q}{cursor}"), TEXT),
                st("   e.g. get secrets · create pods/exec shop · patch deployments.apps", fx::STEEL),
            ]))
            .style(Style::new().bg(fx::c(fx::BG))),
            line_area,
        );
    }
}

fn empty_hint(app: &App) -> Vec<Line<'static>> {
    let caps = &app.snap.caps;
    let msg = match app.view {
        View::Rollouts => "no workloads yet".to_string(),
        View::Argo if !caps.argocd => "Argo CD (applications.argoproj.io) is not installed in this cluster.".into(),
        View::Argo => "no Argo CD Applications".into(),
        View::Network => match app.net_tab {
            NetTab::Gateways if !caps.gateway_api => "Gateway API CRDs (gateway.networking.k8s.io) are not installed in this cluster.".into(),
            NetTab::Certs | NetTab::Issuers if !caps.cert_manager => "cert-manager CRDs (cert-manager.io) are not installed in this cluster.".into(),
            _ => "nothing here".into(),
        },
        _ => "no RBAC data (needs list on roles/bindings) — press s to include system:* objects".into(),
    };
    vec![Line::from(st(msg, fx::STEEL))]
}

fn summary(app: &App) -> Vec<Span<'static>> {
    match app.view {
        View::Rollouts => {
            let r = &app.snap.rollouts;
            let count = |p: Phase| r.iter().filter(|x| x.phase == p).count();
            let mut v = vec![st(format!("  {} ", r.len()), fx::STEEL)];
            for (p, rgb) in [(Phase::Progressing, fx::CYAN), (Phase::Paused, fx::AMBER), (Phase::Degraded, fx::RED), (Phase::Aborted, fx::RED)] {
                let n = count(p);
                if n > 0 {
                    v.push(st(format!(" {n} {}", p.as_str().to_lowercase()), rgb));
                }
            }
            if !app.snap.caps.argo_rollouts {
                v.push(st("  (no Argo Rollouts CRD)", fx::STEEL));
            }
            v
        }
        View::Argo => {
            let a = &app.snap.apps;
            let out = a.iter().filter(|x| x.sync != "Synced").count();
            let bad = a.iter().filter(|x| x.health() == Health::Bad).count();
            let running = a.iter().filter(|x| x.running()).count();
            let mut v = vec![st(format!("  {} apps", a.len()), fx::STEEL)];
            if out > 0 {
                v.push(st(format!("  {out} out of sync"), fx::AMBER));
            }
            if running > 0 {
                v.push(st(format!("  {running} syncing"), fx::CYAN));
            }
            if bad > 0 {
                v.push(st(format!("  {bad} failing"), fx::RED));
            }
            v
        }
        _ => vec![],
    }
}

fn sync_rgb(sync: &str) -> Rgb {
    match sync {
        "Synced" => fx::GREEN,
        "OutOfSync" => fx::AMBER,
        _ => fx::STEEL,
    }
}

fn app_health_rgb(h: &str) -> Rgb {
    match h {
        "Healthy" => fx::GREEN,
        "Progressing" => fx::CYAN,
        "Suspended" | "Unknown" | "" => fx::STEEL,
        _ => fx::RED,
    }
}

fn op_rgb(phase: &str) -> Rgb {
    match phase {
        "Succeeded" => fx::GREEN,
        "Running" | "Terminating" => fx::CYAN,
        _ => fx::RED,
    }
}

fn row(app: &App, o: &OpsRef, t: f64) -> Line<'static> {
    let h = app.ops_health(o);
    let dot = st("  • ", health_rgb(h));
    let net = &app.snap.net;
    match o {
        OpsRef::Rollout(r) => {
            let mut s = vec![dot, st(format!("{:<7}", r.kind.short()), fx::STEEL), st(pad(&r.name, 28), TEXT), Span::raw(" ")];
            s.extend(bar(r.updated, r.desired, 10, phase_rgb(r.phase), t, r.phase == Phase::Progressing));
            s.push(st(format!(" {:>2}/{:<2} ", r.updated, r.desired), TEXT));
            s.push(st(format!("{:<11}", r.phase.as_str()), phase_rgb(r.phase)));
            if let (Some(step), false) = (r.step, r.steps.is_empty()) {
                s.push(st(format!(" step {}/{}", step + 1, r.steps.len()), fx::STEEL));
            }
            if let Some(w) = r.weight {
                s.push(st(format!(" w{w}%"), fx::MAGENTA));
            }
            if r.revision > 0 && r.kind != RolloutKind::DaemonSet && r.kind != RolloutKind::StatefulSet {
                s.push(st(format!(" r{}", r.revision), fx::STEEL));
            }
            Line::from(s)
        }
        OpsRef::Route(r) => {
            let mut s = vec![dot, st(format!("{:<5}", r.kind.short()), fx::STEEL), st(pad(&r.name, 24), TEXT), Span::raw(" ")];
            s.push(st(pad(&r.hosts.join(","), 26), fx::CYAN));
            let (ready, total) = r.backends.iter().fold((0, 0), |(a, b), x| match x.endpoints {
                Some((r, t)) => (a + r, b + t),
                None => (a, b),
            });
            let missing = r.backends.iter().any(|b| b.endpoints.is_none());
            s.push(st(format!(" → {}", r.backends.len()), fx::STEEL));
            s.push(st(if missing { " svc missing".into() } else { format!(" ep {ready}/{total}") }, if missing || ready == 0 { fx::RED } else if ready < total { fx::AMBER } else { fx::GREEN }));
            for (secret, _) in r.tls.iter().take(1) {
                match net.cert_for(&r.namespace, secret) {
                    Some(c) if c.ready => s.push(st(format!("  tls {}", c.days_left.map(|d| format!("{d}d")).unwrap_or_else(|| "ok".into())), health_rgb(c.health()))),
                    Some(_) => s.push(st("  tls NOT READY", fx::RED)),
                    None if net.tls_secrets.contains(&(r.namespace.clone(), secret.clone())) => s.push(st("  tls (unmanaged)", fx::STEEL)),
                    None => s.push(st("  tls secret missing", fx::RED)),
                }
            }
            if !r.problems.is_empty() {
                s.push(st(format!("  {} problem(s)", r.problems.len()), fx::RED));
            }
            Line::from(s)
        }
        OpsRef::Gateway(g) => Line::from(vec![
            dot,
            st(pad(&g.name, 22), TEXT),
            st(pad(&g.class, 12), fx::STEEL),
            st(pad(&g.addresses.join(","), 18), fx::CYAN),
            st(format!(" {} listeners  {} routes ", g.listeners.len(), g.listeners.iter().map(|l| l.attached).sum::<i64>()), fx::STEEL),
            st(if g.programmed { "Programmed" } else { "NOT programmed" }, if g.programmed { fx::GREEN } else { fx::RED }),
        ]),
        OpsRef::Cert(c) => {
            let mut s = vec![dot, st(pad(&c.name, 30), TEXT), st(pad(&c.dns_names.join(","), 30), fx::CYAN), Span::raw(" ")];
            match (c.ready, c.days_left) {
                (true, Some(d)) => {
                    s.extend(bar(d.clamp(0, 90) as i32, 90, 8, health_rgb(c.health()), t, false));
                    s.push(st(format!(" {d:>3}d"), health_rgb(c.health())));
                }
                (true, None) => s.push(st("Ready", fx::GREEN)),
                (false, _) => s.push(st(format!("NOT READY {}", c.reason), fx::RED)),
            }
            Line::from(s)
        }
        OpsRef::Issuer(i) => Line::from(vec![
            dot,
            st(format!("{:<14}", if i.cluster { "ClusterIssuer" } else { "Issuer" }), fx::STEEL),
            st(pad(&i.name, 24), TEXT),
            st(pad(&i.kind, 44), fx::STEEL),
            st(if i.ready { " Ready" } else { " NOT READY" }, if i.ready { fx::GREEN } else { fx::RED }),
        ]),
        OpsRef::Subject(s, bindings) => {
            let rbac = &app.snap.rbac;
            let mut v = vec![dot, st(pad(&s.name, 36), TEXT), st(format!("{:>2} binding{} ", bindings.len(), if bindings.len() == 1 { " " } else { "s" }), fx::STEEL)];
            let cluster_admin = bindings.iter().any(|b| b.cluster && b.role == "cluster-admin");
            if cluster_admin {
                v.push(st(" cluster-admin", fx::RED));
            } else if let Some((h, why)) = rbac.subject_risk(bindings) {
                v.push(st(format!(" {why}"), health_rgb(h)));
            }
            if rbac.missing_sa(s) {
                v.push(st(" SA does not exist", fx::AMBER));
            }
            Line::from(v)
        }
        OpsRef::App(a) => {
            let spinner = ["◐", "◓", "◑", "◒"][((t * 6.0) as usize) % 4];
            let mut s = vec![dot, st(pad(&a.name, 26), TEXT), Span::raw(" ")];
            s.push(st(format!("{:<10}", a.sync), sync_rgb(&a.sync)));
            s.push(st(format!("{:<12}", a.health), app_health_rgb(&a.health)));
            match a.op_phase.as_deref() {
                Some("Running") => s.push(st(format!("{spinner} syncing  "), fx::CYAN)),
                Some(p @ ("Failed" | "Error")) => s.push(st(format!("sync {}  ", p.to_lowercase()), fx::RED)),
                _ => s.push(Span::raw("           ")),
            }
            let rev = match a.revision.split_once(',') {
                Some((first, _)) => format!("{first}+"),
                None => a.revision.clone(),
            };
            s.push(st(format!("{} ", pad(&rev, 9)), fx::STEEL));
            s.push(st(if a.auto { "auto  " } else { "manual" }, fx::STEEL));
            let drift = a.drift().len();
            if drift > 0 {
                s.push(st(format!("  drift {drift}"), fx::AMBER));
            }
            Line::from(s)
        }
        OpsRef::Role(r) => {
            let bound = app.snap.rbac.bindings.iter().filter(|b| app.snap.rbac.role_of(b).is_some_and(|x| std::ptr::eq(x, *r))).count();
            let mut v = vec![
                dot,
                st(format!("{:<12}", if r.cluster { "ClusterRole" } else { "Role" }), fx::STEEL),
                st(pad(&r.name, 40), TEXT),
                st(format!("{:>3} rules  bound {:>2}× ", r.rules.len(), bound), fx::STEEL),
            ];
            if let Some((h, why)) = r.risk() {
                v.push(st(why, health_rgb(h)));
            }
            if bound == 0 {
                v.push(st("  unused", fx::STEEL));
            }
            Line::from(v)
        }
    }
}

fn phase_rgb(p: Phase) -> Rgb {
    match p {
        Phase::Complete => fx::GREEN,
        Phase::Progressing => fx::CYAN,
        Phase::Paused => fx::AMBER,
        Phase::Degraded | Phase::Aborted => fx::RED,
    }
}

fn detail(f: &mut Frame, area: Rect, app: &App) {
    let rgb = accent(app.view);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(fx::fade(rgb, 0.55)))
        .title(Line::from(vec![Span::styled(" ◆ ", Style::new().fg(fx::c(rgb))), bold("DETAILS", rgb), Span::raw(" ")]));
    let lines = match app.selected_ops() {
        None => vec![Line::from(st("select an entry", fx::STEEL))],
        Some(o) => detail_lines(app, &o, area.width.saturating_sub(4) as usize),
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

/// Detail of an entry; also used (as plain text) for the full-screen popup.
pub fn detail_lines(app: &App, o: &OpsRef, width: usize) -> Vec<Line<'static>> {
    let t = app.t();
    match o {
        OpsRef::Rollout(r) => rollout_detail(app, r, t, width),
        OpsRef::Route(r) => {
            let net = &app.snap.net;
            let kind = match r.kind {
                crate::ops::RouteKind::Ingress => "Ingress",
                crate::ops::RouteKind::HttpRoute => "HTTPRoute",
                crate::ops::RouteKind::GrpcRoute => "GRPCRoute",
            };
            let mut l = vec![
                Line::from(vec![bold(format!("{kind} "), fx::CYAN), bold(format!("{}/{}", r.namespace, r.name), TEXT)]),
                kv(if r.kind == crate::ops::RouteKind::Ingress { "class" } else { "parents" }, r.class.clone()),
                kv("address", if r.address.is_empty() { "— (not admitted yet)".into() } else { r.address.clone() }),
                Line::raw(""),
                section("traffic"),
            ];
            let mut hosts: Vec<&str> = r.backends.iter().map(|b| b.host.as_str()).collect();
            hosts.dedup();
            for host in hosts {
                l.push(Line::from(vec![st("  ", TEXT), bold(host.to_string(), fx::CYAN)]));
                for b in r.backends.iter().filter(|b| b.host == host) {
                    let (ep, rgb) = match b.endpoints {
                        None => ("service not found".to_string(), fx::RED),
                        Some((0, total)) => (format!("0/{total} endpoints ready"), fx::RED),
                        Some((ready, total)) if ready < total => (format!("{ready}/{total} endpoints ready"), fx::AMBER),
                        Some((ready, total)) => (format!("{ready}/{total} endpoints"), fx::GREEN),
                    };
                    let dots: String = match b.endpoints {
                        Some((ready, total)) => "●".repeat(ready.min(12)) + &"○".repeat(total.saturating_sub(ready).min(12)),
                        None => String::new(),
                    };
                    l.push(Line::from(vec![
                        st(format!("    {} ", pad(&b.path, 18)), TEXT),
                        st("→ ", fx::STEEL),
                        st(format!("svc {}:{}  ", b.service, b.port), TEXT),
                        st(format!("{dots} "), rgb),
                        st(ep, rgb),
                    ]));
                }
            }
            if !r.tls.is_empty() {
                l.push(Line::raw(""));
                l.push(section("TLS"));
                for (secret, hosts) in &r.tls {
                    l.push(Line::from(vec![st("  secret ", fx::STEEL), bold(secret.clone(), TEXT), st(format!("  {}", hosts.join(",")), fx::STEEL)]));
                    l.extend(cert_chain(net, &r.namespace, secret));
                }
            }
            if !r.problems.is_empty() {
                l.push(Line::raw(""));
                l.push(section("problems"));
                l.extend(r.problems.iter().map(|p| Line::from(st(format!("  {p}"), fx::RED))));
            }
            l
        }
        OpsRef::Gateway(g) => {
            let net = &app.snap.net;
            let mut l = vec![
                Line::from(vec![bold("Gateway ", fx::CYAN), bold(format!("{}/{}", g.namespace, g.name), TEXT)]),
                kv("class", g.class.clone()),
                kv("addresses", g.addresses.join(", ")),
                Line::from(vec![st(format!("{:<10}", "status"), fx::STEEL), st(if g.programmed { "Programmed" } else { "NOT programmed" }, if g.programmed { fx::GREEN } else { fx::RED }), st(format!("  {}", g.message), fx::STEEL)]),
                Line::raw(""),
                section("listeners"),
            ];
            for li in &g.listeners {
                let mut s = vec![
                    st(format!("  {:<10}", li.name), TEXT),
                    st(format!("{:<6}{:>5}  ", li.protocol, li.port), fx::STEEL),
                    st(pad(if li.hostname.is_empty() { "*" } else { &li.hostname }, 22), fx::CYAN),
                    st(format!(" {} routes", li.attached), fx::STEEL),
                ];
                if let Some(secret) = &li.tls_secret {
                    s.push(st(format!("  tls {secret}"), TEXT));
                }
                l.push(Line::from(s));
                if let Some(secret) = &li.tls_secret {
                    l.extend(cert_chain(net, &g.namespace, secret));
                }
            }
            l.push(Line::raw(""));
            l.push(section("attached routes"));
            let key = format!("{}/{}", g.namespace, g.name);
            for r in net.routes.iter().filter(|r| r.class.split(',').any(|p| p == key || p.starts_with(&format!("{key}:")))) {
                l.push(Line::from(vec![st("  • ", health_rgb(net.route_health(r))), st(format!("{}/{}  ", r.namespace, r.name), TEXT), st(r.hosts.join(","), fx::CYAN)]));
            }
            l
        }
        OpsRef::Cert(c) => {
            let net = &app.snap.net;
            let mut l = vec![
                Line::from(vec![bold("Certificate ", fx::CYAN), bold(format!("{}/{}", c.namespace, c.name), TEXT)]),
                kv("secret", c.secret.clone()),
                kv("dns", c.dns_names.join(", ")),
                kv("issuer", c.issuer.clone()),
            ];
            l.extend(issuer_line(net, &c.namespace, &c.issuer));
            l.push(Line::from(vec![
                st(format!("{:<10}", "status"), fx::STEEL),
                st(if c.ready { "Ready".to_string() } else { format!("NOT READY ({})", c.reason) }, if c.ready { fx::GREEN } else { fx::RED }),
            ]));
            if !c.message.is_empty() {
                l.push(Line::from(st(format!("          {}", c.message), fx::STEEL)));
            }
            if let Some(na) = &c.not_after {
                let mut s = vec![st(format!("{:<10}", "expires"), fx::STEEL), st(format!("{na}  "), TEXT)];
                if let Some(d) = c.days_left {
                    s.extend(bar(d.clamp(0, 90) as i32, 90, 18, health_rgb(c.health()), t, false));
                    s.push(st(format!(" {d} days left"), health_rgb(c.health())));
                }
                l.push(Line::from(s));
            }
            if let Some(r) = &c.renewal {
                l.push(kv("renews", r.clone()));
            }
            if !c.challenges.is_empty() {
                l.push(Line::raw(""));
                l.push(section("ACME challenges"));
                l.extend(c.challenges.iter().map(|ch| Line::from(st(format!("  {ch}"), fx::AMBER))));
            }
            l.push(Line::raw(""));
            l.push(section("used by"));
            let users: Vec<_> = net
                .routes
                .iter()
                .filter(|r| r.tls.iter().any(|(s, _)| *s == c.secret) && (r.namespace == c.namespace || net.cert_for(&r.namespace, &c.secret).is_some()))
                .collect();
            if users.is_empty() {
                l.push(Line::from(st("  no Ingress / route references this secret", fx::STEEL)));
            }
            for r in users {
                l.push(Line::from(vec![st("  • ", health_rgb(net.route_health(r))), st(format!("{} {}/{}  ", r.kind.short(), r.namespace, r.name), TEXT), st(r.hosts.join(","), fx::CYAN)]));
            }
            for g in net.gateways.iter().filter(|g| g.namespace == c.namespace && g.listeners.iter().any(|l| l.tls_secret.as_deref() == Some(&c.secret))) {
                l.push(Line::from(vec![st("  • ", fx::GREEN), st(format!("gateway {}/{}", g.namespace, g.name), TEXT)]));
            }
            l
        }
        OpsRef::Issuer(i) => {
            let net = &app.snap.net;
            let key = format!("{}/{}", if i.cluster { "ClusterIssuer" } else { "Issuer" }, i.name);
            let mut l = vec![
                Line::from(vec![bold(format!("{} ", if i.cluster { "ClusterIssuer" } else { "Issuer" }), fx::CYAN), bold(i.name.clone(), TEXT)]),
                kv("type", i.kind.clone()),
                Line::from(vec![st(format!("{:<10}", "status"), fx::STEEL), st(if i.ready { "Ready" } else { "NOT READY" }, if i.ready { fx::GREEN } else { fx::RED })]),
                Line::from(st(format!("          {}", i.message), fx::STEEL)),
                Line::raw(""),
                section("certificates"),
            ];
            for c in net.certs.iter().filter(|c| c.issuer == key && (i.cluster || c.namespace == i.namespace) && !c.name.ends_with(')')) {
                let days = c.days_left.map(|d| format!("{d}d")).unwrap_or_else(|| if c.ready { "ready".into() } else { "NOT READY".into() });
                l.push(Line::from(vec![st("  • ", health_rgb(c.health())), st(pad(&format!("{}/{}", c.namespace, c.name), 40), TEXT), st(days, health_rgb(c.health()))]));
            }
            l
        }
        OpsRef::Subject(s, bindings) => subject_detail(&app.snap.rbac, s, bindings),
        OpsRef::App(a) => app_detail(app, a),
        OpsRef::Role(r) => {
            let rbac = &app.snap.rbac;
            let mut l = vec![
                Line::from(vec![bold(format!("{} ", if r.cluster { "ClusterRole" } else { "Role" }), fx::AMBER), bold(if r.cluster { r.name.clone() } else { format!("{}/{}", r.namespace, r.name) }, TEXT)]),
            ];
            if r.aggregated {
                l.push(Line::from(st("aggregated ClusterRole (rules filled in from labelled roles)", fx::STEEL)));
            }
            l.push(Line::raw(""));
            l.push(section("rules"));
            l.extend(r.rules.iter().map(rule_line));
            l.push(Line::raw(""));
            l.push(section("bound to"));
            for b in rbac.bindings.iter().filter(|b| rbac.role_of(b).is_some_and(|x| std::ptr::eq(x, *r))) {
                let subjects: Vec<String> = b.subjects.iter().map(Subject::label).collect();
                l.push(Line::from(vec![st(format!("  {} ", b.name), TEXT), st(format!("({}) ", b.scope()), fx::STEEL), st(subjects.join(", "), fx::CYAN)]));
            }
            l
        }
    }
}

fn rollout_detail(app: &App, r: &RolloutInfo, t: f64, width: usize) -> Vec<Line<'static>> {
    let mut l = vec![
        Line::from(vec![bold(format!("{} ", r.kind.as_str()), fx::MAGENTA), bold(format!("{}/{}", r.namespace, r.name), TEXT)]),
        kv("strategy", r.strategy.clone()),
        kv("image", r.image.clone()),
        Line::from(vec![
            st(format!("{:<10}", "phase"), fx::STEEL),
            bold(r.phase.as_str(), phase_rgb(r.phase)),
            st(if r.revision > 0 { format!("  revision {}", r.revision) } else { String::new() }, fx::STEEL),
        ]),
    ];
    if !r.message.is_empty() {
        l.push(Line::from(st(format!("          {}", r.message), fx::STEEL)));
    }
    l.push(Line::raw(""));
    l.push(section("pods"));
    // One cell per desired replica: new revision (updated), ready old ones, missing.
    let mut cells = vec![st("  ", TEXT)];
    for i in 0..r.desired.min(60) {
        let (glyph, rgb) = if i < r.updated {
            ("■ ", if r.phase == Phase::Progressing && i == r.updated - 1 && fx::pulse(t, 0.8) < 0.5 { fx::mix(fx::CYAN, fx::BG, 0.6) } else { fx::CYAN })
        } else if i < r.ready.max(r.updated) {
            ("■ ", fx::STEEL)
        } else {
            ("□ ", fx::RED)
        };
        cells.push(st(glyph, rgb));
    }
    l.push(Line::from(cells));
    l.push(Line::from(vec![
        st("  ■", fx::CYAN),
        st(format!(" updated {}  ", r.updated), fx::STEEL),
        st("■", fx::STEEL),
        st(" old  ", fx::STEEL),
        st("□", fx::RED),
        st(" missing   ", fx::STEEL),
        st(format!("desired {}  ready {}  available {}", r.desired, r.ready, r.available), TEXT),
    ]));
    if let Some(w) = r.weight {
        l.push(Line::raw(""));
        l.push(section("traffic"));
        let cells = width.saturating_sub(22).clamp(10, 50);
        let canary = (w as usize * cells).div_ceil(100);
        let mut s = vec![st("  stable ", fx::GREEN)];
        s.push(st("█".repeat(cells - canary.min(cells)), fx::GREEN));
        s.push(st("█".repeat(canary.min(cells)), fx::MAGENTA));
        s.push(st(format!(" canary {w}%"), fx::MAGENTA));
        l.push(Line::from(s));
    }
    if !r.steps.is_empty() {
        l.push(Line::raw(""));
        l.push(section("steps"));
        for (i, step) in r.steps.iter().enumerate() {
            let (mark, rgb) = match r.step {
                Some(cur) if i < cur => ("✓", fx::GREEN),
                Some(cur) if i == cur && r.phase == Phase::Aborted => ("×", fx::RED),
                Some(cur) if i == cur && r.phase == Phase::Complete => ("✓", fx::GREEN),
                Some(cur) if i == cur => ("▶", if r.paused { fx::AMBER } else { fx::CYAN }),
                _ => (" ", fx::STEEL),
            };
            l.push(Line::from(vec![st(format!("  {mark} {:>2}  ", i + 1), rgb), st(step.clone(), if mark == " " { fx::STEEL } else { TEXT })]));
        }
    }
    if !r.history.is_empty() {
        l.push(Line::raw(""));
        l.push(section("revisions"));
        for h in &r.history {
            let current = h.revision == r.revision;
            let role = h.role.map(|x| format!("{x:<8}")).unwrap_or_else(|| " ".repeat(8));
            l.push(Line::from(vec![
                st(format!("  {}r{:<4}", if current { "▶" } else { " " }, h.revision), if current { fx::MAGENTA } else { fx::STEEL }),
                st(format!("{:<12}", pad(&h.name, 11)), fx::STEEL),
                st(role, match h.role {
                    Some("canary") | Some("preview") => fx::MAGENTA,
                    Some(_) => fx::GREEN,
                    None => fx::STEEL,
                }),
                st(format!("{:>2}/{:<2} ", h.ready, h.replicas), if h.replicas > 0 { TEXT } else { fx::STEEL }),
                st(format!("{:<6}", h.age), fx::STEEL),
                st(h.image.rsplit('/').next().unwrap_or(&h.image).to_string(), if h.replicas > 0 { TEXT } else { fx::STEEL }),
            ]));
        }
    }
    l.push(Line::raw(""));
    l.push(section("actions"));
    if app.read_only {
        l.push(Line::from(st("  read-only mode", fx::STEEL)));
    } else {
        let mut s = vec![st("  ", TEXT)];
        for a in r.actions() {
            s.push(Span::styled(format!(" {} ", a.key()), Style::new().fg(fx::c(fx::BG)).bg(fx::c(fx::MAGENTA))));
            s.push(st(format!(" {}  ", a.label()), TEXT));
        }
        l.push(Line::from(s));
        if app.dry_run {
            l.push(Line::from(st("  --dry-run: actions are validated by the API server but not applied", fx::AMBER)));
        }
    }
    l
}

fn app_detail(app: &App, a: &crate::gitops::AppInfo) -> Vec<Line<'static>> {
    let mut l = vec![
        Line::from(vec![bold("Application ", fx::GREEN), bold(format!("{}/{}", a.namespace, a.name), TEXT), st(format!("  project {}", a.project), fx::STEEL)]),
        Line::from(vec![
            st(format!("{:<10}", "status"), fx::STEEL),
            bold(a.sync.clone(), sync_rgb(&a.sync)),
            st(" · ", fx::STEEL),
            bold(a.health.clone(), app_health_rgb(&a.health)),
            st(format!("  @ {}", if a.revision.is_empty() { "—" } else { &a.revision }), fx::STEEL),
        ]),
    ];
    if !a.health_message.is_empty() {
        l.push(Line::from(st(format!("          {}", a.health_message), fx::STEEL)));
    }
    for (i, src) in a.sources.iter().enumerate() {
        l.push(kv(if i == 0 { "source" } else { "" }, src.clone()));
    }
    l.push(kv("dest", a.destination.clone()));
    l.push(kv("policy", a.policy.clone()));
    if let Some(phase) = &a.op_phase {
        l.push(Line::raw(""));
        l.push(section("last operation"));
        l.push(Line::from(vec![
            st("  ", TEXT),
            bold(phase.clone(), op_rgb(phase)),
            st(format!("  by {}  {}", a.op_by, if a.op_when.is_empty() { String::new() } else { format!("{} ago", a.op_when) }), fx::STEEL),
        ]));
        if !a.op_message.is_empty() {
            l.push(Line::from(st(format!("  {}", a.op_message), if phase == "Succeeded" { fx::STEEL } else { op_rgb(phase) })));
        }
    }
    if !a.conditions.is_empty() {
        l.push(Line::raw(""));
        l.push(section("conditions"));
        l.extend(a.conditions.iter().map(|c| Line::from(st(format!("  {c}"), fx::RED))));
    }
    let drift = a.drift();
    l.push(Line::raw(""));
    l.push(section(&format!("drift ({})", drift.len())));
    if drift.is_empty() {
        l.push(Line::from(st("  live state matches Git", fx::GREEN)));
    }
    for r in &drift {
        l.push(Line::from(vec![
            st("  ≠ ", fx::AMBER),
            st(format!("{:<22}", pad(&r.kind, 21)), fx::STEEL),
            st(if r.namespace.is_empty() { r.name.clone() } else { format!("{}/{}", r.namespace, r.name) }, TEXT),
        ]));
    }
    // Unhealthy resources first, then the rest, compact.
    let mut res: Vec<_> = a.resources.iter().collect();
    res.sort_by_key(|r| (r.health == "Healthy" || r.health.is_empty(), r.kind.clone(), r.name.clone()));
    l.push(Line::raw(""));
    l.push(section(&format!("resources ({})", res.len())));
    for r in res.iter().take(40) {
        let mut s = vec![
            st(format!("  {} ", if r.drifted() { "≠" } else { " " }), fx::AMBER),
            st(format!("{:<22}", pad(&r.kind, 21)), fx::STEEL),
            st(pad(&r.name, 34), TEXT),
            st(format!(" {}", r.health), app_health_rgb(&r.health)),
        ];
        if !r.message.is_empty() {
            s.push(st(format!("  {}", r.message), fx::STEEL));
        }
        l.push(Line::from(s));
    }
    if res.len() > 40 {
        l.push(Line::from(st(format!("  … {} more (⏎ for all)", res.len() - 40), fx::STEEL)));
    }
    l.push(Line::raw(""));
    l.push(section("actions"));
    if app.read_only {
        l.push(Line::from(st("  read-only mode", fx::STEEL)));
    } else {
        let mut s = vec![st("  ", TEXT)];
        for act in crate::gitops::AppAction::ALL {
            s.push(Span::styled(format!(" {} ", act.key()), Style::new().fg(fx::c(fx::BG)).bg(fx::c(fx::GREEN))));
            s.push(st(format!(" {}  ", act.label()), TEXT));
        }
        l.push(Line::from(s));
        if a.auto {
            l.push(Line::from(st("  auto-sync is on: Argo CD will also sync by itself on the next Git change", fx::STEEL)));
        }
    }
    l
}

/// Secret → Certificate → Issuer, indented under a TLS reference.
fn cert_chain(net: &NetInfo, ns: &str, secret: &str) -> Vec<Line<'static>> {
    match net.cert_for(ns, secret) {
        Some(c) => {
            let status = match (c.ready, c.days_left) {
                (false, _) => format!("NOT READY — {} {}", c.reason, c.message),
                (true, Some(d)) => format!("Ready, expires in {d} days"),
                (true, None) => "Ready".into(),
            };
            let mut l = vec![Line::from(vec![
                st("    └ certificate ", fx::STEEL),
                st(format!("{}  ", c.name), TEXT),
                st(status, health_rgb(c.health())),
            ])];
            l.extend(c.challenges.iter().map(|ch| Line::from(st(format!("      challenge {ch}"), fx::AMBER))));
            l.extend(issuer_line(net, &c.namespace, &c.issuer).into_iter().map(|line| {
                let mut spans = vec![st("      └ ", fx::STEEL)];
                spans.extend(line.spans.into_iter().skip(1));
                Line::from(spans)
            }));
            l
        }
        None if net.tls_secrets.contains(&(ns.to_string(), secret.to_string())) => {
            vec![Line::from(st("    └ secret exists, no cert-manager Certificate (managed by hand / another tool)", fx::STEEL))]
        }
        None => vec![Line::from(st("    └ secret does not exist — TLS will fail (or fall back to the default certificate)", fx::RED))],
    }
}

fn issuer_line(net: &NetInfo, ns: &str, issuer: &str) -> Vec<Line<'static>> {
    let Some((kind, name)) = issuer.split_once('/') else { return vec![] };
    let found = net.issuers.iter().find(|i| i.name == name && i.cluster == (kind == "ClusterIssuer") && (i.cluster || i.namespace == ns));
    match found {
        Some(i) => vec![Line::from(vec![
            st(format!("{:<10}", ""), fx::STEEL),
            st(format!("{kind} {name} "), fx::STEEL),
            st(if i.ready { "ready" } else { "NOT READY" }, if i.ready { fx::GREEN } else { fx::RED }),
            st(format!("  {}", i.kind), fx::STEEL),
        ])],
        None => vec![Line::from(vec![st(format!("{:<10}", ""), fx::STEEL), st(format!("{kind} {name} not found"), fx::RED)])],
    }
}

fn rule_line(r: &Rule) -> Line<'static> {
    let join = |v: &[String]| if v.is_empty() { "—".to_string() } else { v.join(",") };
    let groups = r.api_groups.iter().map(|g| if g.is_empty() { "core".to_string() } else { g.clone() }).collect::<Vec<_>>();
    let (rgb, why) = match r.risk() {
        Some((h, why)) => (health_rgb(h), format!("  ← {why}")),
        None => (TEXT, String::new()),
    };
    let what = if r.urls.is_empty() { join(&r.resources) } else { format!("urls {}", join(&r.urls)) };
    let mut s = vec![
        st(format!("  {} ", pad(&what, 26)), rgb),
        st(format!("{} ", pad(&join(&r.verbs), 20)), rgb),
        st(pad(&join(&groups), 14), fx::STEEL),
    ];
    if !r.names.is_empty() {
        s.push(st(format!(" names={}", r.names.join(",")), fx::STEEL));
    }
    s.push(st(why, rgb));
    Line::from(s)
}

fn subject_detail(rbac: &RbacInfo, s: &Subject, bindings: &[&BindingInfo]) -> Vec<Line<'static>> {
    let mut l = vec![Line::from(vec![bold(format!("{} ", s.kind), fx::AMBER), bold(if s.namespace.is_empty() { s.name.clone() } else { format!("{}/{}", s.namespace, s.name) }, TEXT)])];
    if rbac.missing_sa(s) {
        l.push(Line::from(st("ServiceAccount does not exist — stale binding (and a name someone could claim)", fx::AMBER)));
    }
    if s.kind == "ServiceAccount" && s.name == "default" {
        l.push(Line::from(st("permissions on the default SA apply to every pod in the namespace without its own SA", fx::AMBER)));
    }
    for b in bindings {
        l.push(Line::raw(""));
        let role = rbac.role_of(b);
        let risk = role.and_then(|r| r.risk());
        l.push(Line::from(vec![
            st("── ", fx::STEEL),
            bold(b.name.clone(), fx::CYAN),
            st(format!(" ({}) → {}{}", b.scope(), if b.role_cluster { "ClusterRole/" } else { "Role/" }, b.role), TEXT),
            st(risk.map(|(_, w)| format!("  {w}")).unwrap_or_default(), risk.map_or(fx::STEEL, |(h, _)| health_rgb(h))),
        ]));
        match role {
            Some(r) => l.extend(r.rules.iter().map(rule_line)),
            None => l.push(Line::from(st("  role not found (dangling roleRef)", fx::RED))),
        }
    }
    l
}

fn confirm(f: &mut Frame, area: Rect, app: &App) {
    let Some(c) = &app.confirm else { return };
    let w = 76.min(area.width.saturating_sub(4));
    let popup = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + area.height / 3, width: w, height: 8 };
    f.render_widget(Clear, popup);
    let danger = c.pending.danger();
    let (verb, kind, target) = c.pending.title();
    let rgb = if danger { fx::RED } else { fx::AMBER };
    let lines = vec![
        Line::from(vec![
            bold(format!("{verb} "), rgb),
            st(format!("{kind} "), fx::STEEL),
            bold(target, TEXT),
        ]),
        Line::from(st(c.explain.clone(), fx::STEEL)),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" y ", Style::new().fg(fx::c(fx::BG)).bg(fx::c(rgb))),
            st(" confirm    ", TEXT),
            Span::styled(" n ", Style::new().fg(fx::c(fx::BG)).bg(fx::c(fx::STEEL))),
            st(" cancel", TEXT),
            st(if app.dry_run { "      (dry-run: nothing will change)" } else { "" }, fx::AMBER),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Double)
                .border_style(Style::new().fg(fx::c(rgb)))
                .title(Line::from(bold(" ▲ confirm action ", rgb)))
                .style(Style::new().bg(fx::c(fx::BG))),
        ),
        popup,
    );
}
