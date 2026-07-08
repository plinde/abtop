//! Session filter query parsing.
//!
//! The `/` filter box accepts free-text substring terms plus optional
//! duration predicates that filter sessions by activity recency or age.
//! Grammar (whitespace-separated tokens):
//!
//! ```text
//! <field><op><number><unit>
//! ```
//!
//! - field: `time` (time since last turn), `age` (time since session start)
//! - op:    `>`, `<`, `>=`, `<=`
//! - unit:  `s`, `m`, `h`, `d`
//!
//! `time` unifies "active" and "idle": the operator carries the direction, so
//! there is no need for separate keywords.
//!
//! Examples:
//! - `time<24h`  -> sessions with a turn in the last 24 hours (active)
//! - `time>3d`   -> sessions with no turn for more than 3 days (stale)
//! - `age>=7d`   -> sessions started at least 7 days ago
//!
//! Any token that is not a valid predicate is treated as substring text.
//! Multiple predicates are ANDed together; text terms and predicates combine
//! with AND. When no predicate is present, the entire raw query is used as the
//! substring term (preserving the original filter behavior, including spaces).

use std::time::Duration;

/// Which time value a predicate compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationField {
    /// Time since the session's most recent turn (`last_turn_age`).
    Time,
    /// Time since the session started (`elapsed`).
    Age,
}

/// Comparison operator for a duration predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationOp {
    Gt,
    Lt,
    Ge,
    Le,
}

impl DurationOp {
    fn eval(self, lhs: Duration, rhs: Duration) -> bool {
        match self {
            DurationOp::Gt => lhs > rhs,
            DurationOp::Lt => lhs < rhs,
            DurationOp::Ge => lhs >= rhs,
            DurationOp::Le => lhs <= rhs,
        }
    }
}

/// A single parsed duration predicate, e.g. `time>3d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationPredicate {
    pub field: DurationField,
    pub op: DurationOp,
    pub threshold: Duration,
}

impl DurationPredicate {
    /// Parse a single token as a predicate. Returns `None` when the token does
    /// not match the predicate grammar (caller treats it as substring text).
    fn parse(token: &str) -> Option<Self> {
        let lower = token.to_ascii_lowercase();
        let (field, rest) = if let Some(r) = lower.strip_prefix("time") {
            (DurationField::Time, r)
        } else if let Some(r) = lower.strip_prefix("age") {
            (DurationField::Age, r)
        } else {
            return None;
        };

        // Order matters: check the two-char operators before the one-char ones.
        let (op, rest) = if let Some(r) = rest.strip_prefix(">=") {
            (DurationOp::Ge, r)
        } else if let Some(r) = rest.strip_prefix("<=") {
            (DurationOp::Le, r)
        } else if let Some(r) = rest.strip_prefix('>') {
            (DurationOp::Gt, r)
        } else if let Some(r) = rest.strip_prefix('<') {
            (DurationOp::Lt, r)
        } else {
            return None;
        };

        let threshold = parse_duration(rest)?;
        Some(DurationPredicate {
            field,
            op,
            threshold,
        })
    }

    fn matches(&self, time: Duration, age: Duration) -> bool {
        let value = match self.field {
            DurationField::Time => time,
            DurationField::Age => age,
        };
        self.op.eval(value, self.threshold)
    }
}

/// Parse a bare duration like `24h`, `3d`, `30m`, `45s` into a [`Duration`].
fn parse_duration(raw: &str) -> Option<Duration> {
    if raw.len() < 2 {
        return None;
    }
    let (num, unit) = raw.split_at(raw.len() - 1);
    let value: u64 = num.parse().ok()?;
    let secs = match unit {
        "s" => value,
        "m" => value.checked_mul(60)?,
        "h" => value.checked_mul(3_600)?,
        "d" => value.checked_mul(86_400)?,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// A parsed session filter query: substring text plus duration predicates.
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    /// Lowercased substring term; empty means "no text constraint".
    text: String,
    predicates: Vec<DurationPredicate>,
}

impl SessionQuery {
    /// Parse the raw filter box contents into a structured query.
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::default();
        }

        let mut predicates = Vec::new();
        let mut text_tokens = Vec::new();
        for token in trimmed.split_whitespace() {
            match DurationPredicate::parse(token) {
                Some(pred) => predicates.push(pred),
                None => text_tokens.push(token),
            }
        }

        // Backward compatibility: with no predicates, use the entire raw query
        // (including internal spaces) as the substring term, matching the
        // original filter behavior.
        let text = if predicates.is_empty() {
            trimmed.to_lowercase()
        } else {
            text_tokens.join(" ").to_lowercase()
        };

        Self { text, predicates }
    }

    /// True when the query imposes no constraints (show everything).
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.predicates.is_empty()
    }

    /// True when the query has at least one duration predicate.
    pub fn has_duration_predicate(&self) -> bool {
        !self.predicates.is_empty()
    }

    /// Lowercased substring term (empty = no text constraint).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Evaluate the duration predicates against a session's time-since-last-turn
    /// and age durations. Text matching is handled separately by the caller.
    pub fn duration_matches(&self, time: Duration, age: Duration) -> bool {
        self.predicates.iter().all(|p| p.matches(time, age))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dur(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn parses_plain_text_as_substring() {
        let q = SessionQuery::parse("abtop");
        assert_eq!(q.text(), "abtop");
        assert!(!q.has_duration_predicate());
        assert!(!q.is_empty());
    }

    #[test]
    fn empty_query_is_empty() {
        assert!(SessionQuery::parse("").is_empty());
        assert!(SessionQuery::parse("   ").is_empty());
    }

    #[test]
    fn plain_text_preserves_spaces() {
        // No predicate -> whole string is the substring term.
        let q = SessionQuery::parse("my project");
        assert_eq!(q.text(), "my project");
        assert!(!q.has_duration_predicate());
    }

    #[test]
    fn time_less_than_matches_active_window() {
        let q = SessionQuery::parse("time<24h");
        assert!(q.has_duration_predicate());
        assert_eq!(q.text(), "");
        // 1h since last turn is within 24h -> matches (active).
        assert!(q.duration_matches(dur(3_600), dur(0)));
        // 25h -> excluded.
        assert!(!q.duration_matches(dur(90_000), dur(0)));
    }

    #[test]
    fn time_greater_than_matches_stale() {
        let q = SessionQuery::parse("time>3d");
        // 4 days since last turn -> stale, matches.
        assert!(q.duration_matches(dur(4 * 86_400), dur(0)));
        // 2 days -> not stale.
        assert!(!q.duration_matches(dur(2 * 86_400), dur(0)));
    }

    #[test]
    fn age_field_uses_second_argument() {
        let q = SessionQuery::parse("age>7d");
        // time small, age large -> matches on age.
        assert!(q.duration_matches(dur(10), dur(8 * 86_400)));
        assert!(!q.duration_matches(dur(10), dur(6 * 86_400)));
    }

    #[test]
    fn ge_and_le_are_inclusive() {
        let ge = SessionQuery::parse("time>=1h");
        assert!(ge.duration_matches(dur(3_600), dur(0)));
        let le = SessionQuery::parse("time<=1h");
        assert!(le.duration_matches(dur(3_600), dur(0)));
    }

    #[test]
    fn all_units_parse() {
        assert!(SessionQuery::parse("time>30s").has_duration_predicate());
        assert!(SessionQuery::parse("time>30m").has_duration_predicate());
        assert!(SessionQuery::parse("time>30h").has_duration_predicate());
        assert!(SessionQuery::parse("time>30d").has_duration_predicate());
    }

    #[test]
    fn combines_text_and_predicate() {
        let q = SessionQuery::parse("abtop time>1d");
        assert_eq!(q.text(), "abtop");
        assert!(q.has_duration_predicate());
        assert!(q.duration_matches(dur(2 * 86_400), dur(0)));
        assert!(!q.duration_matches(dur(3_600), dur(0)));
    }

    #[test]
    fn multiple_predicates_and_together() {
        // Touched in the last 7d AND idle more than 1h ("warm but paused").
        let q = SessionQuery::parse("time<7d time>1h");
        assert!(q.duration_matches(dur(2 * 3_600), dur(0))); // 2h
        assert!(!q.duration_matches(dur(30), dur(0))); // 30s: too fresh
        assert!(!q.duration_matches(dur(8 * 86_400), dur(0))); // 8d: too stale
    }

    #[test]
    fn invalid_predicate_falls_back_to_text() {
        // Missing unit / bad number -> treated as substring text.
        let q = SessionQuery::parse("time>abc");
        assert!(!q.has_duration_predicate());
        assert_eq!(q.text(), "time>abc");

        let q2 = SessionQuery::parse("time>");
        assert!(!q2.has_duration_predicate());
        assert_eq!(q2.text(), "time>");
    }

    #[test]
    fn retired_keywords_are_plain_text() {
        // `active` / `idle` were unified into `time`; they are now substring
        // text, not predicates.
        let a = SessionQuery::parse("active<24h");
        assert!(!a.has_duration_predicate());
        assert_eq!(a.text(), "active<24h");
        let i = SessionQuery::parse("idle>3d");
        assert!(!i.has_duration_predicate());
        assert_eq!(i.text(), "idle>3d");
    }

    #[test]
    fn case_insensitive_predicate() {
        let q = SessionQuery::parse("TIME>3D");
        assert!(q.has_duration_predicate());
        assert!(q.duration_matches(dur(4 * 86_400), dur(0)));
    }
}
