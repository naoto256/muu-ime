//! Candidate catalog/identity contracts retained from the retired applicator.
use super::*;
use crate::{NodeKind, PathNode, ScalarIndex, SnapshotRevision, TokenKey};
use muu_dict::{DictionaryTokenId, LeftContextId, PosId, RightContextId};
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

// The path cost is a sentinel. These tests are about identity and selection,
// which must not depend on cost.
fn candidate(range: ScalarRange, paths: Vec<Vec<PathNode>>, revision: u64) -> Candidate {
    Candidate::from_group(
        range,
        paths
            .into_iter()
            .map(|nodes| PathCandidate {
                nodes,
                cost: -99_999,
                snapshot_revision: SnapshotRevision::new(revision),
            })
            .collect(),
    )
    .unwrap()
}

fn segments(reading: &str, choices: Vec<(Vec<Candidate>, usize)>, focus: usize) -> Segments {
    let segments = choices
        .into_iter()
        .map(|(candidates, focused)| {
            Segment::new(
                candidates[0].reading_range(),
                candidates,
                focused,
                SegmentState::Unconfirmed,
            )
            .unwrap()
        })
        .collect();
    Segments::new(reading.into(), segments, focus).unwrap()
}

// Choosing a candidate by hand moves only focus indices. Every segment keeps
// its candidate list and range, and the source value is untouched.
#[test]
fn manual_selection_preserves_candidates_paths_and_ranges() {
    let first_range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(1));
    let second_range = ScalarRange::new(ScalarIndex::new(1), ScalarIndex::new(2));
    let baseline = segments(
        "ab",
        vec![
            (
                vec![candidate(
                    first_range,
                    vec![vec![node(0, 1, "記者", 0, 0, 0, 0)]],
                    0,
                )],
                0,
            ),
            (
                vec![
                    candidate(second_range, vec![vec![node(1, 2, "基礎", 1, 0, 0, 0)]], 0),
                    candidate(second_range, vec![vec![node(1, 2, "汽車", 2, 0, 0, 0)]], 0),
                ],
                0,
            ),
        ],
        0,
    );
    let original = baseline.clone();
    let mut applied = baseline.clone();
    applied.move_focus(1).unwrap();
    applied.select_focused_candidate(1).unwrap();
    applied.move_focus(0).unwrap();
    assert_eq!(applied.segments()[1].focused_candidate(), 1);
    assert_eq!(applied.segments()[0].focused_candidate(), 0);
    assert_eq!(applied.focused_segment(), baseline.focused_segment());
    // Manual choice changes focus only; rebuilding candidates here would
    // sever the candidate IDs and supporting paths used at commit (docs/src/internals/conversion.md「文節ごとの候補」).
    for (before, after) in baseline.segments().iter().zip(applied.segments()) {
        assert_eq!(before.candidates(), after.candidates());
        assert_eq!(before.reading_range(), after.reading_range());
    }
    assert_eq!(baseline, original);
}

// Two candidates with the same surface but different tokens keep different
// ids and their own paths, so selecting by id picks the right path. A bad
// index or an unknown id is an error and leaves the segments unchanged.
#[test]
fn same_nfc_surface_preserves_distinct_candidate_path_authority() {
    let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(1));
    let a = candidate(range, vec![vec![node(0, 1, "同", 0, 40, 0, 0)]], 0);
    let b = candidate(range, vec![vec![node(0, 1, "同", 1, 10, 0, 0)]], 0);
    assert_eq!(a.surface(), b.surface());
    assert_ne!(a.id(), b.id());
    let baseline = segments("a", vec![(vec![a.clone(), b.clone()], 0)], 0);
    let mut selected = baseline.clone();
    selected.select_focused_candidate_by_id(b.id()).unwrap();
    assert_eq!(selected.focused().unwrap().focused().paths(), b.paths());
    assert_eq!(baseline.focused().unwrap().focused().paths(), a.paths());
    let original = selected.clone();
    assert!(selected.select_focused_candidate(usize::MAX).is_err());
    assert_eq!(selected, original);
    assert!(
        selected
            .select_focused_candidate_by_id(&CandidateId::new("wrong".into()).unwrap())
            .is_err()
    );
    assert_eq!(selected, original);
}

// A range crosses a boundary only strictly inside it: its endpoints and
// points outside do not count, and an empty range crosses nothing.
#[test]
fn scalar_boundary_crossing_is_strict_and_does_not_include_endpoints() {
    let range = ScalarRange::new(ScalarIndex::new(2), ScalarIndex::new(4));
    for boundary in [0, 1, 2, 4, 5] {
        assert!(!range.crosses_boundary(ScalarIndex::new(boundary)));
    }
    assert!(range.crosses_boundary(ScalarIndex::new(3)));
    let empty = ScalarRange::new(ScalarIndex::new(3), ScalarIndex::new(3));
    assert!(!empty.crosses_boundary(ScalarIndex::new(3)));
}
