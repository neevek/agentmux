use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::AgentInfo;
use super::estimate_cost;
use super::process::AgentKind;
use super::state::{
    FileMetadata, ParsedTokens, codex_sessions_dir, extract_claude_session_id,
    extract_codex_session_id, file_metadata, parse_claude_tokens, parse_codex_tokens,
};

const DELTA_INTERVAL_SECS: u64 = 120; // 2 minutes
const OWNER_HEARTBEAT_STALE_SECS: u64 = DELTA_INTERVAL_SECS * 3;

// --- Public data structures ---

#[derive(Debug, Clone, Default)]
pub struct AgentTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub turns: u32,
}

#[derive(Debug, Clone, Default)]
pub struct AgentPeriodStats {
    pub today: AgentTotals,
    pub seven_days: AgentTotals,
    pub total: AgentTotals,
}

#[derive(Debug, Clone, Default)]
pub struct AggregatedStats {
    pub claude: AgentPeriodStats,
    pub codex: AgentPeriodStats,
}

// --- Internal ---

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
    path: PathBuf,
    parse_fn: fn(&Path) -> ParsedTokens,
    baseline: SessionBaseline,
}

#[derive(Default)]
struct HistoryRuntime {
    baseline: AggregatedStats,
    active_delta: AggregatedStats,
    db_session_baselines: HashMap<String, SessionBaseline>,
    active_sessions: HashMap<String, ActiveSession>,
    last_active_scan: Option<Instant>,
}

fn is_initialized() -> bool {
    crate::config::read_value("core", "initialized").is_some_and(|v| v == "true")
}

fn set_initialized() {
    crate::config::write_value("core", "initialized", "true");
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn full_refresh_marker_value() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}:{nanos}", std::process::id())
}

fn process_is_alive(pid: u32) -> bool {
    // `kill(pid, 0)` is only a same-user liveness check here; EPERM is treated
    // as not claimable, which is fine for agentmux sidebars under one tmux user.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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
        "CREATE TABLE IF NOT EXISTS runtime_state (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    );
}

fn try_claim_owner(db: &sqlite::Connection) -> bool {
    ensure_runtime_state_table(db);
    if db.execute("BEGIN IMMEDIATE").is_err() {
        return false;
    }

    let this_pid = std::process::id();
    let now = unix_now_secs();
    let owner_pid = read_runtime_state(db, "owner_pid").and_then(|v| v.parse::<u32>().ok());
    let owner_seen = read_runtime_state(db, "owner_heartbeat")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let can_claim = owner_pid.is_none_or(|pid| {
        pid == this_pid
            || !process_is_alive(pid)
            || now.saturating_sub(owner_seen) > OWNER_HEARTBEAT_STALE_SECS
    });

    if can_claim {
        write_runtime_state(db, "owner_pid", &this_pid.to_string());
        write_runtime_state(db, "owner_heartbeat", &now.to_string());
        let _ = db.execute("COMMIT");
    } else {
        let _ = db.execute("ROLLBACK");
    }

    can_claim
}

fn heartbeat_owner(db: &sqlite::Connection) {
    write_runtime_state(db, "owner_heartbeat", &unix_now_secs().to_string());
}

fn daily_refresh_date(db: &sqlite::Connection) -> Option<String> {
    ensure_runtime_state_table(db);
    read_runtime_state(db, "last_daily_refresh_date")
}

fn set_daily_refresh_date(db: &sqlite::Connection, date: &str) {
    write_runtime_state(db, "last_daily_refresh_date", date);
    write_runtime_state(db, "last_full_refresh_marker", &full_refresh_marker_value());
}

fn full_refresh_marker(db: &sqlite::Connection) -> Option<String> {
    ensure_runtime_state_table(db);
    read_runtime_state(db, "last_full_refresh_marker")
}

// --- Date helpers ---

/// Returns today's local date as "YYYY-MM-DD".
fn local_date_today() -> String {
    local_date_for_offset(0)
}

/// Returns the local date N days ago as "YYYY-MM-DD".
fn local_date_for_offset(days_ago: i32) -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_mday -= days_ago;
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        libc::mktime(&mut tm); // normalizes overflowed fields
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

// --- File discovery ---

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
        return Vec::new();
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
            let p = sub.path();
            if p.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(p);
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

// --- File helpers ---

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

fn reset_runtime_baseline(
    runtime: &mut HistoryRuntime,
    snapshot: AggregatedStats,
    baselines: HashMap<String, SessionBaseline>,
) {
    runtime.baseline = snapshot;
    runtime.db_session_baselines = baselines;
    runtime.active_delta = AggregatedStats::default();
    runtime.active_sessions.clear();
    runtime.last_active_scan = None;
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

/// Local date string from a file's mtime.
fn file_mtime_date(path: &Path) -> String {
    let ts = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
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

// --- Database ---

fn open_db() -> Option<sqlite::Connection> {
    let dir = crate::config::config_dir();
    let _ = fs::create_dir_all(&dir);
    let db = sqlite::open(dir.join("stats.db")).ok()?;
    db.execute(
        "CREATE TABLE IF NOT EXISTS sessions (
            agent_type TEXT NOT NULL,
            session_id TEXT NOT NULL,
            file_path TEXT NOT NULL,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_creation_tokens INTEGER DEFAULT 0,
            cost_usd REAL DEFAULT 0.0,
            turns INTEGER DEFAULT 0,
            model TEXT,
            last_modified_ts INTEGER DEFAULT 0,
            file_size INTEGER DEFAULT 0,
            PRIMARY KEY (agent_type, session_id)
        )",
    )
    .ok()?;
    // Daily totals: token deltas are attributed to the day they occur.
    // This gives accurate Today/Weekly stats even for long-running sessions.
    db.execute(
        "CREATE TABLE IF NOT EXISTS daily_totals (
            agent_type TEXT NOT NULL,
            date TEXT NOT NULL,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cost_usd REAL DEFAULT 0.0,
            turns INTEGER DEFAULT 0,
            PRIMARY KEY (agent_type, date)
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
    let week_start = local_date_for_offset(6); // 7 calendar days including today

    // Today: only today's date
    fill_daily(db, &mut stats, &today, |s| &mut s.today);
    // Weekly: last 7 calendar days (strict day boundaries)
    fill_daily(db, &mut stats, &week_start, |s| &mut s.seven_days);
    // Total: all time
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
        let key = format!("{agent_type}:{session_id}");
        baselines.insert(
            key,
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

// --- File processing ---

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

    // Skip unchanged files
    if baselines
        .get(&key)
        .is_some_and(|bl| file_is_unchanged(bl, metadata))
    {
        return;
    }

    let tokens = parse_fn(path);
    let cost = session_cost(kind, &tokens);

    // Compute delta and attribute to the correct date
    let current = tokens_to_totals(&tokens, cost);
    let (delta, date) = if let Some(bl) = baselines.get(&key) {
        // Existing session: delta is new activity, attribute to today
        (session_delta(&current, bl), today.to_string())
    } else {
        // New session (or reinitialization): use file mtime date
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
    shared: &Arc<Mutex<HistoryRuntime>>,
    baselines: &mut HashMap<String, SessionBaseline>,
) {
    let today = local_date_today();

    for path in discover_claude_jsonl_files() {
        if let Some(sid) = extract_claude_session_id(&path) {
            process_file(
                db,
                AgentKind::ClaudeCode,
                &sid,
                &path,
                parse_claude_tokens,
                baselines,
                &today,
            );
        }
    }
    for path in discover_codex_jsonl_files() {
        if let Some(sid) = extract_codex_session_id(&path) {
            process_file(
                db,
                AgentKind::Codex,
                &sid,
                &path,
                parse_codex_tokens,
                baselines,
                &today,
            );
        }
    }

    // Recompute snapshot from daily_totals
    let snapshot = compute_snapshot(db);
    reset_runtime_baseline(&mut shared.lock().unwrap(), snapshot, baselines.clone());
}

fn parser_for_kind(kind: AgentKind) -> fn(&Path) -> ParsedTokens {
    match kind {
        AgentKind::ClaudeCode => parse_claude_tokens,
        AgentKind::Codex => parse_codex_tokens,
    }
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

fn current_session_baseline(
    kind: AgentKind,
    path: &Path,
    parse_fn: fn(&Path) -> ParsedTokens,
) -> SessionBaseline {
    let metadata = file_metadata(path);
    let tokens = parse_fn(path);
    let cost = session_cost(kind, &tokens);
    SessionBaseline {
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        cost_usd: cost,
        turns: tokens.turn_count,
        file_size: metadata.size,
        last_modified_ts: metadata.modified_ts,
    }
}

fn session_cost(kind: AgentKind, tokens: &ParsedTokens) -> f64 {
    tokens
        .model
        .as_deref()
        .map(|m| {
            let (cr, cc) = if kind == AgentKind::ClaudeCode {
                (tokens.cache_read_tokens, tokens.cache_creation_tokens)
            } else {
                (0, 0)
            };
            estimate_cost(m, tokens.input_tokens, tokens.output_tokens, cr, cc)
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
    let current_totals = baseline_totals(&current);
    let delta = session_delta(&current_totals, &active.baseline);
    add_agent_delta(&mut runtime.active_delta, active.kind, &delta);
    active.baseline = current;
}

fn finalize_active_session(runtime: &mut HistoryRuntime, key: &str) {
    let Some(active) = runtime.active_sessions.remove(key) else {
        return;
    };
    let current = current_session_baseline(active.kind, &active.path, active.parse_fn);
    let delta = session_delta(&baseline_totals(&current), &active.baseline);
    add_agent_delta(&mut runtime.active_delta, active.kind, &delta);
}

fn refresh_active_sessions(runtime: &mut HistoryRuntime, agents: &[AgentInfo]) {
    let mut seen = HashSet::new();

    for agent in agents {
        let Some(session_id) = agent.session_id.as_deref() else {
            continue;
        };
        let Some(path) = agent.jsonl_path.as_deref() else {
            continue;
        };
        let parse_fn = parser_for_kind(agent.kind);

        let key = active_key(agent.kind, session_id);
        seen.insert(key.clone());

        // Skip re-parsing if file is unchanged since last baseline
        let metadata = file_metadata(path);
        if let Some(active) = runtime.active_sessions.get(&key) {
            if file_is_unchanged(&active.baseline, metadata) {
                continue;
            }
        }

        let current = current_session_baseline(agent.kind, path, parse_fn);

        if runtime.active_sessions.contains_key(&key) {
            record_active_delta(runtime, &key, current);
        } else {
            let db_baseline = runtime
                .db_session_baselines
                .get(&key)
                .cloned()
                .unwrap_or_else(|| zero_baseline(file_metadata(path)));
            let delta = session_delta(&baseline_totals(&current), &db_baseline);
            add_agent_delta(&mut runtime.active_delta, agent.kind, &delta);
            runtime.active_sessions.insert(
                key,
                ActiveSession {
                    kind: agent.kind,
                    path: path.to_path_buf(),
                    parse_fn,
                    baseline: current,
                },
            );
        }
    }

    let stale: Vec<String> = runtime
        .active_sessions
        .keys()
        .filter(|key| !seen.contains(*key))
        .cloned()
        .collect();
    for key in stale {
        finalize_active_session(runtime, &key);
    }
}

// --- Background worker ---

fn background_worker(shared: Arc<Mutex<HistoryRuntime>>) {
    let Some(db) = open_db() else { return };

    let initialized = is_initialized();
    let mut baselines = load_baselines(&db);
    let mut is_owner = try_claim_owner(&db);
    let mut known_refresh_marker = full_refresh_marker(&db);

    if is_owner && !initialized {
        // Reinitializing: clear stale data so sessions are re-attributed
        // to correct dates via file mtime.
        let _ = db.execute("DELETE FROM daily_totals");
        let _ = db.execute("DELETE FROM sessions");
        baselines = HashMap::new();
    }

    // Scan all files: initialization inserts + computes snapshot,
    // or subsequent run catches up with changes since last shutdown.
    if is_owner {
        scan_and_update(&db, &shared, &mut baselines);
        set_daily_refresh_date(&db, &local_date_today());
        known_refresh_marker = full_refresh_marker(&db);
    } else {
        let snapshot = compute_snapshot(&db);
        reset_runtime_baseline(&mut shared.lock().unwrap(), snapshot, baselines.clone());
    }

    if is_owner && !initialized {
        set_initialized();
    }

    // Periodic ownership, daily refresh, and baseline synchronization.
    loop {
        thread::sleep(Duration::from_secs(DELTA_INTERVAL_SECS));
        is_owner = try_claim_owner(&db);

        if is_owner {
            heartbeat_owner(&db);
            let today = local_date_today();
            if daily_refresh_date(&db).as_deref() != Some(today.as_str()) {
                baselines = load_baselines(&db);
                scan_and_update(&db, &shared, &mut baselines);
                set_daily_refresh_date(&db, &today);
                known_refresh_marker = full_refresh_marker(&db);
                continue;
            }
        }

        let refresh_marker = full_refresh_marker(&db);
        if refresh_marker != known_refresh_marker {
            let snapshot = compute_snapshot(&db);
            baselines = load_baselines(&db);
            reset_runtime_baseline(&mut shared.lock().unwrap(), snapshot, baselines.clone());
            known_refresh_marker = refresh_marker;
        }
    }
}

// --- Public API ---

pub struct HistoryStore {
    shared: Arc<Mutex<HistoryRuntime>>,
}

impl HistoryStore {
    pub fn start() -> Self {
        let shared = Arc::new(Mutex::new(HistoryRuntime::default()));
        let bg_shared = Arc::clone(&shared);

        thread::spawn(move || {
            background_worker(bg_shared);
        });

        Self { shared }
    }

    pub fn aggregated_stats(&self, active_agents: &[AgentInfo]) -> AggregatedStats {
        let mut runtime = self.shared.lock().unwrap();
        let current_keys: HashSet<String> = active_agents
            .iter()
            .filter_map(|agent| {
                agent
                    .session_id
                    .as_deref()
                    .map(|session_id| active_key(agent.kind, session_id))
            })
            .collect();
        let has_closed_sessions = runtime
            .active_sessions
            .keys()
            .any(|key| !current_keys.contains(key));
        let should_refresh = runtime
            .last_active_scan
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(DELTA_INTERVAL_SECS));

        if should_refresh || has_closed_sessions {
            refresh_active_sessions(&mut runtime, active_agents);
            runtime.last_active_scan = Some(Instant::now());
        }

        merge_stats(&runtime.baseline, &runtime.active_delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::state::AgentState;
    use std::io::Write;

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
        AgentInfo {
            kind: AgentKind::ClaudeCode,
            pane_id: "%1".to_string(),
            cwd: "/tmp".to_string(),
            window_id: "@1".to_string(),
            window_name: "win".to_string(),
            state: AgentState::Working,
            elapsed_secs: 10,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            cost_usd: 0.0,
            turn_count: 0,
            session_id: Some("s1".to_string()),
            jsonl_path: Some(path.to_path_buf()),
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
        assert_eq!(delta.cost_usd, 0.0);
        assert_eq!(delta.turns, 2);
    }

    #[test]
    fn active_session_overlay_adds_only_delta_from_db_baseline() {
        let path = write_temp_jsonl(
            "active-overlay",
            r#"{"sessionId":"s1","message":{"role":"assistant","id":"m1","usage":{"input_tokens":30,"output_tokens":7}}}
"#,
        );
        let mut runtime = HistoryRuntime::default();
        runtime.db_session_baselines.insert(
            "claude:s1".to_string(),
            SessionBaseline {
                input_tokens: 10,
                output_tokens: 2,
                cost_usd: 0.0,
                turns: 0,
                // Metadata intentionally mismatches the temp file; this test
                // is about active overlay deltas, not unchanged-file skipping.
                file_size: 0,
                last_modified_ts: 0,
            },
        );
        let agent = claude_agent(&path);

        refresh_active_sessions(&mut runtime, &[agent]);

        assert_eq!(runtime.active_delta.claude.today.input_tokens, 20);
        assert_eq!(runtime.active_delta.claude.today.output_tokens, 5);
        assert_eq!(runtime.active_delta.claude.seven_days.input_tokens, 20);
        assert_eq!(runtime.active_delta.claude.total.input_tokens, 20);

        let _ = fs::remove_file(path);
    }
}
