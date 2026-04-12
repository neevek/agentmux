# Conservative Global Binding: Pane-to-Session Mapping Redesign

## Context

The sidebar displays stats (tokens, cost, state, model) for each agent pane by binding panes to JSONL session files on disk. The current binding architecture processes panes sequentially in scan order, with each pane greedily claiming the best available unowned file. This produces three classes of bugs when `/new`, `/clear`, or session restarts occur:

1. **Stats reset on wrong pane**: A different pane steals the new file, showing reset stats for the wrong agent
2. **No stats updates**: The correct pane can't find its own file (stolen by another), stays on stale data
3. **Selection-pane mismatch**: AgentInfo for a pane_id carries wrong session data, making the sidebar look incorrect even though pane_id-based selection tracking is sound

Root cause: per-pane greedy binding with scan-order-dependent ownership. Whichever pane processes first claims unowned files, regardless of which pane actually created them.

## Design Principles

1. **Never bind unless guaranteed** — If there's any ambiguity about which pane owns a file, leave it unbound. Show "no data" rather than wrong data.
2. **Global assignment** — Process all panes together in a single pass. Binding decisions consider ALL panes and ALL files simultaneously, eliminating ordering dependency.
3. **Self-healing via elimination** — Ambiguous cases resolve naturally as panes become distinguishable. The system converges to correct bindings without hacks.
4. **CPU-efficient** — Skip global resolve when all bindings are stable. Cache directory listings. Avoid redundant JSONL parsing.

## Architecture: Two-Phase Binding

### Phase 1 — Classify (read-only, no SessionCache mutations)

For each agent pane, determine its binding status:

```
enum BindingStatus {
    Keep(PathBuf),                          // Valid binding: file exists, session_id matches
    NeedsBinding {                          // Invalid/missing binding: needs resolution
        cwd: String,
        process_age: u64,
        old_file_mtime: Option<SystemTime>, // When old file was last written
    },
}
```

Classification rules:
- Existing binding where file exists AND session_id matches → `Keep`
- Binding where file is deleted → `NeedsBinding`
- Binding where session_id changed → `NeedsBinding`
- No existing binding → `NeedsBinding`
- **File is stale but valid → `Keep`** (staleness alone is NOT a trigger)

### Phase 2 — Global Resolve (single atomic assignment)

Only runs when at least one pane has `NeedsBinding` status AND new unclaimed files exist.

```
1. Group NeedsBinding panes by project directory (Claude) or CWD (Codex)
2. For each group:
     available_files = unclaimed files in this directory (not in path_owners)
     if no available_files → skip (nothing to bind)
     if 1 pane, 1+ files → bind pane to best age-matched file   [GUARANTEED]
     if N panes, M files (N > 1):
         for each file:
             score all candidate panes
             if exactly 1 pane scores decisively → bind it       [GUARANTEED]
             else → don't bind (ambiguous)                       [WAIT]
3. Apply all decided bindings atomically to SessionCache
```

### Phase 3 — Read Details

For each pane:
- If bound: read JSONL for stats, state, tokens, model
- If unbound: return skeleton AgentInfo with kind, pane_id, state=Idle, zero stats, details_ready=false

## Binding Guarantee Criteria

A binding is only made when exactly one of these conditions holds:

| Condition | Logic | Guarantee Level |
|-----------|-------|-----------------|
| **Sole candidate** | Exactly 1 pane with NeedsBinding in this project dir | 100% — no ambiguity |
| **Age match** | Process age matches file session_start within 5s AND no other pane within 30s | 100% — for initial binding |
| **Clear temporal winner** | Pane's old file stopped within TEMPORAL_CLOSE_SECS (10s) of new file creation AND next-closest pane is >TEMPORAL_FAR_SECS (30s) away | 100% — strong causal signal. The 10s/30s values are tunable constants; 10s captures the typical /new→file-creation gap, 30s ensures a 3x margin of safety. |
| **Keep existing** | File exists, session_id matches, pane alive | 100% — already bound |

If NONE of these conditions hold → **don't bind**. The pane remains unbound with zero stats.

## Rebinding Triggers

**Old model (removed):** Staleness (file not modified in 15-120s) triggers rebinding probes.

**New model:** Rebinding is triggered ONLY by:

| Trigger | Action |
|---------|--------|
| File deleted | Unbind pane, mark as NeedsBinding |
| Session_id changes (file reused by different session) | Unbind pane, mark as NeedsBinding |
| Process PID changes AND process age doesn't match binding | Unbind pane, mark as NeedsBinding |
| File stale (idle agent) | **KEEP binding — NOT a trigger** |

**File-arrival detection (new):** On each scan, compare the current directory listing against a cached listing from the previous scan. If new files appeared AND at least one pane in the group has NeedsBinding → run Phase 2 for that group.

## Self-Healing for Ambiguous Cases

When binding is refused (ambiguous), the system waits. Resolution paths:

1. **User sends first message** → new file becomes live → on next scan, if this pane is the sole NeedsBinding pane remaining, it gets the file
2. **Other pane resumes activity** → its old file gets new writes → its binding becomes valid again → it's no longer NeedsBinding → remaining pane is sole candidate
3. **Both panes send messages** → two new files appear → 1:1 matching via age/temporal proximity
4. **Process exits** in one pane → pane removed from agents list → remaining pane becomes sole candidate

Typical resolution: 1-3 seconds after user sends first message (one scan cycle).

## Performance: CPU Efficiency

### Skip Phase 2 when stable

The common case: all panes have valid bindings (no `/new`, no `/clear`). Phase 2 is entirely skipped — zero directory scanning overhead.

```
if all panes have BindingStatus::Keep → skip Phase 2
```

### Cache directory listings

Maintain a `HashMap<PathBuf, CachedDirListing>` in SessionCache:
- `CachedDirListing { files: HashSet<PathBuf>, scanned_at: Instant }`
- Re-scan directory only when:
  - At least one pane has NeedsBinding status, AND
  - Last scan was >1s ago (rate limit)
- Detect new files by diffing current listing against cached listing
- Reuse existing `CodexSessionIndex` for Codex files (already caches metadata)

### Lazy JSONL parsing

- Only parse file headers (session_id, session_start_secs) when establishing NEW bindings
- For existing Keep bindings, reuse cached session_start_secs from SessionBinding
- For stats/tokens/state, read only the last 32KB of JSONL (existing approach, already efficient)

### No additional timer or polling

- File-arrival detection piggybacks on existing 3s leader refresh cycle
- No new kqueue/inotify/fs-event infrastructure
- No additional lsof or ps calls beyond what's already done

### Estimated overhead

| Operation | Frequency | Cost |
|-----------|-----------|------|
| Phase 1 classify | Every 3s refresh | ~0 (HashMap lookups + file exists checks) |
| Phase 2 resolve | Only when NeedsBinding + new files | ~1-5ms (directory listing + scoring) |
| Directory diff | Only when triggered | ~0.1ms (HashSet difference) |
| JSONL header parse | Only on new binding | ~0.5ms per file |

Steady-state (all bindings stable): Phase 2 never runs. Same CPU profile as current implementation minus the staleness-based probing and rebind backoff.

## Impact on Existing Code

### Files to modify

**`src/detect/state.rs` (primary changes):**
- Add `BindingStatus` enum
- Refactor `select_claude_jsonl_path()` → `classify_claude_binding()` (returns BindingStatus, doesn't bind)
- Refactor `select_codex_jsonl_path()` → `classify_codex_binding()` (returns BindingStatus, doesn't bind)
- Add `resolve_bindings_globally()` (Phase 2: score, filter, assign)
- Add `CachedDirListing` to SessionCache for file-arrival detection
- Remove staleness-based rebinding triggers (BOUND_PATH_LIVE_SECS, CLAUDE_REPLACEMENT_LIVE_SECS usage in rebinding)
- Remove `find_newer_claude_session_replacement()` and `find_newer_codex_session_replacement_cached()` (replaced by global resolve)
- Remove `should_probe_bound_session()`, `record_rebind_probe_result()`, rebind backoff fields
- Remove `rebind_probe_due_at`, `rebind_probe_backoff_secs` from SessionBinding

**`src/detect/mod.rs`:**
- Refactor `agents_from_detected()` to use two-phase flow
- Refactor `refresh_agents_incremental_with_elapsed()` to use two-phase flow (incremental path)
- Remove `scan_order_key()` and `binding_priority()` (no longer needed — not scan-order-dependent)

### What stays the same

- `Selection` enum and `sync_selection_from_focus()` — already correct (pane_id-based)
- `LiveSnapshot` publication/consumption
- Leader/follower architecture
- Process tree walking (`process.rs`)
- History tracking (`history.rs`)
- Rendering (`sidebar/render.rs`)
- Input handling (`sidebar/input.rs`)

## Verification

1. **Unit tests**: Test `resolve_bindings_globally()` with scenarios:
   - Single pane, single file → binds
   - Two panes, one new file, clear temporal winner → binds winner
   - Two panes, one new file, ambiguous → neither binds
   - Two panes, two new files → matches 1:1
   - All panes have Keep bindings → Phase 2 skipped
2. **Integration test**: Start two Claude panes in same project, `/new` on one, verify correct pane gets new stats
3. **Manual test**: Run sidebar with multiple agents, exercise `/new`, `/clear`, resume scenarios
4. **CPU test**: Monitor CPU usage during steady state — should be same or lower than current
