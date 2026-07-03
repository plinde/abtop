use crate::collector::{read_rate_limits, McpServer, MultiCollector};
use crate::host_info::{AgentAggregate, HostMetrics, HostSampler};
use crate::model::{AgentSession, OrphanPort, RateLimitInfo, SessionStatus};
use crate::theme::Theme;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

/// Maximum data points kept for the live token-rate graph.
const GRAPH_HISTORY_LEN: usize = 200;
/// Max concurrent summary jobs.
const MAX_SUMMARY_JOBS: usize = 3;
/// Max summary attempts per session before giving up.
const MAX_SUMMARY_RETRIES: u32 = 2;

/// Produce a terminal-safe fallback summary from a raw prompt.
fn sanitize_fallback(prompt: &str, max_len: usize) -> String {
    prompt
        .chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .take(max_len)
        .collect()
}

/// Outcome of an Enter-key jump attempt. Distinct from `Option<String>` so
/// callers (notably `--exit-on-jump`) can tell a real terminal jump apart from
/// a no-op (unsupported terminal, or empty session list).
#[derive(Debug, PartialEq, Eq)]
pub enum JumpOutcome {
    /// Actually switched to a terminal pane/tab/window.
    Jumped,
    /// Tried to jump through an applicable backend, but the focus command failed.
    Failed(String),
    /// Unsupported terminal, or nothing selected — nothing happened.
    NoOp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowTab {
    Work,
    Usage,
    System,
}

impl NarrowTab {
    pub const ALL: [Self; 3] = [Self::Work, Self::Usage, Self::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "Work",
            Self::Usage => "Usage",
            Self::System => "System",
        }
    }

    pub fn shortcut(self) -> char {
        match self {
            Self::Work => 'w',
            Self::Usage => 'u',
            Self::System => 's',
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowSection {
    Sessions,
    Projects,
    Context,
    Quota,
    Tokens,
    Ports,
    Mcp,
}

impl NarrowSection {
    pub fn tab(self) -> NarrowTab {
        match self {
            Self::Sessions | Self::Projects => NarrowTab::Work,
            Self::Context | Self::Quota | Self::Tokens => NarrowTab::Usage,
            Self::Ports | Self::Mcp => NarrowTab::System,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSortColumn {
    Ai,
    Recent,
    Pid,
    Project,
    Session,
    Config,
    Summary,
    Status,
    Model,
    Context,
    Tokens,
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Memory,
    Turn,
    Everything,
    Branch,
    Version,
    Cwd,
    Effort,
}

impl SessionSortColumn {
    pub const ALL: [Self; 22] = [
        Self::Ai,
        Self::Recent,
        Self::Pid,
        Self::Project,
        Self::Session,
        Self::Config,
        Self::Summary,
        Self::Status,
        Self::Model,
        Self::Context,
        Self::Tokens,
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::Memory,
        Self::Turn,
        Self::Everything,
        Self::Branch,
        Self::Version,
        Self::Cwd,
        Self::Effort,
    ];

    pub const DEFAULT_COLUMNS: [Self; 18] = [
        Self::Ai,
        Self::Recent,
        Self::Pid,
        Self::Project,
        Self::Session,
        Self::Config,
        Self::Summary,
        Self::Status,
        Self::Model,
        Self::Context,
        Self::Tokens,
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::Memory,
        Self::Turn,
        Self::Everything,
    ];

    fn default_ascending(self) -> bool {
        match self {
            Self::Context
            | Self::Recent
            | Self::Tokens
            | Self::Input
            | Self::Output
            | Self::CacheRead
            | Self::CacheWrite
            | Self::Memory
            | Self::Turn
            | Self::Everything => false,
            Self::Ai
            | Self::Pid
            | Self::Project
            | Self::Session
            | Self::Config
            | Self::Summary
            | Self::Status
            | Self::Model
            | Self::Branch
            | Self::Version
            | Self::Cwd
            | Self::Effort => true,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Ai => "ai",
            Self::Recent => "recent",
            Self::Pid => "pid",
            Self::Project => "project",
            Self::Session => "session",
            Self::Config => "config",
            Self::Summary => "summary",
            Self::Status => "status",
            Self::Model => "model",
            Self::Context => "context",
            Self::Tokens => "tokens",
            Self::Input => "input",
            Self::Output => "output",
            Self::CacheRead => "cache_r",
            Self::CacheWrite => "cache_w",
            Self::Memory => "memory",
            Self::Turn => "turn",
            Self::Everything => "everything",
            Self::Branch => "branch",
            Self::Version => "version",
            Self::Cwd => "cwd",
            Self::Effort => "effort",
        }
    }

    pub fn from_id(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ai" => Some(Self::Ai),
            "recent" | "last" | "last_turn" | "last_turn_at" | "activity" => Some(Self::Recent),
            "pid" => Some(Self::Pid),
            "project" => Some(Self::Project),
            "session" | "session_id" | "sess" => Some(Self::Session),
            "config" | "cfg" => Some(Self::Config),
            "summary" => Some(Self::Summary),
            "status" => Some(Self::Status),
            "model" => Some(Self::Model),
            "context" | "ctx" => Some(Self::Context),
            "tokens" | "active_tokens" => Some(Self::Tokens),
            "input" | "input_tokens" => Some(Self::Input),
            "output" | "output_tokens" => Some(Self::Output),
            "cache_r" | "cache_read" | "cache_read_tokens" => Some(Self::CacheRead),
            "cache_w" | "cache_write" | "cache_create" | "cache_creation_tokens" => {
                Some(Self::CacheWrite)
            }
            "memory" | "mem" => Some(Self::Memory),
            "turn" | "turns" => Some(Self::Turn),
            "everything" | "total" | "total_tokens" => Some(Self::Everything),
            "branch" | "git_branch" => Some(Self::Branch),
            "version" => Some(Self::Version),
            "cwd" | "path" => Some(Self::Cwd),
            "effort" => Some(Self::Effort),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ai => "AI",
            Self::Recent => "Recent",
            Self::Pid => "Pid",
            Self::Project => "Project",
            Self::Session => "Session",
            Self::Config => "Config",
            Self::Summary => "Summary",
            Self::Status => "Status",
            Self::Model => "Model",
            Self::Context => "Context",
            Self::Tokens => "Tokens",
            Self::Input => "Input",
            Self::Output => "Output",
            Self::CacheRead => "CacheR",
            Self::CacheWrite => "CacheW",
            Self::Memory => "Memory",
            Self::Turn => "Turn",
            Self::Everything => "Everything",
            Self::Branch => "Branch",
            Self::Version => "Version",
            Self::Cwd => "Cwd",
            Self::Effort => "Effort",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionSort {
    pub column: SessionSortColumn,
    pub ascending: bool,
}

pub struct App {
    pub sessions: Vec<AgentSession>,
    pub selected: usize,
    pub session_sort: Option<SessionSort>,
    pub session_sort_mode: bool,
    pub session_columns: Vec<SessionSortColumn>,
    pub should_quit: bool,
    /// Token rate per tick (delta). Ring buffer for the braille graph.
    pub token_rates: VecDeque<f64>,
    /// Account-level rate limits (Claude, Codex, etc.)
    pub rate_limits: Vec<RateLimitInfo>,
    /// Per-session previous token totals, keyed by (agent_cli, session_id).
    prev_tokens: HashMap<(String, String), u64>,
    /// Rate limit poll counter (read every 5 ticks = 10s)
    rate_limit_counter: u32,
    collector: MultiCollector,
    /// Cached LLM-generated summaries, keyed by session_id.
    pub summaries: HashMap<String, String>,
    /// Session IDs currently being summarized.
    pending_summaries: HashSet<String>,
    /// Per-session retry count for failed summary attempts.
    summary_retries: HashMap<String, u32>,
    /// Channel to receive completed summaries from background threads.
    /// Tuple: (session_id, prompt, maybe_summary).
    summary_rx: mpsc::Receiver<(String, String, Option<String>)>,
    summary_tx: mpsc::Sender<(String, String, Option<String>)>,
    /// Ports left open by processes whose parent sessions have ended.
    pub orphan_ports: Vec<OrphanPort>,
    /// Transient status message shown in the footer (auto-clears after 3s).
    pub status_msg: Option<(String, Instant)>,
    /// Kill confirmation: (selected_index, timestamp). Expires after 2s.
    kill_confirm: Option<(usize, Instant)>,
    pub theme: Theme,
    pub show_context: bool,
    pub show_quota: bool,
    pub show_tokens: bool,
    pub show_projects: bool,
    pub show_ports: bool,
    pub show_sessions: bool,
    pub show_mcp: bool,
    pub narrow_tab: NarrowTab,
    pub active_narrow_section: Option<NarrowSection>,
    pub maximized_narrow_section: Option<NarrowSection>,
    /// MCP servers detected on the most recent tick (sourced from
    /// MultiCollector). Populated regardless of `show_mcp` so panel
    /// toggling doesn't cost a discovery roundtrip.
    pub mcp_servers: Vec<McpServer>,
    /// When true (default), mcp-server-owned rollouts are hidden from
    /// the sessions panel. Toggle with Shift+M.
    pub mcp_suppress_sessions: bool,
    pub config_open: bool,
    pub config_selected: usize,
    pub tree_view: bool,
    /// When true, `t` toggles tree_view instead of cycling themes.
    pub lock_theme: bool,
    /// Section under the mouse cursor (None when nothing hovered).
    pub hovered_section: Option<NarrowSection>,
    /// Additional dirs scanned for abtop-rate-limits.json (from config claude_config_dirs).
    rate_limit_dirs: Vec<PathBuf>,
    pub filter_text: String,
    pub filter_active: bool,
    pub show_timeline: bool,
    pub timeline_scroll: usize,
    pub show_file_audit: bool,
    /// Host vitals sampler (CPU% delta needs prior snapshot).
    host_sampler: HostSampler,
    /// Latest host metrics snapshot (None until first valid sample).
    pub host_metrics: Option<HostMetrics>,
    /// Aggregate metrics across all sessions (recomputed each tick).
    pub agent_aggregate: AgentAggregate,
    /// Help overlay (`?`) visibility.
    pub help_open: bool,
    /// View leader overlay (`v`) visibility.
    pub view_open: bool,
}

impl App {
    #[cfg(test)]
    pub fn new_with_config(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
    ) -> Self {
        Self::new_with_config_and_claude_dirs(theme, hidden_agents, panels, &[], false)
    }

    pub fn new_with_config_and_claude_dirs(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
        claude_config_dirs: &[PathBuf],
        lock_theme: bool,
    ) -> Self {
        Self::new_with_config_and_claude_dirs_and_columns(
            theme,
            hidden_agents,
            panels,
            claude_config_dirs,
            lock_theme,
            &[],
        )
    }

    pub fn new_with_config_and_claude_dirs_and_columns(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
        claude_config_dirs: &[PathBuf],
        lock_theme: bool,
        session_columns: &[String],
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let summaries = load_summary_cache();
        let mut collector =
            MultiCollector::with_hidden_and_claude_config_dirs(hidden_agents, claude_config_dirs);
        collector.set_mcp_suppress(true);
        Self {
            sessions: Vec::new(),
            selected: 0,
            session_sort: None,
            session_sort_mode: false,
            session_columns: normalize_session_columns(session_columns),
            should_quit: false,
            token_rates: VecDeque::with_capacity(GRAPH_HISTORY_LEN),
            rate_limits: Vec::new(),
            prev_tokens: HashMap::new(),
            rate_limit_counter: 5,
            collector,
            summaries,
            pending_summaries: HashSet::new(),
            summary_retries: HashMap::new(),
            summary_rx: rx,
            summary_tx: tx,
            orphan_ports: Vec::new(),
            status_msg: None,
            kill_confirm: None,
            theme,
            show_context: panels.context,
            show_quota: panels.quota,
            show_tokens: panels.tokens,
            show_projects: panels.projects,
            show_ports: panels.ports,
            show_sessions: panels.sessions,
            show_mcp: panels.mcp,
            narrow_tab: NarrowTab::Work,
            active_narrow_section: Some(NarrowSection::Sessions),
            maximized_narrow_section: None,
            mcp_servers: Vec::new(),
            mcp_suppress_sessions: true,
            config_open: false,
            config_selected: 0,
            tree_view: false,
            lock_theme,
            hovered_section: None,
            rate_limit_dirs: claude_config_dirs.to_vec(),
            filter_text: String::new(),
            filter_active: false,
            show_timeline: false,
            timeline_scroll: 0,
            show_file_audit: false,
            host_sampler: HostSampler::new(),
            host_metrics: None,
            agent_aggregate: AgentAggregate::default(),
            help_open: false,
            view_open: false,
        }
    }

    pub fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
        if self.help_open {
            self.view_open = false;
        }
    }

    pub fn toggle_view_menu(&mut self) {
        self.view_open = !self.view_open;
        if self.view_open {
            self.help_open = false;
        }
    }

    pub fn toggle_panel(&mut self, panel: u8) {
        match panel {
            1 => self.show_context = !self.show_context,
            2 => self.show_quota = !self.show_quota,
            3 => self.show_tokens = !self.show_tokens,
            4 => self.show_projects = !self.show_projects,
            5 => self.show_ports = !self.show_ports,
            6 => self.show_sessions = !self.show_sessions,
            7 => self.show_mcp = !self.show_mcp,
            _ => return,
        }
        self.persist_panel_visibility();
        self.clamp_narrow_tab();
    }

    /// Toggle whether mcp-server-owned rollouts are hidden from the
    /// sessions panel. Default is on; turning it off restores upstream
    /// behavior so the user can see exactly what mcp-server fd holding
    /// produces (mostly stale "Done" rows).
    pub fn toggle_mcp_session_suppression(&mut self) {
        self.mcp_suppress_sessions = !self.mcp_suppress_sessions;
        let label = if self.mcp_suppress_sessions {
            "on"
        } else {
            "off"
        };
        self.set_status(format!("mcp session suppression: {}", label));
    }

    fn persist_panel_visibility(&mut self) {
        let panels = crate::config::PanelVisibility {
            context: self.show_context,
            quota: self.show_quota,
            tokens: self.show_tokens,
            projects: self.show_projects,
            ports: self.show_ports,
            sessions: self.show_sessions,
            mcp: self.show_mcp,
        };
        if let Err(e) = crate::config::save_panel_visibility(&panels) {
            self.set_status(format!("panels save failed: {}", e));
        }
    }

    pub fn toggle_file_audit(&mut self) {
        self.show_file_audit = !self.show_file_audit;
    }

    pub fn toggle_config(&mut self) {
        self.config_open = !self.config_open;
        if self.config_open {
            self.config_selected = 0;
        }
    }

    pub fn config_item_count(&self) -> usize {
        8 + SessionSortColumn::ALL.len() // theme + 7 panel toggles + session columns
    }

    pub fn config_select_next(&mut self) {
        if self.config_selected + 1 < self.config_item_count() {
            self.config_selected += 1;
        }
    }

    pub fn config_select_prev(&mut self) {
        self.config_selected = self.config_selected.saturating_sub(1);
    }

    pub fn config_toggle_selected(&mut self) {
        match self.config_selected {
            0 => {
                self.cycle_theme();
                return;
            }
            1 => self.show_context = !self.show_context,
            2 => self.show_quota = !self.show_quota,
            3 => self.show_tokens = !self.show_tokens,
            4 => self.show_projects = !self.show_projects,
            5 => self.show_ports = !self.show_ports,
            6 => self.show_sessions = !self.show_sessions,
            7 => self.show_mcp = !self.show_mcp,
            idx => {
                let column_idx = idx.saturating_sub(8);
                let Some(&column) = SessionSortColumn::ALL.get(column_idx) else {
                    return;
                };
                self.toggle_session_column(column);
                return;
            }
        }
        self.persist_panel_visibility();
        self.clamp_narrow_tab();
    }

    pub fn session_column_enabled(&self, column: SessionSortColumn) -> bool {
        self.session_columns.contains(&column)
    }

    pub fn toggle_session_column(&mut self, column: SessionSortColumn) {
        if let Some(pos) = self.session_columns.iter().position(|&c| c == column) {
            if self.session_columns.len() == 1 {
                self.set_status("keep at least one session column".to_string());
                return;
            }
            self.session_columns.remove(pos);
        } else {
            self.session_columns.push(column);
            self.session_columns
                .sort_by_key(|column| session_column_order(*column));
        }
        self.persist_session_columns();
    }

    fn persist_session_columns(&mut self) {
        if let Err(e) = crate::config::save_session_columns(&self.session_columns) {
            self.set_status(format!("columns save failed: {}", e));
        }
    }

    pub fn narrow_tab_visible(&self, tab: NarrowTab) -> bool {
        match tab {
            NarrowTab::Work => self.show_sessions || self.show_projects,
            NarrowTab::Usage => self.show_context || self.show_quota || self.show_tokens,
            NarrowTab::System => self.show_ports || self.show_mcp,
        }
    }

    pub fn visible_narrow_tabs(&self) -> Vec<NarrowTab> {
        NarrowTab::ALL
            .into_iter()
            .filter(|&tab| self.narrow_tab_visible(tab))
            .collect()
    }

    pub fn active_narrow_tab(&self) -> Option<NarrowTab> {
        if self.narrow_tab_visible(self.narrow_tab) {
            Some(self.narrow_tab)
        } else {
            NarrowTab::ALL
                .into_iter()
                .find(|&tab| self.narrow_tab_visible(tab))
        }
    }

    pub fn set_narrow_tab(&mut self, tab: NarrowTab) {
        if self.narrow_tab_visible(tab) {
            self.narrow_tab = tab;
            self.clamp_narrow_section();
        }
    }

    pub fn select_next_narrow_tab(&mut self) {
        let tabs = self.visible_narrow_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = self.active_narrow_tab().unwrap_or(tabs[0]);
        let pos = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        self.narrow_tab = tabs[(pos + 1) % tabs.len()];
        self.clamp_narrow_section();
    }

    pub fn select_prev_narrow_tab(&mut self) {
        let tabs = self.visible_narrow_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = self.active_narrow_tab().unwrap_or(tabs[0]);
        let pos = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        self.narrow_tab = tabs[(pos + tabs.len() - 1) % tabs.len()];
        self.clamp_narrow_section();
    }

    fn clamp_narrow_tab(&mut self) {
        if let Some(tab) = self.active_narrow_tab() {
            self.narrow_tab = tab;
        }
        self.clamp_narrow_section();
    }

    pub fn narrow_section_visible(&self, section: NarrowSection) -> bool {
        match section {
            NarrowSection::Sessions => self.show_sessions,
            NarrowSection::Projects => self.show_projects,
            NarrowSection::Context => self.show_context,
            NarrowSection::Quota => self.show_quota,
            NarrowSection::Tokens => self.show_tokens,
            NarrowSection::Ports => self.show_ports,
            NarrowSection::Mcp => self.show_mcp,
        }
    }

    pub fn visible_narrow_sections(&self, tab: NarrowTab) -> Vec<NarrowSection> {
        let sections: &[NarrowSection] = match tab {
            NarrowTab::Work => &[NarrowSection::Sessions, NarrowSection::Projects],
            NarrowTab::Usage => &[
                NarrowSection::Context,
                NarrowSection::Quota,
                NarrowSection::Tokens,
            ],
            NarrowTab::System => &[NarrowSection::Ports, NarrowSection::Mcp],
        };
        sections
            .iter()
            .copied()
            .filter(|&section| self.narrow_section_visible(section))
            .collect()
    }

    pub fn active_narrow_section(&self) -> Option<NarrowSection> {
        let tab = self.active_narrow_tab()?;
        if let Some(section) = self.active_narrow_section {
            if section.tab() == tab && self.narrow_section_visible(section) {
                return Some(section);
            }
        }
        self.visible_narrow_sections(tab).into_iter().next()
    }

    pub fn set_active_narrow_section(&mut self, section: NarrowSection) {
        if self.narrow_section_visible(section) {
            self.narrow_tab = section.tab();
            self.active_narrow_section = Some(section);
            self.clamp_narrow_section();
        }
    }

    const SECTION_ORDER: &[NarrowSection] = &[
        NarrowSection::Context,
        NarrowSection::Quota,
        NarrowSection::Tokens,
        NarrowSection::Projects,
        NarrowSection::Ports,
        NarrowSection::Sessions,
        NarrowSection::Mcp,
    ];

    pub fn select_next_section(&mut self) {
        let current = self.active_narrow_section().unwrap_or(NarrowSection::Context);
        let pos = Self::SECTION_ORDER
            .iter()
            .position(|s| *s == current)
            .unwrap_or(0);
        for offset in 1..=Self::SECTION_ORDER.len() {
            let candidate = Self::SECTION_ORDER[(pos + offset) % Self::SECTION_ORDER.len()];
            if self.narrow_section_visible(candidate) {
                self.set_active_narrow_section(candidate);
                return;
            }
        }
    }

    pub fn select_prev_section(&mut self) {
        let current = self.active_narrow_section().unwrap_or(NarrowSection::Context);
        let pos = Self::SECTION_ORDER
            .iter()
            .position(|s| *s == current)
            .unwrap_or(0);
        let len = Self::SECTION_ORDER.len();
        for offset in 1..=len {
            let candidate = Self::SECTION_ORDER[(pos + len - offset) % len];
            if self.narrow_section_visible(candidate) {
                self.set_active_narrow_section(candidate);
                return;
            }
        }
    }

    pub fn maximized_narrow_section(&self) -> Option<NarrowSection> {
        let section = self.maximized_narrow_section?;
        if self.active_narrow_tab() == Some(section.tab()) && self.narrow_section_visible(section) {
            Some(section)
        } else {
            None
        }
    }

    pub fn toggle_narrow_section_zoom(&mut self, section: NarrowSection) {
        if !self.narrow_section_visible(section) {
            return;
        }
        self.set_active_narrow_section(section);
        self.maximized_narrow_section = if self.maximized_narrow_section() == Some(section) {
            None
        } else {
            Some(section)
        };
    }

    pub fn maximize_active_narrow_section(&mut self) {
        if let Some(section) = self.active_narrow_section() {
            self.maximized_narrow_section = Some(section);
        }
    }

    pub fn restore_narrow_sections(&mut self) {
        self.maximized_narrow_section = None;
    }

    fn clamp_narrow_section(&mut self) {
        self.active_narrow_section = self.active_narrow_section();
        if self.maximized_narrow_section().is_none() {
            self.maximized_narrow_section = None;
        }
    }

    pub fn toggle_timeline(&mut self) {
        self.show_timeline = !self.show_timeline;
        self.timeline_scroll = 0;
    }

    pub fn cycle_theme(&mut self) {
        let names = crate::theme::THEME_NAMES;
        let current = names
            .iter()
            .position(|&n| n == self.theme.name)
            .unwrap_or(0);
        let next = (current + 1) % names.len();
        self.theme = Theme::by_name(names[next]).unwrap_or_default();
        if let Err(e) = crate::config::save_theme(names[next]) {
            self.set_status(format!("theme: {} (save failed: {})", names[next], e));
        } else {
            self.set_status(format!("theme: {}", names[next]));
        }
    }

    /// Set a transient status message that auto-clears after 3 seconds.
    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }

    /// Full refresh used by the TUI: collect monitored data, then generate and
    /// retry session summaries. Equivalent to [`App::tick_no_summaries`] followed
    /// by [`App::drain_and_retry_summaries`].
    pub fn tick(&mut self) {
        self.tick_no_summaries();
        self.drain_and_retry_summaries();
    }

    /// Refresh all monitored data WITHOUT spawning background summary jobs.
    ///
    /// `tick` additionally calls [`App::drain_and_retry_summaries`], which
    /// shells out to `claude --print` to generate session titles. Headless
    /// consumers (e.g. the web snapshot API) call this variant so they never
    /// spawn subprocesses or consume the user's Claude quota.
    pub fn tick_no_summaries(&mut self) {
        let selected_key = self
            .sessions
            .get(self.selected)
            .map(|s| (s.agent_cli, s.session_id.clone(), s.pid));
        self.collector.set_mcp_suppress(self.mcp_suppress_sessions);
        self.sessions = self.collector.collect();
        self.orphan_ports = self.collector.orphan_ports.clone();
        self.mcp_servers = self.collector.mcp_servers.clone();
        self.host_metrics = self.host_sampler.sample();
        self.agent_aggregate = AgentAggregate::from_sessions(&self.sessions);
        if let Some((agent_cli, session_id, pid)) = selected_key {
            if let Some(idx) = self.sessions.iter().position(|s| {
                s.agent_cli == agent_cli && s.session_id == session_id && s.pid == pid
            }) {
                self.selected = idx;
            }
        }
        if self.selected >= self.sessions.len() && !self.sessions.is_empty() {
            self.selected = self.visible_indices().first().copied().unwrap_or(0);
        }
        self.clamp_selection_to_visible();

        // Compute rate as sum of per-session deltas (stable across session churn).
        // Update prev_tokens in place; stale entries are harmless (bounded by
        // total unique sessions ever seen) and keeping them avoids false spikes
        // when a session transiently disappears from one poll.
        let mut rate: f64 = 0.0;
        for s in &self.sessions {
            let key = (s.agent_cli.to_string(), s.session_id.clone());
            let total = s.active_tokens();
            let prev = self.prev_tokens.get(&key).copied().unwrap_or(total);
            rate += total.saturating_sub(prev) as f64;
            self.prev_tokens.insert(key, total);
        }

        self.token_rates.push_back(rate);
        if self.token_rates.len() > GRAPH_HISTORY_LEN {
            self.token_rates.pop_front();
        }

        // Poll rate limits: first tick immediately, then every 5 ticks ≈ 10s
        if self.rate_limits.is_empty() || self.rate_limit_counter >= 5 {
            self.rate_limit_counter = 0;
            let mut extra_dirs = self.collector.all_config_dirs();
            extra_dirs.extend_from_slice(&self.rate_limit_dirs);
            self.rate_limits = read_rate_limits(&extra_dirs);
            // Merge live rate limits from agent collectors (e.g. Codex JSONL parsing)
            self.rate_limits.extend(self.collector.agent_rate_limits());
        } else {
            self.rate_limit_counter += 1;
        }

        promote_waiting_to_rate_limited(&mut self.sessions, &self.rate_limits);
    }

    /// Drain completed summary results and spawn retries. Does NOT recollect
    /// sessions, so it is safe for `--once` mode (stable snapshot).
    pub fn drain_and_retry_summaries(&mut self) {
        while let Ok((sid, prompt, maybe_summary)) = self.summary_rx.try_recv() {
            self.pending_summaries.remove(&sid);
            match maybe_summary {
                Some(summary) => {
                    self.summary_retries.remove(&sid);
                    self.summaries.insert(sid, summary);
                    save_summary_cache(&self.summaries);
                }
                None => {
                    let count = self.summary_retries.entry(sid.clone()).or_insert(0);
                    *count += 1;
                    if *count >= MAX_SUMMARY_RETRIES {
                        // Exhausted — store sanitized fallback using prompt from worker
                        self.summaries.insert(sid, sanitize_fallback(&prompt, 80));
                        save_summary_cache(&self.summaries);
                    }
                }
            }
        }

        // Spawn summary jobs for sessions that need one
        for s in &self.sessions {
            let retries = self
                .summary_retries
                .get(&s.session_id)
                .copied()
                .unwrap_or(0);
            let has_input = !s.initial_prompt.is_empty() || !s.first_assistant_text.is_empty();
            if has_input
                && !self.summaries.contains_key(&s.session_id)
                && !self.pending_summaries.contains(&s.session_id)
                && self.pending_summaries.len() < MAX_SUMMARY_JOBS
                && retries < MAX_SUMMARY_RETRIES
            {
                self.pending_summaries.insert(s.session_id.clone());
                let sid = s.session_id.clone();
                let prompt = s.initial_prompt.clone();
                let assistant_text = s.first_assistant_text.clone();
                let tx = self.summary_tx.clone();
                std::thread::spawn(move || {
                    let result = generate_summary(&prompt, &assistant_text);
                    let fallback_text = if prompt.is_empty() {
                        assistant_text
                    } else {
                        prompt
                    };
                    let _ = tx.send((sid, fallback_text, result));
                });
            }
        }
    }

    pub fn has_pending_summaries(&self) -> bool {
        !self.pending_summaries.is_empty()
    }

    /// True if any session still qualifies for a summary retry.
    pub fn has_retryable_summaries(&self) -> bool {
        self.sessions.iter().any(|s| {
            (!s.initial_prompt.is_empty() || !s.first_assistant_text.is_empty())
                && !self.summaries.contains_key(&s.session_id)
                && !self.pending_summaries.contains(&s.session_id)
                && self
                    .summary_retries
                    .get(&s.session_id)
                    .copied()
                    .unwrap_or(0)
                    < MAX_SUMMARY_RETRIES
        })
    }

    /// Returns indices of sessions matching the current filter.
    pub fn visible_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = if self.filter_text.is_empty() {
            (0..self.sessions.len()).collect()
        } else {
            let query = self.filter_text.to_lowercase();
            self.sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| Self::session_matches(s, &query))
                .map(|(i, _)| i)
                .collect()
        };

        if let Some(sort) = self.session_sort {
            indices.sort_by(|&a, &b| self.compare_sessions(a, b, sort));
        }

        indices
    }

    fn compare_sessions(&self, a: usize, b: usize, sort: SessionSort) -> Ordering {
        let lhs = &self.sessions[a];
        let rhs = &self.sessions[b];
        let mut ord = match sort.column {
            SessionSortColumn::Ai => cmp_str(lhs.agent_cli, rhs.agent_cli),
            SessionSortColumn::Recent => lhs.last_turn_at.cmp(&rhs.last_turn_at),
            SessionSortColumn::Pid => lhs.pid.cmp(&rhs.pid),
            SessionSortColumn::Project => cmp_str(&lhs.project_name, &rhs.project_name),
            SessionSortColumn::Session => cmp_str(&lhs.session_id, &rhs.session_id),
            SessionSortColumn::Config => cmp_str(&lhs.config_root, &rhs.config_root),
            SessionSortColumn::Summary => {
                cmp_str(&self.session_summary(lhs), &self.session_summary(rhs))
            }
            SessionSortColumn::Status => status_rank(&lhs.status).cmp(&status_rank(&rhs.status)),
            SessionSortColumn::Model => cmp_str(&lhs.model, &rhs.model),
            SessionSortColumn::Context => lhs.context_percent.total_cmp(&rhs.context_percent),
            SessionSortColumn::Tokens => lhs.active_tokens().cmp(&rhs.active_tokens()),
            SessionSortColumn::Input => lhs.total_input_tokens.cmp(&rhs.total_input_tokens),
            SessionSortColumn::Output => lhs.total_output_tokens.cmp(&rhs.total_output_tokens),
            SessionSortColumn::CacheRead => lhs.total_cache_read.cmp(&rhs.total_cache_read),
            SessionSortColumn::CacheWrite => lhs.total_cache_create.cmp(&rhs.total_cache_create),
            SessionSortColumn::Memory => lhs.mem_mb.cmp(&rhs.mem_mb),
            SessionSortColumn::Turn => lhs.turn_count.cmp(&rhs.turn_count),
            SessionSortColumn::Everything => lhs.total_tokens().cmp(&rhs.total_tokens()),
            SessionSortColumn::Branch => cmp_str(&lhs.git_branch, &rhs.git_branch),
            SessionSortColumn::Version => cmp_str(&lhs.version, &rhs.version),
            SessionSortColumn::Cwd => cmp_str(&lhs.cwd, &rhs.cwd),
            SessionSortColumn::Effort => cmp_str(&lhs.effort, &rhs.effort),
        };

        if !sort.ascending {
            ord = ord.reverse();
        }

        ord.then_with(|| rhs.started_at.cmp(&lhs.started_at))
            .then_with(|| lhs.agent_cli.cmp(rhs.agent_cli))
            .then_with(|| lhs.session_id.cmp(&rhs.session_id))
            .then_with(|| lhs.pid.cmp(&rhs.pid))
    }

    pub fn toggle_session_sort(&mut self, column: SessionSortColumn) {
        let next = match self.session_sort {
            Some(current) if current.column == column => SessionSort {
                column,
                ascending: !current.ascending,
            },
            _ => SessionSort {
                column,
                ascending: column.default_ascending(),
            },
        };
        self.session_sort = Some(next);
        self.clamp_selection_to_visible();
        let direction = if next.ascending { "ascending" } else { "descending" };
        self.set_status(format!("sort: {} {}", column.label(), direction));
    }

    pub fn toggle_session_sort_mode(&mut self) {
        self.session_sort_mode = !self.session_sort_mode;
        if self.session_sort_mode && self.session_sort.is_none() {
            self.set_session_sort(
                SessionSortColumn::Recent,
                SessionSortColumn::Recent.default_ascending(),
            );
        } else if self.session_sort_mode {
            self.set_sort_status();
        }
    }

    pub fn close_session_sort_mode(&mut self) {
        self.session_sort_mode = false;
    }

    pub fn select_next_session_sort_column(&mut self) {
        self.select_next_session_sort_column_from(&SessionSortColumn::ALL);
    }

    pub fn select_prev_session_sort_column(&mut self) {
        self.select_prev_session_sort_column_from(&SessionSortColumn::ALL);
    }

    pub fn select_next_session_sort_column_from(&mut self, columns: &[SessionSortColumn]) {
        self.shift_session_sort_column(columns, 1);
    }

    pub fn select_prev_session_sort_column_from(&mut self, columns: &[SessionSortColumn]) {
        self.shift_session_sort_column(columns, -1);
    }

    pub fn ensure_session_sort_column_in(&mut self, columns: &[SessionSortColumn]) {
        if columns.is_empty() {
            return;
        }
        let current = self.session_sort.map(|sort| sort.column);
        if current.is_none_or(|column| !columns.contains(&column)) {
            let column = columns[0];
            self.set_session_sort(column, column.default_ascending());
        }
    }

    fn shift_session_sort_column(&mut self, columns: &[SessionSortColumn], delta: isize) {
        if columns.is_empty() {
            return;
        }
        let current = self.session_sort.unwrap_or(SessionSort {
            column: SessionSortColumn::Recent,
            ascending: SessionSortColumn::Recent.default_ascending(),
        });
        let pos = columns
            .iter()
            .position(|&column| column == current.column)
            .unwrap_or_else(|| {
                if delta >= 0 {
                    0
                } else {
                    columns.len().saturating_sub(1)
                }
            });
        let len = columns.len() as isize;
        let next = (pos as isize + delta).rem_euclid(len) as usize;
        let column = columns[next];
        self.set_session_sort(column, column.default_ascending());
    }

    pub fn set_session_sort_ascending(&mut self) {
        let column = self
            .session_sort
            .map(|sort| sort.column)
            .unwrap_or(SessionSortColumn::Recent);
        self.set_session_sort(column, true);
    }

    pub fn set_session_sort_descending(&mut self) {
        let column = self
            .session_sort
            .map(|sort| sort.column)
            .unwrap_or(SessionSortColumn::Recent);
        self.set_session_sort(column, false);
    }

    fn set_session_sort(&mut self, column: SessionSortColumn, ascending: bool) {
        self.session_sort = Some(SessionSort { column, ascending });
        self.clamp_selection_to_visible();
        self.set_sort_status();
    }

    fn set_sort_status(&mut self) {
        if let Some(sort) = self.session_sort {
            let direction = if sort.ascending {
                "ascending"
            } else {
                "descending"
            };
            self.set_status(format!("sort: {} {}", sort.column.label(), direction));
        }
    }

    pub fn cycle_session_sort_column(&mut self) {
        let next_column = match self.session_sort {
            None => SessionSortColumn::Ai,
            Some(sort) => {
                let pos = SessionSortColumn::ALL
                    .iter()
                    .position(|&column| column == sort.column)
                    .unwrap_or(0);
                SessionSortColumn::ALL[(pos + 1) % SessionSortColumn::ALL.len()]
            }
        };
        self.session_sort = Some(SessionSort {
            column: next_column,
            ascending: next_column.default_ascending(),
        });
        self.clamp_selection_to_visible();
        let sort = self.session_sort.unwrap();
        let direction = if sort.ascending {
            "ascending"
        } else {
            "descending"
        };
        self.set_status(format!("sort: {} {}", sort.column.label(), direction));
    }

    pub fn reverse_session_sort(&mut self) {
        let mut sort = self.session_sort.unwrap_or(SessionSort {
            column: SessionSortColumn::Ai,
            ascending: SessionSortColumn::Ai.default_ascending(),
        });
        sort.ascending = !sort.ascending;
        self.session_sort = Some(sort);
        self.clamp_selection_to_visible();
        let direction = if sort.ascending { "ascending" } else { "descending" };
        self.set_status(format!("sort: {} {}", sort.column.label(), direction));
    }

    pub fn session_sort_indicator(&self, column: SessionSortColumn) -> Option<&'static str> {
        match self.session_sort {
            Some(sort) if sort.column == column && sort.ascending => Some("↑"),
            Some(sort) if sort.column == column => Some("↓"),
            _ => None,
        }
    }

    fn session_matches(s: &AgentSession, query: &str) -> bool {
        s.project_name.to_lowercase().contains(query)
            || s.model.to_lowercase().contains(query)
            || s.session_id.to_lowercase().contains(query)
            || s.initial_prompt.to_lowercase().contains(query)
            || s.cwd.to_lowercase().contains(query)
            || s.config_root.to_lowercase().contains(query)
            || format!("{:?}", s.status).to_lowercase().contains(query)
    }

    /// Ensure `selected` points to a session included in the current filter.
    /// No-op when no sessions match; otherwise snaps to the first visible.
    fn clamp_selection_to_visible(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if !visible.contains(&self.selected) {
            self.selected = visible[0];
        }
    }

    pub fn filter_push(&mut self, c: char) {
        self.filter_text.push(c);
        self.clamp_selection_to_visible();
    }

    pub fn filter_pop(&mut self) {
        self.filter_text.pop();
        self.clamp_selection_to_visible();
    }

    pub fn clear_filter(&mut self) {
        self.filter_active = false;
        self.filter_text.clear();
    }

    pub fn select_next(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if let Some(pos) = visible.iter().position(|&i| i == self.selected) {
            if pos + 1 < visible.len() {
                self.selected = visible[pos + 1];
            }
        } else {
            self.selected = visible[0];
        }
    }

    pub fn select_prev(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if let Some(pos) = visible.iter().position(|&i| i == self.selected) {
            if pos > 0 {
                self.selected = visible[pos - 1];
            }
        } else {
            self.selected = *visible.last().unwrap();
        }
    }

    pub fn select_session(&mut self, index: usize) {
        if index < self.sessions.len() && self.visible_indices().contains(&index) {
            self.selected = index;
        }
    }

    pub fn kill_selected(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let session = &self.sessions[self.selected];
        if matches!(session.status, SessionStatus::Done | SessionStatus::Unknown) {
            return;
        }

        // Check if we have a pending confirmation for this exact session
        if let Some((idx, ts)) = self.kill_confirm.take() {
            if idx == self.selected && ts.elapsed().as_secs() < 2 {
                // Confirmed — verify PID still runs a killable agent before killing
                let pid = session.pid;
                let verified = std::process::Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "command="])
                    .output()
                    .ok()
                    .map(|output| {
                        let cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        is_killable_agent_command(&cmd)
                    })
                    .unwrap_or(false);
                if !verified {
                    self.set_status(format!("PID {} is no longer a known agent process", pid));
                    return;
                }
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
                self.tick();
                return;
            }
        }

        // First press — ask for confirmation
        let name = self
            .summaries
            .get(&session.session_id)
            .cloned()
            .unwrap_or_else(|| format!("PID {}", session.pid));
        self.kill_confirm = Some((self.selected, Instant::now()));
        self.set_status(format!("Press x again to kill: {}", name));
    }

    /// Kill all orphan port processes (Shift+X).
    /// Does a fresh port scan and validates PID identity + port ownership
    /// immediately before sending any signals to avoid PID reuse / stale cache issues.
    pub fn kill_orphan_ports(&mut self) {
        use crate::collector::process::get_listening_ports;

        // Fresh port scan right now — don't rely on cached data
        let fresh_ports = get_listening_ports();

        for orphan in &self.orphan_ports {
            // 1. Verify PID still listens on the expected port
            let still_listening = fresh_ports
                .get(&orphan.pid)
                .is_some_and(|ports| ports.contains(&orphan.port));
            if !still_listening {
                continue;
            }
            // 2. Verify PID still runs the expected command (full match, not substring)
            if let Ok(output) = std::process::Command::new("ps")
                .args(["-p", &orphan.pid.to_string(), "-o", "command="])
                .output()
            {
                let current_cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if current_cmd == orphan.command {
                    let _ = std::process::Command::new("kill")
                        .args([&orphan.pid.to_string()])
                        .output();
                }
            }
        }
        // Re-collect to reflect changes
        self.tick();
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Jump to the terminal running the selected session's agent process.
    /// Delegates to the terminal-jumper registry (cmux / tmux / iTerm2);
    /// see [`crate::jump`]. No-op when nothing is selected or no backend
    /// recognizes the process.
    pub fn jump_to_session(&mut self) -> JumpOutcome {
        if self.sessions.is_empty() {
            return JumpOutcome::NoOp;
        }
        let target_pid = self.sessions[self.selected].pid;
        crate::jump::run_jump(target_pid)
    }

    /// Get the display summary for a session: LLM summary > "..." if pending > raw prompt > "—"
    /// Done sessions skip pending state to avoid stuck "..." display.
    pub fn session_summary(&self, session: &AgentSession) -> String {
        if let Some(summary) = self.summaries.get(&session.session_id) {
            summary.clone()
        } else if matches!(session.status, SessionStatus::Done) {
            // Done sessions: don't wait for pending summary, show fallback immediately
            if !session.initial_prompt.is_empty() {
                sanitize_fallback(&session.initial_prompt, 80)
            } else if !session.first_assistant_text.is_empty() {
                sanitize_fallback(&session.first_assistant_text, 80)
            } else {
                "—".to_string()
            }
        } else if self.pending_summaries.contains(&session.session_id) {
            // Animate dots: . → .. → ... (cycles every ~1.5s at 2s tick)
            let dots = match (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 500)
                % 3
            {
                0 => ".",
                1 => "..",
                _ => "...",
            };
            dots.to_string()
        } else if !session.initial_prompt.is_empty() {
            sanitize_fallback(&session.initial_prompt, 80)
        } else if !session.first_assistant_text.is_empty() {
            sanitize_fallback(&session.first_assistant_text, 80)
        } else {
            "—".to_string()
        }
    }
}

/// Call `claude --print` via stdin pipe to summarize a prompt.
/// Returns `None` on timeout so the caller can retry later.
fn generate_summary(prompt: &str, assistant_text: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // Build input from user prompt and/or first assistant response
    let user_part: String = prompt.chars().take(200).collect();
    let assistant_part: String = assistant_text.chars().take(200).collect();

    let context = if !user_part.is_empty() && !assistant_part.is_empty() {
        format!(
            "User message: {}\n\nAssistant response: {}",
            user_part, assistant_part
        )
    } else if !assistant_part.is_empty() {
        format!("Assistant response: {}", assistant_part)
    } else {
        format!("User message: {}", user_part)
    };

    let request = format!(
        "You are a conversation title generator. Given the conversation below, create a short title (3-5 words) that describes the session's main topic. Be specific and actionable. Do NOT output generic titles like 'New conversation' or 'Initial setup'. Output ONLY the title, no quotes, no explanation.\n\n{}",
        context
    );

    let mut child = match Command::new("claude")
        .args(["--print", "-"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Some(sanitize_fallback(prompt, 80)),
    };

    // Write prompt via stdin (no shell injection)
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.as_bytes());
    }

    // Run wait_with_output in a helper thread so we can apply a bounded timeout.
    // This drains stdout internally, avoiding pipe-full deadlock.
    let child_pid = child.id();
    let (wo_tx, wo_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = wo_tx.send(child.wait_with_output());
    });

    let result = match wo_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(r) => r,
        Err(_) => {
            // Timeout or disconnected — kill the child so the helper thread can exit.
            let _ = std::process::Command::new("kill")
                .args(["-9", &child_pid.to_string()])
                .status();
            return None;
        }
    };

    let fallback = sanitize_fallback(prompt, 80);

    match result {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let lower = raw.to_lowercase();
            // Reject empty, too long, generic, or prompt-echo outputs
            if raw.is_empty()
                || raw.chars().count() > 80
                || raw.contains("Summarize")
                || raw.starts_with("- ")
                || lower.contains("new conversation")
                || lower.contains("initial setup")
                || lower.contains("initial project")
                || lower.contains("initial conversation")
                || lower.starts_with("greeting")
            {
                Some(fallback)
            } else {
                Some(raw.trim_matches('"').trim_matches('\'').to_string())
            }
        }
        _ => Some(fallback),
    }
}

/// Cache directory: ~/.cache/abtop/
fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("abtop")
}

fn cache_path() -> std::path::PathBuf {
    cache_dir().join("summaries.json")
}

fn load_summary_cache() -> HashMap<String, String> {
    let path = cache_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let mut cache: HashMap<String, String> =
                serde_json::from_str(&content).unwrap_or_default();
            // Purge polluted or old truncated-fallback entries so they regenerate
            let before = cache.len();
            cache.retain(|_, v| !v.contains("You are a conversation tit") && !v.ends_with('…'));
            if cache.len() < before {
                // Persist cleaned cache
                let _ = std::fs::create_dir_all(cache_dir());
                let _ = std::fs::write(&path, serde_json::to_string(&cache).unwrap_or_default());
            }
            cache
        }
        Err(_) => HashMap::new(),
    }
}

fn save_summary_cache(summaries: &HashMap<String, String>) {
    let path = cache_path();
    let _ = std::fs::create_dir_all(cache_dir());
    if let Ok(json) = serde_json::to_string(summaries) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Threshold above which a rate-limited bucket is surfaced as RateLimited
/// in the session list. 90% leaves enough headroom to catch near-saturation
/// before the account actually blocks.
const RATE_LIMITED_PCT: f64 = 90.0;

/// Promote Waiting sessions to RateLimited when a rate limit from the SAME
/// agent CLI is over `RATE_LIMITED_PCT`. Matching on source avoids a
/// Claude-only saturation freezing Codex sessions and vice versa.
fn promote_waiting_to_rate_limited(sessions: &mut [AgentSession], rate_limits: &[RateLimitInfo]) {
    if rate_limits.is_empty() {
        return;
    }
    for s in sessions.iter_mut() {
        if s.status != SessionStatus::Waiting {
            continue;
        }
        let over = rate_limits.iter().any(|rl| {
            rl.source == s.agent_cli
                && (rl.five_hour_pct.unwrap_or(0.0) > RATE_LIMITED_PCT
                    || rl.seven_day_pct.unwrap_or(0.0) > RATE_LIMITED_PCT)
        });
        if over {
            s.status = SessionStatus::RateLimited;
        }
    }
}

fn is_supported_agent_command(cmd: &str) -> bool {
    crate::collector::process::cmd_has_binary(cmd, "claude")
        || crate::collector::process::cmd_has_binary(cmd, "codex")
        || crate::collector::process::cmd_has_binary(cmd, "opencode")
}

fn is_killable_agent_command(cmd: &str) -> bool {
    is_supported_agent_command(cmd)
        && !(crate::collector::process::cmd_has_binary(cmd, "codex") && cmd.contains(" app-server"))
}

fn cmp_str(lhs: &str, rhs: &str) -> Ordering {
    lhs.to_lowercase().cmp(&rhs.to_lowercase())
}

fn status_rank(status: &SessionStatus) -> u8 {
    match status {
        SessionStatus::Thinking => 0,
        SessionStatus::Executing => 1,
        SessionStatus::RateLimited => 2,
        SessionStatus::Waiting => 3,
        SessionStatus::Unknown => 4,
        SessionStatus::Done => 5,
    }
}

fn normalize_session_columns(raw: &[String]) -> Vec<SessionSortColumn> {
    let mut columns = Vec::new();
    for column in raw.iter().filter_map(|s| SessionSortColumn::from_id(s)) {
        if !columns.contains(&column) {
            columns.push(column);
        }
    }
    if columns.is_empty() {
        SessionSortColumn::DEFAULT_COLUMNS.to_vec()
    } else {
        columns
    }
}

fn session_column_order(column: SessionSortColumn) -> usize {
    SessionSortColumn::ALL
        .iter()
        .position(|&candidate| candidate == column)
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting_session(cli: &'static str) -> AgentSession {
        AgentSession {
            agent_cli: cli,
            pid: 1,
            session_id: String::new(),
            cwd: String::new(),
            project_name: String::new(),
            started_at: 0,
            last_turn_at: 0,
            status: SessionStatus::Waiting,
            model: String::new(),
            effort: String::new(),
            context_percent: 0.0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 0,
            compaction_count: 0,
            current_tasks: vec![],
            version: String::new(),
            git_branch: String::new(),
            mem_mb: 0,
            token_history: vec![],
            context_history: vec![],
            context_window: 0,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children: vec![],
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: vec![],
            tool_calls: vec![],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![],
            config_root: String::new(),
            git_added: 0,
            git_modified: 0,
        }
    }

    fn rate_limit(source: &str, pct: f64) -> RateLimitInfo {
        RateLimitInfo {
            source: source.to_string(),
            five_hour_pct: Some(pct),
            five_hour_resets_at: None,
            seven_day_pct: None,
            seven_day_resets_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn test_rate_limited_promotion_is_per_agent_cli() {
        // Claude is saturated, Codex is not. Only the Claude session should
        // be promoted.
        let mut sessions = vec![waiting_session("claude"), waiting_session("codex")];
        let limits = vec![rate_limit("claude", 95.0)];
        promote_waiting_to_rate_limited(&mut sessions, &limits);
        assert_eq!(sessions[0].status, SessionStatus::RateLimited);
        assert_eq!(sessions[1].status, SessionStatus::Waiting);
    }

    #[test]
    fn test_rate_limited_promotion_ignores_below_threshold() {
        let mut sessions = vec![waiting_session("claude")];
        let limits = vec![rate_limit("claude", 89.9)];
        promote_waiting_to_rate_limited(&mut sessions, &limits);
        assert_eq!(sessions[0].status, SessionStatus::Waiting);
    }

    #[test]
    fn session_sort_orders_visible_indices_without_reordering_sessions() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let mut a = waiting_session("claude");
        a.session_id = "a".into();
        a.project_name = "zeta".into();
        a.total_input_tokens = 10;
        let mut b = waiting_session("codex");
        b.session_id = "b".into();
        b.project_name = "alpha".into();
        b.total_input_tokens = 30;
        app.sessions = vec![a, b];

        app.toggle_session_sort(SessionSortColumn::Project);
        assert_eq!(app.visible_indices(), vec![1, 0]);
        assert_eq!(app.sessions[0].project_name, "zeta");

        app.toggle_session_sort(SessionSortColumn::Project);
        assert_eq!(app.visible_indices(), vec![0, 1]);
    }

    #[test]
    fn token_and_everything_sort_are_distinct() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let mut active_heavy = waiting_session("claude");
        active_heavy.session_id = "active".into();
        active_heavy.total_input_tokens = 100;
        active_heavy.total_output_tokens = 100;
        active_heavy.total_cache_read = 0;
        let mut cache_heavy = waiting_session("codex");
        cache_heavy.session_id = "cache".into();
        cache_heavy.total_input_tokens = 10;
        cache_heavy.total_output_tokens = 10;
        cache_heavy.total_cache_read = 1_000;
        app.sessions = vec![active_heavy, cache_heavy];

        app.toggle_session_sort(SessionSortColumn::Tokens);
        assert_eq!(app.visible_indices(), vec![0, 1]);

        app.toggle_session_sort(SessionSortColumn::Everything);
        assert_eq!(app.visible_indices(), vec![1, 0]);
    }

    #[test]
    fn status_sort_orders_by_activity_rank() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let mut done = waiting_session("claude");
        done.session_id = "done".into();
        done.status = SessionStatus::Done;
        let mut executing = waiting_session("codex");
        executing.session_id = "executing".into();
        executing.status = SessionStatus::Executing;
        let mut rate_limited = waiting_session("opencode");
        rate_limited.session_id = "rate".into();
        rate_limited.status = SessionStatus::RateLimited;
        app.sessions = vec![done, executing, rate_limited];

        app.toggle_session_sort(SessionSortColumn::Status);
        assert_eq!(app.visible_indices(), vec![1, 2, 0]);

        app.toggle_session_sort(SessionSortColumn::Status);
        assert_eq!(app.visible_indices(), vec![0, 2, 1]);
    }

    #[test]
    fn recent_sort_uses_last_turn_timestamp() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let mut oldest = waiting_session("claude");
        oldest.session_id = "oldest".into();
        oldest.started_at = 100;
        oldest.last_turn_at = 200;
        let mut newest = waiting_session("codex");
        newest.session_id = "newest".into();
        newest.started_at = 50;
        newest.last_turn_at = 900;
        app.sessions = vec![oldest, newest];

        app.toggle_session_sort(SessionSortColumn::Recent);
        assert_eq!(app.visible_indices(), vec![1, 0]);

        app.set_session_sort_ascending();
        assert_eq!(app.visible_indices(), vec![0, 1]);
    }

    #[test]
    fn sort_mode_selects_column_and_direction_with_arrows() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );

        app.toggle_session_sort_mode();
        assert!(app.session_sort_mode);
        assert_eq!(
            app.session_sort,
            Some(SessionSort {
                column: SessionSortColumn::Recent,
                ascending: false,
            })
        );

        app.select_next_session_sort_column();
        assert_eq!(app.session_sort.unwrap().column, SessionSortColumn::Pid);

        app.set_session_sort_ascending();
        assert!(app.session_sort.unwrap().ascending);

        app.select_prev_session_sort_column();
        assert_eq!(app.session_sort.unwrap().column, SessionSortColumn::Recent);
        assert!(!app.session_sort.unwrap().ascending);

        app.close_session_sort_mode();
        assert!(!app.session_sort_mode);
    }

    #[test]
    fn sort_mode_can_cycle_only_rendered_columns() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let rendered = [
            SessionSortColumn::Ai,
            SessionSortColumn::Recent,
            SessionSortColumn::Session,
            SessionSortColumn::Summary,
            SessionSortColumn::Status,
        ];

        app.session_sort = Some(SessionSort {
            column: SessionSortColumn::Session,
            ascending: true,
        });
        app.select_next_session_sort_column_from(&rendered);
        assert_eq!(app.session_sort.unwrap().column, SessionSortColumn::Summary);

        app.select_next_session_sort_column_from(&rendered);
        assert_eq!(app.session_sort.unwrap().column, SessionSortColumn::Status);

        app.session_sort = Some(SessionSort {
            column: SessionSortColumn::Config,
            ascending: true,
        });
        app.ensure_session_sort_column_in(&rendered);
        assert_eq!(app.session_sort.unwrap().column, SessionSortColumn::Ai);
    }

    #[test]
    fn configured_session_columns_parse_known_ids_and_ignore_unknown() {
        let raw = vec![
            "summary".to_string(),
            "bogus".to_string(),
            "cache_r".to_string(),
            "summary".to_string(),
        ];
        let app = App::new_with_config_and_claude_dirs_and_columns(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
            &[],
            false,
            &raw,
        );

        assert_eq!(
            app.session_columns,
            vec![SessionSortColumn::Summary, SessionSortColumn::CacheRead]
        );
    }

    #[test]
    fn test_rate_limited_promotion_skips_non_waiting_sessions() {
        let mut sessions = vec![waiting_session("claude")];
        sessions[0].status = SessionStatus::Thinking;
        let limits = vec![rate_limit("claude", 99.0)];
        promote_waiting_to_rate_limited(&mut sessions, &limits);
        assert_eq!(sessions[0].status, SessionStatus::Thinking);
    }

    #[test]
    fn supported_agent_command_accepts_opencode() {
        assert!(is_supported_agent_command("/usr/local/bin/claude"));
        assert!(is_supported_agent_command("codex --resume abc"));
        assert!(is_supported_agent_command("/opt/homebrew/bin/opencode"));
        assert!(!is_supported_agent_command("node server.js"));
    }

    #[test]
    fn killable_agent_command_rejects_codex_app_server() {
        assert!(is_killable_agent_command("codex --resume abc"));
        assert!(is_killable_agent_command("/usr/local/bin/claude"));
        assert!(!is_killable_agent_command(
            "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled"
        ));
    }

    fn test_app(lock_theme: bool) -> App {
        let panels = crate::config::PanelVisibility::default();
        let theme = crate::theme::Theme::default();
        App::new_with_config_and_claude_dirs(theme, &[], panels, &[], lock_theme)
    }

    #[test]
    fn test_lock_theme_default_false() {
        let app = test_app(false);
        assert!(!app.lock_theme);
    }

    #[test]
    fn test_lock_theme_true() {
        let app = test_app(true);
        assert!(app.lock_theme);
    }

    #[test]
    fn test_toggle_section_zoom_roundtrip() {
        let mut app = test_app(false);
        assert!(app.maximized_narrow_section().is_none());
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert_eq!(app.maximized_narrow_section(), Some(NarrowSection::Quota));
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert!(app.maximized_narrow_section().is_none());
    }

    #[test]
    fn test_toggle_section_zoom_switches_section() {
        let mut app = test_app(false);
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert_eq!(app.maximized_narrow_section(), Some(NarrowSection::Quota));
        app.toggle_narrow_section_zoom(NarrowSection::Tokens);
        assert_eq!(app.maximized_narrow_section(), Some(NarrowSection::Tokens));
    }

    #[test]
    fn test_maximize_active_section_defaults_to_sessions() {
        let mut app = test_app(false);
        // Default active section is Sessions (Work tab, first section)
        let active = app.active_narrow_section();
        assert!(active.is_some());
        app.maximize_active_narrow_section();
        assert_eq!(app.maximized_narrow_section(), active);
    }

    #[test]
    fn test_restore_narrow_sections() {
        let mut app = test_app(false);
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert!(app.maximized_narrow_section().is_some());
        app.restore_narrow_sections();
        assert!(app.maximized_narrow_section().is_none());
    }

    #[test]
    fn test_restore_idempotent() {
        let mut app = test_app(false);
        app.restore_narrow_sections();
        assert!(app.maximized_narrow_section().is_none());
    }

    #[test]
    fn test_hovered_section_defaults_to_none() {
        let app = test_app(false);
        assert!(app.hovered_section.is_none());
    }

    #[test]
    fn test_set_active_section_switches_tab() {
        let mut app = test_app(false);
        app.set_active_narrow_section(NarrowSection::Quota);
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Quota));
        assert_eq!(app.active_narrow_tab(), Some(NarrowTab::Usage));
    }

    #[test]
    fn test_select_next_section_cycles_forward() {
        let mut app = test_app(false);
        // Default active is Sessions (Work tab). Next should be Mcp.
        app.select_next_section();
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Mcp));
        app.select_next_section();
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Context));
    }

    #[test]
    fn test_select_prev_section_cycles_backward() {
        let mut app = test_app(false);
        // Default active is Sessions.
        app.select_prev_section();
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Ports));
    }

    #[test]
    fn test_select_next_section_wraps_around() {
        let mut app = test_app(false);
        // Wrap through all 7 sections back to start
        app.set_active_narrow_section(NarrowSection::Mcp);
        app.select_next_section();
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Context));
    }

    #[test]
    fn test_select_next_section_wraps_backward() {
        let mut app = test_app(false);
        app.set_active_narrow_section(NarrowSection::Context);
        app.select_prev_section();
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Mcp));
    }

    #[test]
    fn test_select_next_section_skips_hidden() {
        let mut app = test_app(false);
        app.show_quota = false;
        app.show_tokens = false;
        app.show_projects = false;
        app.show_ports = false;
        // Visible: Context, Sessions, Mcp. From Sessions → Mcp.
        app.set_active_narrow_section(NarrowSection::Sessions);
        app.select_next_section();
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Mcp));
        app.select_next_section();
        assert_eq!(app.active_narrow_section(), Some(NarrowSection::Context));
    }

    #[test]
    fn test_set_active_section_ignores_hidden() {
        let mut app = test_app(false);
        app.show_quota = false;
        app.set_active_narrow_section(NarrowSection::Quota);
        // Should NOT set — hidden
        assert_ne!(app.active_narrow_section(), Some(NarrowSection::Quota));
    }

    #[test]
    fn test_maximized_section_none_when_tab_hidden() {
        let mut app = test_app(false);
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert_eq!(app.maximized_narrow_section(), Some(NarrowSection::Quota));
        // Hide all Usage sections — maximized should return None
        app.show_quota = false;
        app.show_tokens = false;
        app.show_context = false;
        assert!(app.maximized_narrow_section().is_none());
    }

    #[test]
    fn test_toggle_zoom_twice_restores() {
        let mut app = test_app(false);
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert_eq!(app.maximized_narrow_section(), Some(NarrowSection::Quota));
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert!(app.maximized_narrow_section().is_none());
        // Third toggle zooms again
        app.toggle_narrow_section_zoom(NarrowSection::Quota);
        assert_eq!(app.maximized_narrow_section(), Some(NarrowSection::Quota));
    }
}
