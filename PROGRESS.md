# PROGRESS: Fix Claude /new and /clear session rebinding
**Status:** complete
**Updated:** 2026-04-11

## Goal
Fix stale stats and wrong focus when Claude sessions reset via /new or /clear.

## Research Findings

### Summary
When Claude runs /new or /clear, the process stays alive (same PID, same elapsed_secs) but creates a new JSONL file. The binding system can't detect this because (a) the old file's session_id hasn't changed, so the binding remains valid, and (b) age-matching always prefers the old file (its start age matches the process age). The new file is left unclaimed and may get stolen by another Claude pane.

### Root Cause
`select_claude_jsonl_path` (state.rs:453-469):
1. `bound_path_for_agent()` returns old file A (session_id in A unchanged)
2. `find_claude_jsonl_for_cwd_at()` scores A as perfect match (age 0) vs new file C (age >> 300s threshold)
3. Result: pane stays on stale file A; new file C is unclaimed

Codex has `find_newer_codex_session_replacement()` (state.rs:533-558) for exactly this case. Claude lacks it.

### Key files
- `src/detect/state.rs:453-469` — `select_claude_jsonl_path` — needs replacement check
- `src/detect/state.rs:533-558` — `find_newer_codex_session_replacement` — pattern to follow
- `src/detect/state.rs:520-531` — `bound_path_is_recent` — staleness check
- `src/detect/state.rs:796-818` — `claude_session_start_secs` — start time extraction

### Focus bug (secondary)
Likely a consequence of bug 1: when stats migrate to the wrong item, the user perceives the active item as "focused". Should resolve naturally with the stats fix.

## Plan

### Step 1: Add `find_newer_claude_session_replacement()`
**File:** `src/detect/state.rs`
Add constant and function after `bound_path_is_recent` (line 531).
Guards: current file must be stale; replacement must have started within 5s of current file's mtime.

### Step 2: Wire into `select_claude_jsonl_path()` + test
**File:** `src/detect/state.rs:453-469`
Before `find_claude_jsonl_for_cwd_at`, check for a stale-to-newer replacement.

### Files NOT modified
- `detect/mod.rs` — refresh logic unchanged
- `sidebar/` — focus tracking correct (by pane_id)
- `detect/process.rs` — process detection unaffected

### Verification
`cargo test` + `cargo clippy` + manual /clear test

## Implementation Log
- [x] Step 1: Add `CLAUDE_RESET_GAP_SECS` constant and `find_newer_claude_session_replacement()`
- [x] Step 2: Wire into `select_claude_jsonl_path()`, add 2 tests (rebind + no-steal)
