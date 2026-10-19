use super::{process, context_window_for_model};
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum sessions to fetch from the DB per query.
const MAX_SESSIONS: u32 = 20;

/// Collector for OpenCode sessions.
///
/// Discovery strategy:
/// 1. `ps` to find running opencode processes (from shared process data)
/// 2. Query SQLite DB at ~/.local/share/opencode/opencode.db via `sqlite3` CLI
/// 3. Match running PIDs to sessions by cwd
///
/// Uses `sqlite3 -readonly -json` for safe concurrent reads (WAL mode).
/// DB rows are cached and only refreshed on `shared.slow_tick` (every ~10s)
/// so we don't fork a sqlite3 process every 2s. PID matching, status
/// derivation and the children walk run every tick using live process info.
pub struct OpenCodeCollector {
    db_path: PathBuf,
    /// Whether sqlite3 CLI is available (checked once).
    sqlite3_available: Option<bool>,
    /// Cached DB rows from the last slow-tick query. Reused on fast ticks.
    cached_db_sessions: Vec<DbSession>,
    /// Whether the "sqlite3 missing" warning has been emitted (once).
    #[cfg(target_os = "windows")]
    warned_sqlite3_missing: bool,
}

impl OpenCodeCollector {
    pub fn new() -> Self {
        let data_dir = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
        let db_path = data_dir.join("opencode").join("opencode.db");
        #[cfg(target_os = "windows")]
        let db_path = windows_db_path(db_path);
        Self {
            db_path,
            sqlite3_available: None,
            cached_db_sessions: Vec::new(),
            #[cfg(target_os = "windows")]
            warned_sqlite3_missing: false,
        }
    }

    fn check_sqlite3(&mut self) -> bool {
        if let Some(available) = self.sqlite3_available {
            return available;
        }
        let available = Command::new("sqlite3").arg("--version").output().is_ok();
        self.sqlite3_available = Some(available);
        available
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // Security: skip if db_path is a symlink (fail-closed)
        if is_symlink(&self.db_path) || !self.db_path.exists() {
            self.cached_db_sessions.clear();
            return vec![];
        }
        if !self.check_sqlite3() {
            // The DB exists but we can't read it: on Windows sqlite3 is
            // usually not preinstalled, so say why sessions are missing
            // instead of failing silently.
            #[cfg(target_os = "windows")]
            if !self.warned_sqlite3_missing {
                self.warned_sqlite3_missing = true;
                eprintln!(
                    "abtop: OpenCode database found at {} but the `sqlite3` CLI is not on PATH; \
                     OpenCode sessions will not appear. Install it (e.g. `winget install SQLite.SQLite`) \
                     and restart abtop.",
                    self.db_path.display()
                );
            }
            self.cached_db_sessions.clear();
            return vec![];
        }

        // Find running opencode PIDs and their commands for cwd matching
        let opencode_pids = Self::find_opencode_pids(&shared.process_info);
        let pid_commands: HashMap<u32, &str> = opencode_pids
            .iter()
            .filter_map(|&pid| {
                shared
                    .process_info
                    .get(&pid)
                    .map(|p| (pid, p.command.as_str()))
            })
            .collect();

        // Refresh DB rows on slow ticks only; reuse cache on fast ticks so
        // we don't fork sqlite3 every 2s.
        if shared.slow_tick {
            if let Some(rows) = self.query_sessions() {
                self.cached_db_sessions = rows;
            }
        }

        let now_ms = current_time_ms();
        let mut sessions = Vec::new();

        let mut claimed_pids = HashSet::new();
        for ds in &self.cached_db_sessions {
            let matched_pid =
                Self::match_pid_to_session_once(&pid_commands, &ds.directory, &mut claimed_pids);
            // Drop sessions whose process isn't running. (Done sessions are
            // filtered out by MultiCollector::collect anyway, so emitting
            // a Done row here would be dead code.)
            let Some(matched_pid) = matched_pid else {
                continue;
            };

            let proc = shared.process_info.get(&matched_pid);
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

            let age_ms = now_ms.saturating_sub(ds.time_updated);
            let since_update_secs = age_ms / 1000;
            let status = if since_update_secs < 30 {
                SessionStatus::Thinking
            } else {
                let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
                let has_active_child = process::has_active_descendant(
                    matched_pid,
                    &shared.children_map,
                    &shared.process_info,
                    5.0,
                );
                if cpu_active || has_active_child {
                    SessionStatus::Thinking
                } else {
                    SessionStatus::Waiting
                }
            };

            let project_name = if !ds.project_name.is_empty() {
                ds.project_name.clone()
            } else {
                // last_path_segment also splits on `\` on Windows.
                process::last_path_segment(&ds.directory)
                    .unwrap_or("?")
                    .to_string()
            };

            let current_tasks = if matches!(status, SessionStatus::Waiting) {
                vec!["waiting for input".to_string()]
            } else {
                vec!["thinking...".to_string()]
            };

            // Collect child processes with cycle guard (visited set)
            let mut children = Vec::new();
            let mut stack: Vec<u32> = shared
                .children_map
                .get(&matched_pid)
                .cloned()
                .unwrap_or_default();
            let mut visited = std::collections::HashSet::new();
            while let Some(cpid) = stack.pop() {
                if !visited.insert(cpid) {
                    continue;
                }
                if let Some(cproc) = shared.process_info.get(&cpid) {
                    let port = shared.ports.get(&cpid).and_then(|v| v.first().copied());
                    children.push(ChildProcess {
                        pid: cpid,
                        command: cproc.command.clone(),
                        mem_kb: cproc.rss_kb,
                        port,
                    });
                }
                if let Some(grandchildren) = shared.children_map.get(&cpid) {
                    stack.extend(grandchildren);
                }
            }

            let model = if !ds.provider.is_empty() && !ds.model.is_empty() {
                format!("{}/{}", ds.provider, ds.model)
            } else if !ds.model.is_empty() {
                ds.model.clone()
            } else {
                "-".to_string()
            };

            let context_window = context_window_for_model(&model, "", 0);
            let context_percent = if context_window > 0 {
                ((ds.total_input + ds.total_output) as f64 / context_window as f64) * 100.0
            } else {
                0.0
            };

            sessions.push(AgentSession {
                agent_cli: "opencode",
                pid: matched_pid,
                session_id: ds.id.clone(),
                cwd: ds.directory.clone(),
                project_name,
                started_at: ds.time_created,
                last_turn_at: ds.time_updated.max(ds.time_created),
                status,
                model,
                effort: String::new(),
                context_percent,
                total_input_tokens: ds.total_input,
                total_output_tokens: ds.total_output,
                total_cache_read: ds.total_cache_read,
                total_cache_create: ds.total_cache_write,
                turn_count: ds.turn_count,
                current_tasks,
                mem_mb,
                version: ds.version.clone(),
                git_branch: String::new(),
                git_added: 0,
                git_modified: 0,
                token_history: vec![],
                context_history: vec![],
                compaction_count: 0,
                context_window,
                subagents: vec![],
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt: ds.title.clone(),
                first_assistant_text: String::new(),
                chat_messages: vec![],
                tool_calls: vec![],
                pending_since_ms: 0,
                thinking_since_ms: 0,
                file_accesses: vec![],
                config_root: super::abbrev_path(
                    self.db_path.parent().unwrap_or(std::path::Path::new(".")),
                ),
            });
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn find_opencode_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        process_info
            .iter()
            .filter(|(_, info)| {
                process::cmd_has_binary(&info.command, "opencode")
                    && !info.command.contains("grep")
                    && !is_shared_service(&info.command)
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Match a running PID to a session by comparing its working directory
    /// with the DB session's `directory`, falling back to a command-line
    /// substring match. Returns `None` if no PID's cwd or command line ties
    /// to this session — we deliberately do not fall back to "the only
    /// opencode process" here, because that would mark every DB row as
    /// alive whenever a single opencode is running in an unrelated dir.
    #[cfg(test)]
    fn match_pid_to_session(pid_commands: &HashMap<u32, &str>, session_dir: &str) -> Option<u32> {
        Self::match_pid_to_session_excluding(pid_commands, session_dir, &HashSet::new())
    }

    fn match_pid_to_session_once(
        pid_commands: &HashMap<u32, &str>,
        session_dir: &str,
        claimed_pids: &mut HashSet<u32>,
    ) -> Option<u32> {
        let pid = Self::match_pid_to_session_excluding(pid_commands, session_dir, claimed_pids)?;
        claimed_pids.insert(pid);
        Some(pid)
    }

    fn match_pid_to_session_excluding(
        pid_commands: &HashMap<u32, &str>,
        session_dir: &str,
        claimed_pids: &HashSet<u32>,
    ) -> Option<u32> {
        // Empty / single-character `session_dir` (e.g. "" or "/") would
        // make the substring fallback match unrelated commands, so skip
        // matching entirely in that case.
        if session_dir.len() < 2 {
            return None;
        }
        for (&pid, &cmd) in pid_commands {
            if claimed_pids.contains(&pid) {
                continue;
            }
            if let Some(cwd) = get_process_cwd(pid) {
                if paths_equal(&cwd, session_dir) {
                    return Some(pid);
                }
            }
            if cmd.contains(session_dir) {
                return Some(pid);
            }
        }
        None
    }

    /// Run a single sqlite3 query and parse the JSON output.
    fn run_query(&self, sql: &str) -> Option<Vec<Value>> {
        let db = self.db_path.to_str()?;
        let output = Command::new("sqlite3")
            .args(["-readonly", "-json", db])
            .arg(sql)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Some(vec![]);
        }
        serde_json::from_str(stdout.trim()).ok()
    }

    /// Whether the DB has the OpenCode V2 tables (`session_v2` +
    /// `session_message`). V1 hosts only have `session`/`message`/`part`.
    fn has_v2_tables(&self) -> bool {
        self.run_query(V2_DETECT_SQL)
            .and_then(|rows| rows.first().and_then(|r| r["n"].as_u64()))
            .is_some_and(|n| n == 2)
    }

    fn query_sessions(&self) -> Option<Vec<DbSession>> {
        let rows = self.run_query(&session_sql(self.has_v2_tables()))?;

        let mut sessions = Vec::new();
        for row in rows {
            let id = row["id"].as_str().unwrap_or("").to_string();
            let model = sanitize_db_field(row["model"].as_str().unwrap_or(""), 256);
            let provider = sanitize_db_field(row["provider"].as_str().unwrap_or(""), 256);

            // Sanitize DB-sourced strings before they reach the TUI/JSON snapshot.
            let title = sanitize_db_title(row["title"].as_str().unwrap_or(""));
            let directory = sanitize_db_field(row["directory"].as_str().unwrap_or(""), 4096);
            let version = sanitize_db_field(row["version"].as_str().unwrap_or(""), 64);
            let project_name = sanitize_db_field(row["project_name"].as_str().unwrap_or(""), 256);

            sessions.push(DbSession {
                id,
                title,
                directory,
                version,
                // time_created and time_updated are in milliseconds since epoch
                time_created: row["time_created"].as_u64().unwrap_or(0),
                time_updated: row["time_updated"].as_u64().unwrap_or(0),
                project_name,
                turn_count: row["turn_count"].as_u64().unwrap_or(0) as u32,
                total_input: row["total_input"].as_u64().unwrap_or(0),
                total_output: row["total_output"].as_u64().unwrap_or(0),
                total_cache_read: row["total_cache_read"].as_u64().unwrap_or(0),
                total_cache_write: row["total_cache_write"].as_u64().unwrap_or(0),
                model,
                provider,
            });
        }

        Some(sessions)
    }
}

const V2_DETECT_SQL: &str = "SELECT COUNT(*) AS n FROM sqlite_master \
     WHERE type = 'table' AND name IN ('session_v2', 'session_message');";

/// Build the session query.
///
/// OpenCode V1 stores sessions in `session` and messages in `message`
/// (`$.role`, `$.modelID`, `$.providerID`). OpenCode V2 (2.x) stores them in
/// `session_v2` and `session_message` (`type` column, `$.model.id`,
/// `$.model.providerID`); token paths (`$.tokens.*`) are the same in both.
/// A V2 host keeps the frozen V1 tables and copies V1 sessions into
/// `session_v2`, so when V2 tables exist V1 rows are only included for
/// session ids absent from `session_v2` (no double counting).
///
/// Only the newest `MAX_SESSIONS` sessions are aggregated.
fn session_sql(has_v2: bool) -> String {
    let v1_recent = "SELECT id, title, directory, version, time_created, time_updated, \
         project_id, NULL AS model_json, 'v1' AS src FROM session";
    let v1_msgs = "SELECT m.session_id AS sid, m.time_created AS tc, m.data AS data \
         FROM message m JOIN recent r ON r.id = m.session_id AND r.src = 'v1' \
         WHERE json_extract(m.data, '$.role') = 'assistant'";
    let (recent, msgs) = if has_v2 {
        (
            format!(
                "SELECT id, title, directory, version, time_created, time_updated, \
                 project_id, model AS model_json, 'v2' AS src FROM session_v2 \
                 UNION ALL {v1_recent} WHERE id NOT IN (SELECT id FROM session_v2)"
            ),
            format!(
                "SELECT m.session_id AS sid, m.time_created AS tc, m.data AS data \
                 FROM session_message m JOIN recent r ON r.id = m.session_id AND r.src = 'v2' \
                 WHERE m.type = 'assistant' \
                 UNION ALL {v1_msgs}"
            ),
        )
    } else {
        (v1_recent.to_string(), v1_msgs.to_string())
    };
    format!(
        r#"
WITH recent AS (
  {recent}
  ORDER BY time_updated DESC
  LIMIT {MAX_SESSIONS}
),
msgs AS MATERIALIZED (
  {msgs}
)
SELECT
  r.id, r.title, r.directory, r.version, r.time_created, r.time_updated,
  COALESCE(p.name, '') AS project_name,
  (SELECT COUNT(*) FROM msgs x WHERE x.sid = r.id) AS turn_count,
  (SELECT COALESCE(SUM(json_extract(x.data, '$.tokens.input')), 0) FROM msgs x WHERE x.sid = r.id) AS total_input,
  (SELECT COALESCE(SUM(json_extract(x.data, '$.tokens.output')), 0) FROM msgs x WHERE x.sid = r.id) AS total_output,
  (SELECT COALESCE(SUM(json_extract(x.data, '$.tokens.cache.read')), 0) FROM msgs x WHERE x.sid = r.id) AS total_cache_read,
  (SELECT COALESCE(SUM(json_extract(x.data, '$.tokens.cache.write')), 0) FROM msgs x WHERE x.sid = r.id) AS total_cache_write,
  COALESCE(
    (SELECT COALESCE(json_extract(x.data, '$.model.id'), json_extract(x.data, '$.modelID'))
       FROM msgs x WHERE x.sid = r.id ORDER BY x.tc DESC LIMIT 1),
    json_extract(r.model_json, '$.id'),
    '') AS model,
  COALESCE(
    (SELECT COALESCE(json_extract(x.data, '$.model.providerID'), json_extract(x.data, '$.providerID'))
       FROM msgs x WHERE x.sid = r.id ORDER BY x.tc DESC LIMIT 1),
    json_extract(r.model_json, '$.providerID'),
    '') AS provider
FROM recent r
LEFT JOIN project p ON r.project_id = p.id
ORDER BY r.time_updated DESC;"#
    )
}

impl Default for OpenCodeCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for OpenCodeCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

struct DbSession {
    id: String,
    title: String,
    directory: String,
    version: String,
    time_created: u64,
    time_updated: u64,
    project_name: String,
    turn_count: u32,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_write: u64,
    model: String,
    provider: String,
}

/// OpenCode V2 runs one shared background server (`opencode serve --service`)
/// for all clients. It owns no single session and its cwd is usually `$HOME`,
/// so it must not be matched to a session; the TUI/`run` clients are.
fn is_shared_service(command: &str) -> bool {
    let has = |arg: &str| command.split_whitespace().any(|a| a == arg);
    has("serve") && has("--service")
}

/// Check if a path is a symlink (fail-closed: returns true on error).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

fn sanitize_db_title(raw: &str) -> String {
    super::redact_secrets(&sanitize_db_field(raw, 512))
}

fn sanitize_db_field(raw: &str, max_bytes: usize) -> String {
    let mut value = super::sanitize_terminal_text(raw);
    truncate_field(&mut value, max_bytes);
    value
}

/// Truncate a string at a char boundary to avoid panics on multi-byte UTF-8.
fn truncate_field(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Compare a process cwd with a DB session directory.
/// On Windows paths are case-insensitive and may mix `/` and `\`, so
/// normalize before comparing; elsewhere keep the exact comparison.
#[cfg(target_os = "windows")]
fn paths_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

#[cfg(not(target_os = "windows"))]
fn paths_equal(a: &str, b: &str) -> bool {
    a == b
}

/// On Windows, OpenCode builds (e.g. installed via npm) have been observed to
/// keep the XDG-style `~/.local/share/opencode` layout, so prefer the same
/// path as unix; fall back to probing `%LOCALAPPDATA%` / `%APPDATA%` in case
/// a build stores the DB there instead.
#[cfg(target_os = "windows")]
fn windows_db_path(default: PathBuf) -> PathBuf {
    if default.exists() {
        return default;
    }
    for var in ["LOCALAPPDATA", "APPDATA"] {
        if let Ok(base) = std::env::var(var) {
            if base.is_empty() {
                continue;
            }
            let candidate = PathBuf::from(base).join("opencode").join("opencode.db");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    default
}

/// Get the current working directory of a process.
/// Uses /proc on Linux, sysinfo (PEB) on Windows, lsof on macOS/other Unix.
#[cfg(target_os = "linux")]
fn get_process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(target_os = "windows")]
fn get_process_cwd(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    // `lsof` does not exist on Windows; sysinfo reads the cwd from the
    // process PEB. Refresh just this one PID — this runs only for the
    // handful of opencode PIDs, once per tick.
    let mut sys = System::new();
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
    );
    sys.process(pid)
        .and_then(|p| p.cwd())
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
fn get_process_cwd(pid: u32) -> Option<String> {
    // -a ANDs the selection terms; without it, lsof ORs `-p <pid>` with
    // `-d cwd` and returns cwd entries for unrelated processes too.
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // lsof -Fn output: lines starting with 'n' contain the path
    stdout
        .lines()
        .find(|l| l.starts_with('n') && l.len() > 1)
        .map(|l| l[1..].to_string())
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_opencode_pids() {
        let mut info = HashMap::new();
        info.insert(
            100,
            process::ProcInfo {
                pid: 100,
                ppid: 1,
                rss_kb: 1000,
                cpu_pct: 0.0,
                command: "/home/user/.opencode/bin/opencode".to_string(),
            },
        );
        info.insert(
            200,
            process::ProcInfo {
                pid: 200,
                ppid: 1,
                rss_kb: 500,
                cpu_pct: 0.0,
                command: "grep opencode".to_string(),
            },
        );
        info.insert(
            300,
            process::ProcInfo {
                pid: 300,
                ppid: 1,
                rss_kb: 800,
                cpu_pct: 0.0,
                command: "node /usr/bin/opencode run test".to_string(),
            },
        );
        let pids = OpenCodeCollector::find_opencode_pids(&info);
        assert!(pids.contains(&100));
        assert!(!pids.contains(&200)); // grep excluded
        assert!(pids.contains(&300));
        assert_eq!(pids.len(), 2);
    }

    #[test]
    fn test_db_path_default() {
        let collector = OpenCodeCollector::new();
        let path_str = collector.db_path.to_string_lossy();
        assert!(path_str.contains("opencode"));
        assert!(path_str.ends_with("opencode.db"));
    }

    #[test]
    fn sanitize_db_field_removes_terminal_control_chars() {
        assert_eq!(
            sanitize_db_field("proj\u{202E}\u{0008}name", 512),
            "projname"
        );
    }

    #[test]
    fn sanitize_db_title_redacts_known_secret_prefixes() {
        assert_eq!(
            sanitize_db_title("debug sk-ant-secret-value now"),
            "debug [REDACTED] now"
        );
    }

    #[test]
    fn match_pid_short_session_dir_never_matches() {
        // Regression: previously, an empty `directory` made `cmd.contains("")`
        // always true, and `/` matched every absolute path. Both should fail
        // the length guard now, regardless of how many opencode procs run.
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        pid_commands.insert(100, "/usr/local/bin/opencode");
        pid_commands.insert(200, "/usr/local/bin/opencode --foo");
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, ""),
            None
        );
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, "/"),
            None
        );
    }

    #[test]
    fn match_pid_no_last_resort_when_cwd_and_cmdline_disagree() {
        // Regression: previously, when exactly one opencode process was
        // running, the `pid_commands.len() == 1` last-resort branch matched
        // every DB session to it, even when neither the cwd nor the command
        // line matched. With the fallback removed, this returns None.
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        // u32::MAX is a synthetic PID that has no /proc/<pid>/cwd entry,
        // so the cwd branch can't accidentally succeed.
        pid_commands.insert(u32::MAX, "/usr/local/bin/opencode");
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, "/home/u/proj-a"),
            None
        );
    }

    #[test]
    fn match_pid_substring_fallback_still_works() {
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        pid_commands.insert(u32::MAX, "node /usr/bin/opencode run --cwd=/home/u/proj-a");
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, "/home/u/proj-a"),
            Some(u32::MAX)
        );
    }

    #[test]
    fn match_pid_to_session_once_does_not_reuse_pid_for_old_rows() {
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        pid_commands.insert(u32::MAX, "node /usr/bin/opencode run --cwd=/home/u/proj-a");
        let mut claimed_pids = std::collections::HashSet::new();

        assert_eq!(
            OpenCodeCollector::match_pid_to_session_once(
                &pid_commands,
                "/home/u/proj-a",
                &mut claimed_pids,
            ),
            Some(u32::MAX)
        );
        assert_eq!(
            OpenCodeCollector::match_pid_to_session_once(
                &pid_commands,
                "/home/u/proj-a",
                &mut claimed_pids,
            ),
            None
        );
    }

    #[test]
    fn shared_service_process_is_not_a_session_pid() {
        assert!(is_shared_service(
            "/opt/homebrew/Cellar/opencode-v2/2.0.32/bin/opencode serve --service"
        ));
        assert!(!is_shared_service("opencode serve --port 4096"));
        assert!(!is_shared_service("opencode run hello"));
        let mut info = HashMap::new();
        info.insert(
            1,
            process::ProcInfo {
                pid: 1,
                ppid: 0,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "/usr/local/bin/opencode serve --service".to_string(),
            },
        );
        assert!(OpenCodeCollector::find_opencode_pids(&info).is_empty());
    }

    // ---- fixture DB tests (need the sqlite3 CLI, as the collector does) ----

    const V1_SCHEMA: &str = "
CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT, name TEXT);
CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, title TEXT, directory TEXT,
  version TEXT, time_created INTEGER, time_updated INTEGER);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
  time_updated INTEGER, data TEXT);
CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);
";

    const V2_SCHEMA: &str = "
CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT, slug TEXT, directory TEXT,
  title TEXT, version TEXT, tokens_input INTEGER DEFAULT 0, model TEXT,
  time_created INTEGER, time_updated INTEGER);
CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
  time_created INTEGER, time_updated INTEGER, data TEXT);
";

    fn v1_session(id: &str, updated: u64) -> String {
        format!(
            "INSERT INTO session VALUES ('{id}','p1','t {id}','/w/{id}','1.14.0',{updated},{updated});"
        )
    }

    fn v1_assistant(mid: &str, sid: &str, tc: u64, input: u64, model: &str) -> String {
        format!(
            "INSERT INTO message VALUES ('{mid}','{sid}',{tc},{tc},'{{\"role\":\"assistant\",\
             \"modelID\":\"{model}\",\"providerID\":\"v1prov\",\"tokens\":{{\"input\":{input},\
             \"output\":1,\"cache\":{{\"read\":2,\"write\":3}}}}}}');"
        )
    }

    fn v1_user(mid: &str, sid: &str, tc: u64) -> String {
        format!(
            "INSERT INTO message VALUES ('{mid}','{sid}',{tc},{tc},'{{\"role\":\"user\",\
             \"tokens\":{{\"input\":999999}}}}');"
        )
    }

    fn v2_session(id: &str, updated: u64) -> String {
        format!(
            "INSERT INTO session_v2 VALUES ('{id}','p1','s','/w/{id}','t {id}','2.0.32',0,\
             '{{\"id\":\"fallback-model\",\"providerID\":\"fallback-prov\"}}',{updated},{updated});"
        )
    }

    fn v2_msg(
        mid: &str,
        sid: &str,
        ty: &str,
        seq: u64,
        tc: u64,
        input: u64,
        model: &str,
    ) -> String {
        format!(
            "INSERT INTO session_message VALUES ('{mid}','{sid}','{ty}',{seq},{tc},{tc},\
             '{{\"agent\":\"build\",\"model\":{{\"id\":\"{model}\",\"providerID\":\"v2prov\"}},\
             \"content\":[],\"tokens\":{{\"input\":{input},\"output\":10,\"reasoning\":0,\
             \"cache\":{{\"read\":20,\"write\":30}}}}}}');"
        )
    }

    /// Build a fixture DB, or `None` when the sqlite3 CLI is unavailable.
    fn fixture(sql: &str) -> Option<(tempfile::TempDir, OpenCodeCollector)> {
        use std::io::Write;
        use std::process::Stdio;
        let dir = tempfile::tempdir().ok()?;
        let db = dir.path().join("opencode.db");
        let mut child = Command::new("sqlite3")
            .arg(&db)
            .stdin(Stdio::piped())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(sql.as_bytes()).ok()?;
        assert!(child.wait().ok()?.success(), "fixture SQL failed");
        let mut c = OpenCodeCollector::new();
        c.db_path = db;
        Some((dir, c))
    }

    fn by_id(rows: Vec<DbSession>) -> HashMap<String, DbSession> {
        rows.into_iter().map(|r| (r.id.clone(), r)).collect()
    }

    #[test]
    fn query_v1_only_db() {
        let sql = [
            V1_SCHEMA.to_string(),
            "INSERT INTO project VALUES ('p1','/w','proj');".into(),
            v1_session("a", 2000),
            v1_assistant("m1", "a", 100, 5, "old-model"),
            v1_assistant("m2", "a", 200, 7, "new-model"),
            v1_user("m3", "a", 150),
            v1_session("b", 1000),
        ]
        .join("\n");
        let Some((_d, c)) = fixture(&sql) else { return };
        assert!(!c.has_v2_tables());
        let rows = c.query_sessions().expect("query");
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let m = by_id(rows);
        let a = &m["a"];
        assert_eq!(a.turn_count, 2);
        assert_eq!(a.total_input, 12);
        assert_eq!(a.total_output, 2);
        assert_eq!(a.total_cache_read, 4);
        assert_eq!(a.total_cache_write, 6);
        assert_eq!(a.model, "new-model");
        assert_eq!(a.provider, "v1prov");
        assert_eq!(a.project_name, "proj");
        assert_eq!(m["b"].turn_count, 0);
        assert_eq!(m["b"].model, "");
    }

    #[test]
    fn query_v2_only_db() {
        let sql = [
            V1_SCHEMA.to_string(),
            V2_SCHEMA.to_string(),
            "INSERT INTO project VALUES ('p1','/w','proj');".into(),
            v2_session("c", 3000),
            v2_msg("x1", "c", "user", 1, 100, 999999, "ignored"),
            v2_msg("x2", "c", "assistant", 2, 200, 40, "m-old"),
            v2_msg("x3", "c", "assistant", 3, 300, 2, "m-new"),
            v2_msg("x4", "c", "compaction", 4, 400, 999999, "ignored"),
            v2_session("d", 2000),
        ]
        .join("\n");
        let Some((_d, c)) = fixture(&sql) else { return };
        assert!(c.has_v2_tables());
        let m = by_id(c.query_sessions().expect("query"));
        assert_eq!(m.len(), 2);
        let s = &m["c"];
        assert_eq!(s.turn_count, 2);
        assert_eq!(s.total_input, 42);
        assert_eq!(s.total_output, 20);
        assert_eq!(s.total_cache_read, 40);
        assert_eq!(s.total_cache_write, 60);
        assert_eq!(s.model, "m-new");
        assert_eq!(s.provider, "v2prov");
        assert_eq!(s.version, "2.0.32");
        // No assistant messages yet: fall back to session_v2.model.
        assert_eq!(m["d"].model, "fallback-model");
        assert_eq!(m["d"].provider, "fallback-prov");
    }

    #[test]
    fn query_mixed_db_does_not_double_count_migrated_sessions() {
        let sql = [
            V1_SCHEMA.to_string(),
            V2_SCHEMA.to_string(),
            "INSERT INTO project VALUES ('p1','/w','proj');".into(),
            // "dup" exists in both V1 and V2 (migrated copy): V2 wins.
            v1_session("dup", 1500),
            v1_assistant("m1", "dup", 100, 1000, "v1-model"),
            v2_session("dup", 1500),
            v2_msg("y1", "dup", "assistant", 1, 100, 5, "v2-model"),
            // V1-only session (not copied into session_v2).
            v1_session("old", 1000),
            v1_assistant("m2", "old", 100, 7, "v1-model"),
            // V2-native session.
            v2_session("new", 3000),
            v2_msg("y2", "new", "assistant", 1, 100, 9, "v2-model"),
        ]
        .join("\n");
        let Some((_d, c)) = fixture(&sql) else { return };
        let rows = c.query_sessions().expect("query");
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["new", "dup", "old"]
        );
        let m = by_id(rows);
        assert_eq!(m["dup"].total_input, 5);
        assert_eq!(m["dup"].turn_count, 1);
        assert_eq!(m["dup"].model, "v2-model");
        assert_eq!(m["old"].total_input, 7);
        assert_eq!(m["old"].model, "v1-model");
        assert_eq!(m["new"].total_input, 9);
    }

    #[test]
    fn query_limits_to_newest_sessions_across_versions() {
        let mut stmts = vec![V1_SCHEMA.to_string(), V2_SCHEMA.to_string()];
        for i in 0..MAX_SESSIONS as u64 {
            stmts.push(v1_session(&format!("v1_{i}"), 1000 + i));
            stmts.push(v2_session(&format!("v2_{i}"), 5000 + i));
        }
        let Some((_d, c)) = fixture(&stmts.join("\n")) else {
            return;
        };
        let rows = c.query_sessions().expect("query");
        assert_eq!(rows.len(), MAX_SESSIONS as usize);
        assert!(rows.iter().all(|r| r.id.starts_with("v2_")));
    }
}
