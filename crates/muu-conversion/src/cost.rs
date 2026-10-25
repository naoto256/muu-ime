use std::collections::BTreeMap;

use muu_dict::{Dictionary, LeftContextId, LexicalClass, RightContextId};

use crate::{ConversionError, LatticeNode, LexiconOverlay, PathNode, TokenKey};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeCostBreakdown {
    pub base: i32,
    pub lexical_class_adjustment: i32,
    pub adjustment: i32,
    pub total: i32,
}

/// A deliberately modest prior for ambiguous entries in the built-in lexicon.
/// Overlay tiers are explicit user authority and never receive this adjustment.
fn internal_lexical_class_adjustment(class: LexicalClass) -> i32 {
    match class {
        LexicalClass::General | LexicalClass::Symbol => 0,
        LexicalClass::ProperOther => 200,
        LexicalClass::Organization => 250,
        LexicalClass::Person | LexicalClass::Place => 450,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionCostBreakdown {
    pub base: i32,
    pub total: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotRevision(u64);

impl SnapshotRevision {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CostAdjustments {
    revision: SnapshotRevision,
    node: BTreeMap<TokenKey, i32>,
}

impl CostAdjustments {
    pub fn new(revision: SnapshotRevision) -> Self {
        Self {
            revision,
            ..Self::default()
        }
    }

    // Replaces any earlier delta for the token rather than adding to it; the
    // provider passes each token's final correction once. Any token kind is
    // accepted, but learning only supplies dictionary tokens (docs/src/internals/conversion.md「学習」).
    pub fn add_node_cost(&mut self, token: TokenKey, delta: i32) {
        self.node.insert(token, delta);
    }

    pub fn is_empty(&self) -> bool {
        self.node.is_empty()
    }
}

pub trait CostProvider {
    /// Captures all additions into an owned value. Search never calls the
    /// provider again, so one conversion cannot observe a changing model.
    fn capture(&self) -> CostAdjustments;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopCostProvider;

impl CostProvider for NoopCostProvider {
    fn capture(&self) -> CostAdjustments {
        CostAdjustments::default()
    }
}

pub struct CostSnapshot<'dictionary> {
    dictionary: &'dictionary Dictionary,
    // Positionally User, Imported, Postal, as `capture_with_overlay_catalogs`
    // passes them; `capture` leaves all three empty. The lattice treats all
    // three alike; tiers come from the entries themselves.
    overlays: [Option<&'dictionary LexiconOverlay>; 3],
    adjustments: CostAdjustments,
}

impl<'dictionary> CostSnapshot<'dictionary> {
    pub fn capture(dictionary: &'dictionary Dictionary, provider: &dyn CostProvider) -> Self {
        Self {
            dictionary,
            overlays: [None, None, None],
            adjustments: provider.capture(),
        }
    }

    pub fn baseline(dictionary: &'dictionary Dictionary) -> Self {
        Self::capture(dictionary, &NoopCostProvider)
    }

    pub fn capture_with_overlay_catalogs(
        dictionary: &'dictionary Dictionary,
        user: Option<&'dictionary LexiconOverlay>,
        imported: Option<&'dictionary LexiconOverlay>,
        postal: Option<&'dictionary LexiconOverlay>,
        provider: &dyn CostProvider,
    ) -> Self {
        Self {
            dictionary,
            overlays: [user, imported, postal],
            adjustments: provider.capture(),
        }
    }

    pub fn revision(&self) -> SnapshotRevision {
        self.adjustments.revision
    }

    pub fn dictionary(&self) -> &'dictionary Dictionary {
        self.dictionary
    }

    pub fn overlays(&self) -> [Option<&'dictionary LexiconOverlay>; 3] {
        self.overlays
    }

    pub(crate) fn node_cost(&self, node: &LatticeNode) -> Result<i32, ConversionError> {
        Ok(self
            .node_cost_breakdown(node.word_cost, node.lexical_class, node.token_key.as_ref())?
            .total)
    }

    pub(crate) fn path_node_cost_breakdown(
        &self,
        node: &PathNode,
    ) -> Result<NodeCostBreakdown, ConversionError> {
        self.node_cost_breakdown(node.word_cost, node.lexical_class, Some(&node.token_key))
    }

    fn node_cost_breakdown(
        &self,
        base: i32,
        lexical_class: LexicalClass,
        token: Option<&TokenKey>,
    ) -> Result<NodeCostBreakdown, ConversionError> {
        let lexical_class_adjustment = match token {
            Some(TokenKey::Dictionary(_)) => internal_lexical_class_adjustment(lexical_class),
            Some(TokenKey::Unknown { .. } | TokenKey::Overlay { .. }) | None => 0,
        };
        let adjustment = token
            .and_then(|key| self.adjustments.node.get(key))
            .copied()
            .unwrap_or(0);
        let total = base
            .checked_add(lexical_class_adjustment)
            .and_then(|cost| cost.checked_add(adjustment))
            .ok_or(ConversionError::CostOverflow)?;
        Ok(NodeCostBreakdown {
            base,
            lexical_class_adjustment,
            adjustment,
            total,
        })
    }

    pub fn transition_cost(
        &self,
        previous_right: RightContextId,
        next_left: LeftContextId,
    ) -> Result<i32, ConversionError> {
        Ok(self
            .transition_cost_breakdown(previous_right, next_left)?
            .total)
    }

    pub fn transition_cost_breakdown(
        &self,
        previous_right: RightContextId,
        next_left: LeftContextId,
    ) -> Result<TransitionCostBreakdown, ConversionError> {
        let base = self.dictionary.connection_cost(previous_right, next_left)?;
        // Learning adjusts node costs only. Connection costs are the immutable
        // dictionary matrix; retaining empty correction maps would imply a
        // production source for a retired experiment.
        Ok(TransitionCostBreakdown { base, total: base })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_provider_captures_a_stable_empty_snapshot() {
        assert_eq!(
            NoopCostProvider.capture(),
            CostAdjustments::new(SnapshotRevision::new(0))
        );
    }

    #[test]
    fn built_in_lexical_class_prior_is_explicit_and_modest() {
        assert_eq!(internal_lexical_class_adjustment(LexicalClass::General), 0);
        assert_eq!(internal_lexical_class_adjustment(LexicalClass::Symbol), 0);
        assert_eq!(
            internal_lexical_class_adjustment(LexicalClass::ProperOther),
            200
        );
        assert_eq!(
            internal_lexical_class_adjustment(LexicalClass::Organization),
            250
        );
        assert_eq!(internal_lexical_class_adjustment(LexicalClass::Person), 450);
        assert_eq!(internal_lexical_class_adjustment(LexicalClass::Place), 450);
    }
}
