//! Stage ② enumerates distinct whole-lattice surfaces, not combinations of
//! stage ①'s segment catalogs (docs/src/internals/conversion.md「言語モデルによる選び直し」). A bounded DAG frontier keeps the exact
//! top K or returns an error; a partially explored frontier is never a result.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

use unicode_normalization::UnicodeNormalization;

use crate::{
    ConversionError, CostSnapshot, Lattice, LatticeNode, NodeKind, PathNode, SnapshotRevision,
};

/// Separate from local candidate search: exhausting an optional selector's
/// resources must not change stage ① or silently approximate its alternatives.
#[derive(Clone, Copy, Debug)]
pub struct HypothesisBudget {
    pub max_nodes: usize,
    pub max_prefixes: usize,
    /// Edge inspections in has_alternative; independent of enumeration.
    pub max_reachability_edges: usize,
    /// Predecessor seeding plus heap pops in enumerate, not a whole-run total.
    pub max_enumeration_steps: usize,
    /// Peak tracked payload (docs/src/internals/conversion.md「言語モデルによる選び直し」): lattice accounting plus owned working array
    /// slots, string buffers, prefix/seen/heap storage, temporary NFC surfaces,
    /// and simultaneous frontier/result/actual-baseline path copies. Working
    /// Vec/String buffers are charged by capacity; occupied hash keys include
    /// their String headers. Allocator/hash-table bookkeeping, stack frames,
    /// normalization-library scratch space and borrowed input/store resources
    /// are outside this data budget, so it is not an RSS limit.
    pub max_bytes: usize,
}

impl Default for HypothesisBudget {
    fn default() -> Self {
        Self {
            max_nodes: 65_536,
            max_prefixes: 262_144,
            max_reachability_edges: 1_000_000,
            max_enumeration_steps: 1_000_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// The actual path of a scored surface. It has no segment candidate indices:
/// those candidates do not exist until a selected path is materialized (docs/src/internals/conversion.md「言語モデルによる選び直し」).
///
/// `surface` is the NFC of the concatenated node surfaces and is the identity
/// the ranking deduplicates on. `base_cost` is the whole path's cost under the
/// snapshot, BOS and EOS transitions included. Exactly one hypothesis in an
/// enumeration has `is_baseline`: the caller's actual path, never a cheaper
/// path with the same surface.
#[derive(Clone, Debug)]
pub struct FullHypothesis {
    pub surface: String,
    pub base_cost: i64,
    pub nodes: Vec<PathNode>,
    pub is_baseline: bool,
    pub snapshot_revision: SnapshotRevision,
}

impl FullHypothesis {
    pub fn surface(&self) -> &str {
        &self.surface
    }
    pub fn base_cost(&self) -> i64 {
        self.base_cost
    }
    pub fn is_baseline(&self) -> bool {
        self.is_baseline
    }
    pub fn nodes(&self) -> &[PathNode] {
        &self.nodes
    }
}

/// One immutable lattice per conversion. Each run constrains this same input
/// differently, because earlier runs' applied paths become frozen neighbours.
pub struct FullHypothesisLattice {
    lattice: Lattice,
    budget: HypothesisBudget,
    revision: SnapshotRevision,
}

impl FullHypothesisLattice {
    /// Builds the whole-reading lattice within `max_nodes` and `max_bytes`.
    /// Over either, the build fails as a budget error; the selector then
    /// leaves every run at its baseline, and stage ① is unaffected.
    pub fn build(
        reading: &str,
        snapshot: &CostSnapshot<'_>,
        budget: HypothesisBudget,
    ) -> Result<Self, ConversionError> {
        let lattice = Lattice::build_bounded(
            reading,
            snapshot.dictionary(),
            snapshot.overlays(),
            budget.max_nodes,
            budget.max_bytes,
        )?;
        Ok(Self {
            lattice,
            budget,
            revision: snapshot.revision(),
        })
    }

    /// Frozen nodes are real input paths[0], including nodes introduced by
    /// stage ①'s span-local symbol/number construction. Such nodes may not be
    /// in a whole-reading lattice, so preserve them as explicit anchors rather
    /// than substituting another path with the same visible text (docs/src/internals/conversion.md「言語モデルによる選び直し」).
    ///
    /// `fixed` must be ordered, non-overlapping and inside the reading. A
    /// lattice node may stay only if it does not overlap any anchor, or is
    /// that anchor exactly (see `matches_node`). Because paths cover the
    /// reading contiguously, every remaining path passes through every anchor.
    /// An anchor with no identical lattice node is appended as a new node.
    /// The node array is copied per run, which is why twice the lattice's
    /// retained bytes must fit before anything is built.
    pub fn constrain(&self, fixed: &[PathNode]) -> Result<ConstrainedHypotheses, ConversionError> {
        let len = self.lattice.reading().chars().count();
        if fixed.iter().any(|n| n.start >= n.end || n.end.get() > len)
            || fixed.windows(2).any(|p| p[0].end > p[1].start)
        {
            return Err(ConversionError::NoConstrainedPath);
        }
        if self.lattice.retained_bytes.saturating_mul(2) > self.budget.max_bytes {
            return Err(ConversionError::FullHypothesisBudgetExceeded);
        }
        let mut nodes = self.lattice.nodes().to_vec();
        let mut allowed = vec![true; nodes.len()];
        for (id, node) in nodes.iter().enumerate() {
            if matches!(node.kind, NodeKind::Bos | NodeKind::Eos) {
                continue;
            }
            // Ordered, disjoint anchors allow logarithmic lookup instead of
            // scanning every frozen node for every lattice node on long input.
            let index = fixed.partition_point(|f| f.end <= node.start);
            allowed[id] = fixed
                .get(index)
                .is_none_or(|f| f.start >= node.end || matches_node(node, f));
        }
        let mut anchor_bytes = self.lattice.retained_bytes.saturating_mul(2);
        for anchor in fixed {
            if let Some(&id) = self
                .lattice
                .nodes_starting_at(anchor.start)
                .iter()
                .find(|&&id| matches_node(&nodes[id], anchor))
            {
                allowed[id] = true;
            } else {
                if nodes.len() >= self.budget.max_nodes {
                    return Err(ConversionError::FullHypothesisBudgetExceeded);
                }
                anchor_bytes = anchor_bytes
                    .saturating_add(std::mem::size_of::<LatticeNode>())
                    .saturating_add(anchor.surface.len());
                if let crate::TokenKey::Unknown { surface, .. } = &anchor.token_key {
                    anchor_bytes = anchor_bytes.saturating_add(surface.len());
                }
                if anchor_bytes > self.budget.max_bytes {
                    return Err(ConversionError::FullHypothesisBudgetExceeded);
                }
                nodes.push(LatticeNode {
                    start: anchor.start,
                    end: anchor.end,
                    surface: anchor.surface.clone(),
                    left_id: anchor.left_id,
                    right_id: anchor.right_id,
                    left_pos_id: Some(anchor.left_pos_id),
                    right_pos_id: Some(anchor.right_pos_id),
                    lexical_class: anchor.lexical_class,
                    token_key: Some(anchor.token_key.clone()),
                    word_cost: anchor.word_cost,
                    kind: anchor.kind,
                });
                allowed.push(true);
            }
        }
        let mut starting = vec![Vec::new(); len + 1];
        let mut ending = vec![Vec::new(); len + 1];
        let mut bytes = self.lattice.retained_bytes;
        for (id, node) in nodes.iter().enumerate() {
            bytes = bytes.saturating_add(std::mem::size_of::<LatticeNode>() + node.surface.len());
            if let Some(crate::TokenKey::Unknown { surface, .. }) = &node.token_key {
                bytes = bytes.saturating_add(surface.len());
            }
            if allowed[id] {
                starting[node.start.get()].push(id);
                ending[node.end.get()].push(id);
            }
        }
        bytes = bytes
            .saturating_add((len + 1) * 2 * std::mem::size_of::<Vec<usize>>())
            .saturating_add(nodes.len() * (2 * std::mem::size_of::<usize>() + 1));
        if bytes > self.budget.max_bytes {
            return Err(ConversionError::FullHypothesisBudgetExceeded);
        }
        Ok(ConstrainedHypotheses {
            nodes,
            starting,
            ending,
            len,
            bos: self.lattice.bos,
            eos: self.lattice.eos,
            budget: self.budget,
            revision: self.revision,
            bytes,
        })
    }
}

// Every field must agree. A node with the same span and surface but another
// token, context or cost is a different node, and is not used in place of the
// anchor.
fn matches_node(node: &LatticeNode, path: &PathNode) -> bool {
    node.start == path.start
        && node.end == path.end
        && node.surface == path.surface
        && node.left_id == path.left_id
        && node.right_id == path.right_id
        && node.left_pos_id == Some(path.left_pos_id)
        && node.right_pos_id == Some(path.right_pos_id)
        && node.lexical_class == path.lexical_class
        && node.token_key.as_ref() == Some(&path.token_key)
        && node.word_cost == path.word_cost
        && node.kind == path.kind
}

/// A run's constrained graph. Reachability and enumeration use identical
/// anchors, so the eligibility guard cannot describe a different search.
pub struct ConstrainedHypotheses {
    nodes: Vec<LatticeNode>,
    starting: Vec<Vec<usize>>,
    ending: Vec<Vec<usize>>,
    len: usize,
    bos: usize,
    eos: usize,
    budget: HypothesisBudget,
    revision: SnapshotRevision,
    bytes: usize,
}

// What `has_alternative` knows about the prefixes ending at a node: none
// reach it, all reaching paths share one normalized surface, or there are at
// least two. `Many` is absorbing, so no further strings are built for it.
enum Diversity {
    Unreachable,
    One(String),
    Many,
}

impl ConstrainedHypotheses {
    /// Decide whether the constrained language is a singleton before checking
    /// LM availability (the reason order in docs/src/internals/conversion.md「言語モデルによる選び直し」). This computes no ranked paths and
    /// no costs. At a common ending node all prefixes have the same possible
    /// continuations; canonical concatenation preserves distinct normalized
    /// strings, so a multiple-surface state remains multiple downstream.
    ///
    /// True when the constrained graph yields two or more surfaces, or a
    /// single surface that is not `baseline`'s. No path to EOS is an error
    /// (`NoConstrainedPath`), not false. Edge inspections count against
    /// `max_reachability_edges` only.
    pub fn has_alternative(&self, baseline: &str) -> Result<bool, ConversionError> {
        let mut bytes = self.bytes;
        charge_bytes(
            &mut bytes,
            self.nodes.len() * std::mem::size_of::<Diversity>(),
            self.budget,
        )?;
        let mut diversity: Vec<Diversity> = (0..self.nodes.len())
            .map(|_| Diversity::Unreachable)
            .collect();
        diversity[self.bos] = Diversity::One(String::new());
        let mut steps = 0;
        for pos in 0..=self.len {
            for &id in &self.starting[pos] {
                if id == self.bos {
                    continue;
                }
                let mut current = Diversity::Unreachable;
                for &prev in &self.ending[pos] {
                    if prev == id {
                        continue;
                    }
                    step(&mut steps, self.budget.max_reachability_edges)?;
                    match &diversity[prev] {
                        Diversity::Unreachable => {}
                        Diversity::Many => {
                            current = Diversity::Many;
                            break;
                        }
                        Diversity::One(prefix) => {
                            let current_bytes = match &current {
                                Diversity::One(s) => s.capacity(),
                                _ => 0,
                            };
                            let text = bounded_nfc(
                                prefix.chars().chain(self.nodes[id].surface.chars()),
                                bytes + current_bytes,
                                self.budget,
                            )?;
                            match &current {
                                Diversity::Unreachable => current = Diversity::One(text),
                                Diversity::One(old) if old != &text => {
                                    current = Diversity::Many;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                if let Diversity::One(text) = &current {
                    bytes = bytes.saturating_add(text.capacity());
                }
                if bytes > self.budget.max_bytes {
                    return Err(ConversionError::FullHypothesisBudgetExceeded);
                }
                diversity[id] = current;
            }
        }
        match &diversity[self.eos] {
            Diversity::Unreachable => Err(ConversionError::NoConstrainedPath),
            Diversity::Many => Ok(true),
            Diversity::One(surface) => {
                Ok(surface != &bounded_nfc(baseline.chars(), bytes, self.budget)?)
            }
        }
    }

    /// One exact K-best frontier per eligible run. The state is the terminal
    /// node. Transition costs now depend only on context IDs, so a coarser
    /// state (position and right ID) would also be exact; keeping the node
    /// keeps the tie rule below, which names predecessor nodes, and the
    /// budgets' step counts unchanged.
    ///
    /// Each node keeps at most `limit` prefixes, cheapest first, one per
    /// normalized surface. Keeping only the cheapest prefix of a surface is
    /// safe because every continuation from this node costs the same for all
    /// of them. Cutting at `limit` is safe because a prefix ranked below
    /// `limit` distinct cheaper ones can only produce final surfaces that are
    /// beaten by `limit` distinct cheaper finals (the same continuation keeps
    /// distinct prefixes distinct, as `has_alternative` also relies on).
    ///
    /// Predecessor lists are merged lazily through a heap ordered by (cost,
    /// predecessor node id, rank within that predecessor). That order is the
    /// tie rule, and it makes the result independent of hash or heap
    /// internals. Any budget error discards the whole run: a frontier cut
    /// short would rank surfaces against an incomplete set.
    ///
    /// Refused as `NoConstrainedPath`: a limit outside 1..=16, a snapshot
    /// other than the one the lattice was built with, or a baseline that does
    /// not cover the reading contiguously from 0 to its end.
    pub fn enumerate(
        &self,
        baseline: &[PathNode],
        snapshot: &CostSnapshot<'_>,
        limit: usize,
    ) -> Result<Vec<FullHypothesis>, ConversionError> {
        if !(1..=16).contains(&limit)
            || snapshot.revision() != self.revision
            || baseline.first().is_none_or(|n| n.start.get() != 0)
            || baseline.last().is_none_or(|n| n.end.get() != self.len)
            || baseline.windows(2).any(|p| p[0].end != p[1].start)
        {
            return Err(ConversionError::NoConstrainedPath);
        }
        let frontier = self.prefix_frontier(snapshot, limit)?;
        self.materialize(frontier, baseline, snapshot, limit)
    }

    /// Keep ranked prefixes separate from real paths. After taking EOS, all
    /// other DP prefixes are dead and must be released before deep-copying
    /// result nodes; retaining them would spend the byte budget twice (docs/src/internals/conversion.md「言語モデルによる選び直し」).
    fn prefix_frontier(
        &self,
        snapshot: &CostSnapshot<'_>,
        limit: usize,
    ) -> Result<Vec<Prefix>, ConversionError> {
        let mut bytes = self.bytes;
        charge_bytes(
            &mut bytes,
            self.nodes.len() * std::mem::size_of::<Vec<Prefix>>(),
            self.budget,
        )?;
        charge_bytes(&mut bytes, std::mem::size_of::<Prefix>(), self.budget)?;
        let mut dp = vec![Vec::<Prefix>::new(); self.nodes.len()];
        dp[self.bos] = vec![Prefix {
            surface: String::new(),
            cost: 0,
            ids: vec![],
        }];
        if self.budget.max_prefixes == 0 {
            return Err(ConversionError::FullHypothesisBudgetExceeded);
        }
        let mut steps = 0;
        let mut prefixes = 1usize;
        for pos in 0..=self.len {
            for &id in &self.starting[pos] {
                if id == self.bos {
                    continue;
                }
                let node = &self.nodes[id];
                let node_cost = i64::from(snapshot.node_cost(node)?);
                // There can be at most one heap entry per predecessor. Reserve
                // its entire slot array once, so growing the heap cannot hide
                // an additional allocation behind an uncounted capacity.
                let queue_bytes = self.ending[pos].len()
                    * std::mem::size_of::<Reverse<(i64, usize, usize, i64)>>();
                check_bytes(bytes, queue_bytes, self.budget)?;
                let mut queue = BinaryHeap::with_capacity(self.ending[pos].len());
                for &prev in &self.ending[pos] {
                    if prev == id || dp[prev].is_empty() {
                        continue;
                    }
                    step(&mut steps, self.budget.max_enumeration_steps)?;
                    let edge = i64::from(
                        snapshot.transition_cost(self.nodes[prev].right_id, node.left_id)?,
                    )
                    .checked_add(node_cost)
                    .ok_or(ConversionError::CostOverflow)?;
                    let cost = dp[prev][0]
                        .cost
                        .checked_add(edge)
                        .ok_or(ConversionError::CostOverflow)?;
                    queue.push(Reverse((cost, prev, 0usize, edge)));
                }
                if queue.is_empty() {
                    continue;
                }
                check_bytes(
                    bytes,
                    queue_bytes + limit * std::mem::size_of::<Prefix>(),
                    self.budget,
                )?;
                bytes += limit * std::mem::size_of::<Prefix>();
                let mut picked = Vec::with_capacity(limit);
                let mut seen = HashSet::new();
                let mut seen_bytes = 0usize;
                while let Some(Reverse((cost, prev, rank, edge))) = queue.pop() {
                    step(&mut steps, self.budget.max_enumeration_steps)?;
                    let old = &dp[prev][rank];
                    // Normalize a borrowed character chain, not a cloned raw
                    // concatenation. The temporary normalized buffer remains
                    // charged alongside both DP and duplicate-detection keys.
                    let surface = bounded_nfc(
                        old.surface.chars().chain(node.surface.chars()),
                        bytes + queue_bytes + seen_bytes,
                        self.budget,
                    )?;
                    if !seen.contains(&surface) {
                        let extra =
                            surface.capacity() + std::mem::size_of::<String>() + surface.len();
                        check_bytes(bytes, queue_bytes + seen_bytes + extra, self.budget)?;
                        let key = surface.clone();
                        seen_bytes += std::mem::size_of::<String>() + key.capacity();
                        check_bytes(
                            bytes,
                            queue_bytes + seen_bytes + surface.capacity(),
                            self.budget,
                        )?;
                        seen.insert(key);
                        let id_len = old.ids.len() + usize::from(id != self.eos);
                        check_bytes(
                            bytes,
                            queue_bytes
                                + seen_bytes
                                + surface.capacity()
                                + id_len * std::mem::size_of::<usize>(),
                            self.budget,
                        )?;
                        let mut ids = Vec::with_capacity(id_len);
                        ids.extend_from_slice(&old.ids);
                        if id != self.eos {
                            ids.push(id);
                        }
                        bytes += surface.capacity() + ids.capacity() * std::mem::size_of::<usize>();
                        check_bytes(bytes, queue_bytes + seen_bytes, self.budget)?;
                        prefixes += 1;
                        if prefixes > self.budget.max_prefixes {
                            return Err(ConversionError::FullHypothesisBudgetExceeded);
                        }
                        picked.push(Prefix { surface, cost, ids });
                        if picked.len() == limit {
                            break;
                        }
                    }
                    if rank + 1 < dp[prev].len() {
                        queue.push(Reverse((
                            dp[prev][rank + 1]
                                .cost
                                .checked_add(edge)
                                .ok_or(ConversionError::CostOverflow)?,
                            prev,
                            rank + 1,
                            edge,
                        )));
                    }
                }
                dp[id] = picked;
            }
        }
        let frontier = std::mem::take(&mut dp[self.eos]);
        drop(dp);
        if frontier.is_empty() {
            return Err(ConversionError::NoConstrainedPath);
        }
        Ok(frontier)
    }

    /// Copy real node authority only after DP release. Result buffers coexist
    /// with the remaining EOS frontier, and the actual baseline coexists with
    /// the representative it will replace: check both peaks before publishing
    /// any result. A failure returns no partial ranked list (docs/src/internals/conversion.md「言語モデルによる選び直し」).
    fn materialize(
        &self,
        frontier: Vec<Prefix>,
        baseline: &[PathNode],
        snapshot: &CostSnapshot<'_>,
        limit: usize,
    ) -> Result<Vec<FullHypothesis>, ConversionError> {
        let prefix_slots = frontier.capacity() * std::mem::size_of::<Prefix>();
        let mut bytes = self.bytes + prefix_slots;
        for p in &frontier {
            charge_bytes(
                &mut bytes,
                p.surface.capacity() + p.ids.capacity() * std::mem::size_of::<usize>(),
                self.budget,
            )?;
        }
        charge_bytes(
            &mut bytes,
            limit * std::mem::size_of::<FullHypothesis>(),
            self.budget,
        )?;
        let mut result = Vec::with_capacity(limit);
        for p in frontier {
            charge_bytes(
                &mut bytes,
                p.ids.len() * std::mem::size_of::<PathNode>(),
                self.budget,
            )?;
            let mut nodes = Vec::with_capacity(p.ids.len());
            for &id in &p.ids {
                let n = &self.nodes[id];
                check_bytes(bytes, node_clone_bytes(n), self.budget)?;
                let copy = n.to_path_node().ok_or(ConversionError::NoPath)?;
                charge_bytes(&mut bytes, path_dynamic_bytes(&copy), self.budget)?;
                nodes.push(copy);
            }
            bytes -= p.ids.capacity() * std::mem::size_of::<usize>();
            result.push(FullHypothesis {
                surface: p.surface,
                base_cost: p.cost,
                nodes,
                is_baseline: false,
                snapshot_revision: self.revision,
            });
        }
        // The consumed frontier's slot buffer has now been freed, while its
        // surface buffers moved into result and are still accounted above.
        bytes -= prefix_slots;
        let surface = bounded_nfc(
            baseline.iter().flat_map(|n| n.surface.chars()),
            bytes,
            self.budget,
        )?;
        charge_bytes(&mut bytes, surface.capacity(), self.budget)?;
        charge_bytes(&mut bytes, std::mem::size_of_val(baseline), self.budget)?;
        let mut nodes = Vec::with_capacity(baseline.len());
        for n in baseline {
            let requested = n.surface.len() + unknown_surface_len(&n.token_key);
            check_bytes(bytes, requested, self.budget)?;
            let copy = n.clone();
            charge_bytes(&mut bytes, path_dynamic_bytes(&copy), self.budget)?;
            nodes.push(copy);
        }
        let actual = FullHypothesis {
            surface,
            base_cost: path_cost(baseline, snapshot)?,
            nodes,
            is_baseline: true,
            snapshot_revision: self.revision,
        };
        // Rank still names the enumerated surface (docs/src/internals/conversion.md「言語モデルによる選び直し」). The actual baseline
        // path is privileged over a cheaper same-surface representative, and
        // inserted/replaced in place without sorting a second time. So the
        // list is in cost order except at the baseline: it can sit at its
        // surface's rank with a higher cost, or, when its surface was not
        // among the top K, in the last slot in place of the K-th.
        if let Some(rank) = result.iter().position(|h| h.surface == actual.surface) {
            result[rank] = actual;
        } else {
            result.truncate(limit - 1);
            result.push(actual);
        }
        Ok(result)
    }
}

// A DP entry: the NFC surface so far, its cost, and the lattice node ids on
// the path (BOS and EOS excluded).
#[derive(Clone)]
struct Prefix {
    surface: String,
    cost: i64,
    ids: Vec<usize>,
}

// Counts one unit of work and fails once `maximum` units have been used.
fn step(steps: &mut usize, maximum: usize) -> Result<(), ConversionError> {
    if *steps >= maximum {
        return Err(ConversionError::FullHypothesisBudgetExceeded);
    }
    *steps += 1;
    Ok(())
}

// `check_bytes` only asks whether `additional` would fit beside the live
// total; `charge_bytes` also adds it, for data that stays alive.
fn check_bytes(
    live: usize,
    additional: usize,
    budget: HypothesisBudget,
) -> Result<(), ConversionError> {
    if live.saturating_add(additional) > budget.max_bytes {
        return Err(ConversionError::FullHypothesisBudgetExceeded);
    }
    Ok(())
}
fn charge_bytes(
    live: &mut usize,
    additional: usize,
    budget: HypothesisBudget,
) -> Result<(), ConversionError> {
    check_bytes(*live, additional, budget)?;
    *live += additional;
    Ok(())
}

/// Bound each owned NFC output buffer while it grows, including its spare
/// capacity. Borrowed source characters need no raw concatenation buffer.
fn bounded_nfc(
    chars: impl Iterator<Item = char>,
    live: usize,
    budget: HypothesisBudget,
) -> Result<String, ConversionError> {
    let mut out = String::new();
    for ch in chars.nfc() {
        let needed = out.len() + ch.len_utf8();
        if needed > out.capacity() {
            check_bytes(live, needed, budget)?;
            let capacity = needed
                .max(out.capacity().saturating_mul(2))
                .min(budget.max_bytes - live);
            out.try_reserve_exact(capacity - out.len())
                .map_err(|_| ConversionError::FullHypothesisBudgetExceeded)?;
            check_bytes(live, out.capacity(), budget)?;
        }
        out.push(ch);
    }
    Ok(out)
}
fn unknown_surface_len(key: &crate::TokenKey) -> usize {
    match key {
        crate::TokenKey::Unknown { surface, .. } => surface.len(),
        _ => 0,
    }
}
fn path_dynamic_bytes(n: &PathNode) -> usize {
    n.surface.capacity()
        + match &n.token_key {
            crate::TokenKey::Unknown { surface, .. } => surface.capacity(),
            _ => 0,
        }
}
fn node_clone_bytes(n: &LatticeNode) -> usize {
    n.surface.len() + n.token_key.as_ref().map_or(0, unknown_surface_len)
}

/// Recompute real baseline authority, never sum span-local candidate costs:
/// their boundary transitions may overlap or use different neighbours.
fn path_cost(nodes: &[PathNode], snapshot: &CostSnapshot<'_>) -> Result<i64, ConversionError> {
    let mut cost = 0i64;
    let mut right = snapshot.dictionary().bos_right_id();
    for node in nodes {
        let node_cost = snapshot.path_node_cost_breakdown(node)?.total;
        cost = cost
            .checked_add(i64::from(snapshot.transition_cost(right, node.left_id)?))
            .and_then(|v| v.checked_add(i64::from(node_cost)))
            .ok_or(ConversionError::CostOverflow)?;
        right = node.right_id;
    }
    cost.checked_add(i64::from(
        snapshot.transition_cost(right, snapshot.dictionary().eos_left_id())?,
    ))
    .ok_or(ConversionError::CostOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ScalarIndex, TokenKey, viterbi_1best};
    use muu_dict::{
        Dictionary, DictionaryBuilder, LeftContextId, LexiconEntry, PosId, RightContextId,
        UnknownClass,
    };
    use tempfile::NamedTempFile;

    fn fixture(extra: &[(&str, &str, i16)]) -> (NamedTempFile, Dictionary) {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"full-hypothesis-test\"\n");
        builder.add_unknown(muu_dict::UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 1_000,
        });
        for &(reading, surface, cost) in extra {
            builder.add_entry(LexiconEntry {
                reading: reading.into(),
                surface: surface.into(),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: i32::from(cost),
            });
        }
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        (file, dictionary)
    }

    fn all_paths(
        graph: &ConstrainedHypotheses,
        current: usize,
        path: &mut Vec<PathNode>,
        out: &mut Vec<Vec<PathNode>>,
    ) {
        if current == graph.eos {
            out.push(path.clone());
            return;
        }
        let position = graph.nodes[current].end.get();
        for &next in &graph.starting[position] {
            if next == graph.bos || next == current {
                continue;
            }
            if next != graph.eos {
                path.push(graph.nodes[next].to_path_node().unwrap());
            }
            all_paths(graph, next, path, out);
            if next != graph.eos {
                path.pop();
            }
        }
    }

    // The DP against brute force: every surface's cost equals the cheapest path
    // with that surface, the first result costs what Viterbi found, results come
    // in cost order, and a second run returns the same surfaces and paths.
    // Negative word costs are included so a sign or pruning slip shows.
    #[test]
    fn whole_frontier_matches_exhaustive_surfaces_and_viterbi_with_negative_costs() {
        let (_file, dictionary) = fixture(&[
            ("あ", "亜", -10),
            ("あ", "亜", 3),
            ("あ", "阿", -3),
            ("い", "井", -5),
            ("い", "胃", 1),
            ("あい", "愛", -8),
        ]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あい", &snapshot, HypothesisBudget::default()).unwrap();
        let best = viterbi_1best(&lattice.lattice, &snapshot).unwrap();
        let graph = lattice.constrain(&[]).unwrap();
        let result = graph.enumerate(&best.nodes, &snapshot, 16).unwrap();
        let mut paths = Vec::new();
        all_paths(&graph, graph.bos, &mut Vec::new(), &mut paths);
        let mut exact = std::collections::BTreeMap::<String, i64>::new();
        for path in paths {
            let surface = path
                .iter()
                .map(|n| n.surface.as_str())
                .collect::<String>()
                .nfc()
                .collect();
            let cost = path_cost(&path, &snapshot).unwrap();
            exact
                .entry(surface)
                .and_modify(|old| *old = (*old).min(cost))
                .or_insert(cost);
        }
        assert_eq!(result[0].base_cost, i64::from(best.total_cost));
        assert_eq!(result.len(), exact.len().min(16));
        for hypothesis in &result {
            assert_eq!(exact[&hypothesis.surface], hypothesis.base_cost);
        }
        assert!(result.windows(2).all(|p| p[0].base_cost <= p[1].base_cost));
        let again = graph.enumerate(&best.nodes, &snapshot, 16).unwrap();
        assert_eq!(
            result
                .iter()
                .map(|h| (&h.surface, &h.nodes))
                .collect::<Vec<_>>(),
            again
                .iter()
                .map(|h| (&h.surface, &h.nodes))
                .collect::<Vec<_>>()
        );
        assert!(graph.has_alternative(&result[0].surface).unwrap());
    }

    // The baseline is a dearer path (cost 20) with the cheapest surface. It takes
    // that surface's rank 0 with its own path and cost, and the list is not
    // re-sorted, so rank 1 is cheaper than rank 0.
    #[test]
    fn same_surface_baseline_uses_actual_path_and_cost_in_the_original_rank() {
        let (_file, dictionary) =
            fixture(&[("あい", "愛", -8), ("あい", "愛", 20), ("あい", "藍", -3)]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あい", &snapshot, HypothesisBudget::default()).unwrap();
        let baseline = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "愛" && n.word_cost == 20)
            .unwrap()
            .to_path_node()
            .unwrap();
        let result = lattice
            .constrain(&[])
            .unwrap()
            .enumerate(std::slice::from_ref(&baseline), &snapshot, 16)
            .unwrap();
        assert!(result[0].is_baseline);
        assert_eq!(result[0].nodes, [baseline]);
        assert_eq!(result[0].base_cost, 20);
        assert!(
            result[1].base_cost < result[0].base_cost,
            "baseline replacement must not resort ranks"
        );
    }

    // A baseline whose surface ranks outside the top 16 takes the last slot; the
    // first 15 representatives are kept.
    #[test]
    fn actual_baseline_outside_sixteen_replaces_the_last_representative() {
        let entries = (0..20)
            .map(|i| ("あい".to_owned(), format!("S{i:02}"), i as i16))
            .collect::<Vec<_>>();
        let borrowed = entries
            .iter()
            .map(|(r, s, c)| (r.as_str(), s.as_str(), *c))
            .collect::<Vec<_>>();
        let (_file, dictionary) = fixture(&borrowed);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あい", &snapshot, HypothesisBudget::default()).unwrap();
        let baseline = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "S19")
            .unwrap()
            .to_path_node()
            .unwrap();
        let result = lattice
            .constrain(&[])
            .unwrap()
            .enumerate(std::slice::from_ref(&baseline), &snapshot, 16)
            .unwrap();
        assert_eq!(result.len(), 16);
        assert_eq!(result[15].nodes, [baseline]);
        assert!(result[15].is_baseline);
        assert_eq!(result[14].surface, "S14");
    }

    // An anchor that is not in the lattice (a local literal node) is added as is,
    // every result passes through it, and no node crosses its span (愛 over
    // あい is excluded).
    #[test]
    fn frozen_spans_exclude_crossing_paths_and_preserve_missing_local_anchors() {
        let (_file, dictionary) = fixture(&[
            ("あ", "亜", -10),
            ("い", "井", -5),
            ("い", "胃", 1),
            ("あい", "愛", -30),
        ]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あい", &snapshot, HypothesisBudget::default()).unwrap();
        let mut first = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "亜")
            .unwrap()
            .to_path_node()
            .unwrap();
        let last = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "井")
            .unwrap()
            .to_path_node()
            .unwrap();
        // A real local node need not occur in the full-reading lattice.
        first.surface = "literal".into();
        first.kind = NodeKind::Unknown(UnknownClass::AsciiAlphabetic);
        first.token_key = TokenKey::Unknown {
            class: UnknownClass::AsciiAlphabetic,
            surface: first.surface.clone(),
        };
        let graph = lattice.constrain(&[first.clone()]).unwrap();
        let result = graph
            .enumerate(&[first.clone(), last], &snapshot, 16)
            .unwrap();
        assert!(result.len() > 1);
        assert!(result.iter().all(|h| h.nodes[0] == first));
        assert!(result.iter().all(|h| {
            h.nodes
                .iter()
                .all(|n| n.start.get() != 0 || n.end == ScalarIndex::new(1))
        }));
    }

    // か followed by a combining dakuten and the single node が are the same NFC
    // surface. Enumeration returns it once, and reachability sees no
    // alternative to the decomposed baseline.
    #[test]
    fn canonical_surfaces_compose_across_nodes_in_enumeration_and_reachability() {
        let (_file, dictionary) =
            fixture(&[("あ", "か", -1), ("い", "\u{3099}", -1), ("あい", "が", 0)]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あい", &snapshot, HypothesisBudget::default()).unwrap();
        let mut graph = lattice.constrain(&[]).unwrap();
        for list in graph.starting.iter_mut().chain(graph.ending.iter_mut()) {
            list.retain(|&id| {
                matches!(
                    graph.nodes[id].kind,
                    NodeKind::Bos | NodeKind::Eos | NodeKind::Known
                )
            });
        }
        let mut paths = Vec::new();
        all_paths(&graph, graph.bos, &mut Vec::new(), &mut paths);
        let result = graph.enumerate(&paths[0], &snapshot, 16).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].surface, "が");
        assert!(!graph.has_alternative("か\u{3099}").unwrap());
    }

    // The byte budget also covers copying result paths after the DP. The search
    // finds a limit where the frontier fits but materialization does not, and
    // that must fail as a budget error, not return a partial list.
    #[test]
    fn result_copies_can_exhaust_bytes_after_the_prefix_frontier_succeeds() {
        let surface = "x".repeat(8192);
        let (_file, dictionary) = fixture(&[("あ", &surface, -1)]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あ", &snapshot, HypothesisBudget::default()).unwrap();
        let best = viterbi_1best(&lattice.lattice, &snapshot).unwrap();
        let mut graph = lattice.constrain(&[]).unwrap();
        // Find a logical byte limit, not a timing/machine-dependent threshold.
        // This explicitly isolates result materialization from prefix search.
        let mut limit = None;
        for max_bytes in (35_000..100_000).step_by(128) {
            graph.budget.max_bytes = max_bytes;
            if graph.prefix_frontier(&snapshot, 16).is_ok()
                && matches!(
                    graph.enumerate(&best.nodes, &snapshot, 16),
                    Err(ConversionError::FullHypothesisBudgetExceeded)
                )
            {
                limit = Some(max_bytes);
                break;
            }
        }
        let limit = limit.expect("real nodes/baseline must have a separately checked peak");
        eprintln!("prefix fits but result fails at {limit} tracked bytes");
        graph.budget.max_bytes = limit;
        assert!(graph.prefix_frontier(&snapshot, 16).is_ok());
        assert!(matches!(
            graph.enumerate(&best.nodes, &snapshot, 16),
            Err(ConversionError::FullHypothesisBudgetExceeded)
        ));
        graph.budget = HypothesisBudget::default();
        assert_eq!(
            graph.enumerate(&best.nodes, &snapshot, 16).unwrap().len(),
            2
        );
    }

    // For every K from 1 to 16 over a language of more than 16 surfaces, the
    // result is exactly the K cheapest surfaces with their exact costs,
    // including surfaces that only exist after NFC composition across nodes.
    #[test]
    fn unicode_top_k_smaller_than_the_language_matches_exhaustive_costs() {
        let (_file, dictionary) = fixture(&[
            ("あ", "e", 0),
            ("あ", "é", 13),
            ("あ", "か", 41),
            ("い", "\u{301}", 0),
            ("い", "\u{3099}", 5),
            ("い", "a", 17),
            ("う", "u", 1),
            ("う", "U", 97),
            ("あい", "é", -3),
        ]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あいう", &snapshot, HypothesisBudget::default()).unwrap();
        let best = viterbi_1best(&lattice.lattice, &snapshot).unwrap();
        let graph = lattice.constrain(&[]).unwrap();
        let mut paths = Vec::new();
        all_paths(&graph, graph.bos, &mut Vec::new(), &mut paths);
        let mut exact = std::collections::BTreeMap::<String, i64>::new();
        for path in paths {
            let surface: String = path.iter().flat_map(|n| n.surface.chars()).nfc().collect();
            let cost = path_cost(&path, &snapshot).unwrap();
            exact
                .entry(surface)
                .and_modify(|old| *old = (*old).min(cost))
                .or_insert(cost);
        }
        assert!(exact.len() > 16);
        assert!(exact.contains_key("éu") && exact.contains_key("がu"));
        let mut ranked = exact
            .into_iter()
            .map(|(surface, cost)| (cost, surface))
            .collect::<Vec<_>>();
        ranked.sort();
        for k in 1..=16 {
            let result = graph.enumerate(&best.nodes, &snapshot, k).unwrap();
            let mut actual = result
                .iter()
                .map(|h| (h.base_cost, h.surface.clone()))
                .collect::<Vec<_>>();
            actual.sort();
            assert_eq!(actual, ranked[..k], "K={k}");
        }
    }

    // Why the DP state is the node: B is dearer than A at the same position but
    // has a right context that makes X much cheaper. A state of position only
    // would drop B and miss BX, the true best.
    #[test]
    fn terminal_tokens_and_right_contexts_keep_distinct_continuation_costs() {
        let mut builder =
            DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"terminal-state-test\"\n");
        builder.add_unknown(muu_dict::UnknownDefinition {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 1000,
        });
        for (reading, surface, cost, left, right) in [
            ("あ", "A", 0, 0, 0),
            ("あ", "B", 5, 0, 1),
            ("あ", "C", 1, 0, 0),
            ("い", "X", 0, 1, 0),
        ] {
            builder.add_entry(LexiconEntry {
                reading: reading.into(),
                surface: surface.into(),
                left_id: LeftContextId::new(left),
                right_id: RightContextId::new(right),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: cost,
            });
        }
        builder
            .set_connection_cost(RightContextId::new(0), LeftContextId::new(1), 100)
            .unwrap();
        builder
            .set_connection_cost(RightContextId::new(1), LeftContextId::new(1), -100)
            .unwrap();
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あい", &snapshot, HypothesisBudget::default()).unwrap();
        let a = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "A")
            .unwrap();
        let b = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "B")
            .unwrap();
        let x = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "X")
            .unwrap();
        assert_eq!(a.end, b.end);
        assert!(snapshot.node_cost(a).unwrap() < snapshot.node_cost(b).unwrap());
        assert_eq!(
            snapshot.transition_cost(a.right_id, x.left_id).unwrap(),
            100
        );
        assert_eq!(
            snapshot.transition_cost(b.right_id, x.left_id).unwrap(),
            -100
        );
        let best = viterbi_1best(&lattice.lattice, &snapshot).unwrap();
        let graph = lattice.constrain(&[]).unwrap();
        let result = graph.enumerate(&best.nodes, &snapshot, 1).unwrap();
        assert_eq!((&*result[0].surface, result[0].base_cost), ("BX", -95));
        // One prefix per position would have discarded B before seeing X.
        assert_eq!(result[0].nodes[0].right_id, RightContextId::new(1));
        assert_ne!(a.token_key, b.token_key);
        let c = lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == "C")
            .unwrap();
        assert_eq!(a.right_id, c.right_id);
        assert!(snapshot.node_cost(a).unwrap() < snapshot.node_cost(c).unwrap());
    }

    // With all costs equal, order follows the tie rule. At EOS the predecessors
    // are the two last nodes, taken in node id order, and within each its
    // prefixes in rank order, so the surfaces come grouped by their last node.
    #[test]
    fn equal_cost_order_is_predecessor_node_then_its_prefix_rank() {
        let (_file, dictionary) = fixture(&[
            ("あ", "A", 0),
            ("あ", "B", 0),
            ("い", "X", 0),
            ("い", "Y", 0),
        ]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あい", &snapshot, HypothesisBudget::default()).unwrap();
        let graph = lattice.constrain(&[]).unwrap();
        let by_id = |pos: usize| -> Vec<String> {
            graph
                .nodes
                .iter()
                .filter(|n| {
                    n.kind == NodeKind::Known && n.start.get() == pos && n.end.get() == pos + 1
                })
                .map(|n| n.surface.clone())
                .collect()
        };
        let prefixes = by_id(0);
        let terminals = by_id(1);
        assert_eq!(prefixes.len(), 2);
        assert_eq!(terminals.len(), 2);
        let expected = terminals
            .iter()
            .flat_map(|t| prefixes.iter().map(move |p| p.clone() + t))
            .collect::<Vec<_>>();
        let baseline = viterbi_1best(&lattice.lattice, &snapshot).unwrap();
        let result = graph.enumerate(&baseline.nodes, &snapshot, 4).unwrap();
        assert_eq!(
            result.iter().map(|h| h.surface.clone()).collect::<Vec<_>>(),
            expected
        );
        assert!(result.iter().all(|h| h.base_cost == 0));
    }

    // Each work budget limits only its own phase: zero reachability edges fail
    // has_alternative but not enumerate, and the reverse.
    #[test]
    fn reachability_and_enumeration_have_independent_work_allowances() {
        let (_file, dictionary) = fixture(&[("あ", "亜", 0), ("あ", "阿", 1)]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        let lattice =
            FullHypothesisLattice::build("あ", &snapshot, HypothesisBudget::default()).unwrap();
        let baseline = viterbi_1best(&lattice.lattice, &snapshot).unwrap();
        let mut graph = lattice.constrain(&[]).unwrap();
        graph.budget.max_reachability_edges = 0;
        assert!(matches!(
            graph.has_alternative("亜"),
            Err(ConversionError::FullHypothesisBudgetExceeded)
        ));
        assert!(graph.enumerate(&baseline.nodes, &snapshot, 16).is_ok());
        graph.budget.max_reachability_edges = 1_000_000;
        graph.budget.max_enumeration_steps = 0;
        assert!(graph.has_alternative("亜").unwrap());
        assert!(matches!(
            graph.enumerate(&baseline.nodes, &snapshot, 16),
            Err(ConversionError::FullHypothesisBudgetExceeded)
        ));
    }

    // Each budget (nodes, enumeration steps, prefixes, bytes) fails as
    // FullHypothesisBudgetExceeded instead of returning fewer results.
    #[test]
    fn selector_budgets_return_errors_without_truncating_or_affecting_generation() {
        let (_file, dictionary) = fixture(&[("あい", "愛", 0)]);
        let snapshot = CostSnapshot::baseline(&dictionary);
        assert!(matches!(
            FullHypothesisLattice::build(
                "あい",
                &snapshot,
                HypothesisBudget {
                    max_nodes: 1,
                    ..Default::default()
                }
            ),
            Err(ConversionError::FullHypothesisBudgetExceeded)
        ));
        let ordinary = Lattice::build("あい", &dictionary).unwrap();
        let baseline = viterbi_1best(&ordinary, &snapshot).unwrap();
        for budget in [
            HypothesisBudget {
                max_enumeration_steps: 0,
                ..Default::default()
            },
            HypothesisBudget {
                max_prefixes: 1,
                ..Default::default()
            },
        ] {
            let lattice = FullHypothesisLattice::build("あい", &snapshot, budget).unwrap();
            assert!(matches!(
                lattice
                    .constrain(&[])
                    .unwrap()
                    .enumerate(&baseline.nodes, &snapshot, 16),
                Err(ConversionError::FullHypothesisBudgetExceeded)
            ));
        }
        assert!(matches!(
            FullHypothesisLattice::build(
                "あい",
                &snapshot,
                HypothesisBudget {
                    max_bytes: 1,
                    ..Default::default()
                }
            ),
            Err(ConversionError::FullHypothesisBudgetExceeded)
        ));
    }
}

#[cfg(test)]
#[path = "full_hypothesis/contract_tests.rs"]
mod contract_tests;
