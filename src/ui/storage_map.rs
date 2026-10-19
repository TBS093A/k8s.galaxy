//! Storage map: every node is a disk platter with a usage arc (kubelet), the volumes pinned to it
//! orbit as planets — size by capacity, filled by usage. Storage outside the nodes (NFS, cloud
//! volumes) and distributed in-cluster pools (Longhorn, Ceph…) are blocks on the right, linked to
//! the nodes whose pods mount their volumes.

use std::collections::BTreeMap;
use std::f64::consts::TAU;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Line as CLine, Points, Rectangle};

use super::ops::{location_rgb, used_of};
use super::{frame, health_rgb, square_bounds};
use crate::app::{App, OpsRef};
use crate::fx::{self, Rgb, pulse};
use crate::metrics::fmt_mem;
use crate::storage::{Location, VolumeInfo};

const GIB: f64 = (1u64 << 30) as f64;

fn fill_rgb(frac: f64) -> Rgb {
    if frac >= 0.95 {
        fx::RED
    } else if frac >= 0.85 {
        fx::AMBER
    } else {
        fx::GREEN
    }
}

/// Volumes whose data is on one node's own disk.
fn node_local(v: &VolumeInfo) -> bool {
    matches!(v.kind.as_str(), "hostPath" | "local") || v.location.what().contains("node disk")
}

/// Where a volume is drawn: on a node, or on a storage host block.
enum Home {
    Node(String),
    Host(String),
    Nowhere,
}

fn home(v: &VolumeInfo) -> Home {
    if node_local(v) {
        return match v.measure_node() {
            Some(n) => Home::Node(n),
            None => Home::Nowhere,
        };
    }
    match &v.location {
        Location::Unknown => Home::Nowhere,
        l => Home::Host(l.what().to_string()),
    }
}

/// A planet: filled up to the used fraction (concentric rings from the centre), outline by health.
fn planet(ctx: &mut ratatui::widgets::canvas::Context, x: f64, y: f64, r: f64, used: Option<f64>, outline: Rgb) {
    if let Some(frac) = used {
        let color = fill_rgb(frac);
        let fill_r = r * frac.clamp(0.0, 1.0);
        let mut k = 0.35;
        while k < fill_r {
            ctx.draw(&Circle { x, y, radius: k, color: fx::fade(color, 0.85) });
            k += 0.45;
        }
    }
    ctx.draw(&Circle { x, y, radius: r, color: fx::fade(outline, if used.is_some() { 0.9 } else { 0.55 }) });
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let t = app.t();
    let (w, h) = square_bounds(area);
    let st = &app.snap.storage;
    let selected = match app.selected_ops() {
        Some(OpsRef::Volume(v)) => Some((v.group(), v.name())),
        _ => None,
    };

    // Storage hosts: distributed in-cluster pools first, then external services.
    let mut hosts: BTreeMap<(bool, String), Vec<&VolumeInfo>> = BTreeMap::new();
    let mut on_node: BTreeMap<String, Vec<&VolumeInfo>> = BTreeMap::new();
    for v in &st.volumes {
        match home(v) {
            Home::Node(n) => on_node.entry(n).or_default().push(v),
            Home::Host(hst) => hosts.entry((matches!(v.location, Location::External(_)), hst)).or_default().push(v),
            Home::Nowhere => {}
        }
    }

    let nodes: Vec<String> = if app.snap.nodes.is_empty() {
        st.node_fs.keys().cloned().collect()
    } else {
        app.snap.nodes.iter().map(|n| n.name.clone()).collect()
    };
    let zone_w = if hosts.is_empty() { w } else { w * 0.70 };
    let n = nodes.len().max(1);
    // Grid shaped to the zone, like the galaxy.
    let cols = ((n as f64 * zone_w / h).sqrt().ceil() as usize).clamp(1, n);
    let rows = n.div_ceil(cols);
    let (cw, ch) = (zone_w / cols as f64, h / rows as f64);
    let r = (cw * 0.36).min(ch * 0.30).max(3.0);
    // Leave room under the top edge for the two label lines.
    let center = |i: usize| (cw * ((i % cols) as f64 + 0.5), h - ch * ((i / cols) as f64 + 0.5) - 2.0);
    let label_chars = (cw - 2.0).max(8.0) as usize;
    let cut = |s: String| if s.chars().count() > label_chars { format!("{}…", s.chars().take(label_chars - 1).collect::<String>()) } else { s };

    // One size scale for every planet: the biggest volume gets ~a quarter of a node's radius.
    let max_gib = st.volumes.iter().filter_map(|v| v.size()).max().unwrap_or(GIB as i64) as f64 / GIB;
    let pr = |v: &VolumeInfo| {
        let gib = v.size().unwrap_or(0) as f64 / GIB;
        (0.7 + (gib.max(0.0).sqrt() / max_gib.sqrt().max(1e-9)) * (r * 0.24 - 0.7)).clamp(0.7, r * 0.26)
    };
    let used_frac = |v: &VolumeInfo| used_of(app, v).filter(|(_, size, _)| *size > 0).map(|(u, s, _)| u as f64 / s as f64);

    // Planet positions, computed once for drawing, links and the selection.
    let mut placed: Vec<(&VolumeInfo, f64, f64)> = vec![];
    for (i, node) in nodes.iter().enumerate() {
        let (cx, cy) = center(i);
        let list = on_node.get(node).map(Vec::as_slice).unwrap_or(&[]);
        for (k, v) in list.iter().enumerate() {
            let a = k as f64 / list.len() as f64 * TAU + t * 0.08 + i as f64;
            placed.push((v, cx + r * 0.62 * a.cos(), cy + r * 0.62 * a.sin()));
        }
    }
    let host_x = w * 0.74;
    let host_w = w * 0.24;
    let gap = 3.0;
    let host_h = if hosts.is_empty() { 0.0 } else { ((h - 4.0 - gap * (hosts.len() as f64 - 1.0)) / hosts.len() as f64).clamp(4.0, 12.0) };
    let mut boxes: Vec<(&(bool, String), f64, f64)> = vec![]; // key, x, y (bottom)
    for (j, (key, list)) in hosts.iter().enumerate() {
        let y = h - 2.0 - (j as f64 + 1.0) * host_h - j as f64 * gap;
        boxes.push((key, host_x, y));
        let slots = list.len().max(1) as f64;
        for (k, v) in list.iter().enumerate() {
            placed.push((v, host_x + host_w * (k as f64 + 0.5) / slots, y + host_h * 0.45));
        }
    }

    // Totals for the title.
    let disk_used: i64 = st.node_fs.values().map(|d| d.used).sum();
    let disk_cap: i64 = st.node_fs.values().map(|d| d.capacity).sum();
    let provisioned: i64 = st.volumes.iter().filter(|v| v.pv.is_some()).filter_map(|v| v.size()).sum();
    let used: i64 = st.volumes.iter().filter_map(|v| used_of(app, v).map(|u| u.0)).sum();
    let known = st.volumes.iter().filter(|v| used_of(app, v).is_some()).count();
    let external: i64 = st.volumes.iter().filter(|v| v.pv.is_some() && matches!(v.location, Location::External(_))).filter_map(|v| v.size()).sum();
    let title = format!(
        "STORAGE MAP   node disks {} / {} ({:.0}%)   volumes {} provisioned · {} used ({known} of {} measured)   external {}",
        fmt_mem(disk_used),
        fmt_mem(disk_cap),
        disk_used as f64 * 100.0 / disk_cap.max(1) as f64,
        fmt_mem(provisioned),
        fmt_mem(used),
        st.volumes.len(),
        fmt_mem(external),
    );

    let canvas = Canvas::default()
        .block(frame(&title, (150, 120, 255), false))
        .background_color(fx::c(fx::BG))
        .marker(Marker::Braille)
        .x_bounds([0.0, w])
        .y_bounds([0.0, h])
        .paint(|ctx| {
            // Links: node mounting an external/pool volume → its host, packets flowing.
            for (v, px, py) in &placed {
                if !matches!(home(v), Home::Host(_)) {
                    continue;
                }
                let color = location_rgb(&v.location);
                for (_, node) in &v.mounted_by {
                    let Some(i) = nodes.iter().position(|n| n == node) else { continue };
                    let (nx, ny) = center(i);
                    let (dx, dy) = (px - nx, py - ny);
                    let d = (dx * dx + dy * dy).sqrt().max(1e-6);
                    let (sx0, sy0) = (nx + dx / d * (r + 1.0), ny + dy / d * (r + 1.0));
                    ctx.draw(&CLine::new(sx0, sy0, *px, *py, fx::fade(color, 0.25)));
                    for p in 0..2 {
                        let s = (t * 0.3 + p as f64 * 0.5 + fx::unit(fx::hash(&v.name()))).fract();
                        ctx.draw(&Points { coords: &[(sx0 + (px - sx0) * s, sy0 + (py - sy0) * s)], color: fx::c(color) });
                    }
                }
            }
            ctx.layer();

            // Nodes: platter rings, hub, disk usage arc (clockwise from the top).
            for (i, node) in nodes.iter().enumerate() {
                let (cx, cy) = center(i);
                ctx.draw(&Circle { x: cx, y: cy, radius: r * 0.92, color: fx::fade(fx::STEEL, 0.25) });
                ctx.draw(&Circle { x: cx, y: cy, radius: r * 0.35, color: fx::fade(fx::STEEL, 0.18) });
                ctx.draw(&Circle { x: cx, y: cy, radius: 0.8, color: fx::fade(fx::STEEL, 0.6) });
                if let Some(d) = st.node_fs.get(node) {
                    let frac = d.used as f64 / d.capacity.max(1) as f64;
                    let pts = 96;
                    let lit = (frac.clamp(0.0, 1.0) * pts as f64).round() as usize;
                    let ring: Vec<_> = (0..pts)
                        .map(|j| {
                            let a = std::f64::consts::FRAC_PI_2 - j as f64 / pts as f64 * TAU;
                            (cx + (r + 0.6) * a.cos(), cy + (r + 0.6) * a.sin())
                        })
                        .collect();
                    ctx.draw(&Points { coords: &ring[lit..], color: fx::fade(fx::STEEL, 0.2) });
                    let rgb = fill_rgb(frac);
                    ctx.draw(&Points { coords: &ring[..lit], color: fx::fade(rgb, if frac > 0.85 { 0.6 + 0.4 * pulse(t, 0.8) } else { 0.95 }) });
                }
            }

            // Storage hosts.
            for ((external, _), x, y) in &boxes {
                let color = if *external { fx::CYAN } else { fx::GREEN };
                ctx.draw(&Rectangle { x: *x, y: *y, width: host_w, height: host_h, color: fx::fade(color, 0.7) });
                ctx.draw(&Rectangle { x: x + 0.6, y: y + 0.6, width: host_w - 1.2, height: host_h - 1.2, color: fx::fade(color, 0.2) });
            }

            // Volumes.
            for (v, x, y) in &placed {
                let outline = match v.health(app.measurement(v)) {
                    crate::model::Health::Ok => location_rgb(&v.location),
                    h => health_rgb(h),
                };
                planet(ctx, *x, *y, pr(v), used_frac(v), outline);
                if selected.as_ref().is_some_and(|(g, n)| *g == v.group() && *n == v.name()) {
                    ctx.draw(&Circle { x: *x, y: *y, radius: pr(v) + 1.2 + pulse(t, 0.8), color: fx::c(fx::MAGENTA) });
                }
            }
            ctx.layer();

            // Labels last.
            for (i, node) in nodes.iter().enumerate() {
                let (cx, cy) = center(i);
                let count = on_node.get(node).map_or(0, Vec::len);
                let name = cut(node.clone());
                let disk = cut(match st.node_fs.get(node) {
                    Some(d) => format!(
                        "{count} vol · {:.0}/{:.0}G {:.0}%",
                        d.used as f64 / GIB,
                        d.capacity as f64 / GIB,
                        d.used as f64 * 100.0 / d.capacity.max(1) as f64
                    ),
                    None => format!("{count} vol"),
                });
                let top = cy + r + 1.5;
                ctx.print(cx - name.chars().count() as f64 / 2.0, top + 2.0, Span::styled(name, Style::new().fg(fx::c((200, 220, 230)))));
                ctx.print(cx - disk.chars().count() as f64 / 2.0, top, Span::styled(disk, Style::new().fg(fx::c(fx::STEEL))));
            }
            for ((external, name), x, y) in &boxes {
                let list = &hosts[&(*external, name.clone())];
                let size: i64 = list.iter().filter_map(|v| v.size()).sum();
                let used: i64 = list.iter().filter_map(|v| used_of(app, v).map(|u| u.0)).sum();
                let label = format!("{} {name}  {}/{}", if *external { "→ external" } else { "◆ in-cluster pool" }, fmt_mem(used), fmt_mem(size));
                ctx.print(*x, y + host_h + 1.6, Span::styled(label, Style::new().fg(fx::c(if *external { fx::CYAN } else { fx::GREEN }))));
            }
            if let Some((v, x, y)) = placed.iter().find(|(v, _, _)| selected.as_ref().is_some_and(|(g, n)| *g == v.group() && *n == v.name())) {
                let usage = match used_of(app, v) {
                    Some((u, s, _)) => format!("{}/{}", fmt_mem(u), fmt_mem(s)),
                    None => format!("?/{}", v.size().map(fmt_mem).unwrap_or_default()),
                };
                let label = format!("{} {usage}", v.name());
                let lx = (x + pr(v) + 2.0).min(w - label.chars().count() as f64 - 1.0).max(0.0);
                ctx.print(lx, *y, TLine::from(Span::styled(label, Style::new().fg(fx::c(fx::MAGENTA)).add_modifier(Modifier::BOLD))));
            }
        });
    f.render_widget(canvas, area);
}
