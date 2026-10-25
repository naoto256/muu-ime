//! Pure-Rust lattice construction and kana-kanji path search.
//!
//! Lattice positions use Unicode scalar indices, matching the conversion
//! algorithm in the design. Any position exposed as an editing boundary must
//! also be present in [`GraphemeBoundaries`]; dictionary matches that would
//! split an extended grapheme cluster are rejected.

#[cfg(test)]
#[path = "../../../test-support/real_assets.rs"]
mod real_assets;
#[cfg(test)]
#[path = "../../../test-support/real_dictionary.rs"]
mod real_dictionary;

mod auxiliary;
mod cost;
mod full_hypothesis;
mod lexicon_overlay;
mod local;
mod number;
mod operations;
mod segments;
mod symbol;
mod trace;
mod user_pos;

use std::fmt;

use muu_dict::{
    Dictionary, DictionaryError, DictionaryTokenId, LeftContextId, LexicalClass, PosId,
    RightContextId, UnknownClass, UnknownDefinition,
};
use unicode_segmentation::UnicodeSegmentation;

pub use auxiliary::{AuxiliaryEntry, AuxiliaryKind};
pub use cost::{
    CostAdjustments, CostProvider, CostSnapshot, NodeCostBreakdown, NoopCostProvider,
    SnapshotRevision, TransitionCostBreakdown,
};
pub use full_hypothesis::{FullHypothesis, FullHypothesisLattice, HypothesisBudget};

pub use lexicon_overlay::{
    LexiconOverlay, LexiconOverlayEntry, LexiconOverlayTier, LexiconSuppression, OverlayFingerprint,
};
pub use local::{
    BoundaryContext, MAX_CANDIDATE_PROVENANCE_PER_READING, MAX_CANDIDATE_SURFACES,
    candidates_from_paths, local_search,
};
pub(crate) use local::{best_path_with_required_boundary, local_search_surface_dominant};
pub use operations::{
    ConversionOutcome, convert_to_segments, convert_to_segments_resilient, move_segment_focus,
    rebuild_run_from_hypothesis, resize_focused_segment,
};
pub use segments::{
    BoundaryDecision, Candidate, CandidateId, PathCandidate, ScalarRange, Segment, SegmentRule,
    SegmentState, SegmentedPath, SegmentedPathGroup, Segmenter, Segments, SegmentsError,
};
pub use trace::{
    CandidateTrace, ConversionTrace, NodeTrace, PathTrace, SegmentTrace, trace_conversion,
};
pub use user_pos::{
    USER_POS_DEFINITIONS, UserPosBehavior, UserPosDefinition, UserPosExpansion, UserPosForm,
    user_pos_definition,
};

// A Unicode scalar (code point) offset into the reading: not a byte offset
// and not UTF-16. muu-session projects it to UTF-16 only when it builds a view.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScalarIndex(usize);

impl ScalarIndex {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapDirection {
    Backward,
    Forward,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphemeBoundaries {
    scalar_to_byte: Vec<usize>,
    legal_scalar_boundaries: Vec<bool>,
}

impl GraphemeBoundaries {
    pub fn new(text: &str) -> Self {
        let mut scalar_to_byte: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
        scalar_to_byte.push(text.len());
        let mut legal_scalar_boundaries = vec![false; scalar_to_byte.len()];
        for (byte, _) in text.grapheme_indices(true) {
            let scalar = scalar_to_byte
                .binary_search(&byte)
                .expect("grapheme boundaries are UTF-8 scalar boundaries");
            legal_scalar_boundaries[scalar] = true;
        }
        legal_scalar_boundaries[scalar_to_byte.len() - 1] = true;
        Self {
            scalar_to_byte,
            legal_scalar_boundaries,
        }
    }

    pub fn scalar_len(&self) -> usize {
        self.scalar_to_byte.len() - 1
    }

    pub fn is_grapheme_boundary(&self, index: ScalarIndex) -> bool {
        self.legal_scalar_boundaries
            .get(index.get())
            .copied()
            .unwrap_or(false)
    }

    // Clamps past-the-end indices to the end first. Both loops terminate:
    // index 0 of a non-empty text and the end index are always legal.
    pub fn snap(&self, index: ScalarIndex, direction: SnapDirection) -> ScalarIndex {
        let mut scalar = index.get().min(self.scalar_len());
        match direction {
            SnapDirection::Backward => {
                while !self.legal_scalar_boundaries[scalar] {
                    scalar -= 1;
                }
            }
            SnapDirection::Forward => {
                while !self.legal_scalar_boundaries[scalar] {
                    scalar += 1;
                }
            }
        }
        ScalarIndex::new(scalar)
    }

    pub fn byte_offset(&self, index: ScalarIndex) -> Option<usize> {
        self.scalar_to_byte.get(index.get()).copied()
    }

    fn scalar_index_for_byte(&self, byte: usize) -> Option<ScalarIndex> {
        self.scalar_to_byte
            .binary_search(&byte)
            .ok()
            .map(ScalarIndex::new)
    }

    fn next_grapheme_boundary(&self, index: ScalarIndex) -> Option<ScalarIndex> {
        ((index.get() + 1)..=self.scalar_len())
            .find(|candidate| self.legal_scalar_boundaries[*candidate])
            .map(ScalarIndex::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Bos,
    Eos,
    Known,
    Unknown(UnknownClass),
    Overlay,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TokenKey {
    Dictionary(DictionaryTokenId),
    Unknown {
        class: UnknownClass,
        surface: String,
    },
    Overlay {
        tier: LexiconOverlayTier,
        dictionary_key: u64,
        entry_id: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatticeNode {
    pub start: ScalarIndex,
    pub end: ScalarIndex,
    pub surface: String,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos_id: Option<PosId>,
    pub right_pos_id: Option<PosId>,
    pub lexical_class: LexicalClass,
    pub token_key: Option<TokenKey>,
    pub word_cost: i32,
    pub kind: NodeKind,
}

impl LatticeNode {
    fn to_path_node(&self) -> Option<PathNode> {
        Some(PathNode {
            start: self.start,
            end: self.end,
            surface: self.surface.clone(),
            left_id: self.left_id,
            right_id: self.right_id,
            left_pos_id: self.left_pos_id?,
            right_pos_id: self.right_pos_id?,
            lexical_class: self.lexical_class,
            token_key: self.token_key.clone()?,
            word_cost: self.word_cost,
            kind: self.kind,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Lattice {
    reading: String,
    boundaries: GraphemeBoundaries,
    nodes: Vec<LatticeNode>,
    nodes_starting_at: Vec<Vec<usize>>,
    nodes_ending_at: Vec<Vec<usize>>,
    bos: usize,
    eos: usize,
    node_limit: usize,
    byte_limit: usize,
    retained_bytes: usize,
}

impl Lattice {
    // At every grapheme start the lattice gets the dictionary prefixes, each
    // overlay's exact-reading entries, number rewrites and exactly one
    // unknown node. Symbol nodes exist only when the whole reading is a
    // symbol reading. Because every grapheme start has an unknown node, a
    // BOS-to-EOS path always exists; `NoPath` from search means a bug, not an
    // unconvertible reading.
    pub fn build(reading: &str, dictionary: &Dictionary) -> Result<Self, ConversionError> {
        Self::build_with_overlays(reading, dictionary, [None, None])
    }

    pub fn build_with_overlays<const N: usize>(
        reading: &str,
        dictionary: &Dictionary,
        overlays: [Option<&LexiconOverlay>; N],
    ) -> Result<Self, ConversionError> {
        Self::build_bounded(reading, dictionary, overlays, usize::MAX, usize::MAX)
    }

    // Only stage ② uses a bounded lattice. Stage ① keeps its existing
    // construction and search contract; an optional selector must not make
    // ordinary conversion fail merely because its own budget is exhausted.
    pub(crate) fn build_bounded<const N: usize>(
        reading: &str,
        dictionary: &Dictionary,
        overlays: [Option<&LexiconOverlay>; N],
        node_limit: usize,
        byte_limit: usize,
    ) -> Result<Self, ConversionError> {
        let retained_bytes = reading
            .chars()
            .count()
            .saturating_add(1)
            .saturating_mul(
                2 * std::mem::size_of::<Vec<usize>>() + std::mem::size_of::<usize>() + 1,
            )
            .saturating_add(reading.len());
        if retained_bytes > byte_limit {
            return Err(ConversionError::FullHypothesisBudgetExceeded);
        }
        let boundaries = GraphemeBoundaries::new(reading);
        let position_count = boundaries.scalar_len() + 1;
        let mut lattice = Self {
            reading: reading.to_owned(),
            boundaries,
            nodes: Vec::new(),
            nodes_starting_at: vec![Vec::new(); position_count],
            nodes_ending_at: vec![Vec::new(); position_count],
            bos: 0,
            eos: 0,
            node_limit,
            byte_limit,
            retained_bytes,
        };

        lattice.bos = lattice.push_node(LatticeNode {
            start: ScalarIndex::new(0),
            end: ScalarIndex::new(0),
            surface: String::new(),
            left_id: LeftContextId::new(0),
            right_id: dictionary.bos_right_id(),
            left_pos_id: None,
            right_pos_id: None,
            lexical_class: LexicalClass::General,
            token_key: None,
            word_cost: 0,
            kind: NodeKind::Bos,
        })?;

        lattice.add_symbol_nodes(dictionary)?;

        for start_value in 0..lattice.boundaries.scalar_len() {
            let start = ScalarIndex::new(start_value);
            if !lattice.boundaries.is_grapheme_boundary(start) {
                continue;
            }
            lattice.add_known_nodes(start, dictionary, &overlays)?;
            for overlay in overlays.into_iter().flatten() {
                lattice.add_overlay_nodes(start, dictionary, Some(overlay), &overlays)?;
            }
            lattice.add_number_nodes(start, dictionary)?;
            lattice.add_unknown_node(start, dictionary)?;
        }

        let end = ScalarIndex::new(lattice.boundaries.scalar_len());
        lattice.eos = lattice.push_node(LatticeNode {
            start: end,
            end,
            surface: String::new(),
            left_id: dictionary.eos_left_id(),
            right_id: RightContextId::new(0),
            left_pos_id: None,
            right_pos_id: None,
            lexical_class: LexicalClass::General,
            token_key: None,
            word_cost: 0,
            kind: NodeKind::Eos,
        })?;
        Ok(lattice)
    }

    // An overlay entry is dropped when any overlay has an entry of a higher
    // tier for the same reading (postal readings compare in canonical form),
    // or when a suppression of its own tier or above matches it: a User word
    // hides every Imported and Postal entry for that reading. Built-in
    // dictionary nodes are never displaced by overlay entries, only by
    // suppressions (`add_known_nodes`).
    fn add_overlay_nodes(
        &mut self,
        start: ScalarIndex,
        _dictionary: &Dictionary,
        overlay: Option<&LexiconOverlay>,
        suppressors: &[Option<&LexiconOverlay>],
    ) -> Result<(), ConversionError> {
        let Some(overlay) = overlay.filter(|overlay| !overlay.is_empty()) else {
            return Ok(());
        };
        let start_byte = self
            .boundaries
            .byte_offset(start)
            .ok_or(ConversionError::InvalidScalarBoundary)?;
        let mut end = start;
        while let Some(next) = self.boundaries.next_grapheme_boundary(end) {
            end = next;
            let end_byte = self
                .boundaries
                .byte_offset(end)
                .ok_or(ConversionError::InvalidScalarBoundary)?;
            let entries = {
                let exact_reading = &self.reading[start_byte..end_byte];
                overlay
                    .entries_for_reading(exact_reading)
                    .iter()
                    .filter(|entry| {
                        !suppressors.iter().flatten().any(|suppressor| {
                            suppressor.has_higher_tier_entry_for_reading(exact_reading, entry.tier)
                                || suppressor.suppresses(
                                    exact_reading,
                                    &entry.surface,
                                    Some(entry.tier),
                                )
                        })
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for entry in entries {
                self.push_node(LatticeNode {
                    start,
                    end,
                    surface: entry.surface,
                    left_id: entry.left_id,
                    right_id: entry.right_id,
                    left_pos_id: Some(entry.left_pos_id),
                    right_pos_id: Some(entry.right_pos_id),
                    lexical_class: LexicalClass::General,
                    token_key: Some(TokenKey::Overlay {
                        tier: entry.tier,
                        dictionary_key: entry.dictionary_key,
                        entry_id: entry.entry_id,
                    }),
                    word_cost: entry.word_cost,
                    kind: NodeKind::Overlay,
                })?;
            }
        }
        Ok(())
    }

    pub fn reading(&self) -> &str {
        &self.reading
    }

    pub fn boundaries(&self) -> &GraphemeBoundaries {
        &self.boundaries
    }

    pub fn nodes(&self) -> &[LatticeNode] {
        &self.nodes
    }

    pub fn nodes_starting_at(&self, index: ScalarIndex) -> &[usize] {
        self.nodes_starting_at
            .get(index.get())
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn add_known_nodes(
        &mut self,
        start: ScalarIndex,
        dictionary: &Dictionary,
        suppressors: &[Option<&LexiconOverlay>],
    ) -> Result<(), ConversionError> {
        let start_byte = self
            .boundaries
            .byte_offset(start)
            .ok_or(ConversionError::InvalidScalarBoundary)?;
        for prefix in dictionary.common_prefix_search(&self.reading[start_byte..])? {
            let end_byte = start_byte
                .checked_add(prefix.reading_length_bytes)
                .ok_or(ConversionError::InvalidScalarBoundary)?;
            let Some(end) = self.boundaries.scalar_index_for_byte(end_byte) else {
                continue;
            };
            if !self.boundaries.is_grapheme_boundary(end) {
                continue;
            }
            for token in prefix.tokens {
                if suppressors.iter().flatten().any(|suppressor| {
                    suppressor.suppresses(&self.reading[start_byte..end_byte], token.surface, None)
                }) {
                    continue;
                }
                self.push_node(LatticeNode {
                    start,
                    end,
                    surface: token.surface.to_owned(),
                    left_id: token.left_id,
                    right_id: token.right_id,
                    left_pos_id: Some(token.left_pos_id),
                    right_pos_id: Some(token.right_pos_id),
                    lexical_class: token.lexical_class,
                    token_key: Some(TokenKey::Dictionary(token.token_id)),
                    word_cost: token.word_cost,
                    kind: NodeKind::Known,
                })?;
            }
        }
        Ok(())
    }

    fn add_number_nodes(
        &mut self,
        start: ScalarIndex,
        dictionary: &Dictionary,
    ) -> Result<(), ConversionError> {
        let start_byte = self
            .boundaries
            .byte_offset(start)
            .ok_or(ConversionError::InvalidScalarBoundary)?;
        let rewrites = number::rewrites(&self.reading[start_byte..]);
        if rewrites.is_empty() {
            return Ok(());
        }
        let definition = unknown_definition_with_fallback(dictionary, UnknownClass::Number)?;
        for rewrite in rewrites {
            let end_byte = start_byte
                .checked_add(rewrite.byte_len)
                .ok_or(ConversionError::InvalidScalarBoundary)?;
            let Some(end) = self.boundaries.scalar_index_for_byte(end_byte) else {
                continue;
            };
            if !self.boundaries.is_grapheme_boundary(end) {
                continue;
            }
            let surface = rewrite.surface;
            self.push_node(LatticeNode {
                start,
                end,
                surface: surface.clone(),
                left_id: definition.left_id,
                right_id: definition.right_id,
                left_pos_id: Some(definition.left_pos_id),
                right_pos_id: Some(definition.right_pos_id),
                lexical_class: LexicalClass::General,
                token_key: Some(TokenKey::Unknown {
                    class: UnknownClass::Number,
                    surface,
                }),
                word_cost: definition.word_cost.saturating_sub(rewrite.word_cost_bonus),
                kind: NodeKind::Unknown(UnknownClass::Number),
            })?;
        }
        Ok(())
    }

    // An ASCII-variant reading gets two nodes: the reading itself, then the
    // ASCII form one cost unit worse, so the Japanese form ranks first and
    // the typed ASCII second (docs/src/usage/input.md「変換と確定」).
    fn add_symbol_nodes(&mut self, dictionary: &Dictionary) -> Result<(), ConversionError> {
        let surfaces = symbol::surfaces(&self.reading);
        let ascii_variant = symbol::ascii_variant(&self.reading);
        if surfaces.is_empty() && ascii_variant.is_none() {
            return Ok(());
        }
        let definition = unknown_definition_with_fallback(dictionary, UnknownClass::Other)?;
        let start = ScalarIndex::new(0);
        let end = ScalarIndex::new(self.boundaries.scalar_len());
        for surface in surfaces {
            let surface = (*surface).to_owned();
            self.push_node(LatticeNode {
                start,
                end,
                surface: surface.clone(),
                left_id: definition.left_id,
                right_id: definition.right_id,
                left_pos_id: Some(definition.left_pos_id),
                right_pos_id: Some(definition.right_pos_id),
                lexical_class: LexicalClass::General,
                token_key: Some(TokenKey::Unknown {
                    class: UnknownClass::Other,
                    surface,
                }),
                word_cost: definition.word_cost.saturating_sub(symbol::WORD_COST_BONUS),
                kind: NodeKind::Unknown(UnknownClass::Other),
            })?;
        }
        if let Some(ascii) = ascii_variant {
            for (surface, cost_delta) in [(self.reading.clone(), 0), (ascii.to_owned(), 1)] {
                self.push_node(LatticeNode {
                    start,
                    end,
                    surface: surface.clone(),
                    left_id: definition.left_id,
                    right_id: definition.right_id,
                    left_pos_id: Some(definition.left_pos_id),
                    right_pos_id: Some(definition.right_pos_id),
                    lexical_class: LexicalClass::General,
                    token_key: Some(TokenKey::Unknown {
                        class: UnknownClass::Other,
                        surface,
                    }),
                    word_cost: definition
                        .word_cost
                        .saturating_sub(symbol::WORD_COST_BONUS)
                        .saturating_add(cost_delta),
                    kind: NodeKind::Unknown(UnknownClass::Other),
                })?;
            }
        }
        Ok(())
    }

    fn add_unknown_node(
        &mut self,
        start: ScalarIndex,
        dictionary: &Dictionary,
    ) -> Result<(), ConversionError> {
        let start_byte = self
            .boundaries
            .byte_offset(start)
            .ok_or(ConversionError::InvalidScalarBoundary)?;
        let first_end = self
            .boundaries
            .next_grapheme_boundary(start)
            .ok_or(ConversionError::InvalidScalarBoundary)?;
        let first_end_byte = self
            .boundaries
            .byte_offset(first_end)
            .ok_or(ConversionError::InvalidScalarBoundary)?;
        let class = classify_grapheme(&self.reading[start_byte..first_end_byte]);
        let definition = unknown_definition_with_fallback(dictionary, class)?;
        // A grouping class gets only the longest same-class run from this
        // start (up to `max_length`, 0 meaning unbounded), not every length,
        // so a path can end inside such a run only where another node ends.
        let end = if definition.group {
            self.group_unknown_run(start, class, definition.max_length)?
        } else {
            first_end
        };
        let end_byte = self
            .boundaries
            .byte_offset(end)
            .ok_or(ConversionError::InvalidScalarBoundary)?;
        let surface = self.reading[start_byte..end_byte].to_owned();
        self.push_node(LatticeNode {
            start,
            end,
            surface: surface.clone(),
            left_id: definition.left_id,
            right_id: definition.right_id,
            left_pos_id: Some(definition.left_pos_id),
            right_pos_id: Some(definition.right_pos_id),
            lexical_class: LexicalClass::General,
            token_key: Some(TokenKey::Unknown { class, surface }),
            word_cost: definition.word_cost,
            kind: NodeKind::Unknown(class),
        })?;
        Ok(())
    }

    fn group_unknown_run(
        &self,
        start: ScalarIndex,
        class: UnknownClass,
        max_length: u32,
    ) -> Result<ScalarIndex, ConversionError> {
        let limit = if max_length == 0 {
            usize::MAX
        } else {
            usize::try_from(max_length).unwrap_or(usize::MAX)
        };
        let mut end = start;
        for _ in 0..limit {
            let Some(next) = self.boundaries.next_grapheme_boundary(end) else {
                break;
            };
            let start_byte = self
                .boundaries
                .byte_offset(end)
                .ok_or(ConversionError::InvalidScalarBoundary)?;
            let end_byte = self
                .boundaries
                .byte_offset(next)
                .ok_or(ConversionError::InvalidScalarBoundary)?;
            if classify_grapheme(&self.reading[start_byte..end_byte]) != class {
                break;
            }
            end = next;
        }
        if end == start {
            return Err(ConversionError::InvalidScalarBoundary);
        }
        Ok(end)
    }

    // Limits are infinite except for stage ②'s bounded lattices; there the
    // estimate counts the node, its two index slots and its owned strings.
    fn push_node(&mut self, node: LatticeNode) -> Result<usize, ConversionError> {
        let token_bytes = match &node.token_key {
            Some(TokenKey::Unknown { surface, .. }) => surface.len(),
            _ => 0,
        };
        let bytes = self
            .retained_bytes
            .saturating_add(std::mem::size_of::<LatticeNode>() + 2 * std::mem::size_of::<usize>())
            .saturating_add(node.surface.len())
            .saturating_add(token_bytes);
        if self.nodes.len() >= self.node_limit || bytes > self.byte_limit {
            return Err(ConversionError::FullHypothesisBudgetExceeded);
        }
        let id = self.nodes.len();
        self.nodes_starting_at[node.start.get()].push(id);
        self.nodes_ending_at[node.end.get()].push(id);
        self.nodes.push(node);
        self.retained_bytes = bytes;
        Ok(id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathNode {
    pub start: ScalarIndex,
    pub end: ScalarIndex,
    pub surface: String,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos_id: PosId,
    pub right_pos_id: PosId,
    pub lexical_class: LexicalClass,
    pub token_key: TokenKey,
    pub word_cost: i32,
    pub kind: NodeKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BestPath {
    pub nodes: Vec<PathNode>,
    pub total_cost: i32,
    pub snapshot_revision: SnapshotRevision,
}

impl BestPath {
    pub fn surface(&self) -> String {
        self.nodes
            .iter()
            .map(|node| node.surface.as_str())
            .collect()
    }
}

pub fn viterbi_1best(
    lattice: &Lattice,
    snapshot: &CostSnapshot<'_>,
) -> Result<BestPath, ConversionError> {
    let mut costs = vec![None; lattice.nodes.len()];
    let mut predecessors = vec![None; lattice.nodes.len()];
    costs[lattice.bos] = Some(0_i32);

    for position in 0..=lattice.boundaries.scalar_len() {
        for &node_id in &lattice.nodes_starting_at[position] {
            if node_id == lattice.bos {
                continue;
            }
            let node = &lattice.nodes[node_id];
            let mut best: Option<(i32, usize)> = None;
            for &previous_id in &lattice.nodes_ending_at[position] {
                if previous_id == node_id {
                    continue;
                }
                let Some(previous_cost) = costs[previous_id] else {
                    continue;
                };
                let previous = &lattice.nodes[previous_id];
                let edge_cost = snapshot.transition_cost(previous.right_id, node.left_id)?;
                let node_cost = snapshot.node_cost(node)?;
                let candidate_cost = previous_cost
                    .checked_add(edge_cost)
                    .and_then(|cost| cost.checked_add(node_cost))
                    .ok_or(ConversionError::CostOverflow)?;
                // Equal costs keep the lower predecessor id, the node pushed
                // first, so the 1-best path is deterministic.
                let candidate = (candidate_cost, previous_id);
                if best.is_none_or(|current| candidate < current) {
                    best = Some(candidate);
                }
            }
            if let Some((cost, previous)) = best {
                costs[node_id] = Some(cost);
                predecessors[node_id] = Some(previous);
            }
        }
    }

    let total_cost = costs[lattice.eos].ok_or(ConversionError::NoPath)?;
    let mut path = Vec::new();
    let mut current = lattice.eos;
    while current != lattice.bos {
        let previous = predecessors[current].ok_or(ConversionError::NoPath)?;
        if current != lattice.eos {
            let node = &lattice.nodes[current];
            path.push(
                node.to_path_node()
                    .expect("non-endpoint lattice nodes always have token metadata"),
            );
        }
        current = previous;
    }
    path.reverse();
    Ok(BestPath {
        nodes: path,
        total_cost,
        snapshot_revision: snapshot.revision(),
    })
}

#[derive(Debug)]
pub enum ConversionError {
    Dictionary(DictionaryError),
    Segments(SegmentsError),
    InvalidScalarBoundary,
    CostOverflow,
    NoPath,
    FullHypothesisBudgetExceeded,
    SelectedCandidateBudgetExceeded,
    SearchBudgetExceeded {
        expanded_states: usize,
        queued_states: usize,
        completed_paths: usize,
        reading_len: usize,
    },
    InvalidSegmentLength,
    CannotResizeLastSegment,
    NoConstrainedPath,
}

impl fmt::Display for ConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dictionary(error) => write!(formatter, "{error}"),
            Self::Segments(error) => write!(formatter, "{error}"),
            Self::InvalidScalarBoundary => write!(formatter, "invalid scalar or grapheme boundary"),
            Self::CostOverflow => write!(formatter, "Viterbi path cost exceeds i32"),
            Self::NoPath => write!(formatter, "lattice has no BOS-to-EOS path"),
            Self::FullHypothesisBudgetExceeded => {
                write!(formatter, "full hypothesis budget exceeded")
            }
            Self::SelectedCandidateBudgetExceeded => {
                write!(formatter, "selected candidate exceeds catalog budget")
            }
            Self::SearchBudgetExceeded {
                expanded_states,
                queued_states,
                completed_paths,
                reading_len,
            } => write!(
                formatter,
                "local search truncated: expanded_states={expanded_states} queued_states={queued_states} completed_paths={completed_paths} reading_len={reading_len}"
            ),
            Self::InvalidSegmentLength => write!(formatter, "requested segment length is invalid"),
            Self::CannotResizeLastSegment => {
                write!(
                    formatter,
                    "the last segment has no following boundary to move"
                )
            }
            Self::NoConstrainedPath => write!(formatter, "no path satisfies the segment boundary"),
        }
    }
}

impl std::error::Error for ConversionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Dictionary(error) => Some(error),
            Self::Segments(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DictionaryError> for ConversionError {
    fn from(value: DictionaryError) -> Self {
        Self::Dictionary(value)
    }
}

impl From<SegmentsError> for ConversionError {
    fn from(value: SegmentsError) -> Self {
        Self::Segments(value)
    }
}

fn unknown_definition_with_fallback(
    dictionary: &Dictionary,
    class: UnknownClass,
) -> Result<UnknownDefinition, ConversionError> {
    if let Some(definition) = dictionary.find_unknown_definition(class)? {
        return Ok(definition);
    }
    dictionary
        .unknown_definition(UnknownClass::Other)
        .map_err(Into::into)
}

// Hiragana has no class of its own and falls to `Other`; the prolonged sound
// mark ー (U+30FC) is in the katakana block and classifies as katakana.
fn classify_grapheme(grapheme: &str) -> UnknownClass {
    let mut characters = grapheme.chars();
    let Some(first) = characters.next() else {
        return UnknownClass::Other;
    };
    if first.is_ascii_digit() && characters.all(|character| character.is_ascii_digit()) {
        UnknownClass::Number
    } else if first.is_ascii_alphabetic()
        && characters.all(|character| character.is_ascii_alphabetic())
    {
        UnknownClass::AsciiAlphabetic
    } else if is_katakana(first) && characters.all(is_katakana) {
        UnknownClass::Katakana
    } else if is_kanji(first) && characters.all(is_kanji) {
        UnknownClass::Kanji
    } else {
        UnknownClass::Other
    }
}

fn is_katakana(character: char) -> bool {
    matches!(
        character,
        '\u{30A0}'..='\u{30FF}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{FF66}'..='\u{FF9F}'
    )
}

fn is_kanji(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2FA1F}'
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use muu_dict::{DictionaryBuilder, LexiconEntry};
    use tempfile::NamedTempFile;

    use super::*;

    fn unknown(class: UnknownClass, id: u16) -> UnknownDefinition {
        UnknownDefinition {
            class,
            group: class != UnknownClass::Other,
            max_length: 64,
            left_id: LeftContextId::new(id),
            right_id: RightContextId::new(id),
            left_pos_id: PosId::new(id),
            right_pos_id: PosId::new(id),
            word_cost: 300,
        }
    }

    fn synthetic_dictionary(
        configure: impl FnOnce(&mut DictionaryBuilder),
    ) -> (NamedTempFile, Dictionary) {
        let mut builder =
            DictionaryBuilder::new(8, 8, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"muu-conversion-test\"\n");
        builder.add_entry(LexiconEntry {
            reading: "むう".into(),
            surface: "Muu".into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: PosId::new(1),
            right_pos_id: PosId::new(1),
            word_cost: 100,
        });
        for (class, id) in [
            (UnknownClass::Number, 2),
            (UnknownClass::AsciiAlphabetic, 3),
            (UnknownClass::Katakana, 4),
            (UnknownClass::Kanji, 5),
            (UnknownClass::Other, 6),
        ] {
            builder.add_unknown(unknown(class, id));
        }
        configure(&mut builder);
        let bytes = builder.build().unwrap();
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        (file, dictionary)
    }

    fn overlay_entry(
        tier: LexiconOverlayTier,
        entry_id: u32,
        reading: &str,
        surface: &str,
    ) -> LexiconOverlayEntry {
        LexiconOverlayEntry {
            tier,
            dictionary_key: u64::from(entry_id),
            entry_id,
            form_id: 0,
            reading: reading.to_owned(),
            surface: surface.to_owned(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: PosId::new(1),
            right_pos_id: PosId::new(1),
            word_cost: -10_000,
        }
    }

    #[test]
    fn overlay_catalogs_suppress_only_lower_tiers_for_an_exact_reading() {
        let (_file, dictionary) = synthetic_dictionary(|_| {});
        let user = LexiconOverlay::new([overlay_entry(
            LexiconOverlayTier::User,
            1,
            "むう",
            "ユーザー",
        )]);
        let imported = LexiconOverlay::new([
            overlay_entry(LexiconOverlayTier::Imported, 2, "むう", "取り込みA"),
            overlay_entry(LexiconOverlayTier::Imported, 3, "むう", "取り込みB"),
        ]);
        let postal = LexiconOverlay::new([overlay_entry(
            LexiconOverlayTier::PostalCode,
            4,
            "むう",
            "郵便",
        )]);
        let lattice = Lattice::build_with_overlays(
            "むう",
            &dictionary,
            [Some(&user), Some(&imported), Some(&postal)],
        )
        .unwrap();
        let surfaces = lattice
            .nodes()
            .iter()
            .map(|node| node.surface.as_str())
            .collect::<Vec<_>>();
        assert!(
            surfaces.contains(&"Muu"),
            "the base dictionary always remains"
        );
        assert!(surfaces.contains(&"ユーザー"));
        assert!(!surfaces.contains(&"取り込みA"));
        assert!(!surfaces.contains(&"取り込みB"));
        assert!(!surfaces.contains(&"郵便"));

        let without_user = Lattice::build_with_overlays(
            "むう",
            &dictionary,
            [None, Some(&imported), Some(&postal)],
        )
        .unwrap();
        let surfaces = without_user
            .nodes()
            .iter()
            .map(|node| node.surface.as_str())
            .collect::<Vec<_>>();
        assert!(surfaces.contains(&"取り込みA"));
        assert!(surfaces.contains(&"取り込みB"));
        assert!(!surfaces.contains(&"郵便"));
    }

    #[test]
    fn postal_canonical_reading_preserves_tier_precedence_and_suppression() {
        let (_file, dictionary) = synthetic_dictionary(|_| {});
        let user = LexiconOverlay::new([overlay_entry(
            LexiconOverlayTier::User,
            1,
            "1000001",
            "ユーザー住所",
        )]);
        let postal = LexiconOverlay::new([overlay_entry(
            LexiconOverlayTier::PostalCode,
            2,
            "1000001",
            "郵便住所",
        )]);
        let lattice = Lattice::build_with_overlays(
            "１００－０００１",
            &dictionary,
            [Some(&user), None, Some(&postal)],
        )
        .unwrap();
        let surfaces = lattice
            .nodes()
            .iter()
            .map(|node| node.surface.as_str())
            .collect::<Vec<_>>();
        assert!(surfaces.contains(&"ユーザー住所"));
        assert!(!surfaces.contains(&"郵便住所"));

        let suppressor = LexiconOverlay::with_suppressions(
            [],
            [LexiconSuppression {
                tier: LexiconOverlayTier::User,
                reading: Some("1000001".to_owned()),
                surface: Some("郵便住所".to_owned()),
            }],
        );
        let suppressed = Lattice::build_with_overlays(
            "100ー0001",
            &dictionary,
            [Some(&suppressor), None, Some(&postal)],
        )
        .unwrap();
        assert!(
            !suppressed
                .nodes()
                .iter()
                .any(|node| node.surface == "郵便住所")
        );
    }

    #[test]
    fn viterbi_uses_previous_right_then_next_left() {
        let (_file, dictionary) = synthetic_dictionary(|builder| {
            builder.add_entry(LexiconEntry {
                reading: "あ".into(),
                surface: "A".into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: 0,
            });
            builder.add_entry(LexiconEntry {
                reading: "い".into(),
                surface: "正".into(),
                left_id: LeftContextId::new(2),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(2),
                right_pos_id: PosId::new(2),
                word_cost: 0,
            });
            builder.add_entry(LexiconEntry {
                reading: "い".into(),
                surface: "転置".into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(3),
                right_pos_id: PosId::new(3),
                word_cost: 20,
            });
            builder
                .set_connection_cost(RightContextId::new(1), LeftContextId::new(2), -50)
                .unwrap();
            builder
                .set_connection_cost(RightContextId::new(2), LeftContextId::new(1), 80)
                .unwrap();
        });
        let lattice = Lattice::build("あい", &dictionary).unwrap();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let path = viterbi_1best(&lattice, &snapshot).unwrap();

        assert_eq!(path.surface(), "A正");
        assert_eq!(path.total_cost, -50);
    }

    #[test]
    fn path_cost_accumulates_beyond_i16() {
        let (_file, dictionary) = synthetic_dictionary(|builder| {
            builder.add_entry(LexiconEntry {
                reading: "あ".into(),
                surface: "亜".into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: -30_000,
            });
            builder.add_entry(LexiconEntry {
                reading: "い".into(),
                surface: "伊".into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: -30_000,
            });
        });
        let lattice = Lattice::build("あい", &dictionary).unwrap();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let path = viterbi_1best(&lattice, &snapshot).unwrap();

        assert_eq!(path.surface(), "亜伊");
        assert_eq!(path.total_cost, -60_000);
    }

    #[test]
    fn grouped_unknown_classes_always_keep_the_lattice_connected() {
        let (_file, dictionary) = synthetic_dictionary(|_| {});
        for (reading, expected_classes) in [
            ("12345", vec![UnknownClass::Number]),
            ("MuuIME", vec![UnknownClass::AsciiAlphabetic]),
            ("カタカナ", vec![UnknownClass::Katakana]),
            ("日本語", vec![UnknownClass::Kanji]),
            (
                "Muu123カナ",
                vec![
                    UnknownClass::AsciiAlphabetic,
                    UnknownClass::Number,
                    UnknownClass::Katakana,
                ],
            ),
        ] {
            let lattice = Lattice::build(reading, &dictionary).unwrap();
            let snapshot = CostSnapshot::baseline(&dictionary);
            let path = viterbi_1best(&lattice, &snapshot).unwrap();
            assert_eq!(path.surface(), reading);
            assert_eq!(
                path.nodes
                    .iter()
                    .map(|node| match node.kind {
                        NodeKind::Unknown(class) => class,
                        _ => panic!("expected an unknown node"),
                    })
                    .collect::<Vec<_>>(),
                expected_classes
            );
        }
    }

    #[test]
    fn editing_offsets_snap_to_extended_grapheme_boundaries() {
        for text in ["👨‍👩‍👧‍👦", "👍🏽", "🇯🇵", "e\u{301}", "ｶ\u{FF9E}"]
        {
            let boundaries = GraphemeBoundaries::new(text);
            assert_eq!(
                boundaries.snap(ScalarIndex::new(1), SnapDirection::Backward),
                ScalarIndex::new(0),
                "{text:?}"
            );
            assert_eq!(
                boundaries.snap(ScalarIndex::new(1), SnapDirection::Forward),
                ScalarIndex::new(boundaries.scalar_len()),
                "{text:?}"
            );
        }
    }

    #[test]
    fn dictionary_matches_cannot_split_a_grapheme_cluster() {
        let (_file, dictionary) = synthetic_dictionary(|builder| {
            builder.add_entry(LexiconEntry {
                reading: "e".into(),
                surface: "split".into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: -1000,
            });
        });
        let reading = "e\u{301}";
        let lattice = Lattice::build(reading, &dictionary).unwrap();
        assert!(
            lattice
                .nodes()
                .iter()
                .all(|node| node.kind != NodeKind::Known)
        );
        let snapshot = CostSnapshot::baseline(&dictionary);
        let path = viterbi_1best(&lattice, &snapshot).unwrap();
        assert_eq!(path.surface(), reading);
        assert_eq!(path.nodes.len(), 1);
        assert_eq!(path.nodes[0].end, ScalarIndex::new(2));
    }
}
