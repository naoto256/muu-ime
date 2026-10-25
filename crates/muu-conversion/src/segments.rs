use std::fmt;
use std::sync::Arc;

use muu_dict::PosId;
use unicode_normalization::UnicodeNormalization;

use crate::{
    AuxiliaryEntry, BestPath, GraphemeBoundaries, PathNode, ScalarIndex, SnapDirection,
    SnapshotRevision, TokenKey,
};

// Half-open [start, end) in Unicode scalars of the reading.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScalarRange {
    pub start: ScalarIndex,
    pub end: ScalarIndex,
}

impl ScalarRange {
    pub const fn new(start: ScalarIndex, end: ScalarIndex) -> Self {
        Self { start, end }
    }

    pub fn len(self) -> usize {
        self.end.get().saturating_sub(self.start.get())
    }

    pub const fn is_empty(self) -> bool {
        self.start.get() >= self.end.get()
    }

    /// Whether a boundary cuts strictly inside this range (docs/src/internals/conversion.md「文節の移動と伸縮」). Touching an
    /// endpoint is not a cut: selection may preserve such a complete node,
    /// but must never split a node's surface by guessing a character offset.
    pub const fn crosses_boundary(self, boundary: ScalarIndex) -> bool {
        self.start.get() < boundary.get() && boundary.get() < self.end.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathCandidate {
    pub nodes: Vec<PathNode>,
    pub cost: i32,
    pub snapshot_revision: SnapshotRevision,
}

impl PathCandidate {
    pub fn surface(&self) -> String {
        self.nodes
            .iter()
            .map(|node| node.surface.as_str())
            .collect::<String>()
            .nfc()
            .collect()
    }

    pub fn token_keys(&self) -> impl Iterator<Item = &TokenKey> {
        self.nodes.iter().map(|node| &node.token_key)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CandidateId(String);

impl CandidateId {
    pub fn new(value: String) -> Result<Self, SegmentsError> {
        if value.is_empty() {
            return Err(SegmentsError::EmptyCandidateId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    // "<start>-<end>/" then one item per token: `d<token id>.`,
    // `u<class>:<surface hex>.` or `o<tier>:[<key hex>@]<entry id>.`. The
    // ID only names a path; Swift echoes it back to select a candidate.
    // `form_id` is left out, so two forms of one overlay entry over the same
    // range share an ID. In the pinned conjugation table no two forms of a
    // family share a reading suffix with different surfaces, so such forms
    // also share a surface and fall into one candidate.
    pub(crate) fn from_path(range: ScalarRange, path: &PathCandidate) -> Self {
        let mut value = format!("{}-{}/", range.start.get(), range.end.get());
        for key in path.token_keys() {
            match key {
                TokenKey::Dictionary(id) => {
                    value.push('d');
                    value.push_str(&id.get().to_string());
                    value.push('.');
                }
                TokenKey::Unknown { class, surface } => {
                    value.push('u');
                    value.push_str(&(*class as u8).to_string());
                    value.push(':');
                    for byte in surface.as_bytes() {
                        use std::fmt::Write;
                        write!(value, "{byte:02x}").expect("writing to String cannot fail");
                    }
                    value.push('.');
                }
                TokenKey::Overlay {
                    tier,
                    dictionary_key,
                    entry_id,
                } => {
                    value.push('o');
                    value.push_str(&(*tier as u8).to_string());
                    value.push(':');
                    if *dictionary_key != 0 {
                        use std::fmt::Write;
                        write!(value, "{dictionary_key:016x}@")
                            .expect("writing to String cannot fail");
                    }
                    value.push_str(&entry_id.to_string());
                    value.push('.');
                }
            }
        }
        Self(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    id: CandidateId,
    reading_range: ScalarRange,
    surface: String,
    content: CandidateContent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CandidateContent {
    /// Exact local-search provenance, stable representative first.
    Paths {
        cost: i32,
        paths: Vec<PathCandidate>,
    },
    Auxiliary {
        entry: Arc<AuxiliaryEntry>,
        // Only local resizing may use this pre-rewrite boundary context. It
        // is never returned as this candidate's committed/learned path.
        boundary_path: Arc<PathCandidate>,
    },
}

impl Candidate {
    // All paths of one NFC surface over the same range, cheapest first. The
    // first path is the representative: it gives the ID and the cost, and it
    // is what commit and learning use (`paths()[0]`).
    pub fn from_group(
        reading_range: ScalarRange,
        paths: Vec<PathCandidate>,
    ) -> Result<Self, SegmentsError> {
        let representative = paths.first().ok_or(SegmentsError::EmptyCandidatePaths)?;
        let revision = representative.snapshot_revision;
        for path in &paths {
            validate_path_range(reading_range, path)?;
            if path.snapshot_revision != revision {
                return Err(SegmentsError::MixedCostSnapshots);
            }
        }
        if paths.windows(2).any(|pair| pair[0].cost > pair[1].cost) {
            return Err(SegmentsError::CandidatePathsNotCostSorted);
        }
        let surface = representative.surface();
        if paths.iter().any(|path| path.surface() != surface) {
            return Err(SegmentsError::MixedCandidateSurfaces);
        }
        Ok(Self {
            id: CandidateId::from_path(reading_range, representative),
            reading_range,
            surface,
            content: CandidateContent::Paths {
                cost: representative.cost,
                paths,
            },
        })
    }

    pub fn from_auxiliary(
        reading_range: ScalarRange,
        entry: Arc<AuxiliaryEntry>,
        boundary_path: Arc<PathCandidate>,
    ) -> Result<Self, SegmentsError> {
        if reading_range.is_empty() {
            return Err(SegmentsError::EmptyReadingRange);
        }
        validate_path_range(reading_range, &boundary_path)?;
        let mut identity = format!(
            "{}-{}/a:{}:",
            reading_range.start.get(),
            reading_range.end.get(),
            entry.kind().stable_name()
        );
        for byte in entry.data_sha256() {
            use std::fmt::Write;
            write!(identity, "{byte:02x}").expect("writing to String cannot fail");
        }
        identity.push(':');
        identity.push_str(&entry.entry_id().to_string());
        Ok(Self {
            id: CandidateId::new(identity)?,
            reading_range,
            surface: entry.surface().to_owned(),
            content: CandidateContent::Auxiliary {
                entry,
                boundary_path,
            },
        })
    }

    pub fn id(&self) -> &CandidateId {
        &self.id
    }

    pub fn reading_range(&self) -> ScalarRange {
        self.reading_range
    }

    pub fn surface(&self) -> &str {
        &self.surface
    }

    pub fn cost(&self) -> Option<i32> {
        match &self.content {
            CandidateContent::Paths { cost, .. } => Some(*cost),
            CandidateContent::Auxiliary { .. } => None,
        }
    }

    /// Auxiliary candidates have no lattice path. An empty slice must never
    /// be replaced with their retained boundary context in learning/recording.
    pub fn paths(&self) -> &[PathCandidate] {
        match &self.content {
            CandidateContent::Paths { paths, .. } => paths,
            CandidateContent::Auxiliary { .. } => &[],
        }
    }

    pub fn auxiliary(&self) -> Option<&AuxiliaryEntry> {
        match &self.content {
            CandidateContent::Paths { .. } => None,
            CandidateContent::Auxiliary { entry, .. } => Some(entry),
        }
    }

    pub(crate) fn boundary_path(&self) -> &PathCandidate {
        match &self.content {
            CandidateContent::Paths { paths, .. } => &paths[0],
            CandidateContent::Auxiliary { boundary_path, .. } => boundary_path,
        }
    }
}

fn validate_path_range(
    reading_range: ScalarRange,
    path: &PathCandidate,
) -> Result<(), SegmentsError> {
    let first = path.nodes.first().ok_or(SegmentsError::EmptyPathNodes)?;
    let last = path.nodes.last().ok_or(SegmentsError::EmptyPathNodes)?;
    if first.start != reading_range.start
        || last.end != reading_range.end
        || path
            .nodes
            .windows(2)
            .any(|pair| pair[0].end != pair[1].start)
    {
        return Err(SegmentsError::PathDoesNotCoverReadingRange);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentState {
    Unconfirmed,
    Confirmed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    reading_range: ScalarRange,
    candidates: Vec<Candidate>,
    focused_candidate: usize,
    state: SegmentState,
}

impl Segment {
    pub fn new(
        reading_range: ScalarRange,
        candidates: Vec<Candidate>,
        focused_candidate: usize,
        state: SegmentState,
    ) -> Result<Self, SegmentsError> {
        if reading_range.is_empty() {
            return Err(SegmentsError::EmptyReadingRange);
        }
        if candidates.is_empty() {
            return Err(SegmentsError::EmptyCandidates);
        }
        if candidates
            .iter()
            .any(|candidate| candidate.reading_range() != reading_range)
        {
            return Err(SegmentsError::CandidateRangeMismatch);
        }
        if focused_candidate >= candidates.len() {
            return Err(SegmentsError::FocusedCandidateOutOfRange);
        }
        Ok(Self {
            reading_range,
            candidates,
            focused_candidate,
            state,
        })
    }

    pub fn reading_range(&self) -> ScalarRange {
        self.reading_range
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn focused_candidate(&self) -> usize {
        self.focused_candidate
    }

    pub fn focused(&self) -> &Candidate {
        &self.candidates[self.focused_candidate]
    }

    pub fn state(&self) -> SegmentState {
        self.state
    }

    pub fn select(&mut self, index: usize) -> Result<(), SegmentsError> {
        if index >= self.candidates.len() {
            return Err(SegmentsError::FocusedCandidateOutOfRange);
        }
        self.focused_candidate = index;
        Ok(())
    }

    // IDs are not checked for uniqueness within a segment; the first match
    // wins. Builders group by surface, so in practice each ID appears once.
    pub fn select_by_id(&mut self, id: &CandidateId) -> Result<(), SegmentsError> {
        let index = self
            .candidates
            .iter()
            .position(|candidate| candidate.id() == id)
            .ok_or(SegmentsError::CandidateIdNotFound)?;
        self.focused_candidate = index;
        Ok(())
    }

    pub fn set_state(&mut self, state: SegmentState) {
        self.state = state;
    }

    // A rebuilt segment keeps its state but restarts at candidate 0, the
    // cheapest: a resize returns to cost order (docs/src/internals/conversion.md「文節の移動と伸縮」).
    pub(crate) fn with_candidates(
        &self,
        reading_range: ScalarRange,
        candidates: Vec<Candidate>,
    ) -> Result<Self, SegmentsError> {
        Self::new(reading_range, candidates, 0, self.state)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segments {
    reading: String,
    segments: Vec<Segment>,
    focused_segment: usize,
}

impl Segments {
    // Segments tile the whole reading: contiguous, non-empty, on grapheme
    // boundaries. The only empty value is an empty reading with focus 0.
    pub fn new(
        reading: String,
        segments: Vec<Segment>,
        focused_segment: usize,
    ) -> Result<Self, SegmentsError> {
        if segments.is_empty() {
            if reading.is_empty() && focused_segment == 0 {
                return Ok(Self {
                    reading,
                    segments,
                    focused_segment,
                });
            }
            return Err(SegmentsError::EmptySegments);
        }
        if focused_segment >= segments.len() {
            return Err(SegmentsError::FocusedSegmentOutOfRange);
        }
        validate_ranges(&reading, &segments)?;
        Ok(Self {
            reading,
            segments,
            focused_segment,
        })
    }

    pub fn reading(&self) -> &str {
        &self.reading
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn focused_segment(&self) -> usize {
        self.focused_segment
    }

    pub fn focused(&self) -> Option<&Segment> {
        self.segments.get(self.focused_segment)
    }

    pub fn move_focus(&mut self, index: usize) -> Result<(), SegmentsError> {
        if index >= self.segments.len() {
            return Err(SegmentsError::FocusedSegmentOutOfRange);
        }
        self.focused_segment = index;
        Ok(())
    }

    pub fn select_focused_candidate(&mut self, index: usize) -> Result<(), SegmentsError> {
        self.segments
            .get_mut(self.focused_segment)
            .ok_or(SegmentsError::FocusedSegmentOutOfRange)?
            .select(index)
    }

    pub fn select_focused_candidate_by_id(
        &mut self,
        id: &CandidateId,
    ) -> Result<(), SegmentsError> {
        self.segments
            .get_mut(self.focused_segment)
            .ok_or(SegmentsError::FocusedSegmentOutOfRange)?
            .select_by_id(id)
    }
}

fn validate_ranges(reading: &str, segments: &[Segment]) -> Result<(), SegmentsError> {
    let boundaries = GraphemeBoundaries::new(reading);
    let mut expected_start = ScalarIndex::new(0);
    for segment in segments {
        let range = segment.reading_range();
        if range.start != expected_start || range.is_empty() {
            return Err(SegmentsError::NonContiguousReadingRanges);
        }
        if boundaries.snap(range.start, SnapDirection::Backward) != range.start
            || boundaries.snap(range.end, SnapDirection::Backward) != range.end
        {
            return Err(SegmentsError::RangeSplitsGrapheme);
        }
        expected_start = range.end;
    }
    if expected_start != ScalarIndex::new(boundaries.scalar_len()) {
        return Err(SegmentsError::ReadingRangeDoesNotCoverInput);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryDecision {
    Break,
    Join,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentRule {
    pub previous_right_pos: Option<PosId>,
    pub next_left_pos: Option<PosId>,
    pub previous_surface_suffix: Option<String>,
    pub next_surface_prefix: Option<String>,
    pub decision: BoundaryDecision,
}

impl SegmentRule {
    fn matches(&self, previous: &PathNode, next: &PathNode) -> bool {
        self.previous_right_pos
            .is_none_or(|pos| pos == previous.right_pos_id)
            && self.next_left_pos.is_none_or(|pos| pos == next.left_pos_id)
            && self
                .previous_surface_suffix
                .as_ref()
                .is_none_or(|suffix| previous.surface.ends_with(suffix))
            && self
                .next_surface_prefix
                .as_ref()
                .is_none_or(|prefix| next.surface.starts_with(prefix))
    }
}

#[derive(Clone, Debug)]
pub struct Segmenter {
    rules: Vec<SegmentRule>,
    default_decision: BoundaryDecision,
}

impl Segmenter {
    pub fn new(rules: Vec<SegmentRule>, default_decision: BoundaryDecision) -> Self {
        Self {
            rules,
            default_decision,
        }
    }

    pub fn segment(&self, path: &BestPath) -> SegmentedPath {
        self.segment_nodes(&path.nodes)
    }

    /// Apply the same POS rules to a selected lattice path. Stage ② has an
    /// actual path rather than a Viterbi BestPath; fabricating a dummy total
    /// cost just to obtain its boundaries would misrepresent that authority.
    pub fn segment_nodes(&self, nodes: &[PathNode]) -> SegmentedPath {
        let mut groups: Vec<Vec<PathNode>> = Vec::new();
        for node in nodes {
            let should_break = groups
                .last()
                .and_then(|group| group.last())
                .is_some_and(|previous| self.decision(previous, node) == BoundaryDecision::Break);
            if should_break || groups.is_empty() {
                groups.push(Vec::new());
            }
            groups.last_mut().unwrap().push(node.clone());
        }
        SegmentedPath {
            groups: groups
                .into_iter()
                .map(|nodes| {
                    let reading_range =
                        ScalarRange::new(nodes.first().unwrap().start, nodes.last().unwrap().end);
                    SegmentedPathGroup {
                        reading_range,
                        nodes,
                    }
                })
                .collect(),
        }
    }

    // The first matching rule decides; unset fields match anything. A
    // boundary no rule matches takes the default decision.
    fn decision(&self, previous: &PathNode, next: &PathNode) -> BoundaryDecision {
        self.rules
            .iter()
            .find(|rule| rule.matches(previous, next))
            .map_or(self.default_decision, |rule| rule.decision)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentedPath {
    pub groups: Vec<SegmentedPathGroup>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentedPathGroup {
    pub reading_range: ScalarRange,
    pub nodes: Vec<PathNode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentsError {
    InvalidAuxiliarySurface,
    EmptyCandidateId,
    EmptyCandidatePaths,
    EmptyPathNodes,
    PathDoesNotCoverReadingRange,
    MixedCostSnapshots,
    CandidatePathsNotCostSorted,
    MixedCandidateSurfaces,
    EmptyReadingRange,
    EmptyCandidates,
    CandidateRangeMismatch,
    EmptySegments,
    FocusedCandidateOutOfRange,
    CandidateIdNotFound,
    FocusedSegmentOutOfRange,
    NonContiguousReadingRanges,
    ReadingRangeDoesNotCoverInput,
    RangeSplitsGrapheme,
}

impl fmt::Display for SegmentsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidAuxiliarySurface => {
                "auxiliary surface is empty, oversized or contains a control character"
            }
            Self::EmptyCandidateId => "candidate ID must not be empty",
            Self::EmptyCandidatePaths => "candidate group must contain a path",
            Self::EmptyPathNodes => "candidate path must contain a token node",
            Self::PathDoesNotCoverReadingRange => {
                "candidate path must contiguously cover its reading range"
            }
            Self::MixedCostSnapshots => "candidate paths must use one immutable cost snapshot",
            Self::CandidatePathsNotCostSorted => {
                "candidate paths must be ordered by nondecreasing cost"
            }
            Self::MixedCandidateSurfaces => "candidate group mixes normalized surfaces",
            Self::EmptyReadingRange => "segment reading range must not be empty",
            Self::EmptyCandidates => "segment must contain a candidate",
            Self::CandidateRangeMismatch => "candidate reading range differs from its segment",
            Self::EmptySegments => "non-empty reading must contain a segment",
            Self::FocusedCandidateOutOfRange => "focused candidate is outside the segment",
            Self::CandidateIdNotFound => "candidate ID is not present in the segment",
            Self::FocusedSegmentOutOfRange => "focused segment is outside the segment list",
            Self::NonContiguousReadingRanges => "segment reading ranges must be contiguous",
            Self::ReadingRangeDoesNotCoverInput => {
                "segment reading ranges must cover the entire reading"
            }
            Self::RangeSplitsGrapheme => "segment reading range splits a grapheme cluster",
        })
    }
}

impl std::error::Error for SegmentsError {}

#[cfg(test)]
mod tests {
    use muu_dict::{DictionaryTokenId, LeftContextId, RightContextId};

    use super::*;
    use crate::{NodeKind, TokenKey};

    fn node(start: usize, end: usize, surface: &str, pos: u16, token: u64) -> PathNode {
        node_with_pos_endpoints(start, end, surface, pos, pos, token)
    }

    fn node_with_pos_endpoints(
        start: usize,
        end: usize,
        surface: &str,
        left_pos: u16,
        right_pos: u16,
        token: u64,
    ) -> PathNode {
        PathNode {
            start: ScalarIndex::new(start),
            end: ScalarIndex::new(end),
            surface: surface.into(),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(left_pos),
            right_pos_id: PosId::new(right_pos),
            lexical_class: muu_dict::LexicalClass::General,
            token_key: TokenKey::Dictionary(DictionaryTokenId::new(token)),
            word_cost: 0,
            kind: NodeKind::Known,
        }
    }

    #[test]
    fn segmenter_groups_content_words_with_following_function_words() {
        let path = BestPath {
            nodes: vec![
                node(0, 1, "私", 1, 1),
                node(1, 2, "は", 2, 2),
                node(2, 4, "学校", 1, 3),
                node(4, 5, "に", 2, 4),
                node(5, 7, "行く", 3, 5),
            ],
            total_cost: 0,
            snapshot_revision: SnapshotRevision::new(0),
        };
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

        let segmented = segmenter.segment(&path);
        assert_eq!(segmented.groups.len(), 3);
        assert_eq!(
            segmented
                .groups
                .iter()
                .map(|group| {
                    group
                        .nodes
                        .iter()
                        .map(|node| node.surface.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>(),
            ["私は", "学校に", "行く"]
        );
        assert_eq!(
            segmented
                .groups
                .iter()
                .map(|group| group.reading_range)
                .collect::<Vec<_>>(),
            [
                ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2)),
                ScalarRange::new(ScalarIndex::new(2), ScalarIndex::new(5)),
                ScalarRange::new(ScalarIndex::new(5), ScalarIndex::new(7)),
            ]
        );
    }

    #[test]
    fn segmenter_uses_previous_right_and_next_left_pos_endpoints() {
        let path = BestPath {
            nodes: vec![
                node_with_pos_endpoints(0, 1, "前", 90, 1, 1),
                node_with_pos_endpoints(1, 2, "後", 2, 80, 2),
            ],
            total_cost: 0,
            snapshot_revision: SnapshotRevision::new(0),
        };
        let segmenter = Segmenter::new(
            vec![SegmentRule {
                previous_right_pos: Some(PosId::new(1)),
                next_left_pos: Some(PosId::new(2)),
                previous_surface_suffix: None,
                next_surface_prefix: None,
                decision: BoundaryDecision::Join,
            }],
            BoundaryDecision::Break,
        );

        assert_eq!(segmenter.segment(&path).groups.len(), 1);
    }

    #[test]
    fn ordered_surface_rule_can_override_a_pos_pair_rule() {
        let path = BestPath {
            nodes: vec![node(0, 1, "語", 1, 1), node(1, 2, "。", 2, 2)],
            total_cost: 0,
            snapshot_revision: SnapshotRevision::new(0),
        };
        let segmenter = Segmenter::new(
            vec![
                SegmentRule {
                    previous_right_pos: None,
                    next_left_pos: None,
                    previous_surface_suffix: None,
                    next_surface_prefix: Some("。".into()),
                    decision: BoundaryDecision::Break,
                },
                SegmentRule {
                    previous_right_pos: None,
                    next_left_pos: Some(PosId::new(2)),
                    previous_surface_suffix: None,
                    next_surface_prefix: None,
                    decision: BoundaryDecision::Join,
                },
            ],
            BoundaryDecision::Break,
        );

        assert_eq!(segmenter.segment(&path).groups.len(), 2);
    }

    #[test]
    fn public_model_owns_candidate_focus_and_rejects_broken_ranges() {
        let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2));
        let path = PathCandidate {
            nodes: vec![node(0, 2, "仮名", 1, 1)],
            cost: 10,
            snapshot_revision: SnapshotRevision::new(0),
        };
        let candidate = Candidate::from_group(range, vec![path]).unwrap();
        let segment = Segment::new(range, vec![candidate], 0, SegmentState::Unconfirmed).unwrap();
        let mut segments = Segments::new("かな".into(), vec![segment], 0).unwrap();

        assert_eq!(segments.focused_segment(), 0);
        assert_eq!(segments.focused().unwrap().focused_candidate(), 0);
        assert_eq!(segments.focused().unwrap().focused().surface(), "仮名");
        assert!(segments.move_focus(1).is_err());
    }

    #[test]
    fn model_rejects_scalar_ranges_that_split_a_grapheme() {
        let path = PathCandidate {
            nodes: vec![node(0, 1, "x", 1, 1)],
            cost: 0,
            snapshot_revision: SnapshotRevision::new(0),
        };
        let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(1));
        let candidate = Candidate::from_group(range, vec![path]).unwrap();
        let segment = Segment::new(range, vec![candidate], 0, SegmentState::Unconfirmed).unwrap();

        assert_eq!(
            Segments::new("e\u{301}".into(), vec![segment], 0),
            Err(SegmentsError::RangeSplitsGrapheme)
        );
    }
}

#[cfg(test)]
#[path = "segments/current_contract_tests.rs"]
mod current_contract_tests;
