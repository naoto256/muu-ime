//! When the model may replace the lattice's first hypothesis.

/// Edge's policy version, reported by `ModelKind::Edge` and by muu-session
/// when no model scored. `select_for` uses the actual model's version and
/// margin; the reading-identity veto and captured token learning are
/// applied by muu-session.
pub const SELECTION_RULES_VERSION: u32 = 3;

/// Nats by which edge must prefer an alternative (docs/src/internals/conversion.md「替えるかどうかの規則」). Zenz has its own
/// margin; see `ModelKind::switch_margin`.
pub const MIN_SWITCH_MARGIN: f32 = 2.0;

/// Index of the hypothesis to use instead of `baseline`, if any. The margin
/// comes from `kind`'s profile; there is no model-less default.
///
/// Do not constrain character types: doing so also prevents correcting the
/// lattice's mistaken kanji for colloquial endings and particles back to kana.
/// The margin alone was preferred (docs/src/internals/conversion.md「替えるかどうかの規則」);
/// spelling changes in digits, Latin letters, and symbols follow the same rule.
///
/// `surfaces` is consulted only to check that it parallels `scores`; the
/// decision is made from the scores. The one rule that depends on what a
/// surface says, never switching to the unchanged reading, is applied by the
/// caller in `muu-session`, because only it knows the reading. A caller that
/// uses this function directly does not get that rule.
///
/// Never returns `Some(baseline)`: callers tell "kept" from "switched" by
/// comparing the returned index with the baseline.
// Non-finite scores never win, and a non-finite baseline blocks any switch:
// an input too long for the model scores everything as infinity and keeps
// the lattice choice. Among equal minimum scores the lowest index wins
// (`min_by` keeps the first), as docs/src/internals/conversion.md「替えるかどうかの規則」 requires.
pub fn select_for(
    kind: crate::ModelKind,
    baseline: usize,
    surfaces: &[&str],
    scores: &[f32],
) -> Option<usize> {
    if surfaces.len() != scores.len() || baseline >= surfaces.len() {
        return None;
    }
    let (best, best_score) = scores
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, score)| score.is_finite())
        .min_by(|left, right| left.1.total_cmp(&right.1))?;
    if best == baseline
        || !scores[baseline].is_finite()
        || scores[baseline] - best_score < kind.switch_margin()
    {
        return None;
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_model_selects_its_own_rule_version_and_margin() {
        use crate::ModelKind::{Edge, Zenz};
        let surfaces = ["baseline", "alternative"];
        assert_eq!(Edge.rules_version(), 3);
        assert_eq!(Zenz.rules_version(), 4);
        assert_eq!(select_for(Zenz, 0, &surfaces, &[2.5, 2.0]), Some(1));
        assert_eq!(select_for(Zenz, 0, &surfaces, &[2.49, 2.0]), None);
        assert_eq!(select_for(Edge, 0, &surfaces, &[2.5, 2.0]), None);
        assert_eq!(select_for(Edge, 0, &surfaces, &[4.0, 2.0]), Some(1));
        assert_eq!(select_for(Zenz, 0, &surfaces, &[f32::NAN, 2.0]), None);
    }

    // Edge is named explicitly: the 2.0 margin these cases straddle is edge's.
    // A clear preference switches; a gap under the margin, or a baseline that
    // is already the best, keeps the lattice choice.
    #[test]
    fn switches_only_on_a_clear_score_preference() {
        let surfaces = ["変換制度が悪い", "変換精度が悪い", "返還制度が悪い"];
        assert_eq!(
            select_for(crate::ModelKind::Edge, 0, &surfaces, &[12.0, 9.5, 14.0]),
            Some(1)
        );
        assert_eq!(
            select_for(crate::ModelKind::Edge, 0, &surfaces, &[12.0, 10.5, 14.0]),
            None
        );
        assert_eq!(
            select_for(crate::ModelKind::Edge, 1, &surfaces, &[12.0, 9.5, 14.0]),
            None
        );
    }

    #[test]
    fn uses_the_same_margin_for_kana_digits_latin_and_symbols() {
        // Contract: one margin regardless of script or spelling. Every pair
        // goes through the clear switch, both sides of the 2.0 boundary and
        // the reversed baseline.
        for surfaces in [
            ["そのほうがいい", "その方がいい"],
            ["だめ", "ダメ"],
            // A simultaneous spelling change does not veto the preference.
            ["行ったとおり", "言った通り"],
            ["年のため", "念のため"],
            ["値", "ね"],
            ["ね", "値"],
            ["3人", "三人"],
            ["エービーシー", "ABC"],
            ["abc", "ＡＢＣ"],
            ["・", "-"],
        ] {
            assert_eq!(
                select_for(crate::ModelKind::Edge, 0, &surfaces, &[20.0, 10.0]),
                Some(1)
            );
            assert_eq!(
                select_for(crate::ModelKind::Edge, 0, &surfaces, &[12.0, 10.0]),
                Some(1)
            );
            assert_eq!(
                select_for(crate::ModelKind::Edge, 0, &surfaces, &[11.99, 10.0]),
                None
            );
            assert_eq!(
                select_for(crate::ModelKind::Edge, 1, &surfaces, &[10.0, 12.0]),
                Some(0)
            );
            assert_eq!(
                select_for(crate::ModelKind::Edge, 1, &surfaces, &[10.0, 11.99]),
                None
            );
        }
        assert_eq!(SELECTION_RULES_VERSION, 3);
    }

    // Mismatched lengths, an out-of-range baseline and a non-finite baseline
    // all keep the lattice choice rather than switching or panicking.
    #[test]
    fn rejects_malformed_input() {
        assert_eq!(
            select_for(crate::ModelKind::Edge, 0, &["a"], &[1.0, 2.0]),
            None
        );
        assert_eq!(
            select_for(crate::ModelKind::Edge, 3, &["a", "b"], &[1.0, 2.0]),
            None
        );
        assert_eq!(
            select_for(crate::ModelKind::Edge, 0, &["年", "念"], &[f32::NAN, 1.0]),
            None
        );
    }
}
