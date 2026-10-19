//! Cluster picker: every context found in ~/.kube and $KUBECONFIG, probed live.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::app::{Active, App, ProbeState};
use crate::fx::{self, Rgb};
use crate::infra::{Hosting, Infra};
use crate::kubeconfigs::{short_server, tilde};

const TEXT: Rgb = (200, 220, 230);

fn st(text: impl Into<String>, rgb: Rgb) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(fx::c(rgb)))
}

fn pad(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count > n {
        format!("{}… ", s.chars().take(n.saturating_sub(1)).collect::<String>())
    } else {
        format!("{s}{} ", " ".repeat(n - count))
    }
}

/// Color of a hosting kind: managed services cyan, cloud VMs amber, bare metal green.
pub fn hosting_rgb(infra: &Infra) -> Rgb {
    match infra.hosting {
        Hosting::Managed(_) => fx::CYAN,
        Hosting::CloudVms(_) => fx::AMBER,
        Hosting::BareMetal => fx::GREEN,
        Hosting::Hybrid(_) => fx::MAGENTA,
        Hosting::Local(_) | Hosting::Unknown => fx::STEEL,
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let Some(sel) = app.picker else { return };
    let t = app.t();
    let rows = app.clusters.len() + 1;
    let w = area.width.saturating_sub(8).min(150);
    let h = (rows as u16 + 9).min(area.height.saturating_sub(2));
    let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height.saturating_sub(h)) / 3, width: w, height: h };
    f.render_widget(Clear, popup);

    let spinner = ["◐", "◓", "◑", "◒"][((t * 6.0) as usize) % 4];
    let mut lines = vec![Line::from(vec![
        st("    ", TEXT),
        st(pad("CLUSTER", 26), fx::STEEL),
        st(pad("CONTEXT", 32), fx::STEEL),
        st(pad("API SERVER", 24), fx::STEEL),
        st(pad("VERSION", 11), fx::STEEL),
        st(pad("NODES", 6), fx::STEEL),
        st("RUNS ON", fx::STEEL),
    ])];
    for (i, c) in app.clusters.iter().enumerate() {
        let selected = i == sel;
        let mut spans = vec![st(if selected { " ▶ " } else { "   " }, fx::MAGENTA)];
        let probe = app.probes.get(i).cloned().unwrap_or(ProbeState::Pending);
        match &probe {
            ProbeState::Pending => spans.push(st(spinner, fx::STEEL)),
            ProbeState::Ok(_) => spans.push(st("●", fx::GREEN)),
            ProbeState::Err(_) => spans.push(st("●", fx::RED)),
        }
        let name_style = if selected { Style::new().fg(fx::c(TEXT)).add_modifier(Modifier::BOLD) } else { Style::new().fg(fx::c(TEXT)) };
        spans.push(Span::styled(format!(" {}", pad(&c.label, 25)), name_style));
        spans.push(st(pad(&c.context, 32), fx::STEEL));
        spans.push(st(pad(&short_server(&c.server), 24), fx::STEEL));
        match &probe {
            ProbeState::Pending => spans.push(st("probing…", fx::STEEL)),
            ProbeState::Ok(p) => {
                spans.push(st(pad(&p.version, 11), TEXT));
                spans.push(st(pad(&p.nodes.to_string(), 6), TEXT));
                spans.push(st(p.infra.badge(), hosting_rgb(&p.infra)));
            }
            ProbeState::Err(e) => spans.push(st(format!("unreachable: {}", short_error(e)), fx::RED)),
        }
        if app.active == Active::Entry(i) {
            spans.push(st("  ◆ active", fx::MAGENTA));
        } else if c.default {
            spans.push(st("  (default context)", fx::STEEL));
        }
        lines.push(Line::from(spans));
    }
    let demo_sel = sel == app.clusters.len();
    lines.push(Line::from(vec![
        st(if demo_sel { " ▶ " } else { "   " }, fx::MAGENTA),
        st("◇", fx::CYAN),
        st(format!(" {}", pad("demo", 25)), TEXT),
        st("simulated cluster: Hetzner + AWS + homelab, failures included", fx::STEEL),
        st(if app.active == Active::Demo { "  ◆ active" } else { "" }, fx::MAGENTA),
    ]));

    // Details of the selected row.
    lines.push(Line::raw(""));
    if let Some(c) = app.clusters.get(sel) {
        lines.push(Line::from(vec![st("  file    ", fx::STEEL), st(tilde(&c.file), TEXT), st(format!("   user {}", c.user), fx::STEEL)]));
        match app.probes.get(sel) {
            Some(ProbeState::Ok(p)) => {
                let cp = if p.infra.control_plane_visible {
                    "control plane runs on your nodes (self-managed)"
                } else if p.infra.managed() {
                    "control plane run by the provider"
                } else {
                    "no control-plane nodes visible"
                };
                lines.push(Line::from(vec![st("  infra   ", fx::STEEL), st(p.infra.badge(), hosting_rgb(&p.infra)), st(format!(" — {cp}"), fx::STEEL)]));
            }
            Some(ProbeState::Err(e)) => lines.push(Line::from(vec![st("  error   ", fx::STEEL), st(e.clone(), fx::RED)])),
            _ => {}
        }
    } else {
        lines.push(Line::from(st("  no API server: everything is simulated, actions included", fx::STEEL)));
    }

    let title = Line::from(vec![
        Span::styled(" ◆ ", Style::new().fg(fx::c(fx::CYAN))),
        Span::styled("CLUSTERS", Style::new().fg(fx::c(fx::CYAN)).add_modifier(Modifier::BOLD)),
        st(format!("  {} found in ~/.kube and $KUBECONFIG ", app.clusters.len()), fx::STEEL),
    ]);
    let esc = if app.active == Active::None { "quit" } else { "close" };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(fx::c(fx::CYAN)))
        .title(title)
        .title_bottom(Line::from(st(format!(" j/k select · ⏎ connect · r re-probe · Esc {esc} "), fx::STEEL)))
        .style(Style::new().bg(fx::c(fx::BG)));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), popup);
}

/// The interesting tail of a connection error (`…: Connection refused (os error 111)`).
fn short_error(e: &str) -> String {
    if e.contains("deadline has elapsed") || e.contains("timed out") {
        return "connection timed out".into();
    }
    let last = e.rsplit(": ").next().unwrap_or(e);
    let s = if last.len() < 12 { e } else { last };
    s.chars().take(60).collect()
}
