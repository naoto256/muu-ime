//! Stage ③ copies actual conversion facts into v5. No hypothesis enumeration
//! occurs here; surface representatives cannot identify applied
//! candidates after surface deduplication or beam pruning (docs/src/internals/storage.md「記録」).
use crate::{
    ConversionSnapshot,
    commit_stream::{CommitStreamState, PreparedRecord, ReadingRange},
    core::LanguageModelSnapshot,
    selection::{
        SelectionOutcome, SelectionReport, SelectionRunReport, SelectionSegmentFact,
        SelectionSkipped,
    },
};
use muu_conversion::{ConversionOutcome, Segments};
use serde::Serialize;
use std::{collections::BTreeSet, time::Instant};

// Both instants are taken by `Session::convert`; the record reports the time
// to the result, and `complete` adds the time to commit.
pub(crate) struct RecordingTimes {
    pub conversion_started: Instant,
    pub result_finished: Instant,
}
/// Capture while the retained input versions and one LM state are available.
/// Reading Core again at commit would combine facts from different versions.
pub(crate) fn prepare(
    stream: Option<&mut CommitStreamState>,
    generated: &ConversionOutcome,
    selection: &SelectionOutcome,
    captured: &ConversionSnapshot,
    lm: &LanguageModelSnapshot,
    times: RecordingTimes,
) {
    if let Some(stream) = stream {
        stream.begin(
            generated.segments(),
            &selection.segments,
            &selection.report,
            captured,
            lm,
            times,
        );
    }
}
pub(crate) fn complete(
    stream: &mut CommitStreamState,
    segments: &Segments,
    selected: &BTreeSet<(usize, usize)>,
) -> Option<PreparedRecord> {
    stream.complete(segments, selected)
}

/// JSON cannot represent IEEE nonfinite values. Keep them distinct from an
/// absent measurement instead of silently losing evidence through null (docs/src/internals/storage.md「記録」).
#[derive(Clone, Copy, Serialize)]
#[serde(untagged)]
enum RecordedNumber {
    Finite(f32),
    NonFinite { non_finite: &'static str },
}
impl From<f32> for RecordedNumber {
    fn from(value: f32) -> Self {
        if value.is_finite() {
            Self::Finite(value)
        } else {
            Self::NonFinite {
                non_finite: if value.is_nan() {
                    "nan"
                } else if value.is_sign_positive() {
                    "positive_infinity"
                } else {
                    "negative_infinity"
                },
            }
        }
    }
}
#[derive(Serialize)]
pub(crate) struct RecordedSelectionReport {
    format_version: u32,
    rules_version: u32,
    skipped: Option<SelectionSkipped>,
    runs: Vec<RecordedRun>,
}
#[derive(Serialize)]
struct RecordedRun {
    reading_range: ReadingRange,
    protected_segments: Vec<usize>,
    baseline_segments: Vec<SelectionSegmentFact>,
    applied_segments: Vec<SelectionSegmentFact>,
    switched: bool,
    hypotheses: Vec<RecordedHypothesis>,
    baseline_rank: Option<usize>,
    selected_rank: Option<usize>,
    baseline_minus_best_score: Option<RecordedNumber>,
    context_chars: usize,
    skipped: Option<SelectionSkipped>,
    excluded_reading_identity_ranks: Vec<usize>,
    reading_identity_vetoed_rank: Option<usize>,
}
#[derive(Serialize)]
struct RecordedHypothesis {
    rank: usize,
    surface: String,
    full_surface: String,
    base_cost: i64,
    is_baseline: bool,
    path: RecordedHypothesisPath,
    score: RecordedNumber,
    learning_bias: RecordedNumber,
    adjusted_score: RecordedNumber,
}
#[derive(Serialize)]
struct RecordedHypothesisPath {
    nodes: Vec<crate::commit_stream::RecordedNode>,
}

impl RecordedSelectionReport {
    pub(crate) fn from_report(report: &SelectionReport) -> Self {
        Self {
            format_version: report.format_version,
            rules_version: report.rules_version,
            skipped: report.skipped,
            runs: report.runs.iter().map(RecordedRun::from_run).collect(),
        }
    }
}
impl RecordedRun {
    fn from_run(run: &SelectionRunReport) -> Self {
        Self {
            reading_range: ReadingRange {
                start: run.reading_range.start.get(),
                end: run.reading_range.end.get(),
            },
            protected_segments: run.protected_segments.clone(),
            baseline_segments: run.baseline_segments.clone(),
            applied_segments: run.applied_segments.clone(),
            switched: run.switched,
            // Whole-path facts use nodes, not coordinates in catalogs whose
            // segmentation does not yet exist (docs/src/internals/conversion.md「言語モデルによる選び直し」, docs/src/internals/storage.md「記録」).
            hypotheses: run
                .hypotheses
                .iter()
                .enumerate()
                .map(|(rank, h)| RecordedHypothesis {
                    rank,
                    surface: h.surface.clone(),
                    full_surface: h.hypothesis.surface().to_owned(),
                    base_cost: h.hypothesis.base_cost(),
                    is_baseline: h.hypothesis.is_baseline(),
                    path: RecordedHypothesisPath {
                        // Only run nodes are variable facts; outside anchors
                        // already occur in baseline. Repeating them would
                        // increase record size with run count squared (docs/src/internals/storage.md「記録」).
                        nodes: h
                            .hypothesis
                            .nodes()
                            .iter()
                            .filter(|n| {
                                n.start >= run.reading_range.start && n.end <= run.reading_range.end
                            })
                            .map(crate::commit_stream::recorded_node)
                            .collect(),
                    },
                    score: h.score.into(),
                    learning_bias: h.learning_bias.into(),
                    adjusted_score: h.adjusted_score.into(),
                })
                .collect(),
            baseline_rank: run.baseline_rank,
            selected_rank: run.selected_rank,
            baseline_minus_best_score: run.baseline_minus_best_score.map(Into::into),
            context_chars: run.context_chars,
            skipped: run.skipped,
            excluded_reading_identity_ranks: run.excluded_reading_identity_ranks.clone(),
            reading_identity_vetoed_rank: run.reading_identity_vetoed_rank,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nonfinite_numbers_are_facts_not_absent_or_misleading_zero() {
        for (value, name) in [
            (f32::NAN, "nan"),
            (f32::INFINITY, "positive_infinity"),
            (f32::NEG_INFINITY, "negative_infinity"),
        ] {
            assert_eq!(
                serde_json::to_value(RecordedNumber::from(value)).unwrap(),
                serde_json::json!({"non_finite":name})
            );
        }
        assert_eq!(
            serde_json::to_value(RecordedNumber::from(2.5)).unwrap(),
            serde_json::json!(2.5)
        );
        assert_eq!(
            serde_json::to_value(None::<RecordedNumber>).unwrap(),
            serde_json::Value::Null
        );
    }
}
