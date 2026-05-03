use crate::theme::Theme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

const ENTRIES: &[(&str, &str)] = &[
    ("Navigation", ""),
    ("  ↑↓ / j k", "select session"),
    ("  ↵",        "jump to tmux pane (when in tmux)"),
    ("  /",        "filter sessions"),
    ("  Esc",      "clear filter / close overlay"),
    ("Actions", ""),
    ("  x",        "kill selected session"),
    ("  X",        "kill orphan ports"),
    ("  r",        "force refresh"),
    ("  q",        "quit"),
    ("Views", ""),
    ("  v",        "open view menu"),
    ("  c",        "open config"),
    ("  t / T",    "cycle theme / toggle tree"),
    ("  l",        "toggle timeline"),
    ("  f",        "toggle file audit"),
    ("  1-7",      "toggle panels (context/quota/tokens/projects/ports/sessions/mcp)"),
    ("  M",        "toggle mcp-server suppression in sessions panel"),
    ("Help", ""),
    ("  ?",        "this help"),
];

pub(crate) fn draw_help_overlay(f: &mut Frame, theme: &Theme) {
    let area = f.area();
    let popup_w = 60u16.min(area.width.saturating_sub(4));
    let popup_h = (ENTRIES.len() as u16 + 4).min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_w)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .style(Style::default().bg(theme.main_bg))
        .title(
            Line::from(vec![Span::styled(
                " Keybindings ",
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

    let mut lines: Vec<Line> = Vec::with_capacity(ENTRIES.len() + 2);
    for (key, desc) in ENTRIES {
        if desc.is_empty() {
            lines.push(Line::from(Span::styled(
                (*key).to_string(),
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<14}", key), Style::default().fg(theme.hi_fg)),
                Span::styled((*desc).to_string(), Style::default().fg(theme.main_fg)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Press any key to close ",
        Style::default().fg(theme.graph_text),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}
