//! Header, sidebar (nodes / details / pods), event ticker, footer and the logs popup.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};

use super::{frame, health_rgb};
use crate::app::{App, Focus, Res, ResKind, Row, View};
use crate::fx::{self, pulse};
use crate::model::Health;

fn styled(text: impl Into<String>, rgb: fx::Rgb) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(fx::c(rgb)))
}

pub fn header(f: &mut Frame, area: Rect, app: &App) {
    let t = app.t();
    let snap = &app.snap;
    let mut spans = vec![
        Span::styled(" ◆ K8S.GALAXY ", Style::new().fg(fx::c(fx::BG)).bg(fx::c(fx::CYAN)).add_modifier(Modifier::BOLD)),
        styled(format!("  {} ", snap.cluster), fx::STEEL),
    ];
    if snap.infra.hosting != crate::infra::Hosting::Unknown {
        spans.push(styled(format!("[{}] ", snap.infra.badge()), super::picker::hosting_rgb(&snap.infra)));
    }
    // One total per currency (Hetzner bills in EUR, the others in USD).
    let mut totals: Vec<(String, f64)> = vec![];
    for c in &snap.costs {
        match totals.iter_mut().find(|(s, _)| s == c.symbol()) {
            Some((_, t)) => *t += c.monthly(),
            None => totals.push((c.symbol().to_string(), c.monthly())),
        }
    }
    if !totals.is_empty() {
        let text = totals.iter().map(|(s, t)| format!("{s}{t:.0}")).collect::<Vec<_>>().join(" + ");
        spans.push(styled(format!("{text}/mo "), fx::AMBER));
    }
    spans.push(Span::raw(" "));
    // Nine views do not fit next to a long cluster name on a narrow terminal: short labels then.
    let full: usize = View::ALL.iter().enumerate().map(|(i, v)| format!(" {} {} ", i + 1, v.title()).chars().count()).sum();
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let short = used + full + 60 > area.width as usize;
    for (i, v) in View::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, if short { v.short() } else { v.title() });
        spans.push(if *v == app.view {
            Span::styled(label, Style::new().fg(fx::c(fx::BG)).bg(fx::c(fx::MAGENTA)).add_modifier(Modifier::BOLD))
        } else {
            styled(label, fx::STEEL)
        });
    }
    let alerts = snap.alerts();
    let alert_rgb = if alerts > 0 { fx::RED } else { fx::GREEN };
    spans.extend([
        styled("   NODES ", fx::STEEL),
        Span::styled(format!("{:02}", snap.nodes.len()), Style::new().fg(fx::c(fx::CYAN)).add_modifier(Modifier::BOLD)),
        styled("  PODS ", fx::STEEL),
        Span::styled(format!("{:02}", snap.pods.len()), Style::new().fg(fx::c(fx::CYAN)).add_modifier(Modifier::BOLD)),
        styled("  ALERTS ", fx::STEEL),
        Span::styled(format!("{alerts:02}"), Style::new().fg(fx::c(alert_rgb)).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
    ]);
    if !app.synced {
        let dots = ".".repeat((t * 2.0) as usize % 4);
        let text = match (&app.switching, app.active) {
            (Some(label), _) => format!("◌ CONNECTING to {label}{dots}"),
            (None, crate::app::Active::None) => "◌ NO CLUSTER — press c to pick one".to_string(),
            _ => format!("◌ CONNECTING{dots}"),
        };
        spans.push(Span::styled(text, Style::new().fg(fx::c(fx::AMBER))));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    match &snap.error {
        // Data from an earlier poll is still shown: say how old it is rather than "lost".
        Some(err) if !snap.nodes.is_empty() => spans.push(Span::styled(
            format!("● STALE {}s: {}", snap.fetched_at.elapsed().as_secs(), err.lines().next().unwrap_or_default()),
            Style::new().fg(fx::fade(fx::AMBER, 0.5 + 0.5 * pulse(t, 1.0))),
        )),
        Some(err) => spans.push(Span::styled(
            format!("● LINK LOST: {}", err.lines().next().unwrap_or_default()),
            Style::new().fg(fx::fade(fx::RED, 0.5 + 0.5 * pulse(t, 1.0))),
        )),
        None => {
            spans.push(Span::styled("● LIVE", Style::new().fg(fx::fade(fx::GREEN, 0.4 + 0.6 * pulse(t, 2.0)))));
            spans.push(styled(format!(" sync {}s", snap.fetched_at.elapsed().as_secs()), fx::STEEL));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn sidebar(f: &mut Frame, area: Rect, app: &App) {
    let node_rows = (app.snap.nodes.len() as u16 + 2).clamp(3, 14);
    let [nodes_area, detail_area, pods_area] =
        Layout::vertical([Constraint::Length(node_rows), Constraint::Length(11), Constraint::Fill(1)]).areas(area);

    // Nodes.
    let items: Vec<ListItem> = app
        .snap
        .nodes
        .iter()
        .map(|n| {
            let glyph = if n.is_control_plane() { "◆ " } else { "● " };
            ListItem::new(Line::from(vec![
                styled(glyph, health_rgb(n.health())),
                Span::styled(format!("{:<20}", truncate(&n.name, 20)), Style::new().fg(fx::c((200, 220, 230)))),
                if n.unschedulable {
                    styled(format!("{} ⊘", truncate(&n.place, 17)), fx::AMBER)
                } else {
                    styled(truncate(&n.place, 20), fx::STEEL)
                },
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.node_idx));
    let list = List::new(items)
        .block(frame("NODES", fx::CYAN, app.focus == Focus::Nodes))
        .highlight_style(Style::new().bg(fx::fade(fx::CYAN, 0.18)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶");
    f.render_stateful_widget(list, nodes_area, &mut state);

    // Details.
    let pods = app.selected_pods();
    let detail = match app.selected_node() {
        None => vec![Line::from(styled("waiting for cluster data…", fx::STEEL))],
        Some(n) => {
            let bad = pods.iter().filter(|p| p.health() == Health::Bad).count();
            let warn = pods.iter().filter(|p| p.health() == Health::Warn).count();
            let kv = |k: &str, v: String| Line::from(vec![styled(format!("{k:<9}"), fx::STEEL), styled(v, (200, 220, 230))]);
            vec![
                kv(
                    "status",
                    format!(
                        "{}{}",
                        if n.ready { "Ready" } else { "NotReady" },
                        if n.unschedulable { " · cordoned (C uncordon)" } else { "" }
                    ),
                ),
                kv("roles", n.roles.join(",")),
                kv("place", n.place.clone()),
                kv("region", format!("{} / {}", dash(&n.region), dash(&n.zone))),
                kv("ip", format!("{}  {}", n.internal_ip, n.kubelet)),
                kv("infra", {
                    // What the node is; with a Hetzner bill, the server type and its price instead of the id.
                    let id = n
                        .cloud
                        .as_deref()
                        .and_then(|c| c.split_whitespace().last())
                        .filter(|id| id.starts_with('#'))
                        .map(|id| id.trim_start_matches('#').to_string())
                        .or_else(|| n.provider_id.strip_prefix("hcloud://").map(str::to_string));
                    // Hetzner servers match by id, AWS/GCP/Azure nodes by name.
                    let priced = app
                        .snap
                        .costs
                        .iter()
                        .find_map(|c| id.as_deref().and_then(|id| c.server(id)).or_else(|| c.server(&n.name)).map(|s| (c, s)));
                    match (priced, &id) {
                        (Some((c, s)), _) => format!("{} {} · {}{:.3}/h", crate::infra::node_kind(n), s.detail, c.symbol(), s.hourly),
                        (None, Some(id)) => format!("{} #{id}", crate::infra::node_kind(n)),
                        _ => crate::infra::node_kind(n),
                    }
                }),
                load_line(
                    "cpu",
                    app.node_usage(&n.name).map(|u| u.cpu_m),
                    app.node_requests(&n.name).cpu_m,
                    n.allocatable.cpu_m,
                    crate::metrics::fmt_cpu,
                ),
                load_line(
                    "mem",
                    app.node_usage(&n.name).map(|u| u.mem),
                    app.node_requests(&n.name).mem,
                    n.allocatable.mem,
                    crate::metrics::fmt_mem,
                ),
                Line::from(vec![
                    styled(format!("{:<9}", "pods"), fx::STEEL),
                    styled(format!("{} ", pods.len()), (200, 220, 230)),
                    styled(format!("×{bad} "), fx::RED),
                    styled(format!("▲{warn}  "), fx::AMBER),
                    styled(format!("{} cpu {}", n.cpu, n.memory), fx::STEEL),
                ]),
            ]
        }
    };
    f.render_widget(
        Paragraph::new(detail).block(frame(
            if app.snap.caps.metrics { "DETAILS" } else { "DETAILS  no metrics-server: requests" },
            fx::AMBER,
            false,
        )),
        detail_area,
    );

    resources(f, pods_area, app);
}

/// Resource list: namespace sections, kind tabs in the title (`t` cycles them).
fn resources(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Resources;
    let text = (200, 220, 230);
    let rows = app.rows();
    let mut selected_row = None;
    let mut item_no = 0;
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(row_no, row)| match row {
            Row::Header { namespace, total, bad, warn } => {
                let mut spans = vec![
                    Span::styled(format!("▾ {namespace} "), Style::new().fg(fx::c(fx::CYAN)).add_modifier(Modifier::BOLD)),
                    styled(format!("{total}"), fx::STEEL),
                ];
                if *bad > 0 {
                    spans.push(styled(format!(" ×{bad}"), fx::RED));
                }
                if *warn > 0 {
                    spans.push(styled(format!(" ▲{warn}"), fx::AMBER));
                }
                ListItem::new(Line::from(spans))
            }
            Row::Item(res) => {
                if focused && item_no == app.res_idx {
                    selected_row = Some(row_no);
                }
                item_no += 1;
                let h = res.health();
                let mut spans = vec![styled("  • ", health_rgb(h))];
                match res {
                    Res::Pod(p) => {
                        spans.push(styled(truncate(&p.name, 26), text));
                        if h != Health::Ok {
                            spans.push(styled(format!(" {}", p.status()), health_rgb(h)));
                        }
                        if p.restarts > 0 {
                            spans.push(styled(format!(" r:{}", p.restarts), fx::STEEL));
                        }
                        if let Some(u) = app.pod_usage(p) {
                            spans.push(styled(
                                format!(" {} {}", crate::metrics::fmt_cpu(u.cpu_m), crate::metrics::fmt_mem(u.mem)),
                                (120, 150, 170),
                            ));
                        }
                    }
                    Res::Workload(w) => {
                        spans.push(styled(format!("{:<30}", truncate(&w.name, 29)), text));
                        spans.push(styled(format!("{}/{}", w.ready, w.desired), health_rgb(h)));
                    }
                    Res::Helm(r) => {
                        spans.push(styled(format!("{:<15}", truncate(&r.name, 14)), text));
                        spans.push(styled(format!("{:<17}", truncate(&r.chart, 16)), fx::STEEL));
                        let state = if r.status == "deployed" { format!("r{}", r.revision) } else { r.status.clone() };
                        spans.push(styled(state, health_rgb(h)));
                    }
                    Res::Job(j) => {
                        spans.push(styled(format!("{:<19}", truncate(&j.name, 18)), text));
                        spans.push(styled(format!("{:<9}", j.status.to_string()), health_rgb(h)));
                        spans.push(styled(format!("{}/{}", j.succeeded, j.completions), fx::STEEL));
                        if let Some(d) = j.duration_secs() {
                            spans.push(styled(format!(" {}m{:02}s", d / 60, d % 60), (120, 150, 170)));
                        }
                    }
                    Res::Cron(c) => {
                        spans.push(styled(format!("{:<16}", truncate(&c.name, 15)), text));
                        spans.push(styled(format!("{:<13}", truncate(&c.schedule, 12)), fx::STEEL));
                        let when = if c.suspend {
                            "suspended".to_string()
                        } else {
                            c.next.map(crate::jobs::until).unwrap_or_else(|| "—".into())
                        };
                        spans.push(styled(when, health_rgb(h)));
                    }
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();
    let empty = items.is_empty();

    // Title: kind tabs, the current one inverted.
    let accent = fx::GREEN;
    let mut title = vec![Span::styled(" ◆ ", Style::new().fg(fx::c(accent)))];
    for kind in ResKind::ALL {
        title.push(if kind == app.res_kind {
            Span::styled(format!(" {} ", kind.short()), Style::new().fg(fx::c(fx::BG)).bg(fx::c(accent)).add_modifier(Modifier::BOLD))
        } else {
            Span::styled(format!(" {} ", kind.short()), Style::new().fg(fx::fade(accent, 0.6)))
        });
    }
    title.push(Span::raw(" "));
    let scope = match app.res_kind {
        ResKind::Pods => format!("on {}", app.selected_node().map(|n| n.name.as_str()).unwrap_or("—")),
        ResKind::ReplicaSets if app.stale_replicasets() > 0 => format!("cluster, {} scaled-to-0 hidden", app.stale_replicasets()),
        _ => "cluster".into(),
    };
    let enter = match app.res_kind {
        ResKind::Pods => "logs",
        ResKind::Helm => "info",
        ResKind::Jobs => "info · d delete",
        ResKind::CronJobs => "info · x run · s suspend",
        _ => "pods",
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { fx::c(accent) } else { fx::fade(accent, 0.45) }))
        .title(Line::from(title))
        .title_bottom(Line::from(styled(format!(" t kind · ⏎ {enter} · {scope} "), fx::STEEL)));

    if empty {
        let msg = match app.res_kind {
            ResKind::Helm => "no Helm releases (or no access to secrets)",
            ResKind::Jobs => "no Jobs",
            ResKind::CronJobs => "no CronJobs",
            _ => "nothing here",
        };
        f.render_widget(Paragraph::new(styled(msg, fx::STEEL)).block(block), area);
        return;
    }
    let mut state = ListState::default().with_selected(selected_row);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(fx::fade(accent, 0.18)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶");
    f.render_stateful_widget(list, area, &mut state);
}

pub fn ticker(f: &mut Frame, area: Rect, app: &App) {
    let warnings = &app.snap.warnings;
    let rows = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if warnings.is_empty() {
        vec![Line::from(styled("all quiet — no warning events", fx::STEEL))]
    } else {
        warnings
            .iter()
            .take(rows)
            .enumerate()
            .map(|(i, e)| {
                let rgb = if i == 0 { fx::mix(fx::AMBER, fx::RED, pulse(app.t(), 1.2)) } else { fx::AMBER };
                Line::from(vec![
                    styled("▲ ", rgb),
                    Span::styled(format!("{:<16}", truncate(&e.reason, 16)), Style::new().fg(fx::c(rgb)).add_modifier(Modifier::BOLD)),
                    styled(format!("{:<34} ", truncate(&e.object, 34)), fx::CYAN),
                    styled(if e.count > 1 { format!("×{:<4}", e.count) } else { "     ".into() }, fx::STEEL),
                    styled(e.message.clone(), (170, 185, 195)),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(frame(&format!("WARNINGS  {}", warnings.len()), fx::AMBER, false)), area);
}

pub fn footer(f: &mut Frame, area: Rect, app: &App) {
    let key = |k: &str, d: &str| {
        vec![Span::styled(format!(" {k} "), Style::new().fg(fx::c(fx::BG)).bg(fx::c(fx::STEEL))), styled(format!(" {d}  "), fx::STEEL)]
    };
    let keys: &[(&str, &str)] = if app.view.is_ops() {
        &[
            ("1-9/Tab", "view"),
            ("j/k", "select"),
            ("t", "tab"),
            ("⏎", "details"),
            ("Esc", "map"),
            ("C/D", "cordon·drain"),
            ("c", "clusters"),
            ("$", "costs"),
            ("r", "refresh"),
            ("q", "quit"),
        ]
    } else {
        &[
            ("1-9/Tab", "view"),
            ("j/k", "select"),
            ("⏎/→", "list·open"),
            ("t", "kind"),
            ("Esc", "back"),
            ("+/-", "zoom"),
            ("HJKL", "pan"),
            ("f", "fit"),
            ("0", "world"),
            ("c", "clusters"),
            ("$", "costs"),
            ("r", "refresh"),
            ("q", "quit"),
        ]
    };
    let mut spans: Vec<Span> = keys.iter().flat_map(|(k, d)| key(k, d)).collect();
    if let Some(toast) = app.toast.as_ref().filter(|t| t.at.elapsed().as_secs() < 8) {
        let rgb = if toast.ok { fx::GREEN } else { fx::RED };
        spans.push(Span::styled(format!(" {} ", if toast.ok { "✓" } else { "×" }), Style::new().fg(fx::c(fx::BG)).bg(fx::c(rgb))));
        spans.push(styled(format!(" {}", toast.text), rgb));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn logs(f: &mut Frame, area: Rect, app: &App) {
    let Some(logs) = &app.logs else { return };
    let popup = centered(area, 90, 85);
    f.render_widget(Clear, popup);
    let height = popup.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if logs.loading {
        vec![Line::from(styled("fetching logs…", fx::STEEL))]
    } else {
        let start = logs.scroll.saturating_sub(height.saturating_sub(1));
        logs.lines
            .iter()
            .skip(start)
            .take(height)
            .map(|l| {
                let lower = l.to_lowercase();
                let bad = ["error", "panic", "fatal", "crashloop", "backoff", "oomkilled", "failed"];
                let rgb = if bad.iter().any(|k| lower.contains(k)) {
                    fx::RED
                } else if lower.contains("warn") || lower.contains("pending") {
                    fx::AMBER
                } else {
                    (170, 185, 195)
                };
                Line::from(styled(l.clone(), rgb))
            })
            .collect()
    };
    let prefix = if logs.is_log { "LOGS " } else { "" };
    let title = format!("{prefix}{}  [{}/{}]  j/k PgUp/PgDn g/G  Esc close", logs.title, logs.scroll + 1, logs.lines.len());
    f.render_widget(Paragraph::new(lines).style(Style::new().bg(fx::c(fx::BG))).block(frame(&title, fx::GREEN, true)), popup);
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let [_, v, _] =
        Layout::vertical([Constraint::Percentage((100 - pct_y) / 2), Constraint::Percentage(pct_y), Constraint::Fill(1)]).areas(area);
    let [_, h, _] =
        Layout::horizontal([Constraint::Percentage((100 - pct_x) / 2), Constraint::Percentage(pct_x), Constraint::Fill(1)]).areas(v);
    h
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

fn dash(s: &str) -> &str {
    if s.is_empty() { "—" } else { s }
}

/// `cpu  ▰▰▰▱▱▱ 34% 1.3/3.8 req 55%` — live usage when metrics exist, otherwise requests.
fn load_line(name: &str, used: Option<i64>, requested: i64, total: i64, fmt: fn(i64) -> String) -> Line<'static> {
    let total = total.max(1);
    let (value, live) = match used {
        Some(u) => (u, true),
        None => (requested, false),
    };
    let frac = value as f64 / total as f64;
    let rgb = crate::ui::galaxy::load_rgb(frac);
    let cells = 8;
    let lit = ((frac.clamp(0.0, 1.0)) * cells as f64).round() as usize;
    let mut spans = vec![
        styled(format!("{name:<9}"), fx::STEEL),
        styled("▰".repeat(lit), rgb),
        styled("▱".repeat(cells - lit), (40, 55, 65)),
        styled(format!(" {:>3.0}% ", frac * 100.0), rgb),
        styled(format!("{}/{}", fmt(value), fmt(total)), (200, 220, 230)),
    ];
    spans.push(if live {
        styled(format!(" req {:.0}%", requested as f64 * 100.0 / total as f64), fx::STEEL)
    } else {
        styled(" requested", fx::STEEL)
    });
    Line::from(spans)
}
