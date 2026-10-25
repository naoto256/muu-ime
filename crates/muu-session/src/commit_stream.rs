//! v5 records actual conversion facts, never membership in an enumerated beam.
//! A Session owns a draft; Core owns only completed, bounded bytes (docs/src/internals/storage.md「記録」).

// Spelling only: each recorded digest is computed by its owner (dictionary
// payload, model artifacts, auxiliary data).
use muu_digest::hex_lower as hex;
use std::collections::{BTreeSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use muu_conversion::{LexiconOverlayTier, OverlayFingerprint, Segments, TokenKey};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::{ConversionSnapshot, core::LanguageModelSnapshot, selection::SelectionReport};

pub const COMMIT_STREAM_V5_SCHEMA: &str = "muu.commit-stream.v5";
// Bounds one record and also the whole queue. The Swift recorder rejects
// anything larger than this same size and then stops recording for the rest
// of the process, so the two limits must be changed together.
pub(crate) const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_QUEUED_RECORDS: usize = 256;

// Fixed by the host at startup and copied into every record. Only the v5
// schema name is accepted, so a host built for another format fails Session
// creation instead of producing records it cannot read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CommitStreamConfig {
    pub schema: String,
    pub process_nonce: String,
    pub source_commit: String,
    pub build: u64,
}

impl CommitStreamConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema == COMMIT_STREAM_V5_SCHEMA {
            Ok(())
        } else {
            Err("unsupported commit stream schema")
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReadingRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RecordedSegment {
    pub segment_index: usize,
    pub reading_range: ReadingRange,
    pub candidate_id: String,
    pub surface: String,
    pub path: Option<RecordedPath>,
    pub origin_layers: Vec<&'static str>,
    auxiliary: Option<RecordedAuxiliary>,
}

#[derive(Clone, Debug, Serialize)]
struct RecordedAuxiliary {
    kind: &'static str,
    data_sha256: String,
    entry_id: u32,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct RecordedPath {
    index: usize,
    nodes: Vec<RecordedNode>,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct RecordedNode {
    reading_range: ReadingRange,
    surface: String,
    token: RecordedToken,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RecordedToken {
    Dictionary {
        token_id: u64,
    },
    Overlay {
        tier: &'static str,
        dictionary_key: String,
        entry_id: u32,
    },
    Unknown {
        class: &'static str,
    },
}

/// Capture paths[0], the actual commit/learning authority, not a scored latent
/// path. Incomplete evidence remains incomplete instead of inventing nodes (docs/src/internals/storage.md「記録」).
pub(crate) fn actual_segments(segments: &Segments) -> Vec<RecordedSegment> {
    segments
        .segments()
        .iter()
        .enumerate()
        .map(|(segment_index, segment)| {
            let candidate = segment.focused();
            let auxiliary = candidate.auxiliary().map(|entry| RecordedAuxiliary {
                kind: entry.kind().stable_name(),
                data_sha256: hex(entry.data_sha256()),
                entry_id: entry.entry_id(),
            });
            // Indices into the layer names below, so the list comes out
            // sorted and without duplicates.
            let mut layers = BTreeSet::new();
            if auxiliary.is_some() {
                layers.insert(4);
            }
            let path = candidate.paths().first().map(|path| {
                for node in &path.nodes {
                    match node.token_key {
                        TokenKey::Dictionary(_) => {
                            layers.insert(0);
                        }
                        TokenKey::Overlay { tier, .. } => {
                            layers.insert(match tier {
                                LexiconOverlayTier::User => 1,
                                LexiconOverlayTier::Imported => 2,
                                LexiconOverlayTier::PostalCode => 3,
                            });
                        }
                        TokenKey::Unknown { .. } => {}
                    }
                }
                RecordedPath {
                    index: 0,
                    nodes: path.nodes.iter().map(recorded_node).collect(),
                }
            });
            RecordedSegment {
                segment_index,
                reading_range: ReadingRange {
                    start: segment.reading_range().start.get(),
                    end: segment.reading_range().end.get(),
                },
                candidate_id: candidate.id().as_str().to_owned(),
                surface: candidate.surface().to_owned(),
                path,
                // Unknown-word nodes belong to no layer, so a segment made
                // only of them has an empty list.
                origin_layers: layers
                    .into_iter()
                    .map(|i| ["internal", "user", "imported", "postal", "auxiliary"][i])
                    .collect(),
                auxiliary,
            }
        })
        .collect()
}

/// Share the token representation of scored and actual paths (docs/src/internals/storage.md「記録」). Candidate
/// IDs exist only after a segment catalog has been materialized.
pub(crate) fn recorded_node(node: &muu_conversion::PathNode) -> RecordedNode {
    let token = match node.token_key {
        TokenKey::Dictionary(id) => RecordedToken::Dictionary { token_id: id.get() },
        TokenKey::Overlay {
            tier,
            dictionary_key,
            entry_id,
        } => RecordedToken::Overlay {
            tier: match tier {
                LexiconOverlayTier::User => "user",
                LexiconOverlayTier::Imported => "imported",
                LexiconOverlayTier::PostalCode => "postal",
            },
            dictionary_key: format!("{dictionary_key:016x}"),
            entry_id,
        },
        TokenKey::Unknown { class, .. } => RecordedToken::Unknown {
            class: match class {
                muu_dict::UnknownClass::Number => "number",
                muu_dict::UnknownClass::AsciiAlphabetic => "ascii_alphabetic",
                muu_dict::UnknownClass::Katakana => "katakana",
                muu_dict::UnknownClass::Kanji => "kanji",
                muu_dict::UnknownClass::Other => "other",
            },
        },
    };
    RecordedNode {
        reading_range: ReadingRange {
            start: node.start.get(),
            end: node.end.get(),
        },
        surface: node.surface.clone(),
        token,
    }
}

#[derive(Serialize)]
struct DictionaryAuthority {
    payload_sha256: String,
    token_count: u64,
}
#[derive(Serialize)]
struct LayerAuthority {
    version: u64,
    fingerprint: OverlayFingerprint,
}
#[derive(Serialize)]
struct VocabularyAuthority {
    user: LayerAuthority,
    imported: LayerAuthority,
    postal: LayerAuthority,
}
#[derive(Serialize)]
struct LearningAuthority {
    generation: u64,
    version: u64,
    personalization_applied: bool,
}
#[derive(Serialize)]
struct ModelAuthority {
    state: &'static str,
    weights_sha256: Option<String>,
    tokenizer_sha256: Option<String>,
    #[serde(flatten)]
    details: crate::LanguageModelInfo,
}
#[derive(Serialize)]
struct Provenance {
    source_commit: String,
    build: u64,
    dictionary: Option<DictionaryAuthority>,
    vocabulary: VocabularyAuthority,
    learning: LearningAuthority,
    language_model: ModelAuthority,
    rules_version: u32,
}
#[derive(Serialize)]
struct Initial {
    reading: String,
    generation_segments: Vec<RecordedSegment>,
    selected_segments: Vec<RecordedSegment>,
    selection_report: crate::recording::RecordedSelectionReport,
}
#[derive(Serialize)]
struct FinalSegment {
    #[serde(flatten)]
    segment: RecordedSegment,
    // The existing learning mark: only changed candidates, cleared by resize.
    explicitly_selected: bool,
}
#[derive(Serialize)]
struct Final {
    segments: Vec<FinalSegment>,
    // Any accepted candidate operation, including reselection; never used for
    // learning. This survives resize, unlike the per-segment learning mark (docs/src/internals/storage.md「記録」).
    candidate_selected_explicitly: bool,
    segment_moved: bool,
    segment_resized: bool,
    surface: String,
}
#[derive(Serialize)]
struct Timing {
    conversion_to_result_us: u64,
    conversion_to_commit_us: u64,
}
#[derive(Serialize)]
struct Event {
    schema: &'static str,
    candidate_format_version: u32,
    candidate_presentation: crate::candidate_presentation::PresentationMetadata,
    process_nonce: String,
    session_nonce: String,
    event_seq: u64,
    provenance: Provenance,
    initial: Initial,
    #[serde(rename = "final")]
    final_state: Final,
    timing: Timing,
}

// Test-only fault points: preparation at `begin`, and finalization or
// serialization at `complete`. Each must turn into one lost record and
// nothing else.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RecordingFailure {
    Preparation,
    Finalization,
    Serialization,
    SerializationPanic,
}

// One conversion's record in progress. `initial` and `provenance` are filled
// once, when the conversion produces its result, and are moved unchanged into
// the event at commit: later moves, resizes and candidate choices only set the
// three flags. That is what lets the record show what the conversion first
// offered next to what was finally committed (docs/src/internals/storage.md「記録」).
struct Draft {
    started: Instant,
    result_us: u64,
    initial: Initial,
    provenance: Provenance,
    candidate_presentation: crate::candidate_presentation::PresentationMetadata,
    explicitly_selected: bool,
    moved: bool,
    resized: bool,
}

/// The draft does not retain a full candidate catalog. This releases discarded
/// conversion candidates and keeps recording independent of search enumerators.
pub(crate) struct CommitStreamState {
    config: CommitStreamConfig,
    session_nonce: String,
    // None is permanent sequence exhaustion: never wrap and reuse identity.
    pub(crate) next_event_seq: Option<u64>,
    // Err retains a failed capture until commit/cancel. Counting immediately
    // would report a lost record even if the user later cancels (docs/src/internals/storage.md「記録」).
    active: Option<Result<Draft, ()>>,
    #[cfg(test)]
    pub(crate) fail_next: Option<RecordingFailure>,
}
impl CommitStreamState {
    pub(crate) fn new(config: CommitStreamConfig, session_nonce: String) -> Self {
        Self {
            config,
            session_nonce,
            next_event_seq: Some(0),
            active: None,
            #[cfg(test)]
            fail_next: None,
        }
    }
    // Starts the draft for a conversion that has just produced its result,
    // replacing any draft still present. Vocabulary, learning and LM facts
    // come from the values this conversion already holds, never from the Core
    // as it is now, so the provenance describes the inputs that were used.
    pub(crate) fn begin(
        &mut self,
        generated: &Segments,
        selected: &Segments,
        report: &SelectionReport,
        captured: &ConversionSnapshot,
        lm: &LanguageModelSnapshot,
        times: crate::recording::RecordingTimes,
    ) {
        if self.next_event_seq.is_none() {
            self.active = Some(Err(()));
            return;
        }
        #[cfg(test)]
        let injected = if self.fail_next == Some(RecordingFailure::Preparation) {
            self.fail_next.take()
        } else {
            None
        };
        // Capturing telemetry cannot invalidate a successfully generated result.
        // Preserve a failed draft so only a later commit publishes its loss (docs/src/internals/storage.md「記録」).
        self.active = Some(
            catch_unwind(AssertUnwindSafe(|| {
                #[cfg(test)]
                assert!(injected.is_none(), "injected record preparation panic");
                let started = times.conversion_started;
                let finished = times.result_finished;
                let layer = |revision, overlay: &muu_conversion::LexiconOverlay| LayerAuthority {
                    version: revision,
                    fingerprint: overlay.fingerprint().clone(),
                };
                let hashes = lm.model.as_ref().map(|m| {
                    let (w, t) = m.artifact_sha256();
                    (hex(w), hex(t))
                });
                Draft {
                    started,
                    result_us: elapsed_us(started, finished),
                    explicitly_selected: false,
                    moved: false,
                    resized: false,
                    candidate_presentation: crate::candidate_presentation::metadata(),
                    initial: Initial {
                        reading: generated.reading().to_owned(),
                        generation_segments: actual_segments(generated),
                        selected_segments: actual_segments(selected),
                        selection_report: crate::recording::RecordedSelectionReport::from_report(
                            report,
                        ),
                    },
                    provenance: Provenance {
                        source_commit: self.config.source_commit.clone(),
                        build: self.config.build,
                        dictionary: captured.dictionary.as_ref().map(|d| {
                            let f = d.fingerprint();
                            DictionaryAuthority {
                                payload_sha256: hex(f.payload_sha256()),
                                token_count: f.token_count(),
                            }
                        }),
                        vocabulary: VocabularyAuthority {
                            user: layer(
                                captured.vocabulary.user.revision,
                                &captured.vocabulary.user.overlay,
                            ),
                            imported: layer(
                                captured.vocabulary.imported.revision,
                                &captured.vocabulary.imported.overlay,
                            ),
                            postal: layer(
                                captured.vocabulary.postal.revision,
                                &captured.vocabulary.postal.overlay,
                            ),
                        },
                        learning: LearningAuthority {
                            generation: captured.learning.generation,
                            version: captured.learning.revision,
                            personalization_applied: !captured.learning.frequency.is_empty(),
                        },
                        language_model: ModelAuthority {
                            state: match lm.state {
                                crate::LanguageModelState::Unloaded => "unloaded",
                                crate::LanguageModelState::Loading => "loading",
                                crate::LanguageModelState::Available => "available",
                                crate::LanguageModelState::Failed => "failed",
                            },
                            weights_sha256: hashes.as_ref().map(|h| h.0.clone()),
                            tokenizer_sha256: hashes.map(|h| h.1),
                            details: lm.info.clone(),
                        },
                        rules_version: report.rules_version,
                    },
                }
            }))
            .map_err(|_| ()),
        );
    }
    pub(crate) fn cancel_active(&mut self) {
        self.active = None;
    }
    pub(crate) fn selected_explicitly(&mut self) {
        if let Some(Ok(d)) = &mut self.active {
            d.explicitly_selected = true;
        }
    }
    pub(crate) fn moved(&mut self) {
        if let Some(Ok(d)) = &mut self.active {
            d.moved = true;
        }
    }
    pub(crate) fn resized(&mut self) {
        if let Some(Ok(d)) = &mut self.active {
            d.resized = true;
        }
    }
    #[cfg(test)]
    pub(crate) fn has_active_for_testing(&self) -> bool {
        self.active.is_some()
    }
    /// Per-segment marks match learning; the draft's ever-selected flag survives
    /// resize clearing those marks. Conflating them would change learning (docs/src/internals/storage.md「記録」).
    ///
    /// The sequence number advances only here, when a draft becomes a record.
    /// A commit that had no draft, or whose draft was cancelled first,
    /// produces nothing and leaves no gap. Failed local preparation and queue
    /// refusal leave a gap and contribute to the next record's dropped_records.
    /// Exhaustion stops records, not commits; subsequent commits still count losses.
    /// Local final capture/serialization panics are isolated here as record loss.
    /// This catch does not include learning preparation (Session failure) or
    /// shared publication (Core failure); neither boundary moves into telemetry.
    pub(crate) fn complete(
        &mut self,
        segments: &Segments,
        selected_ranges: &BTreeSet<(usize, usize)>,
    ) -> Option<PreparedRecord> {
        // Sequence numbers start at 1: `next_event_seq` holds the last one
        // issued, and each completed draft, kept or lost, takes the next.
        let draft = self.active.take()?;
        let sequence = self.next_event_seq.and_then(|seq| seq.checked_add(1));
        self.next_event_seq = sequence;
        let (Some(sequence), Ok(draft)) = (sequence, draft) else {
            return Some(PreparedRecord::Dropped);
        };
        #[cfg(test)]
        let injected = self.fail_next.take();
        // Advance identity first, then isolate only final telemetry capture and
        // serialization. Learning/commit and Core's publication guard remain
        // outside this catch: a local record failure is just a loss (docs/src/internals/architecture.md「失敗したとき」, docs/src/internals/storage.md「記録」).
        Some(
            match catch_unwind(AssertUnwindSafe(|| {
                #[cfg(test)]
                assert!(
                    injected != Some(RecordingFailure::Finalization),
                    "injected record finalization panic"
                );
                let event = Event {
                    schema: COMMIT_STREAM_V5_SCHEMA,
                    candidate_format_version: 2,
                    candidate_presentation: draft.candidate_presentation,
                    process_nonce: self.config.process_nonce.clone(),
                    session_nonce: self.session_nonce.clone(),
                    event_seq: sequence,
                    provenance: draft.provenance,
                    initial: draft.initial,
                    final_state: Final {
                        segments: actual_segments(segments)
                            .into_iter()
                            .map(|segment| {
                                let explicitly_selected = selected_ranges.contains(&(
                                    segment.reading_range.start,
                                    segment.reading_range.end,
                                ));
                                FinalSegment {
                                    segment,
                                    explicitly_selected,
                                }
                            })
                            .collect(),
                        candidate_selected_explicitly: draft.explicitly_selected,
                        segment_moved: draft.moved,
                        segment_resized: draft.resized,
                        surface: segments
                            .segments()
                            .iter()
                            .map(|s| s.focused().surface())
                            .collect::<String>()
                            .nfc()
                            .collect(),
                    },
                    timing: Timing {
                        conversion_to_result_us: draft.result_us,
                        conversion_to_commit_us: elapsed_us(draft.started, Instant::now()),
                    },
                };
                #[cfg(test)]
                assert!(
                    injected != Some(RecordingFailure::SerializationPanic),
                    "injected serialization panic"
                );
                #[cfg(test)]
                if injected == Some(RecordingFailure::Serialization) {
                    return Err(());
                }
                PreparedRecord::from_json(&event)
            })) {
                Ok(Ok(record)) => record,
                Ok(Err(())) | Err(_) => PreparedRecord::Dropped,
            },
        )
    }
}

/// All expensive serialization and capacity allocation happens before Core's
/// queue lock. Only a bounded decimal counter is appended during publication.
pub(crate) enum PreparedRecord {
    Json(Vec<u8>),
    // Loss is published under the same queue lock as capacity refusal, so the
    // next successful event cannot miss a failed local preparation (docs/src/internals/storage.md「記録」).
    Dropped,
}
impl PreparedRecord {
    // The closing brace is held back because the last field, the number of
    // records dropped before this one, is only known under the queue lock.
    // Reserving 21 bytes (20 digits of a u64 and the brace) means that step
    // appends without allocating. `value` must serialize to a non-empty JSON
    // object, or the appended field would not form valid JSON.
    pub(crate) fn from_json(value: &impl Serialize) -> Result<Self, ()> {
        let mut prefix = serde_json::to_vec(value).map_err(|_| ())?;
        if prefix.len() <= 2 || prefix.first() != Some(&b'{') || prefix.pop() != Some(b'}') {
            return Err(());
        }
        prefix.extend_from_slice(b",\"dropped_records\":");
        prefix.try_reserve(21).map_err(|_| ())?;
        Ok(Self::Json(prefix))
    }
}
// Delivery weight travels beside opaque bytes, so a host can return a lost
// record and every earlier loss it carried without decoding the v5 format.
pub(crate) struct QueuedRecord {
    pub(crate) bytes: Vec<u8>,
    pub(crate) drop_weight: u64,
}
#[derive(Default)]
pub(crate) struct RecordQueue {
    records: VecDeque<QueuedRecord>,
    // Total size of the queued records, kept so the byte cap needs no scan.
    bytes: usize,
    // Records refused since the last one accepted. It is written into the
    // next accepted record and cleared only then; if that record is refused
    // as well, the count simply grows, so no loss goes unreported.
    dropped: u64,
}
impl RecordQueue {
    /// Drop newest, preserving already published order. Oversized individual
    /// records also count; never truncate hypotheses to hide missing facts (docs/src/internals/storage.md「記録」).
    // The queue's total bytes share the single-record limit, so a host that
    // never drains holds at most MAX_RECORD_BYTES in at most 256 records.
    // `drop_weight` is this record plus every loss it carries, which is what
    // the host must report back if it later loses the record itself.
    pub(crate) fn push(&mut self, record: PreparedRecord) {
        let PreparedRecord::Json(mut prefix) = record else {
            self.dropped = self.dropped.saturating_add(1);
            return;
        };
        let mut digits = [0u8; 20];
        let mut n = self.dropped;
        let mut start = 20;
        loop {
            start -= 1;
            digits[start] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let size = prefix.len() + 20 - start + 1;
        if self.records.len() >= MAX_QUEUED_RECORDS
            || size > MAX_RECORD_BYTES
            || self.bytes + size > MAX_RECORD_BYTES
        {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        prefix.extend_from_slice(&digits[start..]);
        prefix.push(b'}');
        self.records.push_back(QueuedRecord {
            bytes: prefix,
            drop_weight: self.dropped.saturating_add(1),
        });
        self.bytes += size;
        self.dropped = 0;
    }
    pub(crate) fn pop_front(&mut self) -> Option<Vec<u8>> {
        let record = self.records.pop_front()?;
        self.bytes -= record.bytes.len();
        Some(record.bytes)
    }
    pub(crate) fn front(&self) -> Option<&QueuedRecord> {
        self.records.front()
    }
    pub(crate) fn report_dropped(&mut self, weight: u64) {
        // Only future pushes receive this debt; queued bytes are immutable.
        self.dropped = self.dropped.saturating_add(weight);
    }
}
fn elapsed_us(start: Instant, end: Instant) -> u64 {
    end.saturating_duration_since(start)
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(pad: usize) -> PreparedRecord {
        PreparedRecord::from_json(&serde_json::json!({"pad":"x".repeat(pad)})).unwrap()
    }
    fn dropped(record: &[u8]) -> u64 {
        serde_json::from_slice::<serde_json::Value>(record).unwrap()["dropped_records"]
            .as_u64()
            .unwrap()
    }
    #[test]
    fn serialization_rejection_is_a_result_and_loss_uses_the_same_queue_counter() {
        struct Rejected;
        impl Serialize for Rejected {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("synthetic rejection"))
            }
        }
        assert!(PreparedRecord::from_json(&Rejected).is_err());
        assert!(PreparedRecord::from_json(&serde_json::json!({})).is_err());
        assert!(PreparedRecord::from_json(&serde_json::json!([])).is_err());
        let mut queue = RecordQueue::default();
        queue.push(PreparedRecord::Dropped);
        queue.push(record(0));
        assert_eq!(dropped(&queue.pop_front().unwrap()), 1);
    }

    #[test]
    fn queue_caps_count_and_reports_newest_drops_only_on_next_success() {
        let mut queue = RecordQueue::default();
        for _ in 0..MAX_QUEUED_RECORDS {
            queue.push(record(0));
        }
        queue.push(record(0));
        queue.push(record(0));
        assert_eq!(queue.records.len(), MAX_QUEUED_RECORDS);
        assert_eq!(queue.dropped, 2);
        assert_eq!(dropped(&queue.pop_front().unwrap()), 0);
        queue.push(record(0));
        assert_eq!(queue.dropped, 0);
        for _ in 1..MAX_QUEUED_RECORDS {
            assert_eq!(dropped(&queue.pop_front().unwrap()), 0);
        }
        assert_eq!(dropped(&queue.pop_front().unwrap()), 2);
        assert_eq!(queue.bytes, 0);
    }
    #[test]
    fn queue_caps_bytes_and_counts_individual_oversize_without_truncation() {
        let mut queue = RecordQueue::default();
        queue.push(record(MAX_RECORD_BYTES / 2));
        let bytes = queue.bytes;
        queue.push(record(MAX_RECORD_BYTES / 2));
        queue.push(record(MAX_RECORD_BYTES));
        assert_eq!(queue.bytes, bytes);
        assert!(queue.bytes <= MAX_RECORD_BYTES);
        assert_eq!(queue.records.len(), 1);
        assert_eq!(queue.dropped, 2);
        queue.pop_front();
        queue.push(record(0));
        assert_eq!(dropped(&queue.pop_front().unwrap()), 2);
        assert_eq!(queue.bytes, 0);
    }
    #[test]
    fn delivery_weights_preserve_carried_losses_and_saturate() {
        let mut queue = RecordQueue::default();
        for _ in 0..3 {
            queue.push(PreparedRecord::Dropped);
        }
        queue.push(record(0));
        assert_eq!(queue.front().unwrap().drop_weight, 4);
        assert_eq!(dropped(&queue.pop_front().unwrap()), 3);
        queue.push(record(0)); // Already queued before the return: unchanged.
        queue.report_dropped(4);
        queue.push(record(0));
        assert_eq!(queue.front().unwrap().drop_weight, 1);
        assert_eq!(dropped(&queue.pop_front().unwrap()), 0);
        assert_eq!(queue.front().unwrap().drop_weight, 5);
        assert_eq!(dropped(&queue.pop_front().unwrap()), 4);
        queue.report_dropped(5);
        queue.report_dropped(u64::MAX);
        queue.push(PreparedRecord::Dropped);
        queue.push(record(0));
        assert_eq!(queue.front().unwrap().drop_weight, u64::MAX);
        assert_eq!(dropped(&queue.pop_front().unwrap()), u64::MAX);
        assert_eq!(queue.bytes, 0);
    }
}
