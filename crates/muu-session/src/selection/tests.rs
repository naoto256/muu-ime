// These inherited cases fix the non-learning selection contracts. Pass an
// explicit empty capture so their policy input stays the same after rule 3.
fn select_without_learning(
    segments: &muu_conversion::Segments,
    complete: bool,
    snapshot: &muu_conversion::CostSnapshot<'_>,
    context: &str,
    lm: &crate::core::LanguageModelSnapshot,
    segmenter: &muu_conversion::Segmenter,
) -> crate::selection::SelectionOutcome {
    crate::selection::select(
        segments,
        complete,
        snapshot,
        &crate::personal_frequency::CapturedPersonalFrequency::new(
            &crate::personal_frequency::PersonalFrequencyModel::new(Default::default(), None)
                .unwrap(),
        ),
        context,
        lm,
        segmenter,
    )
}
use muu_conversion::{CostSnapshot, Segmenter, Segments as ConversionSegments};

use muu_conversion::{
    Candidate, ScalarIndex, ScalarRange, Segment, SegmentState, Segments, TokenKey,
};

use crate::session::*;
use crate::test_support::*;
use crate::*;
use muu_dict::Dictionary;

// When the best-scoring hypothesis just repeats the kana reading, the run
// keeps the baseline and reports the veto, while its scores and ranks stay
// exactly as scored.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn reading_identity_is_rejected_per_run_without_rewriting_scored_evidence() {
    let Some(core) = run_selection_core() else {
        return;
    };
    let captured = core.conversion_snapshot();
    let costs = captured.costs().unwrap();
    let lm = core.language_model();
    let generated = generate_conversion("くも、きょう", &captured, &core.segmenter).unwrap();
    // Same self-written left context as the C ABI `kumo` fixture.
    let context = "はし";
    let selected = select_without_learning(
        generated.segments(),
        true,
        &costs,
        context,
        &lm,
        &core.segmenter,
    );
    assert_eq!(selected.report.runs.len(), 2);
    let first = &selected.report.runs[0];
    assert_eq!(first.skipped, None);
    let baseline = first.baseline_rank.unwrap();
    let identity = first
        .hypotheses
        .iter()
        .position(|h| h.surface == "くも")
        .unwrap();
    assert_ne!(baseline, identity);
    assert!(first.excluded_reading_identity_ranks.contains(&identity));
    assert_eq!(first.reading_identity_vetoed_rank, Some(identity));
    assert_eq!(first.selected_rank, first.baseline_rank);
    let surfaces = first
        .hypotheses
        .iter()
        .map(|h| h.surface.as_str())
        .collect::<Vec<_>>();
    let scores = lm
        .model
        .as_ref()
        .unwrap()
        .score(context, "くも", &surfaces)
        .unwrap();
    assert_eq!(
        scores,
        first.hypotheses.iter().map(|h| h.score).collect::<Vec<_>>()
    );
    let best = scores
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .unwrap()
        .0;
    assert_eq!(best, identity);
    assert!(scores[baseline] - scores[best] >= muu_lm::MIN_SWITCH_MARGIN);
    assert!(
        selected
            .segments
            .segments()
            .iter()
            .map(|s| s.focused().surface())
            .collect::<String>()
            .starts_with("雲、")
    );
    let unavailable = select_without_learning(
        generated.segments(),
        true,
        &costs,
        "",
        &LanguageModelSnapshot {
            state: LanguageModelState::Unloaded,
            model: None,
            info: crate::LanguageModelInfo::default(),
        },
        &core.segmenter,
    );
    assert!(
        unavailable
            .report
            .runs
            .iter()
            .all(|r| r.excluded_reading_identity_ranks.is_empty()
                && r.reading_identity_vetoed_rank.is_none())
    );
}

// Kana runs separated by any non-kana literal are scored separately; the
// second run's model input uses the first run's applied choice and the
// literal, not the original first candidate.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn selection_runs_cross_punctuation_numbers_latin_katakana_and_symbols() {
    let Some(core) = run_selection_core() else {
        return;
    };
    let captured = core.conversion_snapshot();
    let costs = captured.costs().unwrap();
    let lm = core.language_model();
    for literal in ["、", "12", "abc", "カタカナ", "!"] {
        let reading = format!("へんかんせいどがわるい{literal}きょう");
        let generated = generate_conversion(&reading, &captured, &core.segmenter).unwrap();
        let selected =
            select_without_learning(generated.segments(), true, &costs, "", &lm, &core.segmenter);
        assert_eq!(selected.report.skipped, None, "{literal}");
        assert_eq!(selected.report.runs.len(), 2, "{literal}");
        for run in &selected.report.runs {
            assert_eq!(run.skipped, None, "{literal}");
            assert!(!run.hypotheses.is_empty());
        }
        let first = &selected.report.runs[0];
        assert_ne!(first.baseline_rank, first.selected_rank, "{literal}");
        let chosen = &first.hypotheses[first.selected_rank.unwrap()].surface;
        assert_eq!(chosen, "変換精度が悪い");
        let second = &selected.report.runs[1];
        let surfaces = second
            .hypotheses
            .iter()
            .map(|h| h.surface.as_str())
            .collect::<Vec<_>>();
        // Scores must use the changed first run and literal, not the old
        // first candidate. Equal scores prove the actual model input.
        let context = format!("{chosen}{literal}");
        let scores = lm
            .model
            .as_ref()
            .unwrap()
            .score(&context, "きょう", &surfaces)
            .unwrap();
        assert_eq!(
            scores,
            second
                .hypotheses
                .iter()
                .map(|h| h.score)
                .collect::<Vec<_>>()
        );
        assert_eq!(second.context_chars, context.chars().count());
        let range = first.reading_range.end.get()..second.reading_range.start.get();
        let outside = |segments: &Segments| {
            segments
                .segments()
                .iter()
                .flat_map(|s| &s.focused().paths()[0].nodes)
                .filter(|n| range.contains(&n.start.get()))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            outside(generated.segments()),
            outside(&selected.segments),
            "literal authority stays unchanged"
        );
    }
}

// A ー after katakana stays with the katakana and does not start a kana
// run, so the following hiragana is still one selectable run; a reading of
// only such text has no runs.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn katakana_prolonged_marks_leave_the_following_hiragana_selectable() {
    let Some(core) = run_selection_core() else {
        return;
    };
    let captured = core.conversion_snapshot();
    for reading in ["コーヒー、をのむ", "コーヒーをのむ"] {
        let generated = generate_conversion(reading, &captured, &core.segmenter).unwrap();
        let selected = select_without_learning(
            generated.segments(),
            true,
            &captured.costs().unwrap(),
            "",
            &core.language_model(),
            &core.segmenter,
        );
        assert_eq!(selected.report.runs.len(), 1);
        let run = &selected.report.runs[0];
        assert_eq!(
            run.reading_range.start.get(),
            if reading.contains('、') { 5 } else { 4 }
        );
        assert_eq!(run.skipped, None);
        assert!(!run.hypotheses.is_empty());
        assert_eq!(
            selected.segments.segments()[0].focused().paths()[0].nodes[0],
            generated.segments().segments()[0].focused().paths()[0].nodes[0]
        );
    }
    for reading in ["ー", "aー"] {
        let generated = generate_conversion(reading, &captured, &core.segmenter).unwrap();
        let selected = select_without_learning(
            generated.segments(),
            true,
            &captured.costs().unwrap(),
            "",
            &core.language_model(),
            &core.segmenter,
        );
        assert_eq!(
            selected.report.skipped,
            Some(SelectionSkipped::NonKanaReading)
        );
        assert!(selected.report.runs.is_empty());
        assert_eq!(selected.segments, *generated.segments());
    }
}

// A run whose constraints leave only the baseline reports
// no_alternative_after_constraints, ahead of the model's state.
#[test]
fn literal_constraint_with_only_the_baseline_skips_before_model_availability() {
    let dictionary = conversion_dictionary();
    let dictionary = Dictionary::open(dictionary.path()).unwrap();
    let costs = CostSnapshot::baseline(&dictionary);
    let candidate = |start, end, surface, token| {
        let range = ScalarRange::new(ScalarIndex::new(start), ScalarIndex::new(end));
        let mut paths = conversion_candidate(range, surface, 0, token)
            .paths()
            .to_vec();
        paths[0].snapshot_revision = costs.revision();
        Segment::new(
            range,
            vec![Candidate::from_group(range, paths).unwrap()],
            0,
            SegmentState::Unconfirmed,
        )
        .unwrap()
    };
    let input = Segments::new(
        "aは".to_owned(),
        vec![candidate(0, 1, "a", 0), candidate(1, 2, "は", 1)],
        0,
    )
    .unwrap();
    let lm = LanguageModelSnapshot {
        state: LanguageModelState::Loading,
        model: None,
        info: crate::LanguageModelInfo::default(),
    };
    let selected = select_without_learning(
        &input,
        true,
        &costs,
        "",
        &lm,
        &Segmenter::new(Vec::new(), muu_conversion::BoundaryDecision::Break),
    );
    assert_eq!(selected.segments, input);
    assert_eq!(selected.report.skipped, None);
    assert_eq!(
        selected.report.runs[0].skipped,
        Some(SelectionSkipped::NoAlternativeAfterConstraints)
    );
    assert!(selected.report.runs[0].hypotheses.is_empty());
}

// A node crossing the run boundary reports
// inseparable_reading_boundary before any other reason.
#[test]
fn boundary_failure_precedes_no_alternative_and_model_loading() {
    let file = conversion_dictionary();
    let dictionary = Dictionary::open(file.path()).unwrap();
    let costs = CostSnapshot::baseline(&dictionary);
    let range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(2));
    let mut paths = conversion_candidate(range, "カあ", 0, 0).paths().to_vec();
    paths[0].snapshot_revision = costs.revision();
    let input = Segments::new(
        "カあ".into(),
        vec![
            Segment::new(
                range,
                vec![Candidate::from_group(range, paths).unwrap()],
                0,
                SegmentState::Unconfirmed,
            )
            .unwrap(),
        ],
        0,
    )
    .unwrap();
    let selected = select_without_learning(
        &input,
        true,
        &costs,
        "",
        &LanguageModelSnapshot {
            state: LanguageModelState::Loading,
            model: None,
            info: crate::LanguageModelInfo::default(),
        },
        &Segmenter::new(Vec::new(), muu_conversion::BoundaryDecision::Break),
    );
    assert_eq!(selected.segments, input);
    assert_eq!(
        selected.report.runs[0].skipped,
        Some(SelectionSkipped::InseparableReadingBoundary)
    );
    assert!(selected.report.runs[0].hypotheses.is_empty());
}

// A run with a protected segment and nothing else to vary reports
// no_alternative_after_constraints even while the model is loading.
#[test]
fn partial_registration_with_no_alternative_precedes_model_loading() {
    let file = conversion_dictionary();
    let dictionary = Dictionary::open(file.path()).unwrap();
    let costs = CostSnapshot::baseline(&dictionary);
    let make = |start, end, surface, token, registered| {
        let range = ScalarRange::new(ScalarIndex::new(start), ScalarIndex::new(end));
        let mut paths = conversion_candidate(range, surface, 0, token)
            .paths()
            .to_vec();
        paths[0].snapshot_revision = costs.revision();
        if registered {
            paths[0].nodes[0].token_key = TokenKey::Overlay {
                tier: muu_conversion::LexiconOverlayTier::User,
                dictionary_key: 0,
                entry_id: 0,
            };
        }
        Segment::new(
            range,
            vec![Candidate::from_group(range, paths).unwrap()],
            0,
            SegmentState::Unconfirmed,
        )
        .unwrap()
    };
    // The mutable reading is "い" and has only its unknown surface in
    // this fixture's lattice. Catalog size alone no longer proves that
    // no full-lattice alternative exists (docs/src/internals/conversion.md「言語モデルによる選び直し」).
    let input = Segments::new(
        "あい".into(),
        vec![make(0, 1, "登録語", 0, true), make(1, 2, "い", 1, false)],
        0,
    )
    .unwrap();
    let selected = select_without_learning(
        &input,
        true,
        &costs,
        "",
        &LanguageModelSnapshot {
            state: LanguageModelState::Loading,
            model: None,
            info: crate::LanguageModelInfo::default(),
        },
        &Segmenter::new(Vec::new(), muu_conversion::BoundaryDecision::Break),
    );
    assert_eq!(selected.segments, input);
    assert_eq!(selected.report.runs[0].protected_segments, vec![0]);
    assert_eq!(
        selected.report.runs[0].skipped,
        Some(SelectionSkipped::NoAlternativeAfterConstraints)
    );
    assert!(selected.report.runs[0].hypotheses.is_empty());
}

// A User word with its particle is frozen as a whole: alone it skips as
// authoritative; beside other text, every hypothesis keeps its nodes and
// only the rest can switch.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn registered_noun_with_a_particle_is_fixed_while_other_segments_are_scored() {
    let Some(core) = run_selection_core() else {
        return;
    };
    core.set_user_fixture(user_dictionary_snapshot([(
        "きょう",
        "共有",
        UserDictionaryPriority::High,
    )]))
    .unwrap();
    let captured = core.conversion_snapshot();
    let costs = captured.costs().unwrap();
    let unloaded = LanguageModelSnapshot {
        state: LanguageModelState::Unloaded,
        model: None,
        info: crate::LanguageModelInfo::default(),
    };
    let short = generate_conversion("きょうは", &captured, &core.segmenter).unwrap();
    assert_eq!(short.segments().segments().len(), 1);
    assert_eq!(
        short.segments().segments()[0].focused().paths()[0]
            .nodes
            .len(),
        2
    );
    assert_eq!(short.segments().segments()[0].focused().surface(), "共有は");
    for lm in [unloaded, core.language_model()] {
        let selected =
            select_without_learning(short.segments(), true, &costs, "", &lm, &core.segmenter);
        assert_eq!(selected.segments, *short.segments());
        let run = &selected.report.runs[0];
        assert_eq!(run.protected_segments, vec![0]);
        assert_eq!(
            run.skipped,
            Some(SelectionSkipped::AuthoritativeDictionaryBaseline)
        );
        assert!(run.hypotheses.is_empty());
    }
    let long =
        generate_conversion("きょうをへんかんせいどがわるい", &captured, &core.segmenter).unwrap();
    assert_eq!(long.segments().segments()[0].focused().surface(), "共有を");
    let selected = select_without_learning(
        long.segments(),
        true,
        &costs,
        "",
        &core.language_model(),
        &core.segmenter,
    );
    assert_eq!(selected.report.runs.len(), 1);
    let run = &selected.report.runs[0];
    assert_eq!(run.protected_segments, vec![0]);
    assert_eq!(run.skipped, None);
    assert!(run.hypotheses.len() > 1);
    assert!(
        run.hypotheses
            .iter()
            .all(|h| h.surface.starts_with("共有を"))
    );
    for h in &run.hypotheses {
        let frozen = &long.segments().segments()[0].focused().paths()[0].nodes;
        assert_eq!(&h.hypothesis.nodes()[..frozen.len()], frozen);
    }
    assert_eq!(
        selected.segments.segments()[0],
        long.segments().segments()[0]
    );
    assert_ne!(run.selected_rank, run.baseline_rank);
    assert!(
        selected
            .segments
            .segments()
            .iter()
            .map(|s| s.focused().surface())
            .collect::<String>()
            .contains("変換精度")
    );
}

// The same protection for an imported word and its particle.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn imported_word_with_particle_is_preserved_through_partial_run_selection() {
    let Some(core) = run_selection_core() else {
        return;
    };
    core.set_imported_fixture(ImportedDictionaryCatalogSnapshot {
        schema_version: 1,
        dictionaries: vec![ImportedDictionarySnapshot {
            dictionary_id: "partial-protection".into(),
            enabled: true,
            entries: vec![ImportedDictionaryRecord {
                reading: "きょう".into(),
                surface: "共有".into(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            }],
        }],
    })
    .unwrap();
    let captured = core.conversion_snapshot();
    let generated =
        generate_conversion("きょうをへんかんせいどがわるい", &captured, &core.segmenter).unwrap();
    let first = &generated.segments().segments()[0];
    assert_eq!(first.focused().surface(), "共有を");
    assert_eq!(first.focused().paths()[0].nodes.len(), 2);
    assert!(matches!(
        first.focused().paths()[0].nodes[0].token_key,
        TokenKey::Overlay {
            tier: muu_conversion::LexiconOverlayTier::Imported,
            ..
        }
    ));
    assert_partial_protection_through_scoring_and_application(
        generated.segments(),
        &captured,
        &core.language_model(),
        "共有を",
        &core.segmenter,
    );
}

// The same protection for a postal candidate at a nonzero index, whose
// particle lies inside the run.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn postal_literal_and_particle_are_preserved_through_partial_run_selection() {
    let Some(core) = run_selection_core() else {
        return;
    };
    core.set_postal_fixture(postal_code_snapshot([("1000001", "東京都千代田区千代田")]))
        .unwrap();
    let captured = core.conversion_snapshot();
    let generated = generate_conversion(
        "1000001をへんかんせいどがわるい",
        &captured,
        &core.segmenter,
    )
    .unwrap();
    let mut input = generated.segments().clone();
    // Generation ranks the numeric literal ahead of the postal variant
    // for this joined phrase. Select a real generated postal candidate to
    // exercise partial protection and restoration of a nonzero index.
    let postal = input.segments()[0]
        .candidates()
        .iter()
        .find(|c| c.surface() == "東京都千代田区千代田を")
        .unwrap()
        .id()
        .clone();
    input.select_focused_candidate_by_id(&postal).unwrap();
    let first = &input.segments()[0];
    assert_ne!(first.focused_candidate(), 0);
    assert_eq!(first.focused().surface(), "東京都千代田区千代田を");
    assert_eq!(first.focused().paths()[0].nodes.len(), 2);
    assert!(matches!(
        first.focused().paths()[0].nodes[0].token_key,
        TokenKey::Overlay {
            tier: muu_conversion::LexiconOverlayTier::PostalCode,
            ..
        }
    ));
    // The postal node is outside the run; its attached particle is inside.
    // Exercise both registration freezing and literal-view write-back.
    assert_partial_protection_through_scoring_and_application(
        &input,
        &captured,
        &core.language_model(),
        "を",
        &core.segmenter,
    );
}

// Protection applies only to runs that overlap the registered segment;
// another run in the same conversion is still scored.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn registered_word_in_a_mixed_segment_protects_only_intersecting_runs() {
    let Some(core) = run_selection_core() else {
        return;
    };
    core.set_user_fixture(user_dictionary_snapshot([(
        "きょう",
        "登録語",
        UserDictionaryPriority::High,
    )]))
    .unwrap();
    let captured = core.conversion_snapshot();
    let generated = generate_conversion(
        "abcきょう、へんかんせいどがわるい",
        &captured,
        &core.segmenter,
    )
    .unwrap();
    let segments = generated.segments().segments();
    assert_eq!(segments[0].focused().surface(), "abc");
    assert_eq!(segments[1].focused().surface(), "登録語");
    // Explicitly join a literal and registered word. A fixed-boundary
    // input is allowed to contain both, even when these production POS
    // rules happen to keep this particular pair separate.
    let range = ScalarRange::new(
        segments[0].reading_range().start,
        segments[1].reading_range().end,
    );
    let mut path = segments[0].focused().paths()[0].clone();
    path.nodes
        .extend_from_slice(&segments[1].focused().paths()[0].nodes);
    path.cost += segments[1].focused().paths()[0].cost;
    let joined = Segment::new(
        range,
        vec![Candidate::from_group(range, vec![path]).unwrap()],
        0,
        SegmentState::Unconfirmed,
    )
    .unwrap();
    let mut input = vec![joined];
    input.extend_from_slice(&segments[2..]);
    let input = Segments::new(generated.segments().reading().to_owned(), input, 0).unwrap();
    let selected = select_without_learning(
        &input,
        true,
        &captured.costs().unwrap(),
        "",
        &core.language_model(),
        &core.segmenter,
    );
    assert_eq!(selected.report.runs.len(), 2);
    assert_eq!(
        selected.report.runs[0].skipped,
        Some(SelectionSkipped::AuthoritativeDictionaryBaseline)
    );
    assert_eq!(selected.report.runs[1].skipped, None);
    assert!(!selected.report.runs[1].hypotheses.is_empty());
    assert_eq!(selected.segments.segments()[0], input.segments()[0]);
}

// A reading with no hiragana gets non_kana_reading and is left unchanged.
#[test]
fn literal_only_readings_have_no_selection_runs() {
    let dictionary = build_conversion_dictionary();
    let core = Core::new(CoreConfig {
        dictionary_path: Some(dictionary.path()),
    })
    .unwrap();
    let captured = core.conversion_snapshot();
    for reading in ["、。", "123", "ABC", "カタカナ", "!?"] {
        let generated = generate_conversion(reading, &captured, &core.segmenter).unwrap();
        let selected = select_without_learning(
            generated.segments(),
            true,
            &captured.costs().unwrap(),
            "",
            &core.language_model(),
            &core.segmenter,
        );
        assert_eq!(
            selected.report.skipped,
            Some(SelectionSkipped::NonKanaReading)
        );
        assert!(selected.report.runs.is_empty());
        assert_eq!(selected.segments, *generated.segments());
    }
}

// A typed sentence with punctuation gives two scored runs from a single LM
// state read for the whole conversion.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn romaji_sentence_uses_two_selection_runs_and_one_lm_state_read() {
    let Some(core) = run_selection_core() else {
        return;
    };
    let mut session = core.create_session().unwrap();
    for character in "kyouha,amedesu.".chars() {
        let code = match character {
            ',' => 43,
            '.' => 47,
            _ => 0,
        };
        session.handle(Request::Key {
            key: KeyEvent {
                event_type: "key_down".to_owned(),
                keycode: code,
                modifiers: 0,
                characters_utf8: Some(character.to_string()),
                characters_ignoring_modifiers: Some(character.to_string()),
                is_repeat: false,
            },
        });
    }
    assert_eq!(session.composer.preedit().display, "きょうは、あめです。");
    let before = core
        .language_model_reads
        .load(std::sync::atomic::Ordering::Relaxed);
    session.handle(Request::Command {
        command: "convert".to_owned(),
        arg: serde_json::Value::Null,
    });
    assert_eq!(
        core.language_model_reads
            .load(std::sync::atomic::Ordering::Relaxed)
            - before,
        1
    );
    let report = session.last_selection_report.as_ref().unwrap();
    assert_eq!(report.skipped, None);
    assert_eq!(report.runs.len(), 2);
    assert!(
        report
            .runs
            .iter()
            .all(|run| run.skipped.is_none() && !run.hypotheses.is_empty())
    );
}

// Each global or per-run reason (incomplete evidence, non-kana, no model,
// authoritative baseline) keeps the generated segments unchanged.
#[test]
fn selection_report_explains_incomplete_non_kana_and_absent_model() {
    let dictionary = build_conversion_dictionary();
    let core = Core::new(CoreConfig {
        dictionary_path: Some(dictionary.path()),
    })
    .unwrap();
    let captured = core.conversion_snapshot();
    let snapshot = captured.costs().unwrap();
    for (reading, complete, reason) in [
        ("きょう", false, SelectionSkipped::IncompletePathEvidence),
        ("1", true, SelectionSkipped::NonKanaReading),
        ("きょう", true, SelectionSkipped::LanguageModelNotLoaded),
    ] {
        let generated = generate_conversion(reading, &captured, &core.segmenter).unwrap();
        let selected = select_without_learning(
            generated.segments(),
            complete,
            &snapshot,
            "",
            &core.language_model(),
            &core.segmenter,
        );
        assert_eq!(selected.segments, *generated.segments());
        assert_eq!(
            selected.report.skipped.or_else(|| selected
                .report
                .runs
                .first()
                .and_then(|run| run.skipped)),
            Some(reason)
        );
        assert!(
            selected
                .report
                .runs
                .iter()
                .all(|run| run.hypotheses.is_empty())
        );
        assert!(
            selected
                .report
                .runs
                .iter()
                .all(|run| run.context_chars == 0)
        );
        assert_eq!(
            selected.report.rules_version,
            muu_lm::SELECTION_RULES_VERSION
        );
    }
    core.set_user_fixture(user_dictionary_snapshot([(
        "きょう",
        "共有語",
        UserDictionaryPriority::High,
    )]))
    .unwrap();
    let captured = core.conversion_snapshot();
    let generated = generate_conversion("きょう", &captured, &core.segmenter).unwrap();
    let selected = select_without_learning(
        generated.segments(),
        true,
        &captured.costs().unwrap(),
        "",
        &core.language_model(),
        &core.segmenter,
    );
    assert_eq!(
        selected.report.runs[0].skipped,
        Some(SelectionSkipped::AuthoritativeDictionaryBaseline)
    );
    assert_eq!(selected.segments, *generated.segments());
}

// Unloaded, loading and failed each keep the baseline and report their own
// reason.
#[test]
fn every_unavailable_lm_state_keeps_the_same_baseline_with_its_reason() {
    let dictionary = build_conversion_dictionary();
    let core = Core::new(CoreConfig {
        dictionary_path: Some(dictionary.path()),
    })
    .unwrap();
    let captured = core.conversion_snapshot();
    let generated = generate_conversion("きょう", &captured, &core.segmenter).unwrap();
    for (state, reason) in [
        (
            LanguageModelState::Unloaded,
            SelectionSkipped::LanguageModelNotLoaded,
        ),
        (
            LanguageModelState::Loading,
            SelectionSkipped::LanguageModelLoading,
        ),
        (
            LanguageModelState::Failed,
            SelectionSkipped::LanguageModelFailed,
        ),
    ] {
        let lm = LanguageModelSnapshot {
            state,
            model: None,
            info: crate::LanguageModelInfo::default(),
        };
        let selected = select_without_learning(
            generated.segments(),
            true,
            &captured.costs().unwrap(),
            "",
            &lm,
            &core.segmenter,
        );
        assert_eq!(selected.segments, *generated.segments());
        assert_eq!(
            selected.report.skipped.or_else(|| selected
                .report
                .runs
                .first()
                .and_then(|run| run.skipped)),
            Some(reason)
        );
        assert!(
            selected
                .report
                .runs
                .iter()
                .all(|run| run.hypotheses.is_empty())
        );
    }
}

// For User, Imported and Postal alike, incomplete evidence and non-kana
// outrank the authoritative-baseline skip, and none changes the segments.
#[test]
fn selection_skip_precedence_protects_all_three_overlay_tiers() {
    let dictionary = build_conversion_dictionary();
    for tier in ["user", "imported", "postal"] {
        let core = Core::new(CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap();
        let (reading, surface) = if tier == "postal" {
            ("1000001", "東京都千代田区千代田")
        } else {
            ("きょう", "共有語")
        };
        match tier {
            "user" => core
                .set_user_fixture(user_dictionary_snapshot([(
                    "きょう",
                    "共有語",
                    UserDictionaryPriority::High,
                )]))
                .unwrap(),
            "imported" => core
                .set_imported_fixture(ImportedDictionaryCatalogSnapshot {
                    schema_version: 1,
                    dictionaries: vec![ImportedDictionarySnapshot {
                        dictionary_id: "test-imported".to_owned(),
                        enabled: true,
                        entries: vec![ImportedDictionaryRecord {
                            reading: reading.to_owned(),
                            surface: surface.to_owned(),
                            part_of_speech: UserDictionaryPartOfSpeech::noun(),
                            priority: UserDictionaryPriority::High,
                        }],
                    }],
                })
                .unwrap(),
            "postal" => core
                .set_postal_fixture(postal_code_snapshot([("1000001", "東京都千代田区千代田")]))
                .unwrap(),
            _ => unreachable!(),
        }
        let captured = core.conversion_snapshot();
        let generated = generate_conversion(reading, &captured, &core.segmenter).unwrap();
        assert_eq!(
            generated
                .segments()
                .segments()
                .iter()
                .map(|s| s.focused().surface())
                .collect::<String>(),
            surface
        );
        for (complete, reason) in [
            (false, SelectionSkipped::IncompletePathEvidence),
            (
                true,
                if tier == "postal" {
                    SelectionSkipped::NonKanaReading
                } else {
                    SelectionSkipped::AuthoritativeDictionaryBaseline
                },
            ),
        ] {
            let selected = select_without_learning(
                generated.segments(),
                complete,
                &captured.costs().unwrap(),
                "",
                &core.language_model(),
                &core.segmenter,
            );
            assert_eq!(
                selected.report.skipped.or_else(|| selected
                    .report
                    .runs
                    .first()
                    .and_then(|run| run.skipped)),
                Some(reason),
                "{tier}"
            );
            assert_eq!(selected.segments, *generated.segments());
        }
    }
    assert_eq!(
        postal_code_snapshot([("かなよみ", "住所")])
            .into_overlay()
            .unwrap_err(),
        PostalCodeError::InvalidReading
    );
}

// The baseline hypothesis is the input's actual path even when other
// candidate combinations give the same surface.
#[test]
#[ignore = "requires public edge (check-assets)"]
fn baseline_report_names_actual_input_path_despite_same_surface_candidate_combinations() {
    let Some(lm_dir) = real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
        return;
    };
    let dictionary = conversion_dictionary();
    let core = Core::new(CoreConfig {
        dictionary_path: Some(dictionary.path()),
    })
    .unwrap();
    core.configure_sessions(CoreSessionSettings {
        conversion_lm_dir: Some(lm_dir),
        conversion_lm_load_synchronously: true,
        ..Default::default()
    })
    .unwrap_or_else(|_| panic!("first settings"));
    let costs = CostSnapshot::baseline(core.dictionary.as_deref().unwrap());
    let first = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(1));
    let second = ScalarRange::new(ScalarIndex::new(1), ScalarIndex::new(2));
    let candidate = |range, surface, cost, token| {
        let mut paths = conversion_candidate(range, surface, cost, token)
            .paths()
            .to_vec();
        paths[0].snapshot_revision = costs.revision();
        Candidate::from_group(range, paths).unwrap()
    };
    let input = Segments::new(
        "あい".to_owned(),
        vec![
            Segment::new(
                first,
                vec![
                    candidate(first, "あ", 20, 0),
                    candidate(first, "あい", 0, 1),
                ],
                0,
                SegmentState::Unconfirmed,
            )
            .unwrap(),
            Segment::new(
                second,
                vec![
                    candidate(second, "いう", 20, 2),
                    candidate(second, "う", 0, 3),
                ],
                0,
                SegmentState::Unconfirmed,
            )
            .unwrap(),
        ],
        0,
    )
    .unwrap();
    // Baseline now replaces the enumerated surface representative with
    // its actual input path, without inventing catalog coordinates (docs/src/internals/conversion.md「言語モデルによる選び直し」).
    let selected = select_without_learning(
        &input,
        true,
        &costs,
        "",
        &core.language_model(),
        &core.segmenter,
    );
    assert_eq!(selected.report.skipped, None);
    assert_eq!(selected.report.runs.len(), 1);
    let report = &selected.report.runs[0];
    assert_eq!(report.skipped, None);
    let baseline = &report.hypotheses[report.baseline_rank.unwrap()].hypothesis;
    assert!(baseline.is_baseline());
    let actual = |segments: &Segments| {
        segments
            .segments()
            .iter()
            .flat_map(|s| s.focused().paths()[0].nodes.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(baseline.nodes(), actual(&input));
    assert_eq!(baseline.surface(), "あいう");
    let chosen = &report.hypotheses[report.selected_rank.unwrap()].hypothesis;
    assert_eq!(chosen.nodes(), actual(&selected.segments));
    if report.baseline_rank == report.selected_rank {
        assert_eq!(selected.segments, input);
        assert_eq!(
            input
                .segments()
                .iter()
                .map(|s| s.focused().id().as_str())
                .collect::<Vec<_>>(),
            ["0-1/d0.", "1-2/d2."]
        );
    }
}

// Reported hypotheses are whole-run surfaces with finite scores, at most 16,
// with context capped at 40 characters; the applied choice becomes the
// actual paths[0], and the baseline is always the input's path.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn selection_report_matches_whole_surfaces_and_actual_applied_nodes_with_bounded_context() {
    let (Some(dictionary_path), Some(lm_dir)) = (
        real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
        real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
    ) else {
        return;
    };
    let core = Core::new(CoreConfig {
        dictionary_path: Some(&dictionary_path),
    })
    .unwrap();
    core.configure_sessions(CoreSessionSettings {
        conversion_lm_dir: Some(lm_dir),
        conversion_lm_load_synchronously: true,
        ..Default::default()
    })
    .unwrap_or_else(|_| panic!("first settings"));
    let captured = core.conversion_snapshot();
    let generated =
        generate_conversion("へんかんせいどがわるい", &captured, &core.segmenter).unwrap();
    // A selected path is materialized as actual paths[0]; baseline is
    // always captured from the input, even with same-surface alternatives.
    let matches_actual_nodes = |hypothesis: &SelectionHypothesis, segments: &ConversionSegments| {
        hypothesis.nodes()
            == segments
                .segments()
                .iter()
                .flat_map(|s| s.focused().paths()[0].nodes.clone())
                .collect::<Vec<_>>()
    };
    for context in [String::new(), "あ".repeat(50)] {
        let selected = select_without_learning(
            generated.segments(),
            generated.path_evidence_complete(),
            &captured.costs().unwrap(),
            &context,
            &core.language_model(),
            &core.segmenter,
        );
        assert_eq!(selected.report.skipped, None);
        assert_eq!(selected.report.runs.len(), 1);
        let report = &selected.report.runs[0];
        assert_eq!(report.skipped, None);
        assert_eq!(report.context_chars, context.chars().count().min(40));
        assert!(report.hypotheses.len() <= muu_lm::MAX_SCORED_HYPOTHESES);
        assert!(report.hypotheses.iter().all(|h| h.score.is_finite()));
        let baseline = report.baseline_rank.unwrap();
        let chosen = report.selected_rank.unwrap();
        let whole_surface = |segments: &ConversionSegments| {
            segments
                .segments()
                .iter()
                .map(|s| s.focused().surface())
                .collect::<String>()
        };
        assert_eq!(
            report.hypotheses[baseline].hypothesis.surface(),
            whole_surface(generated.segments())
        );
        assert_eq!(
            report.hypotheses[chosen].hypothesis.surface(),
            whole_surface(&selected.segments)
        );
        assert!(
            matches_actual_nodes(
                &report.hypotheses[baseline].hypothesis,
                generated.segments()
            ),
            "actual baseline/selected nodes, not surface-only authority"
        );
        assert!(
            matches_actual_nodes(&report.hypotheses[chosen].hypothesis, &selected.segments),
            "actual baseline/selected nodes, not surface-only authority"
        );
        let best = report
            .hypotheses
            .iter()
            .map(|h| h.score)
            .min_by(f32::total_cmp)
            .unwrap();
        assert_eq!(
            report.baseline_minus_best_score,
            Some(report.hypotheses[baseline].score - best)
        );
        if context.is_empty() {
            assert_ne!(chosen, baseline);
            assert_eq!(
                report.hypotheses[chosen].hypothesis.surface(),
                "変換精度が悪い"
            );
        }
    }
}
