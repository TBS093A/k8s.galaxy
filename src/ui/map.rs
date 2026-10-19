//! Geographic view: braille world map, nodes pinned to their datacenters,
//! links to the control plane with packets flowing along them.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Line as CLine, Map, MapResolution, Points};

use super::labels::Placer;
use super::{frame, health_rgb, square_bounds};
use crate::app::App;
use crate::fx::{self, hash, pulse, unit};
use crate::model::Health;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.t();
    let (w, h) = square_bounds(area);
    app.map_size = (area.width.saturating_sub(2), area.height.saturating_sub(2));

    let cam = app.cam;
    let lon_span = 360.0 / cam.zoom;
    let lat_span = lon_span * h / w;
    let x_bounds = [cam.lon - lon_span / 2.0, cam.lon + lon_span / 2.0];
    let y_bounds = [cam.lat - lat_span / 2.0, cam.lat + lat_span / 2.0];
    let col = lon_span / w; // degrees per column

    // Nodes sharing a datacenter are fanned out on a small ring so each stays visible.
    let mut groups: BTreeMap<(i64, i64), Vec<usize>> = BTreeMap::new();
    for (i, n) in app.snap.nodes.iter().enumerate() {
        if let Some((lon, lat)) = n.geo {
            groups.entry(((lon * 10.0) as i64, (lat * 10.0) as i64)).or_default().push(i);
        }
    }
    let mut pos: Vec<Option<(f64, f64)>> = vec![None; app.snap.nodes.len()];
    let mut sites: Vec<(f64, f64, Health, bool)> = vec![]; // lon, lat, worst health, has control plane
    for members in groups.values() {
        let (lon, lat) = app.snap.nodes[members[0]].geo.unwrap();
        let ring = if members.len() > 1 { col * (1.5 + 0.8 * members.len() as f64) } else { 0.0 };
        for (k, &i) in members.iter().enumerate() {
            let a = k as f64 / members.len() as f64 * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2;
            pos[i] = Some((lon + ring * a.cos(), lat + ring * a.sin()));
        }
        let worst = members.iter().map(|&i| app.snap.nodes[i].health()).min().unwrap_or(Health::Ok);
        let cp = members.iter().any(|&i| app.snap.nodes[i].is_control_plane());
        sites.push((lon, lat, worst, cp));
    }
    let hub = sites.iter().find(|s| s.3).or(sites.first()).map(|s| (s.0, s.1));

    let selected = app.node_idx;
    let nodes = &app.snap.nodes;
    let unplaced = nodes.iter().filter(|n| n.geo.is_none()).count();

    // Claim terminal cells in priority order: selected, then worst health, control planes, the rest.
    // Markers first (nudged when a cell is taken), then labels (dropped when nothing fits).
    let mut order: Vec<usize> = (0..nodes.len()).filter(|&i| pos[i].is_some()).collect();
    order.sort_by_key(|&i| (i != selected, nodes[i].health(), !nodes[i].is_control_plane(), nodes[i].name.clone()));
    let mut placer = Placer::new(area, x_bounds, y_bounds);
    let mut marker_xy: Vec<Option<(f64, f64)>> = vec![None; nodes.len()];
    for &i in &order {
        let (x, y) = pos[i].unwrap();
        marker_xy[i] = placer.marker(x, y, 3);
    }
    let mut label_xy: Vec<Option<(f64, f64)>> = vec![None; nodes.len()];
    for &i in &order {
        if let Some((x, y)) = marker_xy[i] {
            label_xy[i] = placer.label(x, y, nodes[i].name.chars().count());
        }
    }
    let hidden = order.iter().filter(|&&i| marker_xy[i].is_some() && label_xy[i].is_none()).count();

    let mut title = format!("GEO MAP  ×{:.1}", cam.zoom);
    if unplaced > 0 {
        title.push_str(&format!("  {unplaced} unplaced"));
    }
    if hidden > 0 {
        title.push_str(&format!("  {hidden} labels hidden, zoom in (+)"));
    }

    let canvas = Canvas::default()
        .block(frame(&title, fx::CYAN, true))
        .background_color(fx::c(fx::BG))
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|ctx| {
            // Graticule, then landmass on top of it.
            for lon in (-180..=180).step_by(30) {
                let lon = lon as f64;
                ctx.draw(&CLine::new(lon, -90.0, lon, 90.0, fx::c(fx::GRID)));
            }
            for lat in (-60..=90).step_by(30) {
                let lat = lat as f64;
                ctx.draw(&CLine::new(-180.0, lat, 180.0, lat, fx::c(fx::GRID)));
            }
            ctx.draw(&Map { resolution: MapResolution::High, color: fx::c(fx::LAND) });
            ctx.layer();

            // Links site -> hub with travelling packets.
            if let Some((hx, hy)) = hub {
                for &(sx, sy, health, _) in &sites {
                    if (sx, sy) == (hx, hy) {
                        continue;
                    }
                    let link = if health == Health::Bad { fx::RED } else { fx::CYAN };
                    ctx.draw(&CLine::new(sx, sy, hx, hy, fx::fade(link, 0.22)));
                    let seed = unit(hash(&format!("{sx}{sy}")));
                    for p in 0..3 {
                        let s = (t * 0.35 + seed + p as f64 / 3.0).fract();
                        // Trail: head bright, tail faded.
                        for k in 0..4 {
                            let s = s - k as f64 * 0.012;
                            if s < 0.0 {
                                continue;
                            }
                            let (x, y) = (sx + (hx - sx) * s, sy + (hy - sy) * s);
                            ctx.draw(&Points { coords: &[(x, y)], color: fx::fade(link, 1.0 - k as f64 * 0.25) });
                        }
                    }
                }
            }
            ctx.layer();

            // Site beacons: expanding sonar rings.
            for &(sx, sy, health, cp) in &sites {
                let rgb = health_rgb(health);
                let period = if health == Health::Bad { 0.8 } else { 2.4 };
                let phase = unit(hash(&format!("{sx}{sy}"))) * period;
                for wave in 0..2 {
                    let k = ((t + phase) / period + wave as f64 * 0.5).fract();
                    ctx.draw(&Circle { x: sx, y: sy, radius: col * (1.0 + k * 6.0), color: fx::fade(rgb, (1.0 - k) * 0.7) });
                }
                if cp {
                    ctx.draw(&Circle { x: sx, y: sy, radius: col * 8.0, color: fx::fade(fx::MAGENTA, 0.35) });
                }
            }

            // Crosshair on the selected node.
            // Leader lines from a nudged marker back to its real location.
            for (i, m) in marker_xy.iter().enumerate() {
                if let (Some((mx, my)), Some((px, py))) = (m, pos[i])
                    && ((mx - px).powi(2) + (my - py).powi(2)).sqrt() > col * 1.5
                {
                    ctx.draw(&CLine::new(*mx, *my, px, py, fx::fade(fx::STEEL, 0.35)));
                }
            }

            if let Some(Some((x, y))) = marker_xy.get(selected) {
                // Short targeting ticks instead of a full-screen crosshair.
                let ch = fx::fade(fx::MAGENTA, 0.5 + 0.4 * pulse(t, 1.5));
                let (gap, len) = (col * 4.0, col * 9.0);
                for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                    ctx.draw(&CLine::new(x + dx * gap, y + dy * gap, x + dx * len, y + dy * len, ch));
                }
                ctx.draw(&Circle { x: *x, y: *y, radius: col * (2.0 + pulse(t, 1.0)), color: fx::c(fx::MAGENTA) });
            }

            for (i, n) in nodes.iter().enumerate() {
                let Some((x, y)) = marker_xy[i] else { continue };
                let rgb = health_rgb(n.health());
                let blink = n.health() == Health::Bad && pulse(t, 0.6) < 0.4;
                let glyph = if n.is_control_plane() { "◆" } else { "●" };
                ctx.print(x, y, Span::styled(glyph, Style::new().fg(if blink { fx::fade(rgb, 0.3) } else { fx::c(rgb) })));
                if let Some((lx, ly)) = label_xy[i] {
                    let style = if i == selected {
                        Style::new().fg(fx::c(fx::MAGENTA)).add_modifier(Modifier::BOLD)
                    } else if n.health() == Health::Bad {
                        Style::new().fg(fx::c(fx::RED))
                    } else {
                        Style::new().fg(fx::c(fx::STEEL))
                    };
                    ctx.print(lx, ly, Line::from(Span::styled(n.name.clone(), style)));
                }
            }
        });
    f.render_widget(canvas, area);
}
