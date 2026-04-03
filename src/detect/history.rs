use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use super::estimate_cost;
use super::state::{
    codex_sessions_dir, extract_claude_session_id, extract_codex_session_id, parse_claude_tokens,
    parse_codex_tokens, ParsedTokens,
};

const SCAN_INTERVAL_SECS: u64 = 300; // 5 minutes

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

struct SessionBaseline {
    file_size: u64,
}

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("agentmux")
}

// --- Config (initialized flag) ---

fn config_toml_path() -> PathBuf {
    config_dir().join("config.toml")
}

fn is_initialized() -> bool {
    fs::read_to_string(config_toml_path())
        .unwrap_or_default()
        .lines()
        .any(|l| l.trim() == "initialized = true")
}

fn set_initialized() {
    let path = config_toml_path();
    let _ = fs::create_dir_all(config_dir());
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<&str> = content
        .lines()
        .filter(|l| !l.trim().starts_with("initialized"))
        .collect();
    lines.push("initialized = true");
    let _ = fs::write(path, lines.join("\n") + "\n");
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
    walk_jsonl(&sessions_dir, &mut files);
    files
}

fn walk_jsonl(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
}

// --- File helpers ---

fn file_mtime_ts(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0)
}

// --- Database ---

fn open_db() -> Option<sqlite::Connection> {
    let _ = fs::create_dir_all(config_dir());
    let db = sqlite::open(config_dir().join("stats.db")).ok()?;
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
    Some(db)
}

fn upsert_session(
    db: &sqlite::Connection,
    agent_type: &str,
    session_id: &str,
    file_path: &Path,
    tokens: &ParsedTokens,
    cost: f64,
    fsize: u64,
) {
    let sql = "INSERT OR REPLACE INTO sessions \
               (agent_type, session_id, file_path, input_tokens, output_tokens, \
                cache_read_tokens, cache_creation_tokens, cost_usd, turns, model, \
                last_modified_ts, file_size) \
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    let Ok(mut stmt) = db.prepare(sql) else { return };
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
    let _ = stmt.bind((11, file_mtime_ts(file_path) as i64));
    let _ = stmt.bind((12, fsize as i64));
    let _ = stmt.next();
}

fn midnight_local_today() -> u64 {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        libc::mktime(&mut tm) as u64
    }
}

fn fill_period(
    db: &sqlite::Connection,
    stats: &mut AggregatedStats,
    since_ts: u64,
    accessor: fn(&mut AgentPeriodStats) -> &mut AgentTotals,
) {
    let sql = "SELECT agent_type, SUM(input_tokens), SUM(output_tokens), \
               SUM(cost_usd), SUM(turns) FROM sessions \
               WHERE last_modified_ts >= ? GROUP BY agent_type";
    let Ok(mut stmt) = db.prepare(sql) else { return };
    let _ = stmt.bind((1, since_ts as i64));
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

fn compute_snapshot(db: &sqlite::Connection) -> AggregatedStats {
    let mut stats = AggregatedStats::default();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seven_days_ago = now.saturating_sub(7 * 86400);
    let midnight = midnight_local_today();

    fill_period(db, &mut stats, 0, |s| &mut s.total);
    fill_period(db, &mut stats, seven_days_ago, |s| &mut s.seven_days);
    fill_period(db, &mut stats, midnight, |s| &mut s.today);

    stats
}

fn load_baselines(db: &sqlite::Connection) -> HashMap<String, SessionBaseline> {
    let mut baselines = HashMap::new();
    let sql = "SELECT agent_type, session_id, file_size FROM sessions";
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
                file_size: stmt.read::<i64, _>(2).unwrap_or(0) as u64,
            },
        );
    }
    baselines
}

// --- File processing ---

fn process_file(
    db: &sqlite::Connection,
    agent_type: &str,
    session_id: &str,
    path: &Path,
    parse_fn: fn(&Path) -> ParsedTokens,
    baselines: &mut HashMap<String, SessionBaseline>,
) {
    let fsize = file_size(path);
    let key = format!("{agent_type}:{session_id}");

    // Skip unchanged files
    if baselines.get(&key).is_some_and(|bl| bl.file_size == fsize) {
        return;
    }

    let tokens = parse_fn(path);
    let cost = tokens
        .model
        .as_deref()
        .map(|m| {
            let (cr, cc) = if agent_type == "claude" {
                (tokens.cache_read_tokens, tokens.cache_creation_tokens)
            } else {
                (0, 0)
            };
            estimate_cost(m, tokens.input_tokens, tokens.output_tokens, cr, cc)
        })
        .unwrap_or(0.0);

    upsert_session(db, agent_type, session_id, path, &tokens, cost, fsize);

    baselines.insert(key, SessionBaseline { file_size: fsize });
}

fn scan_and_update(
    db: &sqlite::Connection,
    shared: &Arc<Mutex<AggregatedStats>>,
    baselines: &mut HashMap<String, SessionBaseline>,
) {
    for path in discover_claude_jsonl_files() {
        if let Some(sid) = extract_claude_session_id(&path) {
            process_file(db, "claude", &sid, &path, parse_claude_tokens, baselines);
        }
    }
    for path in discover_codex_jsonl_files() {
        if let Some(sid) = extract_codex_session_id(&path) {
            process_file(db, "codex", &sid, &path, parse_codex_tokens, baselines);
        }
    }

    // Recompute snapshot from DB (covers all time periods including shifting today boundary)
    let snapshot = compute_snapshot(db);
    *shared.lock().unwrap() = snapshot;
}

// --- Background worker ---

fn background_worker(shared: Arc<Mutex<AggregatedStats>>) {
    let Some(db) = open_db() else { return };

    let initialized = is_initialized();
    let mut baselines;

    if initialized {
        // Load snapshot from DB, then baselines for delta tracking
        let snapshot = compute_snapshot(&db);
        *shared.lock().unwrap() = snapshot;
        baselines = load_baselines(&db);
    } else {
        // First time: baselines are empty, all sessions treated as new
        baselines = HashMap::new();
    }

    // Scan all files: initialization inserts + computes snapshot,
    // or subsequent run catches up with changes since last shutdown.
    scan_and_update(&db, &shared, &mut baselines);

    if !initialized {
        set_initialized();
    }

    // Periodic scan
    loop {
        thread::sleep(Duration::from_secs(SCAN_INTERVAL_SECS));
        scan_and_update(&db, &shared, &mut baselines);
    }
}

// --- Public API ---

pub struct HistoryStore {
    shared: Arc<Mutex<AggregatedStats>>,
}

impl HistoryStore {
    pub fn start() -> Self {
        let shared = Arc::new(Mutex::new(AggregatedStats::default()));
        let bg_shared = Arc::clone(&shared);

        thread::spawn(move || {
            background_worker(bg_shared);
        });

        Self { shared }
    }

    pub fn aggregated_stats(&self) -> AggregatedStats {
        self.shared.lock().unwrap().clone()
    }
}
