use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

/// Items rendered in the `v` overlay. The `key` char is the hotkey accepted
/// while the overlay is open; `accessor` returns the current on/off state for
/// rendering the indicator.
pub(crate) struct ViewItem {
    pub key: char,
    pub label: &'static str,
    pub state: ViewState,
}

pub(crate) enum ViewState {
    On,
    Off,
    Action,
}

pub(crate) fn items(app: &App) -> Vec<ViewItem> {
    use ViewState::*;
    let bool_state = |b: bool| if b { On } else { Off };
    vec![
        ViewItem { key: 'T', label: "tree view",       state: bool_state(app.tree_view) },
        ViewItem { key: 'l', label: "timeline",        state: bool_state(app.show_timeline) },
        ViewItem { key: 'f', label: "file audit",      state: bool_state(app.show_file_audit) },
        ViewItem { key: '1', label: "context panel",   state: bool_state(app.show_context) },
        ViewItem { key: '2', label: "quota panel",     state: bool_state(app.show_quota) },
        ViewItem { key: '3', label: "tokens panel",    state: bool_state(app.show_tokens) },
        ViewItem { key: '4', label: "projects panel",  state: bool_state(app.show_projects) },
        ViewItem { key: '5', label: "ports panel",     state: bool_state(app.show_ports) },
        ViewItem { key: '6', label: "sessions panel",  state: bool_state(app.show_sessions) },
        ViewItem { key: '7', label: "mcp servers panel", state: bool_state(app.show_mcp) },
        ViewItem { key: 'M', label: "mcp session hide", state: bool_state(app.mcp_suppress_sessions) },
        ViewItem { key: 't', label: "cycle theme",     state: Action },
    ]
}

pub(crate) fn draw_view_overlay(f: &mut Frame, app: &App, theme: &Theme) {
    let area = f.area();
    let entries = items(app);
    let popup_w = 44u16.min(area.width.saturating_sub(4));
    let popup_h = (entries.len() as u16 + 4).min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_w)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .style(Style::default().bg(theme.main_bg))
        .title(
            Line::from(vec![Span::styled(
                " View ",
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            )])
            .alignment(Alignment::Center),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.cpu_box));
    f.render_widget(block, popup);

    let inner = Rect::new(
        popup.x + 2,
        popup.y + 1,
        popup.width.saturating_sub(4),
        popup.height.saturating_sub(2),
    );

    let mut lines: Vec<Line> = Vec::with_capacity(entries.len() + 2);
    for item in &entries {
        let state_str = match item.state {
            ViewState::On => "on",
            ViewState::Off => "off",
            ViewState::Action => "→",
        };
        let state_style = match item.state {
            ViewState::On => Style::default().fg(theme.proc_misc),
            ViewState::Off => Style::default().fg(theme.inactive_fg),
            ViewState::Action => Style::default().fg(theme.session_id),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {}  ", item.key), Style::default().fg(theme.hi_fg).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<22}", item.label), Style::default().fg(theme.main_fg)),
            Span::styled(state_str.to_string(), state_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " key = toggle  ·  Esc = close ",
        Style::default().fg(theme.graph_text),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}
