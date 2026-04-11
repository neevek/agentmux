use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::AgentInfo;
use super::process::{AgentKind, DetectedAgent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    pub session_id: Option<String>,
    pub jsonl_path: Option<PathBuf>,
    pub display_elapsed_secs: Option<u64>,
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
            session_id: None,
            jsonl_path: None,
            display_elapsed_secs: None,
        }
    }
}

pub struct SessionCache {
    entries: HashMap<PathBuf, CachedData>,
    bindings: HashMap<AgentBindingKey, SessionBinding>,
    path_owners: HashMap<PathBuf, AgentBindingKey>,
}

#[derive(Clone)]
struct CachedData {
    metadata: FileMetadata,
    tokens: ParsedTokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AgentBindingKey {
    kind: AgentKind,
    pane_id: String,
}

#[derive(Clone)]
struct SessionBinding {
    jsonl_path: PathBuf,
    session_id: Option<String>,
    last_agent_pid: u32,
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
    pub(crate) session_id: Option<String>,
}

impl SessionCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            bindings: HashMap::new(),
            path_owners: HashMap::new(),
        }
    }

    pub fn retain_live_agents(&mut self, live_agents: &[DetectedAgent]) {
        let live_keys: HashSet<AgentBindingKey> =
            live_agents.iter().map(AgentBindingKey::from).collect();
        self.retain_live_keys(&live_keys);
    }

    pub fn retain_agent_infos(&mut self, live_agents: &[AgentInfo]) {
        let live_keys: HashSet<AgentBindingKey> =
            live_agents.iter().map(AgentBindingKey::from).collect();
        self.retain_live_keys(&live_keys);
    }

    fn retain_live_keys(&mut self, live_keys: &HashSet<AgentBindingKey>) {
        self.bindings.retain(|key, binding| {
            let keep = live_keys.contains(key);
            if !keep {
                self.path_owners.remove(&binding.jsonl_path);
            }
            keep
        });
        self.path_owners
            .retain(|_, owner| live_keys.contains(owner));
    }

    fn get_or_update_with_metadata(
        &mut self,
        path: &Path,
        parser: fn(&Path) -> ParsedTokens,
        current_metadata: FileMetadata,
    ) -> &ParsedTokens {
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

    fn claimed_paths_for_agent(&self, agent: &DetectedAgent) -> Vec<PathBuf> {
        let key = AgentBindingKey::from(agent);
        self.path_owners
            .iter()
            .filter(|(_, owner)| **owner != key)
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn bound_path_for_agent(
        &mut self,
        agent: &DetectedAgent,
        extract_session_id: fn(&Path) -> Option<String>,
    ) -> Option<PathBuf> {
        let key = AgentBindingKey::from(agent);
        let binding = self.bindings.get(&key)?.clone();

        if !binding.jsonl_path.is_file() {
            self.unbind_agent(agent);
            return None;
        }
        if self.path_owners.get(&binding.jsonl_path) != Some(&key) {
            self.unbind_agent(agent);
            return None;
        }

        let current_session_id = extract_session_id(&binding.jsonl_path);
        if binding.session_id.is_some() && current_session_id != binding.session_id {
            self.unbind_agent(agent);
            return None;
        }

        if binding.last_agent_pid != agent.agent_pid
            && let Some(existing) = self.bindings.get_mut(&key)
        {
            existing.last_agent_pid = agent.agent_pid;
        }

        Some(binding.jsonl_path)
    }

    fn bind_agent_path(
        &mut self,
        agent: &DetectedAgent,
        jsonl_path: PathBuf,
        session_id: Option<String>,
    ) {
        let key = AgentBindingKey::from(agent);
        if let Some(owner) = self.path_owners.get(&jsonl_path)
            && owner != &key
        {
            return;
        }

        if let Some(existing) = self.bindings.get(&key)
            && existing.jsonl_path != jsonl_path
        {
            self.path_owners.remove(&existing.jsonl_path);
        }

        self.path_owners.insert(jsonl_path.clone(), key.clone());
        self.bindings.insert(
            key,
            SessionBinding {
                jsonl_path,
                session_id,
                last_agent_pid: agent.agent_pid,
            },
        );
    }

    fn unbind_agent(&mut self, agent: &DetectedAgent) {
        let key = AgentBindingKey::from(agent);
        if let Some(binding) = self.bindings.remove(&key) {
            self.path_owners.remove(&binding.jsonl_path);
        }
    }
}

impl From<&DetectedAgent> for AgentBindingKey {
    fn from(agent: &DetectedAgent) -> Self {
        Self {
            kind: agent.kind,
            pane_id: agent.pane_id.clone(),
        }
    }
}

impl From<&AgentInfo> for AgentBindingKey {
    fn from(agent: &AgentInfo) -> Self {
        Self {
            kind: agent.kind,
            pane_id: agent.pane_id.clone(),
        }
    }
}

const ACTIVE_WRITE_THRESHOLD: Duration = Duration::from_secs(3);
const CODEX_BOUND_PATH_LIVE_SECS: u64 = 15;
const TAIL_BYTES: u64 = 32768;
/// Ambiguous in-turn events (plain user prompt, exec_command_end) should stay
/// working for a long time, but eventually settle to idle for stalled/crashed
/// sessions that never emit a completion marker.
const INFERRED_WORKING_STALE_SECS: u64 = 24 * 60 * 60;
/// Maximum acceptable difference (seconds) between a JSONL session's start age
/// and the agent process age for the file to be considered a match.  Prevents
/// a freshly-started process from claiming a session that began hours earlier.
const MAX_CLAUDE_AGE_MISMATCH_SECS: u64 = 300;

// --- Public API ---

pub fn claude_code_details(agent: &DetectedAgent, cache: &mut SessionCache) -> SessionDetails {
    let Some(home) = dirs::home_dir() else {
        return SessionDetails::default();
    };
    let encoded = encode_project_dir(&agent.cwd);
    let projects_dir = home.join(".claude").join("projects").join(&encoded);
    let jsonl_path = cache
        .bound_path_for_agent(agent, extract_claude_session_id)
        .or_else(|| {
            let claimed_paths = cache.claimed_paths_for_agent(agent);
            find_claude_jsonl_for_cwd(
                &projects_dir,
                agent.elapsed_secs,
                agent.resumed,
                &claimed_paths,
            )
        });
    let details = agent_details(
        jsonl_path.as_deref(),
        agent.elapsed_secs,
        !agent.resumed,
        cache,
        detect_claude_state,
        parse_claude_tokens,
    );
    update_binding(cache, agent, &details);
    details
}

pub fn codex_details(agent: &DetectedAgent, cache: &mut SessionCache) -> SessionDetails {
    let sessions_dir = codex_sessions_dir();
    let jsonl_path = sessions_dir
        .as_ref()
        .and_then(|dir| select_codex_jsonl_path(dir, agent, cache, unix_now_secs()));
    let mut details = agent_details(
        jsonl_path.as_deref(),
        agent.elapsed_secs,
        !agent.resumed,
        cache,
        detect_codex_state,
        parse_codex_tokens,
    );
    details.display_elapsed_secs = details
        .jsonl_path
        .as_deref()
        .and_then(|path| codex_session_elapsed_secs(path, unix_now_secs()));
    update_binding(cache, agent, &details);
    details
}

pub fn refresh_bound_details(
    kind: AgentKind,
    jsonl_path: Option<&Path>,
    expected_session_id: Option<&str>,
    agent_age_secs: u64,
    resumed: bool,
    cache: &mut SessionCache,
) -> SessionDetails {
    let mut details = match kind {
        AgentKind::ClaudeCode => agent_details(
            jsonl_path,
            agent_age_secs,
            !resumed,
            cache,
            detect_claude_state,
            parse_claude_tokens,
        ),
        AgentKind::Codex => {
            let mut details = agent_details(
                jsonl_path,
                agent_age_secs,
                !resumed,
                cache,
                detect_codex_state,
                parse_codex_tokens,
            );
            details.display_elapsed_secs = details
                .jsonl_path
                .as_deref()
                .and_then(|path| codex_session_elapsed_secs(path, unix_now_secs()));
            details
        }
    };

    if expected_session_id.is_some() && details.session_id.as_deref() != expected_session_id {
        details = SessionDetails::default();
    }

    details
}

pub fn refresh_tracked_details(
    agent: &AgentInfo,
    process_elapsed_secs: u64,
    cache: &mut SessionCache,
) -> SessionDetails {
    match agent.kind {
        AgentKind::ClaudeCode => refresh_bound_details(
            AgentKind::ClaudeCode,
            agent.jsonl_path.as_deref(),
            agent.session_id.as_deref(),
            process_elapsed_secs,
            agent.resumed,
            cache,
        ),
        AgentKind::Codex => {
            let detected = DetectedAgent {
                kind: AgentKind::Codex,
                pane_id: agent.pane_id.clone(),
                cwd: agent.cwd.clone(),
                window_id: agent.window_id.clone(),
                window_index: 0,
                agent_pid: agent.agent_pid.unwrap_or_default(),
                resumed: agent.resumed,
                elapsed_secs: process_elapsed_secs,
            };
            // Re-run full Codex binding selection each refresh so /new and /clear
            // can move the pane to a newer session file instead of pinning stale stats.
            codex_details(&detected, cache)
        }
    }
}

fn select_codex_jsonl_path(
    sessions_dir: &Path,
    agent: &DetectedAgent,
    cache: &mut SessionCache,
    now_secs: u64,
) -> Option<PathBuf> {
    let current_path = cache.bound_path_for_agent(agent, extract_codex_session_id);
    let claimed_paths = cache.claimed_paths_for_agent(agent);
    let newer_replacement = current_path.as_deref().and_then(|current| {
        find_newer_codex_session_replacement(sessions_dir, current, &agent.cwd, &claimed_paths)
    });
    let candidate_path = find_codex_jsonl_for_cwd_at(
        sessions_dir,
        &agent.cwd,
        agent.elapsed_secs,
        agent.resumed,
        &claimed_paths,
        now_secs,
    );

    if let Some(current) = &current_path {
        let replacement = if newer_replacement.is_some() {
            newer_replacement
        } else {
            match candidate_path {
                Some(candidate)
                    if current != &candidate
                        && should_replace_codex_binding(
                            current,
                            &candidate,
                            &agent.cwd,
                            agent.elapsed_secs,
                            now_secs,
                        ) =>
                {
                    Some(candidate)
                }
                _ => None,
            }
        };

        return replacement
            .filter(|path| codex_path_is_recent(path, CODEX_BOUND_PATH_LIVE_SECS))
            .or(current_path);
    }

    candidate_path
}

fn codex_path_is_recent(path: &Path, max_age_secs: u64) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let file_age = meta
        .modified()
        .ok()
        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    file_age <= max_age_secs
}

fn find_newer_codex_session_replacement(
    sessions_dir: &Path,
    current_path: &Path,
    cwd: &str,
    used_paths: &[PathBuf],
) -> Option<PathBuf> {
    let current_start = codex_session_start_secs(current_path)?;
    let mut files = Vec::new();
    walk_jsonl(sessions_dir, &mut files);

    files
        .into_iter()
        .filter(|path| path != current_path)
        .filter(|path| !used_paths.iter().any(|used| used == path))
        .filter_map(|path| {
            let meta = parse_codex_session_meta(&path)?;
            let session_cwd = json_str(&meta, &["payload", "cwd"])?;
            if !codex_cwds_match(cwd, session_cwd) {
                return None;
            }
            let started = codex_session_start_secs(&path)?;
            (started > current_start).then_some((started, path))
        })
        .max_by_key(|(started, _)| *started)
        .map(|(_, path)| path)
}

pub(crate) fn binding_priority(agent: &DetectedAgent) -> u64 {
    match agent.kind {
        AgentKind::ClaudeCode => claude_binding_priority(agent),
        AgentKind::Codex => codex_binding_priority(agent),
    }
}

fn claude_binding_priority(agent: &DetectedAgent) -> u64 {
    let Some(home) = dirs::home_dir() else {
        return u64::MAX;
    };
    let encoded = encode_project_dir(&agent.cwd);
    let projects_dir = home.join(".claude").join("projects").join(&encoded);
    let now = unix_now_secs();
    fs::read_dir(projects_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| claude_match_score(&path, agent.elapsed_secs, now))
        .min()
        .unwrap_or(u64::MAX)
}

fn codex_binding_priority(agent: &DetectedAgent) -> u64 {
    let Some(sessions_dir) = codex_sessions_dir() else {
        return u64::MAX;
    };
    let mut files = Vec::new();
    walk_jsonl(&sessions_dir, &mut files);
    let now = unix_now_secs();
    files
        .into_iter()
        .filter_map(|path| {
            codex_binding_age_mismatch_secs(&path, &agent.cwd, agent.elapsed_secs, now)
        })
        .min()
        .unwrap_or(u64::MAX)
}

fn update_binding(cache: &mut SessionCache, agent: &DetectedAgent, details: &SessionDetails) {
    if let Some(path) = &details.jsonl_path {
        cache.bind_agent_path(agent, path.clone(), details.session_id.clone());
    } else {
        cache.unbind_agent(agent);
    }
}

fn agent_details(
    jsonl_path: Option<&Path>,
    agent_age_secs: u64,
    enforce_file_age_match: bool,
    cache: &mut SessionCache,
    detect_state: fn(&Path, u64) -> AgentState,
    parse_tokens: fn(&Path) -> ParsedTokens,
) -> SessionDetails {
    let Some(path) = jsonl_path else {
        return SessionDetails::default();
    };
    let Ok(meta) = fs::metadata(path) else {
        return SessionDetails::default();
    };
    let mtime = meta.modified().ok();
    let file_age = mtime
        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    if enforce_file_age_match && file_age > agent_age_secs + 5 {
        return SessionDetails::default();
    }

    let state = if file_age < ACTIVE_WRITE_THRESHOLD.as_secs() {
        AgentState::Working
    } else {
        detect_state(path, file_age)
    };

    let metadata = FileMetadata {
        size: meta.len(),
        modified_ts: mtime
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    let cached = cache.get_or_update_with_metadata(path, parse_tokens, metadata);
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
        session_id: cached.session_id.clone(),
        jsonl_path: Some(path.to_path_buf()),
        display_elapsed_secs: None,
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

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

fn find_claude_jsonl_for_cwd(
    projects_dir: &Path,
    agent_age_secs: u64,
    resumed: bool,
    used_paths: &[PathBuf],
) -> Option<PathBuf> {
    find_claude_jsonl_for_cwd_at(
        projects_dir,
        agent_age_secs,
        resumed,
        used_paths,
        unix_now_secs(),
    )
}

fn find_claude_jsonl_for_cwd_at(
    projects_dir: &Path,
    agent_age_secs: u64,
    resumed: bool,
    used_paths: &[PathBuf],
    now_secs: u64,
) -> Option<PathBuf> {
    if !projects_dir.is_dir() {
        return None;
    }
    let all_files: Vec<PathBuf> = fs::read_dir(projects_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    if all_files.is_empty() {
        return None;
    }

    let available_files: Vec<PathBuf> = all_files
        .into_iter()
        .filter(|path| !used_paths.iter().any(|used| used == path))
        .collect();
    if available_files.is_empty() {
        return None;
    }

    let scored: Vec<(u64, PathBuf)> = available_files
        .iter()
        .filter_map(|path| {
            claude_match_score(path, agent_age_secs, now_secs).map(|s| (s, path.clone()))
        })
        .collect();
    if scored.is_empty() {
        return most_recent_jsonl(&available_files);
    }

    let best = scored.into_iter().min_by_key(|(score, path)| {
        let mtime = fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        (*score, std::cmp::Reverse(mtime))
    });
    match best {
        Some((score, path)) if score <= MAX_CLAUDE_AGE_MISMATCH_SECS => Some(path),
        Some(_) if resumed => most_recent_jsonl(&available_files),
        _ => None,
    }
}

/// Score a Claude JSONL by how closely its session start time matches the agent
/// process age.  Lower is better; `None` means no usable timestamp was found.
fn claude_match_score(path: &Path, agent_age_secs: u64, now_secs: u64) -> Option<u64> {
    let start_secs = claude_session_start_secs(path)?;
    let start_age = now_secs.checked_sub(start_secs)?;
    Some(start_age.abs_diff(agent_age_secs))
}

/// Extract the session start time (epoch seconds) from the first timestamped
/// entry in a Claude Code JSONL file.
fn claude_session_start_secs(path: &Path) -> Option<u64> {
    let file = fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in std::io::BufRead::lines(reader)
        .take(10)
        .map_while(Result::ok)
    {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(ts) = json_str(&entry, &["timestamp"])
            && let Some(secs) = parse_rfc3339_utc_secs(ts)
        {
            return Some(secs);
        }
    }
    // Fallback: file creation time
    fs::metadata(path)
        .ok()
        .and_then(|m| m.created().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Recursively collect all .jsonl files under a directory.
pub(crate) fn walk_jsonl(dir: &Path, files: &mut Vec<PathBuf>) {
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

fn find_codex_jsonl_for_cwd_at(
    sessions_dir: &Path,
    cwd: &str,
    agent_age_secs: u64,
    resumed: bool,
    used_paths: &[PathBuf],
    now_secs: u64,
) -> Option<PathBuf> {
    if !sessions_dir.is_dir() {
        return None;
    }
    let mut all_files: Vec<PathBuf> = Vec::new();
    walk_jsonl(sessions_dir, &mut all_files);
    if all_files.is_empty() {
        return None;
    }

    let available_files: Vec<PathBuf> = all_files
        .into_iter()
        .filter(|path| !used_paths.iter().any(|used| used == path))
        .collect();
    if available_files.is_empty() {
        return None;
    }

    let mut candidates: Vec<(CodexMatchScore, PathBuf)> = Vec::new();
    let mut unscorable: Vec<PathBuf> = Vec::new();
    for path in available_files {
        match codex_candidate(&path, cwd, agent_age_secs, now_secs) {
            CodexCandidate::Scored(score) => candidates.push((score, path)),
            CodexCandidate::Unscorable => unscorable.push(path),
            CodexCandidate::CwdMismatch => {}
        }
    }

    if resumed {
        let resumed_candidates: Vec<PathBuf> =
            candidates.iter().map(|(_, path)| path.clone()).collect();
        return most_recent_jsonl(&resumed_candidates).or_else(|| most_recent_jsonl(&unscorable));
    }

    if candidates.is_empty() {
        return most_recent_jsonl(&unscorable);
    }

    candidates.sort_by(compare_codex_candidates);
    candidates.into_iter().next().map(|(_, path)| path)
}

fn most_recent_jsonl(paths: &[PathBuf]) -> Option<PathBuf> {
    paths
        .iter()
        .max_by_key(|path| {
            fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        })
        .cloned()
}

const CODEX_TOKEN_BIAS_SECS: u64 = 30;

/// Parse the first line of a Codex JSONL file if it is a `session_meta` entry.
fn parse_codex_session_meta(path: &Path) -> Option<Value> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    let entry: Value = serde_json::from_str(&first_line).ok()?;
    (json_str(&entry, &["type"]) == Some("session_meta")).then_some(entry)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexMatchScore {
    age_mismatch_secs: u64,
    has_token_count: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexCandidate {
    Scored(CodexMatchScore),
    Unscorable,
    CwdMismatch,
}

fn compare_codex_candidates(
    (lhs_score, lhs_path): &(CodexMatchScore, PathBuf),
    (rhs_score, rhs_path): &(CodexMatchScore, PathBuf),
) -> std::cmp::Ordering {
    let within_bias_window = lhs_score
        .age_mismatch_secs
        .abs_diff(rhs_score.age_mismatch_secs)
        <= CODEX_TOKEN_BIAS_SECS;
    if within_bias_window && lhs_score.has_token_count != rhs_score.has_token_count {
        return rhs_score.has_token_count.cmp(&lhs_score.has_token_count);
    }

    lhs_score
        .age_mismatch_secs
        .cmp(&rhs_score.age_mismatch_secs)
        .then_with(|| rhs_score.has_token_count.cmp(&lhs_score.has_token_count))
        .then_with(|| {
            let lhs_mtime = fs::metadata(lhs_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let rhs_mtime = fs::metadata(rhs_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            rhs_mtime.cmp(&lhs_mtime)
        })
}

fn should_replace_codex_binding(
    current_path: &Path,
    candidate_path: &Path,
    cwd: &str,
    agent_age_secs: u64,
    now_secs: u64,
) -> bool {
    let current_score = codex_match_score(current_path, cwd, agent_age_secs, now_secs);
    let candidate_score = codex_match_score(candidate_path, cwd, agent_age_secs, now_secs);

    match (current_score, candidate_score) {
        (Some(current), Some(candidate)) => compare_codex_candidates(
            &(candidate, candidate_path.to_path_buf()),
            &(current, current_path.to_path_buf()),
        )
        .is_lt(),
        (None, Some(_)) => true,
        _ => false,
    }
}

fn codex_match_score(
    path: &Path,
    cwd: &str,
    agent_age_secs: u64,
    now_secs: u64,
) -> Option<CodexMatchScore> {
    match codex_candidate(path, cwd, agent_age_secs, now_secs) {
        CodexCandidate::Scored(score) => Some(score),
        CodexCandidate::Unscorable | CodexCandidate::CwdMismatch => None,
    }
}

fn codex_binding_age_mismatch_secs(
    path: &Path,
    cwd: &str,
    agent_age_secs: u64,
    now_secs: u64,
) -> Option<u64> {
    let meta = parse_codex_session_meta(path)?;
    let session_cwd = json_str(&meta, &["payload", "cwd"])?;
    if !codex_cwds_match(cwd, session_cwd) {
        return None;
    }

    json_str(&meta, &["payload", "timestamp"])
        .and_then(parse_rfc3339_utc_secs)
        .and_then(|started| now_secs.checked_sub(started))
        .map(|age| age.abs_diff(agent_age_secs))
        .or_else(|| {
            fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|mtime| mtime.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| {
                    now_secs
                        .saturating_sub(d.as_secs())
                        .abs_diff(agent_age_secs)
                })
        })
}

fn codex_candidate(path: &Path, cwd: &str, agent_age_secs: u64, now_secs: u64) -> CodexCandidate {
    let Some(meta) = parse_codex_session_meta(path) else {
        return CodexCandidate::Unscorable;
    };
    let Some(session_cwd) = json_str(&meta, &["payload", "cwd"]) else {
        return CodexCandidate::Unscorable;
    };
    if !codex_cwds_match(cwd, session_cwd) {
        return CodexCandidate::CwdMismatch;
    }

    let start_age = json_str(&meta, &["payload", "timestamp"])
        .and_then(parse_rfc3339_utc_secs)
        .and_then(|started| now_secs.checked_sub(started));
    let Some(age_mismatch_secs) = start_age
        .map(|age| age.abs_diff(agent_age_secs))
        .or_else(|| {
            fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|mtime| mtime.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| {
                    now_secs
                        .saturating_sub(d.as_secs())
                        .abs_diff(agent_age_secs)
                })
        })
    else {
        return CodexCandidate::Unscorable;
    };

    CodexCandidate::Scored(CodexMatchScore {
        age_mismatch_secs,
        has_token_count: codex_has_token_count(path),
    })
}

fn codex_cwds_match(lhs: &str, rhs: &str) -> bool {
    lhs == rhs
        || canonicalize_if_exists(lhs)
            .zip(canonicalize_if_exists(rhs))
            .is_some_and(|(a, b)| a == b)
        || normalized_path_equals(lhs, rhs)
}

fn canonicalize_if_exists(path: &str) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

fn normalized_path_equals(lhs: &str, rhs: &str) -> bool {
    normalize_path(lhs) == normalize_path(rhs)
}

fn normalize_path(path: &str) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        normalized.push(component.as_os_str());
    }
    normalized
}

fn codex_has_token_count(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
        if !line.contains("\"token_count\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if json_str(&entry, &["type"]) == Some("event_msg")
            && json_str(&entry, &["payload", "type"]) == Some("token_count")
        {
            return true;
        }
    }
    false
}

fn codex_session_elapsed_secs(path: &Path, now_secs: u64) -> Option<u64> {
    Some(now_secs.saturating_sub(codex_session_start_secs(path)?))
}

fn codex_session_start_secs(path: &Path) -> Option<u64> {
    parse_codex_session_meta(path)
        .and_then(|meta| {
            json_str(&meta, &["payload", "timestamp"]).and_then(parse_rfc3339_utc_secs)
        })
        .or_else(|| {
            fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|mtime| mtime.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
        })
}

fn parse_rfc3339_utc_secs(s: &str) -> Option<u64> {
    if s.len() < 20
        || &s[4..5] != "-"
        || &s[7..8] != "-"
        || &s[10..11] != "T"
        || &s[13..14] != ":"
        || &s[16..17] != ":"
    {
        return None;
    }
    let year = s[0..4].parse::<i32>().ok()?;
    let month = s[5..7].parse::<u32>().ok()?;
    let day = s[8..10].parse::<u32>().ok()?;
    let hour = s[11..13].parse::<u32>().ok()?;
    let minute = s[14..16].parse::<u32>().ok()?;
    let second = s[17..19].parse::<u32>().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || !s[19..].ends_with('Z')
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hour as i64 * 3_600 + minute as i64 * 60 + second as i64)?;
    u64::try_from(secs).ok()
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - (month <= 2) as i32;
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
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

fn detect_claude_state(path: &Path, file_age_secs: u64) -> AgentState {
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
                // A plain user message usually means an in-flight turn. If it
                // remains unchanged for an extremely long time, consider it
                // stale instead of perpetually working.
                return if file_age_secs < INFERRED_WORKING_STALE_SECS {
                    AgentState::Working
                } else {
                    AgentState::Idle
                };
            }
            _ => continue,
        }
    }
    AgentState::Idle
}

fn detect_codex_state(path: &Path, file_age_secs: u64) -> AgentState {
    for entry in read_jsonl(path, Some(TAIL_BYTES)).iter().rev() {
        match json_str(entry, &["type"]) {
            Some("event_msg") => {
                if let Some(pt) = json_str(entry, &["payload", "type"]) {
                    match pt {
                        "task_started" | "user_message" => {
                            return AgentState::Working;
                        }
                        // `exec_command_end` is still mid-turn; Codex often
                        // emits follow-up events before completion.
                        "exec_command_end" => {
                            return if file_age_secs < INFERRED_WORKING_STALE_SECS {
                                AgentState::Working
                            } else {
                                AgentState::Idle
                            };
                        }
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
                    match it {
                        "function_call" | "function_call_output" | "reasoning" => {
                            return AgentState::Working;
                        }
                        "message" => {
                            return if json_str(entry, &["payload", "phase"]) == Some("final_answer")
                            {
                                AgentState::Idle
                            } else {
                                AgentState::Working
                            };
                        }
                        _ => {}
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
    let mut session_id: Option<String> = None;

    for entry in &entries {
        // Extract sessionId from any entry that has it (typically the first)
        if session_id.is_none()
            && let Some(sid) = json_str(entry, &["sessionId"])
        {
            session_id = Some(sid.to_string());
        }
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
    // Fallback session_id: filename stem (typically a UUID)
    if session_id.is_none() {
        session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
    }
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
        session_id,
    }
}

fn claude_effort_level() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude").join("settings.json");
    let content = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    json_str(&val, &["effortLevel"]).map(|s| s.to_string())
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
    let mut session_id: Option<String> = None;

    for entry in &entries {
        // Extract session id from session_meta entry
        if session_id.is_none()
            && json_str(entry, &["type"]) == Some("session_meta")
        {
            session_id = json_str(entry, &["payload", "id"]).map(|s| s.to_string());
        }
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
                    last_activity = Some(extract_codex_tool_detail(name, entry));
                }
            }
            _ => {}
        }
    }

    let context_pct = (context_window > 0 && last_turn_input > 0)
        .then(|| ((last_turn_input as f64 / context_window as f64) * 100.0).min(100.0) as u8);

    if session_id.is_none() {
        session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
    }
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
        session_id,
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

fn extract_codex_tool_detail(name: &str, entry: &Value) -> String {
    let payload = entry.get("payload");
    match name {
        "exec_command" => {
            let cmd = payload
                .and_then(|value| value.get("arguments"))
                .and_then(|value| value.as_str())
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|arguments| {
                    arguments
                        .get("cmd")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string())
                });
            match cmd {
                Some(cmd) if !cmd.trim().is_empty() => truncate_activity(&cmd, 48),
                _ => name.to_string(),
            }
        }
        _ => name.to_string(),
    }
}

fn truncate_activity(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        normalized
    } else {
        let prefix = normalized
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect::<String>();
        format!("{prefix}...")
    }
}

pub(crate) fn extract_claude_session_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in std::io::BufRead::lines(reader).take(10).flatten() {
        if let Ok(val) = serde_json::from_str::<Value>(&line)
            && let Some(sid) = json_str(&val, &["sessionId"])
        {
            return Some(sid.to_string());
        }
    }
    // Fallback: filename stem (typically a UUID)
    path.file_stem()?.to_str().map(|s| s.to_string())
}

pub(crate) fn extract_codex_session_id(path: &Path) -> Option<String> {
    if let Some(meta) = parse_codex_session_meta(path)
        && let Some(id) = json_str(&meta, &["payload", "id"])
    {
        return Some(id.to_string());
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

    #[test]
    fn codex_jsonl_selection_uses_process_age_and_excludes_used_paths() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-codex-select-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let cwd = "/tmp/project";
        let old_path = dir.join("old.jsonl");
        let new_path = dir.join("new.jsonl");
        let subagent_path = dir.join("subagent.jsonl");
        fs::write(
            &old_path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"old","timestamp":"1970-01-01T00:01:40.000Z","cwd":"{cwd}","source":"cli"}}}}"#
            ),
        )
        .unwrap();
        fs::write(
            &new_path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"new","timestamp":"1970-01-01T00:15:00.000Z","cwd":"{cwd}","source":"cli"}}}}"#
            ),
        )
        .unwrap();
        fs::write(
            &subagent_path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"sub","timestamp":"1970-01-01T00:15:10.000Z","cwd":"{cwd}","source":{{"subagent":"review"}}}}}}
{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":10}}}}}}}}
"#
            ),
        )
        .unwrap();

        let selected = find_codex_jsonl_for_cwd_at(&dir, cwd, 90, false, &[], 1000).unwrap();
        assert_eq!(selected, subagent_path);

        let selected = find_codex_jsonl_for_cwd_at(
            &dir,
            cwd,
            90,
            false,
            std::slice::from_ref(&subagent_path),
            1000,
        )
        .unwrap();
        assert_eq!(selected, new_path);

        let selected =
            find_codex_jsonl_for_cwd_at(&dir, cwd, 900, false, &[subagent_path, new_path], 1000)
                .unwrap();
        assert_eq!(selected, old_path);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_jsonl_selection_matches_normalized_cwd_paths() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-codex-cwd-normalize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let real_cwd = dir.join("project");
        fs::create_dir_all(&real_cwd).unwrap();
        let tmux_cwd = format!("{}/./", real_cwd.display());
        let session_cwd = real_cwd.display().to_string();
        let path = dir.join("session.jsonl");
        fs::write(
            &path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"session","timestamp":"1970-01-01T00:15:00.000Z","cwd":"{session_cwd}","source":"cli"}}}}"#
            ),
        )
        .unwrap();

        let selected = find_codex_jsonl_for_cwd_at(&dir, &tmux_cwd, 100, false, &[], 1000).unwrap();
        assert_eq!(selected, path);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_jsonl_selection_does_not_fallback_across_unmatched_cwds() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-codex-no-cross-cwd-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let other_cwd = "/tmp/other-project";
        let path = dir.join("other.jsonl");
        fs::write(
            &path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"other","timestamp":"1970-01-01T00:15:00.000Z","cwd":"{other_cwd}","source":"cli"}}}}"#
            ),
        )
        .unwrap();

        assert!(find_codex_jsonl_for_cwd_at(&dir, "/tmp/project", 10, false, &[], 1000).is_none());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_jsonl_selection_falls_back_to_most_recent_when_unscorable() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-codex-unscorable-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let older = dir.join("older.jsonl");
        let newer = dir.join("newer.jsonl");
        fs::write(
            &older,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(
            &newer,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        )
        .unwrap();

        let selected =
            find_codex_jsonl_for_cwd_at(&dir, "/tmp/project", 10, false, &[], unix_now_secs())
                .unwrap();
        assert_eq!(selected, newer);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_jsonl_selection_prefers_scored_candidate_over_unscorable_fallback() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-codex-scored-vs-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let cwd = "/tmp/project";
        let now = 1000;

        let scored = dir.join("scored.jsonl");
        let unscorable = dir.join("unscorable.jsonl");
        fs::write(
            &scored,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"scored","timestamp":"{}","cwd":"{cwd}","source":"cli"}}}}"#,
                fmt_rfc3339(now - 10)
            ),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(
            &unscorable,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        )
        .unwrap();

        let selected = find_codex_jsonl_for_cwd_at(&dir, cwd, 10, false, &[], now).unwrap();
        assert_eq!(selected, scored);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_jsonl_selection_uses_mtime_when_timestamp_missing() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-codex-missing-timestamp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let cwd = "/tmp/project";

        let older = dir.join("older.jsonl");
        let newer = dir.join("newer.jsonl");
        fs::write(
            &older,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"older","cwd":"{cwd}","source":"cli"}}}}"#
            ),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(
            &newer,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"newer","cwd":"{cwd}","source":"cli"}}}}"#
            ),
        )
        .unwrap();

        let selected =
            find_codex_jsonl_for_cwd_at(&dir, cwd, 10, false, &[], unix_now_secs()).unwrap();
        assert_eq!(selected, newer);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_token_count_bias_applies_within_narrow_age_window() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-codex-token-tiebreak-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let cwd = "/tmp/project";
        let now = 1000;
        let ideal_started = now - 10;

        let better_age_no_stats = dir.join("better-age-no-stats.jsonl");
        let worse_age_with_stats = dir.join("worse-age-with-stats.jsonl");
        let tied_age_no_stats = dir.join("tied-age-no-stats.jsonl");
        let tied_age_with_stats = dir.join("tied-age-with-stats.jsonl");

        fs::write(
            &better_age_no_stats,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"better","timestamp":"{}","cwd":"{cwd}","source":"cli"}}}}"#,
                fmt_rfc3339(ideal_started - 1)
            ),
        )
        .unwrap();
        fs::write(
            &worse_age_with_stats,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"worse","timestamp":"{}","cwd":"{cwd}","source":"cli"}}}}
{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":10}}}}}}}}
"#,
                fmt_rfc3339(ideal_started - 20)
            ),
        )
        .unwrap();
        fs::write(
            &tied_age_no_stats,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"tie-no-stats","timestamp":"{}","cwd":"{cwd}","source":"cli"}}}}"#,
                fmt_rfc3339(ideal_started)
            ),
        )
        .unwrap();
        fs::write(
            &tied_age_with_stats,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"tie-with-stats","timestamp":"{}","cwd":"{cwd}","source":"cli"}}}}
{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":10}}}}}}}}
"#,
                fmt_rfc3339(ideal_started)
            ),
        )
        .unwrap();

        let selected = find_codex_jsonl_for_cwd_at(&dir, cwd, 10, false, &[], now).unwrap();
        assert_eq!(selected, tied_age_with_stats);

        let selected = find_codex_jsonl_for_cwd_at(
            &dir,
            cwd,
            10,
            false,
            &[tied_age_with_stats.clone(), tied_age_no_stats.clone()],
            now,
        )
        .unwrap();
        assert_eq!(selected, worse_age_with_stats);

        let selected = find_codex_jsonl_for_cwd_at(
            &dir,
            cwd,
            10,
            false,
            &[
                tied_age_with_stats.clone(),
                tied_age_no_stats.clone(),
                worse_age_with_stats.clone(),
            ],
            now,
        )
        .unwrap();
        assert_eq!(selected, better_age_no_stats);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn claude_jsonl_selection_falls_back_to_most_recent_when_unscorable() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-claude-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let older = dir.join("older.jsonl");
        let newer = dir.join("newer.jsonl");
        fs::write(
            &older,
            "{\"message\":{\"role\":\"user\",\"content\":\"older\"}}\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(
            &newer,
            "{\"message\":{\"role\":\"user\",\"content\":\"newer\"}}\n",
        )
        .unwrap();

        let selected = find_claude_jsonl_for_cwd_at(&dir, 10, false, &[], unix_now_secs()).unwrap();
        assert_eq!(selected, newer);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn claude_jsonl_selection_does_not_fallback_when_only_stale_scored_candidates_exist() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-claude-no-stale-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let now = unix_now_secs();
        let path = dir.join("stale.jsonl");
        fs::write(
            &path,
            format!(
                r#"{{"timestamp":"{}","message":{{"role":"user","content":"old"}}}}"#,
                fmt_rfc3339(now - 5000)
            ),
        )
        .unwrap();

        assert!(find_claude_jsonl_for_cwd_at(&dir, 10, false, &[], now).is_none());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn claude_jsonl_selection_resumed_session_uses_most_recent_stale_candidate() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-claude-resume-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let now = unix_now_secs();
        let older = dir.join("older.jsonl");
        let newer = dir.join("newer.jsonl");
        fs::write(
            &older,
            format!(
                r#"{{"timestamp":"{}","message":{{"role":"user","content":"old"}}}}"#,
                fmt_rfc3339(now - 5000)
            ),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(
            &newer,
            format!(
                r#"{{"timestamp":"{}","message":{{"role":"user","content":"new"}}}}"#,
                fmt_rfc3339(now - 4000)
            ),
        )
        .unwrap();

        let selected = find_claude_jsonl_for_cwd_at(&dir, 10, true, &[], now).unwrap();
        assert_eq!(selected, newer);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn claude_plain_user_message_stays_working_without_followup() {
        let path = write_temp_jsonl(
            "claude-plain-user-idle",
            r#"{"message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}
{"message":{"role":"user","content":"try this again"}}
"#,
        );

        assert_eq!(detect_claude_state(&path, 10), AgentState::Working);
        assert_eq!(detect_claude_state(&path, 10_000), AgentState::Working);
        assert_eq!(
            detect_claude_state(&path, INFERRED_WORKING_STALE_SECS + 1),
            AgentState::Idle
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn claude_tool_result_keeps_session_working() {
        let path = write_temp_jsonl(
            "claude-tool-result-working",
            r#"{"message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}
{"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}
"#,
        );

        assert_eq!(detect_claude_state(&path, 300), AgentState::Working);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn codex_state_treats_in_turn_events_as_working() {
        let path = write_temp_jsonl(
            "codex-working-state",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}
{"type":"response_item","payload":{"type":"function_call_output"}}
"#,
        );

        assert_eq!(detect_codex_state(&path, 300), AgentState::Working);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn codex_exec_command_end_stays_working_without_followup() {
        let path = write_temp_jsonl(
            "codex-exec-cmd-end",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}
{"type":"event_msg","payload":{"type":"exec_command_end"}}
"#,
        );

        assert_eq!(detect_codex_state(&path, 10), AgentState::Working);
        assert_eq!(detect_codex_state(&path, 10_000), AgentState::Working);
        assert_eq!(
            detect_codex_state(&path, INFERRED_WORKING_STALE_SECS + 1),
            AgentState::Idle
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn codex_final_answer_is_idle() {
        let path = write_temp_jsonl(
            "codex-final-answer",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}
{"type":"response_item","payload":{"type":"message","phase":"final_answer"}}
"#,
        );

        assert_eq!(detect_codex_state(&path, 300), AgentState::Idle);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn codex_last_activity_shows_exec_command_text() {
        let path = write_temp_jsonl(
            "codex-last-activity-command",
            r#"{"type":"session_meta","payload":{"id":"session-1"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"rtk git diff -- src/sidebar/runtime.rs\",\"workdir\":\"/tmp/demo\"}"}}
"#,
        );

        let parsed = parse_codex_tokens(&path);

        assert_eq!(
            parsed.last_activity.as_deref(),
            Some("rtk git diff -- src/sidebar/runtime.rs")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn claude_jsonl_selection_uses_process_age_and_excludes_used_paths() {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-claude-select-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        // Session started ~900s ago (timestamp = now - 900)
        let now = unix_now_secs();
        let old_ts = fmt_rfc3339(now - 900);
        let new_ts = fmt_rfc3339(now - 100);

        let old_path = dir.join("old-session.jsonl");
        let new_path = dir.join("new-session.jsonl");
        fs::write(
            &old_path,
            format!(
                r#"{{"type":"user","sessionId":"old","timestamp":"{old_ts}","message":{{"role":"user","content":"hi"}}}}"#
            ),
        )
        .unwrap();
        fs::write(
            &new_path,
            format!(
                r#"{{"type":"user","sessionId":"new","timestamp":"{new_ts}","message":{{"role":"user","content":"hi"}}}}"#
            ),
        )
        .unwrap();

        // Agent running ~100s matches the newer session
        let selected = find_claude_jsonl_for_cwd_at(&dir, 100, false, &[], now).unwrap();
        assert_eq!(selected, new_path);

        // Agent running ~900s matches the older session
        let selected = find_claude_jsonl_for_cwd_at(&dir, 900, false, &[], now).unwrap();
        assert_eq!(selected, old_path);

        // Excluding the new session leaves only a stale scoreable candidate, so
        // the selector should prefer returning no match over binding to a clearly
        // wrong old session.
        assert!(
            find_claude_jsonl_for_cwd_at(&dir, 100, false, std::slice::from_ref(&new_path), now,)
                .is_none()
        );

        let _ = fs::remove_dir_all(dir);
    }

    fn fmt_rfc3339(epoch_secs: u64) -> String {
        let s = epoch_secs;
        let days = (s / 86_400) as i64;
        let rem = s % 86_400;
        let h = rem / 3_600;
        let m = (rem % 3_600) / 60;
        let sec = rem % 60;
        // Convert days since epoch to y-m-d (inverse of days_from_civil)
        let (y, mo, d) = civil_from_days(days);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{sec:02}.000Z")
    }

    fn civil_from_days(days: i64) -> (i32, u32, u32) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = if m <= 2 { y + 1 } else { y } as i32;
        (y, m, d)
    }

    fn detected_agent(
        kind: AgentKind,
        pane_id: &str,
        pid: u32,
        cwd: &str,
        elapsed_secs: u64,
    ) -> DetectedAgent {
        DetectedAgent {
            kind,
            pane_id: pane_id.to_string(),
            cwd: cwd.to_string(),
            window_id: "@1".to_string(),
            window_index: 1,
            agent_pid: pid,
            resumed: false,
            elapsed_secs,
        }
    }

    fn tracked_agent_info(
        pane_id: &str,
        pid: u32,
        cwd: &str,
        path: &Path,
        session_id: &str,
    ) -> AgentInfo {
        AgentInfo {
            kind: AgentKind::Codex,
            agent_pid: Some(pid),
            pane_id: pane_id.to_string(),
            cwd: cwd.to_string(),
            window_id: "@1".to_string(),
            window_name: "win".to_string(),
            state: AgentState::Working,
            elapsed_secs: 0,
            process_elapsed_secs: 0,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            cost_usd: 0.0,
            turn_count: 0,
            session_id: Some(session_id.to_string()),
            jsonl_path: Some(path.to_path_buf()),
            resumed: false,
            details_ready: true,
        }
    }

    fn set_file_mtime_secs(path: &Path, secs: i64) {
        #[cfg(unix)]
        unsafe {
            let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            let times = [
                libc::timeval {
                    tv_sec: secs,
                    tv_usec: 0,
                },
                libc::timeval {
                    tv_sec: secs,
                    tv_usec: 0,
                },
            ];
            assert_eq!(libc::utimes(c_path.as_ptr(), times.as_ptr()), 0);
        }
    }

    #[test]
    fn session_cache_reuses_existing_binding_for_same_agent() {
        let path = write_temp_jsonl(
            "codex-binding-reuse",
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:01:40.000Z","source":"cli"}}"#,
        );
        let agent = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 90);
        let mut cache = SessionCache::new();

        cache.bind_agent_path(&agent, path.clone(), Some("session-1".to_string()));

        assert_eq!(
            cache.bound_path_for_agent(&agent, extract_codex_session_id),
            Some(path.clone())
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_cache_keeps_binding_when_descendant_pid_changes() {
        let path = write_temp_jsonl(
            "codex-binding-pid-change",
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:01:40.000Z","source":"cli"}}"#,
        );
        let original = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 90);
        let same_pane_new_pid = detected_agent(AgentKind::Codex, "%1", 202, "/tmp/project", 91);
        let mut cache = SessionCache::new();

        cache.bind_agent_path(&original, path.clone(), Some("session-1".to_string()));

        assert_eq!(
            cache.bound_path_for_agent(&same_pane_new_pid, extract_codex_session_id),
            Some(path.clone())
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_cache_drops_binding_when_agent_disappears() {
        let path = write_temp_jsonl(
            "claude-binding-drop",
            r#"{"sessionId":"session-1","timestamp":"1970-01-01T00:01:40.000Z","message":{"role":"user","content":"hi"}}"#,
        );
        let agent = detected_agent(AgentKind::ClaudeCode, "%1", 202, "/tmp/project", 90);
        let mut cache = SessionCache::new();

        cache.bind_agent_path(&agent, path.clone(), Some("session-1".to_string()));
        cache.retain_live_agents(&[]);

        assert_eq!(
            cache.bound_path_for_agent(&agent, extract_claude_session_id),
            None
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_cache_drops_codex_binding_when_session_id_changes() {
        let path = write_temp_jsonl(
            "codex-binding-session-id-change",
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:01:40.000Z","source":"cli"}}"#,
        );
        let agent = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 90);
        let mut cache = SessionCache::new();

        cache.bind_agent_path(&agent, path.clone(), Some("session-1".to_string()));
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"session-2","cwd":"/tmp/project","timestamp":"1970-01-01T00:03:20.000Z","source":"cli"}}"#,
        )
        .unwrap();

        assert_eq!(
            cache.bound_path_for_agent(&agent, extract_codex_session_id),
            None
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_cache_prevents_double_claiming_same_jsonl_path() {
        let path = write_temp_jsonl(
            "codex-binding-owner",
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:01:40.000Z","source":"cli"}}"#,
        );
        let owner = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 90);
        let other = detected_agent(AgentKind::Codex, "%2", 102, "/tmp/project", 95);
        let mut cache = SessionCache::new();

        cache.bind_agent_path(&owner, path.clone(), Some("session-1".to_string()));
        cache.bind_agent_path(&other, path.clone(), Some("session-1".to_string()));

        assert_eq!(
            cache.bound_path_for_agent(&owner, extract_codex_session_id),
            Some(path.clone())
        );
        assert_eq!(
            cache.bound_path_for_agent(&other, extract_codex_session_id),
            None
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn codex_selects_fresher_candidate_over_stale_bound_path() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "agentmux-codex-select-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&sessions_dir).unwrap();

        let current_path = sessions_dir.join("current.jsonl");
        let candidate_path = sessions_dir.join("candidate.jsonl");
        fs::write(
            &current_path,
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:01:40.000Z","source":"cli"}}"#,
        )
        .unwrap();
        fs::write(
            &candidate_path,
            r#"{"type":"session_meta","payload":{"id":"session-2","cwd":"/tmp/project","timestamp":"1970-01-01T00:16:35.000Z","source":"cli"}}"#,
        )
        .unwrap();

        let agent = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 900);
        let mut cache = SessionCache::new();
        cache.bind_agent_path(&agent, current_path.clone(), Some("session-1".to_string()));
        set_file_mtime_secs(&current_path, 0);

        let selected = select_codex_jsonl_path(&sessions_dir, &agent, &mut cache, 1000).unwrap();
        assert_eq!(selected, candidate_path);

        let _ = fs::remove_file(current_path);
        let _ = fs::remove_file(candidate_path);
        let _ = fs::remove_dir(sessions_dir);
    }

    #[test]
    fn codex_switches_live_bound_path_to_newer_candidate() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "agentmux-codex-live-keep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&sessions_dir).unwrap();

        let current_path = sessions_dir.join("current.jsonl");
        let candidate_path = sessions_dir.join("candidate.jsonl");
        fs::write(
            &current_path,
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:15:30.000Z","source":"cli"}}"#,
        )
        .unwrap();
        fs::write(
            &candidate_path,
            r#"{"type":"session_meta","payload":{"id":"session-2","cwd":"/tmp/project","timestamp":"1970-01-01T00:16:35.000Z","source":"cli"}}"#,
        )
        .unwrap();

        let agent = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 120);
        let mut cache = SessionCache::new();
        cache.bind_agent_path(&agent, current_path.clone(), Some("session-1".to_string()));

        let selected = select_codex_jsonl_path(&sessions_dir, &agent, &mut cache, 1000).unwrap();
        assert_eq!(selected, candidate_path);

        let _ = fs::remove_file(current_path);
        let _ = fs::remove_file(candidate_path);
        let _ = fs::remove_dir(sessions_dir);
    }

    #[test]
    fn codex_clear_style_reset_rebinds_before_new_token_count() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "agentmux-codex-clear-reset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&sessions_dir).unwrap();

        let current_path = sessions_dir.join("current.jsonl");
        let candidate_path = sessions_dir.join("candidate.jsonl");
        fs::write(
            &current_path,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:15:30.000Z","source":"cli"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"output_tokens":90},"last_token_usage":{"input_tokens":10},"model_context_window":200000}}}"#
            ),
        )
        .unwrap();
        fs::write(
            &candidate_path,
            r#"{"type":"session_meta","payload":{"id":"session-2","cwd":"/tmp/project","timestamp":"1970-01-01T00:16:35.000Z","source":"cli"}}"#,
        )
        .unwrap();

        let agent = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 120);
        let mut cache = SessionCache::new();
        cache.bind_agent_path(&agent, current_path.clone(), Some("session-1".to_string()));

        let selected = select_codex_jsonl_path(&sessions_dir, &agent, &mut cache, 1000).unwrap();
        assert_eq!(selected, candidate_path);

        let _ = fs::remove_file(current_path);
        let _ = fs::remove_file(candidate_path);
        let _ = fs::remove_dir(sessions_dir);
    }

    #[test]
    fn refresh_tracked_codex_details_uses_existing_bound_session_details() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "agentmux-codex-refresh-bound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&sessions_dir).unwrap();

        let current_path = sessions_dir.join("current.jsonl");
        let candidate_path = sessions_dir.join("candidate.jsonl");
        fs::write(
            &current_path,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:15:30.000Z","source":"cli"}}"#,
                "\n",
                r#"{"type":"turn_context","payload":{"model":"gpt-5.4","effort":"high"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12,"output_tokens":4},"last_token_usage":{"input_tokens":12},"model_context_window":200000}}}"#
            ),
        )
        .unwrap();
        fs::write(
            &candidate_path,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"session-2","cwd":"/tmp/project","timestamp":"1970-01-01T00:16:35.000Z","source":"cli"}}"#,
                "\n",
                r#"{"type":"turn_context","payload":{"model":"gpt-5.4","effort":"high"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3,"output_tokens":1},"last_token_usage":{"input_tokens":3},"model_context_window":200000}}}"#
            ),
        )
        .unwrap();

        let mut cache = SessionCache::new();
        let detected = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 120);
        cache.bind_agent_path(
            &detected,
            current_path.clone(),
            Some("session-1".to_string()),
        );

        let agent = tracked_agent_info("%1", 101, "/tmp/project", &current_path, "session-1");
        let details = refresh_tracked_details(&agent, 120, &mut cache);

        assert_eq!(details.session_id.as_deref(), Some("session-1"));
        assert_eq!(details.jsonl_path.as_deref(), Some(current_path.as_path()));
        assert_eq!(details.input_tokens, 12);
        assert_eq!(details.output_tokens, 4);

        let _ = fs::remove_file(current_path);
        let _ = fs::remove_file(candidate_path);
        let _ = fs::remove_dir(sessions_dir);
    }

    #[test]
    fn refresh_bound_codex_details_accepts_resumed_old_session_without_new_writes() {
        let path = write_temp_jsonl(
            "codex-resume-old-session",
            concat!(
                r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:01:40.000Z","source":"cli"}}"#,
                "\n",
                r#"{"type":"turn_context","payload":{"model":"gpt-5.4","effort":"high"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":321,"output_tokens":45},"last_token_usage":{"input_tokens":12},"model_context_window":200000}}}"#
            ),
        );
        set_file_mtime_secs(&path, 0);

        let mut cache = SessionCache::new();
        let details = refresh_bound_details(
            AgentKind::Codex,
            Some(&path),
            Some("session-1"),
            30,
            true,
            &mut cache,
        );

        assert_eq!(details.session_id.as_deref(), Some("session-1"));
        assert_eq!(details.input_tokens, 321);
        assert_eq!(details.output_tokens, 45);
        assert_eq!(details.jsonl_path.as_deref(), Some(path.as_path()));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn refresh_bound_claude_details_accepts_resumed_old_session_without_new_writes() {
        let path = write_temp_jsonl(
            "claude-resume-old-session",
            concat!(
                r#"{"sessionId":"session-1","timestamp":"1970-01-01T00:01:40.000Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":321,"output_tokens":45},"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#,
                "\n",
                r#"{"sessionId":"session-1","message":{"role":"user","content":"resume me"}}"#
            ),
        );
        set_file_mtime_secs(&path, 0);

        let mut cache = SessionCache::new();
        let details = refresh_bound_details(
            AgentKind::ClaudeCode,
            Some(&path),
            Some("session-1"),
            30,
            true,
            &mut cache,
        );

        assert_eq!(details.session_id.as_deref(), Some("session-1"));
        assert_eq!(details.input_tokens, 321);
        assert_eq!(details.output_tokens, 45);
        assert_eq!(details.jsonl_path.as_deref(), Some(path.as_path()));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn codex_keeps_scored_bound_path_over_unscorable_candidate() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "agentmux-codex-keep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&sessions_dir).unwrap();

        let current_path = sessions_dir.join("current.jsonl");
        let candidate_path = sessions_dir.join("candidate.jsonl");
        fs::write(
            &current_path,
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:15:30.000Z","source":"cli"}}"#,
        )
        .unwrap();
        fs::write(
            &candidate_path,
            r#"{"type":"event_msg","payload":{"type":"user_message"}}"#,
        )
        .unwrap();

        let agent = detected_agent(AgentKind::Codex, "%1", 101, "/tmp/project", 70);
        let mut cache = SessionCache::new();
        cache.bind_agent_path(&agent, current_path.clone(), Some("session-1".to_string()));

        let selected = select_codex_jsonl_path(&sessions_dir, &agent, &mut cache, 1000).unwrap();
        assert_eq!(selected, current_path);

        let _ = fs::remove_file(current_path);
        let _ = fs::remove_file(candidate_path);
        let _ = fs::remove_dir(sessions_dir);
    }

    #[test]
    fn codex_session_elapsed_uses_session_meta_timestamp() {
        let path = write_temp_jsonl(
            "codex-session-elapsed",
            r#"{"type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","timestamp":"1970-01-01T00:01:40.000Z","source":"cli"}}"#,
        );

        assert_eq!(codex_session_elapsed_secs(&path, 160), Some(60));

        let _ = fs::remove_file(path);
    }
}
