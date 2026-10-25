use std::time::Instant;

use std::sync::Arc;

use crate::commit_stream::CommitStreamState;
use crate::session::*;
use crate::test_support::*;
use std::path::Path;

// On the real dictionary without a model, every run's baseline facts are
// the generation's own segments (id, surface, path), generation and selected
// segments are equal, and the record carries no legacy scorer field.
#[test]
#[ignore = "requires public dictionary (check-assets)"]
fn production_session_records_generation_as_the_selection_baseline_in_v5() {
    let Some(dictionary_path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    for (input, expected) in [
        (
            "honkennnituitehabettokyougiwoshiyou",
            "本件については別途競技を使用",
        ),
        ("kyougishiyou", "競技しよう"),
    ] {
        let mut session = new_session(
            Some(Path::new(&dictionary_path)),
            None,
            TestSessionConfig {
                commit_stream_config: Some(test_commit_stream_config("generation-baseline-test")),
                ..TestSessionConfig::default()
            },
        )
        .expect("production session");
        for character in input.chars() {
            session.handle(key(&character.to_string(), 0));
        }
        let view = session.handle(key(" ", 49));
        let surface = view
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>();
        assert_eq!(surface, expected, "{input}");
        session.handle(key("\r", 36));
        let event = session.take_commit_stream_event().expect("native v5 event");
        let event: serde_json::Value = serde_json::from_slice(&event).expect("event json");
        // Both production inputs pin actual generation baseline authority
        // (docs/src/internals/conversion.md「言語モデルによる選び直し」/docs/src/internals/storage.md「記録」), including candidate IDs and each recorded final surface.
        for baseline in event["initial"]["selection_report"]["runs"][0]["baseline_segments"]
            .as_array()
            .unwrap()
        {
            let index = baseline["segment_index"].as_u64().unwrap() as usize;
            let generation = &event["initial"]["generation_segments"][index];
            assert_eq!(baseline["candidate_id"], generation["candidate_id"]);
            assert_eq!(baseline["surface"], generation["surface"]);
            assert_eq!(baseline["path"]["index"], generation["path"]["index"]);
        }
        assert_eq!(event["schema"], "muu.commit-stream.v5");
        assert_eq!(event["final"]["surface"], expected, "{input}");
        assert!(event.get("native_scorer").is_none());
        assert!(event["timing"]["conversion_to_result_us"].is_u64());
        assert_eq!(
            event["initial"]["generation_segments"],
            event["initial"]["selected_segments"]
        );
    }
}

// With no scored hypothesis at all, a commit is still recorded from the
// actual segments.
#[test]
fn recording_does_not_require_enumerated_baseline_or_applied_membership() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.handle(key(" ", 49));
    // No scored hypotheses. Actual segments suffice for recording (docs/src/internals/storage.md「記録」).
    assert!(
        session
            .last_selection_report
            .as_ref()
            .unwrap()
            .runs
            .iter()
            .all(|r| r.hypotheses.is_empty())
    );
    let committed = session.handle(key("\r", 36));
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["final"]["surface"], committed.commit_text.unwrap());
    assert!(
        event["initial"]["generation_segments"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
    );
}

// Nothing is recorded at conversion; the commit yields exactly one v5
// record with the provenance fields, the actual selected segments, its
// layer and token kinds, ordered timings, and dropped_records 0, inside a
// size budget.
#[test]
fn commit_stream_emits_one_versioned_record_only_after_conversion_commit() {
    let dictionary = commit_stream_dictionary();
    let mut session = commit_stream_session(&dictionary);
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.handle(key(" ", 49));
    let actual = crate::commit_stream::actual_segments(session.segments.as_ref().unwrap());
    assert!(session.take_commit_stream_event().is_none());
    let committed = session.handle(key("\r", 36));
    let event = session.take_commit_stream_event().unwrap();
    assert!(
        event.len() <= 16_384,
        "v5 fixture budget, including actual paths"
    );
    assert!(session.take_commit_stream_event().is_none());
    let json: serde_json::Value = serde_json::from_slice(&event).unwrap();
    assert_eq!(json["schema"], "muu.commit-stream.v5");
    assert_eq!(json["process_nonce"], "test-process");
    assert!(
        json["session_nonce"]
            .as_str()
            .unwrap()
            .starts_with("rust-session-")
    );
    assert_eq!(json["event_seq"], 1);
    assert_eq!(json["initial"]["reading"], "きょう");
    assert_eq!(json["provenance"]["source_commit"], "0123456789abcdef");
    assert_eq!(json["provenance"]["build"], 180);
    assert_eq!(
        json["provenance"]["dictionary"]["payload_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(
        json["provenance"]["dictionary"]["token_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        json["initial"]["selected_segments"],
        serde_json::to_value(actual).unwrap()
    );
    assert_eq!(json["final"]["surface"], committed.commit_text.unwrap());
    assert_eq!(
        json["final"]["segments"][0]["origin_layers"],
        serde_json::json!(["internal"])
    );
    assert_eq!(
        json["final"]["segments"][0]["path"]["nodes"][0]["token"]["kind"],
        "dictionary"
    );
    assert_eq!(json["final"]["candidate_selected_explicitly"], false);
    assert_eq!(
        json["provenance"]["rules_version"],
        muu_lm::SELECTION_RULES_VERSION
    );
    assert_eq!(
        json["initial"]["selection_report"]["rules_version"],
        muu_lm::SELECTION_RULES_VERSION
    );
    assert!(json["provenance"]["language_model"]["weights_sha256"].is_null());
    assert!(
        json["timing"]["conversion_to_commit_us"].as_u64().unwrap()
            >= json["timing"]["conversion_to_result_us"].as_u64().unwrap()
    );
    assert!(json["provenance"]["vocabulary"]["user"]["fingerprint"].is_object());
    assert_eq!(json["dropped_records"], 0);
}

// Session creation refuses any schema other than v5, retired ones included.
#[test]
fn commit_stream_rejects_retired_and_unknown_schemas_at_session_boundary() {
    let dictionary = conversion_dictionary();
    for schema in [
        "muu.commit-stream.v2",
        "muu.commit-stream.v3",
        "muu.commit-stream.v4",
        "muu.commit-stream.v99",
    ] {
        let mut config = test_commit_stream_config("unknown-schema");
        config.schema = schema.to_owned();
        let result = new_session(
            Some(dictionary.path()),
            None,
            TestSessionConfig {
                commit_stream_config: Some(config.clone()),
                ..TestSessionConfig::default()
            },
        );
        assert!(
            matches!(result, Err(SessionError::InvalidCommitStreamSchema)),
            "{schema}"
        );
    }
}

// An incomplete-evidence skip is recorded as such, with an empty run list.
#[test]
fn commit_stream_records_incomplete_evidence_without_fabricating_selection() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.handle(key(" ", 49));
    let segments = session.segments.clone().unwrap();
    let captured = session.conversion_snapshot.as_ref().unwrap();
    let report = selection::SelectionReport {
        format_version: 3,
        rules_version: 3,
        runs: Vec::new(),
        skipped: Some(selection::SelectionSkipped::IncompletePathEvidence),
    };
    let now = Instant::now();
    session.commit_stream.as_mut().unwrap().begin(
        &segments,
        &segments,
        &report,
        captured,
        &session.core.language_model(),
        recording::RecordingTimes {
            conversion_started: now,
            result_finished: now,
        },
    );
    session.handle(key("\r", 36));
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(
        event["initial"]["selection_report"]["skipped"],
        "incomplete_path_evidence"
    );
    assert_eq!(
        event["initial"]["selection_report"]["runs"],
        serde_json::json!([])
    );
}

// Two Sessions of one Core share the process nonce and each start at
// event_seq 1, so (process, session, seq) is still unique.
#[test]
fn commit_stream_event_authority_is_unique_across_engines() {
    fn commit_one(session: &mut Session) -> serde_json::Value {
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        session.handle(key(" ", 49));
        session.handle(key("\r", 36));
        serde_json::from_slice(
            &session
                .take_commit_stream_event()
                .expect("one finalized event"),
        )
        .expect("event json")
    }

    let dictionary = commit_stream_dictionary();
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap(),
    );
    core.configure_sessions(CoreSessionSettings {
        commit_stream_config: Some(test_commit_stream_config("ignored")),
        ..CoreSessionSettings::default()
    })
    .unwrap_or_else(|_| panic!("configure"));
    let first = commit_one(&mut core.create_session().unwrap());
    let second = commit_one(&mut core.create_session().unwrap());

    assert_eq!(first["process_nonce"], second["process_nonce"]);
    assert_ne!(first["session_nonce"], second["session_nonce"]);
    assert_eq!(first["event_seq"], 1);
    assert_eq!(second["event_seq"], 1);
    assert_ne!(
        (
            &first["process_nonce"],
            &first["session_nonce"],
            &first["event_seq"],
        ),
        (
            &second["process_nonce"],
            &second["session_nonce"],
            &second["event_seq"],
        )
    );
}

// A cancelled conversion produces no record and takes no sequence number.
#[test]
fn commit_stream_drops_a_cancelled_conversion() {
    let dictionary = commit_stream_dictionary();
    let mut session = commit_stream_session(&dictionary);
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    session.handle(key("\u{1b}", 53));
    assert!(session.take_commit_stream_event().is_none());

    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    session.handle(key("\r", 36));
    let event = session.take_commit_stream_event().expect("committed event");
    let json: serde_json::Value = serde_json::from_slice(&event).unwrap();
    assert_eq!(
        json["event_seq"], 1,
        "a cancelled conversion consumes no sequence number"
    );
    assert_eq!(json["final"]["segment_resized"], false);
    assert!(session.take_commit_stream_event().is_none());
}

// Delete back to the reading discards the record draft with the segments.
#[test]
fn commit_stream_cancels_a_conversion_discarded_by_backspace() {
    let dictionary = commit_stream_dictionary();
    let mut session = commit_stream_session(&dictionary);
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    session.handle(key(" ", 49));
    assert!(
        session
            .commit_stream
            .as_ref()
            .is_some_and(CommitStreamState::has_active_for_testing)
    );

    let reading = session.handle(key("", 51));
    assert!(reading.segments.is_none());
    assert!(
        !session
            .commit_stream
            .as_ref()
            .is_some_and(CommitStreamState::has_active_for_testing),
        "discarding converted segments must discard their telemetry draft"
    );
}

// An unknown-word node is recorded as kind unknown with no origin layer; a
// segment built from several one-character dictionary nodes keeps each node.
#[test]
fn commit_stream_classifies_unknown_and_single_kanji_composed_authority() {
    let dictionary = commit_stream_dictionary();
    let mut unknown_session = commit_stream_session(&dictionary);
    for character in "nu".chars() {
        unknown_session.handle(key(&character.to_string(), 0));
    }
    unknown_session.handle(key(" ", 49));
    unknown_session.handle(key("\r", 36));
    let unknown: serde_json::Value = serde_json::from_slice(
        &unknown_session
            .take_commit_stream_event()
            .expect("unknown event"),
    )
    .unwrap();
    assert_eq!(
        unknown["final"]["segments"][0]["path"]["nodes"][0]["token"]["kind"],
        "unknown"
    );
    assert_eq!(
        unknown["final"]["segments"][0]["origin_layers"],
        serde_json::json!([])
    );

    let dictionary = multi_segment_dictionary();
    let mut composed_session = commit_stream_session(&dictionary);
    for character in "ai".chars() {
        composed_session.handle(key(&character.to_string(), 0));
    }
    composed_session.handle(key(" ", 49));
    composed_session.handle(key("\r", 36));
    let composed: serde_json::Value = serde_json::from_slice(
        &composed_session
            .take_commit_stream_event()
            .expect("composed event"),
    )
    .unwrap();
    assert_eq!(composed["final"]["segments"].as_array().unwrap().len(), 1);
    let nodes = composed["final"]["segments"][0]["path"]["nodes"]
        .as_array()
        .unwrap();
    assert!(nodes.len() > 1);
    assert!(nodes.iter().all(|n| n["token"]["kind"] == "dictionary"
        && n["surface"].as_str().unwrap().chars().count() == 1));
}

// Recording is off without a config; with one, a NaN margin is written as
// {"non_finite":"nan"}, not null, and recording continues afterwards.
#[test]
fn commit_stream_is_default_off_and_nonfinite_facts_remain_distinct_from_absence() {
    let dictionary = commit_stream_dictionary();
    let mut disabled = session_with_dictionary_path(Some(dictionary.path())).unwrap();
    for c in "kyou".chars() {
        disabled.handle(key(&c.to_string(), 0));
    }
    disabled.handle(key(" ", 49));
    assert!(disabled.handle(key("\r", 36)).commit_text.is_some());
    assert!(disabled.take_commit_stream_event().is_none());
    let mut session = commit_stream_session(&dictionary);
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.handle(key(" ", 49));
    let segments = session.segments.clone().unwrap();
    let mut report = session.last_selection_report.clone().unwrap();
    report.runs[0].baseline_minus_best_score = Some(f32::NAN);
    let now = Instant::now();
    session.commit_stream.as_mut().unwrap().begin(
        &segments,
        &segments,
        &report,
        session.conversion_snapshot.as_ref().unwrap(),
        &session.core.language_model(),
        recording::RecordingTimes {
            conversion_started: now,
            result_finished: now,
        },
    );
    assert!(session.handle(key("\r", 36)).commit_text.is_some());
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(
        event["initial"]["selection_report"]["runs"][0]["baseline_minus_best_score"],
        serde_json::json!({"non_finite":"nan"})
    );
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.handle(key(" ", 49));
    session.handle(key("\r", 36));
    assert!(session.take_commit_stream_event().is_some());
}

// A User dictionary candidate is recorded with the user layer and an
// overlay token.
#[test]
fn commit_stream_marks_user_dictionary_candidate_authority() {
    let dictionary = commit_stream_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("test-user-dictionary-engine")),
        user_dictionary_snapshot([("きょう", "ユーザー今日", UserDictionaryPriority::High)]),
    )
    .expect("valid user dictionary");

    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.handle(key(" ", 49)).preedit[0].text, "ユーザー今日");
    session.handle(key("\r", 36));
    let event: serde_json::Value = serde_json::from_slice(
        &session
            .take_commit_stream_event()
            .expect("user dictionary commit event"),
    )
    .expect("valid JSON");
    assert_eq!(
        event["final"]["segments"][0]["origin_layers"],
        serde_json::json!(["user"])
    );
    assert_eq!(
        event["final"]["segments"][0]["path"]["nodes"][0]["token"]["tier"],
        "user"
    );
    assert_eq!(
        event["final"]["segments"][0]["path"]["nodes"][0]["token"]["dictionary_key"]
            .as_str()
            .unwrap()
            .len(),
        16
    );
}

// An imported dictionary candidate is recorded with the imported layer and
// its opaque dictionary key.
#[test]
fn commit_stream_marks_imported_dictionary_candidate_authority() {
    let dictionary = commit_stream_dictionary();
    let catalog = ImportedDictionaryCatalogSnapshot {
        schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
        dictionaries: vec![ImportedDictionarySnapshot {
            dictionary_id: "example.commit-stream".to_owned(),
            enabled: true,
            entries: vec![ImportedDictionaryRecord {
                reading: "きょう".to_owned(),
                surface: "外部今日".to_owned(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            }],
        }],
    };
    let mut session = session_with_catalogs(
        Some(dictionary.path()),
        Some(test_commit_stream_config("test-imported-dictionary-engine")),
        UserDictionarySnapshot::default(),
        catalog,
        SharedVocabulary::default(),
    )
    .expect("valid imported dictionary");

    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.handle(key(" ", 49)).preedit[0].text, "外部今日");
    session.handle(key("\r", 36));
    let event: serde_json::Value = serde_json::from_slice(
        &session
            .take_commit_stream_event()
            .expect("imported dictionary commit event"),
    )
    .expect("valid JSON");
    assert_eq!(
        event["final"]["segments"][0]["origin_layers"],
        serde_json::json!(["imported"])
    );
    assert_eq!(
        event["final"]["segments"][0]["path"]["nodes"][0]["token"]["tier"],
        "imported"
    );
    assert_eq!(
        event["final"]["segments"][0]["path"]["nodes"][0]["token"]["dictionary_key"]
            .as_str()
            .unwrap()
            .len(),
        16
    );
}

// A postal candidate is recorded with the postal layer.
#[test]
fn commit_stream_marks_postal_code_candidate_authority() {
    let dictionary = commit_stream_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("test-postal-code-engine")),
        UserDictionarySnapshot::default(),
    )
    .expect("session data");
    session
        .core
        .set_postal_fixture(
            serde_json::from_value(
                serde_json::to_value(postal_code_snapshot([("1000001", "東京都千代田区千代田")]))
                    .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    for character in "1000001".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        session.handle(key(" ", 49)).preedit[0].text,
        "東京都千代田区千代田"
    );
    session.handle(key("\r", 36));
    let event: serde_json::Value = serde_json::from_slice(
        &session
            .take_commit_stream_event()
            .expect("postal commit event"),
    )
    .expect("valid JSON");
    assert_eq!(
        event["final"]["segments"][0]["origin_layers"],
        serde_json::json!(["postal"])
    );
    assert_eq!(
        event["final"]["segments"][0]["path"]["nodes"][0]["token"]["tier"],
        "postal"
    );
    assert_eq!(
        event["final"]["segments"][0]["path"]["nodes"][0]["token"]["dictionary_key"]
            .as_str()
            .unwrap()
            .len(),
        16
    );
}

// Every way of committing a conversion (Return, typing on, mode switches,
// Command-Space, force_commit…) completes exactly one record, numbered 1
// with no losses.
#[test]
fn v5_every_converted_commit_entry_completes_exactly_one_record() {
    let dictionary = commit_stream_dictionary();
    let entries = vec![
        ("return", key("\r", 36)),
        ("keypad-return", key("\r", 76)),
        ("commit", command("commit", serde_json::Value::Null)),
        (
            "force-commit",
            command("force_commit", serde_json::Value::Null),
        ),
        ("type-through", key("a", 0)),
        (
            "mode-half-alnum",
            command("set_mode", serde_json::json!({"mode":"half_alnum"})),
        ),
        (
            "mode-katakana",
            command("set_mode", serde_json::json!({"mode":"full_katakana"})),
        ),
        ("shift-literal", key_with_modifiers("A", 0, 1 << 17)),
        ("input-source-switch", key_with_modifiers(" ", 49, 1 << 20)),
        ("non-printable-passthrough", key("\t", 48)),
    ];
    for (name, request) in entries {
        let mut session = commit_stream_session(&dictionary);
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.handle(key(" ", 49));
        let actual = commit_stream::actual_segments(session.segments.as_ref().unwrap());
        let expected = actual
            .iter()
            .map(|s| s.surface.as_str())
            .collect::<String>();
        let view = session.handle(request);
        assert_eq!(
            view.commit_text.as_deref(),
            Some(expected.as_str()),
            "{name}"
        );
        let event: serde_json::Value =
            serde_json::from_slice(&session.take_commit_stream_event().expect(name)).unwrap();
        assert_eq!(event["final"]["surface"], expected, "{name}");
        assert_eq!(event["event_seq"], 1, "{name}");
        assert_eq!(event["dropped_records"], 0, "{name}");
        assert!(session.take_commit_stream_event().is_none(), "{name}");
    }
}

// Cancel, Delete and reconversion discard their draft without a loss, and
// commits that never converted produce nothing; the next record is still
// seq 1 with 0 dropped.
#[test]
fn v5_discarded_drafts_and_unconverted_commits_do_not_count_as_losses() {
    let dictionary = commit_stream_dictionary();
    for operation in ["cancel", "backspace", "restart"] {
        let mut session = commit_stream_session(&dictionary);
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.convert();
        match operation {
            "cancel" => {
                session.cancel();
            }
            "backspace" => {
                session.backspace();
            }
            _ => {
                session.convert();
            }
        }
        assert!(session.take_commit_stream_event().is_none());
        if operation != "restart" {
            session.cancel();
            for c in "kyou".chars() {
                session.handle(key(&c.to_string(), 0));
            }
            session.convert();
        }
        session.commit();
        let event: serde_json::Value =
            serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
        assert_eq!(event["event_seq"], 1);
        assert_eq!(event["dropped_records"], 0);
    }
    for mode in ["half_alnum", "full_katakana"] {
        let mut session = commit_stream_session(&dictionary);
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.handle(command("set_mode", serde_json::json!({"mode":mode})));
        assert!(session.take_commit_stream_event().is_none());
    }
    let mut session = session_with_user_dictionary(
        None,
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    assert!(session.commit().commit_text.is_some());
    assert!(session.take_commit_stream_event().is_none());
}

// Each injected record failure loses only the record: conversion, commit
// and learning go on, the Core stays valid, and the next record shows the
// gap (seq 2) and one drop.
#[test]
fn v5_local_record_failures_preserve_conversion_commit_and_learning() {
    use commit_stream::RecordingFailure::*;
    let dictionary = commit_stream_dictionary();
    for failure in [Preparation, Finalization, Serialization, SerializationPanic] {
        let mut session = commit_stream_session(&dictionary);
        session.commit_stream.as_mut().unwrap().fail_next = Some(failure);
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        let converted = session.convert();
        assert!(converted.error.is_none());
        session.select_candidate_index(1);
        assert!(!session.explicitly_selected_segment_ranges.is_empty());
        let expected = session
            .segments
            .as_ref()
            .unwrap()
            .segments()
            .iter()
            .map(|s| s.focused().surface())
            .collect::<String>();
        let committed = session.commit();
        assert!(committed.error.is_none());
        assert_eq!(committed.commit_text.as_deref(), Some(expected.as_str()));
        assert_eq!(learning_snapshot(&session).unwrap().observation_count, 1);
        assert!(!session.core.is_invalid());
        assert!(session.take_commit_stream_event().is_none());
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.convert();
        session.commit();
        let event: serde_json::Value =
            serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
        assert_eq!(event["event_seq"], 2);
        assert_eq!(event["dropped_records"], 1);
    }
}

// A failed draft that is then cancelled is not a loss: the next record is
// seq 1 with 0 dropped.
#[test]
fn v5_failed_capture_is_counted_only_if_it_is_committed() {
    let dictionary = commit_stream_dictionary();
    let mut session = commit_stream_session(&dictionary);
    session.commit_stream.as_mut().unwrap().fail_next =
        Some(commit_stream::RecordingFailure::Preparation);
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    session.cancel();
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["event_seq"], 1);
    assert_eq!(event["dropped_records"], 0);
}

// After seq u64::MAX no further record is made, but commits and learning
// continue, and the losses reach the next record from another Session.
#[test]
fn v5_sequence_exhaustion_stops_recording_without_stopping_commits() {
    let dictionary = commit_stream_dictionary();
    let mut session = commit_stream_session(&dictionary);
    session.commit_stream.as_mut().unwrap().next_event_seq = Some(u64::MAX - 1);
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["event_seq"].as_u64(), Some(u64::MAX));
    for _ in 0..2 {
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.convert();
        session.select_candidate_index(1);
        assert!(session.commit().commit_text.is_some());
        assert!(session.take_commit_stream_event().is_none());
    }
    assert_eq!(learning_snapshot(&session).unwrap().observation_count, 2);
    let mut other = session.core.create_session().unwrap();
    for c in "kyou".chars() {
        other.handle(key(&c.to_string(), 0));
    }
    other.convert();
    other.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&other.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["event_seq"], 1);
    assert_eq!(event["dropped_records"], 2);
    assert!(!other.core.is_invalid());
}

// Moves, resizes and selections set flags on one draft: the initial part
// stays the generation's, the final part is the committed segments.
#[test]
fn v5_keeps_one_draft_through_move_resize_and_selection() {
    let dictionary = multi_segment_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for c in "aiueo".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    let generated = serde_json::to_value(crate::commit_stream::actual_segments(
        session.segments.as_ref().unwrap(),
    ))
    .unwrap();
    assert!(session.segments.as_ref().unwrap().segments().len() > 1);
    session.move_segment(1);
    let old_ranges = session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|s| s.reading_range())
        .collect::<Vec<_>>();
    session.resize_segment(1);
    assert_ne!(
        old_ranges,
        session
            .segments
            .as_ref()
            .unwrap()
            .segments()
            .iter()
            .map(|s| s.reading_range())
            .collect::<Vec<_>>()
    );
    assert!(
        session
            .segments
            .as_ref()
            .unwrap()
            .focused()
            .unwrap()
            .candidates()
            .len()
            > 1
    );
    session.select_candidate_index(1);
    let actual = serde_json::to_value(crate::commit_stream::actual_segments(
        session.segments.as_ref().unwrap(),
    ))
    .unwrap();
    let committed = session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["initial"]["generation_segments"], generated);
    assert_eq!(event["final"]["surface"], committed.commit_text.unwrap());
    assert_eq!(event["final"]["candidate_selected_explicitly"], true);
    assert_eq!(event["final"]["segment_moved"], true);
    assert_eq!(event["final"]["segment_resized"], true);
    let mut final_segments = event["final"]["segments"].clone();
    assert!(
        final_segments
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["explicitly_selected"] == true)
    );
    for segment in final_segments.as_array_mut().unwrap() {
        segment
            .as_object_mut()
            .unwrap()
            .remove("explicitly_selected");
    }
    assert_eq!(final_segments, actual);
    assert!(session.take_commit_stream_event().is_none());
}

// A resize clears the learning marks but not the record's
// candidate_selected_explicitly flag.
#[test]
fn v5_ever_selected_survives_resize_but_learning_marks_do_not() {
    let dictionary = multi_segment_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for c in "aiueo".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    session.select_candidate_index(1);
    assert!(!session.explicitly_selected_segment_ranges.is_empty());
    session.resize_segment(1);
    assert!(session.explicitly_selected_segment_ranges.is_empty());
    session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["final"]["candidate_selected_explicitly"], true);
    assert!(
        event["final"]["segments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["explicitly_selected"] == false)
    );
}

// Reselecting the current candidate (by index or id) is a recorded touch
// but creates no learning mark and learns nothing.
#[test]
fn v5_accepted_reselection_is_recorded_without_creating_learning_marks() {
    let dictionary = multi_segment_dictionary();
    for by_id in [false, true] {
        let mut session = session_with_user_dictionary(
            Some(dictionary.path()),
            Some(test_commit_stream_config("unused")),
            UserDictionarySnapshot::default(),
        )
        .unwrap();
        for c in "aiueo".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.convert();
        if by_id {
            let id = session
                .segments
                .as_ref()
                .unwrap()
                .focused()
                .unwrap()
                .focused()
                .id()
                .as_str()
                .to_owned();
            session.select_candidate_id(&id);
        } else {
            session.select_candidate_index(0);
        }
        assert!(session.explicitly_selected_segment_ranges.is_empty());
        session.commit();
        let event: serde_json::Value =
            serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
        assert_eq!(event["final"]["candidate_selected_explicitly"], true);
        assert!(
            event["final"]["segments"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["explicitly_selected"] == false)
        );
        assert!(learning_snapshot(&session).is_none());
    }
}

// A selection the session refused is neither a touch nor learned.
#[test]
fn v5_rejected_selection_is_not_recorded_as_a_candidate_touch() {
    let dictionary = multi_segment_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for c in "aiueo".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    session.select_candidate_index(usize::MAX);
    session.select_candidate_id("missing-candidate");
    session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["final"]["candidate_selected_explicitly"], false);
    assert!(learning_snapshot(&session).is_none());
}

// Cycling away and back to the first candidate is a touch and keeps the
// learning mark, because a candidate did change in between.
#[test]
fn v5_candidate_cycle_keeps_touch_when_returning_to_the_first_candidate() {
    let dictionary = multi_segment_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for c in "aiueo".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    let before = session
        .segments
        .as_ref()
        .unwrap()
        .segments()
        .iter()
        .map(|s| s.focused().surface())
        .collect::<String>();
    session.cycle_candidate(1);
    session.cycle_candidate(-1);
    assert_eq!(
        session
            .segments
            .as_ref()
            .unwrap()
            .segments()
            .iter()
            .map(|s| s.focused().surface())
            .collect::<String>(),
        before
    );
    // Cycling changed a candidate, so the existing learning mark remains.
    assert!(!session.explicitly_selected_segment_ranges.is_empty());
    session.commit();
    let event: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(event["final"]["candidate_selected_explicitly"], true);
}

// Each skip reason, global or per run, reaches the record by name with
// the final segments still present.
#[test]
fn v5_records_every_skip_reason_without_requiring_a_scored_hypothesis() {
    use selection::SelectionSkipped::*;
    let dictionary = conversion_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for (reason, global) in [
        (IncompletePathEvidence, true),
        (NonKanaReading, true),
        (AuthoritativeDictionaryBaseline, false),
        (InseparableReadingBoundary, false),
        (NoAlternativeAfterConstraints, false),
        (LanguageModelNotLoaded, false),
        (LanguageModelLoading, false),
        (LanguageModelFailed, false),
        (HypothesisEnumerationFailed, false),
        (BaselineNotFound, false),
        (ScoringFailed, false),
        (HypothesisApplicationFailed, false),
    ] {
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.convert();
        let segments = session.segments.clone().unwrap();
        let mut report = session.last_selection_report.clone().unwrap();
        if global {
            report.runs.clear();
            report.skipped = Some(reason);
        } else {
            report.runs[0].skipped = Some(reason);
        }
        let now = Instant::now();
        session.commit_stream.as_mut().unwrap().begin(
            &segments,
            &segments,
            &report,
            session.conversion_snapshot.as_ref().unwrap(),
            &session.core.language_model(),
            recording::RecordingTimes {
                conversion_started: now,
                result_finished: now,
            },
        );
        session.commit();
        let event: serde_json::Value =
            serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
        let wire = &event["initial"]["selection_report"];
        assert_eq!(
            wire[if global { "skipped" } else { "rules_version" }],
            if global {
                serde_json::to_value(reason).unwrap()
            } else {
                serde_json::json!(report.rules_version)
            }
        );
        if !global {
            assert_eq!(
                wire["runs"][0]["skipped"],
                serde_json::to_value(reason).unwrap()
            );
        }
        assert!(!event["final"]["segments"].as_array().unwrap().is_empty());
    }
}

// A candidate chosen outside the 16 scored hypotheses is recorded by its
// own id and path, and the preceding commit's text never enters the record.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn v5_records_a_manual_candidate_outside_the_scored_sixteen() {
    let Some(dictionary) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let Some(model) = crate::real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
        return;
    };
    let mut session = new_session(
        Some(&dictionary),
        Some(&model),
        TestSessionConfig {
            commit_stream_config: Some(test_commit_stream_config("unused")),
            ..Default::default()
        },
    )
    .unwrap();
    // Commit prior text that must not be copied into the record.
    let context = "sentinelpreviouscontext";
    for c in context.chars() {
        session.handle(key(&c.to_string(), 0));
    }
    let previous_surface = session.handle(key("\r", 36)).commit_text.unwrap();
    for c in "koushou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    let report = session.last_selection_report.clone().unwrap();
    assert_eq!(report.runs[0].hypotheses.len(), 16);
    let focused_range = session
        .segments
        .as_ref()
        .unwrap()
        .focused()
        .unwrap()
        .reading_range();
    let scored = report
        .runs
        .iter()
        .flat_map(|r| r.hypotheses.iter())
        .map(|h| {
            h.hypothesis
                .nodes()
                .iter()
                .filter(|n| n.start >= focused_range.start && n.end <= focused_range.end)
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let candidates = session
        .segments
        .as_ref()
        .unwrap()
        .focused()
        .unwrap()
        .candidates();
    let (index, id) = candidates
        .iter()
        .enumerate()
        .find(|(_, c)| !scored.contains(&c.paths()[0].nodes))
        .map(|(i, c)| (i, c.id().as_str().to_owned()))
        .expect("candidate outside scored beam");
    session.select_candidate_index(index);
    let committed = session.commit();
    let bytes = session.take_commit_stream_event().unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(&previous_surface));
    let event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(event["final"]["segments"][0]["candidate_id"], id);
    assert_eq!(event["final"]["surface"], committed.commit_text.unwrap());
    assert_eq!(event["final"]["candidate_selected_explicitly"], true);
    assert!(
        event["initial"]["selection_report"]["runs"][0]["context_chars"]
            .as_u64()
            .unwrap()
            > 0
    );
}
// With the host not draining, the queue keeps 256 records; the two refused
// ones leave a gap, and the next accepted record reports dropped_records 2.
#[test]
fn v5_queue_loss_preserves_committed_sequence_gaps_and_next_drop_count() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        Some(test_commit_stream_config("unused")),
        UserDictionarySnapshot::default(),
    )
    .unwrap();
    for _ in 0..258 {
        for c in "kyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.convert();
        session.commit();
    }
    let mut records = Vec::new();
    while let Some(bytes) = session.take_commit_stream_event() {
        records.push(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
    }
    assert_eq!(records.len(), 256);
    assert_eq!(records.last().unwrap()["event_seq"], 256);
    for c in "kyou".chars() {
        session.handle(key(&c.to_string(), 0));
    }
    session.convert();
    session.commit();
    let next: serde_json::Value =
        serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(next["event_seq"], 259);
    assert_eq!(next["dropped_records"], 2);
}

// Two public cases that v4 lost: their switched runs are recorded with the
// actual selected segments and the report's scores, without the old
// enumeration-membership fields.
#[test]
#[ignore = "requires public dictionary, edge (check-assets)"]
fn v5_records_the_two_public_surface_representative_mismatches() {
    let Some(dictionary) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
        return;
    };
    let Some(model) = crate::real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
        return;
    };
    // AJIMEE public ids 7 and 251: v4 discarded both after matching actual
    // candidates against its separate 64-item deduplicated enumeration.
    for (reading, context) in [
        (
            "せいねんしでかくことのりゆうとして、だんせいせいねんしにおけるだんせいゆういのせいいしき（",
            "男性青年誌で描くことが多いが、男性",
        ),
        ("すうせつあるが、", "釈尊の入滅は"),
    ] {
        let mut session = new_session(
            Some(&dictionary),
            Some(&model),
            TestSessionConfig {
                commit_stream_config: Some(test_commit_stream_config("unused")),
                ..Default::default()
            },
        )
        .unwrap();
        session.composer.input_literal(reading);
        session.preceding_commit_context.observe(context);
        session.convert();
        let report = session.last_selection_report.clone().unwrap();
        assert!(report.runs.iter().any(|r| r.switched));
        let selected = serde_json::to_value(crate::commit_stream::actual_segments(
            session.segments.as_ref().unwrap(),
        ))
        .unwrap();
        let committed = session.commit();
        let event: serde_json::Value =
            serde_json::from_slice(&session.take_commit_stream_event().unwrap()).unwrap();
        assert_eq!(event["initial"]["selected_segments"], selected);
        assert_eq!(event["final"]["surface"], committed.commit_text.unwrap());
        let wire = &event["initial"]["selection_report"];
        for (run, record) in report.runs.iter().zip(wire["runs"].as_array().unwrap()) {
            assert_eq!(
                record["baseline_segments"],
                serde_json::to_value(&run.baseline_segments).unwrap()
            );
            assert_eq!(
                record["applied_segments"],
                serde_json::to_value(&run.applied_segments).unwrap()
            );
            for (h, original) in record["hypotheses"]
                .as_array()
                .unwrap()
                .iter()
                .zip(&run.hypotheses)
            {
                assert_eq!(h["score"].as_f64().unwrap() as f32, original.score);
                assert!(h.get("selections").is_none());
                let actual_nodes = original
                    .hypothesis
                    .nodes()
                    .iter()
                    .filter(|n| {
                        n.start >= run.reading_range.start && n.end <= run.reading_range.end
                    })
                    .map(crate::commit_stream::recorded_node)
                    .collect::<Vec<_>>();
                assert_eq!(
                    h["path"]["nodes"],
                    serde_json::to_value(actual_nodes).unwrap()
                );
            }
        }
        assert!(session.take_commit_stream_event().is_none());
    }
}
