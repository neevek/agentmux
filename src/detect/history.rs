use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use super::estimate_cost;
use super::state::{codex_sessions_dir, parse_claude_tokens, parse_codex_tokens};

/// Bump this to invalidate stale archived_totals.json from buggy runs.
const ARCHIVE_VERSION: u32 = 2;

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("agentpane")
}

// --- Data structures ---

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub turns: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ArchivedTotals {
    #[serde(default)]
    version: u32,
    claude: AgentTotals,
    codex: AgentTotals,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AgentType {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrackedSession {
    agent_type: AgentType,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: f64,
    turns: u32,
    model: Option<String>,
    file_size: u64,
    last_modified_ts: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionRegistry {
    sessions: HashMap<String, TrackedSession>,
}

#[derive(Debug, Clone, Default)]
pub struct AggregatedStats {
    pub claude: AgentTotals,
    pub codex: AgentTotals,
}

pub struct HistoryStore {
    registry: SessionRegistry,
    archived: ArchivedTotals,
    last_full_scan: Instant,
}

// --- Discovery ---

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

// --- Persistence ---

fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(path: &Path, data: &T) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(data) {
        let _ = fs::write(path, json);
    }
}

fn file_mtime_ts(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --- HistoryStore ---

impl HistoryStore {
    pub fn load() -> Self {
        let dir = config_dir();
        let mut archived: ArchivedTotals = load_json(&dir.join("archived_totals.json"));
        if archived.version != ARCHIVE_VERSION {
            archived = ArchivedTotals {
                version: ARCHIVE_VERSION,
                ..Default::default()
            };
        }
        Self {
            registry: load_json(&dir.join("session_registry.json")),
            archived,
            last_full_scan: Instant::now(),
        }
    }

    fn save(&self) {
        let dir = config_dir();
        save_json(&dir.join("session_registry.json"), &self.registry);
        save_json(&dir.join("archived_totals.json"), &self.archived);
    }

    pub fn full_scan(&mut self) {
        let mut seen_keys: HashSet<String> = HashSet::new();

        // Claude files
        for path in discover_claude_jsonl_files() {
            let key = path.to_string_lossy().to_string();
            seen_keys.insert(key.clone());

            let file_size = fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0);
            if self
                .registry
                .sessions
                .get(&key)
                .is_some_and(|s| s.file_size == file_size)
            {
                continue;
            }

            let tokens = parse_claude_tokens(&path);
            let cost = tokens
                .model
                .as_deref()
                .map(|m| {
                    estimate_cost(
                        m,
                        tokens.input_tokens,
                        tokens.output_tokens,
                        tokens.cache_read_tokens,
                        tokens.cache_creation_tokens,
                    )
                })
                .unwrap_or(0.0);

            self.registry.sessions.insert(
                key,
                TrackedSession {
                    agent_type: AgentType::Claude,
                    input_tokens: tokens.input_tokens,
                    output_tokens: tokens.output_tokens,
                    cache_read_tokens: tokens.cache_read_tokens,
                    cache_creation_tokens: tokens.cache_creation_tokens,
                    cost_usd: cost,
                    turns: tokens.turn_count,
                    model: tokens.model,
                    file_size,
                    last_modified_ts: file_mtime_ts(&path),
                },
            );
        }

        // Codex files
        for path in discover_codex_jsonl_files() {
            let key = path.to_string_lossy().to_string();
            seen_keys.insert(key.clone());

            let file_size = fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0);
            if self
                .registry
                .sessions
                .get(&key)
                .is_some_and(|s| s.file_size == file_size)
            {
                continue;
            }

            let tokens = parse_codex_tokens(&path);
            let cost = tokens
                .model
                .as_deref()
                .map(|m| estimate_cost(m, tokens.input_tokens, tokens.output_tokens, 0, 0))
                .unwrap_or(0.0);

            self.registry.sessions.insert(
                key,
                TrackedSession {
                    agent_type: AgentType::Codex,
                    input_tokens: tokens.input_tokens,
                    output_tokens: tokens.output_tokens,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    cost_usd: cost,
                    turns: tokens.turn_count,
                    model: tokens.model,
                    file_size,
                    last_modified_ts: file_mtime_ts(&path),
                },
            );
        }

        // Archive entries for files that no longer exist on disk
        let disappeared: Vec<String> = self
            .registry
            .sessions
            .keys()
            .filter(|k| !seen_keys.contains(k.as_str()))
            .cloned()
            .collect();
        for key in disappeared {
            self.archive_entry(&key);
        }

        self.last_full_scan = Instant::now();
        self.save();
    }

    fn archive_entry(&mut self, key: &str) {
        if let Some(session) = self.registry.sessions.remove(key) {
            let totals = match session.agent_type {
                AgentType::Claude => &mut self.archived.claude,
                AgentType::Codex => &mut self.archived.codex,
            };
            totals.input_tokens += session.input_tokens;
            totals.output_tokens += session.output_tokens;
            totals.cost_usd += session.cost_usd;
            totals.turns += session.turns;
        }
    }

    pub fn should_rescan(&self) -> bool {
        self.last_full_scan.elapsed().as_secs() >= 60
    }

    pub fn aggregated_stats(&self) -> AggregatedStats {
        let mut claude = self.archived.claude.clone();
        let mut codex = self.archived.codex.clone();

        for session in self.registry.sessions.values() {
            let totals = match session.agent_type {
                AgentType::Claude => &mut claude,
                AgentType::Codex => &mut codex,
            };
            totals.input_tokens += session.input_tokens;
            totals.output_tokens += session.output_tokens;
            totals.cost_usd += session.cost_usd;
            totals.turns += session.turns;
        }

        AggregatedStats { claude, codex }
    }
}
