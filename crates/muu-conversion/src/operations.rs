use crate::{
    BoundaryContext, ConversionError, CostSnapshot, GraphemeBoundaries, PathCandidate, ScalarIndex,
    ScalarRange, Segment, SegmentState, SegmentedPathGroup, Segmenter, Segments,
    best_path_with_required_boundary, candidates_from_paths, local_search,
    local_search_surface_dominant, viterbi_1best,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionOutcome {
    segments: Segments,
    path_evidence_complete: bool,
}

impl ConversionOutcome {
    pub fn segments(&self) -> &Segments {
        &self.segments
    }

    pub fn into_segments(self) -> Segments {
        self.segments
    }

    pub const fn path_evidence_complete(&self) -> bool {
        self.path_evidence_complete
    }
}

#[derive(Clone, Copy)]
enum LocalSearchPolicy {
    PreservePathEvidence,
    PreservePathEvidenceOrSurfaceFallback,
}

pub fn convert_to_segments(
    reading: &str,
    segmenter: &Segmenter,
    snapshot: &CostSnapshot<'_>,
) -> Result<Segments, ConversionError> {
    convert_to_segments_impl(
        reading,
        segmenter,
        snapshot,
        LocalSearchPolicy::PreservePathEvidence,
    )
    .map(ConversionOutcome::into_segments)
}

/// Converts a reading while keeping ordinary candidate availability independent
/// from exhaustive alternate-path evidence collection.
///
/// Exact path evidence remains the fast path. If that bounded search reaches a
/// fail-safe resource limit, conversion is retried with exact visible-surface
/// dominance and the outcome tells the caller not to run language-model selection.
pub fn convert_to_segments_resilient(
    reading: &str,
    segmenter: &Segmenter,
    snapshot: &CostSnapshot<'_>,
) -> Result<ConversionOutcome, ConversionError> {
    convert_to_segments_impl(
        reading,
        segmenter,
        snapshot,
        LocalSearchPolicy::PreservePathEvidenceOrSurfaceFallback,
    )
}

// Stage ①: Viterbi over the whole reading fixes the segment boundaries,
// then each segment's candidates come from a local search of its span
// between the Viterbi neighbours' context IDs. The first candidate is the
// cheapest in that context. That is usually the Viterbi surface, but the
// span's own lattice can hold nodes the whole-reading lattice lacked
// (symbols for the whole span, unknown runs cut at the span end), and ties
// break differently (see `compare_paths`). Every segment starts focused on
// candidate 0.
fn convert_to_segments_impl(
    reading: &str,
    segmenter: &Segmenter,
    snapshot: &CostSnapshot<'_>,
    search_policy: LocalSearchPolicy,
) -> Result<ConversionOutcome, ConversionError> {
    if reading.is_empty() {
        return Segments::new(String::new(), Vec::new(), 0)
            .map(|segments| ConversionOutcome {
                segments,
                path_evidence_complete: true,
            })
            .map_err(Into::into);
    }
    let lattice =
        crate::Lattice::build_with_overlays(reading, snapshot.dictionary(), snapshot.overlays())?;
    let best = viterbi_1best(&lattice, snapshot)?;
    let segmented = segmenter.segment(&best);
    let mut segments = Vec::with_capacity(segmented.groups.len());
    let mut path_evidence_complete = true;
    for (index, group) in segmented.groups.iter().enumerate() {
        let boundary = boundary_from_segmented_path(&segmented.groups, index, snapshot)?;
        let span = reading_slice(reading, group.reading_range)?;
        let local_paths = match search_policy {
            LocalSearchPolicy::PreservePathEvidence => local_search(span, &boundary, snapshot)?,
            LocalSearchPolicy::PreservePathEvidenceOrSurfaceFallback => {
                match local_search(span, &boundary, snapshot) {
                    Ok(paths) => paths,
                    Err(ConversionError::SearchBudgetExceeded { .. }) => {
                        path_evidence_complete = false;
                        local_search_surface_dominant(span, &boundary, snapshot)?
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        // Local search runs on the span alone; move its node ranges back
        // into whole-reading coordinates.
        let paths = shift_paths(local_paths, group.reading_range.start);
        let candidates = candidates_from_paths(group.reading_range, paths)?;
        segments.push(Segment::new(
            group.reading_range,
            candidates,
            0,
            SegmentState::Unconfirmed,
        )?);
    }
    Segments::new(reading.to_owned(), segments, 0)
        .map(|segments| ConversionOutcome {
            segments,
            path_evidence_complete,
        })
        .map_err(Into::into)
}

pub fn move_segment_focus(
    current: &Segments,
    focused_segment: usize,
) -> Result<Segments, ConversionError> {
    // Navigation must preserve LM and manual choices (docs/src/internals/conversion.md「文節の移動と伸縮」). Regenerating the
    // destination would silently reset its choice merely by visiting it.
    //
    // What is given up: regeneration also re-ranked the destination's
    // candidates against its current neighbours. The list now keeps the order
    // it had when the segment was last built (conversion, resize, or a run
    // rebuild), even if a neighbour has changed since.
    Segments::new(
        current.reading().to_owned(),
        current.segments().to_vec(),
        focused_segment,
    )
    .map_err(Into::into)
}

/// Materialize one selected run from its real path (docs/src/internals/conversion.md「替えたあと」). Construct everything
/// first: a local-search failure cannot leave half a run installed, and every
/// preserved segment retains its candidate catalog and selected path.
pub fn rebuild_run_from_hypothesis(
    current: &Segments,
    range: ScalarRange,
    selected: &crate::FullHypothesis,
    protected: &[usize],
    segmenter: &Segmenter,
    snapshot: &CostSnapshot<'_>,
) -> Result<Segments, ConversionError> {
    // The hypothesis must come from the same snapshot as the segments, and a
    // baseline hypothesis must be exactly the current representative path;
    // anything else is refused rather than applied approximately.
    let reading_len = current.reading().chars().count();
    if range.start >= range.end
        || range.end.get() > reading_len
        || selected.snapshot_revision != snapshot.revision()
        || protected.iter().any(|&i| i >= current.segments().len())
    {
        return Err(ConversionError::NoConstrainedPath);
    }
    let baseline = current
        .segments()
        .iter()
        .map(representative)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|p| p.nodes.iter().cloned())
        .collect::<Vec<_>>();
    if selected.is_baseline {
        return if selected.nodes == baseline {
            Ok(current.clone())
        } else {
            Err(ConversionError::NoConstrainedPath)
        };
    }
    path_span(
        &selected.nodes,
        ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(reading_len)),
    )?;
    // Protect actual token/path authority, not merely equal visible strings.
    // Literal neighbours and registered segments must survive exact, including
    // their POS endpoints and word costs used by subsequent conversions.
    for (index, segment) in current.segments().iter().enumerate() {
        for node in &representative(segment)?.nodes {
            if (protected.contains(&index) || node.end <= range.start || node.start >= range.end)
                && !selected.nodes.iter().any(|chosen| chosen == node)
            {
                return Err(ConversionError::NoConstrainedPath);
            }
            if crosses_run_boundary(node, range) {
                return Err(ConversionError::NoConstrainedPath);
            }
        }
    }
    let related = current
        .segments()
        .iter()
        .enumerate()
        .filter(|(_, s)| s.reading_range().start < range.end && range.start < s.reading_range().end)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let first = *related.first().ok_or(ConversionError::NoConstrainedPath)?;
    let last = *related.last().ok_or(ConversionError::NoConstrainedPath)?;
    let mut replacements = Vec::new();
    let mut cursor = current.segments()[first].reading_range().start;
    // A registered segment is a hard boundary even if the POS rules would
    // otherwise join it to a newly selected particle. Registration protects
    // its full original range as well as its nodes (docs/src/internals/conversion.md「替えたあと」).
    for index in first..=last {
        if !protected.contains(&index) {
            continue;
        }
        let frozen = &current.segments()[index];
        if cursor < frozen.reading_range().start {
            replacements.extend(materialize_island(
                current,
                &selected.nodes,
                ScalarRange::new(cursor, frozen.reading_range().start),
                range,
                segmenter,
                snapshot,
            )?);
        }
        replacements.push(frozen.clone());
        cursor = frozen.reading_range().end;
    }
    let end = current.segments()[last].reading_range().end;
    if cursor < end {
        replacements.extend(materialize_island(
            current,
            &selected.nodes,
            ScalarRange::new(cursor, end),
            range,
            segmenter,
            snapshot,
        )?);
    }
    // Every replacement was built above; only now is the input touched, so
    // an error anywhere leaves the caller with its original segments.
    let mut output = current.segments().to_vec();
    output.splice(first..=last, replacements);
    // Segment boundaries may have moved, so the old index means nothing.
    // Focus the new segment that covers where the old focus began.
    let focus_start = current
        .focused()
        .ok_or(ConversionError::NoConstrainedPath)?
        .reading_range()
        .start;
    let focus = output
        .iter()
        .position(|s| s.reading_range().start <= focus_start && focus_start < s.reading_range().end)
        .ok_or(ConversionError::NoConstrainedPath)?;
    Segments::new(current.reading().to_owned(), output, focus).map_err(Into::into)
}

fn crosses_run_boundary(node: &crate::PathNode, range: ScalarRange) -> bool {
    let node_range = ScalarRange::new(node.start, node.end);
    node_range.crosses_boundary(range.start) || node_range.crosses_boundary(range.end)
}

fn path_span(
    nodes: &[crate::PathNode],
    range: ScalarRange,
) -> Result<Vec<crate::PathNode>, ConversionError> {
    let span = nodes
        .iter()
        .filter(|n| n.start >= range.start && n.end <= range.end)
        .cloned()
        .collect::<Vec<_>>();
    if span.first().is_none_or(|n| n.start != range.start)
        || span.last().is_none_or(|n| n.end != range.end)
        || span.windows(2).any(|p| p[0].end != p[1].start)
    {
        return Err(ConversionError::NoConstrainedPath);
    }
    Ok(span)
}

// Rebuilds the segments of one stretch of the selected path between frozen
// segments. Each group's catalog is a local search of its span, kept only
// where it reproduces the group's nodes outside the run exactly; the
// selected path itself is always installed as the first candidate.
fn materialize_island(
    current: &Segments,
    whole_path: &[crate::PathNode],
    island: ScalarRange,
    run: ScalarRange,
    segmenter: &Segmenter,
    snapshot: &CostSnapshot<'_>,
) -> Result<Vec<Segment>, ConversionError> {
    let span = path_span(whole_path, island)?;
    let mut groups = segmenter.segment_nodes(&span).groups;
    // A mixed original segment belongs to this rebuild, but its literal
    // fragment must remain attached to the adjacent run group. No node is
    // split at a guessed character/surface offset (docs/src/internals/conversion.md「替えたあと」).
    while groups.len() > 1 && groups[0].reading_range.end <= run.start {
        join_groups(&mut groups, 0);
    }
    while groups.len() > 1 && groups.last().unwrap().reading_range.start >= run.end {
        let index = groups.len() - 2;
        join_groups(&mut groups, index);
    }
    let mut output = Vec::new();
    for group in groups {
        let first = whole_path
            .iter()
            .position(|n| n.start == group.reading_range.start)
            .ok_or(ConversionError::NoConstrainedPath)?;
        let last = first + group.nodes.len() - 1;
        let previous = first.checked_sub(1).map(|i| &whole_path[i]);
        let next = whole_path.get(last + 1);
        let boundary = BoundaryContext {
            prev_right_id: previous.map_or(snapshot.dictionary().bos_right_id(), |n| n.right_id),
            next_left_id: next.map_or(snapshot.dictionary().eos_left_id(), |n| n.left_id),
        };
        let paths = shift_paths(
            local_search(
                reading_slice(current.reading(), group.reading_range)?,
                &boundary,
                snapshot,
            )?,
            group.reading_range.start,
        );
        let fixed = group
            .nodes
            .iter()
            .filter(|n| n.end <= run.start || n.start >= run.end)
            .collect::<Vec<_>>();
        let paths = paths
            .into_iter()
            .filter(|p| {
                !p.nodes.iter().any(|n| crosses_run_boundary(n, run))
                    && fixed
                        .iter()
                        .all(|fixed| p.nodes.iter().any(|n| n == *fixed))
                    && p.nodes
                        .iter()
                        .filter(|n| n.end <= run.start || n.start >= run.end)
                        .count()
                        == fixed.len()
            })
            .collect();
        // The selected path becomes its own candidate, so its ID is derived
        // from this path. That can differ from the ID a resize would give the
        // same surface, which is derived from the cheapest local path.
        let chosen = PathCandidate {
            cost: cost_with_boundary(&group.nodes, &boundary, snapshot)?,
            nodes: group.nodes,
            snapshot_revision: snapshot.revision(),
        };
        let (candidates, focus) = crate::local::candidates_with_selected_path(
            group.reading_range,
            paths,
            chosen,
            // A boundary change has no corresponding input candidate. Do
            // not invent one by joining or splitting its former neighbours.
            //
            // Every group goes through here, including unchanged surfaces.
            // If the selected and input surfaces agree, the selected candidate
            // is already first: do not add a second copy even if their paths
            // differ. The catalog's current representative remains authoritative.
            current
                .segments()
                .iter()
                .find(|segment| segment.reading_range() == group.reading_range)
                .map(|segment| segment.focused().surface()),
        )?;
        output.push(Segment::new(
            group.reading_range,
            candidates,
            focus,
            SegmentState::Unconfirmed,
        )?);
    }
    Ok(output)
}

fn join_groups(groups: &mut Vec<SegmentedPathGroup>, index: usize) {
    let right = groups.remove(index + 1);
    groups[index].reading_range.end = right.reading_range.end;
    groups[index].nodes.extend(right.nodes);
}

fn cost_with_boundary(
    nodes: &[crate::PathNode],
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
) -> Result<i32, ConversionError> {
    let mut total = 0i32;
    let mut right = boundary.prev_right_id;
    for node in nodes {
        let edge = snapshot.transition_cost(right, node.left_id)?;
        let word = snapshot.path_node_cost_breakdown(node)?.total;
        total = total
            .checked_add(edge)
            .and_then(|v| v.checked_add(word))
            .ok_or(ConversionError::CostOverflow)?;
        right = node.right_id;
    }
    total
        .checked_add(snapshot.transition_cost(right, boundary.next_left_id)?)
        .ok_or(ConversionError::CostOverflow)
}

/// Repartitions the minimal segment span around the requested focused length.
///
/// Requesting the current length is a successful no-op. Zero, a boundary past
/// the reading, or a scalar offset inside a grapheme returns
/// [`ConversionError::InvalidSegmentLength`]. A valid boundary that cannot be
/// represented by the current lattice returns
/// [`ConversionError::NoConstrainedPath`] rather than a successful no-op.
pub fn resize_focused_segment(
    current: &Segments,
    requested_length: usize,
    snapshot: &CostSnapshot<'_>,
) -> Result<Segments, ConversionError> {
    let focused_index = current.focused_segment();
    let mut segments = current.segments().to_vec();
    let focused_range = segments
        .get(focused_index)
        .ok_or(crate::SegmentsError::FocusedSegmentOutOfRange)?
        .reading_range();
    let new_end_value = focused_range
        .start
        .get()
        .checked_add(requested_length)
        .ok_or(ConversionError::InvalidSegmentLength)?;
    let reading_end = current.reading().chars().count();
    if requested_length == 0 || new_end_value > reading_end {
        return Err(ConversionError::InvalidSegmentLength);
    }
    let new_end = ScalarIndex::new(new_end_value);
    let boundaries = GraphemeBoundaries::new(current.reading());
    if !boundaries.is_grapheme_boundary(new_end) {
        return Err(ConversionError::InvalidSegmentLength);
    }
    if new_end == focused_range.end {
        return Ok(current.clone());
    }

    // Growing to exactly the end of a later segment merges the span into one
    // segment. Otherwise the span splits at the requested boundary: the
    // constrained 1-best fixes the left context, the right part's cheapest
    // candidate fixes the focused segment's right context, and both get
    // fresh cost-ordered catalogs. Shrinking the last segment creates a new
    // segment from its tail.
    //
    // Repartition only the minimal span needed to establish both sides of the
    // requested boundary. A following segment is included when available so
    // its candidates inherit the focused segment's new right context. Segments
    // before the focus and after this span remain byte-for-byte unchanged.
    let minimum_last = (focused_index + 1).min(segments.len() - 1);
    let boundary_container = segments
        .iter()
        .enumerate()
        .skip(focused_index)
        .find(|(_, segment)| segment.reading_range().end > new_end)
        .map_or(segments.len() - 1, |(index, _)| index);
    let affected_last = minimum_last.max(boundary_container);
    let affected_end = segments[affected_last].reading_range().end;
    let combined_range = ScalarRange::new(focused_range.start, affected_end);
    let outer_boundary = boundary_around_span(&segments, focused_index, affected_last, snapshot)?;
    let new_focused_range = ScalarRange::new(focused_range.start, new_end);

    if new_end == affected_end {
        let combined_paths = local_search(
            reading_slice(current.reading(), combined_range)?,
            &outer_boundary,
            snapshot,
        )?;
        let focused_paths = shift_paths(combined_paths, combined_range.start);
        let focused_candidates = candidates_from_paths(new_focused_range, focused_paths)?;
        let new_focused =
            segments[focused_index].with_candidates(new_focused_range, focused_candidates)?;
        segments.splice(focused_index..=affected_last, [new_focused]);
        return Segments::new(current.reading().to_owned(), segments, focused_index)
            .map_err(Into::into);
    }

    let split_offset = requested_length;
    let constrained = best_path_with_required_boundary(
        reading_slice(current.reading(), combined_range)?,
        requested_length,
        &outer_boundary,
        snapshot,
    )?;
    let selected_left =
        path_prefix_at(&constrained, split_offset).ok_or(ConversionError::NoConstrainedPath)?;

    let next_range = ScalarRange::new(new_end, affected_end);
    let next_boundary = BoundaryContext {
        prev_right_id: selected_left
            .nodes
            .last()
            .ok_or(ConversionError::NoConstrainedPath)?
            .right_id,
        next_left_id: outer_boundary.next_left_id,
    };
    let next_paths = shift_paths(
        local_search(
            reading_slice(current.reading(), next_range)?,
            &next_boundary,
            snapshot,
        )?,
        next_range.start,
    );
    let next_candidates = candidates_from_paths(next_range, next_paths)?;
    let selected_next = next_candidates
        .first()
        .and_then(|candidate| candidate.paths().first())
        .ok_or(ConversionError::NoConstrainedPath)?;

    let focused_boundary = BoundaryContext {
        prev_right_id: outer_boundary.prev_right_id,
        next_left_id: selected_next
            .nodes
            .first()
            .ok_or(ConversionError::NoConstrainedPath)?
            .left_id,
    };
    let focused_paths = shift_paths(
        local_search(
            reading_slice(current.reading(), new_focused_range)?,
            &focused_boundary,
            snapshot,
        )?,
        new_focused_range.start,
    );
    let focused_candidates = candidates_from_paths(new_focused_range, focused_paths)?;

    let new_focused =
        segments[focused_index].with_candidates(new_focused_range, focused_candidates)?;
    let new_next = segments[affected_last].with_candidates(next_range, next_candidates)?;
    segments.splice(focused_index..=affected_last, [new_focused, new_next]);
    Segments::new(current.reading().to_owned(), segments, focused_index).map_err(Into::into)
}

fn boundary_from_segmented_path(
    groups: &[SegmentedPathGroup],
    index: usize,
    snapshot: &CostSnapshot<'_>,
) -> Result<BoundaryContext, ConversionError> {
    let previous = index
        .checked_sub(1)
        .and_then(|previous| groups[previous].nodes.last());
    let next = groups.get(index + 1).and_then(|group| group.nodes.first());
    Ok(BoundaryContext {
        prev_right_id: previous.map_or(snapshot.dictionary().bos_right_id(), |node| node.right_id),
        next_left_id: next.map_or(snapshot.dictionary().eos_left_id(), |node| node.left_id),
    })
}

fn boundary_around_span(
    segments: &[Segment],
    first_index: usize,
    last_index: usize,
    snapshot: &CostSnapshot<'_>,
) -> Result<BoundaryContext, ConversionError> {
    let previous = if let Some(previous_index) = first_index.checked_sub(1) {
        representative(
            segments
                .get(previous_index)
                .ok_or(crate::SegmentsError::FocusedSegmentOutOfRange)?,
        )?
        .nodes
        .last()
    } else {
        None
    };
    let next = if let Some(segment) = segments.get(last_index + 1) {
        representative(segment)?.nodes.first()
    } else {
        None
    };
    Ok(BoundaryContext {
        prev_right_id: previous.map_or(snapshot.dictionary().bos_right_id(), |node| node.right_id),
        next_left_id: next.map_or(snapshot.dictionary().eos_left_id(), |node| node.left_id),
    })
}

fn representative(segment: &Segment) -> Result<&PathCandidate, ConversionError> {
    // A catalog-only choice has no morphological path. Resizing alone uses
    // the real context retained when the segment was created; it must not
    // escape through Candidate::paths into commit provenance or learning.
    Ok(segment.focused().boundary_path())
}

fn reading_slice(reading: &str, range: ScalarRange) -> Result<&str, ConversionError> {
    let boundaries = GraphemeBoundaries::new(reading);
    let start = boundaries
        .byte_offset(range.start)
        .ok_or(ConversionError::InvalidScalarBoundary)?;
    let end = boundaries
        .byte_offset(range.end)
        .ok_or(ConversionError::InvalidScalarBoundary)?;
    reading
        .get(start..end)
        .ok_or(ConversionError::InvalidScalarBoundary)
}

fn shift_paths(paths: Vec<PathCandidate>, offset: ScalarIndex) -> Vec<PathCandidate> {
    paths
        .into_iter()
        .map(|mut path| {
            for node in &mut path.nodes {
                node.start = ScalarIndex::new(node.start.get() + offset.get());
                node.end = ScalarIndex::new(node.end.get() + offset.get());
            }
            path
        })
        .collect()
}

// The returned cost is the whole path's, not the prefix's; callers use only
// the prefix's nodes.
fn path_prefix_at(path: &PathCandidate, split_offset: usize) -> Option<PathCandidate> {
    let split_index = path
        .nodes
        .iter()
        .position(|node| node.end.get() == split_offset)?
        + 1;
    if split_index >= path.nodes.len() {
        return None;
    }
    Some(PathCandidate {
        nodes: path.nodes[..split_index].to_vec(),
        cost: path.cost,
        snapshot_revision: path.snapshot_revision,
    })
}

#[cfg(test)]
mod tests {
    use crate::TokenKey;
    use std::fs;

    use muu_dict::{
        Dictionary, DictionaryBuilder, LeftContextId, LexiconEntry, PosId, RightContextId,
        UnknownClass, UnknownDefinition,
    };
    use proptest::prelude::*;
    use tempfile::NamedTempFile;

    use super::*;
    use crate::{BoundaryDecision, SegmentRule};

    fn fixture() -> (NamedTempFile, Dictionary, Segmenter) {
        let mut builder =
            DictionaryBuilder::new(8, 8, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"segment-operations-test\"\n");
        for (reading, surface, pos, context) in [
            ("あ", "亜", 1, 1),
            ("い", "伊", 2, 2),
            ("う", "宇", 1, 1),
            ("え", "江", 2, 2),
            ("お", "於", 1, 1),
            ("か", "加", 2, 2),
        ] {
            builder.add_entry(LexiconEntry {
                reading: reading.into(),
                surface: surface.into(),
                left_id: LeftContextId::new(context),
                right_id: RightContextId::new(context),
                left_pos_id: PosId::new(pos),
                right_pos_id: PosId::new(pos),
                word_cost: 0,
            });
        }
        builder.add_unknown(UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(7),
            right_id: RightContextId::new(7),
            left_pos_id: PosId::new(7),
            right_pos_id: PosId::new(7),
            word_cost: 1_000,
        });
        let bytes = builder.build().unwrap();
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        let segmenter = Segmenter::new(
            vec![SegmentRule {
                previous_right_pos: None,
                next_left_pos: Some(PosId::new(2)),
                previous_surface_suffix: None,
                next_surface_prefix: None,
                decision: BoundaryDecision::Join,
            }],
            BoundaryDecision::Break,
        );
        (file, dictionary, segmenter)
    }

    fn spanning_fixture() -> (NamedTempFile, Dictionary, Segmenter) {
        let mut builder =
            DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"segment-span-test\"\n");
        for (reading, surface) in [
            ("あ", "亜"),
            ("い", "伊"),
            ("う", "宇"),
            ("え", "江"),
            ("お", "於"),
            ("か", "加"),
            ("き", "幾"),
            ("く", "久"),
            ("け", "計"),
        ] {
            builder.add_entry(LexiconEntry {
                reading: reading.into(),
                surface: surface.into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: 0,
            });
        }
        builder.add_unknown(UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: PosId::new(1),
            right_pos_id: PosId::new(1),
            word_cost: 1_000,
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        let segmenter = Segmenter::new(
            vec![
                SegmentRule {
                    previous_right_pos: None,
                    next_left_pos: None,
                    previous_surface_suffix: None,
                    next_surface_prefix: Some("江".into()),
                    decision: BoundaryDecision::Break,
                },
                SegmentRule {
                    previous_right_pos: None,
                    next_left_pos: None,
                    previous_surface_suffix: None,
                    next_surface_prefix: Some("於".into()),
                    decision: BoundaryDecision::Break,
                },
            ],
            BoundaryDecision::Join,
        );
        (file, dictionary, segmenter)
    }

    fn evidence_exhaustion_fixture() -> (NamedTempFile, Dictionary, Segmenter) {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"evidence-exhaustion-test\"\n");
        for choice in 0..8 {
            builder.add_entry(LexiconEntry {
                reading: "あ".into(),
                surface: "亜".into(),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: choice,
            });
        }
        builder.add_unknown(UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 10_000,
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        let segmenter = Segmenter::new(
            vec![SegmentRule {
                previous_right_pos: None,
                next_left_pos: Some(PosId::new(0)),
                previous_surface_suffix: None,
                next_surface_prefix: None,
                decision: BoundaryDecision::Join,
            }],
            BoundaryDecision::Break,
        );
        (file, dictionary, segmenter)
    }

    fn exact_token_fixture(entries: &[(&str, &str)]) -> (NamedTempFile, Dictionary, Segmenter) {
        let mut builder =
            DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"segment-exact-token-test\"\n");
        for (reading, surface) in entries {
            builder.add_entry(LexiconEntry {
                reading: (*reading).into(),
                surface: (*surface).into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: 0,
            });
        }
        builder.add_unknown(UnknownDefinition {
            class: UnknownClass::Other,
            group: true,
            max_length: 64,
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: PosId::new(1),
            right_pos_id: PosId::new(1),
            word_cost: 10_000,
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        let segmenter = Segmenter::new(Vec::new(), BoundaryDecision::Break);
        (file, dictionary, segmenter)
    }

    fn assert_invariants(segments: &Segments, revision: crate::SnapshotRevision) {
        let mut previous_end = 0;
        for segment in segments.segments() {
            let range = segment.reading_range();
            assert_eq!(range.start.get(), previous_end);
            assert!(range.start < range.end);
            previous_end = range.end.get();
            for candidate in segment.candidates() {
                assert!(
                    candidate
                        .paths()
                        .iter()
                        .all(|path| path.snapshot_revision == revision)
                );
            }
        }
        assert_eq!(
            previous_end,
            segments.reading().chars().count(),
            "total reading length changed"
        );
    }

    fn production_segmenter() -> Segmenter {
        Segmenter::new(
            vec![
                SegmentRule {
                    previous_right_pos: None,
                    next_left_pos: Some(muu_dict::CoarsePos::AuxiliaryVerb.id()),
                    previous_surface_suffix: None,
                    next_surface_prefix: None,
                    decision: BoundaryDecision::Join,
                },
                SegmentRule {
                    previous_right_pos: None,
                    next_left_pos: Some(muu_dict::CoarsePos::Particle.id()),
                    previous_surface_suffix: None,
                    next_surface_prefix: None,
                    decision: BoundaryDecision::Join,
                },
            ],
            BoundaryDecision::Break,
        )
    }

    fn assert_production_resize_case(
        reading: &str,
        initial_layout: &[usize],
        segmenter: &Segmenter,
        snapshot: &CostSnapshot<'_>,
    ) {
        let mut current = convert_to_segments(reading, segmenter, snapshot).unwrap();
        assert_eq!(
            current
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            initial_layout
        );
        let initial_focused_length = current.focused().unwrap().reading_range().len();
        let reading_length = reading.chars().count();

        for requested_length in (initial_focused_length + 1)..=reading_length {
            let before = current.clone();
            current = resize_focused_segment(&current, requested_length, snapshot).unwrap_or_else(
                |error| panic!("resize reading={reading:?} requested={requested_length}: {error}"),
            );
            assert_eq!(
                current.focused().unwrap().reading_range().len(),
                requested_length
            );
            assert_ne!(current, before);
            assert_invariants(&current, snapshot.revision());
        }
        assert_eq!(current.segments().len(), 1);

        for requested_length in (initial_focused_length..reading_length).rev() {
            let before = current.clone();
            current = resize_focused_segment(&current, requested_length, snapshot).unwrap_or_else(
                |error| panic!("resize reading={reading:?} requested={requested_length}: {error}"),
            );
            assert_eq!(
                current.focused().unwrap().reading_range().len(),
                requested_length
            );
            assert_ne!(current, before);
            assert_invariants(&current, snapshot.revision());
        }
        assert!(current.segments().len() > 1);
        assert_eq!(
            current
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .sum::<usize>(),
            reading_length
        );
    }

    #[test]
    fn conversion_resize_shares_search_and_focus_moves_preserve_candidates() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let segments = convert_to_segments("あいうえおか", &segmenter, &snapshot).unwrap();
        assert_eq!(
            segments
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            [2, 2, 2]
        );

        let preserved_suffix = segments.segments()[2].clone();
        let extended_first = resize_focused_segment(&segments, 3, &snapshot).unwrap();
        assert_eq!(
            extended_first
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            [3, 1, 2]
        );
        assert_eq!(extended_first.segments()[2], preserved_suffix);
        assert_invariants(&extended_first, snapshot.revision());

        let moved = move_segment_focus(&segments, 1).unwrap();
        assert_eq!(moved.focused_segment(), 1);
        assert_eq!(moved.segments(), segments.segments());
        let resized = resize_focused_segment(&moved, 3, &snapshot).unwrap();
        assert_eq!(resized.segments()[1].reading_range().len(), 3);
        assert_eq!(resized.segments()[2].reading_range().len(), 1);
        assert_eq!(resized.segments()[0], segments.segments()[0]);
        assert_invariants(&resized, snapshot.revision());
    }

    #[test]
    fn focus_round_trip_preserves_manual_choice_and_rejects_invalid_destinations() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let mut original = convert_to_segments("あいうえおか", &segmenter, &snapshot).unwrap();
        assert!(original.segments()[0].candidates().len() > 1);
        original.select_focused_candidate(1).unwrap();
        let moved = move_segment_focus(&original, 1).unwrap();
        let returned = move_segment_focus(&moved, 0).unwrap();
        assert_eq!(returned, original);
        assert!(matches!(
            move_segment_focus(&original, original.segments().len()),
            Err(ConversionError::Segments(
                crate::SegmentsError::FocusedSegmentOutOfRange
            ))
        ));
    }

    fn baseline_nodes(segments: &Segments) -> Vec<crate::PathNode> {
        segments
            .segments()
            .iter()
            .flat_map(|s| s.focused().paths()[0].nodes.clone())
            .collect()
    }

    #[test]
    fn selected_lattice_path_changes_boundaries_and_keeps_real_candidate_authority() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let original = convert_to_segments("あいうえおか", &segmenter, &snapshot).unwrap();
        let lattice = crate::FullHypothesisLattice::build(
            original.reading(),
            &snapshot,
            crate::HypothesisBudget::default(),
        )
        .unwrap();
        let hypotheses = lattice
            .constrain(&[])
            .unwrap()
            .enumerate(&baseline_nodes(&original), &snapshot, 16)
            .unwrap();
        let chosen = hypotheses
            .iter()
            .find(|h| {
                h.nodes[1].kind == crate::NodeKind::Unknown(UnknownClass::Other)
                    && h.nodes
                        .iter()
                        .enumerate()
                        .all(|(i, n)| i == 1 || n.kind == crate::NodeKind::Known)
            })
            .unwrap();
        let selected = rebuild_run_from_hypothesis(
            &original,
            ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(6)),
            chosen,
            &[],
            &segmenter,
            &snapshot,
        )
        .unwrap();
        assert_ne!(
            selected
                .segments()
                .iter()
                .map(|s| s.reading_range())
                .collect::<Vec<_>>(),
            original
                .segments()
                .iter()
                .map(|s| s.reading_range())
                .collect::<Vec<_>>()
        );
        assert_eq!(baseline_nodes(&selected), chosen.nodes);
        assert_eq!(
            selected
                .segments()
                .iter()
                .map(|s| s.focused().surface())
                .collect::<String>(),
            chosen.surface
        );
        for segment in selected.segments() {
            assert_eq!(segment.focused_candidate(), 0);
            let previous = original.segments().iter().find(|s| {
                s.reading_range() == segment.reading_range()
                    && segment
                        .candidates()
                        .iter()
                        .skip(1)
                        .any(|c| c.surface() == s.focused().surface())
            });
            let remaining = if let Some(previous) = previous {
                assert_eq!(
                    segment.candidates()[1].surface(),
                    previous.focused().surface()
                );
                &segment.candidates()[2..]
            } else {
                &segment.candidates()[1..]
            };
            assert!(remaining.windows(2).all(|p| p[0].cost() <= p[1].cost()));
            let path = &segment.focused().paths()[0];
            assert_eq!(
                segment.focused().id(),
                &crate::CandidateId::from_path(segment.reading_range(), path)
            );
        }
        // Resizing changes only its affected span; the remaining LM choice is
        // the same real candidate, not a fresh first-cost candidate (docs/src/internals/conversion.md「替えたあと」).
        let suffix = selected.segments().last().unwrap().clone();
        let resized = resize_focused_segment(&selected, 2, &snapshot).unwrap();
        assert_eq!(resized.segments().last().unwrap(), &suffix);
        let original_copy = original.clone();
        let mut wrong_revision = chosen.clone();
        wrong_revision.snapshot_revision =
            crate::SnapshotRevision::new(snapshot.revision().get() + 1);
        assert!(
            rebuild_run_from_hypothesis(
                &original,
                ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(6)),
                &wrong_revision,
                &[],
                &segmenter,
                &snapshot
            )
            .is_err()
        );
        assert_eq!(original, original_copy);
        let mut malformed = chosen.clone();
        malformed.nodes.remove(0);
        assert!(
            rebuild_run_from_hypothesis(
                &original,
                ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(6)),
                &malformed,
                &[],
                &segmenter,
                &snapshot
            )
            .is_err()
        );
        assert_eq!(original.segments().len(), 3);
        assert_eq!(original, original_copy);
    }

    #[test]
    fn rebuilding_partial_run_preserves_complete_protected_and_outside_segments() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let mut original = convert_to_segments("あいうえおか", &segmenter, &snapshot).unwrap();
        let mut segments = original.segments().to_vec();
        let mut protected = segments[0].focused().paths()[0].clone();
        protected.nodes[0].token_key = TokenKey::Overlay {
            tier: crate::LexiconOverlayTier::User,
            dictionary_key: 9,
            entry_id: 1,
        };
        protected.nodes[0].kind = crate::NodeKind::Overlay;
        let frozen = Segment::new(
            segments[0].reading_range(),
            vec![
                crate::Candidate::from_group(segments[0].reading_range(), vec![protected.clone()])
                    .unwrap(),
            ],
            0,
            SegmentState::Unconfirmed,
        )
        .unwrap();
        segments[0] = frozen.clone();
        original = Segments::new(original.reading().to_owned(), segments, 0).unwrap();
        let baseline = baseline_nodes(&original);
        let mut fixed = protected.nodes;
        fixed.extend(original.segments()[2].focused().paths()[0].nodes.clone());
        let lattice = crate::FullHypothesisLattice::build(
            original.reading(),
            &snapshot,
            crate::HypothesisBudget::default(),
        )
        .unwrap();
        let hypotheses = lattice
            .constrain(&fixed)
            .unwrap()
            .enumerate(&baseline, &snapshot, 16)
            .unwrap();
        let chosen = hypotheses
            .iter()
            .find(|h| h.nodes[3].kind == crate::NodeKind::Unknown(UnknownClass::Other))
            .unwrap();
        let selected = rebuild_run_from_hypothesis(
            &original,
            ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(4)),
            chosen,
            &[0],
            &segmenter,
            &snapshot,
        )
        .unwrap();
        assert_eq!(selected.segments()[0], frozen);
        assert_eq!(selected.segments().last().unwrap(), &original.segments()[2]);
        assert_eq!(baseline_nodes(&selected), chosen.nodes);
    }

    #[test]
    fn rebuilding_mixed_segment_keeps_literal_nodes_in_every_candidate() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice = crate::Lattice::build("1あい", &dictionary).unwrap();
        let node = |surface: &str, start| {
            lattice
                .nodes()
                .iter()
                .find(|n| n.surface == surface && n.start.get() == start)
                .unwrap()
                .to_path_node()
                .unwrap()
        };
        let literal = node("1", 0);
        let first = node("亜", 1);
        let last = node("伊", 2);
        let make = |nodes: Vec<crate::PathNode>| {
            let range = ScalarRange::new(nodes.first().unwrap().start, nodes.last().unwrap().end);
            let boundary = BoundaryContext::sentence(&snapshot);
            let path = PathCandidate {
                cost: cost_with_boundary(&nodes, &boundary, &snapshot).unwrap(),
                nodes,
                snapshot_revision: snapshot.revision(),
            };
            Segment::new(
                range,
                vec![crate::Candidate::from_group(range, vec![path]).unwrap()],
                0,
                SegmentState::Unconfirmed,
            )
            .unwrap()
        };
        let original = Segments::new(
            "1あい".into(),
            vec![make(vec![literal.clone(), first]), make(vec![last])],
            0,
        )
        .unwrap();
        let whole = crate::FullHypothesisLattice::build(
            original.reading(),
            &snapshot,
            crate::HypothesisBudget::default(),
        )
        .unwrap();
        let hypotheses = whole
            .constrain(std::slice::from_ref(&literal))
            .unwrap()
            .enumerate(&baseline_nodes(&original), &snapshot, 16)
            .unwrap();
        let chosen = hypotheses
            .iter()
            .find(|h| h.nodes[1].kind == crate::NodeKind::Unknown(UnknownClass::Other))
            .unwrap();
        let selected = rebuild_run_from_hypothesis(
            &original,
            ScalarRange::new(ScalarIndex::new(1), ScalarIndex::new(3)),
            chosen,
            &[],
            &segmenter,
            &snapshot,
        )
        .unwrap();
        assert_eq!(baseline_nodes(&selected), chosen.nodes);
        assert!(
            selected.segments()[0]
                .candidates()
                .iter()
                .all(|c| c.paths().iter().all(|p| p.nodes[0] == literal))
        );
    }

    #[test]
    fn selected_path_replaces_a_cheaper_same_surface_representative() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let original = convert_to_segments("あい", &segmenter, &snapshot).unwrap();
        let cheap = original.segments()[0].focused().paths()[0].clone();
        let mut chosen = cheap.clone();
        chosen.cost += 20;
        chosen.nodes[0].word_cost += 20;
        chosen.nodes[0].token_key = TokenKey::Dictionary(muu_dict::DictionaryTokenId::new(900));
        let range = original.segments()[0].reading_range();
        let (catalog, focus) = crate::local::candidates_with_selected_path(
            range,
            vec![cheap],
            chosen.clone(),
            Some(original.segments()[0].focused().surface()),
        )
        .unwrap();
        assert_eq!(catalog[focus].paths(), &[chosen.clone()]);
        assert_eq!(
            catalog[focus].id(),
            &crate::CandidateId::from_path(range, &chosen)
        );
        assert_eq!(
            catalog
                .iter()
                .filter(|c| c.surface() == chosen.surface())
                .count(),
            1
        );
    }

    #[test]
    fn selected_catalog_offers_the_input_candidate_second_without_rewriting_authority() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let original = convert_to_segments("あい", &segmenter, &snapshot).unwrap();
        let range = original.segments()[0].reading_range();
        let make_path = |token, surface: &str, cost| {
            let mut path = original.segments()[0].focused().paths()[0].clone();
            path.nodes[0].token_key = TokenKey::Dictionary(muu_dict::DictionaryTokenId::new(token));
            path.nodes[0].surface = surface.to_owned();
            path.cost = cost;
            path
        };
        let previous = make_path(900, "返還", 100);
        let old_previous = make_path(904, "返還", 0);
        let chosen = make_path(901, "変換", 300);
        let cheap = make_path(902, "安", 50);
        let other = make_path(903, "他", 200);
        // Changed neighbours replaced the input's representative. Returning
        // to its surface must retain the new catalog's token/path authority.
        let previous_surface = old_previous.surface();
        for previous_surface in [Some(previous_surface.as_str()), None] {
            let (catalog, focus) = crate::local::candidates_with_selected_path(
                range,
                vec![
                    other.clone(),
                    chosen.clone(),
                    previous.clone(),
                    cheap.clone(),
                ],
                chosen.clone(),
                previous_surface,
            )
            .unwrap();
            assert_eq!(focus, 0);
            assert_eq!(catalog[0].paths(), std::slice::from_ref(&chosen));
            assert_eq!(
                catalog[0].id(),
                &crate::CandidateId::from_path(range, &chosen)
            );
            let tail = if let Some(previous_surface) = previous_surface {
                assert_eq!(catalog[1].surface(), previous_surface);
                assert_eq!(
                    catalog[1].id(),
                    &crate::CandidateId::from_path(range, &previous)
                );
                assert_ne!(
                    catalog[1].id(),
                    &crate::CandidateId::from_path(range, &old_previous)
                );
                assert_eq!(catalog[1].paths(), std::slice::from_ref(&previous));
                &catalog[2..]
            } else {
                &catalog[1..]
            };
            assert!(tail.windows(2).all(|p| p[0].cost() <= p[1].cost()));
        }
    }

    #[test]
    fn return_surface_normalizes_across_nodes_and_never_duplicates_the_selected_surface() {
        use unicode_normalization::UnicodeNormalization;
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let original = convert_to_segments("あい", &segmenter, &snapshot).unwrap();
        let range = original.segments()[0].reading_range();
        let mut returned = original.segments()[0].focused().paths()[0].clone();
        assert_eq!(returned.nodes.len(), 2);
        returned.nodes[0].surface = "e".to_owned();
        returned.nodes[1].surface = "\u{301}".to_owned();
        returned.nodes[0].token_key = TokenKey::Dictionary(muu_dict::DictionaryTokenId::new(900));
        returned.nodes[1].token_key = TokenKey::Dictionary(muu_dict::DictionaryTokenId::new(901));
        returned.cost = 100;
        let mut chosen = returned.clone();
        chosen.nodes[0].surface = "別".to_owned();
        chosen.nodes[1].surface = "候補".to_owned();
        chosen.nodes[0].token_key = TokenKey::Dictionary(muu_dict::DictionaryTokenId::new(902));
        chosen.cost = 300;
        let mut cheap = returned.clone();
        cheap.nodes[0].surface = "安".to_owned();
        cheap.nodes[1].surface = "価".to_owned();
        cheap.nodes[0].token_key = TokenKey::Dictionary(muu_dict::DictionaryTokenId::new(903));
        cheap.cost = 50;
        for previous_surface in ["é", "e\u{301}", "別候補", "不在"] {
            let (catalog, focus) = crate::local::candidates_with_selected_path(
                range,
                vec![cheap.clone(), returned.clone()],
                chosen.clone(),
                Some(previous_surface),
            )
            .unwrap();
            assert_eq!(focus, 0);
            assert_eq!(catalog.len(), 3);
            assert_eq!(catalog[0].paths(), std::slice::from_ref(&chosen));
            if previous_surface.nfc().collect::<String>() == "é" {
                assert_eq!(catalog[1].surface(), "é");
                assert_eq!(catalog[1].paths(), std::slice::from_ref(&returned));
            } else {
                assert_eq!(catalog[1].surface(), "安価");
                assert_eq!(catalog[2].surface(), "é");
            }
        }
    }

    #[test]
    fn a_changed_reading_range_does_not_promote_an_equal_input_surface() {
        let mut builder =
            DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"changed-range-return-test\"\n");
        for (reading, surface, cost) in [("あ", "亜", 0), ("あ", "阿", 10), ("い", "伊", 0)] {
            builder.add_entry(LexiconEntry {
                reading: reading.to_owned(),
                surface: surface.to_owned(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: cost,
            });
        }
        builder.add_unknown(UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: PosId::new(1),
            right_pos_id: PosId::new(1),
            word_cost: 1_000,
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice = crate::Lattice::build("あい", &dictionary).unwrap();
        let mut old_node = lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "阿")
            .unwrap()
            .to_path_node()
            .unwrap();
        old_node.end = ScalarIndex::new(2);
        let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2));
        let old = crate::Candidate::from_group(
            range,
            vec![PathCandidate {
                nodes: vec![old_node],
                cost: 10,
                snapshot_revision: snapshot.revision(),
            }],
        )
        .unwrap();
        let original = Segments::new(
            "あい".to_owned(),
            vec![Segment::new(range, vec![old], 0, SegmentState::Unconfirmed).unwrap()],
            0,
        )
        .unwrap();
        let nodes = ["あ", "伊"]
            .map(|surface| {
                lattice
                    .nodes()
                    .iter()
                    .find(|n| n.surface == surface)
                    .unwrap()
                    .to_path_node()
                    .unwrap()
            })
            .to_vec();
        let chosen = crate::FullHypothesis {
            surface: "あ伊".to_owned(),
            base_cost: 1_000,
            nodes,
            is_baseline: false,
            snapshot_revision: snapshot.revision(),
        };
        let rebuilt = rebuild_run_from_hypothesis(
            &original,
            range,
            &chosen,
            &[],
            &Segmenter::new(Vec::new(), BoundaryDecision::Break),
            &snapshot,
        )
        .unwrap();
        let first = &rebuilt.segments()[0];
        assert_ne!(first.reading_range(), range);
        assert_eq!(first.candidates()[0].surface(), "あ");
        assert_eq!(first.candidates()[1].surface(), "亜");
        assert_eq!(
            first.candidates()[2].surface(),
            original.segments()[0].focused().surface()
        );
    }

    #[test]
    fn rebuilding_two_runs_reorders_only_the_new_catalogs() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let mut current = convert_to_segments("あい,うえおか", &segmenter, &snapshot).unwrap();
        let lattice = crate::FullHypothesisLattice::build(
            current.reading(),
            &snapshot,
            crate::HypothesisBudget::default(),
        )
        .unwrap();
        for (start, end) in [(0, 2), (3, 7)] {
            let range = ScalarRange::new(ScalarIndex::new(start), ScalarIndex::new(end));
            let baseline = baseline_nodes(&current);
            let fixed = baseline
                .iter()
                .filter(|n| n.end <= range.start || n.start >= range.end)
                .cloned()
                .collect::<Vec<_>>();
            let hypotheses = lattice
                .constrain(&fixed)
                .unwrap()
                .enumerate(&baseline, &snapshot, 16)
                .unwrap();
            let chosen = hypotheses.iter().find(|h| !h.is_baseline).unwrap();
            let rebuilt =
                rebuild_run_from_hypothesis(&current, range, chosen, &[], &segmenter, &snapshot)
                    .unwrap();
            for old in current.segments().iter().filter(|s| {
                s.reading_range().end <= range.start || s.reading_range().start >= range.end
            }) {
                assert_eq!(
                    rebuilt
                        .segments()
                        .iter()
                        .find(|s| s.reading_range() == old.reading_range()),
                    Some(old)
                );
            }
            for new in rebuilt.segments().iter().filter(|s| {
                s.reading_range().start < range.end && range.start < s.reading_range().end
            }) {
                assert_eq!(new.focused_candidate(), 0);
            }
            assert_eq!(baseline_nodes(&rebuilt), chosen.nodes);
            current = rebuilt;
        }
    }

    #[test]
    fn resilient_conversion_keeps_surfaces_when_path_evidence_hits_its_budget() {
        let (_file, dictionary, segmenter) = evidence_exhaustion_fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let reading = "あ".repeat(6);

        assert!(matches!(
            convert_to_segments(&reading, &segmenter, &snapshot),
            Err(ConversionError::SearchBudgetExceeded { .. })
        ));

        let outcome = convert_to_segments_resilient(&reading, &segmenter, &snapshot)
            .expect("surface-dominant availability path");
        assert!(!outcome.path_evidence_complete());
        assert_eq!(outcome.segments().segments().len(), 1);
        assert_eq!(
            outcome.segments().focused().unwrap().focused().surface(),
            "亜亜亜亜亜亜"
        );
    }

    #[test]
    fn resize_absorbs_short_segments_and_splits_the_last_segment() {
        let (_file, dictionary, segmenter) = spanning_fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let original = convert_to_segments("あいうえおかきくけ", &segmenter, &snapshot).unwrap();
        assert_eq!(
            original
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            [3, 1, 5]
        );

        let extended = resize_focused_segment(&original, 4, &snapshot).unwrap();
        assert_eq!(extended.focused_segment(), 0);
        assert_eq!(
            extended
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            [4, 5]
        );
        assert_invariants(&extended, snapshot.revision());

        let moved = move_segment_focus(&extended, 1).unwrap();
        let preserved_front = moved.segments()[0].clone();
        let shrunk = resize_focused_segment(&moved, 4, &snapshot).unwrap();
        assert_eq!(shrunk.focused_segment(), 1);
        assert_eq!(
            shrunk
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            [4, 4, 1]
        );
        assert_eq!(shrunk.segments()[0], preserved_front);
        assert_invariants(&shrunk, snapshot.revision());

        let merged = resize_focused_segment(&shrunk, 5, &snapshot).unwrap();
        assert_eq!(
            merged
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            [4, 5]
        );
        let shrunk_again = resize_focused_segment(&merged, 3, &snapshot).unwrap();
        assert_eq!(
            shrunk_again
                .segments()
                .iter()
                .map(|segment| segment.reading_range().len())
                .collect::<Vec<_>>(),
            [4, 3, 2]
        );
        assert_eq!(shrunk_again.segments()[0], preserved_front);
        assert_invariants(&shrunk_again, snapshot.revision());
    }

    #[test]
    fn resize_rejects_invalid_boundaries_and_preserves_true_failures() {
        let (_file, dictionary, segmenter) = fixture();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let current = convert_to_segments("あいうえおか", &segmenter, &snapshot).unwrap();

        assert!(matches!(
            resize_focused_segment(&current, 0, &snapshot),
            Err(ConversionError::InvalidSegmentLength)
        ));
        assert!(matches!(
            resize_focused_segment(&current, 7, &snapshot),
            Err(ConversionError::InvalidSegmentLength)
        ));
        assert_eq!(
            resize_focused_segment(&current, 2, &snapshot).unwrap(),
            current
        );

        let (_grapheme_file, grapheme_dictionary, grapheme_segmenter) =
            exact_token_fixture(&[("e\u{301}", "é"), ("x", "x")]);
        let grapheme_snapshot = CostSnapshot::baseline(&grapheme_dictionary);
        let grapheme_segments =
            convert_to_segments("e\u{301}x", &grapheme_segmenter, &grapheme_snapshot).unwrap();
        assert!(matches!(
            resize_focused_segment(&grapheme_segments, 1, &grapheme_snapshot),
            Err(ConversionError::InvalidSegmentLength)
        ));

        let (_exact_file, exact_dictionary, exact_segmenter) =
            exact_token_fixture(&[("あいう", "甲"), ("え", "乙")]);
        let exact_snapshot = CostSnapshot::baseline(&exact_dictionary);
        let exact_segments =
            convert_to_segments("あいうえ", &exact_segmenter, &exact_snapshot).unwrap();
        assert!(matches!(
            resize_focused_segment(&exact_segments, 2, &exact_snapshot),
            Err(ConversionError::NoConstrainedPath)
        ));
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_candidate_paths_keep_particle_and_compound_token_boundaries() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let segmenter = production_segmenter();
        // Candidate surfaces do not define token boundaries (docs/src/internals/conversion.md「替えたあと」): a particle
        // can begin a segment, and a compound can retain several real nodes.
        let kouka = convert_to_segments("こうかなえをかざる", &segmenter, &snapshot).unwrap();
        let following = kouka
            .segments()
            .iter()
            .flat_map(|segment| segment.candidates())
            .find(|candidate| candidate.surface() == "な絵を")
            .unwrap();
        assert_eq!(following.paths()[0].nodes[0].surface, "な");
        assert_ne!(following.paths()[0].nodes.last().unwrap().surface, "な");
        let kousei =
            convert_to_segments("こうせいのあかるさをくらべる", &segmenter, &snapshot).unwrap();
        assert!(kousei.segments().iter().any(|segment| {
            segment
                .candidates()
                .iter()
                .any(|candidate| candidate.surface() == "明る")
        }));
        assert!(
            !kousei
                .segments()
                .iter()
                .any(
                    |segment| segment.candidates().iter().any(|candidate| candidate
                        .paths()
                        .iter()
                        .any(|path| path.nodes.iter().any(|node| node.surface == "明るさ")))
                )
        );
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_dictionary_resize_reaches_each_requested_boundary() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let baseline_segmenter = Segmenter::new(Vec::new(), BoundaryDecision::Break);

        // Keep the former production failure sequence as a real-dictionary
        // contract even when supplemental vocabulary changes which paths fit
        // in the ordinary bounded whole-span result.
        let baseline =
            convert_to_segments("きょうはいいてんき", &baseline_segmenter, &snapshot).unwrap();
        let absorbed_particle = resize_focused_segment(&baseline, 4, &snapshot).unwrap();
        assert_eq!(
            resize_focused_segment(&absorbed_particle, 5, &snapshot)
                .unwrap()
                .focused()
                .unwrap()
                .reading_range()
                .len(),
            5
        );

        assert_production_resize_case(
            "きょうはいいてんき",
            &[3, 1, 5],
            &baseline_segmenter,
            &snapshot,
        );

        let segmenter = production_segmenter();
        assert_production_resize_case("きょうはいいてんきです", &[4, 7], &segmenter, &snapshot);
        assert_production_resize_case("にほんごをにゅうりょく", &[5, 6], &segmenter, &snapshot);
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_dictionary_rewrites_numbers_and_dates_without_lexical_regressions() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let segmenter = production_segmenter();

        for (reading, expected) in [
            ("にじゅうしちにち", "27日"),
            ("きゅうさんねんど", "93年度"),
            ("にせんにじゅうろくねんしがついつか", "2026年4月5日"),
            ("にひゃくろくじゅうご", "265"),
            ("ごひゃくろくじゅうご", "565"),
            ("ごがつみっか", "5月3日"),
            ("さんびゃくろくじゅうご", "365"),
            ("ぜろさん", "03"),
        ] {
            let converted = convert_to_segments(reading, &segmenter, &snapshot)
                .expect("number/date conversion")
                .segments()
                .iter()
                .map(|segment| segment.focused().surface())
                .collect::<String>();
            assert_eq!(converted, expected, "{reading}");
        }

        for (reading, expected) in [
            ("せんせい", "先生"),
            ("ちょうし", "調子"),
            ("まんが", "漫画"),
            ("おくさん", "奥さん"),
            ("ひゃくしょう", "百姓"),
            ("じゅうりょう", "重量"),
            ("せんもん", "専門"),
            ("まんぞく", "満足"),
        ] {
            let converted = convert_to_segments(reading, &segmenter, &snapshot)
                .expect("lexical guard conversion")
                .segments()
                .iter()
                .map(|segment| segment.focused().surface())
                .collect::<String>();
            assert_eq!(converted, expected, "{reading}");
        }
    }

    proptest! {
        #[test]
        fn resize_sequences_preserve_all_invariants(
            operations in prop::collection::vec((any::<usize>(), any::<usize>()), 1..40)
        ) {
            let (_file, dictionary, segmenter) = fixture();
            let snapshot = CostSnapshot::baseline(&dictionary);
            let mut current =
                convert_to_segments("あいうえおか", &segmenter, &snapshot).unwrap();

            for (focus_seed, requested_seed) in operations {
                let focus = focus_seed % current.segments().len();
                current = move_segment_focus(&current, focus).unwrap();
                let before = current.clone();
                let remaining_length = current.reading().chars().count()
                    - current.segments()[focus].reading_range().start.get();
                let requested = 1 + (requested_seed % remaining_length);
                current = resize_focused_segment(
                    &current,
                    requested,
                    &snapshot,
                )
                .unwrap();

                prop_assert_eq!(
                    current.segments()[focus].reading_range().len(),
                    requested
                );
                prop_assert_eq!(
                    &current.segments()[..focus],
                    &before.segments()[..focus],
                    "segments before the focused segment changed"
                );
                prop_assert_eq!(current.focused_segment(), focus);
                assert_invariants(&current, snapshot.revision());
            }
        }
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_dictionary_exposes_arrow_candidates_without_rewriting_phrases() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let segmenter = production_segmenter();

        for (reading, lexical_winner, expected) in [
            ("うえ", "上", "↑"),
            ("した", "した", "↓"),
            ("ひだり", "左", "←"),
            ("みぎ", "右", "→"),
        ] {
            let converted =
                convert_to_segments(reading, &segmenter, &snapshot).expect("symbol conversion");
            assert_eq!(converted.segments().len(), 1, "{reading}");
            assert_eq!(
                converted.segments()[0].focused().surface(),
                lexical_winner,
                "symbol rewrite displaced lexical winner for {reading}"
            );
            let surfaces = converted.segments()[0]
                .candidates()
                .iter()
                .map(|candidate| (candidate.surface(), candidate.cost()))
                .collect::<Vec<_>>();
            let position = surfaces
                .iter()
                .position(|(surface, _)| *surface == expected)
                .unwrap_or_else(|| {
                    panic!("missing {expected} candidate for {reading}: {surfaces:?}")
                });
            assert!(
                position < 9,
                "{expected} is not digit-selectable: {surfaces:?}"
            );
        }

        let arrows = convert_to_segments("やじるし", &segmenter, &snapshot)
            .expect("generic arrow conversion");
        let candidates = arrows.segments()[0]
            .candidates()
            .iter()
            .map(|candidate| candidate.surface())
            .collect::<Vec<_>>();
        assert_eq!(arrows.segments()[0].focused().surface(), "矢印");
        for expected in ["↑", "↓", "←", "→"] {
            assert!(candidates.contains(&expected), "missing {expected}");
        }

        for (reading, expected) in [("うえの", "上の"), ("したがう", "従う"), ("みぎて", "右手")]
        {
            let converted = convert_to_segments(reading, &segmenter, &snapshot)
                .expect("lexical guard conversion")
                .segments()
                .iter()
                .map(|segment| segment.focused().surface())
                .collect::<String>();
            assert_eq!(converted, expected, "{reading}");
        }
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_dictionary_ranks_canonical_kana_punctuation_before_ascii() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let segmenter = production_segmenter();

        for (canonical, ascii) in [
            ("！", "!"),
            ("＂", "\""),
            ("＃", "#"),
            ("＄", "$"),
            ("％", "%"),
            ("＆", "&"),
            ("＇", "'"),
            ("（", "("),
            ("）", ")"),
            ("＊", "*"),
            ("＋", "+"),
            ("、", ","),
            ("ー", "-"),
            ("。", "."),
            ("・", "/"),
            ("：", ":"),
            ("；", ";"),
            ("＜", "<"),
            ("＝", "="),
            ("＞", ">"),
            ("？", "?"),
            ("＠", "@"),
            ("「", "["),
            ("＼", "\\"),
            ("」", "]"),
            ("＾", "^"),
            ("＿", "_"),
            ("｀", "`"),
            ("｛", "{"),
            ("｜", "|"),
            ("｝", "}"),
            ("～", "~"),
        ] {
            let converted = convert_to_segments(canonical, &segmenter, &snapshot)
                .expect("punctuation conversion");
            assert_eq!(converted.segments().len(), 1, "{canonical}");
            let candidates = converted.segments()[0]
                .candidates()
                .iter()
                .map(|candidate| candidate.surface())
                .collect::<Vec<_>>();
            assert_eq!(candidates[0], canonical, "{canonical}: {candidates:?}");
            assert_eq!(candidates[1], ascii, "{canonical}: {candidates:?}");
        }
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_dictionary_exposes_complete_mozc_symbol_readings() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let segmenter = production_segmenter();

        let generic = convert_to_segments("きごう", &segmenter, &snapshot)
            .expect("generic symbol conversion");
        assert_eq!(generic.segments().len(), 1);
        assert_eq!(generic.segments()[0].focused().surface(), "記号");
        let generic_surfaces = generic.segments()[0]
            .candidates()
            .iter()
            .map(|candidate| candidate.surface())
            .collect::<Vec<_>>();
        assert!(
            generic_surfaces.len() >= 349,
            "generic symbol candidates were truncated: {}",
            generic_surfaces.len()
        );
        for expected in ["※", "∀", "⓪", "✈", "➾", "仝", "🄌", "︰"] {
            assert!(
                generic_surfaces.contains(&expected),
                "missing {expected} from きごう"
            );
        }

        for (reading, lexical_winner, expected) in [
            ("さんかく", "参画", ["△", "▲"]),
            ("まる", "丸", ["○", "●"]),
            ("ほし", "星", ["★", "☆"]),
        ] {
            let converted = convert_to_segments(reading, &segmenter, &snapshot)
                .expect("category symbol conversion");
            assert_eq!(converted.segments()[0].focused().surface(), lexical_winner);
            let surfaces = converted.segments()[0]
                .candidates()
                .iter()
                .map(|candidate| candidate.surface())
                .collect::<Vec<_>>();
            for expected in expected {
                assert!(
                    surfaces.contains(&expected),
                    "{reading}: missing {expected}"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_dictionary_exposes_sudachi_lexical_classes_without_name_noise() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let segmenter = production_segmenter();

        let common = convert_to_segments("ようひ", &segmenter, &snapshot)
            .expect("Sudachi common-noun conversion");
        assert_eq!(common.segments().len(), 1);
        assert_eq!(common.segments()[0].focused().surface(), "要否");

        let place = convert_to_segments("あいおいしあいおい", &segmenter, &snapshot)
            .expect("Sudachi place conversion");
        assert_eq!(place.segments().len(), 1);
        let place_candidate = place.segments()[0].focused();
        assert_eq!(place_candidate.surface(), "相生市相生");
        assert!(place_candidate.paths().iter().any(|path| {
            path.nodes
                .iter()
                .any(|node| node.lexical_class == muu_dict::LexicalClass::Place)
        }));

        let person = convert_to_segments("もりしま", &segmenter, &snapshot)
            .expect("Sudachi person conversion");
        let person_candidate = person.segments()[0]
            .candidates()
            .iter()
            .find(|candidate| candidate.surface() == "盛嶋")
            .expect("Sudachi person surface remains reachable");
        assert!(person_candidate.paths().iter().any(|path| {
            path.nodes
                .iter()
                .any(|node| node.lexical_class == muu_dict::LexicalClass::Person)
        }));
    }
}
