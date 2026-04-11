use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::detect::AgentInfo;
use crate::detect::history::AggregatedStats;

pub const POLL_INTERVAL_MS: u64 = 3_000;
pub const LEASE_STALE_MS: u64 = POLL_INTERVAL_MS * 3;
pub const SNAPSHOT_STALE_MS: u64 = LEASE_STALE_MS;
pub const SNAPSHOT_ACTIVATION_MAX_AGE_MS: u64 = 60 * 60 * 1_000;
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const SNAPSHOT_FILE_NAME: &str = "live_snapshot.json";
const RUNTIME_DB_FILE_NAME: &str = "runtime.db";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LeaderLease {
    pub pid: Option<u32>,
    pub heartbeat_ms: u64,
    pub epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotMetadata {
    pub modified_ms: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveSnapshot {
    pub schema_version: u32,
    pub leader_pid: u32,
    pub leader_epoch: u64,
    pub written_at_ms: u64,
    pub agents: Vec<AgentInfo>,
    pub stats: AggregatedStats,
}

#[derive(Debug, Clone, PartialEq)]
struct LiveSnapshotState {
    leader_pid: u32,
    leader_epoch: u64,
    agents: Vec<AgentInfo>,
    stats: AggregatedStats,
}

pub struct RuntimeStore {
    db: Option<sqlite::Connection>,
    snapshot_path: PathBuf,
    last_snapshot_meta: Option<SnapshotMetadata>,
    last_published: Option<LiveSnapshotState>,
}

impl RuntimeStore {
    pub fn new(session: &str) -> Self {
        let dir = config::config_dir();
        let _ = fs::create_dir_all(&dir);
        let snapshot_path = session_scoped_path(&dir, SNAPSHOT_FILE_NAME, session);
        let db = open_db_at(&session_scoped_path(&dir, RUNTIME_DB_FILE_NAME, session));

        Self {
            db,
            snapshot_path,
            last_snapshot_meta: None,
            last_published: None,
        }
    }

    pub fn try_claim_leader(&self) -> Option<u64> {
        let db = self.db.as_ref()?;
        try_claim_leader(db, std::process::id(), unix_now_ms())
    }

    pub fn read_lease(&self) -> LeaderLease {
        self.db.as_ref().map(read_leader_lease).unwrap_or_default()
    }

    pub fn lease_is_stale(&self, lease: &LeaderLease) -> bool {
        lease_is_stale(lease, unix_now_ms())
    }

    pub fn heartbeat_leader(&self, epoch: u64) -> bool {
        let Some(db) = self.db.as_ref() else {
            return false;
        };
        heartbeat_leader(db, std::process::id(), epoch, unix_now_ms())
    }

    pub fn release_leader(&self, epoch: u64) {
        let Some(db) = self.db.as_ref() else {
            return;
        };
        release_leader(db, std::process::id(), epoch);
    }

    pub fn load_snapshot_for_activation(&mut self) -> Option<LiveSnapshot> {
        let metadata = snapshot_metadata(&self.snapshot_path)?;
        let snapshot = read_snapshot(&self.snapshot_path)?;
        if snapshot_is_valid(
            snapshot.schema_version,
            &snapshot,
            SNAPSHOT_ACTIVATION_MAX_AGE_MS,
        ) {
            self.last_snapshot_meta = Some(metadata);
            Some(snapshot)
        } else {
            None
        }
    }

    pub fn load_snapshot_if_changed(&mut self, lease: &LeaderLease) -> Option<LiveSnapshot> {
        let metadata = snapshot_metadata(&self.snapshot_path)?;
        if self.last_snapshot_meta == Some(metadata) {
            return None;
        }
        let snapshot = read_snapshot(&self.snapshot_path)?;
        if snapshot_matches_lease(&snapshot, lease) {
            self.last_snapshot_meta = Some(metadata);
            Some(snapshot)
        } else {
            None
        }
    }

    pub fn publish_snapshot(&mut self, epoch: u64, agents: &[AgentInfo], stats: &AggregatedStats) {
        let state = LiveSnapshotState {
            leader_pid: std::process::id(),
            leader_epoch: epoch,
            agents: agents.to_vec(),
            stats: stats.clone(),
        };
        let snapshot = LiveSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            leader_pid: state.leader_pid,
            leader_epoch: state.leader_epoch,
            written_at_ms: unix_now_ms(),
            agents: state.agents.clone(),
            stats: state.stats.clone(),
        };

        if write_snapshot(&self.snapshot_path, &snapshot).is_ok() {
            self.last_snapshot_meta = snapshot_metadata(&self.snapshot_path);
            self.last_published = Some(state);
        }
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn session_scoped_path(base_dir: &Path, file_name: &str, session: &str) -> PathBuf {
    let session = encode_session_component(session);
    let (stem, ext) = file_name
        .rsplit_once('.')
        .map(|(stem, ext)| (stem.to_string(), format!(".{ext}")))
        .unwrap_or_else(|| (file_name.to_string(), String::new()));
    base_dir.join(format!("{stem}.{session}{ext}"))
}

fn encode_session_component(session: &str) -> String {
    let mut encoded = String::with_capacity(session.len());
    for byte in session.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('_');
                encoded.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
                encoded.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
            }
        }
    }
    if encoded.is_empty() {
        "default".to_string()
    } else {
        encoded
    }
}

fn process_is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn open_db_at(path: &Path) -> Option<sqlite::Connection> {
    let parent = path.parent()?;
    let _ = fs::create_dir_all(parent);
    let db = sqlite::open(path).ok()?;
    ensure_runtime_state_table(&db);
    Some(db)
}

fn ensure_runtime_state_table(db: &sqlite::Connection) {
    let _ = db.execute(
        "CREATE TABLE IF NOT EXISTS runtime_state (\
            key TEXT PRIMARY KEY,\
            value TEXT NOT NULL\
        )",
    );
}

fn read_runtime_state(db: &sqlite::Connection, key: &str) -> Option<String> {
    let mut stmt = db
        .prepare("SELECT value FROM runtime_state WHERE key = ?")
        .ok()?;
    let _ = stmt.bind((1, key));
    if stmt.next().ok()? == sqlite::State::Row {
        stmt.read::<String, _>(0).ok()
    } else {
        None
    }
}

fn write_runtime_state(db: &sqlite::Connection, key: &str, value: &str) {
    let Ok(mut stmt) = db.prepare(
        "INSERT INTO runtime_state (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    ) else {
        return;
    };
    let _ = stmt.bind((1, key));
    let _ = stmt.bind((2, value));
    let _ = stmt.next();
}

fn read_leader_lease(db: &sqlite::Connection) -> LeaderLease {
    LeaderLease {
        pid: read_runtime_state(db, "leader_pid").and_then(|value| value.parse::<u32>().ok()),
        heartbeat_ms: read_runtime_state(db, "leader_heartbeat_ms")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0),
        epoch: read_runtime_state(db, "leader_epoch")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0),
    }
}

fn lease_is_stale(lease: &LeaderLease, now_ms: u64) -> bool {
    lease.pid.is_none()
        || lease.pid.is_some_and(|pid| {
            !process_is_alive(pid) || now_ms.saturating_sub(lease.heartbeat_ms) > LEASE_STALE_MS
        })
}

fn try_claim_leader(db: &sqlite::Connection, self_pid: u32, now_ms: u64) -> Option<u64> {
    ensure_runtime_state_table(db);
    db.execute("BEGIN IMMEDIATE").ok()?;

    let lease = read_leader_lease(db);
    if lease.pid == Some(self_pid) {
        write_runtime_state(db, "leader_pid", &self_pid.to_string());
        write_runtime_state(db, "leader_heartbeat_ms", &now_ms.to_string());
        let _ = db.execute("COMMIT");
        return Some(lease.epoch);
    }

    let can_claim = lease.pid.is_none_or(|pid| {
        !process_is_alive(pid) || now_ms.saturating_sub(lease.heartbeat_ms) > LEASE_STALE_MS
    });

    if !can_claim {
        let _ = db.execute("ROLLBACK");
        return None;
    }

    let new_epoch = lease.epoch.saturating_add(1);
    write_runtime_state(db, "leader_pid", &self_pid.to_string());
    write_runtime_state(db, "leader_heartbeat_ms", &now_ms.to_string());
    write_runtime_state(db, "leader_epoch", &new_epoch.to_string());
    let _ = db.execute("COMMIT");
    Some(new_epoch)
}

#[cfg(test)]
fn seize_leader(db: &sqlite::Connection, self_pid: u32, now_ms: u64) -> Option<u64> {
    ensure_runtime_state_table(db);
    db.execute("BEGIN IMMEDIATE").ok()?;

    let lease = read_leader_lease(db);
    let new_epoch = if lease.pid == Some(self_pid) {
        lease.epoch.max(1)
    } else {
        lease.epoch.saturating_add(1).max(1)
    };

    write_runtime_state(db, "leader_pid", &self_pid.to_string());
    write_runtime_state(db, "leader_heartbeat_ms", &now_ms.to_string());
    write_runtime_state(db, "leader_epoch", &new_epoch.to_string());
    let _ = db.execute("COMMIT");
    Some(new_epoch)
}

fn heartbeat_leader(db: &sqlite::Connection, self_pid: u32, epoch: u64, now_ms: u64) -> bool {
    ensure_runtime_state_table(db);
    if db.execute("BEGIN IMMEDIATE").is_err() {
        return false;
    }

    let lease = read_leader_lease(db);
    if lease.pid != Some(self_pid) || lease.epoch != epoch {
        let _ = db.execute("ROLLBACK");
        return false;
    }

    write_runtime_state(db, "leader_pid", &self_pid.to_string());
    write_runtime_state(db, "leader_heartbeat_ms", &now_ms.to_string());
    db.execute("COMMIT").is_ok()
}

fn release_leader(db: &sqlite::Connection, self_pid: u32, epoch: u64) {
    ensure_runtime_state_table(db);
    if db.execute("BEGIN IMMEDIATE").is_err() {
        return;
    }

    let lease = read_leader_lease(db);
    if lease.pid != Some(self_pid) || lease.epoch != epoch {
        let _ = db.execute("ROLLBACK");
        return;
    }

    write_runtime_state(db, "leader_pid", "");
    write_runtime_state(db, "leader_heartbeat_ms", "0");
    let _ = db.execute("COMMIT");
}

fn snapshot_metadata(path: &Path) -> Option<SnapshotMetadata> {
    let metadata = fs::metadata(path).ok()?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    Some(SnapshotMetadata {
        modified_ms,
        size: metadata.len(),
    })
}

fn read_snapshot(path: &Path) -> Option<LiveSnapshot> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_snapshot(path: &Path, snapshot: &LiveSnapshot) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp_path = path.with_extension(format!("{}.tmp", std::process::id()));
    let content = serde_json::to_vec(snapshot).map_err(std::io::Error::other)?;
    fs::write(&tmp_path, content)?;
    fs::rename(tmp_path, path)
}

fn snapshot_matches_lease(snapshot: &LiveSnapshot, lease: &LeaderLease) -> bool {
    if !snapshot_is_valid(snapshot.schema_version, snapshot, SNAPSHOT_STALE_MS) {
        return false;
    }
    match lease.pid {
        Some(pid) => snapshot.leader_pid == pid && snapshot.leader_epoch == lease.epoch,
        None => true,
    }
}

fn snapshot_is_valid(schema_version: u32, snapshot: &LiveSnapshot, max_age_ms: u64) -> bool {
    schema_version == SNAPSHOT_SCHEMA_VERSION && snapshot_is_recent(snapshot, max_age_ms)
}

fn snapshot_is_recent(snapshot: &LiveSnapshot, max_age_ms: u64) -> bool {
    unix_now_ms().saturating_sub(snapshot.written_at_ms) <= max_age_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentmux-runtime-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lease_claim_increments_epoch() {
        let dir = temp_dir("claim");
        let db = open_db_at(&dir.join("stats.db")).unwrap();

        let first = try_claim_leader(&db, 10, 1_000).unwrap();
        let second = try_claim_leader(&db, 10, 2_000).unwrap();

        assert_eq!(first, 1);
        assert_eq!(second, 1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn seize_leader_takes_over_even_when_existing_leader_is_alive() {
        let dir = temp_dir("seize");
        let db = open_db_at(&dir.join("stats.db")).unwrap();

        write_runtime_state(&db, "leader_pid", &999_999.to_string());
        write_runtime_state(&db, "leader_heartbeat_ms", "2");
        write_runtime_state(&db, "leader_epoch", "7");

        let epoch = seize_leader(&db, 10, 3).unwrap();
        let lease = read_leader_lease(&db);

        assert_eq!(epoch, 8);
        assert_eq!(lease.pid, Some(10));
        assert_eq!(lease.epoch, 8);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_is_rejected_when_lease_mismatches() {
        let snapshot = LiveSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            leader_pid: 42,
            leader_epoch: 7,
            written_at_ms: unix_now_ms(),
            agents: Vec::new(),
            stats: AggregatedStats::default(),
        };

        assert!(snapshot_matches_lease(&snapshot, &LeaderLease::default()));
        assert!(snapshot_matches_lease(
            &snapshot,
            &LeaderLease {
                pid: Some(42),
                heartbeat_ms: unix_now_ms(),
                epoch: 7,
            }
        ));
        assert!(!snapshot_matches_lease(
            &snapshot,
            &LeaderLease {
                pid: Some(43),
                heartbeat_ms: unix_now_ms(),
                epoch: 7,
            }
        ));
    }

    #[test]
    fn publish_snapshot_refreshes_timestamp_for_unchanged_state() {
        let dir = temp_dir("snapshot-refresh");
        let snapshot_path = dir.join(SNAPSHOT_FILE_NAME);
        let mut store = RuntimeStore {
            db: None,
            snapshot_path: snapshot_path.clone(),
            last_snapshot_meta: None,
            last_published: None,
        };
        let agents = Vec::new();
        let stats = AggregatedStats::default();

        store.publish_snapshot(7, &agents, &stats);
        let first = read_snapshot(&snapshot_path).unwrap();
        let first_meta = snapshot_metadata(&snapshot_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));

        store.publish_snapshot(7, &agents, &stats);
        let second = read_snapshot(&snapshot_path).unwrap();
        let second_meta = snapshot_metadata(&snapshot_path).unwrap();

        assert!(second.written_at_ms > first.written_at_ms);
        assert_ne!(first_meta, second_meta);
        assert_ne!(first, second);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn release_leader_does_not_clear_newer_epoch() {
        let dir = temp_dir("release");
        let db = open_db_at(&dir.join("stats.db")).unwrap();

        let first_epoch = try_claim_leader(&db, 10, 1_000).unwrap();
        let second_epoch = try_claim_leader(&db, 20, 20_000).unwrap();
        release_leader(&db, 10, first_epoch);

        let lease = read_leader_lease(&db);
        assert_eq!(first_epoch, 1);
        assert_eq!(second_epoch, 2);
        assert_eq!(lease.pid, Some(20));
        assert_eq!(lease.epoch, 2);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejected_snapshot_is_not_marked_seen() {
        let dir = temp_dir("snapshot-retry");
        let snapshot_path = dir.join(SNAPSHOT_FILE_NAME);
        let snapshot = LiveSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            leader_pid: 42,
            leader_epoch: 7,
            written_at_ms: unix_now_ms(),
            agents: Vec::new(),
            stats: AggregatedStats::default(),
        };
        let mut store = RuntimeStore {
            db: None,
            snapshot_path: snapshot_path.clone(),
            last_snapshot_meta: None,
            last_published: None,
        };

        write_snapshot(&snapshot_path, &snapshot).unwrap();

        assert!(
            store
                .load_snapshot_if_changed(&LeaderLease {
                    pid: Some(99),
                    heartbeat_ms: unix_now_ms(),
                    epoch: 7,
                })
                .is_none()
        );
        assert_eq!(store.last_snapshot_meta, None);
        assert!(
            store
                .load_snapshot_if_changed(&LeaderLease {
                    pid: Some(42),
                    heartbeat_ms: unix_now_ms(),
                    epoch: 7,
                })
                .is_some()
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn activation_snapshot_accepts_recent_previous_leader_snapshot() {
        let dir = temp_dir("activation-snapshot");
        let snapshot_path = dir.join(SNAPSHOT_FILE_NAME);
        let snapshot = LiveSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            leader_pid: 42,
            leader_epoch: 7,
            written_at_ms: unix_now_ms(),
            agents: Vec::new(),
            stats: AggregatedStats::default(),
        };
        let mut store = RuntimeStore {
            db: None,
            snapshot_path: snapshot_path.clone(),
            last_snapshot_meta: None,
            last_published: None,
        };

        write_snapshot(&snapshot_path, &snapshot).unwrap();

        let loaded = store.load_snapshot_for_activation();
        assert_eq!(loaded, Some(snapshot));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn activation_snapshot_rejects_hour_old_snapshot() {
        let dir = temp_dir("activation-stale");
        let snapshot_path = dir.join(SNAPSHOT_FILE_NAME);
        let snapshot = LiveSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            leader_pid: 42,
            leader_epoch: 7,
            written_at_ms: unix_now_ms().saturating_sub(SNAPSHOT_ACTIVATION_MAX_AGE_MS + 1),
            agents: Vec::new(),
            stats: AggregatedStats::default(),
        };
        let mut store = RuntimeStore {
            db: None,
            snapshot_path: snapshot_path.clone(),
            last_snapshot_meta: None,
            last_published: None,
        };

        write_snapshot(&snapshot_path, &snapshot).unwrap();

        assert!(store.load_snapshot_for_activation().is_none());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn session_scoped_path_is_stable_and_flat() {
        let base = Path::new("/tmp/agentmux-tests");

        let first = session_scoped_path(base, SNAPSHOT_FILE_NAME, "alpha");
        let second = session_scoped_path(base, SNAPSHOT_FILE_NAME, "beta");
        let escaped = session_scoped_path(base, SNAPSHOT_FILE_NAME, "dev/session:1");
        let db = session_scoped_path(base, RUNTIME_DB_FILE_NAME, "alpha");
        let underscore = session_scoped_path(base, SNAPSHOT_FILE_NAME, "dev_2fsession:1");

        assert_eq!(first, base.join("live_snapshot.alpha.json"));
        assert_eq!(second, base.join("live_snapshot.beta.json"));
        assert_eq!(escaped, base.join("live_snapshot.dev_2fsession_3a1.json"));
        assert_eq!(db, base.join("runtime.alpha.db"));
        assert_eq!(
            underscore,
            base.join("live_snapshot.dev_5f2fsession_3a1.json")
        );
        assert_ne!(first, second);
        assert_ne!(escaped, underscore);
    }
}
