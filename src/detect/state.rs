use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Working,
    Idle,
}

/// Rich session details from JSONL parsing.
#[derive(Debug, Clone)]
pub struct SessionDetails {
    pub state: AgentState,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_activity: Option<String>,
    /// Context window used percentage (0-100), if available
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

const ACTIVE_WRITE_THRESHOLD: Duration = Duration::from_secs(3);

pub fn claude_code_details(cwd: &str) -> SessionDetails {
    let Some(home) = dirs::home_dir() else {
        return SessionDetails::default();
    };
    let encoded = encode_project_dir(cwd);
    let projects_dir = home.join(".claude").join("projects").join(&encoded);

    let Some(jsonl_path) = find_most_recent_jsonl(&projects_dir) else {
        return SessionDetails::default();
    };

    let recently_active = file_recently_modified(&jsonl_path, ACTIVE_WRITE_THRESHOLD);
    let mut details = parse_claude_code_jsonl(&jsonl_path);

    if recently_active {
        details.state = AgentState::Working;
    }

    details
}

pub fn codex_details(cwd: &str) -> SessionDetails {
    let Some(sessions_dir) = codex_sessions_dir() else {
        return SessionDetails::default();
    };

    let Some(jsonl_path) = find_most_recent_jsonl_recursive(&sessions_dir) else {
        return SessionDetails::default();
    };

    let _ = cwd;
    let recently_active = file_recently_modified(&jsonl_path, ACTIVE_WRITE_THRESHOLD);
    let mut details = parse_codex_jsonl(&jsonl_path);

    if recently_active {
        details.state = AgentState::Working;
    }

    details
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
    // Pick the most recently modified .jsonl file — no age cutoff.
    // The agent process is already confirmed running by the process tree scan,
    // so whatever file is newest belongs to the current session.
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

/// Read the entire JSONL file. Token counting needs all entries for accurate totals.
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

/// Format a token count concisely: 1234 → "1.2k", 1234567 → "1.2M"
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

fn parse_claude_code_jsonl(path: &PathBuf) -> SessionDetails {
    let entries = full_read_jsonl(path);
    if entries.is_empty() {
        return SessionDetails::default();
    }

    let mut state: Option<AgentState> = None;
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut last_activity: Option<String> = None;
    let mut last_turn_input: u64 = 0; // input tokens of the most recent turn (≈ context used)
    let mut model_name: Option<String> = None;

    // Forward pass: accumulate tokens, track last activity
    for entry in &entries {
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let Some(role) = msg.get("role").and_then(|r| r.as_str()) else {
            continue;
        };

        if role == "assistant" {
            // Track model name
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

            // Track last tool_use as activity
            if let Some(Value::Array(items)) = msg.get("content") {
                for item in items {
                    if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                            let detail = extract_tool_detail(name, item);
                            last_activity = Some(detail);
                        }
                    }
                }
            }
        }
    }

    // Reverse pass: find the most recent state
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
                        state = Some(AgentState::Working);
                        break;
                    }
                    if items
                        .iter()
                        .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("thinking"))
                    {
                        state = Some(AgentState::Working);
                        break;
                    }
                }
                match msg.get("stop_reason") {
                    Some(Value::String(reason)) => {
                        if reason == "end_turn" {
                            state = Some(AgentState::Idle);
                        } else if reason == "tool_use" {
                            state = Some(AgentState::Working);
                        } else {
                            state = Some(AgentState::Idle);
                        }
                        break;
                    }
                    Some(Value::Null) | None => {
                        state = Some(AgentState::Working);
                        break;
                    }
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
                        state = Some(AgentState::Idle);
                        break;
                    }
                    if text.contains("<command-name>/exit</command-name>") {
                        state = Some(AgentState::Idle);
                        break;
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
                        state = Some(AgentState::Working);
                        break;
                    }
                }
                state = Some(AgentState::Working);
                break;
            }
            _ => continue,
        }
    }

    // Estimate context window usage: last turn's input ≈ conversation context size
    let context_pct = model_name.as_deref().and_then(|m| {
        let max_ctx = model_context_window(m)?;
        if last_turn_input > 0 && max_ctx > 0 {
            Some(((last_turn_input as f64 / max_ctx as f64) * 100.0).min(100.0) as u8)
        } else {
            None
        }
    });

    SessionDetails {
        state: state.unwrap_or(AgentState::Idle),
        input_tokens,
        output_tokens,
        last_activity,
        context_pct,
    }
}

/// Map Claude model name to context window size in tokens.
fn model_context_window(model: &str) -> Option<u64> {
    if model.contains("opus") {
        Some(1_000_000) // Opus: 1M context (with extended)
    } else if model.contains("sonnet") {
        Some(200_000) // Sonnet: 200k
    } else if model.contains("haiku") {
        Some(200_000) // Haiku: 200k
    } else {
        None
    }
}

/// Extract a short description from a tool_use entry.
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

fn parse_codex_jsonl(path: &PathBuf) -> SessionDetails {
    let entries = full_read_jsonl(path);
    if entries.is_empty() {
        return SessionDetails::default();
    }

    let mut state: Option<AgentState> = None;
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut last_activity: Option<String> = None;
    let mut context_window: u64 = 0;
    let mut last_turn_input: u64 = 0;

    // Forward pass: find token_count entries and last activity
    for entry in &entries {
        let entry_type = entry.get("type").and_then(|t| t.as_str());

        if entry_type == Some("event_msg") {
            let payload_type = entry
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str());

            if payload_type == Some("token_count") {
                let info = entry
                    .get("payload")
                    .and_then(|p| p.get("info"));
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

        // Track function_call as last activity
        if entry_type == Some("response_item") {
            let item_type = entry
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str());
            if item_type == Some("function_call") {
                let name = entry
                    .get("payload")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool");
                last_activity = Some(name.to_string());
            }
        }
    }

    // Reverse pass: find state
    for entry in entries.iter().rev() {
        let entry_type = entry.get("type").and_then(|t| t.as_str());
        match entry_type {
            Some("event_msg") => {
                if let Some(payload_type) = entry
                    .get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                {
                    match payload_type {
                        "task_started" | "user_message" => {
                            state = Some(AgentState::Working);
                            break;
                        }
                        "task_complete" | "turn_aborted" => {
                            state = Some(AgentState::Idle);
                            break;
                        }
                        "agent_message" => {
                            let phase = entry
                                .get("payload")
                                .and_then(|p| p.get("phase"))
                                .and_then(|p| p.as_str());
                            state = Some(if phase == Some("final_answer") {
                                AgentState::Idle
                            } else {
                                AgentState::Working
                            });
                            break;
                        }
                        "token_count" => continue,
                        _ => continue,
                    }
                }
            }
            Some("response_item") => {
                if let Some(item_type) = entry
                    .get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                {
                    if item_type == "function_call" {
                        state = Some(AgentState::Working);
                        break;
                    }
                    if item_type == "message" {
                        let phase = entry
                            .get("payload")
                            .and_then(|p| p.get("phase"))
                            .and_then(|p| p.as_str());
                        if phase == Some("final_answer") {
                            state = Some(AgentState::Idle);
                            break;
                        }
                    }
                }
            }
            _ => {
                if let Some(role) = entry.get("role").and_then(|r| r.as_str()) {
                    state = Some(if role == "user" {
                        AgentState::Working
                    } else {
                        AgentState::Idle
                    });
                    break;
                }
            }
        }
    }

    let context_pct = if context_window > 0 && last_turn_input > 0 {
        Some(((last_turn_input as f64 / context_window as f64) * 100.0).min(100.0) as u8)
    } else {
        None
    };

    SessionDetails {
        state: state.unwrap_or(AgentState::Idle),
        input_tokens,
        output_tokens,
        last_activity,
        context_pct,
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
