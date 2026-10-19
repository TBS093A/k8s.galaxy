//! Galaxy view: every node is a star system, its pods orbit as planets,
//! one orbit per namespace. Failing pods flare red.

use std::collections::BTreeMap;
use std::f64::consts::TAU;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::Span;
use ratatui::widgets::canvas::{Canvas, Circle, Points};

use super::{frame, health_rgb, square_bounds};
use crate::app::App;
use crate::fx::{self, Rng, hash, pulse, unit};
use crate::model::Health;

const MAX_RINGS: usize = 5;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.t();
    let (w, h) = square_bounds(area);
    let nodes = &app.snap.nodes;
    let n = nodes.len().max(1);

    // Grid of systems shaped to the viewport.
    let cols = ((n as f64 * w / h).sqrt().ceil() as usize).clamp(1, n);
    let rows = n.div_ceil(cols);
    let (cw, chh) = (w / cols as f64, h / rows as f64);
    let max_r = (cw.min(chh) / 2.0) * 0.82;
    let sun_r = (max_r * 0.12).max(1.0);

    let highlighted = app.highlighted_pods();
    let unscheduled = app.snap.pods.iter().filter(|p| p.node.is_none()).count();
    let title = if unscheduled > 0 { format!("GALAXY  {unscheduled} drifting (unscheduled)") } else { "GALAXY".into() };

    let canvas = Canvas::default()
        .block(frame(&title, fx::MAGENTA, true))
        .background_color(fx::c(fx::BG))
        .marker(Marker::Braille)
        .x_bounds([0.0, w])
        .y_bounds([0.0, h])
        .paint(|ctx| {
            // Twinkling starfield, bucketed by brightness to keep draw calls low.
            let mut rng = Rng::new(0x57a2_c0de);
            let mut buckets: [Vec<(f64, f64)>; 4] = Default::default();
            for _ in 0..(w * h / 18.0) as usize {
                let (x, y, phase) = (rng.f64() * w, rng.f64() * h, rng.f64() * 5.0);
                let b = (pulse(t + phase, 3.0 + phase) * 3.99) as usize;
                buckets[b].push((x, y));
            }
            for (b, pts) in buckets.iter().enumerate() {
                ctx.draw(&Points { coords: pts, color: fx::fade(fx::STEEL, 0.15 + b as f64 * 0.15) });
            }
            ctx.layer();

            for (i, node) in nodes.iter().enumerate() {
                let (cx, cy) = (cw * ((i % cols) as f64 + 0.5), h - chh * ((i / cols) as f64 + 0.5));
                let pods = app.snap.pods_on(&node.name);
                let mut by_ns: BTreeMap<&str, Vec<_>> = BTreeMap::new();
                for p in &pods {
                    by_ns.entry(p.namespace.as_str()).or_default().push(*p);
                }
                let rings: Vec<_> = by_ns.into_iter().collect();
                let ring_count = rings.len().clamp(1, MAX_RINGS);
                let ring_r = |k: usize| sun_r * 2.2 + (k as f64 + 1.0) * (max_r - sun_r * 2.2) / ring_count as f64;

                // Orbits.
                for k in 0..ring_count {
                    ctx.draw(&Circle { x: cx, y: cy, radius: ring_r(k), color: fx::fade(fx::CYAN, 0.12) });
                }
                // Star with glow falloff.
                let star = if node.ready { if node.is_control_plane() { fx::MAGENTA } else { fx::AMBER } } else { fx::RED };
                let flicker = if node.ready { 0.85 + 0.15 * pulse(t, 2.0) } else { pulse(t, 0.5) };
                for g in (0..4).rev() {
                    let r = sun_r * (0.4 + g as f64 * 0.45);
                    ctx.draw(&Circle { x: cx, y: cy, radius: r, color: fx::fade(star, flicker * (1.0 - g as f64 * 0.22)) });
                }
                // Load gauges: CPU (inner) and memory (outer) arcs, clockwise from the top.
                if let Some(u) = app.node_usage(&node.name) {
                    let a = node.allocatable;
                    for (k, frac) in [(0.0, u.cpu_m as f64 / a.cpu_m.max(1) as f64), (1.0, u.mem as f64 / a.mem.max(1) as f64)] {
                        let r = sun_r * (1.9 + k * 0.5);
                        let rgb = load_rgb(frac);
                        let n = 40;
                        let lit = (frac.clamp(0.0, 1.0) * n as f64).round() as usize;
                        let ring: Vec<_> = (0..n)
                            .map(|j| {
                                let ang = std::f64::consts::FRAC_PI_2 - j as f64 / n as f64 * TAU;
                                (cx + r * ang.cos(), cy + r * ang.sin())
                            })
                            .collect();
                        ctx.draw(&Points { coords: &ring[lit..], color: fx::fade(fx::STEEL, 0.18) });
                        ctx.draw(&Points { coords: &ring[..lit], color: fx::fade(rgb, if frac > 0.9 { 0.6 + 0.4 * pulse(t, 0.8) } else { 0.9 }) });
                    }
                }
                // Selection: rotating dashed halo.
                if i == app.node_idx {
                    let halo = max_r + 1.5;
                    let pts: Vec<_> = (0..48)
                        .filter(|k| k % 3 != 0)
                        .map(|k| {
                            let a = k as f64 / 48.0 * TAU + t * 0.4;
                            (cx + halo * a.cos(), cy + halo * a.sin())
                        })
                        .collect();
                    ctx.draw(&Points { coords: &pts, color: fx::c(fx::MAGENTA) });
                }

                // Planets.
                for (k, (_, ns_pods)) in rings.iter().enumerate() {
                    let ring = k.min(MAX_RINGS - 1);
                    let r = ring_r(ring);
                    let omega = 0.5 / (ring as f64 + 1.0).sqrt();
                    let offset = unit(hash(&node.name)) * TAU + k as f64;
                    for (j, p) in ns_pods.iter().enumerate() {
                        let a = offset + j as f64 / ns_pods.len() as f64 * TAU + t * omega;
                        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
                        let health = p.health();
                        let rgb = health_rgb(health);
                        // Planet size: memory in use when metrics exist, else restarts as a hint of trouble.
                        let size = match app.pod_usage(p) {
                            Some(u) => (0.35 + (u.mem as f64 / (256.0 * 1024.0 * 1024.0)).sqrt() * 0.3).min(1.6),
                            None => 0.5 + (p.restarts.min(10) as f64) * 0.05,
                        };
                        match health {
                            Health::Bad => {
                                let flare = pulse(t, 0.7);
                                ctx.draw(&Circle { x, y, radius: size + 0.6 + flare, color: fx::fade(fx::RED, 0.6 * (1.0 - flare)) });
                                ctx.draw(&Circle { x, y, radius: size, color: fx::c(fx::RED) });
                            }
                            _ => {
                                ctx.draw(&Circle { x, y, radius: size, color: fx::fade(rgb, 0.85) });
                            }
                        }
                        if highlighted.contains(&(p.namespace.as_str(), p.name.as_str())) {
                            ctx.draw(&Circle { x, y, radius: size + 1.2 + pulse(t, 0.8), color: fx::c(fx::MAGENTA) });
                        }
                    }
                }
            }

            // Labels last so they sit above the art.
            for (i, node) in nodes.iter().enumerate() {
                let (cx, cy) = (cw * ((i % cols) as f64 + 0.5), h - chh * ((i / cols) as f64 + 0.5));
                let mut label = format!("{} ·{}", node.name, app.snap.pods_on(&node.name).len());
                if let Some(u) = app.node_usage(&node.name) {
                    let a = node.allocatable;
                    label.push_str(&format!("  cpu {:.0}% mem {:.0}%", pct(u.cpu_m, a.cpu_m), pct(u.mem, a.mem)));
                }
                let style = if i == app.node_idx {
                    Style::new().fg(fx::c(fx::MAGENTA)).add_modifier(Modifier::BOLD)
                } else if node.ready {
                    Style::new().fg(fx::c(fx::STEEL))
                } else {
                    Style::new().fg(fx::c(fx::RED)).add_modifier(Modifier::BOLD)
                };
                let y = (cy - max_r - 2.0).max(0.0);
                ctx.print(cx - label.chars().count() as f64 / 2.0, y, Span::styled(label, style));
            }
        });
    f.render_widget(canvas, area);
}

fn pct(used: i64, total: i64) -> f64 {
    used as f64 * 100.0 / total.max(1) as f64
}

/// Green → amber → red as a node fills up.
pub fn load_rgb(frac: f64) -> fx::Rgb {
    if frac < 0.6 {
        fx::GREEN
    } else if frac < 0.85 {
        fx::AMBER
    } else {
        fx::RED
    }
}
