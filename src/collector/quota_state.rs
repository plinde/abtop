//! External quota state file (`quota_state_file` in config.toml).
//!
//! An external collector may write a JSON document of account-level quota
//! records for any number of providers/accounts. abtop only reads it:
//!
//! ```json
//! {
//!   "schema": 1,
//!   "generated_at": 1795600000,
//!   "records": [
//!     {
//!       "id": "quota:claude:personal", "kind": "gauge", "domain": "ai-quota",
//!       "source": "claude", "account": "personal", "label": "CC",
//!       "status": "ok", "fetched_at": 1795599900, "error": null,
//!       "data": {
//!         "windows": [
//!           {"kind": "5h", "scope": "all", "used_pct": 12, "resets_at": 1795610000},
//!           {"kind": "7d", "scope": "model:Fable", "used_pct": 24, "resets_at": 1796000000}
//!         ],
//!         "balance": {"amount": 19.35, "currency": "USD"},
//!         "binding": {"kind": "7d", "scope": "all", "used_pct": 17, "resets_at": 1796000000}
//!       }
//!     }
//!   ]
//! }
//! ```
//!
//! Only `kind: "gauge"`, `domain: "ai-quota"` records are used; anything else
//! is ignored. `status: "disabled"` records are skipped. Unknown fields are
//! ignored. An unknown `schema` major yields a single "incompatible" record.

use crate::model::{QuotaWindow, RateLimitInfo};
use serde::Deserialize;
use std::path::Path;

/// The only schema major this reader understands.
pub const SUPPORTED_SCHEMA: u64 = 1;

/// A state-file record is stale (dimmed, age shown) after this many seconds.
pub const STATE_STALE_SECS: u64 = 30 * 60;

#[derive(Debug, Deserialize)]
struct Doc {
    #[serde(default)]
    schema: u64,
    #[serde(default)]
    records: Vec<Record>,
}

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    account: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    fetched_at: u64,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    data: Option<Data>,
}

#[derive(Debug, Default, Deserialize)]
struct Data {
    #[serde(default)]
    windows: Vec<Window>,
    #[serde(default)]
    balance: Option<Money>,
    #[serde(default)]
    binding: Option<Window>,
}

#[derive(Debug, Clone, Deserialize)]
struct Window {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    used_pct: f64,
    #[serde(default)]
    resets_at: u64,
}

#[derive(Debug, Deserialize)]
struct Money {
    #[serde(default)]
    amount: f64,
    #[serde(default)]
    currency: String,
}

/// Read the state file. `None` when the file is missing or unparsable, so
/// callers fall back to the legacy sources. Otherwise the usable records,
/// or a single "incompatible" error record for an unknown schema.
pub fn read_quota_state(path: &Path) -> Option<Vec<RateLimitInfo>> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_quota_state(&content)
}

pub fn parse_quota_state(content: &str) -> Option<Vec<RateLimitInfo>> {
    let doc: Doc = serde_json::from_str(content).ok()?;
    if doc.schema != SUPPORTED_SCHEMA {
        return Some(vec![RateLimitInfo {
            source: "state".to_string(),
            label: Some("STATE".to_string()),
            error: Some(format!(
                "incompatible (schema {}, want {})",
                doc.schema, SUPPORTED_SCHEMA
            )),
            from_state: true,
            ..Default::default()
        }]);
    }
    Some(
        doc.records
            .into_iter()
            .filter(|r| r.kind == "gauge" && r.domain == "ai-quota" && r.status != "disabled")
            .map(record_to_info)
            .collect(),
    )
}

fn window_minutes(kind: &str) -> u64 {
    match kind {
        "5h" | "rolling" => 300,
        "daily" => 1440,
        "7d" => 10080,
        "monthly" => 43200,
        _ => u64::MAX,
    }
}

fn window_label(kind: &str) -> String {
    match kind {
        "5h" | "rolling" => "5h".to_string(),
        "daily" => "1d".to_string(),
        "7d" => "7d".to_string(),
        "monthly" => "mo".to_string(),
        other => other.chars().take(2).collect(),
    }
}

fn is_all_scope(scope: &str) -> bool {
    scope.is_empty() || scope == "all"
}

fn record_to_info(r: Record) -> RateLimitInfo {
    let data = r.data.unwrap_or_default();
    let mut windows: Vec<Window> = data
        .windows
        .into_iter()
        .filter(|w| is_all_scope(&w.scope))
        .collect();
    windows.sort_by_key(|w| window_minutes(&w.kind));
    if windows.is_empty() {
        // Fall back to the collector's binding window when only that exists.
        if let Some(b) = data.binding.clone() {
            windows.push(b);
        }
    }

    // Binding = highest usage; on ties the longer window (slower to reset).
    let binding_idx = windows
        .iter()
        .enumerate()
        .fold(None::<usize>, |best, (i, w)| match best {
            Some(b) if windows[b].used_pct >= w.used_pct => Some(b),
            _ => Some(i),
        });

    let quota_windows: Vec<QuotaWindow> = windows
        .iter()
        .enumerate()
        .map(|(i, w)| QuotaWindow {
            label: window_label(&w.kind),
            used_pct: w.used_pct,
            resets_at: (w.resets_at > 0).then_some(w.resets_at),
            binding: Some(i) == binding_idx,
        })
        .collect();

    // Mirror 5h/7d into the legacy fields so rate-limit-aware session status
    // (Waiting → RateLimited) keeps working for state-sourced Claude/Codex.
    let find = |label: &str| windows.iter().find(|w| window_label(&w.kind) == label);
    let five = find("5h");
    let seven = find("7d");

    let balance = data.balance.map(|m| {
        if m.currency.is_empty() || m.currency.eq_ignore_ascii_case("USD") {
            format!("${:.2}", m.amount)
        } else {
            format!("{:.2} {}", m.amount, m.currency)
        }
    });

    let has_values = !quota_windows.is_empty() || balance.is_some();
    let error = if has_values {
        None
    } else {
        Some(
            r.error
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| "no data".to_string()),
        )
    };

    let label = if !r.label.is_empty() {
        r.label
    } else if !r.account.is_empty() && r.source == "opencode-go" {
        r.account.to_uppercase()
    } else {
        r.source.to_uppercase()
    };

    RateLimitInfo {
        source: r.source,
        five_hour_pct: five.map(|w| w.used_pct),
        five_hour_resets_at: five.and_then(|w| (w.resets_at > 0).then_some(w.resets_at)),
        seven_day_pct: seven.map(|w| w.used_pct),
        seven_day_resets_at: seven.and_then(|w| (w.resets_at > 0).then_some(w.resets_at)),
        updated_at: (r.fetched_at > 0).then_some(r.fetched_at),
        label: Some(label),
        windows: quota_windows,
        balance,
        status: (!r.status.is_empty() && r.status != "ok").then_some(r.status),
        error,
        from_state: true,
    }
}

/// Whether a state-sourced record should render dimmed with its age.
pub fn is_state_record_stale(info: &RateLimitInfo, now: u64) -> bool {
    info.status.is_some()
        || info
            .updated_at
            .is_none_or(|ts| now.saturating_sub(ts) > STATE_STALE_SECS)
}

/// Merge sources with the documented precedence:
/// - Codex: live session data > state file > on-disk Codex cache.
/// - Claude: state file > `abtop-rate-limits.json` (legacy hook file).
/// - Other providers: state file only.
///
/// `state` is `None` when no state file is configured or it is unreadable;
/// the result then equals the legacy behaviour (legacy + live, cache fallback).
pub fn merge_rate_limits(
    legacy: Vec<RateLimitInfo>,
    live: Vec<RateLimitInfo>,
    state: Option<Vec<RateLimitInfo>>,
    codex_cache: impl FnOnce() -> Option<RateLimitInfo>,
) -> Vec<RateLimitInfo> {
    let has_live_codex = live.iter().any(|r| r.source == "codex");
    let Some(state) = state else {
        let mut out = legacy;
        out.extend(live);
        if !has_live_codex {
            out.extend(codex_cache());
        }
        return out;
    };

    let state_has = |src: &str| state.iter().any(|r| r.source == src);
    let mut out = Vec::new();

    // Claude
    if state_has("claude") {
        out.extend(state.iter().filter(|r| r.source == "claude").cloned());
    } else {
        out.extend(legacy.iter().filter(|r| r.source == "claude").cloned());
    }
    // Codex
    if has_live_codex {
        out.extend(live.iter().filter(|r| r.source == "codex").cloned());
    } else if state_has("codex") {
        out.extend(state.iter().filter(|r| r.source == "codex").cloned());
    } else {
        out.extend(codex_cache());
    }
    // Any other live sources (none today) and all other state records.
    out.extend(live.into_iter().filter(|r| r.source != "codex"));
    out.extend(
        state
            .into_iter()
            .filter(|r| r.source != "claude" && r.source != "codex"),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(schema: u64, records: &str) -> String {
        format!(r#"{{"schema":{schema},"generated_at":1,"records":[{records}]}}"#)
    }

    fn gauge(source: &str, label: &str, status: &str, fetched: u64, data: &str) -> String {
        format!(
            r#"{{"id":"quota:{source}:x","kind":"gauge","domain":"ai-quota","source":"{source}","account":"x","label":"{label}","status":"{status}","fetched_at":{fetched},"data":{data}}}"#
        )
    }

    #[test]
    fn missing_or_unparsable_falls_back() {
        assert!(read_quota_state(Path::new("/nonexistent/quota-state.json")).is_none());
        assert!(parse_quota_state("{not json").is_none());
    }

    #[test]
    fn unknown_schema_is_incompatible() {
        let out = parse_quota_state(&doc(2, "")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label.as_deref(), Some("STATE"));
        assert!(out[0].error.as_deref().unwrap().contains("incompatible"));
    }

    #[test]
    fn ok_record_maps_windows_and_binding() {
        let data = r#"{"windows":[
            {"kind":"7d","scope":"all","used_pct":17,"resets_at":2000},
            {"kind":"5h","scope":"all","used_pct":12,"resets_at":1000},
            {"kind":"7d","scope":"model:Fable","used_pct":99,"resets_at":2000}]}"#;
        let out = parse_quota_state(&doc(1, &gauge("claude", "CC", "ok", 50, data))).unwrap();
        let r = &out[0];
        assert_eq!(r.label.as_deref(), Some("CC"));
        assert_eq!(r.windows.len(), 2, "scoped windows are excluded");
        assert_eq!(r.windows[0].label, "5h");
        assert_eq!(r.windows[1].label, "7d");
        assert!(r.windows[1].binding && !r.windows[0].binding);
        assert_eq!(r.five_hour_pct, Some(12.0));
        assert_eq!(r.seven_day_pct, Some(17.0));
        assert_eq!(r.updated_at, Some(50));
        assert!(r.status.is_none() && r.error.is_none());
    }

    #[test]
    fn ignores_other_kinds_domains_and_disabled() {
        let w = r#"{"windows":[{"kind":"5h","scope":"all","used_pct":1,"resets_at":1}]}"#;
        let recs = [
            gauge("claude", "CC", "ok", 1, w),
            gauge("opencode-go", "OCe", "disabled", 1, w),
            r#"{"kind":"check","domain":"ai-quota","source":"x"}"#.to_string(),
            r#"{"kind":"gauge","domain":"health","source":"y"}"#.to_string(),
        ]
        .join(",");
        let out = parse_quota_state(&doc(1, &recs)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "claude");
    }

    #[test]
    fn stale_and_error_records() {
        let w = r#"{"windows":[{"kind":"rolling","scope":"all","used_pct":5,"resets_at":1}]}"#;
        let recs = [
            gauge("opencode-go", "OC", "backoff", 1000, w),
            gauge("opencode-go", "OCb", "error", 0, "null"),
            gauge("codex", "CX", "ok", 1000, w),
        ]
        .join(",");
        let out = parse_quota_state(&doc(1, &recs)).unwrap();
        let now = 1000 + 60;
        assert!(
            is_state_record_stale(&out[0], now),
            "non-ok status is stale"
        );
        assert_eq!(out[0].status.as_deref(), Some("backoff"));
        assert!(out[0].error.is_none(), "values present → no error");
        assert_eq!(out[1].error.as_deref(), Some("no data"));
        assert!(!is_state_record_stale(&out[2], now));
        assert!(is_state_record_stale(&out[2], 1000 + STATE_STALE_SECS + 1));
    }

    #[test]
    fn error_text_from_collector() {
        let r = r#"{"kind":"gauge","domain":"ai-quota","source":"opencode-go","label":"OCh","status":"error","error":"HTTP 401"}"#;
        let out = parse_quota_state(&doc(1, r)).unwrap();
        assert_eq!(out[0].error.as_deref(), Some("HTTP 401"));
    }

    #[test]
    fn balance_only_record() {
        let data = r#"{"balance":{"amount":19.354,"currency":"USD"}}"#;
        let out = parse_quota_state(&doc(1, &gauge("nous", "N", "ok", 1, data))).unwrap();
        assert_eq!(out[0].balance.as_deref(), Some("$19.35"));
        assert!(out[0].windows.is_empty() && out[0].error.is_none());
    }

    #[test]
    fn binding_fallback_when_no_all_windows() {
        let data = r#"{"binding":{"kind":"monthly","scope":"all","used_pct":40,"resets_at":5}}"#;
        let out = parse_quota_state(&doc(1, &gauge("opencode-go", "OC", "ok", 1, data))).unwrap();
        assert_eq!(out[0].windows.len(), 1);
        assert_eq!(out[0].windows[0].label, "mo");
        assert!(out[0].windows[0].binding);
    }

    fn info(source: &str, from_state: bool) -> RateLimitInfo {
        RateLimitInfo {
            source: source.to_string(),
            from_state,
            ..Default::default()
        }
    }

    #[test]
    fn merge_without_state_is_legacy_behaviour() {
        let out = merge_rate_limits(vec![info("claude", false)], vec![], None, || {
            Some(info("codex", false))
        });
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].source, "codex", "cache fallback when no live codex");
        let out = merge_rate_limits(vec![], vec![info("codex", false)], None, || {
            panic!("cache must not be read when live codex exists")
        });
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn merge_precedence_with_state() {
        let state = vec![
            info("claude", true),
            info("codex", true),
            info("opencode-go", true),
        ];
        // Live codex beats state; state claude beats legacy.
        let out = merge_rate_limits(
            vec![info("claude", false)],
            vec![info("codex", false)],
            Some(state.clone()),
            || panic!("no cache read"),
        );
        let srcs: Vec<(&str, bool)> = out
            .iter()
            .map(|r| (r.source.as_str(), r.from_state))
            .collect();
        assert_eq!(
            srcs,
            vec![("claude", true), ("codex", false), ("opencode-go", true)]
        );
        // No live codex → state codex.
        let out = merge_rate_limits(vec![], vec![], Some(state), || panic!("no cache read"));
        assert!(out.iter().any(|r| r.source == "codex" && r.from_state));
        // State without claude/codex → legacy claude + codex cache.
        let out = merge_rate_limits(
            vec![info("claude", false)],
            vec![],
            Some(vec![info("nous", true)]),
            || Some(info("codex", false)),
        );
        let srcs: Vec<&str> = out.iter().map(|r| r.source.as_str()).collect();
        assert_eq!(srcs, vec!["claude", "codex", "nous"]);
    }
}
