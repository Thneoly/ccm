//! Usage capture and cost accounting (v0.4 M6).
//!
//! Accepted 2xx response bodies are wrapped at the unified response
//! dispatch point in `proxy.rs` with an incremental scanner that never
//! buffers the stream (DESIGN §19 invariant 6): every chunk is scanned
//! for the small Anthropic usage frames and passed through untouched. One
//! `UsageRecord` is emitted per decision when the body ends cleanly
//! (`complete: true`), when the transport errors (`complete: false`), or
//! when the wrapper is dropped mid-stream — a client disconnect, since
//! hyper drops the body when the connection goes away (`complete: false`).
//!
//! Token sources, merged field-by-field with later frames winning:
//! - streaming: `message_start.message.usage` carries input/cache tokens
//!   natively, and `message_delta.usage` carries output tokens — plus the
//!   REAL input/cache numbers on translated streams, where the translator's
//!   `message_start` usage is an honest zero placeholder (real usage rides
//!   the final delta; V0.4_PLAN §7 correction 1). An upstream `error` event
//!   frame marks the record incomplete even if the stream then closes
//!   cleanly (M6's stream-failure accounting: metrics counted it a success
//!   at response-header time).
//! - non-streaming: the single top-level `usage` object of the response
//!   JSON (the translated envelope carries one too).
//!
//! A `message_start` without a usage field (the v0.3 mock sends exactly
//! that) is tolerated: those fields stay zero until a later frame supplies
//! them.
//!
//! Cost is computed at capture time against the pricing snapshot resolved
//! from the serving request's config: `Σ tokens/1e6 × price`. Unpriced
//! models record `pricing: null` / `cost_usd: null` — never a guessed
//! price. `cost_weight` (routing metadata) is unrelated and untouched.

use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::Bytes;
use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::model::ModelPricing;

/// Cap on a buffered SSE line. Real usage/error frames are a few hundred
/// bytes; a longer line (base64 tool output, a huge text delta) cannot be
/// one, so it is discarded up to its newline and the scanner resyncs on
/// the next line. Matches the 64 KiB bound V0.4_PLAN §10 requires.
const SSE_LINE_CAP: usize = 64 * 1024;

/// Cap on the accumulated non-streaming JSON body prefix (same magnitude
/// as the proxy's request-body cap). A response larger than this records
/// zero tokens rather than unbounded memory; usage in such bodies is lost
/// — recorded honestly as zeros with `complete: true`, never faked.
const JSON_BODY_CAP: usize = 16 * 1024 * 1024;

/// In-memory usage-ring capacity — matches the decisions ring, so the two
/// `/_ccm/*` no-parameter views cover the same recent window.
pub(crate) const USAGE_CAPACITY: usize = 100;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ===========================================================================
// Record
// ===========================================================================

/// One captured usage record per routing decision. Whitelist serde schema
/// like every other history record (DESIGN §19 invariant 1): nothing
/// beyond these fields is ever persisted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct UsageRecord {
    /// Joins `decisions.jsonl` (`RoutingDecision.id`) — the attribution key.
    pub(crate) decision_id: u64,
    pub(crate) timestamp_ms: u64,
    /// Model ALIAS (not model_id): pricing and history are keyed by alias.
    pub(crate) model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) client: Option<String>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    /// false when the body ended without a clean EOF: client disconnect
    /// (wrapper dropped mid-stream), transport error, or an upstream
    /// `error` event frame. Token counts may be partial in that case.
    pub(crate) complete: bool,
    /// Pricing snapshot resolved from config when the response was
    /// accepted; `None` = unpriced model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pricing: Option<ModelPricing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost_usd: Option<f64>,
}

/// Everything the scanner needs about the request being served; captured
/// at the dispatch point and snapshotted into the record.
#[derive(Clone, Debug)]
pub(crate) struct UsageMeta {
    pub(crate) decision_id: u64,
    pub(crate) model: String,
    pub(crate) client: Option<String>,
    pub(crate) pricing: Option<ModelPricing>,
}

impl UsageRecord {
    /// The single definition of the capture→record mapping (timestamped at
    /// call time): [`UsageStream::finalize`] and the history tests build
    /// records through here so the shape cannot drift between them.
    pub(crate) fn from_capture(meta: UsageMeta, captured: CapturedUsage) -> Self {
        let pricing = meta.pricing.clone();
        Self {
            decision_id: meta.decision_id,
            timestamp_ms: now_ms(),
            model: meta.model,
            client: meta.client,
            input_tokens: captured.input_tokens,
            output_tokens: captured.output_tokens,
            cache_read_tokens: captured.cache_read_tokens,
            cache_write_tokens: captured.cache_write_tokens,
            complete: captured.complete,
            cost_usd: pricing.as_ref().map(|p| {
                p.cost_usd(
                    captured.input_tokens,
                    captured.output_tokens,
                    captured.cache_read_tokens,
                    captured.cache_write_tokens,
                )
            }),
            pricing,
        }
    }
}

// ===========================================================================
// Cost aggregation (v0.4 M6): the shared core of `/_ccm/cost` and
// `ccm history cost`. Pure — callers pick the records (a UTC day, a client,
// or both) and serialize/print the result.
// ===========================================================================

/// One model's totals over a set of usage records.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct CostByModel {
    pub(crate) model: String,
    pub(crate) requests: u64,
    /// Records that ended cleanly (no client disconnect, transport error,
    /// or upstream error frame).
    pub(crate) complete: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    /// Records without a pricing table. Their cost is unknown, NOT zero —
    /// counted separately so the sums never overstate spend.
    pub(crate) unpriced_requests: u64,
    /// Sum of the priced records' `cost_usd`.
    pub(crate) cost_usd: f64,
}

/// Aggregation over a record set: per-model rows (sorted by model name) and
/// totals.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct CostAggregation {
    pub(crate) models: Vec<CostByModel>,
    pub(crate) total_requests: u64,
    pub(crate) total_cost_usd: f64,
    pub(crate) total_unpriced_requests: u64,
}

pub(crate) fn aggregate_costs(records: &[UsageRecord]) -> CostAggregation {
    // Index maps are overkill for dozens of models; a sorted vec keeps the
    // output deterministic by construction.
    let mut rows: Vec<CostByModel> = Vec::new();
    for record in records {
        let row = match rows.binary_search_by(|row| row.model.cmp(&record.model)) {
            Ok(index) => &mut rows[index],
            Err(index) => {
                rows.insert(
                    index,
                    CostByModel {
                        model: record.model.clone(),
                        requests: 0,
                        complete: 0,
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        unpriced_requests: 0,
                        cost_usd: 0.0,
                    },
                );
                &mut rows[index]
            }
        };
        row.requests += 1;
        row.complete += u64::from(record.complete);
        row.input_tokens += record.input_tokens;
        row.output_tokens += record.output_tokens;
        row.cache_read_tokens += record.cache_read_tokens;
        row.cache_write_tokens += record.cache_write_tokens;
        match record.cost_usd {
            Some(cost) => row.cost_usd += cost,
            None => row.unpriced_requests += 1,
        }
    }
    let total_cost_usd = rows.iter().map(|row| row.cost_usd).sum();
    let total_unpriced_requests = rows.iter().map(|row| row.unpriced_requests).sum();
    CostAggregation {
        models: rows,
        total_requests: records.len() as u64,
        total_cost_usd,
        total_unpriced_requests,
    }
}

// ===========================================================================
// Scanner
// ===========================================================================

/// Token counts extracted from one response body, plus the completeness
/// verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct CapturedUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    /// Clean end AND no upstream error event.
    pub(crate) complete: bool,
}

/// Incremental usage scanner over response-body bytes. Pure state machine:
/// `feed` chunks as they pass through, then `finish` once. SSE mode scans
/// `data:` lines; JSON mode accumulates the body and parses once at the
/// end. `Copy` token fields are written by reference from `UsageStream`.
pub(crate) struct UsageScanner {
    sse: bool,
    /// Partial line (SSE) or accumulated body prefix (JSON).
    buffer: Vec<u8>,
    /// Set once an SSE line exceeded [`SSE_LINE_CAP`]: input is dropped
    /// until the next newline, then line scanning resumes.
    discarding: bool,
    /// Set once the JSON body prefix hit [`JSON_BODY_CAP`]: further input
    /// is ignored; the truncated prefix will fail the parse at `finish`.
    json_overflow: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    /// An upstream `error` event frame was seen: `complete` stays false
    /// even on a clean close afterwards.
    saw_error: bool,
}

impl UsageScanner {
    pub(crate) fn new(sse: bool) -> Self {
        Self {
            sse,
            buffer: Vec::new(),
            discarding: false,
            json_overflow: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            saw_error: false,
        }
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        if self.sse {
            self.feed_sse(chunk);
        } else {
            self.feed_json(chunk);
        }
    }

    /// Final verdict. `clean_end` says whether the body ended with EOF
    /// (true) or an error/drop (false); the scanner additionally refuses
    /// `complete` when an error event frame was seen mid-stream. Idempotent
    /// for the JSON parse (the buffer is cleared), so a double finalize —
    /// error item then stream end — reports the same counts.
    pub(crate) fn finish(&mut self, clean_end: bool) -> CapturedUsage {
        if !self.sse && !self.json_overflow {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&self.buffer) {
                self.merge_usage_object(value.get("usage"));
            }
        }
        self.buffer = Vec::new();
        CapturedUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            complete: clean_end && !self.saw_error,
        }
    }

    fn feed_json(&mut self, chunk: &[u8]) {
        if self.json_overflow {
            return;
        }
        let room = JSON_BODY_CAP - self.buffer.len();
        if chunk.len() > room {
            self.buffer.extend_from_slice(&chunk[..room]);
            self.buffer.shrink_to_fit();
            self.json_overflow = true;
            return;
        }
        self.buffer.extend_from_slice(chunk);
    }

    fn feed_sse(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while !rest.is_empty() {
            if self.discarding {
                match position(rest, b'\n') {
                    Some(idx) => {
                        self.discarding = false;
                        rest = &rest[idx + 1..];
                    }
                    None => return,
                }
            }
            match position(rest, b'\n') {
                Some(idx) => {
                    // A line may START in an earlier chunk: the newline
                    // arriving now completes the buffered tail, so scan
                    // the joined line — not just this chunk's segment.
                    let segment = &rest[..idx];
                    let joined;
                    let mut line: &[u8] = if self.buffer.is_empty() {
                        segment
                    } else {
                        let mut tail = std::mem::take(&mut self.buffer);
                        tail.extend_from_slice(segment);
                        joined = tail;
                        &joined
                    };
                    if line.last() == Some(&b'\r') {
                        line = &line[..line.len() - 1];
                    }
                    self.scan_line(line);
                    rest = &rest[idx + 1..];
                }
                None => {
                    self.buffer.extend_from_slice(rest);
                    if self.buffer.len() > SSE_LINE_CAP {
                        // A line this large cannot be a usage frame; drop
                        // it wholesale and resync at the next newline.
                        self.buffer = Vec::new();
                        self.buffer.shrink_to_fit();
                        self.discarding = true;
                    }
                    return;
                }
            }
        }
    }

    /// One SSE line. Only `data:` payloads are parsed; `event:` and comment
    /// lines are ignored (the frame type rides the JSON anyway).
    fn scan_line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data == b"[DONE]" {
            return;
        }
        // Cheap substring gate before any allocation-free parse attempt:
        // every frame we care about contains one of these words. Content
        // deltas that merely mention "usage" parse fine and match no frame
        // type below — the gate is an optimization, not correctness.
        if !(subslice(data, b"usage") || subslice(data, b"error")) {
            return;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
            return;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                self.merge_usage_object(value.pointer("/message/usage"));
            }
            Some("message_delta") => self.merge_usage_object(value.get("usage")),
            Some("error") => self.saw_error = true,
            _ => {}
        }
    }

    /// Merge one usage object, present fields overwriting earlier ones.
    /// This is the message_start-then-message_delta contract: native
    /// streams carry input/cache in start and output in delta; translated
    /// streams zero start's input and carry the real numbers in delta.
    fn merge_usage_object(&mut self, usage: Option<&serde_json::Value>) {
        let Some(usage) = usage else { return };
        if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
            self.input_tokens = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
            self.output_tokens = v;
        }
        if let Some(v) = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.cache_read_tokens = v;
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.cache_write_tokens = v;
        }
    }
}

/// `slice::position` for `&[u8]` without a `memchr` dependency.
fn position(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

/// Substring test for `&[u8]` (both non-empty).
fn subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ===========================================================================
// Stream wrapper
// ===========================================================================

/// Body-stream wrapper driving a [`UsageScanner`] to exactly one record.
///
/// The wrapper is moved into the response `Body`; hyper polls it as the
/// client consumes the body and drops it on disconnect — that Drop is the
/// client-disconnect capture path (`complete: false`). Finalization runs
/// at most once (the emit closure is taken); the three exit paths are
/// clean end, error item, and Drop, in that order of arrival.
///
/// `emit` is a plain closure rather than a handle so the proxy layer owns
/// where records go (in-memory ring + history queue) without this module
/// depending on them. It must be `Send`: the wrapper lives on the request
/// task, but the record may outlive it via the history writer thread.
pub(crate) struct UsageStream<E: Send + 'static> {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>,
    scanner: UsageScanner,
    meta: UsageMeta,
    emit: Option<Box<dyn FnOnce(UsageRecord) + Send>>,
}

impl<E> UsageStream<E>
where
    E: Send + 'static,
{
    pub(crate) fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>,
        sse: bool,
        meta: UsageMeta,
        emit: impl FnOnce(UsageRecord) + Send + 'static,
    ) -> Self {
        Self {
            inner,
            scanner: UsageScanner::new(sse),
            meta,
            emit: Some(Box::new(emit)),
        }
    }

    fn finalize(&mut self, clean_end: bool) {
        let Some(emit) = self.emit.take() else {
            return; // already finalized
        };
        let record = UsageRecord::from_capture(self.meta.clone(), self.scanner.finish(clean_end));
        emit(record);
    }
}

impl<E> Stream for UsageStream<E>
where
    E: Send + 'static,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Result<Bytes, E>>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                this.scanner.feed(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.finalize(false);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.finalize(true);
                Poll::Ready(None)
            }
        }
    }
}

impl<E> Drop for UsageStream<E>
where
    E: Send + 'static,
{
    /// Mid-stream drop = client disconnect or handler teardown: the record
    /// still emits, marked incomplete. A no-op when finalization already
    /// ran (clean end or error item).
    fn drop(&mut self) {
        self.finalize(false);
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    /// A `message_start` exactly like the v0.3 mock's: NO usage field.
    const BARE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";

    fn start_with_usage() -> String {
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":40,\"cache_creation_input_tokens\":5}}}\n\n".to_string()
    }

    fn delta_with_usage() -> String {
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n".to_string()
    }

    fn translated_delta() -> String {
        // what translate.rs emits for the final openai usage frame:
        // message_start carried zeros, the delta carries the real numbers
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"input_tokens\":60,\"output_tokens\":3,\"cache_read_input_tokens\":40}}\n\n".to_string()
    }

    fn error_event() -> String {
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n".to_string()
    }

    fn scan(stream: &str) -> CapturedUsage {
        let mut scanner = UsageScanner::new(true);
        scanner.feed(stream.as_bytes());
        scanner.finish(true)
    }

    // 1. the tolerance case from the plan: message_start without a usage
    //    field (the v0.3 mock) leaves zeros, not an error
    #[test]
    fn message_start_without_usage_is_tolerated() {
        let captured = scan(&format!("{BARE_START}{}", delta_with_usage()));
        assert_eq!(
            (
                captured.input_tokens,
                captured.output_tokens,
                captured.cache_read_tokens,
                captured.cache_write_tokens
            ),
            (0, 7, 0, 0)
        );
        assert!(captured.complete);
    }

    // 2. native anthropic shape: input/cache from start, output from delta
    #[test]
    fn native_start_and_delta_merge() {
        let captured = scan(&format!("{}{}", start_with_usage(), delta_with_usage()));
        assert_eq!(
            (
                captured.input_tokens,
                captured.output_tokens,
                captured.cache_read_tokens,
                captured.cache_write_tokens
            ),
            (100, 7, 40, 5)
        );
        assert!(captured.complete);
    }

    // 3. translated shape: start's zeroed input is overwritten by the
    //    delta's real numbers (V0.4_PLAN §7 correction 1)
    #[test]
    fn translated_delta_overwrites_placeholder_start() {
        let mut scanner = UsageScanner::new(true);
        scanner.feed("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n".as_bytes());
        scanner.feed(translated_delta().as_bytes());
        let captured = scanner.finish(true);
        assert_eq!(
            (
                captured.input_tokens,
                captured.output_tokens,
                captured.cache_read_tokens,
                captured.cache_write_tokens
            ),
            (60, 3, 40, 0)
        );
    }

    // 4. chunk boundaries do not matter: byte-by-byte feeding equals one
    //    single feed (SSE lines split mid-JSON, mid-multibyte)
    #[test]
    fn chunk_boundaries_do_not_change_the_result() {
        let full = format!(
            "{BARE_START}{}event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"héllo ✓ usage\"}}}}\n\n{}",
            start_with_usage(),
            delta_with_usage()
        );
        let once = scan(&full);

        let mut piecewise = UsageScanner::new(true);
        for byte in full.bytes() {
            piecewise.feed(&[byte]);
        }
        assert_eq!(piecewise.finish(true), once);
    }

    // 5. an upstream error event frame poisons completeness even though
    //    the stream then ends with a clean EOF
    #[test]
    fn error_event_frame_marks_record_incomplete() {
        let captured = scan(&format!("{}{}", start_with_usage(), error_event()));
        assert!(!captured.complete);
        // tokens seen before the failure are still reported
        assert_eq!(captured.input_tokens, 100);
    }

    // 6. a line larger than the cap (base64 tool output) is discarded and
    //    the scanner resyncs on the next line — usage after it survives
    #[test]
    fn oversize_line_is_discarded_and_resyncs() {
        let big = "x".repeat(SSE_LINE_CAP + 1024);
        let stream = format!(
            "data: {big}\n\n{}data: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":9}}}}\n\n",
            start_with_usage()
        );
        let captured = scan(&stream);
        assert_eq!(captured.input_tokens, 100);
        assert_eq!(captured.output_tokens, 9);
        assert!(captured.complete);
    }

    // 7. JSON mode: top-level usage object of a non-streaming response
    #[test]
    fn json_mode_reads_top_level_usage() {
        let mut scanner = UsageScanner::new(false);
        scanner.feed(br#"{"id":"msg_1","content":[],"usage":{"input_tokens":11,"output_tokens":22,"cache_read_input_tokens":33,"cache_creation_input_tokens":44}}"#);
        let captured = scanner.finish(true);
        assert_eq!(
            (
                captured.input_tokens,
                captured.output_tokens,
                captured.cache_read_tokens,
                captured.cache_write_tokens
            ),
            (11, 22, 33, 44)
        );
        assert!(captured.complete);

        // a body without usage → zeros (still complete)
        let mut scanner = UsageScanner::new(false);
        scanner.feed(br#"{"content":[]}"#);
        assert_eq!(
            scanner.finish(true),
            CapturedUsage {
                complete: true,
                ..CapturedUsage::default()
            }
        );
    }

    // 8. lines that are not data frames, [DONE], or JSON mentioning
    //    "usage" in content text — none of them move the scanner
    #[test]
    fn irrelevant_lines_are_ignored() {
        let stream = format!(
            ": keep-alive comment\nevent: ping\ndata: [DONE]\ndata: not json at all\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"error usage error\"}}}}\n{}",
            delta_with_usage()
        );
        let captured = scan(&stream);
        assert_eq!(captured.output_tokens, 7);
        assert!(captured.complete);
    }

    /// Manual stream for wrapper tests: yields fixed items.
    struct ChunkStream {
        items: Vec<Result<Bytes, IoError>>,
    }

    impl Stream for ChunkStream {
        type Item = Result<Bytes, IoError>;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.items.is_empty() {
                Poll::Ready(None)
            } else {
                Poll::Ready(Some(this.items.remove(0)))
            }
        }
    }

    fn recording_sink() -> (
        std::sync::Arc<std::sync::Mutex<Vec<UsageRecord>>>,
        impl FnOnce(UsageRecord) + Send + 'static,
    ) {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let moved = std::sync::Arc::clone(&sink);
        (sink, move |record| moved.lock().unwrap().push(record))
    }

    fn meta(pricing: Option<ModelPricing>) -> UsageMeta {
        UsageMeta {
            decision_id: 7,
            model: "glm".to_string(),
            client: Some("term1".to_string()),
            pricing,
        }
    }

    fn wrap(
        items: Vec<Result<Bytes, IoError>>,
        sse: bool,
        meta: UsageMeta,
        emit: impl FnOnce(UsageRecord) + Send + 'static,
    ) -> UsageStream<IoError> {
        UsageStream::new(Box::pin(ChunkStream { items }), sse, meta, emit)
    }

    // 9. full drain: clean end, one record, pricing math applied
    #[tokio::test]
    async fn stream_drain_emits_one_complete_record_with_cost() {
        let body = format!("{}{}", start_with_usage(), delta_with_usage());
        let body_len = body.len();
        let (sink, emit) = recording_sink();
        let stream = wrap(
            vec![Ok(Bytes::from(body)), Ok(Bytes::from("data: [DONE]\n\n"))],
            true,
            meta(Some(ModelPricing {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            })),
            emit,
        );
        let drained = axum::body::to_bytes(axum::body::Body::from_stream(stream), 1024 * 1024)
            .await
            .unwrap();
        // "data: [DONE]\n\n" is 14 bytes
        assert_eq!(
            drained.len(),
            body_len + 14,
            "body passes through unchanged"
        );

        let records = sink.lock().unwrap();
        assert_eq!(records.len(), 1, "exactly one record");
        let record = &records[0];
        assert_eq!(record.decision_id, 7);
        assert_eq!(record.model, "glm");
        assert_eq!(record.client.as_deref(), Some("term1"));
        assert_eq!(captured_of(record), (100, 7, 40, 5));
        assert!(record.complete);
        assert!(record.pricing.is_some());
        // 100×3 + 7×15 + 40×0.3 + 5×3.75, per million
        let expected = (100.0 * 3.0 + 7.0 * 15.0 + 40.0 * 0.3 + 5.0 * 3.75) / 1_000_000.0;
        assert!((record.cost_usd.unwrap() - expected).abs() < 1e-12);
    }

    fn captured_of(record: &UsageRecord) -> (u64, u64, u64, u64) {
        (
            record.input_tokens,
            record.output_tokens,
            record.cache_read_tokens,
            record.cache_write_tokens,
        )
    }

    // 10. unpriced model: pricing/cost stay null — never a guess
    #[tokio::test]
    async fn unpriced_model_records_null_cost() {
        let (sink, emit) = recording_sink();
        let stream = wrap(
            vec![Ok(Bytes::from(delta_with_usage()))],
            true,
            meta(None),
            emit,
        );
        axum::body::to_bytes(axum::body::Body::from_stream(stream), 1024 * 1024)
            .await
            .unwrap();
        let records = sink.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].pricing.is_none());
        assert!(records[0].cost_usd.is_none());
        assert!(records[0].complete);
    }

    // 11. dropping the wrapper mid-stream = client disconnect: the record
    //     still emits, marked incomplete
    #[tokio::test]
    async fn drop_mid_stream_emits_incomplete_record() {
        let (sink, emit) = recording_sink();
        let mut stream = wrap(
            vec![
                Ok(Bytes::from(start_with_usage())),
                Ok(Bytes::from(delta_with_usage())),
            ],
            true,
            meta(None),
            emit,
        );
        // consume one item, then drop without reaching EOF — ChunkStream
        // is always Ready, so a noop waker is enough to drive the poll
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let polled = std::pin::Pin::new(&mut stream).poll_next(&mut cx);
        assert!(polled.is_ready(), "first item is immediately ready");
        drop(stream);

        let records = sink.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert!(!records[0].complete, "disconnect before EOF is incomplete");
        assert_eq!(records[0].input_tokens, 100, "tokens seen so far survive");
    }

    // 12. a transport error item finalizes once, incomplete, then EOF does
    //     not emit a second record
    #[tokio::test]
    async fn error_item_finalizes_once_incomplete() {
        let (sink, emit) = recording_sink();
        let stream = wrap(
            vec![
                Ok(Bytes::from(start_with_usage())),
                Err(IoError::new(ErrorKind::ConnectionReset, "reset")),
            ],
            true,
            meta(None),
            emit,
        );
        let result = axum::body::to_bytes(axum::body::Body::from_stream(stream), 1024 * 1024).await;
        assert!(result.is_err(), "the error passes through to the body");
        let records = sink.lock().unwrap();
        assert_eq!(records.len(), 1, "no double emission");
        assert!(!records[0].complete);
    }

    // 13. error event frame through the wrapper: clean EOF but incomplete
    #[tokio::test]
    async fn error_event_frame_through_wrapper() {
        let (sink, emit) = recording_sink();
        let stream = wrap(
            vec![Ok(Bytes::from(format!(
                "{}{}",
                start_with_usage(),
                error_event()
            )))],
            true,
            meta(None),
            emit,
        );
        axum::body::to_bytes(axum::body::Body::from_stream(stream), 1024 * 1024)
            .await
            .unwrap();
        let records = sink.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert!(!records[0].complete);
    }

    // 14. cost aggregation: per-model rows sorted by name, token sums,
    //     unpriced counted separately so the totals never overstate spend
    #[test]
    fn aggregate_costs_buckets_models_and_separates_unpriced() {
        let record = |model: &str, input: u64, cost: Option<f64>| UsageRecord {
            decision_id: 1,
            timestamp_ms: 0,
            model: model.to_string(),
            client: None,
            input_tokens: input,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            complete: true,
            pricing: None,  // pricing snapshot is display-only here
            cost_usd: cost, // ... the aggregation reads only cost_usd
        };
        let aggregation = aggregate_costs(&[
            record("zeta", 100, Some(0.001)),
            record("alpha", 10, None), // unpriced
            record("zeta", 50, Some(0.002)),
            record("alpha", 20, Some(0.0005)),
            UsageRecord {
                model: "alpha".to_string(),
                complete: false, // a disconnected client stream
                ..record("alpha", 5, Some(0.0))
            },
        ]);
        assert_eq!(
            aggregation
                .models
                .iter()
                .map(|row| row.model.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"],
            "rows sorted by model name"
        );
        let alpha = &aggregation.models[0];
        assert_eq!(alpha.requests, 3);
        assert_eq!(alpha.complete, 2, "the disconnected stream is not complete");
        assert_eq!(alpha.input_tokens, 35);
        assert_eq!(alpha.unpriced_requests, 1);
        assert!(
            (alpha.cost_usd - 0.0005).abs() < 1e-12,
            "unpriced adds zero"
        );
        let zeta = &aggregation.models[1];
        assert_eq!(zeta.requests, 2);
        assert_eq!(zeta.input_tokens, 150);
        assert!((zeta.cost_usd - 0.003).abs() < 1e-12);

        assert_eq!(aggregation.total_requests, 5);
        assert_eq!(aggregation.total_unpriced_requests, 1);
        assert!((aggregation.total_cost_usd - 0.0035).abs() < 1e-12);

        // empty input: an honest zero, not an error
        let empty = aggregate_costs(&[]);
        assert!(empty.models.is_empty());
        assert_eq!(empty.total_requests, 0);
        assert_eq!(empty.total_cost_usd, 0.0);
    }
}
