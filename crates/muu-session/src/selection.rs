//! Stage ② owns eligibility, one constrained full-lattice enumeration per run,
//! and the actual application facts (docs/src/internals/conversion.md「言語モデルによる選び直し」). It never acquires mutable stores.
use crate::{LanguageModelSnapshot, LanguageModelState};
use muu_conversion::{
    CostSnapshot, FullHypothesis, FullHypothesisLattice, HypothesisBudget, LexiconOverlayTier,
    PathCandidate, ScalarIndex, ScalarRange, Segment, Segmenter, Segments, TokenKey,
    rebuild_run_from_hypothesis,
};
use unicode_normalization::UnicodeNormalization;

// Why stage ② kept the baseline, for the whole conversion (report.skipped) or
// for one run. The names are written into v5 records, so renaming one is a
// record format change. A run that was scored and kept the baseline by the
// margin rule has no skip reason; its ranks say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSkipped {
    IncompletePathEvidence,
    AuthoritativeDictionaryBaseline,
    NonKanaReading,
    InseparableReadingBoundary,
    NoAlternativeAfterConstraints,
    LanguageModelNotLoaded,
    LanguageModelLoading,
    LanguageModelFailed,
    HypothesisEnumerationFailed,
    HypothesisEnumerationBudgetExceeded,
    BaselineNotFound,
    ScoringFailed,
    HypothesisApplicationFailed,
}

/// Paths of scored run-surface representatives, not catalog indices (docs/src/internals/conversion.md「言語モデルによる選び直し」).
/// Baseline is the explicit exception: it always owns the actual input path.
pub type SelectionHypothesis = FullHypothesis;
// Scores are model NLL in nats: lower is better. The learning bias is
// subtracted, so `adjusted_score` is the value the policy compares.
#[derive(Clone, Debug)]
pub struct ScoredHypothesis {
    pub surface: String,
    pub hypothesis: SelectionHypothesis,
    pub score: f32,
    // Raw model evidence stays intact. Policy and reported margin use the
    // adjusted score, derived from this conversion's captured token counts.
    pub learning_bias: f32,
    pub adjusted_score: f32,
}

/// Version the report's representation independently of the selection policy.
/// v5 remains an opaque record; readers distinguish path facts from the old
/// fixed-catalog coordinates through format_version (docs/src/internals/conversion.md「言語モデルによる選び直し」, docs/src/internals/storage.md「記録」).
#[derive(Clone, Debug)]
pub struct SelectionReport {
    pub format_version: u32,
    pub runs: Vec<SelectionRunReport>,
    pub rules_version: u32,
    pub skipped: Option<SelectionSkipped>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct SelectionReadingRange {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct SelectionSegmentFact {
    pub segment_index: usize,
    pub reading_range: SelectionReadingRange,
    pub candidate_id: String,
    pub surface: String,
    pub path: Option<SelectionPathFact>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct SelectionPathFact {
    pub index: usize,
}
fn related_actual_segments(segments: &Segments, range: ScalarRange) -> Vec<SelectionSegmentFact> {
    segments
        .segments()
        .iter()
        .enumerate()
        .filter(|(_, s)| overlaps(s.reading_range(), range))
        .map(|(segment_index, s)| SelectionSegmentFact {
            segment_index,
            reading_range: SelectionReadingRange {
                start: s.reading_range().start.get(),
                end: s.reading_range().end.get(),
            },
            candidate_id: s.focused().id().as_str().to_owned(),
            surface: s.focused().surface().to_owned(),
            path: s
                .focused()
                .paths()
                .first()
                .map(|_| SelectionPathFact { index: 0 }),
        })
        .collect()
}

/// Ranks name the enumerated run surface. Protected indices belong to this
/// run's input list, applied indices to its output list: previous runs may
/// change segment counts. Reading ranges make those coordinates unambiguous.
#[derive(Clone, Debug)]
pub struct SelectionRunReport {
    pub reading_range: ScalarRange,
    pub baseline_segments: Vec<SelectionSegmentFact>,
    pub applied_segments: Vec<SelectionSegmentFact>,
    pub switched: bool,
    pub protected_segments: Vec<usize>,
    pub hypotheses: Vec<ScoredHypothesis>,
    pub baseline_rank: Option<usize>,
    pub selected_rank: Option<usize>,
    /// A scored property, not evidence that the policy determined a decision.
    pub excluded_reading_identity_ranks: Vec<usize>,
    /// Set only when the selector proposed a reading identity and the policy
    /// vetoed it.
    pub reading_identity_vetoed_rank: Option<usize>,
    pub baseline_minus_best_score: Option<f32>,
    pub context_chars: usize,
    pub skipped: Option<SelectionSkipped>,
}
impl SelectionRunReport {
    fn empty(reading_range: ScalarRange) -> Self {
        Self {
            reading_range,
            baseline_segments: vec![],
            applied_segments: vec![],
            switched: false,
            protected_segments: vec![],
            hypotheses: vec![],
            baseline_rank: None,
            selected_rank: None,
            excluded_reading_identity_ranks: vec![],
            reading_identity_vetoed_rank: None,
            baseline_minus_best_score: None,
            context_chars: 0,
            skipped: None,
        }
    }
}
pub(crate) struct SelectionOutcome {
    pub segments: Segments,
    pub report: SelectionReport,
}

/// The caller takes one LM view and one immutable conversion snapshot. All
/// runs share them, so completion of a background load cannot alter a decision
/// midway through conversion (docs/src/internals/conversion.md「変換が使うデータを固定する」, docs/src/internals/conversion.md「言語モデルの読み込み」). Stage ③ never enumerates again.
pub(crate) fn select(
    segments: &Segments,
    path_evidence_complete: bool,
    snapshot: &CostSnapshot<'_>,
    learning: &crate::personal_frequency::CapturedPersonalFrequency,
    context: &str,
    language_model: &LanguageModelSnapshot,
    segmenter: &Segmenter,
) -> SelectionOutcome {
    let mut report = SelectionReport {
        format_version: 3,
        runs: vec![],
        rules_version: language_model
            .model
            .as_ref()
            .map_or(muu_lm::SELECTION_RULES_VERSION, |m| {
                m.kind().rules_version()
            }),
        skipped: None,
    };
    if !path_evidence_complete
        || segments
            .segments()
            .iter()
            .any(|s| s.focused().auxiliary().is_some())
    {
        report.skipped = Some(SelectionSkipped::IncompletePathEvidence);
        return SelectionOutcome {
            segments: segments.clone(),
            report,
        };
    }
    // Runs are taken left to right on the progressively updated segments, so
    // each later run sees the earlier runs' choices both as its context and
    // as fixed anchors.
    let ranges = kana_ranges(segments.reading());
    if ranges.is_empty() {
        report.skipped = Some(SelectionSkipped::NonKanaReading);
        return SelectionOutcome {
            segments: segments.clone(),
            report,
        };
    }
    let mut selected = segments.clone();
    // Lazy construction avoids any lattice work for globally or authoritatively
    // skipped input. Cache failure too: construction is attempted at most once.
    let mut lattice = None;
    for range in ranges {
        let prefix = prefix_surface(&selected, range.start);
        let run_context = suffix_context(&format!("{context}{prefix}"));
        let baseline_segments = related_actual_segments(&selected, range);
        let (next, mut run) = select_run(
            &selected,
            range,
            snapshot,
            learning,
            &run_context,
            language_model,
            segmenter,
            &mut lattice,
        );
        run.applied_segments = related_actual_segments(&next, range);
        run.switched = baseline_segments
            .iter()
            .map(|s| s.surface.as_str())
            .collect::<String>()
            != run
                .applied_segments
                .iter()
                .map(|s| s.surface.as_str())
                .collect::<String>();
        run.baseline_segments = baseline_segments;
        selected = next;
        report.runs.push(run);
    }
    SelectionOutcome {
        segments: selected,
        report,
    }
}

fn enumeration_failure(error: &muu_conversion::ConversionError) -> SelectionSkipped {
    if matches!(
        error,
        muu_conversion::ConversionError::FullHypothesisBudgetExceeded
    ) {
        SelectionSkipped::HypothesisEnumerationBudgetExceeded
    } else {
        SelectionSkipped::HypothesisEnumerationFailed
    }
}

/// Keep the guard order: complete registration, inseparable node, graph
/// preparation, no alternative after constraints, LM state, enumeration/
/// scoring, then application. Preparation never modifies the input (docs/src/internals/conversion.md「言語モデルによる選び直し」).
#[allow(clippy::too_many_arguments)]
fn select_run(
    segments: &Segments,
    range: ScalarRange,
    snapshot: &CostSnapshot<'_>,
    learning: &crate::personal_frequency::CapturedPersonalFrequency,
    context: &str,
    language_model: &LanguageModelSnapshot,
    segmenter: &Segmenter,
    lattice: &mut Option<Result<FullHypothesisLattice, muu_conversion::ConversionError>>,
) -> (Segments, SelectionRunReport) {
    let protected = segments
        .segments()
        .iter()
        .enumerate()
        .filter(|(_, s)| overlaps(s.reading_range(), range) && authoritative_segment(s))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let mut report = SelectionRunReport::empty(range);
    report.protected_segments = protected.clone();
    let mut keep = |reason| {
        report.skipped = Some(reason);
        (segments.clone(), report.clone())
    };
    let related = segments
        .segments()
        .iter()
        .filter(|s| overlaps(s.reading_range(), range))
        .count();
    if related == protected.len() {
        return keep(SelectionSkipped::AuthoritativeDictionaryBaseline);
    }
    if segments
        .segments()
        .iter()
        .any(|s| crosses_boundary(&s.focused().paths()[0], range))
    {
        return keep(SelectionSkipped::InseparableReadingBoundary);
    }
    let baseline = segments
        .segments()
        .iter()
        .flat_map(|s| s.focused().paths()[0].nodes.iter().cloned())
        .collect::<Vec<_>>();
    // Anchors: every node outside the run, plus every node of a protected
    // segment even where it lies inside the run. Protection freezes the whole
    // segment, particles included, not only the registered token.
    let fixed = segments
        .segments()
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            let protected = protected.contains(&i);
            s.focused().paths()[0]
                .nodes
                .iter()
                .filter(move |n| protected || n.end <= range.start || n.start >= range.end)
                .cloned()
        })
        .collect::<Vec<_>>();
    // The lattice and the reachability test come before the LM state check
    // so that "no alternative" outranks "model unavailable" (docs/src/internals/conversion.md「言語モデルによる選び直し」). The cost is
    // that a conversion without a usable model still builds the lattice once.
    let prepared = lattice.get_or_insert_with(|| {
        FullHypothesisLattice::build(segments.reading(), snapshot, HypothesisBudget::default())
    });
    let input = match prepared {
        Ok(l) => l,
        Err(e) => return keep(enumeration_failure(e)),
    };
    let graph = match input.constrain(&fixed) {
        Ok(g) => g,
        Err(e) => return keep(enumeration_failure(&e)),
    };
    let whole = range.start.get() == 0 && range.end.get() == segments.reading().chars().count();
    // NoAlternativeAfterConstraints concerns actual constraints. Preserve the
    // existing unregistered whole-kana one-item scoring behaviour (docs/src/internals/conversion.md「言語モデルによる選び直し」), where
    // no literal/registered catalog was filtered in the first place.
    if !whole || !protected.is_empty() {
        let baseline_surface = baseline
            .iter()
            .map(|n| n.surface.as_str())
            .collect::<String>()
            .nfc()
            .collect::<String>();
        match graph.has_alternative(&baseline_surface) {
            Ok(false) => return keep(SelectionSkipped::NoAlternativeAfterConstraints),
            Err(e) => return keep(enumeration_failure(&e)),
            Ok(true) => {}
        }
    }
    match language_model.state {
        LanguageModelState::Unloaded => return keep(SelectionSkipped::LanguageModelNotLoaded),
        LanguageModelState::Loading => return keep(SelectionSkipped::LanguageModelLoading),
        LanguageModelState::Failed => return keep(SelectionSkipped::LanguageModelFailed),
        LanguageModelState::Available => {}
    }
    let hypotheses = match graph.enumerate(&baseline, snapshot, muu_lm::MAX_SCORED_HYPOTHESES) {
        Ok(h) => h,
        Err(e) => return keep(enumeration_failure(&e)),
    };
    let Some(baseline_rank) = hypotheses.iter().position(|h| h.is_baseline) else {
        return keep(SelectionSkipped::BaselineNotFound);
    };
    let surfaces = hypotheses
        .iter()
        .map(|h| hypothesis_run_surface(h, range))
        .collect::<Vec<_>>();
    let refs = surfaces.iter().map(String::as_str).collect::<Vec<_>>();
    let reading = segments
        .reading()
        .chars()
        .skip(range.start.get())
        .take(range.len())
        .collect::<String>();
    let lm = language_model
        .model
        .as_ref()
        .expect("available model published before its state");
    let Ok(scores) = lm.score(context, &reading, &refs) else {
        return keep(SelectionSkipped::ScoringFailed);
    };
    // Bias real dictionary tokens once per hypothesis, using the same
    // decayed positive counts that built stage ①. Do not alter model scores
    // or consult a newer Core state while this conversion is in flight (docs/src/internals/conversion.md「変換が使うデータを固定する」).
    let biases = hypotheses
        .iter()
        .map(|h| learning.selection_bias(h, range))
        .collect::<Vec<_>>();
    let adjusted_scores = scores
        .iter()
        .zip(&biases)
        .map(|(s, b)| s - b)
        .collect::<Vec<_>>();
    let (chosen, excluded, vetoed) =
        choose_hypothesis_for(lm.kind(), &reading, baseline_rank, &refs, &adjusted_scores);
    report.excluded_reading_identity_ranks = excluded;
    report.reading_identity_vetoed_rank = vetoed;
    report.baseline_rank = Some(baseline_rank);
    report.selected_rank = Some(chosen);
    let selected = apply_choice(
        segments,
        range,
        &hypotheses[chosen],
        &protected,
        segmenter,
        snapshot,
        &mut report,
    );
    report.context_chars = context.chars().count().min(muu_lm::MAX_CONTEXT_CHARS);
    // Positive when some hypothesis beat the baseline, zero when the baseline
    // was best, non-finite when the baseline's own score was; None when no
    // score was finite at all.
    report.baseline_minus_best_score = adjusted_scores
        .iter()
        .copied()
        .filter(|s| s.is_finite())
        .min_by(f32::total_cmp)
        .map(|best| adjusted_scores[baseline_rank] - best);
    report.hypotheses = hypotheses
        .into_iter()
        .zip(surfaces)
        .zip(scores)
        .zip(biases.into_iter().zip(adjusted_scores))
        .map(
            |(((hypothesis, surface), score), (learning_bias, adjusted_score))| ScoredHypothesis {
                hypothesis,
                surface,
                score,
                learning_bias,
                adjusted_score,
            },
        )
        .collect();
    (selected, report)
}

/// Application is atomic. Keep the scores and proposed-identity veto facts,
/// but report the retained baseline if catalog reconstruction failed (docs/src/internals/conversion.md「言語モデルによる選び直し」).
fn apply_choice(
    segments: &Segments,
    range: ScalarRange,
    hypothesis: &FullHypothesis,
    protected: &[usize],
    segmenter: &Segmenter,
    snapshot: &CostSnapshot<'_>,
    report: &mut SelectionRunReport,
) -> Segments {
    if report.selected_rank == report.baseline_rank {
        return segments.clone();
    }
    match rebuild_run_from_hypothesis(segments, range, hypothesis, protected, segmenter, snapshot) {
        Ok(selected) => selected,
        Err(_) => {
            report.skipped = Some(SelectionSkipped::HypothesisApplicationFailed);
            report.selected_rank = report.baseline_rank;
            segments.clone()
        }
    }
}

fn hypothesis_run_surface(hypothesis: &FullHypothesis, range: ScalarRange) -> String {
    hypothesis
        .nodes
        .iter()
        .filter(|n| n.start >= range.start && n.end <= range.end)
        .map(|n| n.surface.as_str())
        .collect::<String>()
        .nfc()
        .collect()
}

// Maximal runs in scalar indices, the unit of path node bounds. The prolonged
// sound mark extends a run that has already started but never starts one, so
// a mark after katakana or Latin stays with that word and is not scored.
fn kana_ranges(reading: &str) -> Vec<ScalarRange> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (i, c) in reading.chars().enumerate() {
        if matches!(c, 'ぁ'..='ゖ') || (c == 'ー' && start.is_some()) {
            start.get_or_insert(i);
        } else if let Some(start) = start.take() {
            ranges.push(ScalarRange::new(
                ScalarIndex::new(start),
                ScalarIndex::new(i),
            ));
        }
    }
    if let Some(start) = start {
        ranges.push(ScalarRange::new(
            ScalarIndex::new(start),
            ScalarIndex::new(reading.chars().count()),
        ));
    }
    ranges
}

fn suffix_context(context: &str) -> String {
    // Normalize after joining context and preceding nodes: composition can
    // cross their boundaries. Cut the normalized text so the report counts
    // exactly the characters passed to the model (docs/src/internals/conversion.md「言語モデルによる選び直し」).
    let context = context.nfc().collect::<String>();
    let skip = context
        .chars()
        .count()
        .saturating_sub(muu_lm::MAX_CONTEXT_CHARS);
    context.chars().skip(skip).collect()
}

// The text a commit would produce before `end`: focused candidates' paths[0],
// which after an earlier run's rebuild is that run's selected path.
fn prefix_surface(segments: &Segments, end: ScalarIndex) -> String {
    segments
        .segments()
        .iter()
        .flat_map(|s| &s.focused().paths()[0].nodes)
        .filter(|n| n.end <= end)
        .map(|n| n.surface.as_str())
        .collect()
}

fn overlaps(a: ScalarRange, b: ScalarRange) -> bool {
    a.start < b.end && b.start < a.end
}

// Strict on both sides: a node that only touches an edge of the run lies
// wholly inside or outside it. Only a node with scalars on both sides cannot
// be frozen or varied as a unit, and splitting it would be a guess.
fn crosses_boundary(path: &PathCandidate, range: ScalarRange) -> bool {
    path.nodes.iter().any(|n| {
        let node_range = ScalarRange::new(n.start, n.end);
        node_range.crosses_boundary(range.start) || node_range.crosses_boundary(range.end)
    })
}

#[cfg(test)]
fn choose_hypothesis(
    reading: &str,
    baseline: usize,
    surfaces: &[&str],
    scores: &[f32],
) -> (usize, Vec<usize>, Option<usize>) {
    choose_hypothesis_for(muu_lm::ModelKind::Edge, reading, baseline, surfaces, scores)
}

// Returns (chosen rank, ranks whose surface equals the reading, vetoed rank).
// A hypothesis that just repeats the kana reading may be scored, but the
// policy is not allowed to switch to it.
fn choose_hypothesis_for(
    model: muu_lm::ModelKind,
    reading: &str,
    baseline: usize,
    surfaces: &[&str],
    scores: &[f32],
) -> (usize, Vec<usize>, Option<usize>) {
    let reading = reading.nfc().collect::<String>();
    let excluded = surfaces
        .iter()
        .enumerate()
        .filter(|(rank, surface)| *rank != baseline && surface.nfc().collect::<String>() == reading)
        .map(|(rank, _)| rank)
        .collect::<Vec<_>>();
    // The margin rule runs first and the reading-identity veto only judges
    // its proposal. Removing identity hypotheses beforehand would promote the
    // runner-up and could switch to it; a vetoed proposal keeps the baseline.
    let proposed = muu_lm::select_for(model, baseline, surfaces, scores);
    // Record causality at the decision point. Reconstructing it from the
    // minimum and margin would mistake nonfinite-baseline rejection for a
    // reading-identity veto.
    let vetoed = proposed.filter(|rank| excluded.contains(rank));
    let chosen = if vetoed.is_some() {
        baseline
    } else {
        proposed.unwrap_or(baseline)
    };
    (chosen, excluded, vetoed)
}

fn authoritative_segment(segment: &Segment) -> bool {
    // Only paths[0] is the input's actual candidate authority. A registered
    // token anywhere on it remains protected even when particles are joined;
    // latent scored paths must not redefine the input's registration (docs/src/internals/conversion.md「言語モデルによる選び直し」).
    segment.focused().paths()[0]
        .nodes
        .iter()
        .any(|node| authoritative_token(&node.token_key))
}

fn authoritative_token(key: &TokenKey) -> bool {
    matches!(
        key,
        TokenKey::Overlay {
            tier: LexiconOverlayTier::User
                | LexiconOverlayTier::Imported
                | LexiconOverlayTier::PostalCode,
            ..
        }
    )
}

#[cfg(test)]
#[path = "selection/tests.rs"]
mod migrated_tests;

#[cfg(test)]
mod tests {
    use super::*;

    // Existing selection-contract cases exercise an empty learning capture.
    // Learned policy tests call the production entry point with explicit counts.
    fn select(
        segments: &Segments,
        complete: bool,
        snapshot: &CostSnapshot<'_>,
        context: &str,
        lm: &LanguageModelSnapshot,
        segmenter: &Segmenter,
    ) -> SelectionOutcome {
        super::select(
            segments,
            complete,
            snapshot,
            &crate::personal_frequency::CapturedPersonalFrequency::new(
                &crate::personal_frequency::PersonalFrequencyModel::new(Default::default(), None)
                    .unwrap(),
            ),
            context,
            lm,
            segmenter,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn select_run(
        segments: &Segments,
        range: ScalarRange,
        snapshot: &CostSnapshot<'_>,
        context: &str,
        lm: &LanguageModelSnapshot,
        segmenter: &Segmenter,
        lattice: &mut Option<Result<FullHypothesisLattice, muu_conversion::ConversionError>>,
    ) -> (Segments, SelectionRunReport) {
        super::select_run(
            segments,
            range,
            snapshot,
            &crate::personal_frequency::CapturedPersonalFrequency::new(
                &crate::personal_frequency::PersonalFrequencyModel::new(Default::default(), None)
                    .unwrap(),
            ),
            context,
            lm,
            segmenter,
            lattice,
        )
    }
    use muu_conversion::Candidate;
    use muu_conversion::{NodeKind, PathNode, SegmentState, SnapshotRevision};
    use muu_dict::{DictionaryTokenId, LeftContextId, LexicalClass, PosId, RightContextId};

    fn path(literal: &str, literal_token: u64, word: &str, word_token: u64) -> PathCandidate {
        let node = |start, end, surface: &str, token| PathNode {
            start: ScalarIndex::new(start),
            end: ScalarIndex::new(end),
            surface: surface.into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: PosId::new(1),
            right_pos_id: PosId::new(1),
            lexical_class: LexicalClass::General,
            token_key: TokenKey::Dictionary(DictionaryTokenId::new(token)),
            word_cost: 0,
            kind: NodeKind::Known,
        };
        PathCandidate {
            nodes: vec![
                node(0, 1, literal, literal_token),
                node(1, 2, word, word_token),
            ],
            cost: 0,
            snapshot_revision: SnapshotRevision::new(1),
        }
    }

    // zenz switches at a margin of exactly 0.5 (2.51 vs 3.0 does not, 2.5
    // does), while edge's larger margin keeps the baseline for the same scores;
    // a reading-identity minimum is vetoed and keeps the baseline.
    #[test]
    fn zenz_margin_and_reading_identity_veto_use_the_actual_adjusted_scores() {
        let surfaces = ["制度", "精度", "せいど"];
        for (scores, selected, veto) in [
            ([3.0, 2.51, 8.0], 0, None),
            ([3.0, 2.5, 8.0], 1, None),
            ([3.0, 2.5, 2.0], 0, Some(2)),
        ] {
            let (chosen, excluded, vetoed) =
                choose_hypothesis_for(muu_lm::ModelKind::Zenz, "せいど", 0, &surfaces, &scores);
            assert_eq!(chosen, selected);
            assert_eq!(excluded, [2]);
            assert_eq!(vetoed, veto);
        }
        assert_eq!(
            choose_hypothesis_for(
                muu_lm::ModelKind::Edge,
                "せいど",
                0,
                &surfaces,
                &[3.0, 2.5, 8.0]
            )
            .0,
            0
        );
    }

    #[test]
    fn reported_paths_keep_original_scalar_and_token_authority_under_constraints() {
        // The paths below carry their own scalar/token authority. A small
        // dictionary supplies costs and alternatives without real vocabulary.
        let file = crate::test_support::multi_segment_dictionary();
        let dictionary = muu_dict::Dictionary::open(file.path()).unwrap();
        let costs = CostSnapshot::baseline(&dictionary);
        let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2));
        let mut baseline = path("a", 0, "甲", 2);
        baseline.snapshot_revision = costs.revision();
        let original = Segments::new(
            "aあ".into(),
            vec![
                Segment::new(
                    range,
                    vec![Candidate::from_group(range, vec![baseline.clone()]).unwrap()],
                    0,
                    SegmentState::Unconfirmed,
                )
                .unwrap(),
            ],
            0,
        )
        .unwrap();
        let run = ScalarRange::new(ScalarIndex::new(1), ScalarIndex::new(2));
        let lattice =
            FullHypothesisLattice::build(original.reading(), &costs, HypothesisBudget::default())
                .unwrap();
        let hypotheses = lattice
            .constrain(&baseline.nodes[..1])
            .unwrap()
            .enumerate(&baseline.nodes, &costs, 16)
            .unwrap();
        let mut report = SelectionRunReport::empty(run);
        report.baseline_segments = related_actual_segments(&original, run);
        report.hypotheses = hypotheses
            .into_iter()
            .map(|h| ScoredHypothesis {
                surface: hypothesis_run_surface(&h, run),
                hypothesis: h,
                score: 0.0,
                learning_bias: 0.0,
                adjusted_score: 0.0,
            })
            .collect();
        let wire = serde_json::to_value(crate::recording::RecordedSelectionReport::from_report(
            &SelectionReport {
                format_version: 3,
                runs: vec![report.clone()],
                rules_version: 3,
                skipped: None,
            },
        ))
        .unwrap();
        for (h, encoded) in report
            .hypotheses
            .iter()
            .zip(wire["runs"][0]["hypotheses"].as_array().unwrap())
        {
            assert_eq!(h.hypothesis.nodes[0], baseline.nodes[0]);
            assert!(encoded.get("selections").is_none());
            let nodes = encoded["path"]["nodes"].as_array().unwrap();
            assert_eq!(nodes.len(), 1);
            assert_eq!(
                nodes[0],
                serde_json::to_value(crate::commit_stream::recorded_node(&h.hypothesis.nodes[1]))
                    .unwrap()
            );
            assert_eq!(
                nodes[0]["reading_range"],
                serde_json::json!({"start":1,"end":2})
            );
        }
    }

    #[test]
    fn prolonged_marks_only_continue_hiragana_runs() {
        let bounds = |reading| {
            kana_ranges(reading)
                .iter()
                .map(|r| (r.start.get(), r.end.get()))
                .collect::<Vec<_>>()
        };
        for reading in ["ー", "aー", "1ー", "コーヒー"] {
            assert!(bounds(reading).is_empty(), "{reading}");
        }
        assert_eq!(bounds("コーヒーをのむ"), vec![(4, 7)]);
        assert_eq!(bounds("ーあ"), vec![(1, 2)]);
        assert_eq!(bounds("らーめん"), vec![(0, 4)]);
        assert_eq!(bounds("こーーひー"), vec![(0, 5)]);
    }

    #[test]
    fn normalization_composes_across_nodes_and_context_before_cutting() {
        let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2));
        let joined = path("か", 0, "\u{3099}", 1);
        let hypothesis = FullHypothesis {
            surface: "が".into(),
            base_cost: 0,
            nodes: joined.nodes,
            is_baseline: true,
            snapshot_revision: SnapshotRevision::new(1),
        };
        assert_eq!(hypothesis_run_surface(&hypothesis, range), "が");
        let context = format!("{}か{}", "前".repeat(40), "\u{3099}");
        assert_eq!(suffix_context(&context), format!("{}が", "前".repeat(39)));
        assert_eq!(suffix_context(&context).chars().count(), 40);
    }

    #[test]
    fn failed_path_materialization_reports_baseline_and_keeps_scored_facts() {
        // Materialization must reject the missing literal node regardless of
        // production vocabulary; the scored facts are supplied explicitly.
        let file = crate::test_support::multi_segment_dictionary();
        let dictionary = muu_dict::Dictionary::open(file.path()).unwrap();
        let costs = CostSnapshot::baseline(&dictionary);
        let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2));
        let mut baseline = path("a", 0, "甲", 1);
        baseline.snapshot_revision = costs.revision();
        let original = Segments::new(
            "aあ".into(),
            vec![
                Segment::new(
                    range,
                    vec![Candidate::from_group(range, vec![baseline.clone()]).unwrap()],
                    0,
                    SegmentState::Unconfirmed,
                )
                .unwrap(),
            ],
            0,
        )
        .unwrap();
        let mut chosen = baseline.clone();
        chosen.nodes.remove(0);
        let hypothesis = FullHypothesis {
            surface: "乙".into(),
            base_cost: 0,
            nodes: chosen.nodes,
            is_baseline: false,
            snapshot_revision: costs.revision(),
        };
        let mut report = SelectionRunReport::empty(range);
        report.baseline_rank = Some(0);
        report.selected_rank = Some(1);
        report.hypotheses = vec![
            ScoredHypothesis {
                surface: "a甲".into(),
                score: 12.0,
                learning_bias: 0.0,
                adjusted_score: 12.0,
                hypothesis: FullHypothesis {
                    surface: "a甲".into(),
                    base_cost: 0,
                    nodes: baseline.nodes,
                    is_baseline: true,
                    snapshot_revision: costs.revision(),
                },
            },
            ScoredHypothesis {
                surface: "乙".into(),
                score: 8.0,
                learning_bias: 0.0,
                adjusted_score: 8.0,
                hypothesis: hypothesis.clone(),
            },
        ];
        let selected = apply_choice(
            &original,
            range,
            &hypothesis,
            &[],
            &Segmenter::new(vec![], muu_conversion::BoundaryDecision::Break),
            &costs,
            &mut report,
        );
        assert_eq!(selected, original);
        assert_eq!(report.selected_rank, Some(0));
        assert_eq!(
            report.skipped,
            Some(SelectionSkipped::HypothesisApplicationFailed)
        );
        assert_eq!(report.hypotheses.len(), 2);
        assert_eq!(report.hypotheses[1].score, 8.0);
    }

    #[test]
    fn any_registered_token_on_paths_zero_protects_joined_segments_of_every_tier() {
        // Whole-run registration skips before lattice/model work, so only a
        // valid immutable cost fixture is needed for all three overlay tiers.
        let file = crate::test_support::multi_segment_dictionary();
        let dictionary = muu_dict::Dictionary::open(file.path()).unwrap();
        let costs = CostSnapshot::baseline(&dictionary);
        let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2));
        for tier in [
            LexiconOverlayTier::User,
            LexiconOverlayTier::Imported,
            LexiconOverlayTier::PostalCode,
        ] {
            let mut registered = path("登録語", 0, "は", 1);
            registered.nodes[0].token_key = TokenKey::Overlay {
                tier,
                dictionary_key: 0,
                entry_id: 0,
            };
            let plain = path("普通語", 2, "は", 1);
            let segment = Segment::new(
                range,
                vec![
                    Candidate::from_group(range, vec![registered]).unwrap(),
                    Candidate::from_group(range, vec![plain]).unwrap(),
                ],
                0,
                SegmentState::Unconfirmed,
            )
            .unwrap();
            assert!(authoritative_segment(&segment), "{tier:?}");
            let input = Segments::new("あい".into(), vec![segment], 0).unwrap();
            let selected = select(
                &input,
                true,
                &costs,
                "",
                &LanguageModelSnapshot {
                    state: LanguageModelState::Unloaded,
                    model: None,
                    info: crate::LanguageModelInfo::default(),
                },
                &Segmenter::new(vec![], muu_conversion::BoundaryDecision::Break),
            );
            assert_eq!(selected.segments, input);
            assert_eq!(selected.report.runs[0].protected_segments, vec![0]);
            assert_eq!(
                selected.report.runs[0].skipped,
                Some(SelectionSkipped::AuthoritativeDictionaryBaseline)
            );
        }
        // A latent path alone does not redefine the input's registration.
        let plain = path("語", 0, "は", 1);
        let mut latent = plain.clone();
        latent.nodes[0].token_key = TokenKey::Overlay {
            tier: LexiconOverlayTier::User,
            dictionary_key: 0,
            entry_id: 0,
        };
        let segment = Segment::new(
            range,
            vec![Candidate::from_group(range, vec![plain, latent]).unwrap()],
            0,
            SegmentState::Unconfirmed,
        )
        .unwrap();
        assert!(!authoritative_segment(&segment));
    }
    #[test]
    fn reading_identity_minimum_keeps_baseline_without_trying_next_best() {
        let scores = [12.0, 4.0, 6.0];
        assert_eq!(
            choose_hypothesis("くも", 0, &["雲", "くも", "蜘蛛"], &scores),
            (0, vec![1], Some(1))
        );
        assert_eq!(scores, [12.0, 4.0, 6.0]);
    }

    #[test]
    fn reading_identity_baseline_can_switch_and_nonminimum_identity_is_reported() {
        assert_eq!(
            choose_hypothesis("くも", 0, &["くも", "雲"], &[12.0, 8.0]),
            (1, vec![], None)
        );
        assert_eq!(
            choose_hypothesis("くも", 0, &["雲", "蜘蛛", "くも"], &[12.0, 8.0, 10.0]),
            (1, vec![2], None)
        );
    }

    #[test]
    fn reading_identity_uses_nfc_equality() {
        assert_eq!(
            choose_hypothesis("か\u{3099}", 0, &["蛾", "が"], &[12.0, 8.0]),
            (0, vec![1], Some(1))
        );
    }

    #[test]
    fn reading_identity_veto_requires_a_low_level_switch_proposal() {
        for scores in [
            [f32::INFINITY, 8.0],
            [f32::NEG_INFINITY, 8.0],
            [f32::NAN, 8.0],
            [9.0, 8.0],
            [8.0, 12.0],
            [8.0, 8.0],
        ] {
            assert_eq!(
                choose_hypothesis("くも", 0, &["雲", "くも"], &scores),
                (0, vec![1], None)
            );
        }
    }

    // On a tie the first rank is proposed; the veto then judges that proposal
    // only, so a tie led by the reading identity keeps the baseline instead of
    // falling to the other tied hypothesis.
    #[test]
    fn tied_finite_minima_use_first_rank_before_the_reading_identity_veto() {
        assert_eq!(
            choose_hypothesis("くも", 0, &["雲", "くも", "蜘蛛"], &[12.0, 8.0, 8.0]),
            (0, vec![1], Some(1))
        );
        assert_eq!(
            choose_hypothesis("くも", 0, &["雲", "蜘蛛", "くも"], &[12.0, 8.0, 8.0]),
            (1, vec![2], None)
        );
    }

    #[test]
    #[ignore = "requires public edge (check-assets)"]
    fn result_copy_budget_failure_keeps_the_run_and_does_not_score() {
        let Some(model) = crate::real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
            return;
        };
        use muu_dict::{
            DictionaryBuilder, LeftContextId, LexiconEntry, PosId, RightContextId, UnknownClass,
            UnknownDefinition,
        };
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"result-budget-test\"\n");
        builder.add_unknown(UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 1000,
        });
        builder.add_entry(LexiconEntry {
            reading: "あ".into(),
            surface: "x".repeat(8192),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: -1,
        });
        let dictionary = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(dictionary.path(), builder.build().unwrap()).unwrap();
        let core = crate::Core::new(crate::CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap();
        core.configure_sessions(crate::CoreSessionSettings {
            conversion_lm_dir: Some(model),
            conversion_lm_load_synchronously: true,
            ..Default::default()
        })
        .unwrap_or_else(|_| panic!("first settings"));
        let captured = core.conversion_snapshot();
        let generated = crate::generate_conversion("あ", &captured, &core.segmenter).unwrap();
        assert!(generated.path_evidence_complete());
        let costs = captured.costs().unwrap();
        // Same long-surface regression as the conversion-layer test. The
        // prepared graph fits; materializing real paths exceeds this limit.
        let lattice = FullHypothesisLattice::build(
            "あ",
            &costs,
            HypothesisBudget {
                max_bytes: 45_112,
                ..Default::default()
            },
        )
        .unwrap();
        let mut input = Some(Ok(lattice));
        let (kept, report) = select_run(
            generated.segments(),
            ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(1)),
            &costs,
            "",
            &core.language_model(),
            &core.segmenter,
            &mut input,
        );
        assert_eq!(
            report.skipped,
            Some(SelectionSkipped::HypothesisEnumerationBudgetExceeded)
        );
        assert!(report.hypotheses.is_empty());
        assert!(report.selected_rank.is_none());
        assert_eq!(kept, *generated.segments());
        assert!(!core.is_invalid());
    }

    #[test]
    #[ignore = "requires public dictionary, edge (check-assets)"]
    fn a_budget_failure_keeps_one_run_and_does_not_stop_the_next_run() {
        let (Some(dictionary), Some(model)) = (
            crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
            crate::real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
        ) else {
            return;
        };
        let core = crate::Core::new(crate::CoreConfig {
            dictionary_path: Some(&dictionary),
        })
        .unwrap();
        core.configure_sessions(crate::CoreSessionSettings {
            conversion_lm_dir: Some(model),
            conversion_lm_load_synchronously: true,
            ..Default::default()
        })
        .unwrap_or_else(|_| panic!("first settings"));
        let captured = core.conversion_snapshot();
        let reading = "へんかんせいどがわるいにほんごをべんきょうするきょうはあめです、きょう";
        let generated = crate::generate_conversion(reading, &captured, &core.segmenter).unwrap();
        assert!(generated.path_evidence_complete());
        let costs = captured.costs().unwrap();
        let ranges = kana_ranges(reading);
        assert_eq!(ranges.len(), 2);
        let lm = core.language_model();
        let mut found = false;
        // Tune a deterministic work-count limit, never an elapsed-time limit.
        // The large free run exceeds a limit that the short constrained run
        // can satisfy; both calls use the same already constructed lattice.
        for steps in [32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384] {
            let lattice = FullHypothesisLattice::build(
                reading,
                &costs,
                HypothesisBudget {
                    max_enumeration_steps: steps,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut input = Some(Ok(lattice));
            let (kept, first) = select_run(
                generated.segments(),
                ranges[0],
                &costs,
                "",
                &lm,
                &core.segmenter,
                &mut input,
            );
            let (_, next) = select_run(
                &kept,
                ranges[1],
                &costs,
                "",
                &lm,
                &core.segmenter,
                &mut input,
            );
            if first.skipped == Some(SelectionSkipped::HypothesisEnumerationBudgetExceeded)
                && next.skipped.is_none()
            {
                assert_eq!(kept, *generated.segments());
                assert!(first.hypotheses.is_empty());
                assert!(!next.hypotheses.is_empty());
                found = true;
                break;
            }
        }
        assert!(
            found,
            "work budget must isolate the dense run without cancelling the short run"
        );
    }
}

#[cfg(test)]
mod learning_bias_tests {
    use super::*;
    use crate::personal_frequency::{CapturedPersonalFrequency, PersonalFrequencyModel};
    use crate::test_support::*;
    #[test]
    #[ignore = "requires public dictionary, edge (check-assets)"]
    fn bias_uses_run_dictionary_tokens_once_then_margin_and_reading_identity() {
        let Some(core) = run_selection_core() else {
            return;
        };
        let captured = core.conversion_snapshot();
        let generated =
            crate::session::generate_conversion("くも", &captured, &core.segmenter).unwrap();
        let s = &generated.segments().segments()[0];
        let paths = ["雲", "蜘蛛", "くも"].map(|text| {
            s.candidates()
                .iter()
                .find(|c| c.surface() == text)
                .unwrap()
                .paths()[0]
                .clone()
        });
        let hypotheses = paths
            .iter()
            .enumerate()
            .map(|(i, p)| FullHypothesis {
                surface: ["雲", "蜘蛛", "くも"][i].into(),
                base_cost: i64::from(p.cost),
                nodes: p.nodes.clone(),
                is_baseline: i == 0,
                snapshot_revision: captured.costs().unwrap().revision(),
            })
            .collect::<Vec<_>>();
        let ids = paths[1]
            .token_keys()
            .filter_map(|k| match k {
                TokenKey::Dictionary(id) => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 1);
        let fp = core.dictionary.as_ref().unwrap().fingerprint();
        let mut model = PersonalFrequencyModel::new(Default::default(), Some(fp)).unwrap();
        for _ in 0..3 {
            model.observe_explicit_selection(&ids, fp);
        }
        let learning = CapturedPersonalFrequency::new(&model);
        let range = ScalarRange {
            start: ScalarIndex::new(0),
            end: ScalarIndex::new(2),
        };
        let biases = hypotheses
            .iter()
            .map(|h| learning.selection_bias(h, range))
            .collect::<Vec<_>>();
        assert_eq!(biases, vec![0., 2., 0.]);
        for (raw, expected) in [([10., 10., 15.], 1), ([10., 10.1, 15.], 0)] {
            let adjusted = raw
                .iter()
                .zip(&biases)
                .map(|(s, b)| s - b)
                .collect::<Vec<_>>();
            assert_eq!(
                choose_hypothesis("くも", 0, &["雲", "蜘蛛", "くも"], &adjusted).0,
                expected
            );
        }
        let mut repeated = hypotheses[1].clone();
        repeated.nodes.extend(hypotheses[1].nodes.clone());
        assert_eq!(learning.selection_bias(&repeated, range), 2.);
        let outside = ScalarRange {
            start: ScalarIndex::new(2),
            end: ScalarIndex::new(3),
        };
        assert_eq!(learning.selection_bias(&repeated, outside), 0.);
        // The reading-identity veto sees the adjusted ranking, even when a learned
        // token makes the unchanged-reading surface the model's clear favorite.
        assert_eq!(
            choose_hypothesis("くも", 0, &["雲", "蜘蛛", "くも"], &[12., 15., 6.]),
            (0, vec![2], Some(2))
        );
    }
}
