use std::fmt;

use muu_dict::{LeftContextId, PosId, RightContextId};

use crate::{
    ConversionError, CostSnapshot, NodeCostBreakdown, NodeKind, ScalarRange, Segmenter,
    SnapshotRevision, TokenKey, TransitionCostBreakdown, convert_to_segments, viterbi_1best,
};

/// A conversion explained for diagnostics: the whole-reading best path with
/// each node's cost terms, and the segments and candidates conversion produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionTrace {
    pub reading: String,
    pub snapshot_revision: SnapshotRevision,
    pub lattice_node_count: usize,
    pub best_path: PathTrace,
    pub segments: Vec<SegmentTrace>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathTrace {
    pub surface: String,
    pub nodes: Vec<NodeTrace>,
    pub eos_transition: TransitionCostBreakdown,
    pub total_cost: i32,
}

/// One node of the best path. `transition` is the cost of entering it from the
/// previous node (or BOS); `cumulative_cost` includes both, so the last node's
/// value plus the EOS transition is the path's total.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeTrace {
    pub reading_range: ScalarRange,
    pub surface: String,
    pub kind: NodeKind,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos_id: PosId,
    pub right_pos_id: PosId,
    pub token_key: TokenKey,
    pub transition: TransitionCostBreakdown,
    pub node: NodeCostBreakdown,
    pub cumulative_cost: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentTrace {
    pub reading_range: ScalarRange,
    pub selected_surface: String,
    pub candidates: Vec<CandidateTrace>,
}

/// `path_count` is how many real paths stand behind the candidate's surface
/// in its segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateTrace {
    pub rank: usize,
    pub surface: String,
    pub base_cost: i32,
    pub path_count: usize,
}

/// Runs the production conversion path and records why its path and candidates
/// won. The returned value owns no dictionary data and performs no logging, so
/// callers can decide where sensitive readings may be inspected.
///
/// The two halves are computed separately. The best path comes from a
/// lattice built with `Lattice::build`, which has no overlays; the segments
/// come from `convert_to_segments`, which builds with the snapshot's
/// overlays and fails on a local search budget instead of falling back as
/// the session's resilient variant does. With an overlay in the snapshot the
/// best path can therefore describe a different lattice from the segments.
/// Today's callers all pass `CostSnapshot::baseline`, which has none.
pub fn trace_conversion(
    reading: &str,
    segmenter: &Segmenter,
    snapshot: &CostSnapshot<'_>,
) -> Result<ConversionTrace, ConversionError> {
    let lattice = crate::Lattice::build(reading, snapshot.dictionary())?;
    let best = viterbi_1best(&lattice, snapshot)?;
    let mut previous_right = snapshot.dictionary().bos_right_id();
    let mut cumulative_cost = 0_i32;
    let mut nodes = Vec::with_capacity(best.nodes.len());

    for path_node in &best.nodes {
        let transition = snapshot.transition_cost_breakdown(previous_right, path_node.left_id)?;
        let node = snapshot.path_node_cost_breakdown(path_node)?;
        cumulative_cost = cumulative_cost
            .checked_add(transition.total)
            .and_then(|cost| cost.checked_add(node.total))
            .ok_or(ConversionError::CostOverflow)?;
        nodes.push(NodeTrace {
            reading_range: ScalarRange::new(path_node.start, path_node.end),
            surface: path_node.surface.clone(),
            kind: path_node.kind,
            left_id: path_node.left_id,
            right_id: path_node.right_id,
            left_pos_id: path_node.left_pos_id,
            right_pos_id: path_node.right_pos_id,
            token_key: path_node.token_key.clone(),
            transition,
            node,
            cumulative_cost,
        });
        previous_right = path_node.right_id;
    }

    let eos_transition =
        snapshot.transition_cost_breakdown(previous_right, snapshot.dictionary().eos_left_id())?;
    let traced_total = cumulative_cost
        .checked_add(eos_transition.total)
        .ok_or(ConversionError::CostOverflow)?;
    // Recomputing the sum from the breakdowns must give Viterbi's total; a
    // mismatch would mean the trace explains a different cost than search used.
    debug_assert_eq!(traced_total, best.total_cost);

    let converted = convert_to_segments(reading, segmenter, snapshot)?;
    let segments = converted
        .segments()
        .iter()
        .map(|segment| {
            Ok(SegmentTrace {
                reading_range: segment.reading_range(),
                selected_surface: segment.focused().surface().to_owned(),
                candidates: segment
                    .candidates()
                    .iter()
                    .enumerate()
                    .map(|(rank, candidate)| {
                        Ok(CandidateTrace {
                            rank,
                            surface: candidate.surface().to_owned(),
                            // Traces read convert_to_segments, before any
                            // presentation, so no auxiliary reaches here.
                            base_cost: candidate
                                .cost()
                                .ok_or(ConversionError::NoConstrainedPath)?,
                            path_count: candidate.paths().len(),
                        })
                    })
                    .collect::<Result<Vec<_>, ConversionError>>()?,
            })
        })
        .collect::<Result<Vec<_>, ConversionError>>()?;

    Ok(ConversionTrace {
        reading: reading.to_owned(),
        snapshot_revision: snapshot.revision(),
        lattice_node_count: lattice.nodes().len(),
        best_path: PathTrace {
            surface: best.surface(),
            nodes,
            eos_transition,
            total_cost: best.total_cost,
        },
        segments,
    })
}

impl fmt::Display for ConversionTrace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "reading={:?} revision={} lattice_nodes={}",
            self.reading,
            self.snapshot_revision.get(),
            self.lattice_node_count
        )?;
        writeln!(
            formatter,
            "best surface={:?} total_cost={}",
            self.best_path.surface, self.best_path.total_cost
        )?;
        for node in &self.best_path.nodes {
            writeln!(
                formatter,
                "node {}..{} surface={:?} kind={:?} token={:?} pos={}:{} context={}:{} \
                 transition={}[base={}] \
                 word={}[base={} lexical_class={} adjustment={}] cumulative={}",
                node.reading_range.start.get(),
                node.reading_range.end.get(),
                node.surface,
                node.kind,
                node.token_key,
                node.left_pos_id.get(),
                node.right_pos_id.get(),
                node.left_id.get(),
                node.right_id.get(),
                node.transition.total,
                node.transition.base,
                node.node.total,
                node.node.base,
                node.node.lexical_class_adjustment,
                node.node.adjustment,
                node.cumulative_cost
            )?;
        }
        writeln!(
            formatter,
            "eos transition={}[base={}]",
            self.best_path.eos_transition.total, self.best_path.eos_transition.base,
        )?;
        for (index, segment) in self.segments.iter().enumerate() {
            writeln!(
                formatter,
                "segment {index} range={}..{} selected={:?} candidates={}",
                segment.reading_range.start.get(),
                segment.reading_range.end.get(),
                segment.selected_surface,
                segment.candidates.len()
            )?;
            for candidate in &segment.candidates {
                writeln!(
                    formatter,
                    "candidate segment={index} rank={} surface={:?} base={} paths={}",
                    candidate.rank, candidate.surface, candidate.base_cost, candidate.path_count
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use muu_dict::{
        Dictionary, DictionaryBuilder, LeftContextId, LexiconEntry, PosId, RightContextId,
        UnknownClass, UnknownDefinition,
    };
    use tempfile::NamedTempFile;

    use super::*;
    use crate::{BoundaryDecision, CostAdjustments, CostProvider, Segmenter, SnapshotRevision};

    #[derive(Clone)]
    struct FixedProvider(CostAdjustments);

    impl CostProvider for FixedProvider {
        fn capture(&self) -> CostAdjustments {
            self.0.clone()
        }
    }

    fn dictionary() -> (NamedTempFile, Dictionary) {
        let mut builder =
            DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"conversion-trace-test\"\n");
        for (reading, surface, cost) in [("あ", "亜", 100), ("い", "伊", 200)] {
            builder.add_entry(LexiconEntry {
                reading: reading.into(),
                surface: surface.into(),
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
            word_cost: 5_000,
        });
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        (file, dictionary)
    }

    // A learning adjustment on the first node shows up in that node's
    // breakdown and in the total, and the trace carries the snapshot's
    // revision, so the trace reports the costs search actually used.
    #[test]
    fn trace_reports_the_exact_cost_components_used_by_search() {
        let (_file, dictionary) = dictionary();
        let segmenter = Segmenter::new(Vec::new(), BoundaryDecision::Break);
        let baseline_snapshot = CostSnapshot::baseline(&dictionary);
        let baseline = trace_conversion("あい", &segmenter, &baseline_snapshot).unwrap();
        assert_eq!(baseline.best_path.surface, "亜伊");
        assert_eq!(baseline.best_path.nodes.len(), 2);

        let first = &baseline.best_path.nodes[0];
        let mut adjustments = CostAdjustments::new(SnapshotRevision::new(7));
        adjustments.add_node_cost(first.token_key.clone(), 11);
        let snapshot = CostSnapshot::capture(&dictionary, &FixedProvider(adjustments));
        let trace = trace_conversion("あい", &segmenter, &snapshot).unwrap();

        assert_eq!(trace.snapshot_revision, SnapshotRevision::new(7));
        assert_eq!(trace.best_path.nodes[0].node.adjustment, 11);
        assert_eq!(
            trace.best_path.total_cost,
            baseline.best_path.total_cost + 11
        );
    }
}
