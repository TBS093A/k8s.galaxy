//! Radar view: rotating sweep with phosphor trail. Healthy nodes sit on the rim,
//! problems close in on the center; blips light up as the beam passes and slowly decay.

use std::f64::consts::TAU;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::Span;
use ratatui::widgets::canvas::{Canvas, Circle, Line as CLine};

use super::labels::Placer;
use super::{frame, square_bounds};
use crate::app::App;
use crate::fx::{self, Rgb, hash, unit};
use crate::model::Health;

struct Blip {
    label: String,
    angle: f64,
    radius: f64, // 0..1 of radar radius
    rgb: Rgb,
    threat: bool,
    selected: bool,
}

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.t();
    let (w, h) = square_bounds(area);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let big_r = cx.min(cy) * 0.92;
    let sweep = (t * 1.1).rem_euclid(TAU);

    let mut blips: Vec<Blip> = vec![];
    for (i, n) in app.snap.nodes.iter().enumerate() {
        let hv = hash(&n.name);
        let bad = n.health() == Health::Bad;
        blips.push(Blip {
            label: n.name.clone(),
            angle: unit(hv) * TAU,
            radius: if bad { 0.25 + 0.2 * unit(hv >> 7) } else { 0.7 + 0.25 * unit(hv >> 7) },
            rgb: if bad { fx::RED } else { fx::GREEN },
            threat: bad,
            selected: i == app.node_idx,
        });
    }
    let highlighted = app.highlighted_pods();
    for p in app.snap.pods.iter().filter(|p| p.health() != Health::Ok) {
        let hv = hash(&p.name);
        let bad = p.health() == Health::Bad;
        blips.push(Blip {
            label: format!("{} {}", p.name, p.status()),
            angle: unit(hv) * TAU,
            radius: if bad { 0.08 + 0.3 * unit(hv >> 9) } else { 0.4 + 0.25 * unit(hv >> 9) },
            rgb: if bad { fx::RED } else { fx::AMBER },
            threat: bad,
            selected: highlighted.contains(&(p.namespace.as_str(), p.name.as_str())),
        });
    }

    // Blips claim their cells first so no label covers another contact; then labels by priority.
    let xy = |b: &Blip| (cx + big_r * b.radius * b.angle.cos(), cy + big_r * b.radius * b.angle.sin());
    let mut placer = Placer::new(area, [0.0, w], [0.0, h]);
    for b in &blips {
        let (x, y) = xy(b);
        placer.block(x, y);
    }
    let mut order: Vec<usize> = (0..blips.len()).filter(|&i| blips[i].threat || blips[i].selected).collect();
    order.sort_by_key(|&i| (!blips[i].selected, blips[i].label.clone()));
    let mut labels: Vec<(usize, (f64, f64))> = vec![];
    for i in order {
        let (x, y) = xy(&blips[i]);
        if let Some(at) = placer.label(x, y, blips[i].label.chars().count()) {
            labels.push((i, at));
        }
    }

    let threats = blips.iter().filter(|b| b.threat).count();
    let title = format!("RADAR  {} contacts  {} hostile", blips.len(), threats);

    let canvas = Canvas::default()
        .block(frame(&title, fx::GREEN, true))
        .background_color(fx::c(fx::BG))
        .marker(Marker::Braille)
        .x_bounds([0.0, w])
        .y_bounds([0.0, h])
        .paint(|ctx| {
            let grid = fx::fade(fx::GREEN, 0.18);
            for k in 1..=4 {
                ctx.draw(&Circle { x: cx, y: cy, radius: big_r * k as f64 / 4.0, color: grid });
            }
            for k in 0..12 {
                let a = k as f64 / 12.0 * TAU;
                let inner = if k % 3 == 0 { 0.0 } else { big_r * 0.75 };
                ctx.draw(&CLine::new(cx + inner * a.cos(), cy + inner * a.sin(), cx + big_r * a.cos(), cy + big_r * a.sin(), grid));
            }
            ctx.layer();

            // Phosphor trail behind the beam.
            let trail = 28;
            for k in (0..trail).rev() {
                let a = sweep - k as f64 * 0.035;
                let fade = (1.0 - k as f64 / trail as f64).powi(2);
                ctx.draw(&CLine::new(cx, cy, cx + big_r * a.cos(), cy + big_r * a.sin(), fx::fade(fx::GREEN, fade * 0.9)));
            }
            ctx.layer();

            for b in &blips {
                // Brightest right after the beam passes, decaying over a full turn.
                let since = (sweep - b.angle).rem_euclid(TAU) / TAU;
                let glow = (1.0 - since).powi(3).max(if b.threat { 0.25 } else { 0.08 });
                let (x, y) = (cx + big_r * b.radius * b.angle.cos(), cy + big_r * b.radius * b.angle.sin());
                ctx.draw(&Circle { x, y, radius: 0.6, color: fx::fade(b.rgb, glow) });
                if since < 0.08 {
                    ctx.draw(&Circle { x, y, radius: 1.5 + since * 25.0, color: fx::fade(b.rgb, (1.0 - since * 12.0) * 0.8) });
                }
                if b.selected {
                    ctx.draw(&Circle { x, y, radius: 2.5, color: fx::c(fx::MAGENTA) });
                }
            }

            for &(i, (lx, ly)) in &labels {
                let b = &blips[i];
                let since = (sweep - b.angle).rem_euclid(TAU) / TAU;
                let rgb = if b.selected { fx::MAGENTA } else { b.rgb };
                let style = Style::new().fg(fx::fade(rgb, (1.0 - since).max(0.35))).add_modifier(Modifier::BOLD);
                ctx.print(lx, ly, Span::styled(b.label.clone(), style));
            }
        });
    f.render_widget(canvas, area);
}
