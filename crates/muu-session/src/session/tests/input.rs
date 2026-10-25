use muu_conversion::CostSnapshot;
use std::time::Instant;

use std::sync::Arc;

use muu_conversion::trace_conversion;
use muu_dict::UnknownClass;

use crate::session::*;
use crate::test_support::*;

// Romaji becomes kana in the view with UTF-16 length and caret, consumed.
#[test]
fn session_projects_romaji_to_schema_v1_view() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    let mut view = empty_view(false);
    for character in "kya".chars() {
        view = session.handle(key(&character.to_string(), 0));
    }

    assert_eq!(view.preedit[0].text, "きゃ");
    assert_eq!(view.preedit[0].utf16_len, 2);
    assert_eq!(view.caret_utf16, 2);
    assert!(view.consumed);
}

// Every shared compatibility romaji entry gives its output plus pending text
// through real key events, and the old "jえ" failure stays fixed.
#[test]
fn common_romaji_compatibility_reaches_the_session_key_event_seam() {
    for (input, expected_output, expected_next) in common_romaji_compatibility_entries() {
        let mut session =
            new_session(None, None, TestSessionConfig::default()).expect("embedded table");
        let mut view = empty_view(false);
        for character in input.chars() {
            view = session.handle(key(&character.to_string(), 0));
        }
        assert!(view.consumed, "{input}");
        let expected = format!("{expected_output}{expected_next}");
        assert_eq!(view.preedit[0].text, expected, "{input}");
    }

    let mut old_failure =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    old_failure.handle(key("j", 0));
    let fixed = old_failure.handle(key("e", 0));
    assert_eq!(fixed.preedit[0].text, "じぇ");
    assert_ne!(fixed.preedit[0].text, "jえ");
}

// Return turns a pending n into ん before committing; leaving "n" fails.
#[test]
fn return_flushes_n_and_commits_current_kana() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    session.handle(key("n", 45));
    let view = session.handle(key("\r", 36));

    assert_eq!(view.commit_text.as_deref(), Some("ん"));
    assert!(view.preedit.is_empty());
    assert_eq!(view.caret_utf16, 0);
    assert!(view.consumed);
}

// Delete removes one composer unit (っ survives "kka" minus one), and Escape
// cancels without committing.
#[test]
fn escape_cancels_and_backspace_uses_core_units() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    for character in "kka".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let backed = session.handle(key("", 51));
    assert_eq!(backed.preedit[0].text, "っ");

    let cancelled = session.handle(key("", 53));
    assert!(cancelled.preedit.is_empty());
    assert!(cancelled.commit_text.is_none());
    assert!(cancelled.consumed);
}

// Moving the caret settles pending romaji as typed text, so later input is
// inserted at the caret instead of completing the earlier syllable.
#[test]
fn moving_preedit_cursor_stabilizes_pending_romaji() {
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;
    const NAVIGATION_MODIFIERS: u64 = FUNCTION_MODIFIER | NUMERIC_PAD_MODIFIER;

    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    session.handle(key("k", 40));
    let moved = session.handle(key_with_modifiers("\u{f702}", 123, NAVIGATION_MODIFIERS));
    assert!(moved.consumed);
    assert_eq!(moved.preedit[0].text, "k");
    assert_eq!(moved.caret_utf16, 0);

    let inserted = session.handle(key("a", 0));
    assert_eq!(inserted.preedit[0].text, "あk");
    assert_eq!(inserted.caret_utf16, 1);
    assert!(inserted.commit_text.is_none());
}

// With nothing composed, shortcuts and function keys are not consumed and
// produce no text or state.
#[test]
fn shortcuts_and_non_text_keys_pass_through() {
    const CONTROL_MODIFIER: u64 = 1 << 18;
    const OPTION_MODIFIER: u64 = 1 << 19;
    const COMMAND_MODIFIER: u64 = 1 << 20;
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;

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
            new_session(None, None, TestSessionConfig::default()).expect("embedded table");
        let view = session.handle(key_with_modifiers(characters, keycode, modifiers));
        assert!(!view.consumed);
        assert!(view.commit_text.is_none());
        assert!(view.preedit.is_empty());
        assert!(view.segments.is_none());
        assert!(view.candidates.is_none());
    }
}

// While composing, an unbound shortcut is swallowed with a byte-identical
// view, and the pending romaji still completes afterwards.
#[test]
fn active_composer_absorbs_unbound_passthrough_without_mutation() {
    const CONTROL_MODIFIER: u64 = 1 << 18;
    const OPTION_MODIFIER: u64 = 1 << 19;
    const COMMAND_MODIFIER: u64 = 1 << 20;
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;

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
            new_session(None, None, TestSessionConfig::default()).expect("embedded table");
        let before = session.handle(key("k", 40));
        let before_bytes = encode_view(&before).unwrap();

        let blocked = session.handle(key_with_modifiers(characters, keycode, modifiers));
        assert!(blocked.consumed);
        assert!(blocked.commit_text.is_none());
        assert_eq!(encode_view(&blocked).unwrap(), before_bytes);

        let continued = session.handle(key("a", 0));
        assert_eq!(continued.preedit[0].text, "か");
        assert!(continued.commit_text.is_none());
    }
}

// Keys that do mean text elsewhere (non-ASCII text, Tab, a bare arrow)
// commit the composition and are then passed to the client.
#[test]
fn active_non_shortcut_passthrough_contracts_still_commit() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    session.handle(key("k", 0));
    session.handle(key("a", 0));
    let non_ascii = session.handle(key("é", 0));
    assert_eq!(non_ascii.commit_text.as_deref(), Some("か"));
    assert!(!non_ascii.consumed);

    session.handle(key("a", 0));
    let tab = session.handle(key("\t", 48));
    assert_eq!(tab.commit_text.as_deref(), Some("あ"));
    assert!(!tab.consumed);

    session.handle(key("i", 0));
    let explicit_arrow = session.handle(key("\u{f700}", 126));
    assert_eq!(explicit_arrow.commit_text.as_deref(), Some("い"));
    assert!(!explicit_arrow.consumed);
}

// Command-Space with nothing composed returns the unchanged view in every
// mode; with a composition or conversion it commits first and passes the
// key on, and the next input starts fresh.
#[test]
fn command_space_commits_active_composition_then_passes_through() {
    const COMMAND_MODIFIER: u64 = 1 << 20;

    for (mode_name, expected_mode) in [
        ("hiragana", InputMode::Hiragana),
        ("full_katakana", InputMode::FullKatakana),
        ("half_katakana", InputMode::HalfKatakana),
        ("full_alnum", InputMode::FullAlnum),
        ("half_alnum", InputMode::HalfAlnum),
    ] {
        let mut inactive_session =
            new_session(None, None, TestSessionConfig::default()).expect("embedded table");
        inactive_session.handle(command("set_mode", serde_json::json!({"mode": mode_name})));
        let expected = inactive_session.current_view(false);
        let inactive = inactive_session.handle(key_with_modifiers(" ", 49, COMMAND_MODIFIER));
        assert_eq!(inactive, expected);
        assert_eq!(inactive.mode, expected_mode);
        assert!(!inactive.consumed);
        assert!(inactive.commit_text.is_none());
    }

    let mut composer_session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    composer_session.handle(key("k", 40));
    composer_session.handle(key("a", 0));
    let composer_commit = composer_session.handle(key_with_modifiers(" ", 49, COMMAND_MODIFIER));
    assert!(!composer_commit.consumed);
    assert_eq!(composer_commit.commit_text.as_deref(), Some("か"));
    assert!(composer_commit.preedit.is_empty());
    assert!(composer_commit.segments.is_none());
    assert!(composer_commit.candidates.is_none());
    composer_session.handle(key("n", 45));
    let next_composition = composer_session.handle(key("a", 0));
    assert_eq!(next_composition.preedit[0].text, "な");
    assert!(next_composition.commit_text.is_none());

    let dictionary = multi_segment_dictionary();
    let mut converted_session =
        session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "aiueoka".chars() {
        converted_session.handle(key(&character.to_string(), 0));
    }
    converted_session.handle(key(" ", 49));
    let selected = converted_session.select_candidate_index(1);
    assert_eq!(selected.candidates.as_ref().unwrap().selected_index, 1);
    let expected_commit = converted_session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|segment| segment.focused().surface())
        .collect::<String>();

    let converted_commit = converted_session.handle(key_with_modifiers(" ", 49, COMMAND_MODIFIER));
    assert!(!converted_commit.consumed);
    assert_eq!(
        converted_commit.commit_text.as_deref(),
        Some(expected_commit.as_str())
    );
    assert!(converted_commit.preedit.is_empty());
    assert!(converted_commit.segments.is_none());
    assert!(converted_commit.candidates.is_none());
    assert!(converted_session.segments.is_none());
    converted_session.handle(key("k", 40));
    let next_converted_composition = converted_session.handle(key("a", 0));
    assert_eq!(next_converted_composition.preedit[0].text, "か");
    assert!(next_converted_composition.commit_text.is_none());
}

// Inside a Shift literal span, Command-Space only returns to reading input
// (consumed, nothing committed); a second press, or one after an explicit
// half-alnum switch, commits and passes through.
#[test]
fn command_space_restores_reading_without_committing_shift_literal_input() {
    const SHIFT_MODIFIER: u64 = 1 << 17;
    const COMMAND_MODIFIER: u64 = 1 << 20;

    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    session.handle(key_with_modifiers("M", 46, SHIFT_MODIFIER));
    session.handle(key("u", 32));
    session.handle(key("u", 32));

    let restored = session.handle(key_with_modifiers(" ", 49, COMMAND_MODIFIER));
    assert!(restored.consumed);
    assert!(restored.commit_text.is_none());
    assert_eq!(restored.preedit[0].text, "Muu");
    assert_eq!(restored.mode, InputMode::Hiragana);
    assert_eq!(
        restored.composition_mode_transition,
        Some(CompositionModeTransition::Reading)
    );

    session.handle(key("k", 40));
    let reading = session.handle(key("a", 0));
    assert_eq!(reading.preedit[0].text, "Muuか");
    assert!(reading.commit_text.is_none());

    let switched = session.handle(key_with_modifiers(" ", 49, COMMAND_MODIFIER));
    assert!(!switched.consumed);
    assert_eq!(switched.commit_text.as_deref(), Some("Muuか"));
    assert!(switched.preedit.is_empty());

    let mut explicit_alnum =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    for character in "koreha".chars() {
        explicit_alnum.handle(key(&character.to_string(), 0));
    }
    explicit_alnum.handle(command(
        "set_mode",
        serde_json::json!({"mode": "half_alnum"}),
    ));
    for character in "Muu".chars() {
        explicit_alnum.handle(key(&character.to_string(), 0));
    }
    let explicit_switch = explicit_alnum.handle(key_with_modifiers(" ", 49, COMMAND_MODIFIER));
    assert!(!explicit_switch.consumed);
    assert_eq!(explicit_switch.commit_text.as_deref(), Some("これはMuu"));
    assert!(explicit_switch.preedit.is_empty());
}

// Resizing one segment rebuilds that span only: the other run keeps its
// LM-chosen segments, and the original selection report is not redone.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn resizing_one_selected_span_preserves_the_other_runs_lm_choices() {
    let (Some(dictionary_path), Some(model)) = (
        real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
        real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
    ) else {
        return;
    };
    let mut session = new_session(
        Some(&dictionary_path),
        Some(&model),
        TestSessionConfig::default(),
    )
    .unwrap();
    session
        .composer
        .input_literal("へんかんせいどがわるい、へんかんせいどがわるい");
    session.convert();
    let original = session.segments.as_ref().unwrap().clone();
    let report = session.last_selection_report.clone().unwrap();
    assert_eq!(report.runs.len(), 2);
    assert!(
        report.runs[1].switched,
        "fixture must exercise a real LM choice outside the resized span"
    );
    let other_start = report.runs[1].reading_range.start.get();
    let other = original
        .segments()
        .iter()
        .filter(|s| s.reading_range().start.get() >= other_start)
        .cloned()
        .collect::<Vec<_>>();
    assert!(!other.is_empty());
    session.resize_segment(-1);
    let resized = session.segments.as_ref().unwrap();
    assert_ne!(
        resized.segments()[0].reading_range(),
        original.segments()[0].reading_range()
    );
    assert_eq!(
        resized
            .segments()
            .iter()
            .filter(|s| s.reading_range().start.get() >= other_start)
            .cloned()
            .collect::<Vec<_>>(),
        other
    );
    // Resizing runs stage ① only; the original selection report/draft stays
    // intact and the new boundary is represented in the final part (docs/src/internals/conversion.md「文節の移動と伸縮」, docs/src/internals/storage.md「記録」).
    assert_eq!(
        session.last_selection_report.as_ref().unwrap().runs[1].selected_rank,
        report.runs[1].selected_rank
    );
}

// Switching Hiragana to half alnum and back keeps one composition: Latin is
// appended literally, Delete edits it, a redundant switch changes nothing,
// and the whole mixed text commits once.
#[test]
fn public_mode_hotkey_keeps_literal_english_inside_japanese_composition() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    for character in "koreha".chars() {
        session.handle(key(&character.to_string(), 0));
    }

    let roman = session.handle(command(
        "set_mode",
        serde_json::json!({"mode": "half_alnum"}),
    ));
    assert_eq!(roman.preedit[0].text, "これは");
    assert_eq!(roman.mode, InputMode::HalfAlnum);
    assert!(roman.commit_text.is_none());

    for character in "Muu".chars() {
        let view = session.handle(key(&character.to_string(), 0));
        assert!(view.commit_text.is_none());
    }
    assert_eq!(session.current_view(false).preedit[0].text, "これはMuu");

    let backed = session.handle(key("", 51));
    assert_eq!(backed.preedit[0].text, "これはMu");
    session.handle(key("u", 0));

    let redundant = session.handle(command(
        "set_mode",
        serde_json::json!({"mode": "half_alnum"}),
    ));
    assert_eq!(redundant.preedit[0].text, "これはMuu");
    assert!(redundant.commit_text.is_none());

    let hiragana = session.handle(command("set_mode", serde_json::json!({"mode": "hiragana"})));
    assert_eq!(hiragana.preedit[0].text, "これはMuu");
    assert_eq!(hiragana.mode, InputMode::Hiragana);
    assert!(hiragana.commit_text.is_none());

    for character in "dashine".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        session.current_view(false).preedit[0].text,
        "これはMuuだしね"
    );

    let committed = session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("これはMuuだしね"));
    assert!(committed.preedit.is_empty());
}

// A shifted letter starts a literal span inside the composition (case kept,
// transition reported); set_mode hiragana ends it without committing.
#[test]
fn shifted_letter_starts_a_case_preserving_literal_span() {
    const SHIFT_MODIFIER: u64 = 1 << 17;

    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    for character in "koreha".chars() {
        session.handle(key(&character.to_string(), 0));
    }

    let started = session.handle(key_with_modifiers("M", 46, SHIFT_MODIFIER));
    assert_eq!(started.preedit[0].text, "これはM");
    assert_eq!(started.mode, InputMode::Hiragana);
    assert!(started.commit_text.is_none());
    assert_eq!(
        started.composition_mode_transition,
        Some(CompositionModeTransition::Literal)
    );

    session.handle(key("u", 32));
    let literal = session.handle(key("u", 32));
    assert_eq!(literal.preedit[0].text, "これはMuu");
    assert!(literal.commit_text.is_none());

    let reading = session.handle(command("set_mode", serde_json::json!({"mode": "hiragana"})));
    assert_eq!(reading.preedit[0].text, "これはMuu");
    assert!(reading.commit_text.is_none());
    assert_eq!(
        reading.composition_mode_transition,
        Some(CompositionModeTransition::Reading)
    );
    for character in "dashine".chars() {
        session.handle(key(&character.to_string(), 0));
    }

    let committed = session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("これはMuuだしね"));
    assert!(committed.preedit.is_empty());

    session.handle(key("k", 40));
    let next = session.handle(key("a", 0));
    assert_eq!(next.preedit[0].text, "か");
}

// Space inside a literal span inserts a space; after Escape returns to
// reading, Space means convert again.
#[test]
fn space_in_a_literal_span_is_literal_and_converts_again_after_reading_resumes() {
    const SHIFT_MODIFIER: u64 = 1 << 17;

    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    for character in "koreha".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key_with_modifiers("M", 46, SHIFT_MODIFIER));
    session.handle(key("u", 32));
    session.handle(key("u", 32));

    let spaced = session.handle(key(" ", 49));
    assert_eq!(spaced.preedit[0].text, "これはMuu ");
    assert!(spaced.commit_text.is_none());
    assert!(spaced.candidates.is_none());

    session.handle(key_with_modifiers("I", 34, SHIFT_MODIFIER));
    session.handle(key("m", 46));
    let literal = session.handle(key("e", 14));
    assert_eq!(literal.preedit[0].text, "これはMuu Ime");

    // Back in reading input, Space regains its conversion meaning. This
    // session has no dictionary, so conversion leaves the preedit as is.
    session.handle(key("\u{1b}", 53));
    let reading = session.handle(key(" ", 49));
    assert_eq!(reading.preedit[0].text, "これはMuu Ime");
    assert!(reading.commit_text.is_none());

    let committed = session.handle(key("\r", 36));
    assert_eq!(committed.commit_text.as_deref(), Some("これはMuu Ime"));
}

// Deleting a literal span to empty, or Escape inside it, returns to reading
// input; Escape keeps the text and only a second Escape cancels.
#[test]
fn cancelling_a_shift_literal_composition_restores_reading_input() {
    const SHIFT_MODIFIER: u64 = 1 << 17;

    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    let literal = session.handle(key_with_modifiers("M", 46, SHIFT_MODIFIER));
    assert_eq!(literal.preedit[0].text, "M");

    let cancelled = session.handle(key("", 51));
    assert!(cancelled.preedit.is_empty());
    assert!(cancelled.commit_text.is_none());
    assert!(cancelled.consumed);

    session.handle(key("k", 40));
    let restarted = session.handle(key("a", 0));
    assert_eq!(restarted.preedit[0].text, "か");
    assert!(restarted.commit_text.is_none());

    let mut escaped =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    escaped.handle(key_with_modifiers("M", 46, SHIFT_MODIFIER));
    escaped.handle(key("u", 32));
    escaped.handle(key("u", 32));

    let restored = escaped.handle(key("", 53));
    assert_eq!(restored.preedit[0].text, "Muu");
    assert!(restored.commit_text.is_none());
    assert!(restored.consumed);
    assert_eq!(
        restored.composition_mode_transition,
        Some(CompositionModeTransition::Reading)
    );

    escaped.handle(key("k", 40));
    let reading = escaped.handle(key("a", 0));
    assert_eq!(reading.preedit[0].text, "Muuか");
    assert!(reading.commit_text.is_none());

    let cancelled = escaped.handle(key("", 53));
    assert!(cancelled.preedit.is_empty());
    assert!(cancelled.commit_text.is_none());
    assert!(cancelled.consumed);

    escaped.handle(key("k", 40));
    let restarted = escaped.handle(key("a", 0));
    assert_eq!(restarted.preedit[0].text, "か");
}

// A Shift tap inside a literal span returns to reading: the press is not
// consumed, the release is, and nothing is committed.
#[test]
fn shift_tap_restores_reading_without_committing_literal_preedit() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");
    let started = session.handle(key_with_modifiers("M", 46, SHIFT_MODIFIER));
    assert_eq!(
        started.composition_mode_transition,
        Some(CompositionModeTransition::Literal)
    );
    session.handle(key("u", 32));
    session.handle(key("u", 32));

    let shift_down = session.handle(flags_changed(LEFT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    assert_eq!(shift_down.preedit[0].text, "Muu");
    assert!(!shift_down.consumed);
    assert!(shift_down.commit_text.is_none());

    let restored = session.handle(flags_changed(LEFT_SHIFT_KEYCODE, 0));
    assert_eq!(restored.preedit[0].text, "Muu");
    assert!(restored.consumed);
    assert!(restored.commit_text.is_none());
    assert_eq!(
        restored.composition_mode_transition,
        Some(CompositionModeTransition::Reading)
    );

    session.handle(key("k", 40));
    let reading = session.handle(key("a", 0));
    assert_eq!(reading.preedit[0].text, "Muuか");
    assert!(reading.commit_text.is_none());

    let cancelled = session.handle(key("", 53));
    assert!(cancelled.preedit.is_empty());
    assert!(cancelled.commit_text.is_none());
}

// A Shift tap toggles into literal input from an empty or reading
// composition, and either Shift key toggles back; the transition is in the
// view JSON as "literal" / "reading".
#[test]
fn shift_tap_enters_literal_from_an_empty_or_reading_composition() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");

    session.handle(flags_changed(LEFT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    let entered = session.handle(flags_changed(LEFT_SHIFT_KEYCODE, 0));
    assert!(entered.preedit.is_empty());
    assert!(entered.consumed);
    assert_eq!(
        entered.composition_mode_transition,
        Some(CompositionModeTransition::Literal)
    );
    assert_eq!(
        serde_json::to_value(&entered).expect("transition view JSON")["composition_mode_transition"],
        "literal"
    );

    for character in "npm".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.current_view(false).preedit[0].text, "npm");

    session.handle(flags_changed(RIGHT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    let exited = session.handle(flags_changed(RIGHT_SHIFT_KEYCODE, 0));
    assert_eq!(exited.preedit[0].text, "npm");
    assert!(exited.consumed);
    assert_eq!(
        exited.composition_mode_transition,
        Some(CompositionModeTransition::Reading)
    );

    session.handle(flags_changed(LEFT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    let reentered = session.handle(flags_changed(LEFT_SHIFT_KEYCODE, 0));
    assert_eq!(reentered.preedit[0].text, "npm");
    assert_eq!(
        reentered.composition_mode_transition,
        Some(CompositionModeTransition::Literal)
    );
}

// Shift used with a letter, or together with Command, never toggles: those
// releases are not consumed and the mode stays literal.
#[test]
fn shifted_chords_and_other_modifiers_never_complete_a_shift_tap() {
    let mut session =
        new_session(None, None, TestSessionConfig::default()).expect("embedded table");

    session.handle(flags_changed(LEFT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    session.handle(key_with_modifiers("A", 0, SHIFT_MODIFIER));
    let first_release = session.handle(flags_changed(LEFT_SHIFT_KEYCODE, 0));
    assert_eq!(first_release.preedit[0].text, "A");
    assert!(!first_release.consumed);

    session.handle(key("b", 11));
    session.handle(flags_changed(RIGHT_SHIFT_KEYCODE, SHIFT_MODIFIER));
    session.handle(key_with_modifiers("C", 8, SHIFT_MODIFIER));
    let second_release = session.handle(flags_changed(RIGHT_SHIFT_KEYCODE, 0));
    assert_eq!(second_release.preedit[0].text, "AbC");
    assert!(!second_release.consumed);

    session.handle(flags_changed(
        LEFT_SHIFT_KEYCODE,
        SHIFT_MODIFIER | COMMAND_MODIFIER,
    ));
    let modified_release = session.handle(flags_changed(LEFT_SHIFT_KEYCODE, COMMAND_MODIFIER));
    assert_eq!(modified_release.preedit[0].text, "AbC");
    assert!(!modified_release.consumed);

    let still_literal = session.handle(key("k", 40));
    assert_eq!(still_literal.preedit[0].text, "AbCk");
}

// A direct alnum commit becomes a context unit; clear_preceding_context
// empties the context.
#[test]
fn direct_commits_are_context_units_and_the_explicit_lifecycle_boundary_clears_them() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    session.handle(command(
        "set_mode",
        serde_json::json!({"mode": "half_alnum"}),
    ));
    let direct = session.handle(key("x", 7));
    assert_eq!(direct.commit_text.as_deref(), Some("x"));
    assert_eq!(session.preceding_commit_context.snapshot(), ["x"]);

    session.handle(command("set_mode", serde_json::json!({"mode": "hiragana"})));
    session.handle(command("clear_preceding_context", serde_json::json!({})));
    assert!(session.preceding_commit_context.snapshot().is_empty());
}

// On the real dictionary, extending the first segment one character at a
// time reaches the whole reading, one more step reports the boundary error
// with its parameters, and shrinking back restores several segments.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn production_dictionary_key_events_resize_to_end_and_back() {
    const SHIFT_MODIFIER: u64 = 1 << 17;
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;
    const NAVIGATION_MODIFIERS: u64 = FUNCTION_MODIFIER | NUMERIC_PAD_MODIFIER;

    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    for (input, initial_layout, expected_surface_caret) in [
        ("kyouhaiitenkidesu", vec![4, 7], Some(3)),
        ("nihongowonyuuryoku", vec![5, 6], None),
    ] {
        let mut session =
            session_with_dictionary_path(Some(&dictionary_path)).expect("production dictionary");
        for character in input.chars() {
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
            initial_layout
        );
        if let Some(expected_surface_caret) = expected_surface_caret {
            assert_eq!(converted.caret_utf16, expected_surface_caret);
        }
        let initial_focused_length = initial_layout[0];
        let reading_length = initial_layout.iter().sum::<usize>();

        let assert_view = |view: &ViewState, focused_length: usize| {
            let segments = view.segments.as_ref().unwrap();
            assert_eq!(
                segments
                    .iter()
                    .map(|segment| segment.reading_range.len)
                    .sum::<usize>(),
                reading_length
            );
            assert!(segments.windows(2).all(|pair| {
                pair[0].reading_range.start + pair[0].reading_range.len
                    == pair[1].reading_range.start
            }));
            assert_eq!(segments[0].reading_range.start, 0);
            assert_eq!(segments[0].reading_range.len, focused_length);
            assert_eq!(view.preedit.iter().filter(|run| run.is_focused).count(), 1);
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

        let mut previous = converted;
        for requested_length in (initial_focused_length + 1)..=reading_length {
            let view = resize(&mut session, 124);
            assert!(view.error.is_none());
            assert_view(&view, requested_length);
            assert_ne!(view.preedit, previous.preedit);
            previous = view;
        }
        assert_eq!(previous.segments.as_ref().unwrap().len(), 1);

        let past_end = resize(&mut session, 124);
        let error = past_end.error.as_ref().expect("past-end error");
        assert_eq!(error.code, "invalid_segment_boundary");
        assert_eq!(error.params["requested_length"], reading_length + 1);
        assert_eq!(error.params["current_length"], reading_length);
        assert_eq!(error.params["focused_segment"], 0);
        assert_eq!(error.params["segment_count"], 1);
        assert_eq!(past_end.preedit, previous.preedit);
        previous = past_end;

        for requested_length in (initial_focused_length..reading_length).rev() {
            let view = resize(&mut session, 123);
            assert!(view.error.is_none());
            assert_view(&view, requested_length);
            assert_ne!(view.preedit, previous.preedit);
            previous = view;
        }
        assert!(previous.segments.as_ref().unwrap().len() > 1);
    }
}

// Pinned product cases: mixed これはMuuだしね keeps Muu as one unknown Latin
// node by either way of typing it, and うえ keeps ↑ within the nine
// digit-selectable candidates.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn first_product_quality_cases_have_an_explainable_baseline() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let dictionary = crate::real_dictionary::shared_at(&dictionary_path);
    let snapshot = CostSnapshot::baseline(&dictionary);
    let segmenter = default_segmenter();

    let mixed = trace_conversion("これはMuuだしね", &segmenter, &snapshot).unwrap();
    assert_eq!(mixed.best_path.surface, "これはMuuだしね");
    assert!(mixed.best_path.nodes.iter().any(|node| {
        node.surface == "Muu"
            && node.kind == muu_conversion::NodeKind::Unknown(UnknownClass::AsciiAlphabetic)
    }));
    assert_eq!(
        mixed
            .segments
            .iter()
            .map(|segment| segment.selected_surface.as_str())
            .collect::<String>(),
        "これはMuuだしね"
    );

    let mut mixed_session =
        session_with_dictionary_path(Some(&dictionary_path)).expect("production session");
    for character in "koreha".chars() {
        mixed_session.handle(key(&character.to_string(), 0));
    }
    mixed_session.handle(command(
        "set_mode",
        serde_json::json!({"mode": "half_alnum"}),
    ));
    for character in "Muu".chars() {
        mixed_session.handle(key(&character.to_string(), 0));
    }
    mixed_session.handle(command("set_mode", serde_json::json!({"mode": "hiragana"})));
    for character in "dashine".chars() {
        mixed_session.handle(key(&character.to_string(), 0));
    }
    let converted_mixed = mixed_session.handle(key(" ", 49));
    assert_eq!(
        converted_mixed
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "これはMuuだしね"
    );
    assert!(converted_mixed.commit_text.is_none());

    let mut shifted_mixed =
        session_with_dictionary_path(Some(&dictionary_path)).expect("production session");
    for character in "koreha".chars() {
        shifted_mixed.handle(key(&character.to_string(), 0));
    }
    shifted_mixed.handle(key_with_modifiers("M", 46, 1 << 17));
    shifted_mixed.handle(key("u", 32));
    shifted_mixed.handle(key("u", 32));
    shifted_mixed.handle(command("set_mode", serde_json::json!({"mode": "hiragana"})));
    for character in "dashine".chars() {
        shifted_mixed.handle(key(&character.to_string(), 0));
    }
    let converted_shifted = shifted_mixed.handle(key(" ", 49));
    assert_eq!(
        converted_shifted
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "これはMuuだしね"
    );
    assert!(converted_shifted.commit_text.is_none());

    let symbol = trace_conversion("うえ", &segmenter, &snapshot).unwrap();
    assert_eq!(symbol.best_path.surface, "上");
    let arrow = symbol.segments[0]
        .candidates
        .iter()
        .find(|candidate| candidate.surface == "↑")
        .expect("arrow candidate");
    assert!(arrow.rank < 9, "arrow must remain digit-selectable");
}

// A resize with no constrained path reports
// segment_resize_no_constrained_path with its parameters and leaves the
// preedit, segments and selection exactly as they were.
#[test]
fn constrained_resize_failure_is_reported_without_changing_composition() {
    const SHIFT_MODIFIER: u64 = 1 << 17;
    const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
    const FUNCTION_MODIFIER: u64 = 1 << 23;

    let dictionary = constrained_resize_failure_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    for character in "aiue".chars() {
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
        [3, 1]
    );
    let selected = session.select_candidate_index(1);
    assert_eq!(selected.candidates.as_ref().unwrap().selected_index, 1);

    let failed = session.handle(key_with_modifiers(
        "\u{f702}",
        123,
        SHIFT_MODIFIER | NUMERIC_PAD_MODIFIER | FUNCTION_MODIFIER,
    ));
    let error = failed.error.as_ref().expect("structured resize error");
    assert_eq!(error.code, "segment_resize_no_constrained_path");
    assert_eq!(error.params["requested_length"], 2);
    assert_eq!(error.params["current_length"], 3);
    assert_eq!(error.params["focused_segment"], 0);
    assert_eq!(error.params["segment_count"], 2);
    assert_eq!(failed.preedit, selected.preedit);
    assert_eq!(failed.segments, selected.segments);
    assert_eq!(failed.candidates, selected.candidates);
}

// A verb whose surface does not end in its base form is refused when the
// layer is built, before any publication.
#[test]
fn invalid_inflection_base_is_rejected_before_overlay_swap() {
    let snapshot = UserDictionarySnapshot {
        schema_version: USER_DICTIONARY_SCHEMA_VERSION,
        entries: vec![UserDictionaryRecord {
            reading: "かける".to_owned(),
            surface: "書ける".to_owned(),
            part_of_speech: UserDictionaryPartOfSpeech::from_id("KA_GROUP1_VERB").unwrap(),
            priority: UserDictionaryPriority::High,
        }],
    };
    assert!(matches!(
        snapshot.into_overlay(),
        Err(UserDictionaryError::InvalidBaseForm)
    ));
}

// A suggestion-only row adds no candidate; a suppression row removes the
// matching system-dictionary candidate.
#[test]
fn suggestion_only_is_dormant_and_suppression_removes_the_base_token() {
    let dictionary = conversion_dictionary();
    let dormant = UserDictionarySnapshot {
        schema_version: USER_DICTIONARY_SCHEMA_VERSION,
        entries: vec![UserDictionaryRecord {
            reading: "きょう".to_owned(),
            surface: "休眠候補".to_owned(),
            part_of_speech: UserDictionaryPartOfSpeech::from_id("SUGGESTION_ONLY").unwrap(),
            priority: UserDictionaryPriority::High,
        }],
    };
    let mut session = session_with_user_dictionary(Some(dictionary.path()), None, dormant)
        .expect("dormant entries remain valid persistence records");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let dormant_result = session.handle(key(" ", 49));
    assert!(
        dormant_result
            .candidates
            .unwrap()
            .items
            .iter()
            .all(|candidate| candidate.text != "休眠候補")
    );

    let suppression = UserDictionarySnapshot {
        schema_version: USER_DICTIONARY_SCHEMA_VERSION,
        entries: vec![UserDictionaryRecord {
            reading: "きょう".to_owned(),
            surface: "今日".to_owned(),
            part_of_speech: UserDictionaryPartOfSpeech::from_id("SUPPRESSION_WORD").unwrap(),
            priority: UserDictionaryPriority::High,
        }],
    };
    let mut session = session_with_user_dictionary(Some(dictionary.path()), None, suppression)
        .expect("valid suppression record");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let suppressed_result = session.handle(key(" ", 49));
    assert!(
        suppressed_result
            .candidates
            .unwrap()
            .items
            .iter()
            .all(|candidate| candidate.text != "今日")
    );
}

// The bundled seed builds one overlay shared by every session (reference
// count unchanged), survives a User replacement, and converts codes, with the
// first official row winning for a shared code.
#[test]
fn bundled_postal_seed_loads_and_converts_at_product_scale() {
    let build_started = Instant::now();
    let dictionary = conversion_dictionary();
    let postal_code_dictionary = SharedVocabulary::default();
    Core::with_vocabulary(CoreConfig::default(), postal_code_dictionary.clone())
        .unwrap()
        .set_postal_fixture(bundled_postal_code_snapshot())
        .expect("product-scale postal snapshot");
    let build_elapsed = build_started.elapsed();
    let (_, authority) = postal_code_dictionary.postal_snapshot();
    let authority_count = Arc::strong_count(&authority);

    let session_started = Instant::now();
    let mut sessions = (0..16)
        .map(|_| {
            session_with_dictionaries(
                Some(dictionary.path()),
                None,
                UserDictionarySnapshot {
                    schema_version: USER_DICTIONARY_SCHEMA_VERSION,
                    entries: Vec::new(),
                },
                postal_code_dictionary.clone(),
            )
            .expect("session data")
        })
        .collect::<Vec<_>>();
    let session_elapsed = session_started.elapsed();
    assert_eq!(
        Arc::strong_count(&authority),
        authority_count,
        "controllers must share the one process-wide immutable postal overlay"
    );

    let mut session = sessions.pop().unwrap();
    let response = session.core.set_user_fixture(
        serde_json::from_value(
            serde_json::to_value(user_dictionary_snapshot([(
                "むう",
                "Muu",
                UserDictionaryPriority::High,
            )]))
            .unwrap(),
        )
        .unwrap(),
    );
    assert!(response.is_ok());
    let (_, after_user_replace) = postal_code_dictionary.postal_snapshot();
    assert!(Arc::ptr_eq(&authority, &after_user_replace));

    for character in "100-0001".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        session.handle(key(" ", 49)).preedit[0].text,
        "東京都千代田区千代田"
    );
    session.handle(key("\u{1b}", 53));
    for character in "0040000".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        session.handle(key(" ", 49)).preedit[0].text,
        "北海道札幌市厚別区",
        "the first official row wins when a code has multiple municipalities"
    );
    eprintln!("postal product-scale build={build_elapsed:?} sessions16={session_elapsed:?}");
}
// Records outlive their Session in the Core queue, carry the process nonce
// from the config but a Rust-made session nonce per Session, and number
// from 1 per Session.
#[test]
fn core_factory_records_survive_session_destruction_with_unique_rust_nonces() {
    let dictionary = commit_stream_dictionary();
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap(),
    );
    core.configure_sessions(CoreSessionSettings {
        commit_stream_config: Some(test_commit_stream_config("ignored-template-nonce")),
        ..CoreSessionSettings::default()
    })
    .unwrap_or_else(|_| panic!("configure once"));
    for _ in 0..2 {
        let mut session = core.create_session().unwrap();
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        session.handle(key(" ", 49));
        session.handle(key("\r", 36));
        drop(session);
    }
    let first: serde_json::Value =
        serde_json::from_slice(&core.take_commit_stream_event().unwrap()).unwrap();
    let second: serde_json::Value =
        serde_json::from_slice(&core.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(first["schema"], "muu.commit-stream.v5");
    assert_eq!(first["process_nonce"], second["process_nonce"]);
    assert_ne!(first["session_nonce"], second["session_nonce"]);
    assert_ne!(first["session_nonce"], "ignored-template-nonce");
    assert_eq!(first["event_seq"], 1);
    assert_eq!(second["event_seq"], 1);
    assert!(core.take_commit_stream_event().is_none());
}
