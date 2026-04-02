use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Working,
    Idle,
}

#[derive(Debug, Clone)]
pub struct SessionDetails {
    pub state: AgentState,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_activity: Option<String>,
    pub context_pct: Option<u8>,
}

impl Default for SessionDetails {
    fn default() -> Self {
        Self {
            state: AgentState::Idle,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
        }
    }
}

/// Cache for JSONL parsing — avoids re-reading entire files every poll cycle.
/// Only re-parses when file size changes.
pub struct SessionCache {
    entries: HashMap<PathBuf, CachedData>,
}

#[derive(Clone)]
struct CachedData {
    file_size: u64,
    input_tokens: u64,
    output_tokens: u64,
    last_activity: Option<String>,
    context_pct: Option<u8>,
}

impl SessionCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn get_or_update(
        &mut self,
        path: &PathBuf,
        parser: fn(&PathBuf) -> ParsedTokens,
    ) -> CachedData {
        let current_size = fs::metadata(path)
            .ok()
            .map(|m| m.len())
            .unwrap_or(0);

        if let Some(cached) = self.entries.get(path) {
            if cached.file_size == current_size {
                return cached.clone();
            }
        }

        // File grew or first read — do full parse
        let parsed = parser(path);
        let data = CachedData {
            file_size: current_size,
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            last_activity: parsed.last_activity,
            context_pct: parsed.context_pct,
        };
        self.entries.insert(path.clone(), data.clone());
        data
    }
}

struct ParsedTokens {
    input_tokens: u64,
    output_tokens: u64,
    last_activity: Option<String>,
    context_pct: Option<u8>,
}

const ACTIVE_WRITE_THRESHOLD: Duration = Duration::from_secs(3);
const TAIL_BYTES: u64 = 32768;

pub fn claude_code_details(cwd: &str, cache: &mut SessionCache) -> SessionDetails {
    let Some(home) = dirs::home_dir() else {
        return SessionDetails::default();
    };
    let encoded = encode_project_dir(cwd);
    let projects_dir = home.join(".claude").join("projects").join(&encoded);

    let Some(jsonl_path) = find_most_recent_jsonl(&projects_dir) else {
        return SessionDetails::default();
    };

    let recently_active = file_recently_modified(&jsonl_path, ACTIVE_WRITE_THRESHOLD);

    // State detection: always do a fast tail read
    let state = if recently_active {
        AgentState::Working
    } else {
        detect_claude_state(&jsonl_path)
    };

    // Token counting: cached, only re-reads when file grows
    let cached = cache.get_or_update(&jsonl_path, parse_claude_tokens);

    SessionDetails {
        state,
        input_tokens: cached.input_tokens,
        output_tokens: cached.output_tokens,
        last_activity: cached.last_activity,
        context_pct: cached.context_pct,
    }
}

pub fn codex_details(cwd: &str, cache: &mut SessionCache) -> SessionDetails {
    let Some(sessions_dir) = codex_sessions_dir() else {
        return SessionDetails::default();
    };

    // Find the JSONL file whose session_meta.payload.cwd matches this agent's cwd
    let Some(jsonl_path) = find_codex_jsonl_for_cwd(&sessions_dir, cwd) else {
        return SessionDetails::default();
    };

    let recently_active = file_recently_modified(&jsonl_path, ACTIVE_WRITE_THRESHOLD);

    let state = if recently_active {
        AgentState::Working
    } else {
        detect_codex_state(&jsonl_path)
    };

    let cached = cache.get_or_update(&jsonl_path, parse_codex_tokens);

    SessionDetails {
        state,
        input_tokens: cached.input_tokens,
        output_tokens: cached.output_tokens,
        last_activity: cached.last_activity,
        context_pct: cached.context_pct,
    }
}

fn file_recently_modified(path: &PathBuf, threshold: Duration) -> bool {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_some_and(|age| age < threshold)
}

fn encode_project_dir(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            '/' | '.' | '_' => '-',
            _ => c,
        })
        .collect()
}

fn codex_sessions_dir() -> Option<PathBuf> {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        let p = PathBuf::from(codex_home).join("sessions");
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = dirs::home_dir()?;
    let p = home.join(".codex").join("sessions");
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

fn find_most_recent_jsonl(dir: &PathBuf) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "jsonl")
        })
        .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        .map(|e| e.path())
}

fn find_most_recent_jsonl_recursive(dir: &PathBuf) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let mut best: Option<(PathBuf, SystemTime)> = None;

    fn walk(dir: &PathBuf, best: &mut Option<(PathBuf, SystemTime)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, best);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        let dominated = best
                            .as_ref()
                            .is_none_or(|(_, prev_mtime)| mtime > *prev_mtime);
                        if dominated {
                            *best = Some((path, mtime));
                        }
                    }
                }
            }
        }
    }

    walk(dir, &mut best);
    best.map(|(p, _)| p)
}

/// Find the Codex JSONL file whose session_meta.payload.cwd matches the given cwd.
/// Scans all JSONL files, reads only the first line of each to check the cwd.
/// Falls back to the most recent file if no match is found.
fn find_codex_jsonl_for_cwd(sessions_dir: &PathBuf, cwd: &str) -> Option<PathBuf> {
    if !sessions_dir.is_dir() {
        return None;
    }

    let mut all_files: Vec<(PathBuf, SystemTime)> = Vec::new();

    fn walk(dir: &PathBuf, files: &mut Vec<(PathBuf, SystemTime)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        files.push((path, mtime));
                    }
                }
            }
        }
    }

    walk(sessions_dir, &mut all_files);
    if all_files.is_empty() {
        return None;
    }

    // Sort by modification time descending (most recent first)
    all_files.sort_by(|a, b| b.1.cmp(&a.1));

    // Check each file's first line for session_meta with matching cwd
    for (path, _) in &all_files {
        if let Some(session_cwd) = read_codex_session_cwd(path) {
            if session_cwd == cwd {
                return Some(path.clone());
            }
        }
    }

    // Fallback: most recent file
    Some(all_files.into_iter().next()?.0)
}

/// Read the first line of a Codex JSONL file to extract session_meta.payload.cwd.
fn read_codex_session_cwd(path: &PathBuf) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    let entry: Value = serde_json::from_str(&first_line).ok()?;
    if entry.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    entry
        .get("payload")
        .and_then(|p| p.get("cwd"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

/// Fast tail read — only last 32KB, used for state detection every poll cycle.
fn tail_read_jsonl(path: &PathBuf) -> Vec<Value> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    let size = meta.len();
    if size > TAIL_BYTES {
        let _ = file.seek(SeekFrom::Start(size - TAIL_BYTES));
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    buf.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// Full file read — used for token counting, only called when file size changes.
fn full_read_jsonl(path: &PathBuf) -> Vec<Value> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    buf.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens == 0 {
        return String::new();
    }
    if tokens < 1000 {
        format!("{tokens}")
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

// ---- State detection (fast, tail read) ----

fn detect_claude_state(path: &PathBuf) -> AgentState {
    let entries = tail_read_jsonl(path);
    for entry in entries.iter().rev() {
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let Some(role) = msg.get("role").and_then(|r| r.as_str()) else {
            continue;
        };
        match role {
            "assistant" => {
                let content = msg.get("content");
                if let Some(Value::Array(items)) = content {
                    if items
                        .iter()
                        .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                    {
                        return AgentState::Working;
                    }
                    if items
                        .iter()
                        .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("thinking"))
                    {
                        return AgentState::Working;
                    }
                }
                match msg.get("stop_reason") {
                    Some(Value::String(reason)) => {
                        return if reason == "end_turn" {
                            AgentState::Idle
                        } else if reason == "tool_use" {
                            AgentState::Working
                        } else {
                            AgentState::Idle
                        };
                    }
                    Some(Value::Null) | None => return AgentState::Working,
                    _ => {}
                }
            }
            "user" => {
                let content = msg.get("content");
                let text = match content {
                    Some(Value::String(s)) => Some(s.as_str()),
                    Some(Value::Array(items)) => items.iter().find_map(|c| {
                        if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                            c.get("text").and_then(|t| t.as_str())
                        } else {
                            None
                        }
                    }),
                    _ => None,
                };
                if let Some(text) = text {
                    if text.starts_with("[Request interrupted") {
                        return AgentState::Idle;
                    }
                    if text.contains("<command-name>/exit</command-name>") {
                        return AgentState::Idle;
                    }
                    if text.starts_with('<') || text.starts_with('{') {
                        continue;
                    }
                }
                if let Some(Value::Array(items)) = content {
                    if items
                        .iter()
                        .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    {
                        return AgentState::Working;
                    }
                }
                return AgentState::Working;
            }
            _ => continue,
        }
    }
    AgentState::Idle
}

fn detect_codex_state(path: &PathBuf) -> AgentState {
    let entries = tail_read_jsonl(path);
    for entry in entries.iter().rev() {
        let entry_type = entry.get("type").and_then(|t| t.as_str());
        match entry_type {
            Some("event_msg") => {
                if let Some(pt) = entry
                    .get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                {
                    match pt {
                        "task_started" | "user_message" => return AgentState::Working,
                        "task_complete" | "turn_aborted" => return AgentState::Idle,
                        "agent_message" => {
                            let phase = entry
                                .get("payload")
                                .and_then(|p| p.get("phase"))
                                .and_then(|p| p.as_str());
                            return if phase == Some("final_answer") {
                                AgentState::Idle
                            } else {
                                AgentState::Working
                            };
                        }
                        "token_count" => continue,
                        _ => continue,
                    }
                }
            }
            Some("response_item") => {
                if let Some(it) = entry
                    .get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                {
                    if it == "function_call" {
                        return AgentState::Working;
                    }
                    if it == "message" {
                        let phase = entry
                            .get("payload")
                            .and_then(|p| p.get("phase"))
                            .and_then(|p| p.as_str());
                        if phase == Some("final_answer") {
                            return AgentState::Idle;
                        }
                    }
                }
            }
            _ => {
                if let Some(role) = entry.get("role").and_then(|r| r.as_str()) {
                    return if role == "user" {
                        AgentState::Working
                    } else {
                        AgentState::Idle
                    };
                }
            }
        }
    }
    AgentState::Idle
}

// ---- Token counting (cached, full read) ----

fn parse_claude_tokens(path: &PathBuf) -> ParsedTokens {
    let entries = full_read_jsonl(path);
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut last_activity: Option<String> = None;
    let mut last_turn_input: u64 = 0;
    let mut model_name: Option<String> = None;

    for entry in &entries {
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let Some(role) = msg.get("role").and_then(|r| r.as_str()) else {
            continue;
        };
        if role == "assistant" {
            if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                model_name = Some(m.to_string());
            }
            if let Some(usage) = msg.get("usage") {
                let inp = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cache_create = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let out = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let turn_input = inp + cache_read + cache_create;
                input_tokens += turn_input;
                output_tokens += out;
                last_turn_input = turn_input;
            }
            if let Some(Value::Array(items)) = msg.get("content") {
                for item in items {
                    if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                            last_activity = Some(extract_tool_detail(name, item));
                        }
                    }
                }
            }
        }
    }

    let context_pct = model_name.as_deref().and_then(|m| {
        let max_ctx = model_context_window(m)?;
        if last_turn_input > 0 && max_ctx > 0 {
            Some(((last_turn_input as f64 / max_ctx as f64) * 100.0).min(100.0) as u8)
        } else {
            None
        }
    });

    ParsedTokens {
        input_tokens,
        output_tokens,
        last_activity,
        context_pct,
    }
}

fn parse_codex_tokens(path: &PathBuf) -> ParsedTokens {
    let entries = full_read_jsonl(path);
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut last_activity: Option<String> = None;
    let mut context_window: u64 = 0;
    let mut last_turn_input: u64 = 0;

    for entry in &entries {
        let entry_type = entry.get("type").and_then(|t| t.as_str());

        if entry_type == Some("event_msg") {
            let pt = entry
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str());
            if pt == Some("token_count") {
                let info = entry.get("payload").and_then(|p| p.get("info"));
                if let Some(info) = info {
                    if let Some(u) = info.get("total_token_usage") {
                        input_tokens =
                            u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        output_tokens =
                            u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                    if let Some(u) = info.get("last_token_usage") {
                        last_turn_input =
                            u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                    if let Some(cw) = info.get("model_context_window").and_then(|v| v.as_u64()) {
                        context_window = cw;
                    }
                }
            }
        }

        if entry_type == Some("response_item") {
            let it = entry
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str());
            if it == Some("function_call") {
                let name = entry
                    .get("payload")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool");
                last_activity = Some(name.to_string());
            }
        }
    }

    let context_pct = if context_window > 0 && last_turn_input > 0 {
        Some(((last_turn_input as f64 / context_window as f64) * 100.0).min(100.0) as u8)
    } else {
        None
    };

    ParsedTokens {
        input_tokens,
        output_tokens,
        last_activity,
        context_pct,
    }
}

fn model_context_window(model: &str) -> Option<u64> {
    if model.contains("opus") {
        Some(1_000_000)
    } else if model.contains("sonnet") {
        Some(200_000)
    } else if model.contains("haiku") {
        Some(200_000)
    } else {
        None
    }
}

fn extract_tool_detail(name: &str, item: &Value) -> String {
    let input = item.get("input");
    match name {
        "Edit" | "Write" | "Read" => {
            let path = input
                .and_then(|i| i.get("file_path"))
                .and_then(|p| p.as_str())
                .and_then(|p| p.rsplit('/').next())
                .unwrap_or("");
            format!("{name} {path}")
        }
        "Bash" => {
            let cmd = input
                .and_then(|i| i.get("command"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let short = cmd.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
            format!("Bash {short}")
        }
        "Grep" | "Glob" => {
            let pat = input
                .and_then(|i| i.get("pattern"))
                .and_then(|p| p.as_str())
                .unwrap_or("");
            format!("{name} {pat}")
        }
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_project_dir() {
        assert_eq!(
            encode_project_dir("/Users/foo/myproject"),
            "-Users-foo-myproject"
        );
    }

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(0), "");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }
}
