use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn shared_dictionary() -> NamedTempFile {
    let mut builder =
        DictionaryBuilder::new(2_675, 2_675, RightContextId::new(0), LeftContextId::new(0))
            .unwrap();
    builder.provenance(b"name = \"session-multi-segment-test\"\n");
    for (reading, surface, pos, cost) in [
        ("あ", "亜", CoarsePos::Noun, 0),
        ("あ", "阿", CoarsePos::Noun, 10),
        ("い", "伊", CoarsePos::Particle, 0),
        ("う", "宇", CoarsePos::Noun, 0),
        ("う", "雨", CoarsePos::Noun, 10),
        ("え", "江", CoarsePos::Noun, 0),
        ("お", "於", CoarsePos::Particle, 0),
        ("か", "加", CoarsePos::Particle, 0),
    ] {
        builder.add_entry(LexiconEntry {
            reading: reading.into(),
            surface: surface.into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: pos.id(),
            right_pos_id: pos.id(),
            word_cost: cost,
        });
    }
    builder.add_unknown(UnknownDefinition {
        class: UnknownClass::Other,
        group: false,
        max_length: 1,
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Other.id(),
        right_pos_id: CoarsePos::Other.id(),
        word_cost: 1_000,
    });
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), builder.build().unwrap()).unwrap();
    file
}

fn setup(user: UserDictionarySnapshot) -> (NamedTempFile, Arc<Core>) {
    let dictionary = shared_dictionary();
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap(),
    );
    core.initialize_fixture_data(&user, &PersonalFrequencySnapshot::default())
        .unwrap();
    (dictionary, core)
}
fn session(core: &Arc<Core>) -> Session {
    Session::new(core.clone()).unwrap()
}
fn type_and_convert(session: &mut Session, text: &str) -> ViewState {
    for ch in text.chars() {
        session.handle(key(&ch.to_string(), 0));
    }
    session.handle(key(" ", 49))
}
fn surface(view: &ViewState) -> String {
    view.preedit.iter().map(|run| run.text.as_str()).collect()
}
fn select_rain(session: &mut Session) {
    session.handle(command(
        "move_segment",
        serde_json::json!({"direction":"right"}),
    ));
    session.handle(command(
        "move_segment",
        serde_json::json!({"direction":"right"}),
    ));
    let items = session.current_view(false).candidates.unwrap().items;
    // A warmed-up model may already focus rain. Select a different candidate
    // first, so this helper always produces an actual explicit selection.
    let other = items
        .iter()
        .find(|item| item.text != "雨")
        .unwrap()
        .id
        .clone();
    let rain = items
        .iter()
        .find(|item| item.text == "雨")
        .unwrap()
        .id
        .clone();
    session.handle(command("select_candidate", serde_json::json!({"id":other})));
    session.handle(command("select_candidate", serde_json::json!({"id":rain})));
}

// A replacement mid-conversion does not reach that conversion: moves,
// resizes and the commit all keep using the captured User layer, which is
// freed only after commit; the next conversion sees the new layer.
#[test]
fn active_conversion_retains_deleted_and_reordered_user_vocabulary_through_all_operations() {
    let (_dictionary, core) = setup(user_dictionary_snapshot([
        ("あい", "UserAI", UserDictionaryPriority::High),
        ("むう", "Muu", UserDictionaryPriority::High),
    ]));
    let mut a = session(&core);
    let mut b = session(&core);
    let initial = type_and_convert(&mut a, "aiu");
    assert!(surface(&initial).contains("UserAI"));
    let original_id = initial
        .candidates
        .unwrap()
        .items
        .into_iter()
        .find(|item| item.text == "UserAI")
        .unwrap()
        .id;
    a.handle(key(" ", 49));
    a.handle(command(
        "select_candidate",
        serde_json::json!({"id":original_id}),
    ));
    assert!(!a.explicitly_selected_segment_ranges.is_empty());
    let retained = Arc::downgrade(&a.conversion_snapshot.as_ref().unwrap().vocabulary.user);
    let old_fingerprint = a
        .conversion_snapshot
        .as_ref()
        .unwrap()
        .vocabulary
        .user
        .overlay
        .fingerprint()
        .clone();
    b.core
        .set_user_fixture(user_dictionary_snapshot([(
            "むう",
            "MuuNew",
            UserDictionaryPriority::High,
        )]))
        .unwrap();
    assert_ne!(
        core.vocabulary().snapshot().user.overlay.fingerprint(),
        &old_fingerprint
    );
    a.handle(command(
        "move_segment",
        serde_json::json!({"direction":"right"}),
    ));
    a.handle(command(
        "move_segment",
        serde_json::json!({"direction":"left"}),
    ));
    let resized = a.handle(command(
        "resize_segment",
        serde_json::json!({"direction":"extend"}),
    ));
    assert!(resized.error.is_none(), "{resized:?}");
    assert!(surface(&resized).contains("UserAI"));
    let id = resized
        .candidates
        .unwrap()
        .items
        .into_iter()
        .find(|item| item.text.contains("雨"))
        .unwrap()
        .id;
    a.handle(command("select_candidate", serde_json::json!({"id":id})));
    assert_eq!(
        a.conversion_snapshot
            .as_ref()
            .unwrap()
            .vocabulary
            .user
            .overlay
            .fingerprint(),
        &old_fingerprint
    );
    assert!(retained.upgrade().is_some());
    assert!(
        a.handle(command("commit", serde_json::Value::Null))
            .commit_text
            .unwrap()
            .contains("UserAI")
    );
    assert!(retained.upgrade().is_none());
    assert_eq!(learning_snapshot(&b).unwrap().observation_count, 1);
    assert!(!surface(&type_and_convert(&mut b, "aiu")).contains("UserAI"));
}

// Session A ranks with the learning it captured while B keeps learning,
// but A's commit is added to the current learning (41 observations), not
// to its captured copy.
#[test]
fn a_retains_learning_costs_but_commits_into_current_observed_learning() {
    let (_dictionary, core) = setup(UserDictionarySnapshot::default());
    let mut a = session(&core);
    let mut b = session(&core);
    type_and_convert(&mut b, "aiu");
    select_rain(&mut b);
    b.handle(command("commit", serde_json::Value::Null));
    assert_eq!(learning_snapshot(&b).unwrap().observation_count, 1);
    type_and_convert(&mut a, "aiu");
    select_rain(&mut a);
    let captured = a.conversion_snapshot.as_ref().unwrap().learning.clone();
    for _ in 0..15 {
        type_and_convert(&mut b, "aiu");
        select_rain(&mut b);
        b.handle(command("commit", serde_json::Value::Null));
    }
    for _ in 0..24 {
        let view = type_and_convert(&mut b, "a");
        let id = view
            .candidates
            .unwrap()
            .items
            .into_iter()
            .find(|item| item.text == "阿")
            .unwrap()
            .id;
        let alternate = b
            .current_view(false)
            .candidates
            .unwrap()
            .items
            .into_iter()
            .find(|item| item.text != "阿")
            .unwrap()
            .id;
        b.handle(command(
            "select_candidate",
            serde_json::json!({"id":alternate}),
        ));
        b.handle(command("select_candidate", serde_json::json!({"id":id})));
        b.handle(command("commit", serde_json::Value::Null));
    }
    assert_eq!(
        a.conversion_snapshot.as_ref().unwrap().learning.revision,
        captured.revision
    );
    assert_eq!(
        core.conversion_snapshot().learning.generation,
        captured.generation
    );
    assert_ne!(
        muu_conversion::CostProvider::capture(&captured.frequency),
        muu_conversion::CostProvider::capture(&core.conversion_snapshot().learning.frequency),
    );
    assert!(!Arc::ptr_eq(
        &captured,
        &core.conversion_snapshot().learning
    ));
    a.handle(command(
        "move_segment",
        serde_json::json!({"direction":"left"}),
    ));
    a.handle(command(
        "move_segment",
        serde_json::json!({"direction":"right"}),
    ));
    a.handle(command(
        "move_segment",
        serde_json::json!({"direction":"left"}),
    ));
    let resized = a.handle(command(
        "resize_segment",
        serde_json::json!({"direction":"shrink"}),
    ));
    assert!(resized.error.is_none(), "{resized:?}");
    let resized = a.handle(command(
        "resize_segment",
        serde_json::json!({"direction":"extend"}),
    ));
    assert!(resized.error.is_none(), "{resized:?}");
    assert!(Arc::ptr_eq(
        &captured,
        &a.conversion_snapshot.as_ref().unwrap().learning
    ));
    select_rain(&mut a);
    a.handle(command("commit", serde_json::Value::Null));
    assert_eq!(learning_snapshot(&b).unwrap().observation_count, 41);
}

// After a clear, a commit from a conversion started before it is dropped,
// even if learning was already empty; a new conversion learns normally.
#[test]
fn clear_discards_old_conversion_observations_even_when_learning_was_empty() {
    let (_dictionary, core) = setup(UserDictionarySnapshot::default());
    let mut a = session(&core);
    let mut b = session(&core);
    type_and_convert(&mut a, "aiu");
    select_rain(&mut a);
    let generation = a.conversion_snapshot.as_ref().unwrap().learning.generation;
    b.core.clear_personal_frequency();
    assert_ne!(core.conversion_snapshot().learning.generation, generation);
    a.handle(command("commit", serde_json::Value::Null));
    assert_eq!(learning_snapshot(&b).unwrap().observation_count, 0);
    type_and_convert(&mut b, "aiu");
    select_rain(&mut b);
    b.handle(command("commit", serde_json::Value::Null));
    assert_eq!(learning_snapshot(&a).unwrap().observation_count, 1);
    assert_eq!(
        learning_snapshot(&b).unwrap(),
        learning_snapshot(&a).unwrap()
    );
}

// Two Sessions add to one shared learning model; neither overwrites the
// other.
#[test]
fn two_sessions_observations_survive_without_per_engine_models() {
    let (_dictionary, core) = setup(UserDictionarySnapshot::default());
    let mut a = session(&core);
    let mut b = session(&core);
    type_and_convert(&mut a, "aiu");
    select_rain(&mut a);
    type_and_convert(&mut b, "aiu");
    select_rain(&mut b);
    a.handle(command("commit", serde_json::Value::Null));
    b.handle(command("commit", serde_json::Value::Null));
    let snapshot = learning_snapshot(&a).unwrap();
    assert_eq!(snapshot.observation_count, 2);
    assert!(snapshot.entries.iter().any(|entry| entry.count == 2));
}

// Cancelling a conversion frees the vocabulary it captured once a
// replacement has superseded it.
#[test]
fn cancellation_releases_the_old_vocabulary_snapshot() {
    let (_dictionary, core) = setup(user_dictionary_snapshot([(
        "あい",
        "UserAI",
        UserDictionaryPriority::High,
    )]));
    let mut a = session(&core);
    type_and_convert(&mut a, "aiu");
    let old = Arc::downgrade(&a.conversion_snapshot.as_ref().unwrap().vocabulary);
    core.set_user_fixture(UserDictionarySnapshot::default())
        .unwrap();
    assert!(old.upgrade().is_some());
    a.handle(command("cancel", serde_json::Value::Null));
    assert!(old.upgrade().is_none());
}

// Creating 48 Sessions builds the 10,000-entry User layer once and shares
// it; Session creation never rebuilds or fingerprints it.
#[test]
fn forty_eight_sessions_do_not_expand_or_fingerprint_the_ten_thousand_entry_user_dictionary() {
    let digits = ['あ', 'い', 'う', 'え', 'お', 'か', 'き', 'く', 'け', 'こ'];
    let user = UserDictionarySnapshot {
        schema_version: 1,
        entries: (0..10_000)
            .map(|index| UserDictionaryRecord {
                reading: format!(
                    "むう{}",
                    index
                        .to_string()
                        .chars()
                        .map(|c| digits[c.to_digit(10).unwrap() as usize])
                        .collect::<String>()
                ),
                surface: format!("Muu{index}"),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            })
            .collect(),
    };
    let (_dictionary, core) = setup(user);
    assert_eq!(core.user_dictionary_view().entry_count.unwrap(), 10_000);
    let initial = core.vocabulary().snapshot();
    // The counter catches an expansion that is built and discarded, which the
    // pointer check below cannot.
    let builds = crate::user_dictionary::overlay_build_count_for_testing();
    let sessions = (0..48).map(|_| session(&core)).collect::<Vec<_>>();
    assert_eq!(
        crate::user_dictionary::overlay_build_count_for_testing(),
        builds,
        "Session construction must not expand and discard User overlays"
    );
    assert_eq!(core.user_dictionary_view().entry_count.unwrap(), 10_000);
    assert!(Arc::ptr_eq(
        &initial.user,
        &core.vocabulary().snapshot().user
    ));
    assert!(
        sessions
            .iter()
            .all(|session| session.conversion_snapshot.is_none())
    );
}

// A panic inside a shared publication leaves either the whole old or the
// whole new learning or vocabulary, and the other Session keeps converting.
#[test]
fn session_panics_before_or_after_publication_do_not_corrupt_the_other_session() {
    for learning in [false, true] {
        for after in [false, true] {
            let (_dictionary, core) = setup(UserDictionarySnapshot::default());
            let mut a = session(&core);
            let mut b = session(&core);
            type_and_convert(&mut a, "aiu");
            let request = if learning {
                command(
                    "panic_observe_shared_learning",
                    serde_json::json!({"after":after}),
                )
            } else {
                command(
                    "panic_replace_shared_user",
                    serde_json::json!({"after":after, "snapshot":user_dictionary_snapshot([("むう", "MuuNew", UserDictionaryPriority::High)])}),
                )
            };
            assert!(catch_unwind(AssertUnwindSafe(|| a.handle(request))).is_err());
            drop(a);
            if learning {
                let snapshot = learning_snapshot(&b);
                assert_eq!(snapshot.is_some(), after);
                if let Some(snapshot) = snapshot {
                    assert_eq!(snapshot.observation_count, 1);
                }
            } else {
                assert_eq!(
                    surface(&type_and_convert(&mut b, "muu")),
                    if after { "MuuNew" } else { "む宇" }
                );
                b.handle(command("cancel", serde_json::Value::Null));
            }
            assert!(type_and_convert(&mut b, "aiu").error.is_none());
        }
    }
}

// A record's provenance names the vocabulary and learning its conversion
// captured, even after both were replaced or cleared before the commit.
#[test]
fn commit_stream_authority_comes_from_the_retained_vocabulary_and_learning() {
    let dictionary = commit_stream_dictionary();
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap(),
    );
    core.initialize_fixture_data(
        &user_dictionary_snapshot([("むう", "OriginalUser", UserDictionaryPriority::High)]),
        &PersonalFrequencySnapshot::default(),
    )
    .unwrap();
    core.configure_sessions(CoreSessionSettings {
        commit_stream_config: Some(test_commit_stream_config("ignored")),
        ..CoreSessionSettings::default()
    })
    .unwrap_or_else(|_| panic!("configure"));
    let mut b = session(&core);
    type_and_convert(&mut b, "kyou");
    b.handle(key(" ", 49));
    b.handle(command("commit", serde_json::Value::Null));
    assert!(learning_snapshot(&b).is_some());
    // Warm-up B's completed record is independent of A's retained authority.
    while core.take_commit_stream_event().is_some() {}
    let mut a = Session::new(core.clone()).unwrap();
    type_and_convert(&mut a, "kyou");
    let expected = a
        .conversion_snapshot
        .as_ref()
        .unwrap()
        .vocabulary
        .user
        .overlay
        .fingerprint()
        .clone();
    assert!(
        !a.conversion_snapshot
            .as_ref()
            .unwrap()
            .learning
            .frequency
            .is_empty()
    );
    b.core
        .set_user_fixture(UserDictionarySnapshot::default())
        .unwrap();
    b.core.clear_personal_frequency();
    a.handle(key(" ", 49));
    a.handle(command("commit", serde_json::Value::Null));
    let record: serde_json::Value =
        serde_json::from_slice(&a.take_commit_stream_event().unwrap()).unwrap();
    assert_eq!(
        record["provenance"]["learning"]["personalization_applied"],
        true
    );
    assert_eq!(
        record["provenance"]["vocabulary"]["user"]["fingerprint"],
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(learning_snapshot(&b).unwrap().observation_count, 0);
}

// Delete back to the reading frees the captured vocabulary.
#[test]
fn backspace_to_reading_releases_the_old_vocabulary_snapshot() {
    let (_dictionary, core) = setup(user_dictionary_snapshot([(
        "あい",
        "UserAI",
        UserDictionaryPriority::High,
    )]));
    let mut a = session(&core);
    type_and_convert(&mut a, "aiu");
    let old = Arc::downgrade(&a.conversion_snapshot.as_ref().unwrap().vocabulary);
    core.set_user_fixture(UserDictionarySnapshot::default())
        .unwrap();
    assert!(old.upgrade().is_some());
    a.handle(key("", 51));
    assert!(a.segments.is_none());
    assert_eq!(a.composer.preedit().reading_hiragana, "あいう");
    assert!(old.upgrade().is_none());
}

// Converting again releases the old capture and uses the latest layer.
#[test]
fn reconvert_releases_the_previous_snapshot_and_takes_the_latest_vocabulary() {
    let (_dictionary, core) = setup(user_dictionary_snapshot([(
        "あい",
        "UserOld",
        UserDictionaryPriority::High,
    )]));
    let mut a = session(&core);
    assert!(surface(&type_and_convert(&mut a, "aiu")).contains("UserOld"));
    let old = Arc::downgrade(&a.conversion_snapshot.as_ref().unwrap().vocabulary);
    core.set_user_fixture(user_dictionary_snapshot([(
        "あい",
        "UserNew",
        UserDictionaryPriority::High,
    )]))
    .unwrap();
    let view = a.handle(command("convert", serde_json::Value::Null));
    assert!(view.error.is_none());
    assert!(surface(&view).contains("UserNew"));
    assert!(old.upgrade().is_none());
    assert!(Arc::ptr_eq(
        &a.conversion_snapshot.as_ref().unwrap().vocabulary,
        &core.vocabulary().snapshot()
    ));
}

// A failed conversion keeps no captured vocabulary.
#[test]
fn conversion_failure_releases_its_captured_vocabulary_snapshot() {
    // A readable dictionary with no unknown definition produces a genuine
    // conversion error, without a panic hook or modifying the converter.
    let mut builder =
        DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.provenance(b"name = \"conversion-failure-test\"\n");
    builder.add_entry(LexiconEntry {
        reading: "あ".into(),
        surface: "亜".into(),
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Noun.id(),
        right_pos_id: CoarsePos::Noun.id(),
        word_cost: 0,
    });
    let dictionary = NamedTempFile::new().unwrap();
    fs::write(dictionary.path(), builder.build().unwrap()).unwrap();
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap(),
    );
    assert!(core.has_dictionary());
    core.initialize_fixture_data(
        &user_dictionary_snapshot([("むう", "Muu", UserDictionaryPriority::High)]),
        &PersonalFrequencySnapshot::default(),
    )
    .unwrap();
    let old = Arc::downgrade(&core.vocabulary().snapshot());
    let mut a = session(&core);
    let failed = type_and_convert(&mut a, "a");
    assert_eq!(failed.error.as_ref().unwrap().code, "conversion_failed");
    assert!(a.conversion_snapshot.is_none());
    core.set_user_fixture(UserDictionarySnapshot::default())
        .unwrap();
    assert!(old.upgrade().is_none());
}
