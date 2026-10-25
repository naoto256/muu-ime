//! Real dictionary/model → Session → visible runs → commit, on self-authored inputs.
//! Score/kernel goldens live in muu-lm; these pin the application's conversion path.
#[path = "../../../test-support/real_assets.rs"]
mod real_assets;

use muu_session::{
    CommitStreamConfig, ConversionModelKind, Core, CoreConfig, CoreSessionSettings, KeyEvent,
    PersonalFrequencySnapshot, Request, ViewState,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

// One self-authored input and the expected presentation for each model. The
// goldens live in data/production-lm-session.json, which is plain JSON and
// cannot hold comments; the notes on what each field pins are here.
//
// Updating after an intentional conversion change: there is no regenerate
// switch, so edit the JSON by hand from the failure diff, for edge and zenz
// separately, and review each value as a new contract:
// - ranking or LM selection changed: `runs[].text`, `first_candidate`,
//   `selected_candidate`, `commit` and `switched_runs`;
// - segmentation changed: also `runs[].utf16_start/utf16_len`,
//   `segments[].reading_range` and `caret_utf16`;
// - focus or underline presentation changed: `is_focused`, `underline_style`,
//   `candidate_focus` and `selection`.
// If the change means a model no longer switches on any case, replace the
// ambiguous case with one where it does; do not just set the flag to false.
#[derive(Deserialize)]
struct Case {
    id: String,
    romaji: String,
    edge: Value,
    zenz: Value,
}

fn command(name: &str) -> Request {
    Request::Command {
        command: name.to_owned(),
        arg: Value::Null,
    }
}

fn type_text(session: &mut muu_session::Session, text: &str) {
    for character in text.chars() {
        let view = session.handle(Request::Key {
            key: KeyEvent {
                event_type: "key_down".to_owned(),
                keycode: 0,
                modifiers: 0,
                characters_utf8: Some(character.to_string()),
                characters_ignoring_modifiers: Some(character.to_string()),
                is_repeat: false,
            },
        });
        assert!(view.error.is_none());
        assert!(view.consumed);
    }
}

// What the user sees after the first convert, and what commit inserts:
// - runs: each preedit run's text, UTF-16 range, focus, converted flag and
//   underline;
// - caret_utf16 and selection;
// - segments: each segment's reading range and candidate focus;
// - first and selected candidate of the focused segment;
// - commit: the text the commit inserts.
// Nonces and score values are left out, so model rounding cannot break it.
fn presentation(view: &ViewState, commit: &ViewState) -> Value {
    let candidates = view.candidates.as_ref().expect("converted candidates");
    json!({
        "runs": view.preedit,
        "caret_utf16": view.caret_utf16,
        "selection": view.selection,
        "segments": view.segments,
        "first_candidate": candidates.items.first().expect("first candidate").text,
        "selected_candidate": candidates.items[candidates.selected_index].text,
        "commit": commit.commit_text,
    })
}

fn cases() -> Vec<Case> {
    serde_json::from_str(include_str!("data/production-lm-session.json"))
        .expect("self-authored Session goldens")
}

#[test]
fn fixture_keeps_complete_distinct_goldens_and_real_choices_for_both_models() {
    let cases = cases();
    // This test needs no assets and runs in check-fast, so a malformed or
    // emptied golden fails everywhere. Update the count when adding or
    // removing a case.
    assert_eq!(cases.len(), 3, "do not silently lose a conversion contract");
    let ids: BTreeSet<_> = cases.iter().map(|case| &case.id).collect();
    assert_eq!(ids.len(), cases.len());
    for case in &cases {
        assert!(!case.romaji.is_empty());
        for expected in [&case.edge, &case.zenz] {
            assert!(!expected["runs"].as_array().unwrap().is_empty());
            assert!(!expected["commit"].as_str().unwrap().is_empty());
        }
    }
    // Each model must keep at least one case where the LM changed the
    // dictionary baseline (①). Otherwise the goldens would pass with the LM
    // loaded but never choosing.
    for kind in [ConversionModelKind::Edge, ConversionModelKind::Zenz] {
        assert!(
            cases.iter().any(|case| {
                let expected = match kind {
                    ConversionModelKind::Edge => &case.edge,
                    ConversionModelKind::Zenz => &case.zenz,
                };
                expected["switched_runs"]
                    .as_array()
                    .unwrap()
                    .contains(&Value::Bool(true))
            }),
            "keep a golden in which LM selection actually changes the baseline"
        );
    }
}

fn check_model(kind: ConversionModelKind, model_variable: &str) {
    let cases = cases();
    let (Some(dictionary), Some(model)) = (
        real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
        real_assets::real_asset_path(model_variable),
    ) else {
        return;
    };
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(&dictionary),
        })
        .expect("pinned public dictionary"),
    );
    core.initialize_data(
        muu_session::StoredFileState::Missing,
        &[],
        &PersonalFrequencySnapshot::default(),
    )
    .unwrap();
    core.configure_sessions(CoreSessionSettings {
        conversion_lm_dir: Some(model),
        conversion_lm_model: Some(kind),
        conversion_lm_fallback_dir: None,
        conversion_lm_load_synchronously: true,
        commit_stream_config: Some(CommitStreamConfig {
            schema: "muu.commit-stream.v5".to_owned(),
            process_nonce: "public-session-golden".to_owned(),
            source_commit: "0123456789abcdef".to_owned(),
            build: 0,
        }),
    })
    .unwrap_or_else(|_| panic!("one Session configuration"));
    // The requested model actually loaded, with no fallback to the other one.
    let lm = core.language_model();
    assert_eq!(lm.info.actual_model, Some(kind.as_str()));
    assert_eq!(lm.info.route, "requested", "no unnoticed fallback");

    for case in cases {
        // Each conversion starts with empty context and learning. Model weights
        // are shared, but one golden's commit cannot bias the next golden.
        core.clear_personal_frequency();
        let mut session = core.create_session().unwrap();
        type_text(&mut session, &case.romaji);
        let converted = session.handle(command("convert"));
        assert!(converted.error.is_none(), "{}", case.id);
        assert!(converted.degraded.is_none(), "{}", case.id);
        let committed = session.handle(command("commit"));
        assert!(committed.error.is_none());
        assert!(committed.preedit.is_empty());
        let mut actual = presentation(&converted, &committed);
        let expected = match kind {
            ConversionModelKind::Edge => &case.edge,
            ConversionModelKind::Zenz => &case.zenz,
        };
        let event: Value =
            serde_json::from_slice(&core.take_commit_stream_event().unwrap()).unwrap();
        // The commit record's report of the first conversion. Format 3 is a
        // literal here: update it with the SelectionReport format. The rules
        // version comes from the code, so a rules bump needs no edit here.
        let report = &event["initial"]["selection_report"];
        assert_eq!(report["format_version"], 3);
        assert_eq!(report["rules_version"], kind.rules_version());
        assert_eq!(
            event["provenance"]["language_model"]["actual_model"],
            kind.as_str()
        );
        // These are LM selection runs (reading ranges the LM scored), not
        // preedit runs: "公園で遊びます" has two preedit runs and one selection
        // run. Each must be a real choice: not skipped, with at least two
        // finite-scored hypotheses.
        let runs = report["runs"].as_array().expect("scored runs");
        assert!(!runs.is_empty(), "LM must actually score this conversion");
        for run in runs {
            assert!(run.get("skipped").is_none_or(Value::is_null));
            let hypotheses = run["hypotheses"].as_array().unwrap();
            assert!(
                hypotheses.len() >= 2,
                "a genuine choice, not only a loaded model"
            );
            for hypothesis in hypotheses {
                assert!(hypothesis["score"].as_f64().is_some_and(f64::is_finite));
            }
        }
        // switched: the applied surface over the run differs from the
        // dictionary baseline, i.e. the LM moved off ①. One flag per selection
        // run, in report order.
        actual["switched_runs"] = json!(
            runs.iter()
                .map(|run| { run["switched"].as_bool().expect("LM selection fact") })
                .collect::<Vec<_>>()
        );
        assert_eq!(&actual, expected, "{} {}", kind.as_str(), case.id);
        assert!(
            core.take_commit_stream_event().is_none(),
            "one commit record"
        );
    }
}

#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn edge_session_preserves_initial_candidates_runs_and_commit() {
    check_model(ConversionModelKind::Edge, "MUU_LM_MODEL_DIR");
}

#[test]
#[ignore = "requires public dictionary, zenz (check-assets)"]
fn zenz_session_preserves_initial_candidates_runs_and_commit() {
    check_model(ConversionModelKind::Zenz, "MUU_ZENZ_MODEL_DIR");
}
