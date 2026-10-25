use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use muu_dict::{LeftContextId, RightContextId};
use unicode_normalization::UnicodeNormalization;

use crate::{
    Candidate, ConversionError, CostSnapshot, Lattice, NodeKind, PathCandidate, PathNode,
    ScalarRange, SegmentsError, TokenKey,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundaryContext {
    pub prev_right_id: RightContextId,
    pub next_left_id: LeftContextId,
}

impl BoundaryContext {
    pub fn sentence(snapshot: &CostSnapshot<'_>) -> Self {
        Self {
            prev_right_id: snapshot.dictionary().bos_right_id(),
            next_left_id: snapshot.dictionary().eos_left_id(),
        }
    }
}

#[derive(Clone)]
struct SearchState {
    position: usize,
    previous_right_id: RightContextId,
    surface: String,
    nodes: Vec<PathNode>,
    cost: i32,
    required_boundary_reached: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
// Removing token identity from dominance would preserve the minimum cost:
// continuation costs now depend only on the right ID. It can nevertheless
// change equal-cost representative paths and the point where a work budget is
// exhausted. The token stays in the key so that outputs remain byte-for-byte
// what they were; measure such a pruning change separately before reducing it.
struct SearchStateKey {
    position: usize,
    previous_right_id: RightContextId,
    previous_token_key: Option<TokenKey>,
    surface: String,
    required_boundary_reached: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct StructuralStateKey {
    position: usize,
    previous_right_id: RightContextId,
    previous_token_key: Option<TokenKey>,
    required_boundary_reached: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DominancePolicy {
    PreservePathEvidence,
    SurfaceState,
}

impl SearchState {
    fn dominance_key(&self) -> SearchStateKey {
        // The nonempty prefix retains its previous token identity. An empty
        // prefix now uses None instead of its former left-neighbour token:
        // position zero has only this initial state, so there is no other
        // state to compare/prune and the changed sentinel cannot affect output.
        SearchStateKey {
            position: self.position,
            previous_right_id: self.previous_right_id,
            previous_token_key: self.nodes.last().map(|node| node.token_key.clone()),
            surface: self.surface.clone(),
            required_boundary_reached: self.required_boundary_reached,
        }
    }

    fn structural_key(&self) -> StructuralStateKey {
        StructuralStateKey {
            position: self.position,
            previous_right_id: self.previous_right_id,
            previous_token_key: self.nodes.last().map(|node| node.token_key.clone()),
            required_boundary_reached: self.required_boundary_reached,
        }
    }
}

struct QueueEntry {
    estimated_total: i32,
    sequence: u64,
    state: SearchState,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        (self.estimated_total, self.sequence) == (other.estimated_total, other.sequence)
    }
}

impl Eq for QueueEntry {}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; reverse both keys for minimum-first order.
        other
            .estimated_total
            .cmp(&self.estimated_total)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

/// Default number of distinct NFC candidate surfaces returned by one local search.
///
/// Fifty bounds combinatorial path enumeration. Explicit full-span dictionary entries
/// are merged separately up to [`MAX_CANDIDATE_SURFACES`].
pub const DEFAULT_LOCAL_N_BEST_CANDIDATES: usize = 50;

/// Maximum distinct surfaces exposed for one segment after exact path candidates and
/// explicit full-span dictionary entries are combined.
pub const MAX_CANDIDATE_SURFACES: usize = 4_096;

/// Maximum explicit dictionary provenance paths admitted for one reading.
/// Explicit paths bypass the combinatorial queue but share its resource ceiling.
pub const MAX_CANDIDATE_PROVENANCE_PER_READING: usize = MAX_LOCAL_SEARCH_QUEUED_STATES;

/// Maximum JSON-escaped UTF-8 bytes retained across the candidate surfaces of
/// one segment. Candidate IDs and the surrounding protocol envelope remain
/// comfortably inside the 2 MiB product budget when this guard is saturated.
pub const MAX_CANDIDATE_ENCODED_SURFACE_BYTES: usize = 1024 * 1024;

// These are fail-safe limits, not ordinary top-N policy. Dominance pruning keeps
// normal searches well below them; reaching one indicates an adversarial or
// unexpectedly ambiguous lattice and must fail closed instead of consuming the
// input method process.
const MAX_LOCAL_SEARCH_EXPANDED_STATES: usize = 8_192;
const MAX_LOCAL_SEARCH_QUEUED_STATES: usize = 16_384;
const MAX_LOCAL_SEARCH_COMPLETE_PATHS: usize = 2_048;

#[cfg_attr(not(test), allow(dead_code))]
struct LocalSearchOutcome {
    paths: Vec<PathCandidate>,
    expanded_states: usize,
    peak_queue_len: usize,
    dominance_pruned: usize,
}

/// Searches one fixed reading span using the supplied boundary and immutable
/// cost snapshot. Candidate generation, run rebuilding, and
/// constrained resize all call this function. Enumeration is exact and bounded:
/// it stops after the cost threshold containing the top
/// [`DEFAULT_LOCAL_N_BEST_CANDIDATES`] distinct NFC surfaces is exhausted.
///
/// The span is converted on its own lattice, and its costs include the
/// transitions from `prev_right_id` and into `next_left_id`, so the result
/// ranks the span as it sits between those neighbours. Paths are cheapest
/// first; every path found for a kept surface is returned, not one per
/// surface. Single dictionary or overlay words covering the whole span are
/// added outside the 50-surface cut. Budget exhaustion is an error
/// (`SearchBudgetExceeded`), never a truncated list.
pub fn local_search(
    reading_span: &str,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
) -> Result<Vec<PathCandidate>, ConversionError> {
    local_search_impl(
        reading_span,
        boundary,
        snapshot,
        None,
        DominancePolicy::PreservePathEvidence,
    )
}

/// Searches for the exact visible surface frontier while discarding only
/// higher-cost paths that reach the same continuation state with the same
/// accumulated surface.
///
/// This is the bounded availability path for initial conversion. It preserves
/// candidate surfaces and their best representative paths, but it does not
/// promise complete alternate-path evidence. Its caller marks the outcome
/// incomplete, and stage ② then does no language-model selection, because
/// the paths it would enumerate may be missing.
pub(crate) fn local_search_surface_dominant(
    reading_span: &str,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
) -> Result<Vec<PathCandidate>, ConversionError> {
    local_search_impl(
        reading_span,
        boundary,
        snapshot,
        None,
        DominancePolicy::SurfaceState,
    )
}

/// Finds the exact one-best path whose node boundary includes `required_boundary`.
///
/// Resize only needs this path to establish the token context on both sides of
/// the requested split. Visible candidates are generated by ordinary exact
/// top-N searches after that context is fixed. Structural-state dominance is
/// therefore safe here: a strictly more expensive structural state has the same
/// continuation choices and cannot win. Equal-cost states are preserved so the
/// final surface/token-key tie break remains exact, and no visible prefix
/// surface is discarded from a candidate set.
pub(crate) fn best_path_with_required_boundary(
    reading_span: &str,
    required_boundary: usize,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
) -> Result<PathCandidate, ConversionError> {
    let lattice =
        Lattice::build_with_overlays(reading_span, snapshot.dictionary(), snapshot.overlays())?;
    if required_boundary == 0 || required_boundary >= lattice.boundaries.scalar_len() {
        return Err(ConversionError::InvalidSegmentLength);
    }
    let suffix_costs = minimum_suffix_costs(&lattice, boundary, snapshot)?;
    let initial = SearchState {
        position: 0,
        previous_right_id: boundary.prev_right_id,
        surface: String::new(),
        nodes: Vec::new(),
        cost: 0,
        required_boundary_reached: false,
    };
    let initial_estimate =
        minimum_completion_cost(&lattice, &initial, boundary, snapshot, &suffix_costs)?;
    let mut queue = BinaryHeap::from([QueueEntry {
        estimated_total: initial_estimate,
        sequence: 0,
        state: initial.clone(),
    }]);
    let mut next_sequence = 1_u64;
    let mut best_states = BTreeMap::from([(initial.structural_key(), initial.cost)]);
    let mut best_path: Option<PathCandidate> = None;
    let mut expanded_states = 0_usize;
    let mut completed_paths = 0_usize;

    while let Some(entry) = queue.pop() {
        if best_path
            .as_ref()
            .is_some_and(|best| entry.estimated_total > best.cost)
        {
            break;
        }
        if expanded_states >= MAX_LOCAL_SEARCH_EXPANDED_STATES {
            return Err(ConversionError::SearchBudgetExceeded {
                expanded_states,
                queued_states: queue.len(),
                completed_paths,
                reading_len: reading_span.chars().count(),
            });
        }
        expanded_states = expanded_states
            .checked_add(1)
            .ok_or(ConversionError::CostOverflow)?;
        let state = entry.state;
        if best_states.get(&state.structural_key()) != Some(&state.cost) {
            continue;
        }
        if state.position == lattice.boundaries.scalar_len() {
            if !state.required_boundary_reached {
                continue;
            }
            let final_cost =
                snapshot.transition_cost(state.previous_right_id, boundary.next_left_id)?;
            let path = PathCandidate {
                nodes: state.nodes,
                cost: state
                    .cost
                    .checked_add(final_cost)
                    .ok_or(ConversionError::CostOverflow)?,
                snapshot_revision: snapshot.revision(),
            };
            completed_paths = completed_paths
                .checked_add(1)
                .ok_or(ConversionError::CostOverflow)?;
            if completed_paths > MAX_LOCAL_SEARCH_COMPLETE_PATHS {
                return Err(ConversionError::SearchBudgetExceeded {
                    expanded_states,
                    queued_states: queue.len(),
                    completed_paths,
                    reading_len: reading_span.chars().count(),
                });
            }
            if best_path
                .as_ref()
                .is_none_or(|best| compare_paths(&path, best).is_lt())
            {
                best_path = Some(path);
            }
            continue;
        }

        for &node_id in &lattice.nodes_starting_at[state.position] {
            let node = &lattice.nodes[node_id];
            if !matches!(
                node.kind,
                NodeKind::Known | NodeKind::Unknown(_) | NodeKind::Overlay
            ) {
                continue;
            }
            if !state.required_boundary_reached && node.end.get() > required_boundary {
                continue;
            }
            let required_boundary_reached =
                state.required_boundary_reached || node.end.get() == required_boundary;
            let transition_cost =
                snapshot.transition_cost(state.previous_right_id, node.left_id)?;
            let node_cost = snapshot.node_cost(node)?;
            let cost = state
                .cost
                .checked_add(transition_cost)
                .and_then(|value| value.checked_add(node_cost))
                .ok_or(ConversionError::CostOverflow)?;
            let mut nodes = state.nodes.clone();
            nodes.push(
                node.to_path_node()
                    .expect("non-endpoint lattice nodes always have token metadata"),
            );
            let next_state = SearchState {
                position: node.end.get(),
                previous_right_id: node.right_id,
                surface: String::new(),
                nodes,
                cost,
                required_boundary_reached,
            };
            let key = next_state.structural_key();
            if best_states.get(&key).is_some_and(|best| *best < cost) {
                continue;
            }
            let estimated_total = cost
                .checked_add(suffix_costs[node_id].ok_or(ConversionError::NoPath)?)
                .ok_or(ConversionError::CostOverflow)?;
            if best_path
                .as_ref()
                .is_some_and(|best| estimated_total > best.cost)
            {
                continue;
            }
            if queue.len() >= MAX_LOCAL_SEARCH_QUEUED_STATES {
                return Err(ConversionError::SearchBudgetExceeded {
                    expanded_states,
                    queued_states: queue.len(),
                    completed_paths,
                    reading_len: reading_span.chars().count(),
                });
            }
            best_states
                .entry(key)
                .and_modify(|best| *best = (*best).min(cost))
                .or_insert(cost);
            queue.push(QueueEntry {
                estimated_total,
                sequence: next_sequence,
                state: next_state,
            });
            next_sequence = next_sequence
                .checked_add(1)
                .ok_or(ConversionError::CostOverflow)?;
        }
    }

    best_path.ok_or(ConversionError::NoConstrainedPath)
}

fn local_search_impl(
    reading_span: &str,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
    required_boundary: Option<usize>,
    dominance_policy: DominancePolicy,
) -> Result<Vec<PathCandidate>, ConversionError> {
    local_search_outcome(
        reading_span,
        boundary,
        snapshot,
        required_boundary,
        dominance_policy,
    )
    .map(|outcome| outcome.paths)
}

fn local_search_outcome(
    reading_span: &str,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
    required_boundary: Option<usize>,
    dominance_policy: DominancePolicy,
) -> Result<LocalSearchOutcome, ConversionError> {
    let lattice =
        Lattice::build_with_overlays(reading_span, snapshot.dictionary(), snapshot.overlays())?;
    if required_boundary
        .is_some_and(|required| required == 0 || required > lattice.boundaries.scalar_len())
    {
        return Err(ConversionError::InvalidSegmentLength);
    }
    let suffix_costs = minimum_suffix_costs(&lattice, boundary, snapshot)?;
    let initial = SearchState {
        position: 0,
        previous_right_id: boundary.prev_right_id,
        surface: String::new(),
        nodes: Vec::new(),
        cost: 0,
        required_boundary_reached: required_boundary.is_none(),
    };
    let initial_estimate =
        minimum_completion_cost(&lattice, &initial, boundary, snapshot, &suffix_costs)?;
    let initial_key = initial.dominance_key();
    let mut queue = BinaryHeap::from([QueueEntry {
        estimated_total: initial_estimate,
        sequence: 0,
        state: initial,
    }]);
    let mut next_sequence = 1_u64;
    let mut paths = Vec::new();
    let mut distinct_surfaces = BTreeSet::new();
    let mut cutoff_cost = None;
    let mut best_state_costs = (dominance_policy == DominancePolicy::SurfaceState)
        .then(|| BTreeMap::from([(initial_key, 0)]));
    let mut expanded_states = 0_usize;
    let mut peak_queue_len = queue.len();
    let mut dominance_pruned = 0_usize;

    // A* with an exact remaining-cost estimate, so complete paths pop in
    // nondecreasing cost order even with negative word costs. The cut is
    // fixed when the 50th distinct surface completes; paths tied with it
    // still complete, and the sort below keeps at most 50 surfaces.
    while let Some(entry) = queue.pop() {
        if expanded_states >= MAX_LOCAL_SEARCH_EXPANDED_STATES {
            return Err(ConversionError::SearchBudgetExceeded {
                expanded_states,
                queued_states: queue.len(),
                completed_paths: paths.len(),
                reading_len: reading_span.chars().count(),
            });
        }
        expanded_states = expanded_states
            .checked_add(1)
            .ok_or(ConversionError::CostOverflow)?;
        if cutoff_cost.is_some_and(|cutoff| entry.estimated_total > cutoff) {
            break;
        }
        let state = entry.state;
        if let Some(costs) = best_state_costs.as_ref() {
            match costs.get(&state.dominance_key()) {
                Some(best) if state.cost <= *best => {}
                _ => continue,
            }
        }
        if state.position == lattice.boundaries.scalar_len() {
            if !state.required_boundary_reached {
                continue;
            }
            let final_cost =
                snapshot.transition_cost(state.previous_right_id, boundary.next_left_id)?;
            let path = PathCandidate {
                nodes: state.nodes,
                cost: state
                    .cost
                    .checked_add(final_cost)
                    .ok_or(ConversionError::CostOverflow)?,
                snapshot_revision: snapshot.revision(),
            };
            debug_assert_eq!(path.cost, entry.estimated_total);
            let is_new_surface = distinct_surfaces.insert(path.surface());
            if is_new_surface && distinct_surfaces.len() == DEFAULT_LOCAL_N_BEST_CANDIDATES {
                cutoff_cost = Some(path.cost);
            }
            if paths.len() >= MAX_LOCAL_SEARCH_COMPLETE_PATHS {
                return Err(ConversionError::SearchBudgetExceeded {
                    expanded_states,
                    queued_states: queue.len(),
                    completed_paths: paths.len(),
                    reading_len: reading_span.chars().count(),
                });
            }
            paths.push(path);
            continue;
        }

        for &node_id in &lattice.nodes_starting_at[state.position] {
            let node = &lattice.nodes[node_id];
            if !matches!(
                node.kind,
                NodeKind::Known | NodeKind::Unknown(_) | NodeKind::Overlay
            ) {
                continue;
            }
            // Unconstrained full-span dictionary/overlay nodes are merged
            // exactly after the bounded combinatorial search. Sending them
            // through this queue as well is redundant and lets thousands of
            // equal-cost explicit entries exhaust the complete-path budget
            // before the direct merge can run.
            if required_boundary.is_none()
                && state.position == 0
                && node.end.get() == lattice.boundaries.scalar_len()
                && matches!(node.kind, NodeKind::Known | NodeKind::Overlay)
            {
                continue;
            }
            if !state.required_boundary_reached
                && required_boundary.is_some_and(|required| node.end.get() > required)
            {
                continue;
            }
            let required_boundary_reached = state.required_boundary_reached
                || required_boundary.is_some_and(|required| node.end.get() == required);
            let transition_cost =
                snapshot.transition_cost(state.previous_right_id, node.left_id)?;
            let node_cost = snapshot.node_cost(node)?;
            let cost = state
                .cost
                .checked_add(transition_cost)
                .and_then(|value| value.checked_add(node_cost))
                .ok_or(ConversionError::CostOverflow)?;
            let mut nodes = state.nodes.clone();
            nodes.push(
                node.to_path_node()
                    .expect("non-endpoint lattice nodes always have token metadata"),
            );
            let surface = if dominance_policy == DominancePolicy::SurfaceState {
                let mut surface = state.surface.clone();
                surface.push_str(&node.surface);
                surface
            } else {
                String::new()
            };
            let estimated_total = cost
                .checked_add(suffix_costs[node_id].ok_or(ConversionError::NoPath)?)
                .ok_or(ConversionError::CostOverflow)?;
            if cutoff_cost.is_some_and(|cutoff| estimated_total > cutoff) {
                continue;
            }
            let next_state = SearchState {
                position: node.end.get(),
                previous_right_id: node.right_id,
                surface,
                nodes,
                cost,
                required_boundary_reached,
            };
            let dominance_key = next_state.dominance_key();
            if best_state_costs
                .as_ref()
                .and_then(|costs| costs.get(&dominance_key))
                .is_some_and(|best| *best <= cost)
            {
                dominance_pruned = dominance_pruned
                    .checked_add(1)
                    .ok_or(ConversionError::CostOverflow)?;
                continue;
            }
            if let Some(costs) = best_state_costs.as_mut() {
                costs.insert(dominance_key, cost);
            }
            if queue.len() >= MAX_LOCAL_SEARCH_QUEUED_STATES {
                return Err(ConversionError::SearchBudgetExceeded {
                    expanded_states,
                    queued_states: queue.len(),
                    completed_paths: paths.len(),
                    reading_len: reading_span.chars().count(),
                });
            }
            queue.push(QueueEntry {
                estimated_total,
                sequence: next_sequence,
                state: next_state,
            });
            peak_queue_len = peak_queue_len.max(queue.len());
            next_sequence = next_sequence
                .checked_add(1)
                .ok_or(ConversionError::CostOverflow)?;
        }
    }

    paths.sort_by(compare_paths);
    let mut selected_surfaces = BTreeSet::new();
    for path in &paths {
        if selected_surfaces.len() == DEFAULT_LOCAL_N_BEST_CANDIDATES {
            break;
        }
        selected_surfaces.insert(path.surface());
    }
    paths.retain(|path| selected_surfaces.contains(&path.surface()));
    if required_boundary.is_none() {
        for explicit in full_span_explicit_paths(&lattice, boundary, snapshot)? {
            if !paths
                .iter()
                .any(|path| path.cost == explicit.cost && path.nodes == explicit.nodes)
            {
                paths.push(explicit);
            }
        }
        paths.sort_by(compare_paths);
    }
    Ok(LocalSearchOutcome {
        paths,
        expanded_states,
        peak_queue_len,
        dominance_pruned,
    })
}

fn full_span_explicit_paths(
    lattice: &Lattice,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
) -> Result<Vec<PathCandidate>, ConversionError> {
    let end = lattice.boundaries.scalar_len();
    let mut paths = Vec::new();
    for &node_id in &lattice.nodes_starting_at[0] {
        let node = &lattice.nodes[node_id];
        if node.end.get() != end || !matches!(node.kind, NodeKind::Known | NodeKind::Overlay) {
            continue;
        }
        let transition_in = snapshot.transition_cost(boundary.prev_right_id, node.left_id)?;
        let transition_out = snapshot.transition_cost(node.right_id, boundary.next_left_id)?;
        let cost = transition_in
            .checked_add(snapshot.node_cost(node)?)
            .and_then(|value| value.checked_add(transition_out))
            .ok_or(ConversionError::CostOverflow)?;
        let path = PathCandidate {
            nodes: vec![
                node.to_path_node()
                    .expect("known and overlay nodes always have token metadata"),
            ],
            cost,
            snapshot_revision: snapshot.revision(),
        };
        paths.push(path);
    }
    Ok(paths)
}

// Exact cheapest completion from the end of each token node to EOS under
// `boundary.next_left_id`. Nodes are filled from the last start backwards;
// every node a node can continue to starts later, so it is already known.
fn minimum_suffix_costs(
    lattice: &Lattice,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
) -> Result<Vec<Option<i32>>, ConversionError> {
    let mut suffix_costs = vec![None; lattice.nodes.len()];
    let mut node_ids = lattice
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            matches!(
                node.kind,
                NodeKind::Known | NodeKind::Unknown(_) | NodeKind::Overlay
            )
        })
        .map(|(node_id, _)| node_id)
        .collect::<Vec<_>>();
    node_ids.sort_by_key(|&node_id| std::cmp::Reverse(lattice.nodes[node_id].start));

    for node_id in node_ids {
        let node = &lattice.nodes[node_id];
        let state = SearchState {
            position: node.end.get(),
            previous_right_id: node.right_id,
            surface: String::new(),
            nodes: Vec::new(),
            cost: 0,
            required_boundary_reached: true,
        };
        suffix_costs[node_id] = Some(minimum_completion_cost(
            lattice,
            &state,
            boundary,
            snapshot,
            &suffix_costs,
        )?);
    }
    Ok(suffix_costs)
}

fn minimum_completion_cost(
    lattice: &Lattice,
    state: &SearchState,
    boundary: &BoundaryContext,
    snapshot: &CostSnapshot<'_>,
    suffix_costs: &[Option<i32>],
) -> Result<i32, ConversionError> {
    if state.position == lattice.boundaries.scalar_len() {
        return snapshot.transition_cost(state.previous_right_id, boundary.next_left_id);
    }

    let mut minimum = None;
    for &node_id in &lattice.nodes_starting_at[state.position] {
        let node = &lattice.nodes[node_id];
        if !matches!(
            node.kind,
            NodeKind::Known | NodeKind::Unknown(_) | NodeKind::Overlay
        ) {
            continue;
        }
        let suffix = suffix_costs[node_id].ok_or(ConversionError::NoPath)?;
        let cost = snapshot
            .transition_cost(state.previous_right_id, node.left_id)?
            .checked_add(snapshot.node_cost(node)?)
            .and_then(|cost| cost.checked_add(suffix))
            .ok_or(ConversionError::CostOverflow)?;
        minimum = Some(minimum.map_or(cost, |current: i32| current.min(cost)));
    }
    minimum.ok_or(ConversionError::NoPath)
}

pub fn candidates_from_paths(
    reading_range: ScalarRange,
    paths: Vec<PathCandidate>,
) -> Result<Vec<Candidate>, ConversionError> {
    candidates_from_paths_impl(reading_range, paths, None)
}

/// Materialization must install the selected real path, not a cheaper path
/// that merely has the same surface (docs/src/internals/conversion.md「文節ごとの候補」). A singleton authority group keeps
/// paths[0] exact without breaking Candidate's cost-sorted-path invariant.
pub(crate) fn candidates_with_selected_path(
    reading_range: ScalarRange,
    paths: Vec<PathCandidate>,
    selected: PathCandidate,
    previous_surface: Option<&str>,
) -> Result<(Vec<Candidate>, usize), ConversionError> {
    let selected = Candidate::from_group(reading_range, vec![selected])?;
    let id = selected.id().clone();
    let mut candidates = candidates_from_paths_impl(reading_range, paths, Some(selected))?;
    let focus = candidates
        .iter()
        .position(|c| c.id() == &id)
        .ok_or(ConversionError::NoConstrainedPath)?;
    // Put the model's choice first, then the input's NFC surface when it
    // survives in this same-range catalog. One Space must offer a way back
    // when selection was wrong; the remaining alternatives keep cost order.
    // Moving whole Candidates preserves their IDs and paths[0] authority.
    //
    // The returned focus is always 0. Changed neighbours may give the return
    // surface a different representative. Keep that current ID and paths[0]:
    // commit and learning must describe the candidate actually accepted (docs/src/internals/conversion.md「文節ごとの候補」).
    // The selected surface needs no second copy, and an absent return surface
    // must not be reconstructed from the input's old path.
    let selected = candidates.remove(focus);
    let previous = previous_surface.and_then(|surface| {
        let surface = surface.nfc().collect::<String>();
        if surface == selected.surface() {
            return None;
        }
        candidates
            .iter()
            .position(|candidate| candidate.surface() == surface)
            .map(|index| candidates.remove(index))
    });
    if let Some(previous) = previous {
        candidates.insert(0, previous);
    }
    candidates.insert(0, selected);
    Ok((candidates, 0))
}

// Groups paths by NFC surface (cheapest representative first) and orders
// candidates by (representative cost, surface, ID). Without a selected path,
// the list is cut at MAX_CANDIDATE_SURFACES and then at the first candidate
// that would exceed the encoded-byte budget. With one, its slot and bytes
// are reserved first, and an ordinary candidate of the same surface gives
// way to it.
fn candidates_from_paths_impl(
    reading_range: ScalarRange,
    mut paths: Vec<PathCandidate>,
    selected: Option<Candidate>,
) -> Result<Vec<Candidate>, ConversionError> {
    paths.sort_by(compare_paths);
    let mut surface_to_group = BTreeMap::new();
    let mut groups: Vec<Vec<PathCandidate>> = Vec::new();
    for path in paths {
        let surface = path.surface();
        let index = match surface_to_group.get(&surface) {
            Some(index) => *index,
            None => {
                let index = groups.len();
                surface_to_group.insert(surface, index);
                groups.push(Vec::new());
                index
            }
        };
        groups[index].push(path);
    }
    let mut candidates = groups
        .into_iter()
        .map(|group| Candidate::from_group(reading_range, group))
        .collect::<Result<Vec<_>, SegmentsError>>()?;

    let selected_id = selected.as_ref().map(|c| c.id().clone());
    if let Some(selected) = selected {
        candidates.retain(|c| c.surface() != selected.surface());
        candidates.push(selected);
    }

    let compare = |left: &Candidate, right: &Candidate| {
        left.cost()
            .cmp(&right.cost())
            .then_with(|| left.surface().cmp(right.surface()))
            .then_with(|| left.id().cmp(right.id()))
    };
    candidates.sort_by(compare);
    if let Some(selected_id) = selected_id {
        let selected = candidates.remove(
            candidates
                .iter()
                .position(|c| c.id() == &selected_id)
                .ok_or(ConversionError::NoConstrainedPath)?,
        );
        let mut bytes = json_string_body_len(selected.surface());
        if bytes > MAX_CANDIDATE_ENCODED_SURFACE_BYTES {
            return Err(ConversionError::SelectedCandidateBudgetExceeded);
        }
        // Reserve both a slot and encoded bytes for the actual choice. It
        // must not disappear because ordinary local alternatives fill a cap.
        let mut kept = Vec::new();
        for candidate in candidates {
            if kept.len() == MAX_CANDIDATE_SURFACES - 1 {
                break;
            }
            let next = bytes.saturating_add(json_string_body_len(candidate.surface()));
            if next > MAX_CANDIDATE_ENCODED_SURFACE_BYTES {
                break;
            }
            bytes = next;
            kept.push(candidate);
        }
        kept.push(selected);
        kept.sort_by(compare);
        return Ok(kept);
    }
    candidates.truncate(MAX_CANDIDATE_SURFACES);
    let mut encoded_surface_bytes = 0usize;
    let mut retained = 0usize;
    for candidate in &candidates {
        let encoded_bytes = json_string_body_len(candidate.surface());
        let Some(next) = encoded_surface_bytes.checked_add(encoded_bytes) else {
            break;
        };
        if next > MAX_CANDIDATE_ENCODED_SURFACE_BYTES {
            break;
        }
        encoded_surface_bytes = next;
        retained += 1;
    }
    candidates.truncate(retained);
    Ok(candidates)
}

fn json_string_body_len(value: &str) -> usize {
    value
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
            '\u{00}'..='\u{1f}' => 6,
            other => other.len_utf8(),
        })
        .sum()
}

// Ties on cost break on the first node's surface, then on the token keys.
// This differs from `viterbi_1best`, which breaks ties on lattice node
// order, so an equal-cost first candidate need not be the Viterbi surface.
fn compare_paths(left: &PathCandidate, right: &PathCandidate) -> Ordering {
    left.cost
        .cmp(&right.cost)
        .then_with(|| {
            left.nodes
                .first()
                .map(|node| node.surface.as_str())
                .cmp(&right.nodes.first().map(|node| node.surface.as_str()))
        })
        .then_with(|| left.token_keys().cmp(right.token_keys()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use muu_dict::{
        Dictionary, DictionaryBuilder, DictionaryTokenId, LexiconEntry, PosId, UnknownClass,
        UnknownDefinition,
    };
    use tempfile::NamedTempFile;

    use super::*;
    use crate::{CostAdjustments, CostProvider, NoopCostProvider, SnapshotRevision};

    fn dictionary() -> (NamedTempFile, Dictionary) {
        let mut builder =
            DictionaryBuilder::new(6, 6, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"local-search-test\"\n");
        for entry in [
            LexiconEntry {
                reading: "あ".into(),
                surface: "亜".into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: 10,
            },
            LexiconEntry {
                reading: "い".into(),
                surface: "伊".into(),
                left_id: LeftContextId::new(2),
                right_id: RightContextId::new(2),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: 20,
            },
            LexiconEntry {
                reading: "あい".into(),
                surface: "が".into(),
                left_id: LeftContextId::new(3),
                right_id: RightContextId::new(3),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: 100,
            },
            LexiconEntry {
                reading: "あい".into(),
                surface: "か\u{3099}".into(),
                left_id: LeftContextId::new(4),
                right_id: RightContextId::new(4),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                word_cost: 110,
            },
        ] {
            builder.add_entry(entry);
        }
        builder.add_unknown(UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(5),
            right_id: RightContextId::new(5),
            left_pos_id: PosId::new(5),
            right_pos_id: PosId::new(5),
            word_cost: 1_000,
        });
        let bytes = builder.build().unwrap();
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        (file, dictionary)
    }

    fn repeated_surface_dictionary(duplicate_tokens: usize) -> (NamedTempFile, Dictionary) {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"repeated-surface-search-test\"\n");
        for choice in 0..duplicate_tokens {
            builder.add_entry(LexiconEntry {
                reading: "ー".into(),
                surface: "ー".into(),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: i32::try_from(choice).unwrap(),
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
        (file, dictionary)
    }

    fn distinct_surface_dictionary() -> (NamedTempFile, Dictionary) {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"distinct-surface-search-test\"\n");
        for surface in ["甲", "乙"] {
            builder.add_entry(LexiconEntry {
                reading: "ー".into(),
                surface: surface.into(),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: 0,
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
        (file, dictionary)
    }

    fn canonical_prefix_collision_dictionary() -> (NamedTempFile, Dictionary) {
        fn permutations(chars: &mut [char], start: usize, output: &mut Vec<String>) {
            if output.len() >= 51 {
                return;
            }
            if start == chars.len() {
                output.push(chars.iter().collect());
                return;
            }
            for index in start..chars.len() {
                chars.swap(start, index);
                permutations(chars, start + 1, output);
                chars.swap(start, index);
                if output.len() >= 51 {
                    return;
                }
            }
        }

        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"canonical-prefix-collision-test\"\n");

        // These marks have distinct canonical combining classes. Every
        // permutation normalizes to the same NFC surface, but the raw prefix
        // strings are distinct until final PathCandidate surface grouping.
        let mut marks = ['\u{0334}', '\u{0323}', '\u{0300}', '\u{0315}', '\u{0345}'];
        let mut equivalent_prefixes = Vec::new();
        permutations(&mut marks, 0, &mut equivalent_prefixes);
        assert_eq!(equivalent_prefixes.len(), 51);
        for (cost, suffix) in equivalent_prefixes.into_iter().enumerate() {
            builder.add_entry(LexiconEntry {
                reading: "あ".into(),
                surface: format!("a{suffix}"),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: i32::try_from(cost).unwrap(),
            });
        }
        builder.add_entry(LexiconEntry {
            reading: "あ".into(),
            surface: "別".into(),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 100,
        });
        for (reading, surface) in [("い", "X"), ("う", "Y")] {
            builder.add_entry(LexiconEntry {
                reading: reading.into(),
                surface: surface.into(),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: 0,
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
        (file, dictionary)
    }

    #[test]
    fn repeated_same_surface_tokens_are_dominance_bounded() {
        let (_file, dictionary) = repeated_surface_dictionary(4);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);

        for repeated in [1, 2, 4, 8, 18, 32] {
            let reading = format!("{}++", "ー".repeat(repeated));
            let outcome = local_search_outcome(
                &reading,
                &boundary,
                &snapshot,
                None,
                DominancePolicy::SurfaceState,
            )
            .unwrap();
            assert!(!outcome.paths.is_empty(), "repeated={repeated}");
            assert!(
                outcome.paths.iter().all(|path| path.surface() == reading),
                "repeated={repeated}"
            );
            assert!(outcome.expanded_states <= 2_000, "repeated={repeated}");
            assert!(outcome.peak_queue_len <= 256, "repeated={repeated}");
            assert!(outcome.dominance_pruned > 0, "repeated={repeated}");
        }
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_repeated_prolonged_marks_complete_within_the_search_budget() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);
        let reading = format!("{}++", "ー".repeat(18));

        let outcome = local_search_outcome(
            &reading,
            &boundary,
            &snapshot,
            None,
            DominancePolicy::SurfaceState,
        )
        .unwrap();
        assert!(!outcome.paths.is_empty());
        assert!(outcome.expanded_states <= MAX_LOCAL_SEARCH_EXPANDED_STATES);
        assert!(outcome.peak_queue_len <= MAX_LOCAL_SEARCH_QUEUED_STATES);
        assert!(outcome.dominance_pruned > 0);
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_repeated_prolonged_marks_stay_bounded_without_preview_dominance() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);
        let reading = format!("{}++", "ー".repeat(18));

        let paths = local_search(&reading, &boundary, &snapshot)
            .expect("direct full-span merge lets the pinned production shape complete safely");
        assert!(
            paths.len() <= MAX_LOCAL_SEARCH_COMPLETE_PATHS + MAX_CANDIDATE_SURFACES,
            "bounded combinatorial evidence plus direct surfaces exceeded the declared guards"
        );
    }

    #[test]
    fn adversarial_distinct_surfaces_fail_closed_with_bounded_telemetry() {
        let (_file, dictionary) = distinct_surface_dictionary();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);
        let reading = "ー".repeat(32);

        let error = local_search(&reading, &boundary, &snapshot).unwrap_err();
        let ConversionError::SearchBudgetExceeded {
            expanded_states,
            queued_states,
            completed_paths,
            reading_len,
        } = &error
        else {
            panic!("unexpected search result: {error:?}");
        };
        assert!(*expanded_states <= MAX_LOCAL_SEARCH_EXPANDED_STATES);
        assert!(*queued_states <= MAX_LOCAL_SEARCH_QUEUED_STATES);
        assert!(*completed_paths <= MAX_LOCAL_SEARCH_COMPLETE_PATHS);
        assert_eq!(*reading_len, 32);
        assert_eq!(
            error.to_string(),
            format!(
                "local search truncated: expanded_states={expanded_states} queued_states={queued_states} completed_paths={completed_paths} reading_len=32"
            )
        );
    }

    #[test]
    fn exact_surface_dominance_matches_exhaustive_nfc_and_suffix_collision_oracle() {
        let (_file, dictionary) = canonical_prefix_collision_dictionary();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);
        let exhaustive = local_search("あいう", &boundary, &snapshot).unwrap();
        let bounded = local_search_surface_dominant("あいう", &boundary, &snapshot).unwrap();
        let exhaustive_surfaces = exhaustive
            .iter()
            .map(PathCandidate::surface)
            .collect::<BTreeSet<_>>();
        let bounded_surfaces = bounded
            .iter()
            .map(PathCandidate::surface)
            .collect::<BTreeSet<_>>();

        assert!(exhaustive_surfaces.contains("別XY"));
        assert_eq!(bounded_surfaces, exhaustive_surfaces);
    }

    fn token_id(dictionary: &Dictionary, surface: &str) -> DictionaryTokenId {
        dictionary
            .common_prefix_search("あい")
            .unwrap()
            .into_iter()
            .find(|prefix| prefix.reading_length_bytes == "あい".len())
            .unwrap()
            .tokens
            .into_iter()
            .find(|token| token.surface == surface)
            .unwrap()
            .token_id
    }

    #[test]
    fn local_search_includes_both_boundaries_and_is_cost_sorted() {
        let (_file, dictionary) = dictionary();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);

        let paths = local_search("あい", &boundary, &snapshot).unwrap();
        assert_eq!(paths[0].surface(), "亜伊");
        assert_eq!(paths[0].cost, 30);
        assert!(paths.windows(2).all(|pair| pair[0].cost <= pair[1].cost));
        assert!(
            paths
                .iter()
                .all(|path| path.snapshot_revision == snapshot.revision())
        );
    }

    struct FixedProvider(DictionaryTokenId);

    impl CostProvider for FixedProvider {
        fn capture(&self) -> CostAdjustments {
            let mut adjustments = CostAdjustments::new(SnapshotRevision::new(7));
            adjustments.add_node_cost(TokenKey::Dictionary(self.0), -200);
            adjustments
        }
    }

    #[test]
    fn cost_provider_snapshot_can_change_the_path_but_stays_immutable() {
        let (_file, dictionary) = dictionary();
        let baseline = CostSnapshot::capture(&dictionary, &NoopCostProvider);
        let target_token = token_id(&dictionary, "が");
        let adjusted = CostSnapshot::capture(&dictionary, &FixedProvider(target_token));

        assert_eq!(
            local_search("あい", &BoundaryContext::sentence(&baseline), &baseline).unwrap()[0]
                .surface(),
            "亜伊"
        );
        assert_eq!(
            local_search("あい", &BoundaryContext::sentence(&adjusted), &adjusted).unwrap()[0]
                .surface(),
            "が"
        );
        let lattice = Lattice::build("あい", &dictionary).unwrap();
        let global = crate::viterbi_1best(&lattice, &adjusted).unwrap();
        let local = local_search("あい", &BoundaryContext::sentence(&adjusted), &adjusted).unwrap();
        assert_eq!(global.surface(), local[0].surface());
        assert_eq!(global.total_cost, local[0].cost);
        assert_eq!(global.snapshot_revision, local[0].snapshot_revision);
        assert_eq!(adjusted.revision(), SnapshotRevision::new(7));
    }

    #[test]
    fn equal_cost_paths_use_surface_then_token_key_tie_break() {
        fn path(surface: &str, token_id: u64) -> PathCandidate {
            PathCandidate {
                nodes: vec![PathNode {
                    start: crate::ScalarIndex::new(0),
                    end: crate::ScalarIndex::new(1),
                    surface: surface.into(),
                    left_id: LeftContextId::new(0),
                    right_id: RightContextId::new(0),
                    left_pos_id: PosId::new(0),
                    right_pos_id: PosId::new(0),
                    lexical_class: muu_dict::LexicalClass::General,
                    token_key: TokenKey::Dictionary(DictionaryTokenId::new(token_id)),
                    word_cost: 0,
                    kind: NodeKind::Known,
                }],
                cost: 10,
                snapshot_revision: SnapshotRevision::new(0),
            }
        }

        let mut paths = [path("い", 2), path("あ", 2), path("あ", 1)];
        paths.sort_by(compare_paths);

        assert_eq!(paths[0].nodes[0].surface, "あ");
        assert_eq!(
            paths[0].nodes[0].token_key,
            TokenKey::Dictionary(DictionaryTokenId::new(1))
        );
        assert_eq!(paths[1].nodes[0].surface, "あ");
        assert_eq!(
            paths[1].nodes[0].token_key,
            TokenKey::Dictionary(DictionaryTokenId::new(2))
        );
        assert_eq!(paths[2].nodes[0].surface, "い");
    }

    #[test]
    fn candidate_construction_preserves_paths_costs_and_the_input() {
        let (_file, dictionary) = dictionary();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let paths = local_search("あい", &BoundaryContext::sentence(&snapshot), &snapshot).unwrap();
        let original = paths.clone();
        let range = ScalarRange::new(crate::ScalarIndex::new(0), crate::ScalarIndex::new(2));
        let candidates = candidates_from_paths(range, paths.clone()).unwrap();
        assert_eq!(candidates[0].surface(), "亜伊");
        let mut actual = candidates
            .iter()
            .flat_map(|candidate| {
                assert_eq!(candidate.cost(), Some(candidate.paths()[0].cost));
                assert_eq!(
                    candidate.id(),
                    &crate::CandidateId::from_path(range, &candidate.paths()[0])
                );
                candidate.paths().iter().cloned()
            })
            .collect::<Vec<_>>();
        let mut expected = paths.clone();
        actual.sort_by(compare_paths);
        expected.sort_by(compare_paths);
        assert_eq!(actual, expected);
        assert_eq!(paths, original);
    }

    #[test]
    fn encoded_surface_budget_counts_json_string_escapes() {
        assert_eq!(json_string_body_len("plain"), 5);
        assert_eq!(json_string_body_len("記号"), 6);
        assert_eq!(json_string_body_len("\"\\\n"), 6);
        assert_eq!(json_string_body_len("\u{001f}"), 6);
    }

    #[test]
    fn nfc_dedup_keeps_first_cost_ordered_path_and_provenance() {
        let (_file, dictionary) = dictionary();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let paths = local_search("あい", &BoundaryContext::sentence(&snapshot), &snapshot).unwrap();
        let candidates = candidates_from_paths(
            ScalarRange::new(crate::ScalarIndex::new(0), crate::ScalarIndex::new(2)),
            paths,
        )
        .unwrap();

        let voiced_kana = candidates
            .iter()
            .find(|candidate| candidate.surface() == "が")
            .unwrap();
        assert_eq!(voiced_kana.cost(), Some(100));
        assert_eq!(voiced_kana.paths().len(), 2);
        assert_eq!(voiced_kana.paths()[0].cost, 100);
        assert_eq!(voiced_kana.paths()[1].cost, 110);
    }

    #[test]
    fn bounded_local_n_best_is_exact_and_preserves_viterbi_1best() {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"bounded-local-n-best-test\"\n");
        for index in (0..MAX_CANDIDATE_SURFACES + 10).rev() {
            builder.add_entry(LexiconEntry {
                reading: "あ".into(),
                surface: format!("候補{index:02}"),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: i32::try_from(index).unwrap(),
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
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);

        let lattice = Lattice::build("あ", &dictionary).unwrap();
        let one_best = crate::viterbi_1best(&lattice, &snapshot).unwrap();
        let paths = local_search("あ", &boundary, &snapshot).unwrap();
        let candidates = candidates_from_paths(
            ScalarRange::new(crate::ScalarIndex::new(0), crate::ScalarIndex::new(1)),
            paths.clone(),
        )
        .unwrap();
        assert_eq!(candidates.len(), MAX_CANDIDATE_SURFACES);
        assert!(paths.len() >= MAX_CANDIDATE_SURFACES);
        assert_eq!(paths[0].nodes, one_best.nodes);
        assert_eq!(paths[0].cost, one_best.total_cost);
        assert_eq!(candidates[0].surface(), "候補00");
        assert_eq!(candidates.last().unwrap().surface(), "候補4095");
        assert!(
            candidates
                .windows(2)
                .all(|pair| pair[0].cost() <= pair[1].cost())
        );
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_high_cardinality_readings_fit_the_recalibrated_budget() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);

        for reading in ["こうしょう", "たけお", "まさお"] {
            let paths = local_search(reading, &boundary, &snapshot).unwrap();
            let generated = paths
                .iter()
                .map(PathCandidate::surface)
                .collect::<BTreeSet<_>>();
            let direct = dictionary
                .common_prefix_search(reading)
                .unwrap()
                .into_iter()
                .find(|matched| matched.reading_length_bytes == reading.len())
                .unwrap()
                .tokens
                .into_iter()
                .map(|token| token.surface.to_string())
                .collect::<BTreeSet<_>>();
            assert!(direct.is_subset(&generated));
        }
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_largest_direct_reading_set_remains_reachable() {
        let Some(dictionary) = crate::real_dictionary::shared() else {
            return;
        };
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);
        let reading = "こうじ";
        let direct = dictionary
            .common_prefix_search(reading)
            .unwrap()
            .into_iter()
            .find(|matched| matched.reading_length_bytes == reading.len())
            .unwrap()
            .tokens
            .into_iter()
            .map(|token| token.surface.to_string())
            .collect::<BTreeSet<_>>();
        let generated = local_search(reading, &boundary, &snapshot)
            .unwrap()
            .into_iter()
            .map(|path| path.surface())
            .collect::<BTreeSet<_>>();

        assert!(
            direct.len() >= 261,
            "the expanded production dictionary must not lose the original high-cardinality set"
        );
        assert!(direct.len() <= MAX_CANDIDATE_SURFACES);
        assert!(direct.is_subset(&generated));
    }

    #[test]
    fn bounded_best_first_matches_an_exhaustive_multi_node_oracle() {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"bounded-oracle-test\"\n");
        for (position, reading) in ["あ", "い", "う"].into_iter().enumerate() {
            for choice in 0..8 {
                builder.add_entry(LexiconEntry {
                    reading: reading.into(),
                    surface: format!("{position}{choice}"),
                    left_id: LeftContextId::new(0),
                    right_id: RightContextId::new(0),
                    left_pos_id: PosId::new(0),
                    right_pos_id: PosId::new(0),
                    word_cost: choice * 3 + position as i32,
                });
            }
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
        let snapshot = CostSnapshot::baseline(&dictionary);
        let boundary = BoundaryContext::sentence(&snapshot);
        let lattice = Lattice::build("あいう", &dictionary).unwrap();

        // This intentionally retains the old exhaustive DFS as an independent
        // test oracle; production must not call it.
        let mut stack = vec![SearchState {
            position: 0,
            previous_right_id: boundary.prev_right_id,
            surface: String::new(),
            nodes: Vec::new(),
            cost: 0,
            required_boundary_reached: true,
        }];
        let mut exhaustive = Vec::new();
        while let Some(state) = stack.pop() {
            if state.position == lattice.boundaries.scalar_len() {
                let final_cost = snapshot
                    .transition_cost(state.previous_right_id, boundary.next_left_id)
                    .unwrap();
                exhaustive.push(PathCandidate {
                    nodes: state.nodes,
                    cost: state.cost.checked_add(final_cost).unwrap(),
                    snapshot_revision: snapshot.revision(),
                });
                continue;
            }
            for &node_id in &lattice.nodes_starting_at[state.position] {
                let node = &lattice.nodes[node_id];
                if !matches!(node.kind, NodeKind::Known | NodeKind::Unknown(_)) {
                    continue;
                }
                let cost = state
                    .cost
                    .checked_add(
                        snapshot
                            .transition_cost(state.previous_right_id, node.left_id)
                            .unwrap(),
                    )
                    .and_then(|cost| cost.checked_add(snapshot.node_cost(node).unwrap()))
                    .unwrap();
                let mut nodes = state.nodes.clone();
                nodes.push(node.to_path_node().unwrap());
                let mut surface = state.surface.clone();
                surface.push_str(&node.surface);
                stack.push(SearchState {
                    position: node.end.get(),
                    previous_right_id: node.right_id,
                    surface,
                    nodes,
                    cost,
                    required_boundary_reached: true,
                });
            }
        }
        exhaustive.sort_by(compare_paths);
        exhaustive.truncate(DEFAULT_LOCAL_N_BEST_CANDIDATES);

        let bounded = local_search("あいう", &boundary, &snapshot).unwrap();
        assert_eq!(bounded, exhaustive);
    }
}
