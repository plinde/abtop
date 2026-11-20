use crate::app::App;
use crate::locale::t;
use crate::model::RateLimitInfo;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block_active, fmt_tokens, grad_at, make_gradient, remaining_bar, styled_label};

/// Data considered "stale" when its updated_at is older than this many seconds.
const STALE_SECS: u64 = 600;

/// Fixed source order so columns stay stable across runs.
const SOURCES: &[&str] = &["claude", "codex"];

pub(crate) fn draw_quota_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block_active("quota", "²", theme.cpu_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // Bottom summary: total tokens + rate
    let total_tokens: u64 = app.sessions.iter().map(|s| s.total_tokens()).sum();
    let rates = &app.token_rates;
    let ticks_per_min = 30usize;
    let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();

    // Split into side-by-side columns. Without an external quota state file:
    // one per known source (CLAUDE | CODEX), always rendered so the layout
    // stays stable even when a source has no data yet. With state-file records:
    // one column per record (claude, codex, then the rest by label), keeping the
    // worst N that fit and reporting the rest as "+k" on the totals row.
    let content_h = inner.height.saturating_sub(1); // reserve last row for totals
    let columns = if app.rate_limits.iter().any(|r| r.from_state) {
        state_columns(&app.rate_limits)
    } else {
        SOURCES
            .iter()
            .map(|s| QuotaColumn {
                source: s,
                info: app
                    .rate_limits
                    .iter()
                    .find(|r| r.source.eq_ignore_ascii_case(s)),
            })
            .collect()
    };
    let max_cols = (inner.width / MIN_STATE_COL_W).max(1) as usize;
    let (columns, hidden) = fit_columns(columns, max_cols);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let num_sources = columns.len().max(1) as u16;
    let col_w = inner.width / num_sources;
    for (i, col) in columns.iter().enumerate() {
        let col_x = inner.x + (i as u16) * col_w;
        let this_w = if i as u16 == num_sources - 1 {
            inner.width - (i as u16) * col_w
        } else {
            col_w
        };
        let col_area = Rect {
            x: col_x,
            y: inner.y,
            width: this_w,
            height: content_h,
        };

        match col.info {
            Some(info) if info.from_state => {
                draw_state_column(f, col_area, info, &cpu_grad, theme, now)
            }
            rl => draw_source_column(f, col_area, col.source, rl, &cpu_grad, theme),
        }
    }

    // Total tokens summary on last row (full width)
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + content_h,
        width: inner.width,
        height: 1,
    };
    let total_label = t("quota.total");
    f.render_widget(
        Paragraph::new(vec![Line::from(vec![
            Span::styled(
                format!(" {} {}", total_label, fmt_tokens(total_tokens)),
                Style::default().fg(theme.main_fg),
            ),
            Span::styled(
                format!(" {}/min", fmt_tokens(tokens_per_min as u64)),
                Style::default().fg(theme.graph_text),
            ),
            Span::styled(
                if hidden > 0 {
                    format!(" +{}", hidden)
                } else {
                    String::new()
                },
                Style::default().fg(theme.inactive_fg),
            ),
        ])]),
        bottom_area,
    );
}

/// Narrowest column worth rendering for a state-file record.
const MIN_STATE_COL_W: u16 = 12;

pub(crate) struct QuotaColumn<'a> {
    /// Source id used for the legacy "no data" placeholder column.
    pub source: &'a str,
    pub info: Option<&'a RateLimitInfo>,
}

/// Columns for state-file mode: claude, codex (placeholders when absent),
/// then every other record ordered by label.
pub(crate) fn state_columns(rate_limits: &[RateLimitInfo]) -> Vec<QuotaColumn<'_>> {
    let mut cols = Vec::new();
    for src in SOURCES {
        let matching: Vec<&RateLimitInfo> =
            rate_limits.iter().filter(|r| r.source == *src).collect();
        if matching.is_empty() {
            cols.push(QuotaColumn {
                source: src,
                info: None,
            });
        }
        for r in matching {
            cols.push(QuotaColumn {
                source: src,
                info: Some(r),
            });
        }
    }
    let mut others: Vec<&RateLimitInfo> = rate_limits
        .iter()
        .filter(|r| !SOURCES.contains(&r.source.as_str()))
        .collect();
    others.sort_by_key(|r| column_label(r).to_lowercase());
    cols.extend(others.into_iter().map(|r| QuotaColumn {
        source: r.source.as_str(),
        info: Some(r),
    }));
    cols
}

/// Severity used to pick which columns survive overflow: errors first, then
/// highest usage. Placeholders (no data) rank lowest.
fn column_severity(col: &QuotaColumn<'_>) -> f64 {
    match col.info {
        Some(i) if i.error.is_some() => f64::MAX,
        Some(i) => i.worst_pct(),
        None => -1.0,
    }
}

/// Keep at most `max_cols` columns: the worst ones, in their original order.
/// Returns the kept columns and how many were hidden.
pub(crate) fn fit_columns(
    cols: Vec<QuotaColumn<'_>>,
    max_cols: usize,
) -> (Vec<QuotaColumn<'_>>, usize) {
    if cols.len() <= max_cols {
        return (cols, 0);
    }
    let mut idx: Vec<usize> = (0..cols.len()).collect();
    idx.sort_by(|a, b| {
        column_severity(&cols[*b])
            .partial_cmp(&column_severity(&cols[*a]))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });
    let mut keep: Vec<usize> = idx.into_iter().take(max_cols).collect();
    keep.sort_unstable();
    let hidden = cols.len() - keep.len();
    let kept = cols
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .map(|(_, c)| c)
        .collect();
    (kept, hidden)
}

fn column_label(info: &RateLimitInfo) -> String {
    info.label
        .clone()
        .unwrap_or_else(|| info.source.to_uppercase())
}

fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{}{}", secs, t("time.s"))
    } else if secs < 3600 {
        format!("{}{}", secs / 60, t("time.m"))
    } else if secs < 86400 {
        format!("{}{}", secs / 3600, t("time.h"))
    } else {
        format!("{}{}", secs / 86400, t("time.d"))
    }
}

/// One record from the external quota state file: label header (dimmed with
/// age when stale), then an error, a balance, or one bar row per window with
/// the reset countdown under the binding window.
fn draw_state_column(
    f: &mut Frame,
    area: Rect,
    info: &RateLimitInfo,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
    now: u64,
) {
    let bar_w = (area.width as usize).saturating_sub(10).clamp(2, 8);
    let stale = crate::collector::quota_state::is_state_record_stale(info, now);
    let mut header = vec![Span::styled(
        format!(" {}", column_label(info)),
        Style::default()
            .fg(if stale {
                theme.inactive_fg
            } else {
                theme.title
            })
            .add_modifier(Modifier::BOLD),
    )];
    if stale {
        let age = info
            .updated_at
            .map(|ts| fmt_age(now.saturating_sub(ts)))
            .unwrap_or_else(|| "—".to_string());
        header.push(Span::styled(
            format!(" {}", age),
            Style::default().fg(theme.inactive_fg),
        ));
    }
    let mut lines: Vec<Line> = vec![Line::from(header)];
    let max_lines = area.height as usize;
    let max_text = (area.width as usize).saturating_sub(3);

    if let Some(err) = &info.error {
        let text: String = err.chars().take(max_text).collect();
        lines.push(Line::from(Span::styled(
            format!("  ! {}", text),
            Style::default().fg(theme.inactive_fg),
        )));
    }
    if let Some(bal) = &info.balance {
        lines.push(Line::from(Span::styled(
            format!("  {}", bal),
            Style::default().fg(if stale {
                theme.inactive_fg
            } else {
                theme.main_fg
            }),
        )));
    }
    for w in &info.windows {
        if lines.len() >= max_lines {
            break;
        }
        let remaining = (100.0 - w.used_pct).clamp(0.0, 100.0);
        let mut s = vec![styled_label(
            format!(" {}", w.label).as_str(),
            theme.graph_text,
        )];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(if stale {
                theme.inactive_fg
            } else {
                grad_at(cpu_grad, w.used_pct)
            }),
        ));
        lines.push(Line::from(s));
        if w.binding && !stale && lines.len() < max_lines {
            let reset = w.resets_at.map(format_reset_time).unwrap_or_default();
            if !reset.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", reset),
                    Style::default().fg(theme.graph_text),
                )));
            }
        }
    }
    lines.truncate(max_lines.max(1));
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_source_column(
    f: &mut Frame,
    area: Rect,
    source: &str,
    rl: Option<&RateLimitInfo>,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let col_w_usize = area.width as usize;
    let bar_w = col_w_usize.saturating_sub(10).clamp(2, 8);

    let Some(rl) = rl else {
        let hint = if source.eq_ignore_ascii_case("claude") {
            t("quota.abtop_setup")
        } else {
            t("quota.run_codex")
        };
        let no_data = t("quota.no_data");
        let lines = vec![
            Line::from(Span::styled(
                format!(" {}", source.to_uppercase()),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  — {}", no_data),
                Style::default().fg(theme.inactive_fg),
            )),
            Line::from(Span::styled(
                format!(" {}", hint),
                Style::default().fg(theme.graph_text),
            )),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let is_stale = rl
        .updated_at
        .is_some_and(|ts| now.saturating_sub(ts) > STALE_SECS);

    // Stale data → dim the source name. Drops the unlabeled "Xs ago" row
    // we used to render here; keeping a distinct freshness color but no
    // explicit number is enough to signal "values may be out of date"
    // without competing with the reset countdown for the user's attention.
    let source_color = if is_stale {
        theme.inactive_fg
    } else {
        theme.title
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {}", rl.source.to_uppercase()),
        Style::default()
            .fg(source_color)
            .add_modifier(Modifier::BOLD),
    )));

    // Reset countdown is only meaningful when the data is fresh enough
    // that the reported `resets_at` is still in the future. Stale sources
    // get the bar (the % is approximately correct) but no countdown row.
    let show_reset = !is_stale;

    if let Some(used_pct) = rl.five_hour_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let reset = if show_reset {
            rl.five_hour_resets_at
                .map(format_reset_time)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let c = grad_at(cpu_grad, used_pct);
        let label_5h = t("quota.5h");
        let mut s = vec![styled_label(
            format!(" {}", label_5h).as_str(),
            theme.graph_text,
        )];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(c),
        ));
        lines.push(Line::from(s));
        // Always reserve the row so both columns line up vertically;
        // when there's nothing meaningful to show (stale source or the
        // cached reset moment is past), render it blank.
        lines.push(Line::from(Span::styled(
            if reset.is_empty() {
                String::new()
            } else {
                format!("  {}", reset)
            },
            Style::default().fg(theme.graph_text),
        )));
    }
    if let Some(used_pct) = rl.seven_day_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let reset = if show_reset {
            rl.seven_day_resets_at
                .map(format_reset_time)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let c = grad_at(cpu_grad, used_pct);
        let label_7d = t("quota.7d");
        let mut s = vec![styled_label(
            format!(" {}", label_7d).as_str(),
            theme.graph_text,
        )];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(c),
        ));
        lines.push(Line::from(s));
        // Always reserve the row so both columns line up vertically;
        // when there's nothing meaningful to show (stale source or the
        // cached reset moment is past), render it blank.
        lines.push(Line::from(Span::styled(
            if reset.is_empty() {
                String::new()
            } else {
                format!("  {}", reset)
            },
            Style::default().fg(theme.graph_text),
        )));
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Format a reset timestamp as a human countdown labeled "in X" so the
/// row reads as a time-until-reset. Returns an empty string when the
/// reset is already in the past — the actual next reset depends on the
/// window length which the caller doesn't track, and showing a "now"
/// sentinel was misleading on stale sources where the window had
/// already rolled over multiple times. Callers skip the row when this
/// returns empty.
pub(crate) fn format_reset_time(reset_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if reset_ts <= now {
        return String::new();
    }
    let diff = reset_ts - now;
    let prefix = t("quota.in");
    if diff < 60 {
        format!("{} {}{}", prefix, diff, t("time.s"))
    } else if diff < 3600 {
        format!("{} {}{}", prefix, diff / 60, t("time.m"))
    } else if diff < 86400 {
        let h = diff / 3600;
        let m = (diff % 3600) / 60;
        format!("{} {}{} {}{}", prefix, h, t("time.h"), m, t("time.m"))
    } else {
        let d = diff / 86400;
        let h = (diff % 86400) / 3600;
        format!("{} {}{} {}{}", prefix, d, t("time.d"), h, t("time.h"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::quota_state::parse_quota_state;
    use crate::config::PanelVisibility;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                draw_quota_panel_active(f, app, area, &Theme::default(), false)
            })
            .unwrap();
        format!("{}", terminal.backend())
    }

    fn app_with(rate_limits: Vec<RateLimitInfo>) -> App {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.rate_limits = rate_limits;
        app
    }

    fn gauge(source: &str, label: &str, status: &str, fetched: u64, data: &str) -> String {
        format!(
            r#"{{"kind":"gauge","domain":"ai-quota","source":"{source}","label":"{label}","status":"{status}","fetched_at":{fetched},"data":{data}}}"#
        )
    }

    fn state(records: &[String]) -> Vec<RateLimitInfo> {
        parse_quota_state(&format!(
            r#"{{"schema":1,"generated_at":1,"records":[{}]}}"#,
            records.join(",")
        ))
        .unwrap()
    }

    fn win(pct: u32, kind: &str) -> String {
        format!(
            r#"{{"windows":[{{"kind":"{kind}","scope":"all","used_pct":{pct},"resets_at":{}}}]}}"#,
            now() + 7200
        )
    }

    #[test]
    fn legacy_mode_unchanged_shows_claude_and_codex_placeholders() {
        let text = render(&app_with(vec![]), 60, 10);
        assert!(text.contains("CLAUDE") && text.contains("CODEX"), "{text}");
        assert!(text.contains("no data"), "{text}");
    }

    #[test]
    fn state_mode_renders_one_column_per_record_in_order() {
        let n = now();
        let app = app_with(state(&[
            gauge("opencode-go", "OCb", "ok", n, &win(60, "monthly")),
            gauge("codex", "CX", "ok", n, &win(4, "5h")),
            gauge("claude", "CC", "ok", n, &win(12, "5h")),
            gauge("opencode-go", "OC", "ok", n, &win(30, "rolling")),
        ]));
        let text = render(&app, 96, 10);
        let pos = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s}\n{text}"))
        };
        assert!(pos(" CC") < pos(" CX") && pos(" CX") < pos(" OC ") && pos(" OC ") < pos(" OCb"));
        assert!(text.contains(" mo") && text.contains(" 5h"), "{text}");
        assert!(!text.contains('+'), "nothing hidden at this width\n{text}");
    }

    #[test]
    fn state_mode_stale_record_dims_and_shows_age() {
        let n = now();
        let app = app_with(state(&[gauge(
            "claude",
            "CC",
            "backoff",
            n - 45 * 60,
            &win(12, "5h"),
        )]));
        let text = render(&app, 60, 10);
        assert!(text.contains("CC 45m"), "{text}");
    }

    #[test]
    fn state_mode_error_and_balance_records() {
        let n = now();
        let app = app_with(state(&[
            gauge("opencode-go", "OCh", "error", 0, "null"),
            gauge(
                "nous",
                "N",
                "ok",
                n,
                r#"{"balance":{"amount":19.35,"currency":"USD"}}"#,
            ),
        ]));
        let text = render(&app, 96, 10);
        assert!(text.contains("! no data"), "{text}");
        assert!(text.contains("$19.35"), "{text}");
        // Claude/Codex placeholders still render alongside state records.
        assert!(text.contains("CLAUDE") && text.contains("CODEX"), "{text}");
    }

    #[test]
    fn state_mode_incompatible_schema() {
        let app = app_with(parse_quota_state(r#"{"schema":9,"records":[]}"#).unwrap());
        let text = render(&app, 60, 10);
        assert!(
            text.contains("STATE") && text.contains("incompatible"),
            "{text}"
        );
    }

    #[test]
    fn state_mode_overflow_keeps_worst_and_reports_hidden() {
        let n = now();
        let app = app_with(state(&[
            gauge("claude", "CC", "ok", n, &win(10, "5h")),
            gauge("codex", "CX", "ok", n, &win(20, "5h")),
            gauge("opencode-go", "OA", "ok", n, &win(90, "7d")),
            gauge("opencode-go", "OB", "ok", n, &win(5, "7d")),
            gauge("opencode-go", "OC", "ok", n, &win(80, "7d")),
        ]));
        // 38 cols inner width → 3 columns of >=12.
        let text = render(&app, 40, 10);
        assert!(text.contains("+2"), "{text}");
        assert!(text.contains(" OA") && text.contains(" OC"), "{text}");
        assert!(!text.contains(" OB"), "lowest-usage column hidden\n{text}");
    }

    #[test]
    fn fit_columns_preserves_order() {
        let a = RateLimitInfo {
            source: "a".into(),
            five_hour_pct: Some(50.0),
            ..Default::default()
        };
        let b = RateLimitInfo {
            source: "b".into(),
            five_hour_pct: Some(10.0),
            ..Default::default()
        };
        let c = RateLimitInfo {
            source: "c".into(),
            five_hour_pct: Some(90.0),
            ..Default::default()
        };
        let cols = vec![
            QuotaColumn {
                source: "a",
                info: Some(&a),
            },
            QuotaColumn {
                source: "b",
                info: Some(&b),
            },
            QuotaColumn {
                source: "c",
                info: Some(&c),
            },
        ];
        let (kept, hidden) = fit_columns(cols, 2);
        assert_eq!(hidden, 1);
        let srcs: Vec<&str> = kept.iter().map(|c| c.source).collect();
        assert_eq!(srcs, vec!["a", "c"]);
    }
}
