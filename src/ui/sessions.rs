use crate::app::{App, SessionSortColumn};
use crate::locale::t;
use crate::model::{AgentSession, ChatRole, FileOp};
use crate::theme::Theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use super::{btop_block_active, fmt_mem_kb, fmt_tokens, grad_at, make_gradient, truncate_str};

pub(crate) fn draw_sessions_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    // Render the outer block
    let block = btop_block_active("sessions", "⁶", theme.proc_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    let visible = app.visible_indices();
    let session_rows: u16 = visible
        .iter()
        .map(|&i| {
            let base = 2u16;
            if app.tree_view {
                base + app.sessions[i].subagents.len() as u16
            } else {
                base
            }
        })
        .sum();
    let detail_reserve: u16 = if app.show_timeline {
        (inner.height * 2 / 3).min(inner.height.saturating_sub(5))
    } else if inner.height <= 12 {
        6.min(inner.height.saturating_sub(3))
    } else {
        10.min(inner.height.saturating_sub(3))
    };
    let max_table = inner.height.saturating_sub(detail_reserve);
    let table_h = (1 + session_rows).min(max_table);

    let panel_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(table_h),
            Constraint::Length(1), // separator line
            Constraint::Min(0),
        ])
        .split(inner);

    // Draw separator line between session list and detail
    {
        let sep_area = panel_chunks[1];
        let sep_line = "─".repeat(sep_area.width as usize);
        f.render_widget(
            Paragraph::new(Span::styled(sep_line, Style::default().fg(theme.proc_box))),
            sep_area,
        );
    }

    // ── Session list table ──
    let proc_grad = make_gradient(
        theme.proc_grad.start,
        theme.proc_grad.mid,
        theme.proc_grad.end,
    );
    let mut rows = Vec::new();

    let w = inner.width;
    let columns = visible_session_columns(app, w);

    let visible = app.visible_indices();
    for &i in &visible {
        let session = &app.sessions[i];
        let selected = i == app.selected;
        let marker = if selected { "►" } else { " " };

        let is_done = matches!(session.status, crate::model::SessionStatus::Done);
        let row_style = if selected {
            Style::default()
                .bg(theme.selected_bg)
                .fg(theme.selected_fg)
                .add_modifier(Modifier::BOLD)
        } else if is_done {
            Style::default().fg(theme.inactive_fg)
        } else {
            Style::default()
        };

        let mut cells = vec![Cell::from(Span::styled(
            marker,
            Style::default().fg(theme.hi_fg),
        ))];
        for &column in &columns {
            cells.push(session_column_cell(
                app, session, column, w, theme, &proc_grad,
            ));
        }

        rows.push(Row::new(cells).style(row_style).height(1));

        // 2nd line: task text in Summary column
        let summary_idx = columns
            .iter()
            .position(|&column| column == SessionSortColumn::Summary)
            .unwrap_or(0)
            + 1;
        let total_cols = 1 + columns.len();
        let task_cells: Vec<Cell> = (0..total_cols)
            .map(|j| {
                if j == summary_idx {
                    let task_text = session
                        .current_tasks
                        .last()
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    Cell::from(Span::styled(
                        format!("└─ {}", task_text),
                        Style::default().fg(theme.graph_text),
                    ))
                } else {
                    Cell::from("")
                }
            })
            .collect();
        rows.push(Row::new(task_cells).height(1));

        // Tree view: show subagents as indented rows
        if app.tree_view && !session.subagents.is_empty() {
            for (sa_idx, sa) in session.subagents.iter().enumerate() {
                let is_last = sa_idx == session.subagents.len() - 1;
                // Tree connector fits the 3-wide agent column (was truncated before).
                let prefix = if is_last { "└─" } else { "├─" };
                let is_working = sa.status.eq_ignore_ascii_case("working")
                    || sa.status.eq_ignore_ascii_case("in_progress");
                let icon = if is_working { "●" } else { "✓" };
                let sa_fg = if is_working {
                    theme.proc_misc
                } else {
                    theme.inactive_fg
                };

                let mut sa_cells: Vec<Cell> = vec![Cell::from("")];
                for &column in &columns {
                    sa_cells.push(subagent_column_cell(
                        column,
                        prefix,
                        &sa.name,
                        icon,
                        sa_fg,
                        sa.tokens,
                        theme,
                    ));
                }
                rows.push(Row::new(sa_cells).height(1));
            }
        }
    }

    let header_style = Style::default()
        .fg(theme.main_fg)
        .add_modifier(Modifier::BOLD);
    let mut header_cells = vec![Cell::from("")];
    for &column in &columns {
        header_cells.push(sortable_header(
            app,
            column,
            &column_label(column, w),
            header_style,
        ));
    }
    let header = Row::new(header_cells).height(1);

    let mut widths_vec: Vec<Constraint> = vec![Constraint::Length(1)]; // marker
    for &column in &columns {
        widths_vec.push(column_width(column, w));
    }

    // Scroll: rows vary per session in tree view; use the built row list as the source of truth.
    let visible_sessions = app.visible_indices();
    let total_rows = rows.len();
    let needs_scroll = total_rows > panel_chunks[0].height.saturating_sub(1) as usize;

    // Split table area into [table | scrollbar(1)] when scrollable
    let table_area;
    let scrollbar_area: Option<Rect>;
    if needs_scroll && panel_chunks[0].width > 2 {
        let hsplit = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(panel_chunks[0]);
        table_area = hsplit[0];
        scrollbar_area = Some(hsplit[1]);
    } else {
        table_area = panel_chunks[0];
        scrollbar_area = None;
    }

    let visible_rows = table_area.height.saturating_sub(1) as usize; // -1 for header
                                                                     // Row offset for the selected session, accounting for subagent rows in tree view
                                                                     // and filter-hidden sessions above it.
    let selected_pos = visible_sessions
        .iter()
        .position(|&i| i == app.selected)
        .unwrap_or(0);
    let selected_row_start: usize = visible_sessions
        .iter()
        .take(selected_pos)
        .map(|&i| {
            let base = 2;
            if app.tree_view {
                base + app.sessions[i].subagents.len()
            } else {
                base
            }
        })
        .sum();
    let selected_session_rows = if app.tree_view {
        2 + app
            .sessions
            .get(app.selected)
            .map_or(0, |s| s.subagents.len())
    } else {
        2
    };
    let selected_row_end = selected_row_start + selected_session_rows;
    let scroll_offset = selected_row_end.saturating_sub(visible_rows);
    let visible = if scroll_offset < rows.len() {
        rows.into_iter().skip(scroll_offset).collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let table = Table::new(visible, widths_vec).header(header);
    f.render_widget(table, table_area);

    // ── Scrollbar column (dedicated 1-char width, btop-style) ──
    if let Some(sb) = scrollbar_area {
        let bar_h = sb.height as usize;
        if bar_h > 0 {
            let thumb_size = ((visible_rows as f64 / total_rows as f64) * bar_h as f64)
                .ceil()
                .max(1.0) as usize;
            let thumb_size = thumb_size.min(bar_h);
            let thumb_pos = if total_rows > visible_rows {
                ((scroll_offset as f64 / (total_rows - visible_rows) as f64)
                    * (bar_h - thumb_size) as f64)
                    .round() as usize
            } else {
                0
            };

            let buf = f.buffer_mut();
            for i in 0..bar_h {
                let y = sb.y + i as u16;
                let (ch, color) = if i >= thumb_pos && i < thumb_pos + thumb_size {
                    ("┃", theme.main_fg)
                } else {
                    ("│", theme.div_line)
                };
                buf[(sb.x, y)].set_symbol(ch).set_fg(color);
            }

            // ↑/↓ arrows at edges when more content exists
            if scroll_offset > 0 {
                buf[(sb.x, sb.y)].set_symbol("↑").set_fg(theme.proc_box);
            }
            if scroll_offset + visible_rows < total_rows {
                buf[(sb.x, sb.y + sb.height - 1)]
                    .set_symbol("↓")
                    .set_fg(theme.proc_box);
            }
        }
    }

    // ── Detail section for selected session (full-width Paragraph, not Table) ──
    if let Some(session) = app.sessions.get(app.selected) {
        let detail_area = panel_chunks[2];
        if detail_area.height < 3 {
            return;
        }

        // Reserve bottom lines for MEM + version
        let footer_h = 3u16;
        let detail_body_h = detail_area.height.saturating_sub(footer_h);
        let detail_body = Rect {
            x: detail_area.x,
            y: detail_area.y,
            width: detail_area.width,
            height: detail_body_h,
        };
        let detail_footer = Rect {
            x: detail_area.x,
            y: detail_area.y + detail_body_h,
            width: detail_area.width,
            height: footer_h.min(detail_area.height),
        };

        let has_children = !session.children.is_empty();
        let has_subagents = !session.subagents.is_empty();
        let has_tool_calls = !session.tool_calls.is_empty();
        let has_chat = !session.chat_messages.is_empty();
        let has_left_detail = has_children || has_subagents;
        let has_file_audit = app.show_file_audit && !session.file_accesses.is_empty();
        // Focus mode: file audit (F) takes priority over timeline (L) when both
        // are toggled on. Only one "full lower" mode is active at a time.
        let file_audit_focused = has_file_audit;
        let timeline_focused = !file_audit_focused && app.show_timeline && has_tool_calls;
        // Default detail is chat when available; otherwise fall back to the
        // compact timeline split. Both modes split when there is useful
        // left-side detail (children/subagents) on a wide enough terminal,
        // and use the whole lower area otherwise.
        const CHAT_SPLIT_MIN_WIDTH: u16 = 120;
        let chat_default = !file_audit_focused && !app.show_timeline && has_chat;
        let chat_side_by_side =
            chat_default && has_left_detail && detail_body.width >= CHAT_SPLIT_MIN_WIDTH;
        let chat_full_width = chat_default && !chat_side_by_side;
        const TIMELINE_SPLIT_MIN_WIDTH: u16 = 120;
        let timeline_default = !file_audit_focused
            && !app.show_timeline
            && !chat_default
            && has_tool_calls
            && detail_body.width >= TIMELINE_SPLIT_MIN_WIDTH;
        let timeline_side_by_side = timeline_default && has_left_detail;
        let timeline_full_width = timeline_default && !has_left_detail;

        // Always show SESSION header (task) at top, then children/subagents/timeline/file_audit below
        let session_header_h: u16 = {
            let mut h = 1u16; // SESSION title
            if !session.initial_prompt.is_empty() {
                h += 1;
            }
            h
        };
        let has_lower = file_audit_focused
            || timeline_focused
            || chat_full_width
            || chat_side_by_side
            || timeline_full_width
            || timeline_side_by_side
            || has_children
            || has_subagents;
        let (header_area, lower_area) = if has_lower {
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(session_header_h), Constraint::Min(1)])
                .split(detail_body);
            (parts[0], Some(parts[1]))
        } else {
            (detail_body, None)
        };

        // SESSION header — always rendered
        {
            let mut lines = Vec::new();
            let sid_short = if session.session_id.len() >= 8 {
                &session.session_id[..8]
            } else {
                &session.session_id
            };
            let session_ref = if header_area.width <= 80 {
                format!("►{} · {}", sid_short, session.project_name)
            } else {
                format!("►{} · {}", session.session_id, session.cwd)
            };
            lines.push(Line::from(Span::styled(
                truncate_str(
                    &format!(" {} ({})", t("detail.session").as_str(), session_ref),
                    header_area.width as usize,
                ),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )));
            if !session.initial_prompt.is_empty() {
                let max_w = (header_area.width as usize).saturating_sub(9);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {} ", t("detail.task").as_str()),
                        Style::default().fg(theme.graph_text),
                    ),
                    Span::styled(
                        truncate_str(&session.initial_prompt, max_w),
                        Style::default().fg(theme.main_fg),
                    ),
                ]));
            }
            f.render_widget(Paragraph::new(lines), header_area);
        }

        // Layout below the session header:
        //   - file audit focus (F): full-width file audit
        //   - timeline focus (L): full-width timeline
        //   - default chat: full-width, or split with children/subagents on wide terminals
        //   - wide terminal with left detail + tool calls: split lower area
        //   - wide terminal with only tool calls: full-width timeline
        //   - otherwise: children/subagents only (or nothing)
        if let Some(lower) = lower_area {
            if file_audit_focused {
                draw_file_audit(f, session, lower, theme);
            } else if timeline_focused || timeline_full_width {
                draw_timeline(f, session, lower, theme, app.timeline_scroll);
            } else if chat_full_width {
                draw_chat_history(f, session, lower, theme);
            } else {
                let (left_area, right_detail_area) = if chat_side_by_side || timeline_side_by_side {
                    let split = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(lower);
                    (split[0], Some(split[1]))
                } else {
                    (lower, None)
                };

                if let Some(detail_area) = right_detail_area {
                    if chat_side_by_side {
                        draw_chat_history(f, session, detail_area, theme);
                    } else {
                        draw_timeline(f, session, detail_area, theme, app.timeline_scroll);
                    }
                }

                if has_children || has_subagents {
                    let body_chunks = if has_children && has_subagents {
                        if left_area.width < 90 {
                            Layout::default()
                                .direction(Direction::Vertical)
                                .constraints([
                                    Constraint::Percentage(50),
                                    Constraint::Percentage(50),
                                ])
                                .split(left_area)
                        } else {
                            Layout::default()
                                .direction(Direction::Horizontal)
                                .constraints([
                                    Constraint::Percentage(45),
                                    Constraint::Percentage(55),
                                ])
                                .split(left_area)
                        }
                    } else {
                        Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([Constraint::Percentage(100)])
                            .split(left_area)
                    };

                    // Children (left side)
                    if has_children {
                        let children_area = body_chunks[0];
                        let mut lines = Vec::new();
                        lines.push(Line::from(Span::styled(
                            format!(" {}", t("detail.children").as_str()),
                            Style::default()
                                .fg(theme.title)
                                .add_modifier(Modifier::BOLD),
                        )));
                        for child in &session.children {
                            let cmd_short = child
                                .command
                                .split_whitespace()
                                .take(3)
                                .collect::<Vec<_>>()
                                .join(" ");
                            let port_str =
                                child.port.map(|p| format!(" :{}", p)).unwrap_or_default();
                            let max_cmd = (children_area.width as usize).saturating_sub(18);
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!(" {:<6}", child.pid),
                                    Style::default().fg(theme.main_fg),
                                ),
                                Span::styled(
                                    truncate_str(&cmd_short, max_cmd),
                                    Style::default().fg(theme.graph_text),
                                ),
                                Span::styled(
                                    format!(" {:>5}", fmt_mem_kb(child.mem_kb)),
                                    Style::default().fg(theme.graph_text),
                                ),
                                Span::styled(port_str, Style::default().fg(theme.proc_misc)),
                            ]));
                        }
                        f.render_widget(Paragraph::new(lines), children_area);
                    }

                    // Subagents (right side, or full width if no children)
                    if has_subagents {
                        let sa_area = if has_children {
                            body_chunks[1]
                        } else {
                            body_chunks[0]
                        };

                        let mut lines = Vec::new();
                        lines.push(Line::from(Span::styled(
                            format!(" {}", t("detail.subagents").as_str()),
                            Style::default()
                                .fg(theme.title)
                                .add_modifier(Modifier::BOLD),
                        )));

                        let col_w = sa_area.width as usize;
                        let use_two_cols = session.subagents.len() > 6 && col_w >= 50;

                        if use_two_cols {
                            let half_w = col_w / 2;
                            let name_w = half_w.saturating_sub(12);
                            let mid = session.subagents.len().div_ceil(2);
                            let left_agents = &session.subagents[..mid];
                            let right_agents = &session.subagents[mid..];

                            for (row_idx, sa) in left_agents.iter().enumerate() {
                                let mut spans = Vec::new();
                                // Left column
                                let icon = if sa.status == "working" { "●" } else { "✓" };
                                let fg = if sa.status == "working" {
                                    theme.main_fg
                                } else {
                                    theme.graph_text
                                };
                                spans.push(Span::styled(
                                    format!(
                                        "  {} {:<w$}",
                                        icon,
                                        truncate_str(&sa.name, name_w),
                                        w = name_w
                                    ),
                                    Style::default().fg(fg),
                                ));
                                spans.push(Span::styled(
                                    format!("{:>6}", fmt_tokens(sa.tokens)),
                                    Style::default().fg(theme.graph_text),
                                ));

                                // Right column
                                if let Some(sa_r) = right_agents.get(row_idx) {
                                    let icon_r = if sa_r.status == "working" {
                                        "●"
                                    } else {
                                        "✓"
                                    };
                                    let fg_r = if sa_r.status == "working" {
                                        theme.main_fg
                                    } else {
                                        theme.graph_text
                                    };
                                    spans.push(Span::styled(
                                        format!(
                                            "  {} {:<w$}",
                                            icon_r,
                                            truncate_str(&sa_r.name, name_w),
                                            w = name_w
                                        ),
                                        Style::default().fg(fg_r),
                                    ));
                                    spans.push(Span::styled(
                                        format!("{:>6}", fmt_tokens(sa_r.tokens)),
                                        Style::default().fg(theme.graph_text),
                                    ));
                                }
                                lines.push(Line::from(spans));
                            }
                        } else {
                            let name_w = col_w.saturating_sub(12);
                            for sa in &session.subagents {
                                let icon = if sa.status == "working" { "●" } else { "✓" };
                                let fg = if sa.status == "working" {
                                    theme.main_fg
                                } else {
                                    theme.graph_text
                                };
                                lines.push(Line::from(vec![
                                    Span::styled(
                                        format!(
                                            "  {} {:<w$}",
                                            icon,
                                            truncate_str(&sa.name, name_w),
                                            w = name_w
                                        ),
                                        Style::default().fg(fg),
                                    ),
                                    Span::styled(
                                        format!("{:>6}", fmt_tokens(sa.tokens)),
                                        Style::default().fg(theme.graph_text),
                                    ),
                                ]));
                            }
                        }
                        f.render_widget(Paragraph::new(lines), sa_area);
                    }
                } // end if has_children || has_subagents
            } // end else (not focused)
        }

        // Footer: MEM + version (full width)
        {
            let cpu_grad =
                make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);
            let mem_color = if session.mem_line_count >= 180 {
                grad_at(&cpu_grad, 100.0)
            } else {
                theme.graph_text
            };
            let mut footer_lines = vec![Line::from("")];
            // MEM line only for Claude Code sessions (Codex has no memory system)
            if session.agent_cli == "claude" {
                footer_lines.push(Line::from(Span::styled(
                    format!(
                        " {} {} · {}/200 · 200 lines",
                        t("detail.mem").as_str(),
                        session.mem_file_count,
                        session.mem_line_count
                    ),
                    Style::default().fg(mem_color),
                )));
            }
            // Context evolution sparkline (if history available)
            if !session.context_history.is_empty() && session.context_window > 0 {
                let normalized: Vec<f64> = session
                    .context_history
                    .iter()
                    .map(|&v| (v as f64 / session.context_window as f64).min(1.0))
                    .collect();
                let spark_w = (detail_footer.width as usize)
                    .saturating_sub(16)
                    .clamp(4, 40);
                let mut ctx_spans = vec![Span::styled(
                    format!(" {} ", t("detail.ctx").as_str()),
                    Style::default().fg(theme.graph_text),
                )];
                ctx_spans.extend(super::braille_sparkline(
                    &normalized,
                    spark_w,
                    &cpu_grad,
                    theme.graph_text,
                ));
                if session.compaction_count > 0 {
                    ctx_spans.push(Span::styled(
                        format!(" C{}", session.compaction_count),
                        Style::default().fg(grad_at(&cpu_grad, 80.0)),
                    ));
                }
                footer_lines.push(Line::from(ctx_spans));
            }
            let effort_part = if session.effort.is_empty() {
                String::new()
            } else {
                format!(" · effort: {}", session.effort)
            };
            footer_lines.push(Line::from(Span::styled(
                format!(
                    " {} · {} · {} turns{}",
                    session.version,
                    session.elapsed_display(),
                    session.turn_count,
                    effort_part,
                ),
                Style::default().fg(theme.inactive_fg),
            )));
            f.render_widget(Paragraph::new(footer_lines), detail_footer);
        }
    }
}

/// Render the recent user/assistant chat tail for the selected session.
fn draw_chat_history(f: &mut Frame, session: &AgentSession, area: Rect, theme: &Theme) {
    if session.chat_messages.is_empty() {
        return;
    }

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            " {} ({})",
            t("detail.chat").as_str(),
            session.chat_messages.len()
        ),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    )));

    let visible_rows = area.height.saturating_sub(1) as usize;
    let start = session.chat_messages.len().saturating_sub(visible_rows);
    let text_w = (area.width as usize).saturating_sub(6);

    for msg in session.chat_messages.iter().skip(start) {
        let (label, color) = match msg.role {
            ChatRole::User => ("U", theme.hi_fg),
            ChatRole::Assistant => ("A", theme.proc_misc),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", label), Style::default().fg(color)),
            Span::styled(
                truncate_str(&msg.text, text_w),
                Style::default().fg(theme.main_fg),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Render the file access audit log in the given area.
fn draw_file_audit(f: &mut Frame, session: &AgentSession, area: Rect, theme: &Theme) {
    use std::collections::HashSet;
    let unique_files: HashSet<&str> = session
        .file_accesses
        .iter()
        .map(|a| a.path.as_str())
        .collect();
    let unique_count = unique_files.len();
    let total_count = session.file_accesses.len();

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            " {} ({} accesses, {} unique files)",
            t("detail.file_audit").as_str(),
            total_count,
            unique_count
        ),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    )));

    let max_rows = area.height.saturating_sub(1) as usize;
    let max_path_w = (area.width as usize).saturating_sub(5);

    // Show most recent entries first (reverse order), limited to available rows
    for access in session.file_accesses.iter().rev().take(max_rows) {
        let (label, color) = match access.operation {
            FileOp::Read => ("R", theme.session_id), // blue
            FileOp::Edit => ("E", theme.proc_misc),  // yellow
            FileOp::Write => ("W", theme.cpu_box),   // cyan
        };
        let max_path = max_path_w.saturating_sub(4); // room for turn index
        let path_display = truncate_str(&access.path, max_path);
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", label), Style::default().fg(color)),
            Span::styled(path_display, Style::default().fg(theme.main_fg)),
            Span::styled(
                format!(" t{}", access.turn_index),
                Style::default().fg(theme.inactive_fg),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn visible_session_columns(app: &App, width: u16) -> Vec<SessionSortColumn> {
    let mut columns: Vec<SessionSortColumn> = app
        .session_columns
        .iter()
        .copied()
        .filter(|&column| column_fits(column, width))
        .collect();
    if columns.is_empty() {
        columns.push(SessionSortColumn::Ai);
    }
    columns
}

pub(crate) fn visible_sort_columns(app: &App, area: Rect) -> Vec<SessionSortColumn> {
    if area.height < 4 {
        return Vec::new();
    }
    visible_session_columns(app, area.width.saturating_sub(2))
}

fn column_fits(column: SessionSortColumn, width: u16) -> bool {
    match column {
        SessionSortColumn::Ai
        | SessionSortColumn::Recent
        | SessionSortColumn::Project
        | SessionSortColumn::Summary
        | SessionSortColumn::Status
        | SessionSortColumn::Context => true,
        SessionSortColumn::Session => width >= 76,
        SessionSortColumn::Tokens => width >= 86,
        SessionSortColumn::Model => width >= 90,
        SessionSortColumn::Config => width >= 100,
        SessionSortColumn::Memory | SessionSortColumn::Turn => width >= 110,
        SessionSortColumn::Pid | SessionSortColumn::Branch => width >= 120,
        SessionSortColumn::Everything => width >= 130,
        SessionSortColumn::Input | SessionSortColumn::Output => width >= 140,
        SessionSortColumn::CacheRead | SessionSortColumn::CacheWrite => width >= 150,
        SessionSortColumn::Version | SessionSortColumn::Effort => width >= 160,
        SessionSortColumn::Cwd => width >= 180,
    }
}

fn column_width(column: SessionSortColumn, table_width: u16) -> Constraint {
    match column {
        SessionSortColumn::Ai => Constraint::Length(3),
        SessionSortColumn::Recent => Constraint::Length(if table_width >= 100 { 7 } else { 5 }),
        SessionSortColumn::Pid => Constraint::Length(6),
        SessionSortColumn::Project => Constraint::Length(if table_width >= 120 {
            14
        } else if table_width >= 80 {
            10
        } else {
            8
        }),
        SessionSortColumn::Session => Constraint::Length(if table_width >= 110 { 9 } else { 5 }),
        SessionSortColumn::Config => Constraint::Length(if table_width >= 110 { 14 } else { 10 }),
        SessionSortColumn::Summary => Constraint::Fill(1),
        SessionSortColumn::Status => Constraint::Length(if table_width >= 100 {
            8
        } else if table_width >= 72 {
            6
        } else {
            3
        }),
        SessionSortColumn::Model => Constraint::Length(if table_width >= 110 { 13 } else { 10 }),
        SessionSortColumn::Context => Constraint::Length(if table_width >= 100 { 7 } else { 4 }),
        SessionSortColumn::Tokens
        | SessionSortColumn::Input
        | SessionSortColumn::Output
        | SessionSortColumn::CacheRead
        | SessionSortColumn::CacheWrite => Constraint::Length(if table_width >= 100 { 7 } else { 5 }),
        SessionSortColumn::Memory => Constraint::Length(8),
        SessionSortColumn::Turn => Constraint::Length(4),
        SessionSortColumn::Everything => Constraint::Length(11),
        SessionSortColumn::Branch => Constraint::Length(12),
        SessionSortColumn::Version => Constraint::Length(8),
        SessionSortColumn::Cwd => Constraint::Length(24),
        SessionSortColumn::Effort => Constraint::Length(7),
    }
}

fn column_label(column: SessionSortColumn, table_width: u16) -> String {
    match column {
        SessionSortColumn::Session if table_width < 110 => t("col.sess"),
        SessionSortColumn::Config if table_width < 110 => t("col.cfg"),
        SessionSortColumn::Context if table_width < 100 => t("col.ctx"),
        SessionSortColumn::Ai => t("col.ai"),
        SessionSortColumn::Recent => t("col.recent"),
        SessionSortColumn::Pid => t("col.pid"),
        SessionSortColumn::Project => t("col.project"),
        SessionSortColumn::Session => t("col.session"),
        SessionSortColumn::Config => t("col.config"),
        SessionSortColumn::Summary => t("col.summary"),
        SessionSortColumn::Status => t("col.status"),
        SessionSortColumn::Model => t("col.model"),
        SessionSortColumn::Context => t("col.context"),
        SessionSortColumn::Tokens => t("col.tokens"),
        SessionSortColumn::Input => t("col.input"),
        SessionSortColumn::Output => t("col.output"),
        SessionSortColumn::CacheRead => t("col.cache_r"),
        SessionSortColumn::CacheWrite => t("col.cache_w"),
        SessionSortColumn::Memory => t("col.memory"),
        SessionSortColumn::Turn => t("col.turn"),
        SessionSortColumn::Everything => t("col.everything"),
        SessionSortColumn::Branch => t("col.branch"),
        SessionSortColumn::Version => t("col.version"),
        SessionSortColumn::Cwd => t("col.cwd"),
        SessionSortColumn::Effort => t("col.effort"),
    }
}

fn session_column_cell<'a>(
    app: &App,
    session: &AgentSession,
    column: SessionSortColumn,
    table_width: u16,
    theme: &Theme,
    proc_grad: &[Color; 101],
) -> Cell<'a> {
    match column {
        SessionSortColumn::Ai => {
            let (label, color) = agent_label(session.agent_cli, theme);
            Cell::from(Span::styled(label, Style::default().fg(color)))
        }
        SessionSortColumn::Recent => Cell::from(Span::styled(
            session.last_turn_display(),
            Style::default().fg(theme.graph_text),
        )),
        SessionSortColumn::Pid => Cell::from(Span::styled(
            format!("{}", session.pid),
            Style::default().fg(theme.inactive_fg),
        )),
        SessionSortColumn::Project => Cell::from(Span::styled(
            truncate_str(
                &session.project_name,
                column_len(column, table_width).unwrap_or(10) as usize,
            ),
            Style::default().fg(theme.title),
        )),
        SessionSortColumn::Session => {
            let sid_short = if session.session_id.len() >= 8 {
                &session.session_id[..8]
            } else {
                &session.session_id
            };
            Cell::from(Span::styled(
                truncate_str(
                    sid_short,
                    column_len(column, table_width).unwrap_or(8) as usize,
                ),
                Style::default().fg(theme.session_id),
            ))
        }
        SessionSortColumn::Config => Cell::from(Span::styled(
            truncate_str(
                &session.config_root,
                column_len(column, table_width).unwrap_or(10) as usize,
            ),
            Style::default().fg(theme.inactive_fg),
        )),
        SessionSortColumn::Summary => Cell::from(Span::styled(
            truncate_str(
                &app.session_summary(session),
                table_width.saturating_sub(24) as usize,
            ),
            Style::default().fg(theme.main_fg),
        )),
        SessionSortColumn::Status => {
            let (label, color) = status_label(&session.status, theme, proc_grad);
            Cell::from(Span::styled(
                truncate_str(
                    &label,
                    column_len(column, table_width).unwrap_or(6) as usize,
                ),
                Style::default().fg(color),
            ))
        }
        SessionSortColumn::Model => {
            let is_1m = session.context_window >= 1_000_000 || session.model.contains("[1m]");
            let model_short = shorten_model(&session.model, is_1m);
            Cell::from(Span::styled(
                truncate_str(
                    &model_short,
                    column_len(column, table_width).unwrap_or(10) as usize,
                ),
                Style::default().fg(if model_short == "-" {
                    theme.inactive_fg
                } else {
                    theme.graph_text
                }),
            ))
        }
        SessionSortColumn::Context => Cell::from(Span::styled(
            format!("{:.0}%", session.context_percent),
            Style::default().fg(grad_at(proc_grad, session.context_percent)),
        )),
        SessionSortColumn::Tokens => token_cell(session.active_tokens(), theme),
        SessionSortColumn::Input => token_cell(session.total_input_tokens, theme),
        SessionSortColumn::Output => token_cell(session.total_output_tokens, theme),
        SessionSortColumn::CacheRead => token_cell(session.total_cache_read, theme),
        SessionSortColumn::CacheWrite => token_cell(session.total_cache_create, theme),
        SessionSortColumn::Memory => Cell::from(Span::styled(
            if session.mem_mb > 0 {
                format!("{}M", session.mem_mb)
            } else {
                "—".into()
            },
            Style::default().fg(theme.graph_text),
        )),
        SessionSortColumn::Turn => Cell::from(Span::styled(
            format!("{}", session.turn_count),
            Style::default().fg(theme.graph_text),
        )),
        SessionSortColumn::Everything => token_cell(session.total_tokens(), theme),
        SessionSortColumn::Branch => Cell::from(Span::styled(
            truncate_str(&session.git_branch, 12),
            Style::default().fg(theme.title),
        )),
        SessionSortColumn::Version => Cell::from(Span::styled(
            truncate_str(&session.version, 8),
            Style::default().fg(theme.inactive_fg),
        )),
        SessionSortColumn::Cwd => Cell::from(Span::styled(
            truncate_str(&session.cwd, 24),
            Style::default().fg(theme.inactive_fg),
        )),
        SessionSortColumn::Effort => Cell::from(Span::styled(
            truncate_str(&session.effort, 7),
            Style::default().fg(theme.graph_text),
        )),
    }
}

fn subagent_column_cell<'a>(
    column: SessionSortColumn,
    prefix: &str,
    name: &str,
    icon: &str,
    status_color: Color,
    tokens: u64,
    theme: &Theme,
) -> Cell<'a> {
    match column {
        SessionSortColumn::Ai => Cell::from(Span::styled(
            prefix.to_string(),
            Style::default().fg(theme.div_line),
        )),
        SessionSortColumn::Recent => Cell::from(""),
        SessionSortColumn::Project => Cell::from(Span::styled(
            truncate_str(name, 12),
            Style::default().fg(theme.graph_text),
        )),
        SessionSortColumn::Status => {
            Cell::from(Span::styled(icon.to_string(), Style::default().fg(status_color)))
        }
        SessionSortColumn::Tokens | SessionSortColumn::Everything => token_cell(tokens, theme),
        _ => Cell::from(""),
    }
}

fn token_cell<'a>(tokens: u64, theme: &Theme) -> Cell<'a> {
    Cell::from(Span::styled(
        fmt_tokens(tokens),
        Style::default().fg(theme.main_fg),
    ))
}

fn agent_label(agent_cli: &str, theme: &Theme) -> (String, Color) {
    match agent_cli {
        "claude" => ("*CC".to_string(), Color::Rgb(217, 119, 87)),
        "codex" => (">CD".to_string(), Color::Rgb(122, 157, 255)),
        "opencode" => ("#OC".to_string(), Color::Rgb(74, 222, 128)),
        other => (
            other.chars().take(3).collect::<String>().to_uppercase(),
            theme.inactive_fg,
        ),
    }
}

fn status_label(
    status: &crate::model::SessionStatus,
    theme: &Theme,
    proc_grad: &[Color; 101],
) -> (String, Color) {
    match status {
        crate::model::SessionStatus::Thinking => (t("sess.think"), theme.proc_misc),
        crate::model::SessionStatus::Executing => (t("sess.exec"), theme.hi_fg),
        crate::model::SessionStatus::Waiting => (t("sess.wait"), grad_at(proc_grad, 50.0)),
        crate::model::SessionStatus::Unknown => (t("sess.unknown"), theme.inactive_fg),
        crate::model::SessionStatus::RateLimited => (t("sess.rate"), theme.status_fg),
        crate::model::SessionStatus::Done => (t("sess.done"), theme.inactive_fg),
    }
}

fn column_len(column: SessionSortColumn, table_width: u16) -> Option<u16> {
    match column_width(column, table_width) {
        Constraint::Length(width) => Some(width),
        _ => None,
    }
}

fn sortable_header<'a>(
    app: &App,
    column: SessionSortColumn,
    label: &str,
    style: Style,
) -> Cell<'a> {
    let text = if let Some(indicator) = app.session_sort_indicator(column) {
        format!("{indicator}{label}")
    } else {
        label.to_string()
    };
    Cell::from(Span::styled(text, style))
}

pub(crate) fn sort_column_at(
    app: &App,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<SessionSortColumn> {
    if area.height < 4 || row != area.y + 1 {
        return None;
    }
    let inner_width = area.width.saturating_sub(2);
    let x = area.x + 1;
    let rel_x = column.checked_sub(x)?;
    if rel_x >= inner_width {
        return None;
    }

    let w = inner_width;
    let columns = visible_session_columns(app, w);
    let fixed_width: u16 = 1
        + columns
            .iter()
            .filter(|&&column| column != SessionSortColumn::Summary)
            .filter_map(|&column| column_len(column, w))
            .sum::<u16>();
    let summary_w = w.saturating_sub(fixed_width).max(1);

    let mut ranges = vec![(1, None)];
    for column in columns {
        let width = if column == SessionSortColumn::Summary {
            summary_w
        } else {
            column_len(column, w).unwrap_or(1)
        };
        ranges.push((width, Some(column)));
    }

    let mut cursor = 0u16;
    for (width, sort_column) in ranges {
        let end = cursor.saturating_add(width);
        if rel_x >= cursor && rel_x < end {
            return sort_column;
        }
        cursor = end;
    }

    None
}

pub(crate) fn shorten_model(model: &str, is_1m: bool) -> String {
    // "claude-opus-4-6" → "opus4.6", "claude-sonnet-4-6" → "sonnet4.6", "claude-haiku-4-5" → "haiku4.5"
    let s = model.strip_prefix("claude-").unwrap_or(model);
    let s = s.trim_end_matches("[1m]");
    // Extract name and version: "opus-4-6" → ("opus", "4.6")
    let base = if let Some(pos) = s.find(|c: char| c.is_ascii_digit()) {
        let name = s[..pos].trim_end_matches('-');
        let ver = s[pos..].replace('-', ".");
        format!("{}{}", name, ver)
    } else {
        s.to_string()
    };
    if is_1m {
        format!("{}[1m]", base)
    } else {
        base
    }
}

/// Tool name → color mapping for timeline bars, using theme palette.
fn tool_color(name: &str, theme: &Theme) -> Color {
    match name {
        "Read" => theme.session_id, // typically a warm accent
        "Edit" => theme.proc_misc,  // green/active color
        "Write" => theme.cpu_box,   // box/border accent
        "Bash" => theme.hi_fg,      // highlight foreground
        "shell" | "exec_command" | "write_stdin" => theme.hi_fg,
        "apply_patch" => theme.proc_misc,
        "update_plan" => theme.title,
        "spawn_agent" | "send_input" | "wait_agent" => theme.title,
        "view_image" => theme.session_id,
        "Grep" => theme.status_fg,  // status accent
        "Glob" => theme.graph_text, // subtle text
        "find" | "list_mcp_resources" | "read_mcp_resource" => theme.status_fg,
        "Agent" => theme.title,       // title/emphasis
        "Skill" => theme.selected_fg, // selected foreground
        _ => theme.inactive_fg,       // fallback
    }
}

fn tool_label(name: &str) -> &str {
    match name {
        "exec_command" | "shell" => "Exec",
        "write_stdin" => "Input",
        "apply_patch" => "Patch",
        "update_plan" => "Plan",
        "spawn_agent" => "Agent",
        "send_input" => "Send",
        "wait_agent" => "Wait",
        "view_image" => "Image",
        "list_mcp_resources" | "read_mcp_resource" => "MCP",
        other => other,
    }
}

fn fmt_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m{:.0}s", ms / 60_000, (ms % 60_000) as f64 / 1000.0)
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}ms", ms)
    }
}

fn draw_timeline(
    f: &mut Frame,
    session: &crate::model::AgentSession,
    area: Rect,
    theme: &Theme,
    scroll: usize,
) {
    let tool_calls = &session.tool_calls;
    let is_thinking = session.thinking_since_ms > 0
        && matches!(
            session.status,
            crate::model::SessionStatus::Thinking
                | crate::model::SessionStatus::Executing
                | crate::model::SessionStatus::Waiting
                | crate::model::SessionStatus::Unknown
        );
    if tool_calls.is_empty() && !is_thinking {
        return;
    }

    // Live duration for any tool still in flight. The collector leaves
    // `duration_ms == 0` on tools whose assistant turn hasn't been closed yet;
    // combined with `pending_since_ms > 0` that means "tool started at
    // pending_since_ms and is still running right now." We compute the elapsed
    // ms on every frame so the bar appears to grow in real time.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let live_duration = |tc: &crate::model::ToolCall| -> u64 {
        if tc.duration_ms > 0 {
            tc.duration_ms
        } else if session.pending_since_ms > 0 {
            now_ms.saturating_sub(session.pending_since_ms)
        } else {
            0
        }
    };
    let is_pending = |tc: &crate::model::ToolCall| -> bool {
        tc.duration_ms == 0 && session.pending_since_ms > 0
    };
    let thinking_duration = if is_thinking {
        now_ms.saturating_sub(session.thinking_since_ms)
    } else {
        0
    };

    let total_duration: u64 = tool_calls.iter().map(live_duration).sum();
    let max_duration = tool_calls
        .iter()
        .map(live_duration)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut lines = Vec::new();

    // Header — note "1 running" / "thinking Xs" if the session is live.
    let pending_count = tool_calls.iter().filter(|tc| is_pending(tc)).count();
    let mut status_notes: Vec<String> = Vec::new();
    if pending_count > 0 {
        status_notes.push(format!("{} running", pending_count));
    }
    if is_thinking {
        status_notes.push(format!("thinking {}", fmt_duration(thinking_duration)));
    }
    let running_note = if status_notes.is_empty() {
        String::new()
    } else {
        format!(", {}", status_notes.join(", "))
    };
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} ({} calls, {}{})",
            t("detail.timeline").as_str(),
            tool_calls.len(),
            fmt_duration(total_duration),
            running_note,
        ),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    )]));

    // Available width for the bar: total width - name(7) - arg(22) - duration(8) - padding(5)
    let bar_width = (area.width as usize).saturating_sub(42).max(5);

    // Render each tool call as a row. Reserve the last visible row for the
    // live Thinking row when the model is between turns.
    let header_rows = 1;
    let thinking_rows = if is_thinking { 1 } else { 0 };
    let visible_rows = (area.height as usize).saturating_sub(header_rows + thinking_rows);
    let start = scroll.min(tool_calls.len().saturating_sub(visible_rows));

    for tc in tool_calls.iter().skip(start).take(visible_rows) {
        let duration = live_duration(tc);
        let pending = is_pending(tc);
        let bar_fill = if max_duration > 0 {
            ((duration as f64 / max_duration as f64) * bar_width as f64).ceil() as usize
        } else {
            0
        };
        let bar_fill = bar_fill.min(bar_width);
        let bar_empty = bar_width - bar_fill;

        let is_longest = duration == max_duration && max_duration > 0 && !pending;
        let star = if is_longest { " *" } else { "" };

        let color = tool_color(&tc.name, theme);
        // Prefix running rows with a pulsing ● so they're obvious at a glance.
        // The pulse is cheap: flip between bright/dim on a 2-tick (~4s) cycle
        // using the same clock we used for the live duration.
        let pulse_bright = pending && (now_ms / 500).is_multiple_of(2);
        let name_prefix = if pending { "●" } else { " " };
        let name_style = if pending {
            Style::default().fg(color).add_modifier(if pulse_bright {
                Modifier::BOLD
            } else {
                Modifier::DIM
            })
        } else {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        };
        let bar_style = if pending {
            Style::default().fg(color).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(color)
        };

        let duration_label = if pending {
            format!(" {:>5}…", fmt_duration(duration))
        } else {
            format!(" {:>6}{}", fmt_duration(duration), star)
        };
        let duration_color = if is_longest {
            theme.proc_misc
        } else if pending {
            color
        } else {
            theme.graph_text
        };

        let name_label = super::truncate_str(tool_label(&tc.name), 6);
        lines.push(Line::from(vec![
            Span::styled(format!("{}{:<6}", name_prefix, name_label), name_style),
            Span::styled(
                format!(" {:<20}", super::truncate_str(&tc.arg, 20)),
                Style::default().fg(theme.graph_text),
            ),
            Span::styled(" ", Style::default()),
            Span::styled("█".repeat(bar_fill), bar_style),
            Span::styled("░".repeat(bar_empty), Style::default().fg(theme.div_line)),
            Span::styled(duration_label, Style::default().fg(duration_color)),
        ]));
    }

    // Virtual "Thinking" row — the model is generating its next turn, which
    // never shows up as a tool_use in the JSONL. Growth scales against the
    // longest tool so short thinks fill gradually; long thinks cap at full.
    if is_thinking {
        let color = theme.title;
        let pulse_bright = (now_ms / 500).is_multiple_of(2);
        let bar_fill = if max_duration > 0 {
            ((thinking_duration as f64 / max_duration as f64) * bar_width as f64).ceil() as usize
        } else {
            bar_width
        };
        let bar_fill = bar_fill.min(bar_width);
        let bar_empty = bar_width - bar_fill;
        let name_style = Style::default().fg(color).add_modifier(if pulse_bright {
            Modifier::BOLD
        } else {
            Modifier::DIM
        });
        let bar_style = Style::default().fg(color).add_modifier(Modifier::DIM);
        lines.push(Line::from(vec![
            Span::styled("●Think ", name_style),
            Span::styled(
                format!(" {:<20}", "generating reply"),
                Style::default().fg(theme.graph_text),
            ),
            Span::styled(" ", Style::default()),
            Span::styled("█".repeat(bar_fill), bar_style),
            Span::styled("░".repeat(bar_empty), Style::default().fg(theme.div_line)),
            Span::styled(
                format!(" {:>5}…", fmt_duration(thinking_duration)),
                Style::default().fg(color),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use crate::model::SessionStatus;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn codex_exec_command_uses_bash_color() {
        let theme = Theme::default();
        assert_eq!(
            tool_color("exec_command", &theme),
            tool_color("Bash", &theme)
        );
    }

    #[test]
    fn codex_tool_labels_fit_timeline_name_column() {
        assert_eq!(tool_label("exec_command"), "Exec");
        assert_eq!(tool_label("update_plan"), "Plan");
        assert!(tool_label("exec_command").len() <= 6);
    }

    #[test]
    fn codex_non_1m_context_window_does_not_show_1m_suffix() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.sessions.push(AgentSession {
            agent_cli: "codex",
            pid: 42,
            session_id: "codex-session".into(),
            cwd: "/tmp/project".into(),
            project_name: "project".into(),
            started_at: 0,
            last_turn_at: 0,
            status: SessionStatus::Waiting,
            model: "gpt-5".into(),
            effort: String::new(),
            context_percent: 58.7,
            total_input_tokens: 1_000,
            total_output_tokens: 500,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 1,
            current_tasks: vec!["waiting for input".into()],
            mem_mb: 0,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: Vec::new(),
            context_history: Vec::new(),
            compaction_count: 0,
            context_window: 258_400,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: "prompt".into(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
            config_root: String::new(),
        });

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw_sessions_panel_active(
                    f,
                    &app,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 120,
                        height: 20,
                    },
                    &app.theme,
                    true,
                )
            })
            .unwrap();
        let text = format!("{}", terminal.backend());

        assert!(
            text.contains("gpt5"),
            "model should render in session row\n{text}"
        );
        assert!(
            !text.contains("[1m]"),
            "non-1M Codex context windows must not be labeled as 1M\n{text}"
        );
    }
}
