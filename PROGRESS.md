# PROGRESS: Fix sidebar stats binding to wrong session item and remove exited sessions promptly
**Status:** researching
**Updated:** 2026-04-11

## Goal
Keep sidebar stats attached to the correct session item and remove Codex sessions from the sidebar as soon as they exit.

## Research Findings
### Summary
The user clarified the visible failure is data refresh, not row selection. The next investigation needs to focus on the incremental refresh path in `detect::refresh_agents_incremental_from_panes()` and any session-history binding used to map Codex stats onto sidebar items, plus the code path that drops an agent when its pane stops being an active Codex session.

### Key files
- `src/sidebar/mod.rs` — owns the leader refresh loop and publishes sidebar snapshots.
- `src/detect/mod.rs` — incremental/full refresh entry points and agent ordering.
- `src/detect/state.rs` — Codex session-detail selection and binding.
- `src/detect/history.rs` — active-session overlay logic that can affect per-item stats.

### Code flow
Leader refresh in `src/sidebar/mod.rs` builds `agents` via incremental refresh when possible, otherwise via full scan, then publishes the snapshot used by all sidebars. If the wrong `AgentInfo` keeps the wrong session details, or if a pane that is no longer a Codex session is still preserved in `agents`, both user-reported symptoms would follow.

### Dependencies & side effects
The fix will likely touch detect/refresh logic rather than rendering. It must preserve existing ordering and avoid regressing fast incremental updates.

### Risks & unknowns
The likely risk is mixing up process identity with session identity during incremental refresh, especially if panes are reused after exit or multiple Codex sessions have similar elapsed values.

## Plan
### Step 1: Reproduce the mapping and exit behavior from code
**Files:** `src/detect/mod.rs`, `src/detect/state.rs`, `src/detect/history.rs`, `src/sidebar/mod.rs`
**What:** Read the refresh path and find where stats can stick to the wrong `AgentInfo` or where exited sessions are not dropped.

### Step 2: Patch the refresh/removal logic
**Files:** TBD from research
**What:** Make incremental updates preserve correct session binding per pane/process and remove stale entries when a pane is no longer an active Codex session.

### Files NOT modified (and why)
- `src/sidebar/render.rs` — current evidence does not point to rendering.

### Verification
- Run targeted tests around incremental refresh/session binding.
- Run full `cargo test`.

### Rollback
Revert only the detect/sidebar refresh changes added for this bug once the correct root cause is known.

## Implementation Log
- [ ] Step 1: Trace wrong stats binding and stale exit behavior in refresh logic.
- [ ] Step 2: Patch and verify.

## Dead Ends
- The earlier selection/focus hypothesis does not explain the user’s two clarified symptoms.

## Open Questions
- None at the moment.

## Compaction Log
- Re-scope: user clarified the real bug is wrong stats attachment and stale exited sessions, so investigation moved from focus polling to detect/refresh logic.
