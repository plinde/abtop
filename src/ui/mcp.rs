use crate::app::App;
use crate::collector::mcp::ACTIVE_MTIME_SECS;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::time::SystemTime;

use super::{btop_block, grad_at, make_gradient};

pub(crate) fn draw_mcp_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let header_style = Style::default()
        .fg(theme.main_fg)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::from(vec![
        Span::styled(" PARENT  ", header_style),
        Span::styled("PROFILE      ", header_style),
        Span::styled("ACT/TOT ", header_style),
        Span::styled("LAST", header_style),
    ])];

    if app.mcp_servers.is_empty() {
        lines.push(Line::from(Span::styled(
            " no mcp servers",
            Style::default().fg(theme.inactive_fg),
        )));
        let block = btop_block("mcp servers", "⁷", theme.net_box, theme);
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    let proc_grad = make_gradient(theme.proc_grad.start, theme.proc_grad.mid, theme.proc_grad.end);
    let now = SystemTime::now();

    for server in &app.mcp_servers {
        let active = server.active_count(now, ACTIVE_MTIME_SECS);
        let total = server.rollouts.len();
        let last_age = server.latest_mtime().and_then(|m| now.duration_since(m).ok());

        let active_color = if active > 0 {
            grad_at(&proc_grad, 100.0)
        } else if total > 0 {
            theme.proc_misc
        } else {
            theme.inactive_fg
        };
        let count_text = format!("{:>3}/{:<3}", active, total);

        let last_text = match last_age {
            Some(d) => fmt_age(d.as_secs()),
            None => "—".to_string(),
        };

        let parent_label = format!(" {:<7}", server.parent_cli);
        let profile_label = match &server.profile {
            Some(p) => super::truncate_str(p, 12),
            None => "default".to_string(),
        };
        let profile_padded = format!("{:<13}", profile_label);

        lines.push(Line::from(vec![
            Span::styled(parent_label, Style::default().fg(theme.main_fg)),
            Span::styled(profile_padded, Style::default().fg(theme.session_id)),
            Span::styled(format!("{} ", count_text), Style::default().fg(active_color)),
            Span::styled(last_text, Style::default().fg(theme.inactive_fg)),
        ]));
    }

    if !app.mcp_suppress_sessions {
        lines.push(Line::from(Span::styled(
            " suppress: off (M)",
            Style::default().fg(theme.inactive_fg),
        )));
    }

    let block = btop_block("mcp servers", "⁷", theme.net_box, theme);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Format a duration into a compact "Xs / Xm / Xh" label.
fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(5), "5s ago");
        assert_eq!(fmt_age(125), "2m ago");
        assert_eq!(fmt_age(7_200), "2h ago");
        assert_eq!(fmt_age(172_800), "2d ago");
    }
}
