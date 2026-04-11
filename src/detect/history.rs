use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};

use super::AgentInfo;
use super::estimate_cost;
use super::process::AgentKind;
use super::state::{
    FileMetadata, ParsedTokens, codex_sessions_dir, extract_claude_session_id,
    extract_codex_session_id, file_metadata, parse_claude_tokens, parse_codex_tokens,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub turns: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentPeriodStats {
    pub today: AgentTotals,
    pub seven_days: AgentTotals,
    pub total: AgentTotals,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AggregatedStats {
    pub claude: AgentPeriodStats,
    pub codex: AgentPeriodStats,
}

#[derive(Clone)]
struct SessionBaseline {
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    turns: u32,
    file_size: u64,
    last_modified_ts: u64,
}

struct ActiveSession {
    kind: AgentKind,
    baseline: SessionBaseline,
}

#[derive(Default)]
struct HistoryRuntime {
    baseline: AggregatedStats,
    active_delta: AggregatedStats,
    active_sessions: HashMap<String, ActiveSession>,
    last_active_scan: Option<Instant>,
}

pub struct HistoryStore {
    db: Option<sqlite::Connection>,
    initialized: bool,
    baselines: HashMap<String, SessionBaseline>,
    runtime: HistoryRuntime,
}

impl HistoryStore {
    pub fn start() -> Self {
        let db = open_db();
        let initialized = is_initialized();
        let baselines = db.as_ref().map(load_baselines).unwrap_or_default();
        let snapshot = db.as_ref().map(compute_snapshot).unwrap_or_default();
        let mut runtime = HistoryRuntime::default();
        reset_runtime_baseline(&mut runtime, snapshot, &baselines);

        Self {
            db,
            initialized,
            baselines,
            runtime,
        }
    }

    pub fn refresh_persistent_baseline(&mut self) {
        let Some(db) = self.db.as_ref() else {
            return;
        };

        let today = local_date_today();
        let needs_full_scan =
            !self.initialized || daily_refresh_date(db).as_deref() != Some(&today);
        if !needs_full_scan {
            return;
        }

        if !self.initialized {
            let _ = db.execute("DELETE FROM daily_totals");
            let _ = db.execute("DELETE FROM sessions");
            self.baselines.clear();
        }

        scan_and_update(db, &mut self.runtime, &mut self.baselines);
        set_daily_refresh_date(db, &today);
        if !self.initialized {
            set_initialized();
            self.initialized = true;
        }
    }

    pub fn aggregated_stats(&mut self, active_agents: &[AgentInfo]) -> AggregatedStats {
        refresh_active_sessions(&mut self.runtime, &self.baselines, active_agents);
        self.runtime.last_active_scan = Some(Instant::now());

        merge_stats(&self.runtime.baseline, &self.runtime.active_delta)
    }
}

fn is_initialized() -> bool {
    crate::config::read_value("core", "initialized").is_some_and(|v| v == "true")
}

fn set_initialized() {
    crate::config::write_value("core", "initialized", "true");
}

fn read_runtime_state(db: &sqlite::Connection, key: &str) -> Option<String> {
    let mut stmt = db
        .prepare("SELECT value FROM runtime_state WHERE key = ?")
        .ok()?;
    let _ = stmt.bind((1, key));
    if stmt.next().ok()? == sqlite::State::Row {
        stmt.read::<String, _>(0).ok()
    } else {
        None
    }
}

fn write_runtime_state(db: &sqlite::Connection, key: &str, value: &str) {
    let Ok(mut stmt) = db.prepare(
        "INSERT INTO runtime_state (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    ) else {
        return;
    };
    let _ = stmt.bind((1, key));
    let _ = stmt.bind((2, value));
    let _ = stmt.next();
}

fn ensure_runtime_state_table(db: &sqlite::Connection) {
    let _ = db.execute(
        "CREATE TABLE IF NOT EXISTS runtime_state (\
            key TEXT PRIMARY KEY,\
            value TEXT NOT NULL\
        )",
    );
}

fn daily_refresh_date(db: &sqlite::Connection) -> Option<String> {
    ensure_runtime_state_table(db);
    read_runtime_state(db, "last_daily_refresh_date")
}

fn set_daily_refresh_date(db: &sqlite::Connection, date: &str) {
    write_runtime_state(db, "last_daily_refresh_date", date);
}

fn local_date_today() -> String {
    local_date_for_offset(0)
}

fn local_date_for_offset(days_ago: i32) -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_mday -= days_ago;
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        libc::mktime(&mut tm);
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

fn discover_claude_jsonl_files() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(&projects_dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(sub_entries) = fs::read_dir(&path) else {
            continue;
        };
        for sub in sub_entries.flatten() {
            let file = sub.path();
            if file.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(file);
            }
        }
    }
    files
}

fn discover_codex_jsonl_files() -> Vec<PathBuf> {
    let Some(sessions_dir) = codex_sessions_dir() else {
        return Vec::new();
    };
    let mut files = Vec::new();
    super::state::walk_jsonl(&sessions_dir, &mut files);
    files
}

fn file_is_unchanged(baseline: &SessionBaseline, metadata: FileMetadata) -> bool {
    baseline.file_size == metadata.size && baseline.last_modified_ts == metadata.modified_ts
}

fn session_delta(current: &AgentTotals, baseline: &SessionBaseline) -> AgentTotals {
    AgentTotals {
        input_tokens: current.input_tokens.saturating_sub(baseline.input_tokens),
        output_tokens: current.output_tokens.saturating_sub(baseline.output_tokens),
        cost_usd: (current.cost_usd - baseline.cost_usd).max(0.0),
        turns: current.turns.saturating_sub(baseline.turns),
    }
}

fn tokens_to_totals(tokens: &ParsedTokens, cost: f64) -> AgentTotals {
    AgentTotals {
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        cost_usd: cost,
        turns: tokens.turn_count,
    }
}

fn add_totals(target: &mut AgentTotals, delta: &AgentTotals) {
    target.input_tokens = target.input_tokens.saturating_add(delta.input_tokens);
    target.output_tokens = target.output_tokens.saturating_add(delta.output_tokens);
    target.cost_usd += delta.cost_usd;
    target.turns = target.turns.saturating_add(delta.turns);
}

fn add_period_delta(period: &mut AgentPeriodStats, delta: &AgentTotals) {
    add_totals(&mut period.today, delta);
    add_totals(&mut period.seven_days, delta);
    add_totals(&mut period.total, delta);
}

fn add_agent_delta(stats: &mut AggregatedStats, kind: AgentKind, delta: &AgentTotals) {
    match kind {
        AgentKind::ClaudeCode => add_period_delta(&mut stats.claude, delta),
        AgentKind::Codex => add_period_delta(&mut stats.codex, delta),
    }
}

fn add_period_totals(target: &mut AgentPeriodStats, delta: &AgentPeriodStats) {
    add_totals(&mut target.today, &delta.today);
    add_totals(&mut target.seven_days, &delta.seven_days);
    add_totals(&mut target.total, &delta.total);
}

fn merge_stats(base: &AggregatedStats, overlay: &AggregatedStats) -> AggregatedStats {
    let mut merged = base.clone();
    add_period_totals(&mut merged.claude, &overlay.claude);
    add_period_totals(&mut merged.codex, &overlay.codex);
    merged
}

fn reset_runtime_baseline(
    runtime: &mut HistoryRuntime,
    snapshot: AggregatedStats,
    baselines: &HashMap<String, SessionBaseline>,
) {
    runtime.baseline = snapshot;
    runtime.active_delta = AggregatedStats::default();
    runtime.active_sessions.clear();
    runtime.last_active_scan = if baselines.is_empty() {
        None
    } else {
        Some(Instant::now())
    };
}

fn file_mtime_date(path: &Path) -> String {
    let ts = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if ts == 0 {
        return "2000-01-01".to_string();
    }
    unsafe {
        let time = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&time, &mut tm);
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

fn open_db() -> Option<sqlite::Connection> {
    let dir = crate::config::config_dir();
    let _ = fs::create_dir_all(&dir);
    let db = sqlite::open(dir.join("stats.db")).ok()?;
    db.execute(
        "CREATE TABLE IF NOT EXISTS sessions (\
            agent_type TEXT NOT NULL,\
            session_id TEXT NOT NULL,\
            file_path TEXT NOT NULL,\
            input_tokens INTEGER DEFAULT 0,\
            output_tokens INTEGER DEFAULT 0,\
            cache_read_tokens INTEGER DEFAULT 0,\
            cache_creation_tokens INTEGER DEFAULT 0,\
            cost_usd REAL DEFAULT 0.0,\
            turns INTEGER DEFAULT 0,\
            model TEXT,\
            last_modified_ts INTEGER DEFAULT 0,\
            file_size INTEGER DEFAULT 0,\
            PRIMARY KEY (agent_type, session_id)\
        )",
    )
    .ok()?;
    db.execute(
        "CREATE TABLE IF NOT EXISTS daily_totals (\
            agent_type TEXT NOT NULL,\
            date TEXT NOT NULL,\
            input_tokens INTEGER DEFAULT 0,\
            output_tokens INTEGER DEFAULT 0,\
            cost_usd REAL DEFAULT 0.0,\
            turns INTEGER DEFAULT 0,\
            PRIMARY KEY (agent_type, date)\
        )",
    )
    .ok()?;
    ensure_runtime_state_table(&db);
    Some(db)
}

fn upsert_session(
    db: &sqlite::Connection,
    agent_type: &str,
    session_id: &str,
    file_path: &Path,
    tokens: &ParsedTokens,
    cost: f64,
    metadata: FileMetadata,
) {
    let sql = "INSERT OR REPLACE INTO sessions \
               (agent_type, session_id, file_path, input_tokens, output_tokens, \
                cache_read_tokens, cache_creation_tokens, cost_usd, turns, model, \
                last_modified_ts, file_size) \
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    let Ok(mut stmt) = db.prepare(sql) else {
        return;
    };
    let _ = stmt.bind((1, agent_type));
    let _ = stmt.bind((2, session_id));
    let _ = stmt.bind((3, file_path.to_string_lossy().as_ref()));
    let _ = stmt.bind((4, tokens.input_tokens as i64));
    let _ = stmt.bind((5, tokens.output_tokens as i64));
    let _ = stmt.bind((6, tokens.cache_read_tokens as i64));
    let _ = stmt.bind((7, tokens.cache_creation_tokens as i64));
    let _ = stmt.bind((8, cost));
    let _ = stmt.bind((9, tokens.turn_count as i64));
    let _ = stmt.bind((10, tokens.model.as_deref().unwrap_or("")));
    let _ = stmt.bind((11, metadata.modified_ts as i64));
    let _ = stmt.bind((12, metadata.size as i64));
    let _ = stmt.next();
}

fn add_daily_delta(db: &sqlite::Connection, agent_type: &str, date: &str, delta: &AgentTotals) {
    if delta.input_tokens == 0 && delta.output_tokens == 0 && delta.turns == 0 {
        return;
    }
    let sql = "INSERT INTO daily_totals (agent_type, date, input_tokens, output_tokens, cost_usd, turns) \
               VALUES (?, ?, ?, ?, ?, ?) \
               ON CONFLICT(agent_type, date) DO UPDATE SET \
               input_tokens = input_tokens + excluded.input_tokens, \
               output_tokens = output_tokens + excluded.output_tokens, \
               cost_usd = cost_usd + excluded.cost_usd, \
               turns = turns + excluded.turns";
    let Ok(mut stmt) = db.prepare(sql) else {
        return;
    };
    let _ = stmt.bind((1, agent_type));
    let _ = stmt.bind((2, date));
    let _ = stmt.bind((3, delta.input_tokens as i64));
    let _ = stmt.bind((4, delta.output_tokens as i64));
    let _ = stmt.bind((5, delta.cost_usd));
    let _ = stmt.bind((6, delta.turns as i64));
    let _ = stmt.next();
}

fn compute_snapshot(db: &sqlite::Connection) -> AggregatedStats {
    let mut stats = AggregatedStats::default();
    let today = local_date_today();
    let week_start = local_date_for_offset(6);
    fill_daily(db, &mut stats, &today, |s| &mut s.today);
    fill_daily(db, &mut stats, &week_start, |s| &mut s.seven_days);
    fill_daily(db, &mut stats, "0000-00-00", |s| &mut s.total);
    stats
}

fn fill_daily(
    db: &sqlite::Connection,
    stats: &mut AggregatedStats,
    since_date: &str,
    accessor: fn(&mut AgentPeriodStats) -> &mut AgentTotals,
) {
    let sql = "SELECT agent_type, SUM(input_tokens), SUM(output_tokens), \
               SUM(cost_usd), SUM(turns) FROM daily_totals \
               WHERE date >= ? GROUP BY agent_type";
    let Ok(mut stmt) = db.prepare(sql) else {
        return;
    };
    let _ = stmt.bind((1, since_date));
    while let Ok(sqlite::State::Row) = stmt.next() {
        let agent_type: String = stmt.read(0).unwrap_or_default();
        let input: i64 = stmt.read(1).unwrap_or(0);
        let output: i64 = stmt.read(2).unwrap_or(0);
        let cost: f64 = stmt.read(3).unwrap_or(0.0);
        let turns: i64 = stmt.read(4).unwrap_or(0);
        let period_stats = match agent_type.as_str() {
            "claude" => &mut stats.claude,
            "codex" => &mut stats.codex,
            _ => continue,
        };
        let totals = accessor(period_stats);
        totals.input_tokens = input as u64;
        totals.output_tokens = output as u64;
        totals.cost_usd = cost;
        totals.turns = turns as u32;
    }
}

fn load_baselines(db: &sqlite::Connection) -> HashMap<String, SessionBaseline> {
    let mut baselines = HashMap::new();
    let sql = "SELECT agent_type, session_id, input_tokens, output_tokens, \
               cost_usd, turns, file_size, last_modified_ts FROM sessions";
    let Ok(mut stmt) = db.prepare(sql) else {
        return baselines;
    };
    while let Ok(sqlite::State::Row) = stmt.next() {
        let agent_type: String = stmt.read(0).unwrap_or_default();
        let session_id: String = stmt.read(1).unwrap_or_default();
        baselines.insert(
            format!("{agent_type}:{session_id}"),
            SessionBaseline {
                input_tokens: stmt.read::<i64, _>(2).unwrap_or(0) as u64,
                output_tokens: stmt.read::<i64, _>(3).unwrap_or(0) as u64,
                cost_usd: stmt.read::<f64, _>(4).unwrap_or(0.0),
                turns: stmt.read::<i64, _>(5).unwrap_or(0) as u32,
                file_size: stmt.read::<i64, _>(6).unwrap_or(0) as u64,
                last_modified_ts: stmt.read::<i64, _>(7).unwrap_or(0) as u64,
            },
        );
    }
    baselines
}

fn process_file(
    db: &sqlite::Connection,
    kind: AgentKind,
    session_id: &str,
    path: &Path,
    parse_fn: fn(&Path) -> ParsedTokens,
    baselines: &mut HashMap<String, SessionBaseline>,
    today: &str,
) {
    let agent_type = kind.db_key();
    let metadata = file_metadata(path);
    let key = format!("{agent_type}:{session_id}");
    if baselines
        .get(&key)
        .is_some_and(|baseline| file_is_unchanged(baseline, metadata))
    {
        return;
    }

    let tokens = parse_fn(path);
    let cost = session_cost(kind, &tokens);
    let current = tokens_to_totals(&tokens, cost);
    let (delta, date) = if let Some(baseline) = baselines.get(&key) {
        (session_delta(&current, baseline), today.to_string())
    } else {
        (current, file_mtime_date(path))
    };

    add_daily_delta(db, agent_type, &date, &delta);
    upsert_session(db, agent_type, session_id, path, &tokens, cost, metadata);
    baselines.insert(
        key,
        SessionBaseline {
            input_tokens: tokens.input_tokens,
            output_tokens: tokens.output_tokens,
            cost_usd: cost,
            turns: tokens.turn_count,
            file_size: metadata.size,
            last_modified_ts: metadata.modified_ts,
        },
    );
}

fn scan_and_update(
    db: &sqlite::Connection,
    runtime: &mut HistoryRuntime,
    baselines: &mut HashMap<String, SessionBaseline>,
) {
    let today = local_date_today();
    for path in discover_claude_jsonl_files() {
        if let Some(session_id) = extract_claude_session_id(&path) {
            process_file(
                db,
                AgentKind::ClaudeCode,
                &session_id,
                &path,
                parse_claude_tokens,
                baselines,
                &today,
            );
        }
    }
    for path in discover_codex_jsonl_files() {
        if let Some(session_id) = extract_codex_session_id(&path) {
            process_file(
                db,
                AgentKind::Codex,
                &session_id,
                &path,
                parse_codex_tokens,
                baselines,
                &today,
            );
        }
    }
    let snapshot = compute_snapshot(db);
    reset_runtime_baseline(runtime, snapshot, baselines);
}

fn active_key(kind: AgentKind, session_id: &str) -> String {
    format!("{}:{session_id}", kind.db_key())
}

fn zero_baseline(metadata: FileMetadata) -> SessionBaseline {
    SessionBaseline {
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        turns: 0,
        file_size: metadata.size,
        last_modified_ts: metadata.modified_ts,
    }
}

fn current_session_baseline(agent: &AgentInfo) -> SessionBaseline {
    let metadata = agent
        .jsonl_path
        .as_deref()
        .map(file_metadata)
        .unwrap_or(FileMetadata {
            size: 0,
            modified_ts: 0,
        });
    SessionBaseline {
        input_tokens: agent.input_tokens,
        output_tokens: agent.output_tokens,
        cost_usd: agent.cost_usd,
        turns: agent.turn_count,
        file_size: metadata.size,
        last_modified_ts: metadata.modified_ts,
    }
}

fn session_cost(kind: AgentKind, tokens: &ParsedTokens) -> f64 {
    tokens
        .model
        .as_deref()
        .map(|model| {
            let (cache_read, cache_creation) = if kind == AgentKind::ClaudeCode {
                (tokens.cache_read_tokens, tokens.cache_creation_tokens)
            } else {
                (0, 0)
            };
            estimate_cost(
                model,
                tokens.input_tokens,
                tokens.output_tokens,
                cache_read,
                cache_creation,
            )
        })
        .unwrap_or(0.0)
}

fn baseline_totals(baseline: &SessionBaseline) -> AgentTotals {
    AgentTotals {
        input_tokens: baseline.input_tokens,
        output_tokens: baseline.output_tokens,
        cost_usd: baseline.cost_usd,
        turns: baseline.turns,
    }
}

fn record_active_delta(runtime: &mut HistoryRuntime, key: &str, current: SessionBaseline) {
    let Some(active) = runtime.active_sessions.get_mut(key) else {
        return;
    };
    let delta = session_delta(&baseline_totals(&current), &active.baseline);
    add_agent_delta(&mut runtime.active_delta, active.kind, &delta);
    active.baseline = current;
}

fn refresh_active_sessions(
    runtime: &mut HistoryRuntime,
    baselines: &HashMap<String, SessionBaseline>,
    agents: &[AgentInfo],
) {
    let mut seen = HashSet::new();

    for agent in agents {
        let Some(session_id) = agent.session_id.as_deref() else {
            continue;
        };
        let Some(_path) = agent.jsonl_path.as_deref() else {
            continue;
        };
        let key = active_key(agent.kind, session_id);
        seen.insert(key.clone());
        let current = current_session_baseline(agent);
        if let Some(active) = runtime.active_sessions.get(&key) {
            let unchanged = active.baseline.input_tokens == current.input_tokens
                && active.baseline.output_tokens == current.output_tokens
                && active.baseline.turns == current.turns
                && (active.baseline.cost_usd - current.cost_usd).abs() < f64::EPSILON;
            if unchanged {
                continue;
            }
        }

        if runtime.active_sessions.contains_key(&key) {
            record_active_delta(runtime, &key, current);
            continue;
        }

        let baseline = baselines.get(&key).cloned().unwrap_or_else(|| {
            zero_baseline(FileMetadata {
                size: 0,
                modified_ts: 0,
            })
        });
        let delta = session_delta(&baseline_totals(&current), &baseline);
        add_agent_delta(&mut runtime.active_delta, agent.kind, &delta);
        runtime.active_sessions.insert(
            key,
            ActiveSession {
                kind: agent.kind,
                baseline: current,
            },
        );
    }

    let stale: Vec<String> = runtime
        .active_sessions
        .keys()
        .filter(|key| !seen.contains(*key))
        .cloned()
        .collect();
    for key in stale {
        runtime.active_sessions.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::state::AgentState;
    use std::io::Write;
    use std::time::UNIX_EPOCH;

    fn write_temp_jsonl(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agentmux-history-{name}-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn claude_agent(path: &Path) -> AgentInfo {
        let parsed = parse_claude_tokens(path);
        AgentInfo {
            kind: AgentKind::ClaudeCode,
            agent_pid: Some(42),
            pane_id: "%1".to_string(),
            cwd: "/tmp".to_string(),
            window_id: "@1".to_string(),
            window_name: "win".to_string(),
            state: AgentState::Working,
            elapsed_secs: 10,
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            last_activity: parsed.last_activity,
            context_pct: parsed.context_pct,
            model: parsed.model,
            effort: None,
            cost_usd: 0.0,
            turn_count: parsed.turn_count,
            session_id: parsed.session_id,
            jsonl_path: Some(path.to_path_buf()),
            resumed: false,
            details_ready: true,
        }
    }

    #[test]
    fn file_is_unchanged_only_when_size_and_mtime_match() {
        let metadata = FileMetadata {
            size: 100,
            modified_ts: 200,
        };
        let baseline = SessionBaseline {
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: 0.01,
            turns: 1,
            file_size: 100,
            last_modified_ts: 200,
        };

        assert!(file_is_unchanged(&baseline, metadata));
        assert!(!file_is_unchanged(
            &baseline,
            FileMetadata {
                size: 101,
                ..metadata
            }
        ));
        assert!(!file_is_unchanged(
            &baseline,
            FileMetadata {
                modified_ts: 201,
                ..metadata
            }
        ));
    }

    #[test]
    fn session_delta_saturates_from_baseline_to_current_totals() {
        let baseline = SessionBaseline {
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: 0.20,
            turns: 4,
            file_size: 1,
            last_modified_ts: 2,
        };
        let current = AgentTotals {
            input_tokens: 130,
            output_tokens: 45,
            cost_usd: 0.10,
            turns: 6,
        };

        let delta = session_delta(&current, &baseline);
        assert_eq!(delta.input_tokens, 30);
        assert_eq!(delta.output_tokens, 0);
        assert_eq!(delta.turns, 2);
        assert_eq!(delta.cost_usd, 0.0);
    }

    #[test]
    fn active_session_overlay_adds_only_delta_from_db_baseline() {
        let path = write_temp_jsonl(
            "active-overlay",
            r#"{"sessionId":"s1","message":{"role":"assistant","id":"m1","usage":{"input_tokens":30,"output_tokens":7}}}
"#,
        );
        let mut runtime = HistoryRuntime::default();
        let mut baselines = HashMap::new();
        baselines.insert(
            "claude:s1".to_string(),
            SessionBaseline {
                input_tokens: 10,
                output_tokens: 2,
                cost_usd: 0.0,
                turns: 0,
                file_size: 0,
                last_modified_ts: 0,
            },
        );

        refresh_active_sessions(&mut runtime, &baselines, &[claude_agent(&path)]);

        assert_eq!(runtime.active_delta.claude.today.input_tokens, 20);
        assert_eq!(runtime.active_delta.claude.today.output_tokens, 5);
        assert_eq!(runtime.active_delta.claude.seven_days.input_tokens, 20);
        assert_eq!(runtime.active_delta.claude.total.input_tokens, 20);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn finalizing_active_session_keeps_last_observed_overlay() {
        let path = write_temp_jsonl(
            "active-finalize-start",
            r#"{"sessionId":"s1","message":{"role":"assistant","id":"m1","usage":{"input_tokens":30,"output_tokens":7}}}
"#,
        );
        let mut runtime = HistoryRuntime::default();
        let baselines = HashMap::new();

        refresh_active_sessions(&mut runtime, &baselines, &[claude_agent(&path)]);
        assert_eq!(runtime.active_delta.claude.today.input_tokens, 30);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(
            br#"{"sessionId":"s1","message":{"role":"assistant","id":"m2","usage":{"input_tokens":45,"output_tokens":9}}}
"#,
        )
        .unwrap();
        file.flush().unwrap();

        refresh_active_sessions(&mut runtime, &baselines, &[]);

        assert_eq!(runtime.active_delta.claude.today.input_tokens, 30);
        assert_eq!(runtime.active_delta.claude.today.output_tokens, 7);

        let _ = fs::remove_file(path);
    }
}
