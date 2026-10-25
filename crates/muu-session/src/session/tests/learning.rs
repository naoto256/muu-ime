use std::collections::BTreeSet;

use std::fs;

use muu_conversion::{ScalarIndex, ScalarRange, TokenKey};
use muu_dict::{
    DictionaryBuilder, LeftContextId, LexiconEntry, RightContextId, UnknownClass, UnknownDefinition,
};
use tempfile::NamedTempFile;

use crate::session::*;
use crate::test_support::*;
use muu_dict::CoarsePos;

// With learning that prefers 返還 and an LM that picks 変換, the LM choice is
// first and the learned generation choice second, so one Space returns to
// it. Only that Space counts as an explicit choice, in the record and as one
// more learning observation; accepting the LM choice learns nothing.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn one_space_returns_to_the_learned_generation_candidate_after_lm_selection() {
    let (Some(dictionary_path), Some(lm_dir)) = (
        real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
        real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
    ) else {
        return;
    };
    let type_reading = |session: &mut Session| {
        for ch in "henkan".chars() {
            session.handle(key(&ch.to_string(), 0));
        }
        session.handle(key(" ", 49))
    };
    let mut training = new_session(Some(&dictionary_path), None, Default::default()).unwrap();
    let initial = type_reading(&mut training);
    let previous = initial
        .candidates
        .unwrap()
        .items
        .iter()
        .position(|c| c.text == "返還")
        .unwrap();
    training.select_candidate_index(previous);
    training.commit();
    let mut frequency = learning_snapshot(&training).unwrap();
    for entry in &mut frequency.entries {
        entry.count = 4;
    }
    frequency.observation_count = 4;
    for return_to_generation in [false, true] {
        let mut session = new_session(
            Some(&dictionary_path),
            Some(&lm_dir),
            TestSessionConfig {
                personal_frequency: frequency.clone(),
                commit_stream_config: Some(test_commit_stream_config("candidate-order")),
                ..Default::default()
            },
        )
        .unwrap();
        session
            .preceding_commit_context
            .observe("日本語の入力を漢字に");
        let converted = type_reading(&mut session);
        let candidates = converted.candidates.unwrap();
        assert_eq!(candidates.selected_index, 0);
        assert_eq!(candidates.items[0].text, "変換");
        assert_eq!(candidates.items[1].text, "返還");
        assert!(session.explicitly_selected_segment_ranges.is_empty());
        if return_to_generation {
            let next = session.handle(key(" ", 49));
            assert_eq!(next.candidates.unwrap().selected_index, 1);
            assert_eq!(next.preedit[0].text, "返還");
        }
        let committed = session.commit();
        assert_eq!(
            committed.commit_text.as_deref(),
            Some(if return_to_generation {
                "返還"
            } else {
                "変換"
            })
        );
        let event: serde_json::Value =
            serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
        assert_eq!(
            event["final"]["candidate_selected_explicitly"],
            return_to_generation
        );
        assert_eq!(
            event["final"]["segments"][0]["explicitly_selected"],
            return_to_generation
        );
        let learned: PersonalFrequencySnapshot = serde_json::from_slice(
            &session
                .core
                .personal_frequency_update(None)
                .unwrap()
                .unwrap()
                .data,
        )
        .unwrap();
        assert_eq!(
            learned.observation_count,
            frequency.observation_count + u64::from(return_to_generation)
        );
        assert_eq!(
            learned.entries[0].dictionary_token_id,
            frequency.entries[0].dictionary_token_id
        );
        assert_eq!(
            learned.entries[0].count,
            frequency.entries[0].count + u16::from(return_to_generation)
        );
    }
}

// When the generation choice comes back with the same surface but another
// path, presentation places it second by surface, and committing it learns
// and records that new path's tokens, not the old candidate's.
#[test]
fn a_same_surface_return_learns_and_records_the_new_neighbourhood_representative() {
    let mut builder =
        DictionaryBuilder::new(3, 3, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.provenance(b"name = \"same-surface-return-session-test\"\n");
    builder
        .set_connection_cost(RightContextId::new(2), LeftContextId::new(1), 1_000)
        .unwrap();
    builder
        .set_connection_cost(RightContextId::new(1), LeftContextId::new(2), 500)
        .unwrap();
    builder
        .set_connection_cost(RightContextId::new(2), LeftContextId::new(2), -2_000)
        .unwrap();
    for (reading, surface, left, right, cost) in [
        ("あい", "返還", 1, 1, 0),
        ("あい", "返還", 1, 2, 500),
        ("あい", "安", 1, 2, 300),
        ("あい", "変換", 1, 1, 200),
        ("うえ", "機能", 1, 1, 0),
        ("うえ", "作業", 2, 1, 2_500),
    ] {
        builder.add_entry(LexiconEntry {
            reading: reading.to_owned(),
            surface: surface.to_owned(),
            left_id: LeftContextId::new(left),
            right_id: RightContextId::new(right),
            left_pos_id: CoarsePos::Noun.id(),
            right_pos_id: CoarsePos::Noun.id(),
            word_cost: cost,
        });
    }
    builder.add_unknown(UnknownDefinition {
        class: UnknownClass::Other,
        group: false,
        max_length: 1,
        left_id: LeftContextId::new(0),
        right_id: RightContextId::new(0),
        left_pos_id: CoarsePos::Other.id(),
        right_pos_id: CoarsePos::Other.id(),
        word_cost: 10_000,
    });
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), builder.build().unwrap()).unwrap();
    let mut session = new_session(
        Some(file.path()),
        None,
        TestSessionConfig {
            commit_stream_config: Some(test_commit_stream_config("same-surface-return")),
            ..Default::default()
        },
    )
    .unwrap();
    for ch in "aiue".chars() {
        session.handle(key(&ch.to_string(), 0));
    }
    session.handle(key(" ", 49));
    let original = session.segments.as_ref().unwrap().clone();
    assert_eq!(original.segments()[0].focused().surface(), "返還");
    let old_id = original.segments()[0].focused().id().clone();
    let old_path = original.segments()[0].focused().paths()[0].clone();
    let captured = session.conversion_snapshot.as_ref().unwrap();
    let snapshot = captured.costs().unwrap();
    // `one_space_returns_to_the_learned_generation_candidate_after_lm_selection`
    // pins the real-model decision. Choose a real enumerated
    // path here to make the changed-neighbour representative deterministic,
    // then cross the materializer/Session boundary and use normal keys.
    let baseline = original
        .segments()
        .iter()
        .flat_map(|s| s.focused().paths()[0].nodes.iter().cloned())
        .collect::<Vec<_>>();
    let hypotheses = muu_conversion::FullHypothesisLattice::build(
        original.reading(),
        &snapshot,
        muu_conversion::HypothesisBudget::default(),
    )
    .unwrap()
    .constrain(&[])
    .unwrap()
    .enumerate(&baseline, &snapshot, 16)
    .unwrap();
    let chosen = hypotheses
        .iter()
        .find(|h| h.surface() == "変換作業")
        .unwrap();
    let rebuilt = muu_conversion::rebuild_run_from_hypothesis(
        &original,
        ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(4)),
        chosen,
        &[],
        &session.core.segmenter,
        &snapshot,
    )
    .unwrap();
    let returned = rebuilt.segments()[0].candidates()[1].clone();
    assert_eq!(rebuilt.segments()[0].focused().surface(), "変換");
    assert_eq!(returned.surface(), "返還");
    assert_ne!(returned.id(), &old_id);
    assert_ne!(returned.paths()[0].nodes, old_path.nodes);
    assert_eq!(
        returned.paths()[0].nodes[0].right_id,
        RightContextId::new(2)
    );
    // Its cheaper competitor remains in the tail; surface matching, rather
    // than cost order or the old ID, is what places this return second.
    assert_eq!(rebuilt.segments()[0].candidates()[2].surface(), "安");
    assert!(rebuilt.segments()[0].candidates()[2].cost() < returned.cost());
    session.segments = Some(rebuilt);
    assert!(session.explicitly_selected_segment_ranges.is_empty());
    let next = session.handle(key(" ", 49));
    assert_eq!(next.candidates.as_ref().unwrap().selected_index, 1);
    assert_eq!(
        next.candidates.as_ref().unwrap().items[1].id,
        returned.id().as_str()
    );
    assert_eq!(
        session.segments.as_ref().unwrap().segments()[0].focused(),
        &returned
    );
    let committed = session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("返還作業"));
    let learned = learning_snapshot(&session).unwrap();
    let TokenKey::Dictionary(new_token) = returned.paths()[0].nodes[0].token_key else {
        panic!("dictionary return")
    };
    let TokenKey::Dictionary(old_token) = old_path.nodes[0].token_key else {
        panic!("dictionary input")
    };
    assert_ne!(new_token, old_token);
    assert_eq!(learned.observation_count, 1);
    assert_eq!(
        learned.entries,
        vec![PersonalFrequencyEntry {
            dictionary_token_id: new_token.get(),
            count: 1
        }]
    );
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["final"]["candidate_selected_explicitly"], true);
    assert_eq!(event["final"]["segments"][0]["explicitly_selected"], true);
    assert_eq!(event["final"]["segments"][1]["explicitly_selected"], false);
    assert_eq!(
        event["final"]["segments"][0]["candidate_id"],
        returned.id().as_str()
    );
    assert_eq!(event["final"]["segments"][0]["surface"], returned.surface());
    assert_eq!(
        event["final"]["segments"][0]["path"]["nodes"][0]["token"],
        serde_json::json!({"kind":"dictionary","token_id":new_token.get()})
    );
}

// Moving focus by command or by arrow key keeps the LM's choices and the
// manual selection marks exactly; navigation is not a new conversion.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn both_focus_navigation_entries_preserve_lm_manual_choices_and_learning_marks() {
    let (Some(dictionary_path), Some(lm_dir)) = (
        real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
        real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
    ) else {
        return;
    };
    let mut session = new_session(
        Some(&dictionary_path),
        Some(&lm_dir),
        TestSessionConfig {
            commit_stream_config: Some(test_commit_stream_config("navigation")),
            ..Default::default()
        },
    )
    .unwrap();
    for ch in "henkanseidogawarui".chars() {
        session.handle(key(&ch.to_string(), 0));
    }
    let converted = session.handle(key(" ", 49));
    assert_eq!(
        converted
            .preedit
            .iter()
            .map(|p| p.text.as_str())
            .collect::<String>(),
        "変換精度が悪い"
    );
    let original = session.segments.as_ref().unwrap().clone();
    assert!(original.segments().len() > 1);
    assert!(session.explicitly_selected_segment_ranges.is_empty());
    // Visit every segment by the public command, then return by the
    // actual arrow-key entry. Neither may undo automatic selection (docs/src/internals/conversion.md「文節の移動と伸縮」).
    for _ in 1..original.segments().len() {
        session.handle(command(
            "move_segment",
            serde_json::json!({"direction":"right"}),
        ));
        assert_eq!(
            session.segments.as_ref().unwrap().segments(),
            original.segments()
        );
    }
    for _ in 1..original.segments().len() {
        session.handle(key("", 123));
        assert_eq!(
            session.segments.as_ref().unwrap().segments(),
            original.segments()
        );
    }
    assert_eq!(session.segments.as_ref().unwrap(), &original);
    assert!(session.explicitly_selected_segment_ranges.is_empty());
    let manual_index = (original.segments()[0].focused_candidate() + 1)
        % original.segments()[0].candidates().len();
    assert_ne!(manual_index, original.segments()[0].focused_candidate());
    session.select_candidate_index(manual_index);
    let manual = session.segments.as_ref().unwrap().clone();
    let marks = session.explicitly_selected_segment_ranges.clone();
    assert!(!marks.is_empty());
    session.handle(key("", 124));
    session.handle(command(
        "move_segment",
        serde_json::json!({"direction":"left"}),
    ));
    assert_eq!(session.segments.as_ref().unwrap(), &manual);
    assert_eq!(session.explicitly_selected_segment_ranges, marks);
    // Moving beyond the first segment is still a no-op.
    session.handle(key("", 123));
    assert_eq!(session.segments.as_ref().unwrap(), &manual);
    let committed = session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["final"]["segment_moved"], true);
    assert_eq!(event["final"]["surface"], committed.commit_text.unwrap());
    assert!(
        event["final"]["segments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["explicitly_selected"] == true)
    );
}

// Repeatedly choosing 京 eventually makes it the first conversion (bounded
// bonus); clearing learning starts a new generation and restores 今日.
#[test]
fn explicit_dictionary_selection_adjusts_frequency_and_clear_restores_baseline() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    let mut learned = false;
    for _ in 0..8 {
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        let converted = session.handle(key(" ", 49));
        if converted.preedit[0].text == "京" {
            learned = true;
            session.handle(key("\r", 36));
            break;
        }
        let candidate_index = converted
            .candidates
            .as_ref()
            .and_then(|candidates| candidates.items.iter().position(|item| item.text == "京"))
            .expect("learnable dictionary candidate");
        session.select_candidate_index(candidate_index);
        assert_eq!(
            session.handle(key("\r", 36)).commit_text.as_deref(),
            Some("京")
        );
    }
    assert!(
        learned,
        "bounded frequency bonus should eventually outrank baseline"
    );

    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let converted = session.handle(key(" ", 49));
    assert_eq!(converted.preedit[0].text, "京");
    session.handle(key("\r", 36));

    let snapshot =
        learning_snapshot(&session).expect("explicit selection publishes a complete snapshot");
    assert!(!snapshot.entries.is_empty());
    assert!(snapshot.entries.iter().all(|entry| entry.count > 0));
    assert_eq!(learning_snapshot(&session).unwrap(), snapshot);

    let before_clear = session.core.learning_version();
    let cleared = session.core.clear_personal_frequency();
    assert_ne!(before_clear.generation, cleared.generation);
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let baseline = session.handle(key(" ", 49));
    assert_eq!(baseline.preedit[0].text, "今日");
}

// Only the segment whose candidate changed is learned; tokens of untouched
// segments must not appear in the learned set.
#[test]
fn explicit_selection_learns_only_the_changed_segment_dictionary_tokens() {
    let dictionary = multi_segment_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "aiueoka".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    assert!(session.segments.as_ref().unwrap().segments().len() > 1);
    session.select_candidate_index(1);

    let segments = session.segments.as_ref().unwrap();
    let selected_range = {
        let range = segments.focused().unwrap().reading_range();
        (range.start.get(), range.end.get())
    };
    let expected = segments
        .focused()
        .unwrap()
        .focused()
        .paths()
        .first()
        .into_iter()
        .flat_map(|path| path.token_keys())
        .filter_map(|key| match key {
            TokenKey::Dictionary(token_id) => Some(token_id.get()),
            TokenKey::Unknown { .. } | TokenKey::Overlay { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    let unmarked = segments
        .segments()
        .iter()
        .filter(|segment| {
            let range = segment.reading_range();
            (range.start.get(), range.end.get()) != selected_range
        })
        .flat_map(|segment| segment.focused().paths().first())
        .flat_map(|path| path.token_keys())
        .filter_map(|key| match key {
            TokenKey::Dictionary(token_id) => Some(token_id.get()),
            TokenKey::Unknown { .. } | TokenKey::Overlay { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(!expected.is_empty());
    assert!(!unmarked.is_empty());

    session.handle(key("\r", 36));
    let actual = learning_snapshot(&session)
        .expect("changed segment publishes a frequency snapshot")
        .entries
        .into_iter()
        .map(|entry| entry.dictionary_token_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    assert!(actual.is_disjoint(&unmarked));
}

// A correction committed by typing on (plain or Shift letter) is learned,
// and that observation survives the next conversion.
#[test]
fn candidate_correction_learns_at_printable_and_shift_literal_commit_boundaries() {
    const SHIFT_MODIFIER: u64 = 1 << 17;

    for (next, keycode, modifiers, expected_preedit) in
        [("n", 45, 0, "n"), ("M", 46, SHIFT_MODIFIER, "M")]
    {
        let dictionary = conversion_dictionary();
        let mut session =
            session_with_dictionary_path(Some(dictionary.path())).expect("session data");
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        session.handle(key(" ", 49));
        session.select_candidate_index(1);
        let selected_tokens = selected_dictionary_tokens(&session);

        let restarted = session.handle(key_with_modifiers(next, keycode, modifiers));
        assert_eq!(restarted.commit_text.as_deref(), Some("京"));
        assert_eq!(restarted.preedit[0].text, expected_preedit);
        assert_frequency_tokens(&session, &selected_tokens);

        let cancelled = session.handle(key("\u{1b}", 53));
        if modifiers == SHIFT_MODIFIER {
            assert_eq!(cancelled.preedit[0].text, "M");
            assert!(cancelled.commit_text.is_none());
            let cancelled = session.handle(key("\u{1b}", 53));
            assert!(cancelled.preedit.is_empty());
        } else {
            assert!(cancelled.preedit.is_empty());
        }
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        session.handle(key(" ", 49));
        // Frequency learning preserves token observations, rather than an
        // exact-context override. One observation need not change ①.
        assert_frequency_tokens(&session, &selected_tokens);
    }
}

// A cancelled selection, and a commit of the untouched first choice, learn
// nothing: no snapshot is produced.
#[test]
fn uncommitted_or_implicit_baseline_paths_do_not_learn_frequency() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    session.select_candidate_index(1);
    session.handle(key("\u{1b}", 53));
    assert!(learning_snapshot(&session).is_none());

    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    session.handle(key("\r", 36));
    assert!(learning_snapshot(&session).is_none());
}

// Selecting by candidate id, and moving focus to another segment before
// committing, still learn the chosen tokens.
#[test]
fn direct_candidate_id_and_cross_segment_focus_changes_remain_learnable() {
    let dictionary = conversion_dictionary();
    let mut direct = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "kyou".chars() {
        direct.handle(key(&character.to_string(), 0));
    }
    let converted = direct.handle(key(" ", 49));
    let candidate_id = converted.candidates.unwrap().items[1].id.clone();
    direct.handle(command(
        "select_candidate",
        serde_json::json!({"id": candidate_id}),
    ));
    let direct_tokens = selected_dictionary_tokens(&direct);
    direct.handle(key("\r", 36));
    assert_frequency_tokens(&direct, &direct_tokens);

    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;
    let multi_dictionary = multi_segment_dictionary();
    let mut multi =
        session_with_dictionary_path(Some(multi_dictionary.path())).expect("session data");
    for character in "aiueoka".chars() {
        multi.handle(key(&character.to_string(), 0));
    }
    multi.handle(key(" ", 49));
    multi.select_candidate_index(1);
    let multi_tokens = selected_dictionary_tokens(&multi);
    multi.handle(key_with_modifiers(
        "\u{f703}",
        124,
        NUMERIC_PAD_MODIFIER | FUNCTION_MODIFIER,
    ));
    multi.handle(key("\r", 36));
    assert_frequency_tokens(&multi, &multi_tokens);

    for character in "aiueoka".chars() {
        multi.handle(key(&character.to_string(), 0));
    }
    multi.handle(key(" ", 49));
    assert_frequency_tokens(&multi, &multi_tokens);
}

// Replacing the first candidate records the chosen tokens as positive and
// exactly the initial tokens it displaced as rejected, once each; clearing
// empties both lists.
#[test]
fn explicit_commit_rejects_only_replaced_initial_nodes_with_identical_ranges() {
    let file = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(file.path())).unwrap();
    for ch in "kyou".chars() {
        session.handle(key(&ch.to_string(), 0));
    }
    session.convert();
    let tokens = |s: &muu_conversion::Segment| {
        s.focused().paths()[0]
            .token_keys()
            .filter_map(|k| match k {
                TokenKey::Dictionary(id) => Some(*id),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
    };
    let initial = tokens(&session.segments.as_ref().unwrap().segments()[0]);
    session.select_candidate_index(1);
    let final_tokens = tokens(&session.segments.as_ref().unwrap().segments()[0]);
    let expected_rejected = initial
        .difference(&final_tokens)
        .copied()
        .collect::<BTreeSet<_>>();
    assert!(!expected_rejected.is_empty());
    session.commit();
    let learned = learning_snapshot(&session).unwrap();
    assert_eq!(learned.observation_count, 1);
    assert_eq!(
        learned
            .entries
            .iter()
            .map(|e| muu_dict::DictionaryTokenId::new(e.dictionary_token_id))
            .collect::<BTreeSet<_>>(),
        final_tokens
    );
    assert_eq!(
        learned
            .rejected_entries
            .iter()
            .map(|e| muu_dict::DictionaryTokenId::new(e.dictionary_token_id))
            .collect::<BTreeSet<_>>(),
        expected_rejected
    );
    assert!(learned.rejected_entries.iter().all(|e| e.count == 1));
    session.core.clear_personal_frequency();
    let cleared = learning_snapshot(&session).unwrap();
    assert!(cleared.entries.is_empty());
    assert!(cleared.rejected_entries.is_empty());
}

// After a resize the segment has no initial nodes of that range, so a
// choice there learns positives but rejects nothing.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn shrinking_changes_the_range_and_does_not_invent_a_rejection_correspondence() {
    let Some(path) = real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let mut session = new_session(Some(&path), None, Default::default()).unwrap();
    for ch in "henkanseido".chars() {
        session.handle(key(&ch.to_string(), 0));
    }
    session.convert();
    let old = session.segments.as_ref().unwrap().segments()[0].reading_range();
    let resized = session.resize_segment(-1);
    assert!(resized.error.is_none());
    let new = session.segments.as_ref().unwrap().segments()[0].reading_range();
    assert_ne!(old, new);
    assert!(
        !session
            .initial_selection_nodes
            .contains_key(&(new.start.get(), new.end.get()))
    );
    assert!(
        session.segments.as_ref().unwrap().segments()[0]
            .candidates()
            .len()
            > 1
    );
    session.select_candidate_index(1);
    session.commit();
    let learned = learning_snapshot(&session).unwrap();
    assert!(!learned.entries.is_empty());
    assert!(learned.rejected_entries.is_empty());
}

// The report keeps the raw model scores bit for bit, subtracts the learning
// bias to get adjusted scores, and derives the margin from them; the bias
// comes from the conversion's captured learning and survives a later clear.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn learning_report_keeps_model_scores_and_the_conversion_captured_counts() {
    let Some(core) = run_selection_core() else {
        return;
    };
    let captured = core.conversion_snapshot();
    let generated = generate_conversion("くも", &captured, &core.segmenter).unwrap();
    let c = generated.segments().segments()[0]
        .candidates()
        .iter()
        .find(|c| c.surface() == "蜘蛛")
        .unwrap();
    let ids = c.paths()[0]
        .token_keys()
        .filter_map(|k| match k {
            TokenKey::Dictionary(id) => Some(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    for _ in 0..3 {
        core.observe_selection(captured.learning.generation, &ids);
    }
    let mut session = core.create_session().unwrap();
    for ch in "kumo".chars() {
        session.handle(key(&ch.to_string(), 0));
    }
    session.convert();
    let report = session.last_selection_report.as_ref().unwrap();
    assert_eq!(report.format_version, 3);
    assert_eq!(report.rules_version, 3);
    let run = &report.runs[0];
    assert!(run.skipped.is_none());
    let refs = run
        .hypotheses
        .iter()
        .map(|h| h.surface.as_str())
        .collect::<Vec<_>>();
    let raw = core
        .language_model()
        .model
        .as_ref()
        .unwrap()
        .score("", "くも", &refs)
        .unwrap();
    assert!(run.hypotheses.iter().any(|h| h.learning_bias > 0.));
    for (h, score) in run.hypotheses.iter().zip(raw) {
        assert_eq!(h.score.to_bits(), score.to_bits());
        assert_eq!(h.adjusted_score, h.score - h.learning_bias);
    }
    let best = run
        .hypotheses
        .iter()
        .map(|h| h.adjusted_score)
        .filter(|s| s.is_finite())
        .min_by(f32::total_cmp)
        .unwrap();
    assert_eq!(
        run.baseline_minus_best_score,
        Some(run.hypotheses[run.baseline_rank.unwrap()].adjusted_score - best)
    );
    let held = session.conversion_snapshot.as_ref().unwrap().clone();
    let learned_h = &run
        .hypotheses
        .iter()
        .find(|h| h.learning_bias > 0.)
        .unwrap()
        .hypothesis;
    let range = run.reading_range;
    core.clear_personal_frequency();
    assert!(held.learning.frequency.selection_bias(learned_h, range) > 0.);
    assert_eq!(
        core.conversion_snapshot()
            .learning
            .frequency
            .selection_bias(learned_h, range),
        0.
    );
}

// Replacing a word rejects that word's node only; an unchanged particle in
// the same segment is not rejected.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn exact_node_replacements_reject_the_word_but_not_unchanged_particles() {
    let Some(path) = real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    for (roman, before, after, replaced_surface) in [
        ("saitei", "最低", "裁定", "最低"),
        ("seidoga", "制度が", "精度が", "制度"),
    ] {
        let mut session = new_session(Some(&path), None, Default::default()).unwrap();
        for ch in roman.chars() {
            session.handle(key(&ch.to_string(), 0));
        }
        session.convert();
        let original = &session.segments.as_ref().unwrap().segments()[0];
        assert_eq!(original.focused().surface(), before);
        let expected = original.focused().paths()[0]
            .nodes
            .iter()
            .filter(|node| node.surface == replaced_surface)
            .filter_map(|node| match node.token_key {
                TokenKey::Dictionary(id) => Some(id.get()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(expected.len(), 1);
        let index = original
            .candidates()
            .iter()
            .position(|c| c.surface() == after)
            .unwrap();
        session.select_candidate_index(index);
        session.commit();
        let learned = learning_snapshot(&session).unwrap();
        assert_eq!(learned.observation_count, 1);
        assert_eq!(
            learned
                .rejected_entries
                .iter()
                .map(|e| e.dictionary_token_id)
                .collect::<BTreeSet<_>>(),
            expected
        );
        assert!(learned.rejected_entries.iter().all(|e| e.count == 1));
    }
}

// Choosing one word that spans an earlier word+particle pair (今日は → 教派)
// rejects neither original node, however often it repeats.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn merging_a_word_and_particle_does_not_reject_either_original_node() {
    let Some(path) = real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let mut session = new_session(Some(&path), None, Default::default()).unwrap();
    for iteration in 0..4 {
        for ch in "kyouha".chars() {
            session.handle(key(&ch.to_string(), 0));
        }
        session.convert();
        let original = &session.segments.as_ref().unwrap().segments()[0];
        if iteration == 0 {
            assert_eq!(original.focused().surface(), "今日は");
            assert_eq!(original.focused().paths()[0].nodes.len(), 2);
            assert_eq!(original.focused().paths()[0].nodes[1].surface, "は");
        }
        let index = original
            .candidates()
            .iter()
            .position(|c| c.surface() == "教派")
            .unwrap();
        let committed = &original.candidates()[index];
        assert_eq!(committed.paths()[0].nodes.len(), 1);
        session.select_candidate_index(index);
        session.commit();
        let learned = learning_snapshot(&session).unwrap();
        assert!(learned.rejected_entries.is_empty());
        assert!(!learned.entries.is_empty());
    }
}

// Selecting another candidate and then the original again commits the
// original as an explicit choice: it is learned, not rejected.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn returning_to_the_same_token_in_the_same_range_does_not_reject_it() {
    let Some(path) = real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let mut session = new_session(Some(&path), None, Default::default()).unwrap();
    for ch in "saitei".chars() {
        session.handle(key(&ch.to_string(), 0));
    }
    session.convert();
    let original = &session.segments.as_ref().unwrap().segments()[0];
    let initial_index = original.focused_candidate();
    let other = original
        .candidates()
        .iter()
        .position(|c| c.id() != original.focused().id())
        .unwrap();
    session.select_candidate_index(other);
    session.select_candidate_index(initial_index);
    session.commit();
    let learned = learning_snapshot(&session).unwrap();
    assert_eq!(learned.observation_count, 1);
    assert!(!learned.entries.is_empty());
    assert!(learned.rejected_entries.is_empty());
}
