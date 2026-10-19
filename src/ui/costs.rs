//! Costs window (`$`): the Hetzner bill by resource, uptime, cost since the last power-up, what
//! stays billed while the servers are deleted (powered down).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::app::App;
use crate::fx::{self, Rgb};

const TEXT: Rgb = (200, 220, 230);

fn st(text: impl Into<String>, rgb: Rgb) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(fx::c(rgb)))
}

fn ago(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    if d > 0 { format!("{d}d {h}h") } else { format!("{h}h {m}m") }
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let w = 104.min(area.width.saturating_sub(4));
    let mut lines: Vec<Line> = vec![];
    match &app.snap.costs {
        None => {
            lines.push(Line::from(st("No cost data: costs come from the Hetzner Cloud API.", TEXT)));
            lines.push(Line::from(st("Pass --hcloud-env-file PATH (or set HCLOUD_TOKEN) with a read token of the project.", fx::STEEL)));
        }
        Some(c) => {
            let sym = c.symbol().to_string();
            let mut kinds: Vec<&str> = c.items.iter().map(|i| i.kind).collect();
            kinds.dedup();
            for kind in ["server", "volume", "snapshot", "primary ip", "floating ip", "load balancer"] {
                let items: Vec<_> = c.items.iter().filter(|i| i.kind == kind).collect();
                if items.is_empty() {
                    continue;
                }
                let sum: f64 = items.iter().map(|i| i.monthly).sum();
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<16}", format!("{kind}s")), Style::new().fg(fx::c(fx::CYAN)).add_modifier(Modifier::BOLD)),
                    st(format!("{:>3} ", items.len()), fx::STEEL),
                    st(format!("{:>48}{sym}{sum:>8.2}/mo", ""), TEXT),
                ]));
                for i in items {
                    let price = if i.hourly > 0.0 { format!("{sym}{:.4}/h  ", i.hourly) } else { String::new() };
                    lines.push(Line::from(vec![
                        st(format!("  {:<26}", i.name.chars().take(25).collect::<String>()), TEXT),
                        st(format!("{:<14}{:<6}", i.detail.chars().take(13).collect::<String>(), i.location), fx::STEEL),
                        st(format!("{price:>16}"), fx::STEEL),
                        st(format!("{sym}{:>8.2}/mo", i.monthly), TEXT),
                        st(if i.kind == "server" && !i.running { "  off" } else { "" }, fx::AMBER),
                    ]));
                }
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(format!("{:<74}", "total now"), Style::new().fg(fx::c(TEXT)).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{sym}{:>8.2}/mo", c.monthly()), Style::new().fg(fx::c(fx::AMBER)).add_modifier(Modifier::BOLD)),
                st(format!("  ({sym}{:.3}/h)", c.hourly()), fx::STEEL),
            ]));
            lines.push(Line::from(vec![
                st(format!("{:<74}", "still billed when powered down (no servers)"), fx::STEEL),
                st(format!("{sym}{:>8.2}/mo", c.monthly_when_down()), fx::GREEN),
            ]));
            if let Some(up) = c.up_since() {
                let secs = k8s_openapi::jiff::Timestamp::now().as_second() - up.as_second();
                lines.push(Line::from(vec![
                    st(format!("{:<74}", format!("servers up for {} (since {})", ago(secs), up.strftime("%Y-%m-%d %H:%M UTC"))), fx::STEEL),
                    st(format!("{sym}{:>8.2} so far", c.spent_since_up()), TEXT),
                ]));
            }
            lines.push(Line::from(st("gross list prices from the Hetzner API; traffic and backups are not included", fx::STEEL)));
        }
    }

    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: area.y + area.height.saturating_sub(h) / 3, width: w, height: h };
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(fx::c(fx::AMBER)))
        .title(Line::from(vec![
            Span::styled(" ◆ ", Style::new().fg(fx::c(fx::AMBER))),
            Span::styled("COSTS", Style::new().fg(fx::c(fx::AMBER)).add_modifier(Modifier::BOLD)),
            st("  Hetzner Cloud ", fx::STEEL),
        ]))
        .title_bottom(Line::from(st(" Esc close ", fx::STEEL)))
        .style(Style::new().bg(fx::c(fx::BG)));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}
