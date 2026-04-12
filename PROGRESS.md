# PROGRESS: Reduce sidebar refresh CPU below 1% without behavior regressions
**Status:** complete
**Updated:** 2026-04-11

## Goal
Use live sampling plus code inspection to cut `agentmux sidebar` refresh spikes from ~4% CPU every 2-3 seconds to below 1%, while preserving existing detection, rebinding, and sidebar behavior.

## Research Findings
### Summary
Live `sample` output shows the hot path is the leader refresh loop, not rendering. The dominant active work is:
- `src/sidebar/mod.rs` leader refresh every `POLL_INTERVAL_MS` calling `tmux::list_session_panes()`
- `src/detect/mod.rs` incremental refresh calling `process::query_process_elapsed()`
- `src/detect/state.rs` Codex rebinding/session selection doing recursive `walk_jsonl()` plus repeated JSON first-line parsing
- Secondary steady-state overhead from `tmux::window_focus()` polling every 250ms

### Evidence
- `rtk sample 37088 5` on a live `agentmux sidebar` process showed:
- `sidebar::refresh_leader_state()` on the active stack with `list_session_panes`, `refresh_agents_incremental_from_panes`, and `select_codex_jsonl_path`
- `detect::state::find_codex_jsonl_for_cwd_at()` and `parse_codex_session_meta()` as the heaviest pure-Rust work
- `tmux::window_focus()` and other `tmux_output()` calls dominating child-process spawn/wait time

### Key files
- `src/sidebar/mod.rs` — leader/follower loop, refresh cadence, pane fingerprint sweep, focus polling
- `src/detect/mod.rs` — incremental refresh and full-scan fallback
- `src/detect/state.rs` — Codex/Claude JSONL selection, binding cache, token parsing, JSONL walk helpers
- `src/tmux/mod.rs` — every tmux subprocess invocation
- `src/detect/history.rs` — stats aggregation; appears relatively cheap in steady state

### Code flow
1. Leader refresh in `refresh_leader_state()` runs every 3s.
2. It calls `tmux::list_session_panes(session)`.
3. It tries `detect::refresh_agents_incremental_from_panes(...)`, which always spawns `ps` for tracked PIDs.
4. Per Codex agent, `refresh_tracked_details()` may enter `codex_details()`.
5. `codex_details()` calls `select_codex_jsonl_path()`, which can recursively walk the entire Codex sessions tree and parse candidate JSONL metadata.
6. Discovery sweeps and full scans also reuse the same expensive walk/parse patterns.

### Scope / likely root cause
- The process is paying repeated global filesystem scans for Codex session discovery even when the session tree is mostly unchanged.
- Current helper structure re-walks `~/.codex/sessions` in multiple places (`binding_priority`, `find_codex_jsonl_for_cwd_at`, `find_newer_codex_session_replacement`).
- Tmux subprocess usage is also measurable, but the worst avoidable Rust-side work is repeated session-file enumeration and metadata parsing.

### Dependencies & side effects
- The fix should preserve:
- current incremental refresh behavior
- Codex session rebinding when a newer live session takes over a pane
- Claude behavior
- ordering semantics based on binding priority / elapsed time
- existing tests in `detect::state`

### Risks
- Over-caching Codex session metadata could delay discovery of new sessions or stale-path replacement.
- Replacing binding-priority scans must not change existing ordering for ambiguous candidates.
- Any tmux polling changes must not regress active window selection behavior.

## Implementation Results
### What changed
- Added a cache-backed Codex session index in `src/detect/state.rs`.
- The index refreshes at most once per second per sidebar process, reuses recursive JSONL discovery across all Codex helpers, and reparses per-file `session_meta` only when file metadata changes.
- Codex selection, replacement, binding priority, and elapsed-time display now reuse cached session metadata instead of independently walking `~/.codex/sessions`.
- `token_count` detection is now cached lazily per file metadata for Codex tie-breaking.
- `src/sidebar/mod.rs` now uses the already-known `sidebar_window_id` when clearing unseen items, removing one steady-state `tmux display-message` subprocess from each update application.
- Added cached canonical-path reuse for Codex cwd matching so repeated candidate scans no longer call `realpath`/`canonicalize` on every refresh.
- Reverted the experimental process-elapsed throttling after it caused session-item stats/details to lag or disappear; incremental refresh is back to exact `ps` elapsed queries each cycle.

### Verification results
- `rtk cargo test detect::state -- --nocapture`
- `rtk cargo test sidebar:: -- --nocapture`
- `rtk cargo test`
- `rtk cargo clippy -- -D warnings`
- Built and ran this branch’s binary with `rtk cargo build`.
- Post-change live check:
- `sample` of the temporary sidebar process from this checkout no longer showed the previous Codex JSONL walk/parse stack dominating the sampled run.
- `ps -p 80275 -o pid=,ppid=,pcpu=,etime=,command=` showed the temporary process at `0.1%` CPU during steady-state verification.
- Regression verification:
- Restarted the live sidebar pane on PID `57207` after rebuilding this branch.
- `rtk proxy tmux capture-pane -pt %168` showed session-item stats updating again on the restarted pane.
- `rtk ps -p 57207 -o pid=,pcpu=,etime=,command=` showed `0.1%` CPU both before and after a fresh 60-second live sample.
- Fresh 60-second live sample on PID `57207` no longer showed `codex_cwds_match()` / `canonicalize_if_exists()` on the hot path; remaining steady-state work is mostly `tmux::window_focus()` plus `query_process_elapsed()`.

### Notes on profiling
- The original hot-path sample that motivated the change captured leader refresh work in `refresh_leader_state()`, `query_process_elapsed()`, and Codex session selection.
- The post-change temporary sample landed mostly in the normal `poll_input()` / `window_focus()` path instead of the old recursive Codex scan stack, which is the expected shape after removing the repeated JSONL walk/parse churn.

## Plan
### Step 1: Add a cached Codex session index in detect state
**Files:** `src/detect/state.rs`
**What:** Centralize Codex JSONL discovery into a cache-backed index that stores file paths plus cheap metadata needed by selection (`session_meta` cwd/session id/start time, mtime/size).

**Before**
```rust
let mut all_files: Vec<PathBuf> = Vec::new();
walk_jsonl(sessions_dir, &mut all_files);
```

**After**
```rust
let files = cache.codex_session_files(sessions_dir);
for file in files {
    // reuse cached metadata and only parse/update when file metadata changed
}
```

### Step 2: Route Codex selection helpers through the cache
**Files:** `src/detect/state.rs`, `src/detect/mod.rs`
**What:** Make `binding_priority`, `find_codex_jsonl_for_cwd_at`, and `find_newer_codex_session_replacement` reuse the cached index instead of recursively walking the directory each time.

**Before**
```rust
walk_jsonl(sessions_dir, &mut files);
files.into_iter().filter_map(|path| codex_candidate(&path, ...))
```

**After**
```rust
cache
    .codex_candidates(sessions_dir)
    .filter_map(|entry| codex_candidate_from_cached(entry, ...))
```

### Step 3: Trim unnecessary tmux work inside refresh updates
**Files:** `src/sidebar/mod.rs`, optionally `src/tmux/mod.rs`
**What:** Review the per-refresh/sidebar-update path for avoidable tmux subprocess calls and remove any that are not needed for steady state.

**Current suspect**
```rust
tmux::current_window_id()
```

**Target**
```rust
// reuse already-known sidebar/current window context when possible
```

### Files NOT modified
- `src/sidebar/render.rs` — sample does not implicate rendering.
- `src/sidebar/input.rs` — input polling itself is asleep in `poll`, not a CPU issue.
- `src/detect/history.rs` — current steady-state path appears inexpensive relative to detection/rebinding work.

### Verification
- `rtk cargo test detect::state`
- `rtk cargo test`
- `rtk cargo clippy -- -D warnings`
- Run a live `sample` against the updated sidebar process and confirm hot-path reduction / lower steady refresh cost.

### Rollback
Revert only the Codex session-index caching and any tmux refresh micro-optimizations if they prove behaviorally risky.

## Implementation Log
- [x] Research live hot path with `sample` and code inspection.
- [x] Step 1: Add cache-backed Codex session index.
- [x] Step 2: Reuse cached index in selection/rebinding helpers.
- [x] Step 3: Remove unnecessary steady-state tmux work in update application.
- [x] Verify tests and live sampling.

## Dead Ends
- Rendering is not the bottleneck.
- History aggregation does not appear to be the main steady-state CPU source.

## Open Questions
- `window_focus()` polling is now the most visible remaining steady-state stack in the temporary post-change sample. It is not required for this fix, but it is the next place to look if more CPU reduction is needed later.

## Compaction Log
- Research compacted from live `sample` output plus code tracing in `sidebar`, `detect`, and `tmux`.
