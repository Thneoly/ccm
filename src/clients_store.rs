//! Persistent client sessions (`$CCM_HOME/clients.toml`, v0.5 M2 /
//! V0.5_PLAN §3.2).
//!
//! The gap it closes: scoped client entries lived only in memory, so a
//! proxy restart (upgrade, crash, reboot) silently dropped still-running
//! sessions onto the global target — requests kept succeeding, on the
//! wrong model. This store persists `id` / `target` / `last_seen_ms` per
//! entry and nothing else:
//!
//! - `requests` is deliberately NOT persisted — `usage.jsonl` is the
//!   durable ledger; the convenience counter resets to 0 on restart.
//! - The file is written ONLY by the proxy: at a scoped switch, and by a
//!   periodic refresh task that snapshots the map's CURRENT state
//!   (traffic-bumped `last_seen_ms` values are persisted UNCHANGED —
//!   never re-stamped with now(), which would make the TTL dead code).
//!   Both writers go through one write gate (`persist`) that snapshots
//!   inside the critical section, so neither can publish or overwrite
//!   the other's bytes with a stale snapshot.
//! - Load at startup revalidates every target against current config;
//!   unresolvable, expired (TTL), and malformed entries are dropped with
//!   one warning naming them. A corrupt file degrades to an empty map —
//!   byte-identical v0.4 behavior.
//! - Writes are tmp-file + rename. `std::fs::rename` replaces an existing
//!   destination on Windows (MoveFileEx MOVEFILE_REPLACE_EXISTING); the
//!   unit test pins that overwrite path because the repo's rotation code
//!   only ever renames to fresh names.
//! - No lock across processes: two proxies on one `CCM_HOME` are
//!   last-writer-wins here — the scenario is already degraded for
//!   history (`.lock`), and this is the documented boundary.
//! - Never `state.toml` (invariant 9), never credentials (pinned by
//!   test).

use std::{collections::HashMap, fs, path::Path, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::proxy::{valid_client_id, ClientEntry};

/// Refresh cadence of the periodic snapshot task — the metrics-snapshot
/// task's interval convention (config read once at startup, idle-skip,
/// throttled warn-on-failure, no exit-time write).
pub(crate) const REFRESH_INTERVAL_SECS: u64 = 30;

/// One persisted session row — a `[[clients]]` table in clients.toml.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ClientRecord {
    pub(crate) id: String,
    pub(crate) target: String,
    pub(crate) last_seen_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ClientsFile {
    #[serde(default)]
    clients: Vec<ClientRecord>,
}

/// What [`ClientsStore::load`] found, for `serve()` to report: the
/// restored map plus one human-readable note per drop reason.
pub(crate) struct LoadedSessions {
    pub(crate) clients: HashMap<String, ClientEntry>,
    pub(crate) notes: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ClientsStore {
    path: PathBuf,
    persist: bool,
    ttl_ms: u64,
    max_entries: u64,
    /// Write gate serializing every file write this process makes, and
    /// the last bytes it published. Two writers share one file (the
    /// scoped-switch save and the periodic refresh task); without a
    /// gate, a preempted refresh could publish a pre-switch snapshot
    /// over a just-acknowledged switch, and both could collide on the
    /// tmp name (verify pass, v0.5 M2). A tokio mutex because the live
    /// map is re-read — an await — INSIDE the critical section; an
    /// `Arc` so the `Clone` store shares one gate.
    writer: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl ClientsStore {
    pub(crate) fn from_config(config: &AppConfig) -> Result<Self> {
        let path = AppConfig::home_dir()?.join("clients.toml");
        Ok(Self::from_parts(
            path,
            config.clients.persist,
            config.clients.ttl_days,
            config.clients.max_entries,
        ))
    }

    fn from_parts(path: PathBuf, persist: bool, ttl_days: u64, max_entries: u64) -> Self {
        Self {
            path,
            persist,
            ttl_ms: ttl_days.saturating_mul(86_400_000),
            max_entries,
            writer: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Memory-only store (tests, `[clients] persist = false`): no load, no
    /// save — but the entry cap still applies, because bounded memory is a
    /// property of the map, not of persistence.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::from_parts(PathBuf::new(), false, 7, 256)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.persist
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Read and revalidate the persisted sessions. Never fails: every
    /// problem (unreadable, corrupt, invalid id, expired, unresolvable
    /// target) degrades to a dropped entry with a note — persistence must
    /// never prevent routing.
    pub(crate) fn load(&self, config: &AppConfig, now: u64) -> LoadedSessions {
        let mut notes = Vec::new();
        if !self.persist {
            return LoadedSessions {
                clients: HashMap::new(),
                notes,
            };
        }
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Fresh install — nothing persisted, nothing to say.
                return LoadedSessions {
                    clients: HashMap::new(),
                    notes,
                };
            }
            Err(err) => {
                notes.push(format!(
                    "cannot read {} — starting with no persisted client sessions ({err})",
                    self.path.display()
                ));
                return LoadedSessions {
                    clients: HashMap::new(),
                    notes,
                };
            }
        };
        let file: ClientsFile = match toml::from_str(&raw) {
            Ok(file) => file,
            Err(err) => {
                notes.push(format!(
                    "invalid clients.toml ({err}) — starting with no persisted client sessions; the file is left untouched"
                ));
                return LoadedSessions {
                    clients: HashMap::new(),
                    notes,
                };
            }
        };

        let mut clients = HashMap::new();
        let mut invalid_ids = Vec::new();
        let mut expired = Vec::new();
        let mut unresolvable = Vec::new();
        let cutoff = now.saturating_sub(self.ttl_ms);
        for record in file.clients {
            if !valid_client_id(&record.id) {
                invalid_ids.push(record.id);
                continue;
            }
            if record.last_seen_ms < cutoff {
                expired.push(record.id);
                continue;
            }
            if config.resolve_route(&record.target).is_err() {
                unresolvable.push(format!("{} -> `{}`", record.id, record.target));
                continue;
            }
            clients.insert(
                record.id,
                ClientEntry {
                    target: record.target,
                    // The requests counter is runtime-only by design.
                    requests: 0,
                    last_seen_ms: record.last_seen_ms,
                },
            );
        }
        if !invalid_ids.is_empty() {
            notes.push(format!(
                "dropped {} client session(s) with invalid ids: {}",
                invalid_ids.len(),
                invalid_ids.join(", ")
            ));
        }
        if !expired.is_empty() {
            notes.push(format!(
                "dropped {} expired client session(s): {}",
                expired.len(),
                expired.join(", ")
            ));
        }
        if !unresolvable.is_empty() {
            notes.push(format!(
                "dropped {} client session(s) with targets no longer in config: {}",
                unresolvable.len(),
                unresolvable.join(", ")
            ));
        }
        LoadedSessions { clients, notes }
    }

    /// Render the map as the clients.toml body: `[[clients]]` tables sorted
    /// by id, so the file is deterministic (byte-identical snapshots
    /// byte-identical files) and hand-editable in an obvious way.
    pub(crate) fn snapshot_toml(clients: &HashMap<String, ClientEntry>) -> String {
        let mut records: Vec<ClientRecord> = clients
            .iter()
            .map(|(id, entry)| ClientRecord {
                id: id.clone(),
                target: entry.target.clone(),
                last_seen_ms: entry.last_seen_ms,
            })
            .collect();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        let file = ClientsFile { clients: records };
        // Strings and u64s cannot fail to serialize; an empty file from a
        // silent error would be worse than a loud impossible panic.
        toml::to_string_pretty(&file).expect("client records serialize")
    }

    /// Snapshot the LIVE map and persist it — the single write entry
    /// point for both writers (the scoped-switch path and the periodic
    /// refresh task). The snapshot is taken inside the write gate, so a
    /// writer can never publish bytes older than what another writer
    /// already wrote; a body byte-identical to the last successful write
    /// is skipped (the file already holds it — this is the refresh
    /// task's idle-skip, now shared by both writers).
    pub(crate) async fn persist(
        &self,
        clients: &tokio::sync::RwLock<HashMap<String, ClientEntry>>,
    ) -> Result<()> {
        let mut last_written = self.writer.lock().await;
        let map = clients.read().await;
        let snapshot = Self::snapshot_toml(&map);
        if last_written.as_deref() == Some(snapshot.as_str()) {
            return Ok(());
        }
        drop(map);
        self.write(&snapshot)?;
        *last_written = Some(snapshot);
        Ok(())
    }

    /// Atomic write: tmp-file + rename over the existing destination. The
    /// tmp name is stable (`clients.toml.tmp`) so a crash between write and
    /// rename leaves at most one stale tmp file, overwritten by the next
    /// write and never read by `load`. Only ever called from [`persist`],
    /// under the write gate — concurrent writers within this process are
    /// serialized there. A SECOND PROCESS on the same CCM_HOME is still
    /// last-writer-wins (the documented boundary).
    fn write(&self, contents: &str) -> Result<()> {
        let tmp = self.path.with_extension("toml.tmp");
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
        }
        fs::write(&tmp, contents).with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("cannot move {} into place", tmp.display()))?;
        Ok(())
    }

    /// Evict least-recently-seen entries beyond `max_entries` (ties by id,
    /// deterministic). Applied at insert (a scoped switch) and at load —
    /// bounded memory is a property of the map, not of persistence.
    pub(crate) fn enforce_cap(&self, clients: &mut HashMap<String, ClientEntry>) {
        while clients.len() as u64 > self.max_entries {
            let Some(evict) = clients
                .iter()
                .min_by_key(|(id, entry)| (entry.last_seen_ms, (*id).clone()))
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            clients.remove(&evict);
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A config whose models/routes resolve `glm` and `claude`.
    fn config() -> AppConfig {
        AppConfig::starter()
    }

    fn entry(target: &str, last_seen_ms: u64) -> ClientEntry {
        ClientEntry {
            target: target.to_string(),
            requests: 7,
            last_seen_ms,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ccm-clients-store-{name}"))
    }

    // 1. snapshot roundtrip: sorted [[clients]] tables, requests stripped,
    //    and the exact on-disk shape pinned (hand-editable format)
    #[test]
    fn snapshot_is_sorted_and_drops_requests() {
        let mut clients = HashMap::new();
        clients.insert("b".to_string(), entry("glm", 20));
        clients.insert("a".to_string(), entry("claude", 10));
        let text = ClientsStore::snapshot_toml(&clients);
        assert_eq!(
            text,
            "[[clients]]\nid = \"a\"\ntarget = \"claude\"\nlast_seen_ms = 10\n\n\
             [[clients]]\nid = \"b\"\ntarget = \"glm\"\nlast_seen_ms = 20\n"
        );
        // roundtrip through load (fresh file, everything valid)
        let store = ClientsStore::from_parts(scratch("roundtrip"), true, 7, 256);
        std::fs::write(store.path(), &text).unwrap();
        let loaded = store.load(&config(), 30);
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        assert_eq!(loaded.clients.len(), 2);
        let a = &loaded.clients["a"];
        assert_eq!(a.target, "claude");
        assert_eq!(a.last_seen_ms, 10);
        assert_eq!(a.requests, 0, "requests is runtime-only");
        let _ = std::fs::remove_file(store.path());
    }

    // 2. the Windows rename-over-existing path: a second persist replaces
    //    the first file's content (the repo's rotation code only ever
    //    renames to fresh names — the overwrite path is otherwise
    //    unexercised)
    #[tokio::test]
    async fn persist_replaces_an_existing_file_via_rename() {
        let path = scratch("rename-overwrite");
        let _ = std::fs::remove_file(&path);
        let store = ClientsStore::from_parts(path.clone(), true, 7, 256);

        let clients = tokio::sync::RwLock::new(HashMap::new());
        clients
            .write()
            .await
            .insert("a".to_string(), entry("glm", 10));
        store.persist(&clients).await.unwrap();
        let first = clients.read().await;
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            ClientsStore::snapshot_toml(&first)
        );
        drop(first);

        let mut second = clients.write().await;
        second.insert("a".to_string(), entry("claude", 99));
        second.insert("b".to_string(), entry("glm", 55));
        drop(second);
        store.persist(&clients).await.unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let second_map = clients.read().await;
        assert_eq!(on_disk, ClientsStore::snapshot_toml(&second_map));
        assert!(on_disk.contains("last_seen_ms = 99"));
        assert!(!std::path::Path::new(&path.with_extension("toml.tmp")).exists());
        let _ = std::fs::remove_file(&path);
    }

    // 3. load drops with notes: invalid id charset, TTL-expired, and
    //    target-no-longer-in-config each get one warning naming them;
    //    valid entries survive
    #[test]
    fn load_drops_invalid_expired_and_unresolvable_with_notes() {
        let path = scratch("drops");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            concat!(
                "[[clients]]\nid = \"good\"\ntarget = \"glm\"\nlast_seen_ms = 110\n\n",
                "[[clients]]\nid = \"bad id\"\ntarget = \"glm\"\nlast_seen_ms = 110\n\n",
                "[[clients]]\nid = \"stale\"\ntarget = \"glm\"\nlast_seen_ms = 99\n\n",
                "[[clients]]\nid = \"ghost\"\ntarget = \"removed-model\"\nlast_seen_ms = 110\n",
            ),
        )
        .unwrap();
        let store = ClientsStore::from_parts(path.clone(), true, 7, 256);
        // ttl 7 days = 604_800_000 ms; now = 604_800_100 → cutoff 100
        let loaded = store.load(&config(), 604_800_100);
        assert!(loaded.clients.contains_key("good"));
        assert_eq!(loaded.clients.len(), 1);
        let notes = loaded.notes.join("\n");
        assert!(notes.contains("invalid ids: bad id"), "{notes}");
        assert!(
            notes.contains("expired client session(s): stale"),
            "{notes}"
        );
        assert!(
            notes.contains("targets no longer in config: ghost -> `removed-model`"),
            "{notes}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // 4. corrupt file and missing file: empty map, never an error — the
    //    v0.4 history-downgrade shape
    #[test]
    fn corrupt_file_degrades_to_empty_map() {
        let path = scratch("corrupt");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "not [ valid toml").unwrap();
        let store = ClientsStore::from_parts(path.clone(), true, 7, 256);
        let loaded = store.load(&config(), 1000);
        assert!(loaded.clients.is_empty());
        assert_eq!(loaded.notes.len(), 1);
        assert!(loaded.notes[0].contains("invalid clients.toml"));
        // the corrupt file is left untouched for manual recovery
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("valid toml"));
        let _ = std::fs::remove_file(&path);

        let missing = ClientsStore::from_parts(scratch("missing"), true, 7, 256);
        let loaded = missing.load(&config(), 1000);
        assert!(loaded.clients.is_empty());
        assert!(loaded.notes.is_empty(), "fresh install is silent");
    }

    // 5. disabled store: no load, no save — but the cap still applies
    #[test]
    fn disabled_store_is_memory_only_but_still_caps() {
        let store = ClientsStore::disabled();
        assert!(!store.is_enabled());
        std::fs::create_dir_all(scratch("disabled")).unwrap();
        let mut clients = HashMap::new();
        clients.insert("a".to_string(), entry("glm", 1));
        // save on a disabled store must not write anything (empty path is
        // also invalid) — the call sites skip it; pin the skip contract by
        // the callers' tests. Here: only the cap matters.
        store.enforce_cap(&mut clients);
        assert_eq!(clients.len(), 1);
        let _ = std::fs::remove_dir_all(scratch("disabled"));
    }

    // 6. the cap evicts least-recently-seen first, ties by id
    #[test]
    fn cap_evicts_oldest_last_seen_then_lexicographic() {
        let store = ClientsStore::from_parts(scratch("cap"), true, 7, 2);
        let mut clients = HashMap::new();
        clients.insert("old".to_string(), entry("glm", 10));
        clients.insert("new".to_string(), entry("glm", 300));
        clients.insert("mid".to_string(), entry("glm", 100));
        store.enforce_cap(&mut clients);
        assert!(!clients.contains_key("old"), "oldest evicted");
        assert_eq!(clients.len(), 2);

        // tie on last_seen: lexicographically smallest id goes
        let mut tied = HashMap::new();
        tied.insert("b".to_string(), entry("glm", 50));
        tied.insert("a".to_string(), entry("glm", 50));
        tied.insert("c".to_string(), entry("glm", 50));
        store.enforce_cap(&mut tied);
        assert!(!tied.contains_key("a"));
        assert_eq!(tied.len(), 2);
    }

    // 7. the write gate (verify pass, v0.5 M2): concurrent writers — the
    //    scoped-switch shape (mutate, then persist) and refresh-shaped
    //    persists racing on one map — must never lose an entry or leave
    //    the file unreadable. Pre-gate, a preempted writer could publish
    //    a stale snapshot over a newer one, or two writers could collide
    //    on the shared tmp name.
    #[tokio::test]
    async fn concurrent_persists_never_lose_an_entry() {
        let path = scratch("concurrent");
        let _ = std::fs::remove_file(&path);
        let store = ClientsStore::from_parts(path.clone(), true, 7, 256);
        let clients = std::sync::Arc::new(tokio::sync::RwLock::new(
            HashMap::<String, ClientEntry>::new(),
        ));

        // 8 switch-shaped writers (mutate under the write lock, persist
        // outside it) racing with 4 refresh-shaped persists.
        let mut tasks = Vec::new();
        for index in 0..8u64 {
            let store = store.clone();
            let clients = std::sync::Arc::clone(&clients);
            tasks.push(tokio::spawn(async move {
                let id = format!("c{index}");
                {
                    let mut map = clients.write().await;
                    map.insert(id.clone(), entry("glm", 100 + index));
                }
                store.persist(&clients).await.unwrap();
            }));
        }
        for _ in 0..4 {
            let store = store.clone();
            let clients = std::sync::Arc::clone(&clients);
            tasks.push(tokio::spawn(async move {
                store.persist(&clients).await.unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        let loaded = store.load(&config(), 1_000);
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        for index in 0..8u64 {
            assert!(
                loaded.clients.contains_key(&format!("c{index}")),
                "entry c{index} lost; file: {}",
                std::fs::read_to_string(&path).unwrap_or_default()
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}
