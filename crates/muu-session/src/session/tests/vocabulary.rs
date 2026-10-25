use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use muu_conversion::MAX_CANDIDATE_SURFACES;

use crate::session::*;
use crate::test_support::*;
use unicode_segmentation::UnicodeSegmentation;

// An invalid User snapshot is refused before a Session exists.
#[test]
fn user_dictionary_layer_rejects_invalid_snapshots() {
    for snapshot in [
        UserDictionarySnapshot {
            schema_version: USER_DICTIONARY_SCHEMA_VERSION + 1,
            entries: Vec::new(),
        },
        user_dictionary_snapshot([("キョウ", "ユーザー今日", UserDictionaryPriority::High)]),
        user_dictionary_snapshot([
            ("きょう", "ユーザー今日", UserDictionaryPriority::High),
            ("きょう", "ユーザー今日", UserDictionaryPriority::Low),
        ]),
    ] {
        assert!(snapshot.into_overlay().is_err());
    }
}

// Grapheme counts alone are not enough: one huge grapheme exceeds the byte
// bound, and a snapshot over the total byte bound is refused.
#[test]
fn user_dictionary_rejects_byte_amplified_graphemes_and_oversized_snapshots() {
    let one_large_grapheme = format!("á{}", "\u{0301}".repeat(3_000));
    assert_eq!(one_large_grapheme.graphemes(true).count(), 1);
    let oversized_field = UserDictionarySnapshot {
        schema_version: USER_DICTIONARY_SCHEMA_VERSION,
        entries: vec![UserDictionaryRecord {
            reading: "むう".to_owned(),
            surface: one_large_grapheme,
            part_of_speech: UserDictionaryPartOfSpeech::noun(),
            priority: UserDictionaryPriority::High,
        }],
    };
    assert!(matches!(
        oversized_field.into_overlay(),
        Err(UserDictionaryError::InvalidSurface)
    ));

    let bounded_surface = format!("á{}", "\u{0301}".repeat(1_900));
    assert!(bounded_surface.len() <= MAX_USER_DICTIONARY_SURFACE_UTF8_BYTES);
    let oversized_snapshot = UserDictionarySnapshot {
        schema_version: USER_DICTIONARY_SCHEMA_VERSION,
        entries: (0..2_200)
            .map(|_| UserDictionaryRecord {
                reading: "むう".to_owned(),
                surface: bounded_surface.clone(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            })
            .collect(),
    };
    assert!(matches!(
        oversized_snapshot.into_overlay(),
        Err(UserDictionaryError::SnapshotTooLarge)
    ));
}

// User words convert alongside dictionary words, and a higher priority
// gives a lower cost.
#[test]
fn user_dictionary_entries_join_the_production_lattice_and_respect_priority() {
    let dictionary = conversion_dictionary();
    let snapshot = user_dictionary_snapshot([
        ("きょう", "今日ユーザー高", UserDictionaryPriority::High),
        ("きょう", "今日ユーザー低", UserDictionaryPriority::Low),
    ]);
    let mut session = session_with_user_dictionary(Some(dictionary.path()), None, snapshot)
        .expect("valid user dictionary");

    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let converted = session.handle(key(" ", 49));
    assert_eq!(converted.preedit[0].text, "今日ユーザー高");
    let candidates = converted.candidates.expect("candidate list");
    let surfaces = candidates
        .items
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>();
    let high = surfaces
        .iter()
        .position(|surface| *surface == "今日ユーザー高")
        .expect("high-priority user entry");
    let low = surfaces
        .iter()
        .position(|surface| *surface == "今日ユーザー低")
        .expect("low-priority user entry");
    assert!(high < low);
    assert!(surfaces.contains(&"今日"));

    session.handle(key("\u{1b}", 53));
    for character in "kyoufa".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let sentence = session.handle(key(" ", 49));
    assert_eq!(
        sentence
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "今日ユーザー高ファ"
    );
}

// A verb expands to its inflected forms and an abbreviation POS converts
// to its surface.
#[test]
fn user_dictionary_expands_inflection_and_abbreviation_pos() {
    let dictionary = conversion_dictionary();
    let snapshot = UserDictionarySnapshot {
        schema_version: USER_DICTIONARY_SCHEMA_VERSION,
        entries: vec![
            UserDictionaryRecord {
                reading: "かく".to_owned(),
                surface: "書く".to_owned(),
                part_of_speech: UserDictionaryPartOfSpeech::from_id("KA_GROUP1_VERB").unwrap(),
                priority: UserDictionaryPriority::High,
            },
            UserDictionaryRecord {
                reading: "むう".to_owned(),
                surface: "Muu略".to_owned(),
                part_of_speech: UserDictionaryPartOfSpeech::from_id("ABBREVIATION").unwrap(),
                priority: UserDictionaryPriority::High,
            },
        ],
    };
    let mut session = session_with_user_dictionary(Some(dictionary.path()), None, snapshot)
        .expect("valid full-POS user dictionary");

    for character in "kaki".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.handle(key(" ", 49)).preedit[0].text, "書き");
    session.handle(key("\u{1b}", 53));

    for character in "muu".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.handle(key(" ", 49)).preedit[0].text, "Muu略");
}

// A live imported replacement converts at once, a User word for the same
// reading outranks it and hides the imported candidate even at low
// priority, and a disabled dictionary contributes nothing.
#[test]
fn imported_dictionary_live_replace_respects_enable_and_tier_precedence() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        None,
        UserDictionarySnapshot::default(),
    )
    .expect("valid user dictionary");
    let imported = |enabled| ImportedDictionaryCatalogSnapshot {
        schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
        dictionaries: vec![ImportedDictionarySnapshot {
            dictionary_id: "example.general".to_owned(),
            enabled,
            entries: vec![ImportedDictionaryRecord {
                reading: "きょう".to_owned(),
                surface: "外部今日".to_owned(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            }],
        }],
    };
    let response = session.core.set_imported_fixture(
        serde_json::from_value(serde_json::to_value(imported(true)).unwrap()).unwrap(),
    );
    assert!(response.is_ok());
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.handle(key(" ", 49)).preedit[0].text, "外部今日");

    session.handle(key("\u{1b}", 53));
    let response = session.core.set_user_fixture(
        serde_json::from_value(
            serde_json::to_value(user_dictionary_snapshot([(
                "きょう",
                "ユーザー今日",
                UserDictionaryPriority::Low,
            )]))
            .unwrap(),
        )
        .unwrap(),
    );
    assert!(response.is_ok());
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        session.handle(key(" ", 49)).preedit[0].text,
        "ユーザー今日",
        "the explicit user tier must outrank an imported high-priority entry"
    );
    assert!(
        session
            .current_view(false)
            .candidates
            .expect("converted candidates")
            .items
            .iter()
            .all(|candidate| candidate.text != "外部今日"),
        "a matching user reading must suppress lower imported overlay nodes"
    );

    session.handle(key("\u{1b}", 53));
    let response = session.core.set_imported_fixture(
        serde_json::from_value(serde_json::to_value(imported(false)).unwrap()).unwrap(),
    );
    assert!(response.is_ok());
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let candidates = session.handle(key(" ", 49)).candidates.unwrap();
    assert!(
        candidates
            .items
            .iter()
            .all(|candidate| candidate.text != "外部今日")
    );
}

// At the candidate-surface limit every imported row stays reachable, extra
// presentation entries are appended rather than evicting one, and the view
// still fits the envelope.
#[test]
fn imported_dictionary_guard_payload_keeps_all_rows_page_reachable() {
    let dictionary = conversion_dictionary();
    let entries = (0..MAX_CANDIDATE_SURFACES)
        .map(|index| ImportedDictionaryRecord {
            reading: "きょだい".to_owned(),
            surface: format!("巨大候補{index:04}"),
            part_of_speech: UserDictionaryPartOfSpeech::noun(),
            priority: UserDictionaryPriority::Normal,
        })
        .collect();
    let snapshot = ImportedDictionaryCatalogSnapshot {
        schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
        dictionaries: vec![ImportedDictionarySnapshot {
            dictionary_id: "example.guard-payload".to_owned(),
            enabled: true,
            entries,
        }],
    };
    let mut session = session_with_catalogs(
        Some(dictionary.path()),
        None,
        UserDictionarySnapshot::default(),
        snapshot,
        SharedVocabulary::default(),
    )
    .expect("valid giant imported catalog");

    for character in "kyodai".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let converted = session.handle(key(" ", 49));
    let candidates = converted.candidates.as_ref().expect("candidate list");
    // The lattice guard still retains all 4096 imported surfaces. Supplemental
    // presentation entries are appended afterward and must not evict one of
    // those paths merely to keep the display list at the lattice's limit.
    let shown = session.segments.as_ref().unwrap().focused().unwrap();
    let ordinary = shown
        .candidates()
        .iter()
        .filter(|c| c.auxiliary().is_none())
        .collect::<Vec<_>>();
    assert_eq!(ordinary.len(), MAX_CANDIDATE_SURFACES);
    for index in 0..MAX_CANDIDATE_SURFACES {
        assert!(
            ordinary
                .iter()
                .any(|candidate| candidate.surface() == format!("巨大候補{index:04}"))
        );
    }
    let supplemental = shown
        .candidates()
        .iter()
        .filter(|c| c.auxiliary().is_some())
        .count();
    assert_eq!(supplemental, 1);
    assert_eq!(
        candidates.items.len(),
        MAX_CANDIDATE_SURFACES + supplemental
    );
    assert!(
        candidates
            .items
            .iter()
            .any(|candidate| candidate.text == "巨大候補4095"),
        "the guard must not make the final explicit catalog entry unreachable"
    );

    let payload = encode_view(&converted).expect("candidate view envelope");
    assert!(payload.len() < MAX_PROTOCOL_ENVELOPE_BYTES);
    assert!(
        payload.len() <= 2 * 1024 * 1024,
        "4096-row candidate payload unexpectedly grew to {} bytes",
        payload.len()
    );

    let last = candidates
        .items
        .iter()
        .find(|candidate| candidate.text == "巨大候補4095")
        .expect("last explicit candidate");
    let selected = session.handle(command(
        "select_candidate",
        serde_json::json!({ "id": last.id }),
    ));
    assert_eq!(selected.preedit[0].text, "巨大候補4095");
}

// Release-mode scale check: maximal surfaces keep the view under the
// envelope bound.
#[test]
#[ignore = "product-scale release gate"]
fn imported_dictionary_max_surface_payload_is_byte_bounded() {
    let dictionary = conversion_dictionary();
    // The file codec rejects ZWJ through legacy_control_character. Keep the
    // original byte amplification with an NFC surface that passes both file
    // and runtime validation, well below the 256-grapheme limit.
    let surface_prefix = format!("á{}", "\u{0301}".repeat(1_999));
    assert!(unicode_normalization::is_nfc(&surface_prefix));
    assert_eq!(surface_prefix.len(), 4_000);
    assert_eq!(surface_prefix.graphemes(true).count(), 1);
    let example_surface = format!("{surface_prefix}0000");
    assert_eq!(example_surface.len(), 4_004);
    assert_eq!(example_surface.graphemes(true).count(), 5);
    assert!(crate::user_dictionary::valid_user_dictionary_surface(
        &example_surface
    ));
    let entries = (0..MAX_CANDIDATE_SURFACES)
        .map(|index| ImportedDictionaryRecord {
            reading: "きょだい".to_owned(),
            surface: format!("{surface_prefix}{index:04}"),
            part_of_speech: UserDictionaryPartOfSpeech::noun(),
            priority: UserDictionaryPriority::Normal,
        })
        .collect();
    let snapshot = ImportedDictionaryCatalogSnapshot {
        schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
        dictionaries: vec![ImportedDictionarySnapshot {
            dictionary_id: "example.max-surface-payload".to_owned(),
            enabled: true,
            entries,
        }],
    };
    let mut session = session_with_catalogs(
        Some(dictionary.path()),
        None,
        UserDictionarySnapshot::default(),
        snapshot,
        SharedVocabulary::default(),
    )
    .expect("valid maximum-surface catalog");

    for character in "kyodai".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let converted = session.handle(key(" ", 49));
    let candidates = converted.candidates.as_ref().expect("candidate list");
    assert!(!candidates.items.is_empty());
    assert!(candidates.items.len() < MAX_CANDIDATE_SURFACES);
    assert!(
        candidates
            .items
            .iter()
            .any(|candidate| candidate.text == format!("{surface_prefix}0000"))
    );

    let payload = encode_view(&converted).expect("candidate view envelope");
    assert!(
        payload.len() <= 2 * 1024 * 1024,
        "maximum valid surfaces produced a {}-byte view",
        payload.len()
    );
}

// An imported word wins its exact reading at normal priority but never
// hijacks a longer reading that merely starts with it; priorities order
// costs high < normal < low.
#[test]
fn imported_dictionary_normal_priority_wins_exact_reading_without_leaking_to_prefixes() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        None,
        UserDictionarySnapshot::default(),
    )
    .expect("valid user dictionary");
    let snapshot = ImportedDictionaryCatalogSnapshot {
        schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
        dictionaries: vec![ImportedDictionarySnapshot {
            dictionary_id: "example.batch-c".to_owned(),
            enabled: true,
            entries: vec![
                ImportedDictionaryRecord {
                    reading: "ばっちさん".to_owned(),
                    surface: "BatchC三列".to_owned(),
                    part_of_speech: UserDictionaryPartOfSpeech::noun(),
                    priority: UserDictionaryPriority::Normal,
                },
                ImportedDictionaryRecord {
                    reading: "ばっちよん".to_owned(),
                    surface: "BatchC四列".to_owned(),
                    part_of_speech: UserDictionaryPartOfSpeech::noun(),
                    priority: UserDictionaryPriority::Normal,
                },
                ImportedDictionaryRecord {
                    reading: "ばっちじゅん".to_owned(),
                    surface: "BatchC高".to_owned(),
                    part_of_speech: UserDictionaryPartOfSpeech::noun(),
                    priority: UserDictionaryPriority::High,
                },
                ImportedDictionaryRecord {
                    reading: "ばっちじゅん".to_owned(),
                    surface: "BatchC通常".to_owned(),
                    part_of_speech: UserDictionaryPartOfSpeech::noun(),
                    priority: UserDictionaryPriority::Normal,
                },
                ImportedDictionaryRecord {
                    reading: "ばっちじゅん".to_owned(),
                    surface: "BatchC低".to_owned(),
                    part_of_speech: UserDictionaryPartOfSpeech::noun(),
                    priority: UserDictionaryPriority::Low,
                },
            ],
        }],
    };
    let response = session.core.set_imported_fixture(
        serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap(),
    );
    assert!(response.is_ok());

    for (input, expected) in [("batchisan", "BatchC三列"), ("batchiyon", "BatchC四列")] {
        for character in input.chars() {
            session.handle(key(&character.to_string(), 0));
        }
        assert!(
            session
                .current_view(false)
                .preedit
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>()
                .ends_with('n'),
            "the production input keeps the final n pending until Space"
        );
        let converted = session.handle(key(" ", 49));
        assert_eq!(converted.preedit[0].text, expected);
        assert_eq!(
            converted
                .candidates
                .expect("candidate list")
                .items
                .first()
                .expect("first candidate")
                .text,
            expected
        );
        assert_eq!(session.current_view(false).preedit[0].text, expected);
        session.handle(key("\u{1b}", 53));
    }

    for character in "batchijun".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let priority_candidates = session
        .handle(key(" ", 49))
        .candidates
        .expect("priority candidates")
        .items
        .into_iter()
        .map(|candidate| candidate.text)
        .collect::<Vec<_>>();
    let high = priority_candidates
        .iter()
        .position(|surface| surface == "BatchC高")
        .expect("high priority imported entry");
    let normal = priority_candidates
        .iter()
        .position(|surface| surface == "BatchC通常")
        .expect("normal priority imported entry");
    let low = priority_candidates
        .iter()
        .position(|surface| surface == "BatchC低")
        .expect("low priority imported entry");
    assert!(high < normal && normal < low);
    session.handle(key("\u{1b}", 53));

    for character in "batchi".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let prefix = session.handle(key(" ", 49));
    assert!(
        prefix
            .candidates
            .expect("prefix candidates")
            .items
            .iter()
            .all(|candidate| !candidate.text.starts_with("BatchC")),
        "a longer imported reading must not leak into a prefix conversion"
    );
    session.handle(key("\u{1b}", 53));

    for character in "batchisanha".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let sentence = session.handle(key(" ", 49));
    assert_eq!(
        sentence
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "BatchC三列は"
    );
    assert_eq!(
        session
            .current_view(false)
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "BatchC三列は"
    );
}

// Sessions share one imported overlay value.
#[test]
fn imported_dictionary_catalog_is_process_shared() {
    let dictionary = conversion_dictionary();
    let shared = SharedVocabulary::default();
    let snapshot = |enabled| ImportedDictionaryCatalogSnapshot {
        schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
        dictionaries: vec![ImportedDictionarySnapshot {
            dictionary_id: "example.shared".to_owned(),
            enabled,
            entries: vec![ImportedDictionaryRecord {
                reading: "きょう".to_owned(),
                surface: "共有今日".to_owned(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            }],
        }],
    };
    Core::with_vocabulary(CoreConfig::default(), shared.clone())
        .unwrap()
        .set_imported_fixture(snapshot(true))
        .expect("initial catalog");
    let mut first = session_with_shared_catalogs(
        Some(dictionary.path()),
        None,
        UserDictionarySnapshot::default(),
        shared.clone(),
    )
    .expect("first session");
    let mut second = session_with_shared_catalogs(
        Some(dictionary.path()),
        None,
        UserDictionarySnapshot::default(),
        shared.clone(),
    )
    .expect("second session");
    let (_, first_arc) = shared.imported_snapshot();
    let (_, second_arc) = shared.imported_snapshot();
    assert!(Arc::ptr_eq(&first_arc, &second_arc));

    for character in "kyou".chars() {
        first.handle(key(&character.to_string(), 0));
    }
    assert_eq!(first.handle(key(" ", 49)).preedit[0].text, "共有今日");

    first
        .core
        .set_imported_fixture(snapshot(false))
        .expect("disabled catalog");

    for character in "kyou".chars() {
        second.handle(key(&character.to_string(), 0));
    }
    let candidates = second.handle(key(" ", 49)).candidates.unwrap();
    assert!(
        candidates
            .items
            .iter()
            .all(|candidate| candidate.text != "共有今日")
    );
}

// A User word for an exact reading converts as one segment, without
// splitting or taking over longer readings.
#[test]
fn user_dictionary_exact_reading_owns_default_segmentation_without_prefix_leakage() {
    let dictionary = conversion_dictionary();
    let snapshot =
        user_dictionary_snapshot([("むうてすと", "Muu辞書", UserDictionaryPriority::High)]);
    let mut session = session_with_user_dictionary(Some(dictionary.path()), None, snapshot)
        .expect("valid user dictionary");

    for character in "muutesuto".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let exact = session.handle(key(" ", 49));
    assert_eq!(
        exact
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "Muu辞書"
    );
    assert_eq!(exact.preedit.len(), 1, "the registered span must not split");
    assert_eq!(
        exact.candidates.expect("candidate list").items[0].text,
        "Muu辞書"
    );
    assert_eq!(session.current_view(false).preedit[0].text, "Muu辞書");

    session.handle(key("\u{1b}", 53));
    for character in "muu".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let prefix = session.handle(key(" ", 49));
    assert_ne!(
        prefix
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "Muu辞書",
        "a longer registered reading must not leak into a prefix"
    );

    session.handle(key("\u{1b}", 53));
    for character in "muutesutoha".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let sentence = session.handle(key(" ", 49));
    assert_eq!(
        sentence
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "Muu辞書は",
        "the user span may compose normally with a following particle"
    );
}

// Replacing with identical content changes nothing, a new snapshot takes
// effect, and an invalid one is refused with the old layer intact.
#[test]
fn live_user_dictionary_replace_is_atomic() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_user_dictionary(
        Some(dictionary.path()),
        None,
        user_dictionary_snapshot([("きょう", "旧ユーザー今日", UserDictionaryPriority::High)]),
    )
    .expect("valid user dictionary");
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let original = session.handle(key(" ", 49));
    assert_eq!(original.preedit[0].text, "旧ユーザー今日");

    let replacement =
        user_dictionary_snapshot([("きょう", "新ユーザー今日", UserDictionaryPriority::High)]);
    let unchanged = session.core.set_user_fixture(
        serde_json::from_value(serde_json::to_value(replacement).unwrap()).unwrap(),
    );
    assert!(unchanged.is_ok());
    assert_eq!(
        session.current_view(false).preedit[0].text,
        "旧ユーザー今日"
    );

    session.handle(key("\u{1b}", 53));
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let replaced = session.handle(key(" ", 49));
    assert_eq!(replaced.preedit[0].text, "新ユーザー今日");

    let invalid = session.core.set_user_fixture(
        serde_json::from_value(serde_json::json!({"schema_version": 999, "entries": []})).unwrap(),
    );
    assert!(invalid.is_err());
    session.handle(key("\u{1b}", 53));
    for character in "kyou".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        session.handle(key(" ", 49)).preedit[0].text,
        "新ユーザー今日"
    );
}

// A seven-digit code converts as one address segment and composes with a
// following suffix.
#[test]
fn postal_code_dictionary_owns_exact_span_and_composes_with_suffix() {
    let dictionary = conversion_dictionary();
    let mut session = session_with_dictionary_path(Some(dictionary.path())).expect("session data");
    let loaded = session.core.set_postal_fixture(
        serde_json::from_value(
            serde_json::to_value(postal_code_snapshot([
                ("1000001", "東京都千代田区千代田"),
                ("1000001", "東京都千代田区丸の内"),
            ]))
            .unwrap(),
        )
        .unwrap(),
    );
    assert!(loaded.is_ok());

    for character in "1000001".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let exact = session.handle(key(" ", 49));
    assert_eq!(exact.preedit.len(), 1);
    assert_eq!(exact.preedit[0].text, "東京都千代田区千代田");
    let candidates = exact.candidates.expect("candidate list");
    assert_eq!(candidates.items[0].text, "東京都千代田区千代田");
    assert!(
        candidates
            .items
            .iter()
            .any(|candidate| candidate.text == "東京都千代田区丸の内")
    );

    session.handle(key("\u{1b}", 53));
    for character in "100-0001".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        session.handle(key(" ", 49)).preedit[0].text,
        "東京都千代田区千代田"
    );

    session.handle(key("\u{1b}", 53));
    for character in "1000001no".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    let sentence = session.handle(key(" ", 49));
    assert_eq!(
        sentence
            .preedit
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "東京都千代田区千代田の"
    );
}

// An invalid postal replacement is refused, a valid one is shared by all
// Sessions, and the User layer survives it.
#[test]
fn postal_code_live_replace_is_atomic_shared_and_user_overlay_survives() {
    let dictionary = conversion_dictionary();
    let shared_postal = SharedVocabulary::default();
    let mut session = session_with_dictionaries(
        Some(dictionary.path()),
        None,
        user_dictionary_snapshot([("むう", "Muu", UserDictionaryPriority::High)]),
        shared_postal.clone(),
    )
    .expect("valid user dictionary");
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
    let invalid = session.core.set_postal_fixture(
        serde_json::from_value(serde_json::json!({
            "schema_version": POSTAL_CODE_SCHEMA_VERSION,
            "entries": [{"reading": "1000001", "surface": ""}]
        }))
        .unwrap(),
    );
    assert!(invalid.is_err());

    let mut second_session = session_with_dictionaries(
        Some(dictionary.path()),
        None,
        UserDictionarySnapshot::default(),
        shared_postal.clone(),
    )
    .expect("second session sharing postal authority");
    for character in "1000001".chars() {
        second_session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(
        second_session.handle(key(" ", 49)).preedit[0].text,
        "東京都千代田区千代田"
    );

    let postal_before_user_replace = shared_postal.postal_snapshot().1;
    session
        .core
        .set_user_fixture(
            serde_json::from_value(
                serde_json::to_value(user_dictionary_snapshot([(
                    "むう",
                    "Muu",
                    UserDictionaryPriority::High,
                )]))
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let postal_after_user_replace = shared_postal.postal_snapshot().1;
    assert!(Arc::ptr_eq(
        &postal_before_user_replace,
        &postal_after_user_replace
    ));
    for character in "muu".chars() {
        session.handle(key(&character.to_string(), 0));
    }
    assert_eq!(session.handle(key(" ", 49)).preedit[0].text, "Muu");
}

// Holding a postal snapshot does not block a replacement on another thread,
// and the holder keeps its old revision.
#[test]
fn postal_code_snapshot_allows_a_concurrent_swap_while_operation_retains_old_authority() {
    let shared = SharedVocabulary::default();
    Core::with_vocabulary(CoreConfig::default(), shared.clone())
        .unwrap()
        .set_postal_fixture(postal_code_snapshot([("1000001", "東京都千代田区千代田")]))
        .expect("initial snapshot");
    let initial_revision = shared.postal_snapshot().0;
    let barrier = Arc::new(Barrier::new(2));
    let replacement_finished = Arc::new(AtomicBool::new(false));
    let (completed, completion) = std::sync::mpsc::channel();
    let writer = {
        let shared = shared.clone();
        let barrier = Arc::clone(&barrier);
        let replacement_finished = Arc::clone(&replacement_finished);
        std::thread::spawn(move || {
            barrier.wait();
            Core::with_vocabulary(CoreConfig::default(), shared.clone())
                .unwrap()
                .set_postal_fixture(postal_code_snapshot([(
                    "1000002",
                    "東京都千代田区皇居外苑",
                )]))
                .expect("concurrent replacement");
            replacement_finished.store(true, AtomicOrdering::Release);
            completed.send(()).expect("retaining reader is waiting");
        })
    };

    shared.with_postal_snapshot(|revision, overlay| {
        assert_eq!(revision, initial_revision);
        assert!(!overlay.is_empty());
        barrier.wait();
        // Wait inside the retaining callback: joining only afterward would
        // miss a writer blocked by this snapshot. Ten minutes is a deadlock
        // watchdog, not a latency requirement on a loaded test machine.
        completion
            .recv_timeout(Duration::from_secs(600))
            .expect("a retained snapshot must not block publication");
    });
    writer.join().expect("writer thread");
    assert!(replacement_finished.load(AtomicOrdering::Acquire));
    assert_eq!(shared.postal_snapshot().0, initial_revision + 1);
}
