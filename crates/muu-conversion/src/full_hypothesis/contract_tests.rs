//! Current full-path contracts retained from the removed fixed enumerator.
use super::*;
use crate::{ScalarIndex, SnapshotRevision, TokenKey};
use muu_dict::{
    Dictionary, DictionaryBuilder, DictionaryTokenId, LeftContextId, LexiconEntry, PosId,
    RightContextId,
};
use std::fs;
use tempfile::NamedTempFile;
struct Fixture {
    _file: NamedTempFile,
    dictionary: Dictionary,
}

fn fixture(configure: impl FnOnce(&mut DictionaryBuilder)) -> Fixture {
    let mut builder =
        DictionaryBuilder::new(4, 4, RightContextId::new(3), LeftContextId::new(3)).unwrap();
    builder.add_unknown(muu_dict::UnknownDefinition {
        class: muu_dict::UnknownClass::AsciiAlphabetic,
        group: false,
        max_length: 1,
        left_id: LeftContextId::new(0),
        right_id: RightContextId::new(0),
        left_pos_id: PosId::new(0),
        right_pos_id: PosId::new(0),
        word_cost: 1_000,
    });
    builder.provenance(b"name = \"path-cost-contract\"\n");
    for index in 0..8 {
        builder.add_entry(LexiconEntry {
            reading: format!("r{index:02}"),
            surface: format!("S{index:02}"),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 0,
        });
    }
    configure(&mut builder);
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), builder.build().unwrap()).unwrap();
    let dictionary = Dictionary::open(file.path()).unwrap();
    Fixture {
        _file: file,
        dictionary,
    }
}

fn node(
    start: usize,
    end: usize,
    surface: &str,
    token: u64,
    word_cost: i32,
    left: u16,
    right: u16,
) -> PathNode {
    PathNode {
        start: ScalarIndex::new(start),
        end: ScalarIndex::new(end),
        surface: surface.into(),
        left_id: LeftContextId::new(left),
        right_id: RightContextId::new(right),
        left_pos_id: PosId::new(0),
        right_pos_id: PosId::new(0),
        lexical_class: muu_dict::LexicalClass::General,
        token_key: TokenKey::Dictionary(DictionaryTokenId::new(token)),
        word_cost,
        kind: NodeKind::Known,
    }
}

// path_cost adds every term once: BOS transition, word costs, the learning
// adjustment, the transition between nodes, and the EOS transition.
#[test]
fn exact_cost_recomputes_bos_nodes_adjustments_transitions_and_eos() {
    use crate::{CostAdjustments, CostProvider};

    struct Adjusted(CostAdjustments);
    impl CostProvider for Adjusted {
        fn capture(&self) -> CostAdjustments {
            self.0.clone()
        }
    }

    let fixture = fixture(|builder| {
        builder
            .set_connection_cost(RightContextId::new(3), LeftContextId::new(1), 5)
            .unwrap();
        builder
            .set_connection_cost(RightContextId::new(1), LeftContextId::new(2), 7)
            .unwrap();
        builder
            .set_connection_cost(RightContextId::new(2), LeftContextId::new(3), 11)
            .unwrap();
    });
    let nodes = [node(0, 1, "甲", 0, 13, 1, 1), node(1, 2, "乙", 1, 17, 2, 2)];
    let mut adjustments = CostAdjustments::new(SnapshotRevision::new(9));
    adjustments.add_node_cost(TokenKey::Dictionary(DictionaryTokenId::new(1)), 19);
    let snapshot = CostSnapshot::capture(&fixture.dictionary, &Adjusted(adjustments));
    assert_eq!(
        path_cost(&nodes, &snapshot).unwrap(),
        5 + 13 + 7 + 17 + 19 + 11
    );
}

fn graph_fixture(entries: &[(&str, &str, i16)]) -> (NamedTempFile, Dictionary) {
    let mut builder =
        DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.add_unknown(muu_dict::UnknownDefinition {
        class: muu_dict::UnknownClass::AsciiAlphabetic,
        group: false,
        max_length: 1,
        left_id: LeftContextId::new(0),
        right_id: RightContextId::new(0),
        left_pos_id: PosId::new(0),
        right_pos_id: PosId::new(0),
        word_cost: 1_000,
    });
    builder.provenance(b"name = \"path-contracts\"\n");
    for &(reading, surface, cost) in entries {
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
    fs::write(file.path(), builder.build().unwrap()).unwrap();
    let dictionary = Dictionary::open(file.path()).unwrap();
    (file, dictionary)
}

// At every limit from 1 to 16 the result holds exactly one baseline, with
// its own path and cost (500), even when a cheaper surface fills the list.
#[test]
fn baseline_is_retained_at_every_current_limit_with_actual_authority() {
    let (_file, dictionary) = graph_fixture(&[("a", "安", 0), ("a", "基", 500)]);
    let snapshot = CostSnapshot::baseline(&dictionary);
    let lattice =
        FullHypothesisLattice::build("a", &snapshot, HypothesisBudget::default()).unwrap();
    let baseline = lattice
        .lattice
        .nodes()
        .iter()
        .find(|n| n.surface == "基")
        .unwrap()
        .to_path_node()
        .unwrap();
    let graph = lattice.constrain(&[]).unwrap();
    // The current selector accepts 1..=16; the removed beam's 256 limit is
    // not its contract. Actual baseline authority survives each valid limit.
    for limit in 1..=16 {
        let result = graph
            .enumerate(std::slice::from_ref(&baseline), &snapshot, limit)
            .unwrap();
        // Keep unique, actual baseline authority at every retained limit.
        // The nonzero BOS/intersegment/EOS test below checks repeated full
        // results, including nodes and snapshot identity.
        assert_eq!(result.iter().filter(|h| h.is_baseline()).count(), 1);
        let retained = result.iter().find(|h| h.is_baseline()).unwrap();
        assert_eq!(
            retained.base_cost(),
            path_cost(std::slice::from_ref(&baseline), &snapshot).unwrap()
        );
        assert_eq!(retained.nodes()[0].start.get(), 0);
        assert_eq!(retained.nodes()[0].end.get(), 1);
        assert_eq!(retained.surface(), "基");
        assert_eq!(retained.nodes(), std::slice::from_ref(&baseline));
        assert_eq!(retained.base_cost(), 500);
        assert!(result.len() <= limit);
    }
}

// A surface reachable by two paths appears once, represented by its cheaper
// real path.
#[test]
fn duplicate_non_baseline_surfaces_keep_the_minimum_cost_real_path() {
    let (_file, dictionary) = graph_fixture(&[("a", "同", 40), ("a", "同", 10), ("a", "別", 20)]);
    let snapshot = CostSnapshot::baseline(&dictionary);
    let lattice =
        FullHypothesisLattice::build("a", &snapshot, HypothesisBudget::default()).unwrap();
    let baseline = lattice
        .lattice
        .nodes()
        .iter()
        .find(|n| n.surface == "別")
        .unwrap()
        .to_path_node()
        .unwrap();
    let result = lattice
        .constrain(&[])
        .unwrap()
        .enumerate(&[baseline], &snapshot, 8)
        .unwrap();
    assert_eq!(result.iter().filter(|h| h.surface() == "同").count(), 1);
    let duplicate = result.iter().find(|h| h.surface() == "同").unwrap();
    assert_eq!(duplicate.base_cost(), 10);
    assert_eq!(duplicate.nodes()[0].word_cost, 10);
    assert!(!duplicate.is_baseline());
}

// 甲｜乙丙 and 甲乙｜丙 give the same surface. With limit 1 the baseline
// (cost 40) keeps its own segmentation and cost; the cheaper path (cost 0)
// cannot stand in for it.
#[test]
fn duplicate_whole_surface_preserves_the_actual_baseline_path() {
    let (_file, dictionary) = graph_fixture(&[
        ("a", "甲", 20),
        ("a", "甲乙", 0),
        ("b", "乙丙", 20),
        ("b", "丙", 0),
    ]);
    let snapshot = CostSnapshot::baseline(&dictionary);
    let lattice =
        FullHypothesisLattice::build("ab", &snapshot, HypothesisBudget::default()).unwrap();
    let baseline = ["甲", "乙丙"].map(|surface| {
        lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == surface)
            .unwrap()
            .to_path_node()
            .unwrap()
    });
    let result = lattice
        .constrain(&[])
        .unwrap()
        .enumerate(&baseline, &snapshot, 1)
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].surface(), "甲乙丙");
    assert_eq!(result[0].nodes(), &baseline);
    // Unlike the old visible representative, 4c records the actual baseline
    // cost as well as its path. A cheaper same-surface path cannot replace it.
    assert_eq!(result[0].base_cost(), 40);
    assert!(result[0].is_baseline());
}

// Two surfaces with equal cost (53, every transition included) both appear,
// and repeated runs return identical results, nodes and snapshot revision
// included.
#[test]
fn exact_cost_includes_bos_intersegment_and_eos_with_stable_ties() {
    let fixture = fixture(|builder| {
        for (right, left, cost) in [(3, 1, 5), (1, 2, 7), (2, 3, 11)] {
            builder
                .set_connection_cost(RightContextId::new(right), LeftContextId::new(left), cost)
                .unwrap();
        }
        for (reading, surface, cost, id) in
            [("a", "甲", 13, 1), ("a", "乙", 13, 1), ("b", "終", 17, 2)]
        {
            builder.add_entry(LexiconEntry {
                reading: reading.into(),
                surface: surface.into(),
                left_id: LeftContextId::new(id),
                right_id: RightContextId::new(id),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: cost,
            });
        }
    });
    let snapshot = CostSnapshot::baseline(&fixture.dictionary);
    let lattice =
        FullHypothesisLattice::build("ab", &snapshot, HypothesisBudget::default()).unwrap();
    let baseline = ["乙", "終"].map(|surface| {
        lattice
            .lattice
            .nodes()
            .iter()
            .find(|n| n.surface == surface)
            .unwrap()
            .to_path_node()
            .unwrap()
    });
    let graph = lattice.constrain(&[]).unwrap();
    let first = graph.enumerate(&baseline, &snapshot, 2).unwrap();
    let second = graph.enumerate(&baseline, &snapshot, 2).unwrap();
    assert_eq!(
        first
            .iter()
            .map(|h| (
                &h.surface,
                h.base_cost,
                &h.nodes,
                h.is_baseline,
                h.snapshot_revision
            ))
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|h| (
                &h.surface,
                h.base_cost,
                &h.nodes,
                h.is_baseline,
                h.snapshot_revision
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(first.len(), 2);
    assert!(first.iter().all(|h| h.base_cost() == 53));
    assert_eq!(
        first
            .iter()
            .map(|h| h.surface())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["甲終", "乙終"])
    );
}
