use crate::app::{App, SessionSortColumn};
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

pub(crate) fn draw_config_overlay(f: &mut Frame, app: &App, theme: &Theme) {
    let area = f.area();

    let popup_w = 58u16.min(area.width.saturating_sub(4));
    let popup_h = 28u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_w)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let config_title = t("config.title");
    let block = Block::default()
        .style(Style::default().bg(theme.main_bg))
        .title(
            Line::from(vec![Span::styled(
                config_title.clone(),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
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

    let theme_label = t("config.theme");
    let on_str = t("config.on");
    let off_str = t("config.off");
    let mut items: Vec<(String, String)> = vec![
        (theme_label, app.theme.name.to_string()),
        (
            t("config.context_panel"),
            toggle_str(&on_str, &off_str, app.show_context),
        ),
        (
            t("config.quota_panel"),
            toggle_str(&on_str, &off_str, app.show_quota),
        ),
        (
            t("config.tokens_panel"),
            toggle_str(&on_str, &off_str, app.show_tokens),
        ),
        (
            t("config.projects_panel"),
            toggle_str(&on_str, &off_str, app.show_projects),
        ),
        (
            t("config.ports_panel"),
            toggle_str(&on_str, &off_str, app.show_ports),
        ),
        (
            t("config.sessions_panel"),
            toggle_str(&on_str, &off_str, app.show_sessions),
        ),
        (
            t("config.session_details"),
            toggle_str(&on_str, &off_str, app.show_session_details),
        ),
        (
            t("config.mcp_panel"),
            toggle_str(&on_str, &off_str, app.show_mcp),
        ),
    ];
    for &column in &SessionSortColumn::ALL {
        items.push((
            format!("{} {}", t("config.column"), column.label()),
            toggle_str(&on_str, &off_str, app.session_column_enabled(column)),
        ));
    }

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    let footer_lines = 2usize;
    let visible_count = (inner.height as usize)
        .saturating_sub(footer_lines + 1)
        .max(1);
    let max_start = items.len().saturating_sub(visible_count);
    let start = app
        .config_selected
        .saturating_sub(visible_count / 2)
        .min(max_start);
    let end = (start + visible_count).min(items.len());

    for (i, (label, value)) in items.iter().enumerate().take(end).skip(start) {
        let selected = i == app.config_selected;
        let cursor = if selected { ">" } else { " " };

        let label_style = if selected {
            Style::default()
                .fg(theme.selected_fg)
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.main_fg)
        };

        let value_style = if selected {
            Style::default().fg(theme.selected_fg).bg(theme.selected_bg)
        } else if value == "on" {
            Style::default().fg(theme.proc_misc)
        } else if value == "off" {
            Style::default().fg(theme.inactive_fg)
        } else {
            Style::default().fg(theme.session_id)
        };

        let label_w = 30;
        let padded_label = format!("{} {:<width$}", cursor, label, width = label_w);
        let padded_value = format!("{:<10}", value);

        lines.push(Line::from(vec![
            Span::styled(padded_label, label_style),
            Span::styled(padded_value, value_style),
        ]));
    }

    lines.push(Line::from(""));
    let change_label = t("config.change");
    let close_label = t("config.close");
    lines.push(Line::from(Span::styled(
        format!(
            " abtop v{}  {}  Esc {}  {}/{}",
            env!("CARGO_PKG_VERSION"),
            change_label,
            close_label,
            app.config_selected + 1,
            items.len()
        ),
        Style::default().fg(theme.graph_text),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn toggle_str(on_str: &str, off_str: &str, v: bool) -> String {
    if v {
        on_str.to_string()
    } else {
        off_str.to_string()
    }
}
