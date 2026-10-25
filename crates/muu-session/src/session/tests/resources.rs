use std::fs;
use std::sync::Arc;

use crate::session::*;
use crate::test_support::*;

// 48 Sessions share the Core's dictionary, romaji table, keymap and
// segmenter; the dictionary file can be removed after opening, so any
// reopen would fail.
#[test]
fn forty_eight_sessions_reuse_system_dictionary_and_tables() {
    let fixture = commit_stream_dictionary();
    let path = fixture.path().to_path_buf();
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(&path),
        })
        .expect("core resources"),
    );
    let dictionary = core.dictionary.as_ref().expect("dictionary").clone();
    let fingerprint = dictionary.fingerprint();
    fs::remove_file(&path).expect("remove dictionary pathname");
    let mut sessions = (0..48)
        .map(|index| {
            session_from_core(
                core.clone(),
                TestSessionConfig {
                    commit_stream_config: Some(test_commit_stream_config(&format!(
                        "engine-{index}"
                    ))),
                    ..TestSessionConfig::default()
                },
            )
            .expect("no resource files required by session creation")
        })
        .collect::<Vec<_>>();
    assert_eq!(Arc::strong_count(&core), 49);
    // Removing the pathname makes any accidental reopen observable (docs/src/internals/architecture.md「Core と Session」).
    // Sessions retain the Core and its immutable dictionary/table/rules;
    // constructing 48 sessions must not rebuild or fingerprint resources.
    assert_eq!(Arc::strong_count(&dictionary), 2);
    assert_eq!(Arc::strong_count(&core.romaji_table), 49);
    for session in &mut sessions {
        assert!(Arc::ptr_eq(&session.core, &core));
        assert!(Arc::ptr_eq(
            session.core.dictionary.as_ref().unwrap(),
            &dictionary
        ));
        assert!(Arc::ptr_eq(session.composer.table(), &core.romaji_table));
        assert!(std::ptr::eq(&session.core.keymap, &core.keymap));
        assert!(std::ptr::eq(&session.core.segmenter, &core.segmenter));
        assert_eq!(
            session.core.dictionary.as_ref().unwrap().fingerprint(),
            fingerprint
        );
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        assert_eq!(session.handle(key(" ", 49)).preedit[0].text, "今日");
        assert!(session.current_view(false).degraded.is_none());
    }
    assert!(!path.exists());
}

// A Core that could not open its dictionary reports kana_only in every
// Session's view.
#[test]
fn core_degradation_is_retained_by_each_new_session() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("absent.muudict");
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(&path),
        })
        .expect("kana-only core"),
    );
    for _ in 0..2 {
        let session =
            session_from_core(core.clone(), TestSessionConfig::default()).expect("session");
        assert_eq!(
            session.current_view(false).degraded,
            Some(DegradedMode::KanaOnly)
        );
    }
}
