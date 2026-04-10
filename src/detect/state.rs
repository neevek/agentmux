use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
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
    /// Cache read tokens (subset of input_tokens, for cost calc)
    pub cache_read_tokens: u64,
    /// Cache creation tokens (subset of input_tokens, for cost calc)
    pub cache_creation_tokens: u64,
    pub last_activity: Option<String>,
    pub context_pct: Option<u8>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub turn_count: u32,
    pub jsonl_path: Option<PathBuf>,
}

impl Default for SessionDetails {
    fn default() -> Self {
        Self {
            state: AgentState::Idle,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            turn_count: 0,
            jsonl_path: None,
        }
    }
}

pub struct SessionCache {
    entries: HashMap<PathBuf, CachedData>,
}

#[derive(Clone)]
struct CachedData {
    metadata: FileMetadata,
    tokens: ParsedTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileMetadata {
    pub(crate) size: u64,
    pub(crate) modified_ts: u64,
}

#[derive(Clone)]
pub(crate) struct ParsedTokens {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) last_activity: Option<String>,
    pub(crate) context_pct: Option<u8>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) turn_count: u32,
}

impl SessionCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn get_or_update(&mut self, path: &Path, parser: fn(&Path) -> ParsedTokens) -> &ParsedTokens {
        let current_metadata = file_metadata(path);

        let needs_update = self
            .entries
            .get(path)
            .is_none_or(|c| c.metadata != current_metadata);

        if needs_update {
            let tokens = parser(path);
            self.entries.insert(
                path.to_path_buf(),
                CachedData {
                    metadata: current_metadata,
                    tokens,
                },
            );
        }

        &self.entries[path].tokens
    }
}

const ACTIVE_WRITE_THRESHOLD: Duration = Duration::from_secs(3);
const TAIL_BYTES: u64 = 32768;

// --- Public API ---

pub fn claude_code_details(
    cwd: &str,
    agent_age_secs: u64,
    cache: &mut SessionCache,
) -> SessionDetails {
    let Some(home) = dirs::home_dir() else {
        return SessionDetails::default();
    };
    let encoded = encode_project_dir(cwd);
    let projects_dir = home.join(".claude").join("projects").join(&encoded);
    let jsonl_path = find_most_recent_jsonl(&projects_dir);
    agent_details(
        jsonl_path.as_deref(),
        agent_age_secs,
        cache,
        detect_claude_state,
        parse_claude_tokens,
    )
}

pub fn codex_details(cwd: &str, agent_age_secs: u64, cache: &mut SessionCache) -> SessionDetails {
    let sessions_dir = codex_sessions_dir();
    let jsonl_path = sessions_dir
        .as_ref()
        .and_then(|d| find_codex_jsonl_for_cwd(d, cwd));
    agent_details(
        jsonl_path.as_deref(),
        agent_age_secs,
        cache,
        detect_codex_state,
        parse_codex_tokens,
    )
}

fn agent_details(
    jsonl_path: Option<&Path>,
    agent_age_secs: u64,
    cache: &mut SessionCache,
    detect_state: fn(&Path) -> AgentState,
    parse_tokens: fn(&Path) -> ParsedTokens,
) -> SessionDetails {
    let Some(path) = jsonl_path else {
        return SessionDetails::default();
    };
    if file_older_than_process(path, agent_age_secs) {
        return SessionDetails::default();
    }

    let state = if file_recently_modified(path, ACTIVE_WRITE_THRESHOLD) {
        AgentState::Working
    } else {
        detect_state(path)
    };

    let cached = cache.get_or_update(path, parse_tokens);
    SessionDetails {
        state,
        input_tokens: cached.input_tokens,
        output_tokens: cached.output_tokens,
        cache_read_tokens: cached.cache_read_tokens,
        cache_creation_tokens: cached.cache_creation_tokens,
        last_activity: cached.last_activity.clone(),
        context_pct: cached.context_pct,
        model: cached.model.clone(),
        effort: cached.effort.clone(),
        turn_count: cached.turn_count,
        jsonl_path: Some(path.to_path_buf()),
    }
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens == 0 {
        return "0".to_string();
    }
    if tokens < 1000 {
        format!("{tokens}")
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

// --- Helpers ---

fn json_str<'a>(val: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = val;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

fn file_older_than_process(path: &Path, agent_age_secs: u64) -> bool {
    let file_age = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    file_age > agent_age_secs + 5
}

fn file_recently_modified(path: &Path, threshold: Duration) -> bool {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_some_and(|age| age < threshold)
}

pub(crate) fn file_metadata(path: &Path) -> FileMetadata {
    let Ok(metadata) = fs::metadata(path) else {
        return FileMetadata {
            size: 0,
            modified_ts: 0,
        };
    };
    let modified_ts = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    FileMetadata {
        size: metadata.len(),
        modified_ts,
    }
}

fn encode_project_dir(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            '/' | '.' | '_' => '-',
            _ => c,
        })
        .collect()
}

pub(crate) fn codex_sessions_dir() -> Option<PathBuf> {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        let p = PathBuf::from(codex_home).join("sessions");
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = dirs::home_dir()?;
    let p = home.join(".codex").join("sessions");
    if p.is_dir() { Some(p) } else { None }
}

fn find_most_recent_jsonl(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        .map(|e| e.path())
}

fn find_codex_jsonl_for_cwd(sessions_dir: &Path, cwd: &str) -> Option<PathBuf> {
    if !sessions_dir.is_dir() {
        return None;
    }
    let mut all_files: Vec<(PathBuf, SystemTime)> = Vec::new();

    fn walk(dir: &Path, files: &mut Vec<(PathBuf, SystemTime)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files);
            } else if let Some(mtime) = path
                .extension()
                .is_some_and(|ext| ext == "jsonl")
                .then(|| entry.metadata().ok().and_then(|m| m.modified().ok()))
                .flatten()
            {
                files.push((path, mtime));
            }
        }
    }

    walk(sessions_dir, &mut all_files);
    if all_files.is_empty() {
        return None;
    }
    all_files.sort_by(|a, b| b.1.cmp(&a.1));

    for (path, _) in &all_files {
        if read_codex_session_cwd(path).is_some_and(|s| s == cwd) {
            return Some(path.clone());
        }
    }
    Some(all_files.into_iter().next()?.0)
}

fn read_codex_session_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    let entry: Value = serde_json::from_str(&first_line).ok()?;
    if json_str(&entry, &["type"]) != Some("session_meta") {
        return None;
    }
    json_str(&entry, &["payload", "cwd"]).map(|s| s.to_string())
}

fn read_jsonl(path: &Path, tail_bytes: Option<u64>) -> Vec<Value> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    if let Some(tail) = tail_bytes {
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        if size > tail {
            let _ = file.seek(SeekFrom::Start(size - tail));
        }
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    buf.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

// --- State detection (fast tail read) ---

fn detect_claude_state(path: &Path) -> AgentState {
    for entry in read_jsonl(path, Some(TAIL_BYTES)).iter().rev() {
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let Some(role) = json_str(msg, &["role"]) else {
            continue;
        };
        match role {
            "assistant" => {
                if let Some(Value::Array(items)) = msg.get("content") {
                    let has = |t| items.iter().any(|c| json_str(c, &["type"]) == Some(t));
                    if has("tool_use") || has("thinking") {
                        return AgentState::Working;
                    }
                }
                return match msg.get("stop_reason") {
                    Some(Value::String(r)) if r == "end_turn" => AgentState::Idle,
                    Some(Value::String(r)) if r == "tool_use" => AgentState::Working,
                    Some(Value::String(_)) => AgentState::Idle,
                    Some(Value::Null) | None => AgentState::Working,
                    _ => continue,
                };
            }
            "user" => {
                let text = match msg.get("content") {
                    Some(Value::String(s)) => Some(s.as_str()),
                    Some(Value::Array(items)) => items.iter().find_map(|c| {
                        if json_str(c, &["type"]) == Some("text") {
                            c.get("text")?.as_str()
                        } else {
                            None
                        }
                    }),
                    _ => None,
                };
                if let Some(text) = text {
                    if text.starts_with("[Request interrupted")
                        || text.contains("<command-name>/exit</command-name>")
                    {
                        return AgentState::Idle;
                    }
                    if text.starts_with('<') || text.starts_with('{') {
                        continue;
                    }
                }
                if matches!(msg.get("content"), Some(Value::Array(items)) if items.iter().any(|c| json_str(c, &["type"]) == Some("tool_result")))
                {
                    return AgentState::Working;
                }
                return AgentState::Working;
            }
            _ => continue,
        }
    }
    AgentState::Idle
}

fn detect_codex_state(path: &Path) -> AgentState {
    for entry in read_jsonl(path, Some(TAIL_BYTES)).iter().rev() {
        match json_str(entry, &["type"]) {
            Some("event_msg") => {
                if let Some(pt) = json_str(entry, &["payload", "type"]) {
                    match pt {
                        "task_started" | "user_message" => return AgentState::Working,
                        "task_complete" | "turn_aborted" => return AgentState::Idle,
                        "agent_message" => {
                            return if json_str(entry, &["payload", "phase"]) == Some("final_answer")
                            {
                                AgentState::Idle
                            } else {
                                AgentState::Working
                            };
                        }
                        _ => continue,
                    }
                }
            }
            Some("response_item") => {
                if let Some(it) = json_str(entry, &["payload", "type"]) {
                    if it == "function_call" {
                        return AgentState::Working;
                    }
                    if it == "message"
                        && json_str(entry, &["payload", "phase"]) == Some("final_answer")
                    {
                        return AgentState::Idle;
                    }
                }
            }
            _ => {
                if let Some(role) = json_str(entry, &["role"]) {
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

// --- Token counting (cached, full read) ---

pub(crate) fn parse_claude_tokens(path: &Path) -> ParsedTokens {
    let entries = read_jsonl(path, None);
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut cache_read_tokens: u64 = 0;
    let mut cache_creation_tokens: u64 = 0;
    let mut last_activity: Option<String> = None;
    let mut last_turn_input: u64 = 0;
    let mut model_name: Option<String> = None;
    let mut assistant_messages: Vec<&Value> = Vec::new();
    let mut assistant_id_indexes: HashMap<String, usize> = HashMap::new();
    let mut turn_count: u32 = 0;

    for entry in &entries {
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let role = json_str(msg, &["role"]);

        if role == Some("user") {
            // Count user turns: skip pure tool_result arrays and system-like messages
            let is_tool_result_only = matches!(msg.get("content"), Some(Value::Array(items))
                if !items.is_empty() && items.iter().all(|c| json_str(c, &["type"]) == Some("tool_result")));
            if !is_tool_result_only {
                let text = match msg.get("content") {
                    Some(Value::String(s)) => Some(s.as_str()),
                    Some(Value::Array(items)) => items.iter().find_map(|c| {
                        if json_str(c, &["type"]) == Some("text") {
                            c.get("text")?.as_str()
                        } else {
                            None
                        }
                    }),
                    _ => None,
                };
                if !text.is_some_and(|t| t.starts_with('<') || t.starts_with('{')) {
                    turn_count += 1;
                }
            }
            continue;
        }

        if role != Some("assistant") {
            continue;
        }

        // Claude Code writes streaming + final entries with the same id; keep the final entry.
        if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
            if let Some(index) = assistant_id_indexes.get(id).copied() {
                assistant_messages[index] = msg;
            } else {
                assistant_id_indexes.insert(id.to_string(), assistant_messages.len());
                assistant_messages.push(msg);
            }
        } else {
            assistant_messages.push(msg);
        }
    }

    for msg in assistant_messages {
        if let Some(m) = json_str(msg, &["model"]) {
            model_name = Some(m.to_string());
        }
        if let Some(usage) = msg.get("usage") {
            let get = |k| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let base = get("input_tokens");
            let cache_read = get("cache_read_input_tokens");
            let cache_create = get("cache_creation_input_tokens");
            let turn_input = base + cache_read + cache_create;
            input_tokens += turn_input;
            output_tokens += get("output_tokens");
            cache_read_tokens += cache_read;
            cache_creation_tokens += cache_create;
            last_turn_input = turn_input;
        }
        if let Some(Value::Array(items)) = msg.get("content") {
            for item in items {
                if json_str(item, &["type"]) == Some("tool_use")
                    && let Some(name) = json_str(item, &["name"])
                {
                    last_activity = Some(extract_tool_detail(name, item));
                }
            }
        }
    }

    let context_pct = model_name.as_deref().and_then(|m| {
        let max_ctx = model_context_window(m)?;
        (last_turn_input > 0)
            .then(|| ((last_turn_input as f64 / max_ctx as f64) * 100.0).min(100.0) as u8)
    });

    let effort = claude_effort_level();
    ParsedTokens {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        last_activity,
        context_pct,
        model: model_name,
        effort,
        turn_count,
    }
}

fn claude_effort_level() -> Option<String> {
    use std::sync::OnceLock;
    static EFFORT: OnceLock<Option<String>> = OnceLock::new();
    EFFORT
        .get_or_init(|| {
            let home = dirs::home_dir()?;
            let path = home.join(".claude").join("settings.json");
            let content = fs::read_to_string(path).ok()?;
            let val: Value = serde_json::from_str(&content).ok()?;
            json_str(&val, &["effortLevel"]).map(|s| s.to_string())
        })
        .clone()
}

pub(crate) fn parse_codex_tokens(path: &Path) -> ParsedTokens {
    let entries = read_jsonl(path, None);
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut last_activity: Option<String> = None;
    let mut context_window: u64 = 0;
    let mut last_turn_input: u64 = 0;
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut turn_count: u32 = 0;

    for entry in &entries {
        match json_str(entry, &["type"]) {
            Some("turn_context") => {
                // Primary source for model name and effort level
                if let Some(m) = json_str(entry, &["payload", "model"]) {
                    model = Some(m.to_string());
                }
                if let Some(e) = json_str(entry, &["payload", "effort"]) {
                    effort = Some(e.to_string());
                }
            }
            Some("event_msg") => match json_str(entry, &["payload", "type"]) {
                Some("token_count") => {
                    if let Some(info) = entry.get("payload").and_then(|p| p.get("info")) {
                        let get_from = |section: &str, field: &str| {
                            info.get(section)
                                .and_then(|s| s.get(field))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                        };
                        input_tokens = get_from("total_token_usage", "input_tokens");
                        output_tokens = get_from("total_token_usage", "output_tokens");
                        last_turn_input = get_from("last_token_usage", "input_tokens");
                        context_window = info
                            .get("model_context_window")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(context_window);
                    }
                }
                Some("user_message") => {
                    turn_count += 1;
                }
                _ => {}
            },
            Some("response_item")
                if json_str(entry, &["payload", "type"]) == Some("function_call") =>
            {
                if let Some(name) = json_str(entry, &["payload", "name"]) {
                    last_activity = Some(name.to_string());
                }
            }
            _ => {}
        }
    }

    let context_pct = (context_window > 0 && last_turn_input > 0)
        .then(|| ((last_turn_input as f64 / context_window as f64) * 100.0).min(100.0) as u8);

    ParsedTokens {
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        last_activity,
        context_pct,
        model,
        effort,
        turn_count,
    }
}

fn model_context_window(model: &str) -> Option<u64> {
    if model.contains("opus") {
        Some(1_000_000)
    } else if model.contains("sonnet") || model.contains("haiku") {
        Some(200_000)
    } else {
        None
    }
}

fn extract_tool_detail(name: &str, item: &Value) -> String {
    let input = item.get("input");
    match name {
        "Edit" | "Write" | "Read" => {
            let file = input
                .and_then(|i| i.get("file_path"))
                .and_then(|p| p.as_str())
                .and_then(|p| p.rsplit('/').next())
                .unwrap_or("");
            format!("{name} {file}")
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

pub(crate) fn extract_claude_session_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in std::io::BufRead::lines(reader).take(10).flatten() {
        if let Ok(val) = serde_json::from_str::<Value>(&line) {
            if let Some(sid) = json_str(&val, &["sessionId"]) {
                return Some(sid.to_string());
            }
        }
    }
    // Fallback: filename stem (typically a UUID)
    path.file_stem()?.to_str().map(|s| s.to_string())
}

pub(crate) fn extract_codex_session_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    let val: Value = serde_json::from_str(&first_line).ok()?;
    if json_str(&val, &["type"]) == Some("session_meta") {
        if let Some(id) = json_str(&val, &["payload", "id"]) {
            return Some(id.to_string());
        }
    }
    path.file_stem()?.to_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_jsonl(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agentmux-{name}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_encode_project_dir() {
        assert_eq!(
            encode_project_dir("/Users/foo/myproject"),
            "-Users-foo-myproject"
        );
    }

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn claude_duplicate_message_ids_keep_final_usage() {
        let path = write_temp_jsonl(
            "claude-duplicate-final",
            r#"{"sessionId":"s1","message":{"role":"assistant","id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":1}}}
{"sessionId":"s1","message":{"role":"assistant","id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":30,"cache_read_input_tokens":5,"cache_creation_input_tokens":7,"output_tokens":20}}}
"#,
        );

        let tokens = parse_claude_tokens(&path);

        assert_eq!(tokens.input_tokens, 42);
        assert_eq!(tokens.output_tokens, 20);
        assert_eq!(tokens.cache_read_tokens, 5);
        assert_eq!(tokens.cache_creation_tokens, 7);

        let _ = fs::remove_file(path);
    }
}
