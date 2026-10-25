use muu_conversion::CostSnapshot;

use muu_conversion::convert_to_segments;

use crate::session::*;
use crate::test_support::*;
use std::path::Path;

// Numeric readings offer the generated circled (1-50) and Roman (1-12)
// variants, never as the top candidate, and nothing past those ranges: a
// table that grew, or one that outranked the dictionary's number, fails.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn production_numeric_variant_table_exposes_circled_and_roman_candidates() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let dictionary = crate::real_dictionary::shared_at(&dictionary_path);
    let snapshot = CostSnapshot::baseline(&dictionary);
    let candidates_for = |reading: &str| {
        convert_to_segments(reading, &default_segmenter(), &snapshot)
            .expect("numeric conversion")
            .segments()
            .iter()
            .flat_map(|segment| {
                segment
                    .candidates()
                    .iter()
                    .map(|candidate| candidate.surface().to_owned())
            })
            .collect::<Vec<_>>()
    };

    for (reading, expected) in [
        ("1", "①"),
        ("2", "②"),
        ("10", "⑩"),
        ("20", "⑳"),
        ("21", "㉑"),
        ("35", "㉟"),
        ("36", "㊱"),
        ("50", "㊿"),
    ] {
        let candidates = candidates_for(reading);
        assert!(
            candidates.iter().any(|surface| surface == expected),
            "circled variant missing for {reading}"
        );
        assert_ne!(
            candidates.first().map(String::as_str),
            Some(expected),
            "generated variant must not displace the native top candidate"
        );
    }
    for (reading, upper, lower) in [
        ("1", "Ⅰ", "ⅰ"),
        ("4", "Ⅳ", "ⅳ"),
        ("9", "Ⅸ", "ⅸ"),
        ("12", "Ⅻ", "ⅻ"),
    ] {
        let candidates = candidates_for(reading);
        assert!(candidates.iter().any(|surface| surface == upper));
        assert!(candidates.iter().any(|surface| surface == lower));
    }
    let roman_13 = candidates_for("13");
    assert!(
        !roman_13
            .iter()
            .any(|surface| surface == "ⅩⅢ" || surface == "ⅹⅲ")
    );
    let circled_51 = candidates_for("51");
    assert!(!circled_51.iter().any(|surface| {
        surface
            .chars()
            .any(|ch| matches!(ch as u32, 0x3251..=0x325F | 0x32B1..=0x32BF))
    }));
}

// f+vowel and fyu compose to ふぁ… in the preedit, commit as kana and convert
// to katakana; a romaji table that left "fあ" fails at each boundary.
#[test]
fn foreign_f_sounds_reach_session_commit_and_conversion_boundaries() {
    for (input, expected) in [
        ("fa", "ふぁ"),
        ("fi", "ふぃ"),
        ("fe", "ふぇ"),
        ("fo", "ふぉ"),
        ("fyu", "ふゅ"),
    ] {
        let mut session =
            new_session(None, None, TestSessionConfig::default()).expect("embedded table");
        let mut view = empty_view(false);
        for character in input.chars() {
            view = session.handle(key(&character.to_string(), 0));
        }
        assert_eq!(view.preedit[0].text, expected, "{input}");
        assert_ne!(view.preedit[0].text, "fあ", "{input}");
    }

    let mut commit_session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    commit_session.handle(key("f", 3));
    commit_session.handle(key("a", 0));
    let committed = commit_session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("ふぁ"));
    assert!(committed.preedit.is_empty());

    let dictionary = conversion_dictionary();
    let mut conversion_session =
        session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    conversion_session.handle(key("f", 3));
    conversion_session.handle(key("a", 0));
    let converted = conversion_session.handle(key(" ", 49));
    assert!(converted.consumed);
    assert_eq!(converted.preedit[0].text, "ファ");
    assert!(converted.preedit[0].is_converted);
    assert_eq!(converted.candidates.as_ref().unwrap().items[0].text, "ファ");
}

// "je" becomes じぇ at commit and ジェ after conversion, with no Latin left.
#[test]
fn je_reaches_commit_and_conversion_boundaries_without_literal_leakage() {
    let mut commit_session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    commit_session.handle(key("j", 0));
    commit_session.handle(key("e", 0));
    let committed = commit_session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("じぇ"));
    assert!(committed.preedit.is_empty());

    let dictionary = conversion_dictionary();
    let mut conversion_session =
        session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    conversion_session.handle(key("j", 0));
    conversion_session.handle(key("e", 0));
    let converted = conversion_session.handle(key(" ", 49));
    assert!(converted.consumed);
    assert_eq!(converted.preedit[0].text, "ジェ");
    assert!(converted.preedit[0].is_converted);
    assert_eq!(converted.candidates.as_ref().unwrap().items[0].text, "ジェ");
}

// While converted, the first Delete returns to the whole unconverted reading
// (same caret, no candidates, nothing committed); only the second deletes a
// character. Passing Delete to the client, or editing at once, fails.
#[test]
fn converted_backspace_restores_full_reading_before_editing() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let reading = session.current_view(false);
    assert_eq!(reading.preedit[0].text, "きょう");

    let converted = session.handle(key(" ", 49));
    assert!(converted.preedit.iter().all(|run| run.is_converted));
    assert!(converted.candidates.is_some());

    let selected = session.handle(key("\u{f701}", 125));
    assert!(selected.candidates.is_some());

    let restored = session.handle(key("", 51));
    assert!(restored.consumed);
    assert!(restored.commit_text.is_none());
    assert_eq!(restored.preedit[0].text, reading.preedit[0].text);
    assert!(restored.preedit.iter().all(|run| !run.is_converted));
    assert_eq!(restored.caret_utf16, reading.caret_utf16);
    assert!(restored.selection.is_none());
    assert!(restored.candidates.is_none());

    let deleted = session.handle(key("", 51));
    assert!(deleted.consumed);
    assert!(deleted.commit_text.is_none());
    assert_eq!(deleted.preedit[0].text, "きょ");
    assert_eq!(deleted.caret_utf16, 2);
}

// Left/Right move the caret inside an unconverted preedit, typing inserts at
// the caret, and Delete or Left at the start are consumed no-ops. Any of
// these committing or reaching the client fails.
#[test]
fn unconverted_arrows_edit_preedit_without_committing() {
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;
    const NAVIGATION_MODIFIERS: u64 = FUNCTION_MODIFIER | NUMERIC_PAD_MODIFIER;

    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    for character in "kakiku".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.current_view(false).preedit[0].text, "かきく");
    assert_eq!(session.current_view(false).caret_utf16, 3);

    for _ in 0..2 {
        let moved = session.handle(key_with_modifiers("\u{f702}", 123, NAVIGATION_MODIFIERS));
        assert!(moved.consumed);
        assert!(moved.commit_text.is_none());
    }
    assert_eq!(session.current_view(false).caret_utf16, 1);

    session.handle(key("s", 1));
    let pending = session.current_view(false);
    assert_eq!(pending.preedit[0].text, "かsきく");
    assert_eq!(pending.caret_utf16, 2);
    session.handle(key("a", 0));
    let inserted = session.current_view(false);
    assert_eq!(inserted.preedit[0].text, "かさきく");
    assert_eq!(inserted.caret_utf16, 2);
    assert!(inserted.commit_text.is_none());

    let deleted = session.handle(key("", 51));
    assert_eq!(deleted.preedit[0].text, "かきく");
    assert_eq!(deleted.caret_utf16, 1);
    assert!(deleted.commit_text.is_none());

    let left = session.handle(key_with_modifiers("\u{f702}", 123, NAVIGATION_MODIFIERS));
    assert_eq!(left.caret_utf16, 0);
    let stopped = session.handle(key_with_modifiers("\u{f702}", 123, NAVIGATION_MODIFIERS));
    assert!(stopped.consumed);
    assert_eq!(stopped.caret_utf16, 0);
    assert_eq!(stopped.preedit[0].text, "かきく");

    let delete_at_start = session.handle(key("", 51));
    assert!(delete_at_start.consumed);
    assert!(delete_at_start.commit_text.is_none());
    assert_eq!(delete_at_start.caret_utf16, 0);
    assert_eq!(delete_at_start.preedit[0].text, "かきく");
}

// An unbound shortcut during conversion is swallowed and changes nothing: the
// encoded view stays byte-identical. Passing it through would let the client
// act next to a live composition.
#[test]
fn converted_composition_absorbs_unbound_shortcuts_without_mutation() {
    const CONTROL_MODIFIER: u64 = 1 << 18;
    const OPTION_MODIFIER: u64 = 1 << 19;
    const COMMAND_MODIFIER: u64 = 1 << 20;
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;

    let dictionary = conversion_dictionary();
    for (characters, keycode, modifiers) in [
        (
            "\u{f703}",
            124,
            COMMAND_MODIFIER | NUMERIC_PAD_MODIFIER | FUNCTION_MODIFIER,
        ),
        ("c", 8, COMMAND_MODIFIER),
        ("f", 3, CONTROL_MODIFIER),
        ("x", 7, OPTION_MODIFIER),
        ("\u{f704}", 122, FUNCTION_MODIFIER),
    ] {
        let mut session =
            session_with_dictionary_path(Some(dictionary.path())).expect("session data");
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        session.handle(key(" ", 49));
        let selected = session.select_candidate_index(1);
        assert_eq!(selected.candidates.as_ref().unwrap().selected_index, 1);
        let selected_bytes = encode_view(&selected).unwrap();

        let blocked = session.handle(key_with_modifiers(characters, keycode, modifiers));
        assert!(blocked.consumed);
        assert!(blocked.commit_text.is_none());
        assert_eq!(encode_view(&blocked).unwrap(), selected_bytes);
    }
}

// ASCII punctuation enters the preedit in its kana form, converts with that
// form first and the ASCII character second, and commits the kana form; in
// a literal span it stays ASCII.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn kana_punctuation_stays_in_preedit_and_offers_ascii_as_second_candidate() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let mut session =
        session_with_dictionary_path(Some(&dictionary_path)).expect("production dictionary");
    for (input, canonical, ascii) in [
        ("!", "！", "!"),
        ("\"", "＂", "\""),
        ("#", "＃", "#"),
        ("$", "＄", "$"),
        ("%", "％", "%"),
        ("&", "＆", "&"),
        ("'", "＇", "'"),
        ("(", "（", "("),
        (")", "）", ")"),
        ("*", "＊", "*"),
        ("+", "＋", "+"),
        (",", "、", ","),
        ("-", "ー", "-"),
        (".", "。", "."),
        ("/", "・", "/"),
        (":", "：", ":"),
        (";", "；", ";"),
        ("<", "＜", "<"),
        ("=", "＝", "="),
        (">", "＞", ">"),
        ("?", "？", "?"),
        ("@", "＠", "@"),
        ("[", "「", "["),
        ("\\", "＼", "\\"),
        ("]", "」", "]"),
        ("^", "＾", "^"),
        ("_", "＿", "_"),
        ("`", "｀", "`"),
        ("{", "｛", "{"),
        ("|", "｜", "|"),
        ("}", "｝", "}"),
        ("~", "～", "~"),
    ] {
        let typed = session.handle(key(input, 0));
        assert_eq!(typed.preedit[0].text, canonical, "{input}");
        assert!(typed.commit_text.is_none(), "{input}");

        let converted = session.handle(key(" ", 49));
        let candidates = converted.candidates.expect("punctuation candidates");
        assert_eq!(candidates.items[0].text, canonical, "{input}");
        assert_eq!(candidates.items[1].text, ascii, "{input}");

        let committed = session.handle(key("\r", 36));
        assert_eq!(committed.commit_text.as_deref(), Some(canonical), "{input}");
        assert!(committed.preedit.is_empty(), "{input}");
    }

    let mut numeric =
        session_with_dictionary_path(Some(&dictionary_path)).expect("production dictionary");
    numeric.handle(key("1", 0));
    let punctuated = numeric.handle(key("!", 0));
    assert_eq!(punctuated.preedit[0].text, "1！");
    assert!(punctuated.commit_text.is_none());

    let mut literal =
        session_with_dictionary_path(Some(&dictionary_path)).expect("production dictionary");
    literal.handle(flags_changed(LEFT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    literal.handle(flags_changed(LEFT_SHIFT_KEYCODE, 0));
    let literal_punctuation = literal.handle(key("!", 0));
    assert_eq!(literal_punctuation.preedit[0].text, "!");
    assert!(literal_punctuation.commit_text.is_none());
}

// The same input converts differently only when a model directory is set:
// without one the dictionary choice stays, with edge the LM picks 精度.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn conversion_lm_replaces_a_homophone_only_when_configured() {
    let (Some(dictionary_path), Some(lm_dir)) = (
        crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
        crate::real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
    ) else {
        return;
    };
    let convert = |conversion_lm_dir: Option<&Path>| {
        let mut session = new_session(
            Some(&dictionary_path),
            conversion_lm_dir,
            TestSessionConfig {
                ..TestSessionConfig::default()
            },
        )
        .expect("production dictionary");
        for character in "henkanseidogawarui".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        let converted = session.handle(key(" ", 49));
        converted
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
    };
    assert_eq!(convert(None), "変換制度が悪い");
    assert_eq!(convert(Some(&lm_dir)), "変換精度が悪い");
}

// An LM choice that changes segment boundaries is applied with its exact
// scored path, and moving focus afterwards changes neither the segments nor
// the learning marks. The record keeps the generation's boundaries apart
// from the selected ones.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn full_lattice_selection_resegments_and_navigation_preserves_the_applied_paths() {
    let (Some(dictionary_path), Some(model)) = (
        real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
        real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
    ) else {
        return;
    };
    let mut session = new_session(
        Some(&dictionary_path),
        Some(&model),
        TestSessionConfig {
            commit_stream_config: Some(test_commit_stream_config("boundary-change")),
            ..Default::default()
        },
    )
    .unwrap();
    // Public AJIMEE id 1129 supplies a real scored boundary change. It is
    // a path/navigation fixture, not a claim that this surface is gold.
    session
        .preceding_commit_context
        .observe("に集金を行う事を「朝駆け」、逆に深夜に集金を行うことを「");
    session.composer.input_literal("よがけ");
    session.convert();
    let chosen = session.segments.as_ref().unwrap().clone();
    let ranges = chosen
        .segments()
        .iter()
        .map(|s| (s.reading_range().start.get(), s.reading_range().end.get()))
        .collect::<Vec<_>>();
    assert_eq!(ranges, [(0, 1), (1, 3)]);
    assert_eq!(
        chosen
            .segments()
            .iter()
            .map(|s| s.focused().surface())
            .collect::<String>(),
        "夜がけ"
    );
    let report = session.last_selection_report.clone().unwrap();
    assert_eq!(report.format_version, 3);
    assert!(report.runs[0].switched);
    let expected = &report.runs[0].hypotheses[report.runs[0].selected_rank.unwrap()]
        .hypothesis
        .nodes;
    assert_eq!(
        chosen
            .segments()
            .iter()
            .flat_map(|s| s.focused().paths()[0].nodes.clone())
            .collect::<Vec<_>>(),
        *expected
    );
    session.handle(key("", 124));
    session.handle(command(
        "move_segment",
        serde_json::json!({"direction":"left"}),
    ));
    assert_eq!(session.segments.as_ref().unwrap(), &chosen);
    assert!(session.explicitly_selected_segment_ranges.is_empty());
    let committed = session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(
        event["initial"]["generation_segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (
                s["reading_range"]["start"].as_u64().unwrap(),
                s["reading_range"]["end"].as_u64().unwrap()
            ))
            .collect::<Vec<_>>(),
        [(0, 2), (2, 3)]
    );
    assert_eq!(
        event["initial"]["selected_segments"],
        event["final"]["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let mut s = s.clone();
                s.as_object_mut().unwrap().remove("explicitly_selected");
                s
            })
            .collect::<serde_json::Value>()
    );
    assert_eq!(event["final"]["surface"], committed.commit_text.unwrap());
    assert_eq!(event["final"]["segment_moved"], true);
    assert_eq!(event["final"]["segment_resized"], false);
}

// Typing in half alnum while converted commits the conversion plus the
// character in one commit and leaves no composition or segments behind.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn alnum_input_commits_converted_segments_without_leaving_ghost_state() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let mut session =
        session_with_dictionary_path(Some(&dictionary_path)).expect("production dictionary");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let switched = session.handle(command(
        "set_mode",
        serde_json::json!({"mode": "half_alnum"}),
    ));
    assert_eq!(switched.preedit[0].text, "きょう");
    assert!(switched.commit_text.is_none());

    let converted = session.handle(key(" ", 49));
    assert!(converted.segments.is_some());
    let selected = session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|segment| segment.focused().surface())
        .collect::<String>();
    assert_eq!(selected, "今日");

    let typed = session.handle(key("x", 7));
    assert_eq!(typed.commit_text.as_deref(), Some("今日x"));
    assert!(typed.preedit.is_empty());
    assert!(typed.segments.is_none());
    assert!(session.composer.is_empty());
    assert!(session.segments.is_none());

    let entered = session.handle(key("\r", 36));
    assert!(entered.commit_text.is_none());
    let hiragana = session.handle(command("set_mode", serde_json::json!({"mode": "hiragana"})));
    assert!(hiragana.commit_text.is_none());
    assert!(hiragana.preedit.is_empty());
}

// A Shift tap toggles nothing while converted (Shift+arrow resizes instead)
// or outside Hiragana, and its release is not consumed.
#[test]
fn shift_tap_is_a_noop_during_conversion_or_outside_hiragana() {
    let dictionary = conversion_dictionary();
    let mut converted =
        session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "kyou".chars() {
        converted.handle(key(&character.to_string(), 0));
    }
    converted.handle(key(" ", 49));
    converted.handle(flags_changed(LEFT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    let resized = converted.handle(key_with_modifiers(
        "\u{f703}",
        124,
        SHIFT_MODIFIER | NUMERIC_PAD_MODIFIER | FUNCTION_MODIFIER,
    ));
    assert!(resized.segments.is_some());
    assert!(resized.consumed);
    let converted_release = converted.handle(flags_changed(LEFT_SHIFT_KEYCODE, 0));
    assert!(converted_release.segments.is_some());
    assert!(!converted_release.consumed);
    assert!(converted_release.composition_mode_transition.is_none());

    let mut alnum = new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    alnum.handle(command(
        "set_mode",
        serde_json::json!({"mode": "half_alnum"}),
    ));
    alnum.handle(flags_changed(LEFT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    let alnum_release = alnum.handle(flags_changed(LEFT_SHIFT_KEYCODE, 0));
    assert!(!alnum_release.consumed);
    assert!(alnum_release.composition_mode_transition.is_none());
}

// Down moves the selection in Rust's view, and Return commits that
// selection, not the first candidate.
#[test]
fn candidate_navigation_stays_in_rust_and_return_commits_rust_selection() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }

    let converted = session.handle(key(" ", 49));
    assert!(converted.consumed);
    assert_eq!(converted.preedit[0].text, "今日");
    assert!(converted.preedit[0].is_converted);
    assert_eq!(converted.candidates.as_ref().unwrap().items[0].text, "今日");
    assert_eq!(converted.candidates.as_ref().unwrap().selected_index, 0);

    let selected = session.handle(key("\u{f701}", 125));
    assert!(selected.consumed);
    assert_eq!(selected.candidates.as_ref().unwrap().selected_index, 1);
    assert_eq!(selected.preedit[0].text, "京");

    let committed = session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("京"));
    assert!(committed.preedit.is_empty());
}

// A commit becomes one preceding-context unit, and that context does not
// change the dictionary-only candidates of the next conversion.
#[test]
fn converted_commits_are_context_units_without_changing_candidates() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let first = session.handle(key(" ", 49));
    let committed = session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("今日"));
    assert_eq!(session.preceding_commit_context.snapshot(), ["今日"]);

    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let second = session.handle(key(" ", 49));
    assert_eq!(second.candidates, first.candidates);
    assert_eq!(session.preceding_commit_context.snapshot(), ["今日"]);
}

// Long phrases convert without an error and to the expected text: the path
// budget must not cut them off.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn production_long_phrase_conversion_stays_within_complete_path_budget() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    for (input, expected) in [
        ("sonoaidanichousashite", "その間に調査して"),
        ("sonoaidanichousashitekureru", "その間に調査してくれる"),
        ("sonoaidanichousashitemorau", "その間に調査してもらう"),
    ] {
        let mut session = session_with_dictionary_path(Some(Path::new(&dictionary_path)))
            .expect("production dictionary");
        for character in input.chars() {
            session.handle(key(&character.to_string(), 0));
        }

        let converted = session.handle(key(" ", 49));
        assert_eq!(converted.error, None, "{input}");
        assert!(converted.segments.is_some(), "{input}: {converted:?}");
        assert_eq!(
            converted
                .preedit
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            expected,
            "{input}"
        );
    }
}

// Function-word chains that exceed the path-evidence budget still convert
// (resilient conversion) instead of failing.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn production_function_word_chain_converts_when_path_evidence_exceeds_its_budget() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    for (input, expected) in [
        ("mondaidehanaku", "問題ではなく"),
        ("mondaidehanakute", "問題ではなくて"),
    ] {
        let mut session = session_with_dictionary_path(Some(Path::new(&dictionary_path)))
            .expect("production dictionary");
        for character in input.chars() {
            session.handle(key(&character.to_string(), 0));
        }

        let converted = session.handle(key(" ", 49));
        assert_eq!(converted.error, None, "{input}");
        assert!(converted.segments.is_some(), "{input}: {converted:?}");
        assert_eq!(
            converted
                .preedit
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            expected,
            "{input}"
        );
    }
}

// Space/Shift-Space and Down/Up cycle candidates; Shift-Right/Left resize the
// focused segment one character at a time, always covering the reading
// contiguously; growing past the end reports invalid_segment_boundary with
// the lengths; Right moves focus.
#[test]
fn candidate_and_segment_key_events_update_rust_state() {
    const SHIFT_MODIFIER: u64 = 1 << 17;
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;
    const NAVIGATION_MODIFIERS: u64 = FUNCTION_MODIFIER | NUMERIC_PAD_MODIFIER;

    let dictionary = multi_segment_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "aiueoka".chars() {
        session.handle(key(&character.to_string(), 0));
    }

    let converted = session.handle(key(" ", 49));
    let converted_segments = converted.segments.as_ref().unwrap();
    assert_eq!(converted_segments.len(), 3);
    assert_eq!(
        converted_segments
            .iter()
            .map(|segment| segment.reading_range.len)
            .collect::<Vec<_>>(),
        vec![2, 1, 3]
    );

    assert!(converted.consumed);
    assert_eq!(converted.candidates.as_ref().unwrap().selected_index, 0);
    let next_by_space = session.handle(key(" ", 49));
    assert!(next_by_space.consumed);
    assert_eq!(next_by_space.candidates.as_ref().unwrap().selected_index, 1);
    assert_ne!(next_by_space.preedit, converted.preedit);

    let previous_by_shift_space = session.handle(key_with_modifiers(" ", 49, SHIFT_MODIFIER));
    assert!(previous_by_shift_space.consumed);
    assert_eq!(
        previous_by_shift_space
            .candidates
            .as_ref()
            .unwrap()
            .selected_index,
        0
    );
    assert_eq!(previous_by_shift_space.preedit, converted.preedit);

    let next_by_down = session.handle(key_with_modifiers("\u{f701}", 125, NAVIGATION_MODIFIERS));
    assert!(next_by_down.consumed);
    assert_eq!(next_by_down.candidates.as_ref().unwrap().selected_index, 1);

    let previous_by_up = session.handle(key_with_modifiers("\u{f700}", 126, NAVIGATION_MODIFIERS));
    assert!(previous_by_up.consumed);
    assert_eq!(
        previous_by_up.candidates.as_ref().unwrap().selected_index,
        0
    );

    let assert_layout = |view: &ViewState, expected: &[usize]| {
        let segments = view.segments.as_ref().unwrap();
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.reading_range.len)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.reading_range.len)
                .sum::<usize>(),
            6
        );
        assert!(segments.windows(2).all(|pair| {
            pair[0].reading_range.start + pair[0].reading_range.len == pair[1].reading_range.start
        }));
        assert_eq!(segments[0].reading_range.start, 0);
        assert_eq!(segments[0].reading_range.len, expected[0]);
        assert!(view.preedit[0].is_focused);
    };
    let resize = |session: &mut Session, keycode: u16| {
        session.handle(key_with_modifiers(
            if keycode == 124 {
                "\u{f703}"
            } else {
                "\u{f702}"
            },
            keycode,
            SHIFT_MODIFIER | NAVIGATION_MODIFIERS,
        ))
    };

    let extended_three = resize(&mut session, 124);
    assert!(extended_three.consumed);
    assert_layout(&extended_three, &[3, 3]);
    assert_eq!(
        extended_three.candidates.as_ref().unwrap().selected_index,
        0
    );
    assert_ne!(extended_three.preedit, converted.preedit);

    let extended_four = resize(&mut session, 124);
    assert_layout(&extended_four, &[4, 2]);
    assert_ne!(extended_four.preedit, extended_three.preedit);

    let extended_five = resize(&mut session, 124);
    assert_layout(&extended_five, &[5, 1]);
    assert_ne!(extended_five.preedit, extended_four.preedit);

    let extended_to_end = resize(&mut session, 124);
    assert_layout(&extended_to_end, &[6]);
    assert_ne!(extended_to_end.preedit, extended_five.preedit);

    let past_end = resize(&mut session, 124);
    assert_layout(&past_end, &[6]);
    assert_eq!(past_end.preedit, extended_to_end.preedit);
    assert_eq!(
        past_end.error.as_ref().map(|error| error.code.as_str()),
        Some("invalid_segment_boundary")
    );
    assert_eq!(
        past_end.error.as_ref().unwrap().params["requested_length"],
        7
    );
    assert_eq!(past_end.error.as_ref().unwrap().params["current_length"], 6);

    let shrunk_five = resize(&mut session, 123);
    assert_layout(&shrunk_five, &[5, 1]);
    assert_ne!(shrunk_five.preedit, past_end.preedit);

    let shrunk_four = resize(&mut session, 123);
    assert_layout(&shrunk_four, &[4, 2]);
    assert_ne!(shrunk_four.preedit, shrunk_five.preedit);

    let shrunk_three = resize(&mut session, 123);
    assert_layout(&shrunk_three, &[3, 3]);
    assert_ne!(shrunk_three.preedit, shrunk_four.preedit);

    let shrunk_two = resize(&mut session, 123);
    assert_layout(&shrunk_two, &[2, 4]);
    assert_ne!(shrunk_two.preedit, shrunk_three.preedit);

    let moved = session.handle(key_with_modifiers("\u{f703}", 124, NAVIGATION_MODIFIERS));
    assert!(moved.consumed);
    assert!(moved.preedit[1].is_focused);
    assert_eq!(moved.candidates.as_ref().unwrap().selected_index, 0);
    assert_eq!(
        moved
            .segments
            .as_ref()
            .unwrap()
            .iter()
            .map(|segment| segment.reading_range.len)
            .collect::<Vec<_>>(),
        vec![2, 4]
    );
}

// Return commits every segment's selected candidate in one commit, a
// changed middle segment included.
#[test]
fn return_commits_all_selected_multi_segment_candidates_at_once() {
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;
    const NAVIGATION_MODIFIERS: u64 = FUNCTION_MODIFIER | NUMERIC_PAD_MODIFIER;

    let dictionary = multi_segment_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "aiueoka".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let converted = session.handle(key(" ", 49));
    assert_eq!(
        converted
            .segments
            .as_ref()
            .unwrap()
            .iter()
            .map(|segment| segment.reading_range.len)
            .collect::<Vec<_>>(),
        [2, 1, 3]
    );

    let middle = session.handle(key_with_modifiers("\u{f703}", 124, NAVIGATION_MODIFIERS));
    assert!(middle.preedit[1].is_focused);
    let selected = session.select_candidate_index(1);
    assert_eq!(selected.candidates.as_ref().unwrap().selected_index, 1);
    assert_eq!(
        session.segments.as_ref().unwrap().segments()[1].focused_candidate(),
        1
    );

    let expected_commit = session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|segment| segment.focused().surface())
        .collect::<String>();
    let committed = session.handle(key("\r", 36));
    assert_eq!(
        committed.commit_text.as_deref(),
        Some(expected_commit.as_str())
    );
    assert!(committed.preedit.is_empty());
    assert!(committed.segments.is_none());
    assert!(session.segments.is_none());
}

// A printable key while converted commits the selection and starts a new
// composition with that key.
#[test]
fn printable_input_commits_selected_segments_before_starting_new_composition() {
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;
    const NAVIGATION_MODIFIERS: u64 = FUNCTION_MODIFIER | NUMERIC_PAD_MODIFIER;

    let dictionary = multi_segment_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "aiueoka".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    session.handle(key_with_modifiers("\u{f703}", 124, NAVIGATION_MODIFIERS));
    session.select_candidate_index(1);

    let expected_commit = session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|segment| segment.focused().surface())
        .collect::<String>();
    let restarted = session.handle(key("n", 0));
    assert!(restarted.consumed);
    assert_eq!(
        restarted.commit_text.as_deref(),
        Some(expected_commit.as_str())
    );
    assert_eq!(
        restarted
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "n"
    );
    assert!(restarted.segments.is_none());
    assert!(restarted.candidates.is_none());
    assert!(session.segments.is_none());
    assert_eq!(session.composer.preedit().display, "n");

    let continued = session.handle(key("a", 0));
    assert!(continued.consumed);
    assert!(continued.commit_text.is_none());
    assert_eq!(
        continued
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "な"
    );
}

// The force_commit command commits every segment at once.
#[test]
fn force_commit_still_commits_all_converted_segments_immediately() {
    let dictionary = multi_segment_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "aiueoka".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    let expected_commit = session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|segment| segment.focused().surface())
        .collect::<String>();

    let committed = session.handle(command("force_commit", serde_json::Value::Null));
    assert_eq!(
        committed.commit_text.as_deref(),
        Some(expected_commit.as_str())
    );
    assert!(committed.preedit.is_empty());
    assert!(session.segments.is_none());
}

// The same multi-segment commit, on the real dictionary.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn production_dictionary_return_commits_all_selected_segments_at_once() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let mut session =
        session_with_dictionary_path(Some(&dictionary_path)).expect("production dictionary");
    for character in "kyouhaiitenkidesu".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let converted = session.handle(key(" ", 49));
    let segment_count = converted.segments.as_ref().unwrap().len();
    assert!(segment_count > 1);
    let selected = session.select_candidate_index(1);
    assert_eq!(selected.candidates.as_ref().unwrap().selected_index, 1);
    assert_eq!(
        session
            .segments
            .as_ref()
            .unwrap()
            .focused()
            .unwrap()
            .focused_candidate(),
        1
    );
    let expected_commit = session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|segment| segment.focused().surface())
        .collect::<String>();

    let committed = session.handle(key("\r", 36));
    assert_eq!(
        committed.commit_text.as_deref(),
        Some(expected_commit.as_str())
    );
    assert!(committed.preedit.is_empty());
    assert!(committed.segments.is_none());
    assert!(session.segments.is_none());
}
