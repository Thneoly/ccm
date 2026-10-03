//! Observability history store (v0.4 M5): append-only JSONL persistence for
//! routing decisions, metric snapshots, and circuit transitions under
//! `$CCM_HOME/history/`.
//!
//! Layout: `<kind>.jsonl` is the active append target; when it reaches its
//! record or byte cap the writer closes it, renames it to
//! `<kind>-<unix_ms>.jsonl`, and reopens a fresh active file. Rotated files
//! older than the retention window are deleted by mtime. A torn tail line
//! (crash mid-write) is skipped by every reader.
//!
//! Write path: producers `try_send` pre-serialized lines into a bounded
//! channel; a dedicated writer thread batches, appends, and flushes. A full
//! queue drops the record and counts it — recording history must never block
//! or slow `/v1/messages`. Implementation note (deviation from the V0.4 plan
//! §3.3 sketch, which proposed tokio mpsc + a writer task): this uses
//! `std::sync::mpsc::SyncSender` + a plain std thread so the writer performs
//! zero async-runtime IO, `recv_timeout` gives the loop a poll point, and
//! dropping the last handle drains, flushes, and joins the writer (see
//! `Drop for HistoryInner`) — that join is the deterministic flush point for
//! tests. Same bounded-1024, always-try_send semantics.
//!
//! Single-writer rule: `open_history` takes an OS file lock on
//! `history/.lock` (held for the process lifetime, released by the kernel on
//! death — no stale-lock recovery needed). A second proxy on the same
//! `CCM_HOME` fails to open history and must run with history disabled
//! rather than interleaving writes or duplicating decision ids.
//!
//! Honest boundaries: no exit-time final snapshot is claimed for a killed
//! process (Ctrl+C skips destructors; the periodic snapshot task plus
//! torn-tail tolerance cover the gap) — a normal exit that drops the store
//! does drain + flush + join, so nothing in flight is lost there. Runtime
//! metrics always restart from zero — stale snapshots would distort
//! healthiest/weighted ordering, so history queries read disk instead.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::routing::{decision::RoutingDecision, metrics::ModelMetrics};

/// Bounded-channel capacity between producers and the writer thread. A full
/// queue drops (and counts) rather than blocking the request path.
pub(crate) const CHANNEL_CAPACITY: usize = 1024;

/// How long the writer thread parks between channel polls. Also the upper
/// bound on how long dropping the last handle may wait for the park to
/// notice the stop flag.
const WRITER_POLL: Duration = Duration::from_millis(500);

/// Tail window read when recovering the decision id sequence: enough to hold
/// many maximum-size decisions while staying cheap at startup.
const SEQ_TAIL_BYTES: u64 = 64 * 1024;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ===========================================================================
// Records
// ===========================================================================

/// Periodic whole-state snapshot of the per-model metrics map. Raw counters
/// only; derived rates are computed on read so a schema fix applies to old
/// snapshots too.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct MetricsSnapshot {
    pub(crate) timestamp_ms: u64,
    pub(crate) models: BTreeMap<String, ModelMetrics>,
}

/// One circuit-breaker state transition (OPEN / HALF_OPEN / CLOSED entries,
/// with a human-readable reason).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct CircuitTransition {
    pub(crate) timestamp_ms: u64,
    pub(crate) model: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) reason: String,
}

// ===========================================================================
// Engine limits
// ===========================================================================

/// Rotation and retention thresholds. Deliberately decoupled from
/// `[observability]` config so the engine stays config-source agnostic.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HistoryLimits {
    pub(crate) max_records_per_file: u64,
    pub(crate) max_bytes_per_file: u64,
    pub(crate) retention_ms: u64,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_records_per_file: 50_000,
            max_bytes_per_file: 8 * 1024 * 1024,
            retention_ms: 14 * 24 * 60 * 60 * 1000,
        }
    }
}

impl From<&crate::config::ObservabilityConfig> for HistoryLimits {
    fn from(config: &crate::config::ObservabilityConfig) -> Self {
        Self {
            max_records_per_file: config.max_records_per_file,
            max_bytes_per_file: config.max_bytes_per_file,
            retention_ms: config.retention_days * 24 * 60 * 60 * 1000,
        }
    }
}

// ===========================================================================
// Streams and events
// ===========================================================================

/// The three persisted JSONL kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryStream {
    Decisions,
    Metrics,
    Circuit,
}

impl HistoryStream {
    fn file_stem(self) -> &'static str {
        match self {
            HistoryStream::Decisions => "decisions",
            HistoryStream::Metrics => "metrics",
            HistoryStream::Circuit => "circuit",
        }
    }
}

/// One pre-serialized record on its way to the writer thread. Producers
/// serialize (instead of cloning whole structs) so the channel stays small.
pub(crate) enum HistoryEvent {
    Decision(String),
    Metrics(String),
    Circuit(String),
}

impl HistoryEvent {
    fn stream(&self) -> HistoryStream {
        match self {
            HistoryEvent::Decision(_) => HistoryStream::Decisions,
            HistoryEvent::Metrics(_) => HistoryStream::Metrics,
            HistoryEvent::Circuit(_) => HistoryStream::Circuit,
        }
    }

    fn line(&self) -> &str {
        match self {
            HistoryEvent::Decision(line)
            | HistoryEvent::Metrics(line)
            | HistoryEvent::Circuit(line) => line,
        }
    }
}

// ===========================================================================
// Handle
// ===========================================================================

struct HistoryInner {
    dir: PathBuf,
    sender: SyncSender<HistoryEvent>,
    /// Records dropped because the channel was full or the writer had
    /// already stopped. Surfaced by [`HistoryInner::dropped_records`] when
    /// the last handle drops.
    dropped: AtomicU64,
    stop: Arc<AtomicBool>,
    /// Joined exactly once, by [`HistoryInner::stop_and_join`].
    worker: Mutex<Option<JoinHandle<()>>>,
    /// OS file lock proving single-writer ownership; held until drop.
    _lock: File,
}

/// Cloneable handle to the history writer. `None` inner = history disabled
/// (config off, lock lost, or `History::disabled()` in tests) — every
/// `record_*` becomes a no-op, so call sites never branch on it.
#[derive(Clone, Default)]
pub(crate) struct History {
    inner: Option<Arc<HistoryInner>>,
}

impl History {
    /// A disabled handle: records go nowhere. Used by tests and when the
    /// single-writer lock cannot be acquired.
    pub(crate) fn disabled() -> Self {
        Self::default()
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Directory backing this store, for the disk-backed control-API reads.
    pub(crate) fn dir(&self) -> Option<&Path> {
        self.inner.as_ref().map(|inner| inner.dir.as_path())
    }

    /// Enqueue one record. Never blocks: a full channel (or a stopped
    /// writer) drops the record, counts it, and warns once.
    pub(crate) fn record(&self, event: HistoryEvent) {
        let Some(inner) = &self.inner else { return };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            inner.sender.try_send(event)
        {
            let dropped = inner.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 {
                eprintln!(
                    "ccm: history: writer queue full or stopped; dropping history records (total dropped so far: {dropped})"
                );
            }
        }
    }

    pub(crate) fn record_decision(&self, decision: &RoutingDecision) {
        match serde_json::to_string(decision) {
            Ok(line) => self.record(HistoryEvent::Decision(line)),
            Err(err) => {
                eprintln!("ccm: history: failed to serialize decision {err}");
            }
        }
    }

    pub(crate) fn record_metrics_snapshot(&self, snapshot: &MetricsSnapshot) {
        match serde_json::to_string(snapshot) {
            Ok(line) => self.record(HistoryEvent::Metrics(line)),
            Err(err) => {
                eprintln!("ccm: history: failed to serialize metrics snapshot: {err}");
            }
        }
    }

    pub(crate) fn record_circuit_transition(&self, transition: &CircuitTransition) {
        match serde_json::to_string(transition) {
            Ok(line) => self.record(HistoryEvent::Circuit(line)),
            Err(err) => {
                eprintln!("ccm: history: failed to serialize circuit transition: {err}");
            }
        }
    }
}

impl HistoryInner {
    /// Total records dropped over this store's lifetime (full queue or a
    /// writer that had already stopped).
    fn dropped_records(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Signal the writer to stop after draining the queue, then join it
    /// exactly once. Every teardown path funnels through the last handle
    /// going away.
    fn stop_and_join(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = worker {
            let _ = handle.join();
        }
    }
}

impl Drop for HistoryInner {
    /// Teardown on the last handle drop: drain + flush + join the writer, so
    /// a normal `main` return persists everything still in flight, then
    /// surface the lifetime drop count if any records were lost. A killed
    /// process (Ctrl+C) skips destructors entirely — the periodic flushes
    /// plus torn-tail tolerance cover that path, and no exit-time snapshot
    /// is claimed for it.
    fn drop(&mut self) {
        self.stop_and_join();
        let dropped = self.dropped_records();
        if dropped > 0 {
            eprintln!(
                "ccm: history: {dropped} records were dropped (writer queue full or writer stopped)"
            );
        }
    }
}

/// Open (or create) the history store at `dir`, enforcing the single-writer
/// rule via an OS lock on `history/.lock`.
///
/// Errors when the directory cannot be created, the lock file cannot be
/// opened, or another process holds the lock — the caller decides whether to
/// disable history (the proxy must) or abort.
pub(crate) fn open_history(
    dir: PathBuf,
    limits: HistoryLimits,
    capacity: usize,
) -> Result<History> {
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating history directory {}", dir.display()))?;

    let lock = acquire_writer_lock(&dir)?;

    // Retention first (before any new rotation): a restart is also a natural
    // moment to forget files the previous run left behind.
    sweep_retention(&dir, Duration::from_millis(limits.retention_ms));

    let (sender, receiver) = std::sync::mpsc::sync_channel::<HistoryEvent>(capacity);
    let inner = Arc::new(HistoryInner {
        dir: dir.clone(),
        sender,
        dropped: AtomicU64::new(0),
        stop: Arc::new(AtomicBool::new(false)),
        worker: Mutex::new(None),
        _lock: lock,
    });
    let worker = spawn_writer(dir, limits, receiver, Arc::clone(&inner.stop));
    *inner.worker.lock().unwrap() = Some(worker);

    Ok(History { inner: Some(inner) })
}

/// Take the single-writer lock: an OS lock on `history/.lock`, held for the
/// process lifetime. The kernel releases it if the process dies, so there is
/// no stale-lock file problem. The file carries the pid for humans.
fn acquire_writer_lock(dir: &Path) -> Result<File> {
    let path = dir.join(".lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening history lock {}", path.display()))?;
    file.try_lock().with_context(|| {
        format!(
            "another ccm proxy already owns the history directory {} \
             (single-writer rule: one proxy per CCM_HOME may write history)",
            dir.display()
        )
    })?;
    let _ = file.set_len(0);
    let mut file = file;
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(format!("{}\n", std::process::id()).as_bytes());
    Ok(file)
}

// ===========================================================================
// Writer thread
// ===========================================================================

fn spawn_writer(
    dir: PathBuf,
    limits: HistoryLimits,
    receiver: Receiver<HistoryEvent>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut files = StreamFiles::new(dir, limits);
        loop {
            match receiver.recv_timeout(WRITER_POLL) {
                Ok(event) => {
                    if let Err(err) = files.write(event) {
                        // Unrecoverable IO error (disk full, dir removed, …):
                        // say it once and stop writing. Producers keep
                        // try_send-ing into a filling queue; the drop counter
                        // makes the loss visible.
                        eprintln!(
                            "ccm: history: write failed ({err}); stopping the history writer"
                        );
                        return;
                    }
                    for _ in 0..256 {
                        match receiver.try_recv() {
                            Ok(event) => {
                                if let Err(err) = files.write(event) {
                                    eprintln!("ccm: history: write failed ({err}); stopping the history writer");
                                    let _ = files.flush();
                                    return;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if let Err(err) = files.flush() {
                        eprintln!(
                            "ccm: history: write failed ({err}); stopping the history writer"
                        );
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = files.flush();
                    return;
                }
            }
            if stop.load(Ordering::Relaxed) {
                // Drain anything enqueued just before the stop flag, then go.
                while let Ok(event) = receiver.try_recv() {
                    if files.write(event).is_err() {
                        break;
                    }
                }
                let _ = files.flush();
                return;
            }
        }
    })
}

/// Per-stream append state owned by the writer thread.
struct StreamFiles {
    dir: PathBuf,
    limits: HistoryLimits,
    writers: [Option<BufWriter<File>>; 3],
    records: [u64; 3],
    bytes: [u64; 3],
}

impl StreamFiles {
    fn new(dir: PathBuf, limits: HistoryLimits) -> Self {
        Self {
            dir,
            limits,
            writers: [None, None, None],
            records: [0; 3],
            bytes: [0; 3],
        }
    }

    fn slot(stream: HistoryStream) -> usize {
        match stream {
            HistoryStream::Decisions => 0,
            HistoryStream::Metrics => 1,
            HistoryStream::Circuit => 2,
        }
    }

    /// Append one line, rotating first when the active file is at capacity
    /// (so a file never exceeds either cap by more than nothing — the check
    /// runs before the write).
    fn write(&mut self, event: HistoryEvent) -> std::io::Result<()> {
        let stream = event.stream();
        let slot = Self::slot(stream);
        let line = event.line();

        if self.writers[slot].is_none() {
            self.open_active(stream, slot)?;
        }
        if self.records[slot] >= self.limits.max_records_per_file
            || self.bytes[slot] + line.len() as u64 + 1 > self.limits.max_bytes_per_file
        {
            self.rotate(stream, slot)?;
        }

        let writer = self.writers[slot].as_mut().expect("active writer opened");
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        self.records[slot] += 1;
        self.bytes[slot] += line.len() as u64 + 1;
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        for writer in self.writers.iter_mut().flatten() {
            writer.flush()?;
        }
        Ok(())
    }

    fn open_active(&mut self, stream: HistoryStream, slot: usize) -> std::io::Result<()> {
        // If an active file already exists, continue counting from its
        // actual size (a restarted proxy appends to the previous tail).
        let path = self.dir.join(format!("{}.jsonl", stream.file_stem()));
        let already = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.writers[slot] = Some(BufWriter::new(file));
        self.bytes[slot] = already;
        self.records[slot] = 0;
        Ok(())
    }

    /// Close → rename → reopen. Dropping the `BufWriter<File>` closes the
    /// handle BEFORE the rename, which Windows requires.
    fn rotate(&mut self, stream: HistoryStream, slot: usize) -> std::io::Result<()> {
        if let Some(mut writer) = self.writers[slot].take() {
            writer.flush()?;
        }
        let stem = stream.file_stem();
        let active = self.dir.join(format!("{stem}.jsonl"));
        if active.exists() {
            let rotated = self.rotated_path(stem);
            fs::rename(&active, &rotated)?;
            sweep_retention(&self.dir, Duration::from_millis(self.limits.retention_ms));
        }
        self.open_active(stream, slot)
    }

    /// `<stem>-<unix_ms>.jsonl`, with a numeric suffix on same-millisecond
    /// collisions (small test caps can rotate twice inside one ms).
    fn rotated_path(&self, stem: &str) -> PathBuf {
        let base = format!("{stem}-{}.jsonl", now_ms());
        let mut candidate = self.dir.join(&base);
        let mut n = 1u32;
        while candidate.exists() {
            candidate = self.dir.join(format!("{stem}-{}-{n}.jsonl", now_ms()));
            n += 1;
        }
        candidate
    }
}

/// Delete rotated history files whose mtime predates the retention cutoff.
/// Only `<kind>-<digits>.jsonl` names are considered — the active files and
/// `.lock` are never touched. Best effort: unreadable entries are skipped.
fn sweep_retention(dir: &Path, retention: Duration) {
    let Some(cutoff) = SystemTime::now().checked_sub(retention) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_rotated_name(&name) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified < cutoff {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `true` for rotated names (`decisions-1760000000000.jsonl`,
/// `metrics-<ms>-<n>.jsonl`), `false` for active files and `.lock`.
fn is_rotated_name(name: &str) -> bool {
    let Some((_, ts)) = name.rsplit_once('-') else {
        return false;
    };
    let Some(digits) = ts.strip_suffix(".jsonl") else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

// ===========================================================================
// Readers (offline: no lock, no writer needed)
// ===========================================================================

/// Time / model / client / limit filters for disk-backed decision queries.
/// `since`/`until` are inclusive unix-ms bounds on `timestamp_ms`; `model`
/// matches any attempted model or the selected one; `limit` keeps the MOST
/// RECENT N records.
#[derive(Clone, Debug, Default)]
pub(crate) struct DecisionQuery {
    pub(crate) since: Option<u64>,
    pub(crate) until: Option<u64>,
    pub(crate) model: Option<String>,
    pub(crate) client: Option<String>,
    pub(crate) limit: Option<usize>,
}

/// All files of one stream kind, oldest first: rotated files by their
/// timestamp suffix, then the active `<kind>.jsonl` (it holds the newest
/// records). Malformed suffixes sort first (safest for time-ordered reads:
/// they are still complete lines).
pub(crate) fn kind_files(dir: &Path, stream: HistoryStream) -> Vec<PathBuf> {
    let prefix = format!("{}-", stream.file_stem());
    let active_name = format!("{}.jsonl", stream.file_stem());
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name == active_name {
            files.push((u64::MAX, entry.path()));
        } else if let Some(suffix) = name.strip_prefix(&prefix) {
            let key = suffix
                .strip_suffix(".jsonl")
                .and_then(|digits| digits.parse::<u64>().ok())
                .unwrap_or(0);
            files.push((key, entry.path()));
        }
    }
    files.sort_by_key(|(key, _)| *key);
    files.into_iter().map(|(_, path)| path).collect()
}

/// Iterate complete lines of a JSONL file. A torn final line (crash
/// mid-write) and non-JSON lines are skipped by the callers' parse — this
/// helper only guarantees line iteration over lossy UTF-8.
fn jsonl_lines(path: &Path) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn parse_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    jsonl_lines(path)
        .into_iter()
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// Read persisted decisions matching `query`, oldest first (the most recent
/// N when `limit` is set). Torn or foreign lines are skipped.
pub(crate) fn read_decisions(dir: &Path, query: &DecisionQuery) -> Result<Vec<RoutingDecision>> {
    let mut records: Vec<RoutingDecision> = Vec::new();
    for path in kind_files(dir, HistoryStream::Decisions) {
        records.extend(parse_lines::<RoutingDecision>(&path));
    }
    records.retain(|record| matches_query(record, query));
    if let Some(limit) = query.limit {
        if records.len() > limit {
            records.drain(..records.len() - limit);
        }
    }
    Ok(records)
}

fn matches_query(record: &RoutingDecision, query: &DecisionQuery) -> bool {
    if let Some(since) = query.since {
        if record.timestamp_ms < since {
            return false;
        }
    }
    if let Some(until) = query.until {
        if record.timestamp_ms > until {
            return false;
        }
    }
    if let Some(model) = &query.model {
        let touched = record.selected.as_deref() == Some(model.as_str())
            || record.attempts.iter().any(|a| a.model == *model);
        if !touched {
            return false;
        }
    }
    if let Some(client) = &query.client {
        if record.client.as_deref() != Some(client.as_str()) {
            return false;
        }
    }
    true
}

/// Read metric snapshots oldest-first; `limit` keeps the most recent N.
pub(crate) fn read_metrics_snapshots(dir: &Path, limit: Option<usize>) -> Vec<MetricsSnapshot> {
    let mut snapshots: Vec<MetricsSnapshot> = Vec::new();
    for path in kind_files(dir, HistoryStream::Metrics) {
        snapshots.extend(parse_lines::<MetricsSnapshot>(&path));
    }
    if let Some(limit) = limit {
        if snapshots.len() > limit {
            snapshots.drain(..snapshots.len() - limit);
        }
    }
    snapshots
}

/// Read circuit transitions oldest-first, optionally filtered by model;
/// `limit` keeps the most recent N.
pub(crate) fn read_circuit_transitions(
    dir: &Path,
    model: Option<&str>,
    limit: Option<usize>,
) -> Vec<CircuitTransition> {
    let mut transitions: Vec<CircuitTransition> = Vec::new();
    for path in kind_files(dir, HistoryStream::Circuit) {
        transitions.extend(parse_lines::<CircuitTransition>(&path));
    }
    if let Some(model) = model {
        transitions.retain(|t| t.model == model);
    }
    if let Some(limit) = limit {
        if transitions.len() > limit {
            transitions.drain(..transitions.len() - limit);
        }
    }
    transitions
}

/// Next decision id for a fresh proxy run: `max(id)` across persisted
/// decision tails, plus one; 1 on an empty store. Ids therefore stay unique
/// across restarts that share a history directory.
///
/// Parses each line as raw JSON and reads only `id` (not as a
/// `RoutingDecision`), so future additive schema changes do not silently
/// reset the sequence.
pub(crate) fn recover_decision_seq(dir: &Path) -> u64 {
    let mut max: u64 = 0;
    for path in kind_files(dir, HistoryStream::Decisions) {
        for line in tail_lines(&path, SEQ_TAIL_BYTES) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                    max = max.max(id);
                }
            }
        }
    }
    max.saturating_add(1)
}

/// Last `cap` bytes of `path` as complete-ish lines. The first line may be a
/// torn fragment from the seek window; it fails the callers' parse and is
/// skipped, which is exactly the torn-tail policy.
fn tail_lines(path: &Path, cap: u64) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(cap);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::decision::{DecisionAttempt, DecisionCandidate};
    use serde_json::json;

    /// Temporary scratch directory, removed on drop. Explicit `dir` paths —
    /// these tests never touch `CCM_HOME`, so they need no env lock.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ccm-history-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn decision(
        id: u64,
        timestamp_ms: u64,
        models: &[&str],
        selected: Option<&str>,
    ) -> RoutingDecision {
        RoutingDecision {
            id,
            timestamp_ms,
            target: "route".to_string(),
            client: None,
            selection: "ordered".to_string(),
            configured_candidates: models.iter().map(|s| s.to_string()).collect(),
            ranked_candidates: vec![DecisionCandidate {
                rank: 1,
                model: models.first().copied().unwrap_or("m").to_string(),
                reliability_score: 1.0,
                latency_score: 1.0,
                cost_score: 1.0,
                quality_score: 1.0,
                weighted_score: 1.0,
            }],
            attempts: models
                .iter()
                .enumerate()
                .map(|(index, model)| DecisionAttempt {
                    attempt: index + 1,
                    model: model.to_string(),
                    circuit: "CLOSED".to_string(),
                    result: "HTTP 200".to_string(),
                    fallback: index > 0,
                })
                .collect(),
            selected: selected.map(str::to_string),
            outcome: "HTTP 200".to_string(),
        }
    }

    fn store_with(tag: &str, limits: HistoryLimits) -> (TempDir, History) {
        let dir = TempDir::new(tag);
        let history = open_history(dir.path().to_path_buf(), limits, CHANNEL_CAPACITY).unwrap();
        (dir, history)
    }

    // 1. rotation splits at the record cap: 7 writes with cap 3 → two rotated
    //    files of 3 + an active file of 1; every record readable
    #[test]
    fn rotation_splits_files_at_record_limit() {
        let limits = HistoryLimits {
            max_records_per_file: 3,
            ..HistoryLimits::default()
        };
        let (dir, history) = store_with("rotate", limits);
        for id in 1..=7u64 {
            history.record_decision(&decision(id, id * 1000, &["glm"], Some("glm")));
        }
        drop(history);

        let rotated: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_rotated_name(&p.file_name().unwrap_or_default().to_string_lossy()))
            .collect();
        assert_eq!(rotated.len(), 2, "expected exactly two rotated files");

        let records = read_decisions(dir.path(), &DecisionQuery::default()).unwrap();
        let ids: Vec<u64> = records.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7], "order preserved, none lost");

        let active = dir.path().join("decisions.jsonl");
        assert_eq!(jsonl_lines(&active).len(), 1, "active holds the tail");
    }

    // 2. rotation also triggers on the byte cap before exceeding it
    #[test]
    fn rotation_splits_files_at_byte_limit() {
        let limits = HistoryLimits {
            max_bytes_per_file: 600,
            ..HistoryLimits::default()
        };
        let (dir, history) = store_with("rotate-bytes", limits);
        // each serialized decision is ~500 bytes; three of them exceed 600
        for id in 1..=3u64 {
            history.record_decision(&decision(id, id * 1000, &["glm", "claude"], Some("claude")));
        }
        drop(history);
        let rotated = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| is_rotated_name(&e.file_name().to_string_lossy()))
            .count();
        assert!(rotated >= 1, "byte cap rotated at least one file");
        for path in kind_files(dir.path(), HistoryStream::Decisions) {
            assert!(
                fs::metadata(&path).unwrap().len() <= 600,
                "{} within the byte cap",
                path.display()
            );
        }
    }

    // 3. retention deletes rotated files older than the window (by mtime)
    //    and never touches the active file or .lock
    #[test]
    fn retention_sweep_deletes_only_old_rotated_files() {
        let (dir, history) = store_with("retention", HistoryLimits::default());
        history.record_decision(&decision(1, 1, &["glm"], Some("glm")));
        drop(history);

        // hand-write one rotated file aged 20 days past the 14-day window
        let old = dir.path().join("decisions-1000000000000.jsonl");
        fs::write(&old, "{}\n").unwrap();
        let ancient = SystemTime::now() - Duration::from_millis(20 * 24 * 60 * 60 * 1000);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(ancient)
            .unwrap();
        // and one rotated file inside the window
        let fresh = dir.path().join("decisions-2000000000000.jsonl");
        fs::write(&fresh, "{}\n").unwrap();

        sweep_retention(dir.path(), Duration::from_millis(14 * 24 * 60 * 60 * 1000));

        assert!(!old.exists(), "expired rotated file deleted");
        assert!(fresh.exists(), "in-window rotated file kept");
        assert!(dir.path().join("decisions.jsonl").exists(), "active kept");
        assert!(dir.path().join(".lock").exists(), "lock kept");
    }

    // 4. torn tail lines (crash mid-write) are skipped by readers
    #[test]
    fn torn_tail_lines_are_skipped_by_readers() {
        let (dir, history) = store_with("torn", HistoryLimits::default());
        history.record_decision(&decision(1, 100, &["glm"], Some("glm")));
        drop(history);

        let active = dir.path().join("decisions.jsonl");
        let mut contents = fs::read_to_string(&active).unwrap();
        contents.push_str("{\"id\": 9, \"timestamp_ms\":"); // torn, no newline
        fs::write(&active, contents).unwrap();

        let records = read_decisions(dir.path(), &DecisionQuery::default()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, 1);
    }

    // 5. decision ids recover from the persisted tail: max + 1; empty store → 1
    #[test]
    fn decision_seq_recovers_from_persisted_tail() {
        let (dir, history) = store_with("seq", HistoryLimits::default());
        for id in 5..=7u64 {
            history.record_decision(&decision(id, id * 1000, &["glm"], Some("glm")));
        }
        drop(history);
        assert_eq!(recover_decision_seq(dir.path()), 8);

        let empty = TempDir::new("seq-empty");
        assert_eq!(recover_decision_seq(empty.path()), 1);
    }

    // 6. a full channel drops and counts instead of blocking (a blocking
    //    send would hang this test)
    #[test]
    fn full_channel_drops_and_counts_without_blocking() {
        let dir = TempDir::new("full");
        // capacity 1, filled immediately, no writer draining it
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<HistoryEvent>(1);
        sender
            .try_send(HistoryEvent::Decision("{}".into()))
            .unwrap();
        let history = History {
            inner: Some(Arc::new(HistoryInner {
                dir: dir.path().to_path_buf(),
                sender,
                dropped: AtomicU64::new(0),
                stop: Arc::new(AtomicBool::new(true)),
                worker: Mutex::new(None),
                _lock: File::create(dir.path().join(".lock")).unwrap(),
            })),
        };

        for _ in 0..100 {
            history.record(HistoryEvent::Decision("x".into()));
        }
        assert_eq!(history.inner.as_ref().unwrap().dropped_records(), 100);
        assert!(
            matches!(_receiver.try_recv(), Ok(HistoryEvent::Decision(ref line)) if line == "{}"),
            "the queued record was not disturbed"
        );
    }

    // 7. the single-writer lock refuses a second instance and frees on drop
    #[test]
    fn single_writer_lock_refuses_second_instance() {
        let (dir, first) = store_with("lock", HistoryLimits::default());
        let second = open_history(dir.path().to_path_buf(), HistoryLimits::default(), 4);
        let err = second.err().unwrap().to_string();
        assert!(
            err.contains("already owns the history directory"),
            "error explains the single-writer rule: {err}"
        );
        drop(first); // drains, joins the writer, releases the lock
                     // the lock died with the handle: a fresh open succeeds
        open_history(dir.path().to_path_buf(), HistoryLimits::default(), 4).unwrap();
    }

    // 8. metrics snapshots and circuit transitions round-trip through JSONL
    #[test]
    fn metrics_and_circuit_roundtrip() {
        let (dir, history) = store_with("roundtrip", HistoryLimits::default());

        let mut models = BTreeMap::new();
        let metrics = ModelMetrics {
            attempts: 9,
            successes: 7,
            rate_limited: 2,
            latency_ewma_ms: Some(123.5),
            ..ModelMetrics::default()
        };
        models.insert("glm".to_string(), metrics);
        history.record_metrics_snapshot(&MetricsSnapshot {
            timestamp_ms: 1000,
            models,
        });

        history.record_circuit_transition(&CircuitTransition {
            timestamp_ms: 2000,
            model: "glm".to_string(),
            from: "CLOSED".to_string(),
            to: "OPEN".to_string(),
            reason: "failure threshold reached".to_string(),
        });
        history.record_circuit_transition(&CircuitTransition {
            timestamp_ms: 3000,
            model: "claude".to_string(),
            from: "OPEN".to_string(),
            to: "HALF_OPEN".to_string(),
            reason: "cooldown elapsed; probe admitted".to_string(),
        });
        drop(history);

        let snapshots = read_metrics_snapshots(dir.path(), None);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].timestamp_ms, 1000);
        let glm = snapshots[0].models.get("glm").unwrap();
        assert_eq!((glm.attempts, glm.successes, glm.rate_limited), (9, 7, 2));
        assert_eq!(glm.latency_ewma_ms, Some(123.5));

        let transitions = read_circuit_transitions(dir.path(), None, None);
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0].to, "OPEN");
        assert_eq!(transitions[1].to, "HALF_OPEN");
        assert_eq!(
            read_circuit_transitions(dir.path(), Some("claude"), None).len(),
            1
        );
    }

    // 9. query filters: since/until inclusive bounds, model touched via
    //    attempts or selected, client equality, limit keeps the newest
    #[test]
    fn decision_query_filters_since_until_model_client_limit() {
        let (dir, history) = store_with("query", HistoryLimits::default());
        let mut d1 = decision(1, 100, &["glm"], Some("glm"));
        d1.client = Some("term1".into());
        let d2 = decision(2, 200, &["glm", "claude"], Some("claude"));
        let d3 = decision(3, 300, &["kimi"], Some("kimi"));
        for d in [&d1, &d2, &d3] {
            history.record_decision(d);
        }
        drop(history);

        let ids = |records: &[RoutingDecision]| records.iter().map(|r| r.id).collect::<Vec<_>>();

        let all = read_decisions(dir.path(), &DecisionQuery::default()).unwrap();
        assert_eq!(ids(&all), vec![1, 2, 3]);

        let window = read_decisions(
            dir.path(),
            &DecisionQuery {
                since: Some(100),
                until: Some(200),
                ..DecisionQuery::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&window), vec![1, 2], "inclusive bounds");

        let by_model = read_decisions(
            dir.path(),
            &DecisionQuery {
                model: Some("glm".into()),
                ..DecisionQuery::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&by_model), vec![1, 2], "attempted model matches");

        let by_client = read_decisions(
            dir.path(),
            &DecisionQuery {
                client: Some("term1".into()),
                ..DecisionQuery::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&by_client), vec![1]);

        let limited = read_decisions(
            dir.path(),
            &DecisionQuery {
                limit: Some(2),
                ..DecisionQuery::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&limited), vec![2, 3], "limit keeps the most recent");
    }

    // 10. persisted decision lines contain no credential material: only the
    //     whitelist serde fields are ever written (V0.4_PLAN §10, invariant 1)
    #[test]
    fn persisted_decision_lines_carry_no_credential_material() {
        let (dir, history) = store_with("no-cred", HistoryLimits::default());
        let mut record = decision(1, 42, &["glm"], Some("glm"));
        record.outcome = "HTTP 200".to_string();
        history.record_decision(&record);
        drop(history);

        let contents = fs::read_to_string(dir.path().join("decisions.jsonl")).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        // exact whitelist of top-level keys
        let mut keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "attempts",
                "configured_candidates",
                "id",
                "outcome",
                "ranked_candidates",
                "selected",
                "selection",
                "target",
                "timestamp_ms",
            ]
        );
        for forbidden in ["authorization", "api_key", "x-api-key", "bearer", "token"] {
            assert!(
                !contents.to_lowercase().contains(forbidden),
                "history must never contain {forbidden}"
            );
        }
    }

    // 11. the active file's pre-existing size is respected after a restart:
    //     byte accounting continues from the old tail instead of restarting
    #[test]
    fn restart_appends_to_existing_active_file() {
        let limits = HistoryLimits {
            max_bytes_per_file: 10 * 1024,
            ..HistoryLimits::default()
        };
        let (dir, history) = store_with("restart", limits);
        history.record_decision(&decision(1, 100, &["glm"], Some("glm")));
        drop(history); // drain+flush+join, and release the single-writer lock

        // simulate a restart: reopen and keep writing
        let history2 = open_history(dir.path().to_path_buf(), limits, CHANNEL_CAPACITY).unwrap();
        history2.record_decision(&decision(2, 200, &["glm"], Some("glm")));
        drop(history2);

        let records = read_decisions(dir.path(), &DecisionQuery::default()).unwrap();
        assert_eq!(
            records.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2],
            "append semantics across restarts"
        );
    }

    // 12. a metrics line missing fields (schema drift) is skipped, not fatal
    #[test]
    fn foreign_lines_are_skipped_not_fatal() {
        let dir = TempDir::new("foreign");
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(
            dir.path().join("metrics.jsonl"),
            "{\"timestamp_ms\":1}\nnot json at all\n{\"timestamp_ms\":2,\"models\":{}}\n",
        )
        .unwrap();
        let snapshots = read_metrics_snapshots(dir.path(), None);
        assert_eq!(snapshots.len(), 1, "only the well-formed record parses");
        assert_eq!(snapshots[0].timestamp_ms, 2);
    }

    // sanity: the json helper compiles (keeps the serde_json import honest)
    #[test]
    fn json_helper() {
        assert_eq!(json!({"ok": true})["ok"], true);
    }
}
