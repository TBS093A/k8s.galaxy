pub mod galaxy;
mod labels;
mod map;
pub mod ops;
mod panels;
mod radar;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::app::{App, View};
use crate::fx::{self, Rgb};
use crate::model::Health;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(fx::c(fx::BG))), area);

    let [header, body, ticker, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(5), Constraint::Length(1)])
            .areas(area);
    let [main, side] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(46)]).areas(body);

    panels::header(f, header, app);
    match app.view {
        View::Map => map::draw(f, main, app),
        View::Galaxy => galaxy::draw(f, main, app),
        View::Radar => radar::draw(f, main, app),
        View::Rollouts | View::Network | View::Access | View::Argo => ops::draw(f, body, app),
    }
    if !app.view.is_ops() {
        panels::sidebar(f, side, app);
    }
    panels::ticker(f, ticker, app);
    panels::footer(f, footer, app);
    if app.logs.is_some() {
        panels::logs(f, area, app);
    }
}

pub fn health_rgb(h: Health) -> Rgb {
    match h {
        Health::Ok => fx::GREEN,
        Health::Warn => fx::AMBER,
        Health::Bad => fx::RED,
    }
}

/// Neon frame used by every panel.
pub fn frame(title: &str, accent: Rgb, focused: bool) -> Block<'static> {
    let border = if focused { fx::c(accent) } else { fx::fade(accent, 0.45) };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Line::from(vec![
            Span::styled(" ◆ ", Style::new().fg(fx::c(accent))),
            Span::styled(title.to_string(), Style::new().fg(fx::c(accent)).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
}

/// Canvas units such that one x unit is one column and one y unit is half a row —
/// terminal cells are ~2x taller than wide, so circles drawn in these units come out round.
pub fn square_bounds(area: Rect) -> (f64, f64) {
    (area.width.saturating_sub(2).max(1) as f64, area.height.saturating_sub(2).max(1) as f64 * 2.0)
}
