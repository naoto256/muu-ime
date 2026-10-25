//! This independent binary exercises startup against the shipping registry,
//! including updates that arrive before the first engine. (The test dates
//! from ABI 10; the contracts it checks still hold at ABI 13.)
//!
//! Each `tests/process_*.rs` file is its own test binary, hence its own
//! process: the Core registry is process-wide and initialized at most once,
//! so these scenarios cannot share a process.
use muu_dict::{
    CoarsePos, DictionaryBuilder, LeftContextId, LexiconEntry, RightContextId, UnknownDefinition,
};
use muu_ffi::*;
use serde_json::json;
use tempfile::NamedTempFile;

#[path = "support/dictionary_transactions.rs"]
mod dictionary_transactions;
#[path = "support/shared_stores.rs"]
mod support;
use dictionary_transactions::*;
use support::*;

#[test]
fn sessionless_core_updates_and_degraded_startup_learning_are_visible_to_new_sessions() {
    let mut builder =
        DictionaryBuilder::new(2675, 2675, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.provenance(b"name = \"shared-stores-test\"\n");
    builder.add_entry(LexiconEntry {
        reading: "きょう".into(),
        surface: "今日".into(),
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Noun.id(),
        right_pos_id: CoarsePos::Noun.id(),
        word_cost: 0,
    });
    builder.add_unknown(UnknownDefinition {
        word_cost: 10_000,
        ..UnknownDefinition::default()
    });
    let dictionary = NamedTempFile::new().unwrap();
    std::fs::write(dictionary.path(), builder.build().unwrap()).unwrap();

    // Store operations refuse calls before startup; none creates a Core.
    for view in [
        muu_core_user_dictionary_view,
        muu_core_imported_dictionary_view,
        muu_core_postal_code_view,
    ] {
        let mut error = EMPTY;
        assert_eq!(unsafe { view(&mut error) }, MUU_RESULT_CREATE_FAILED);
        assert_eq!(take(error, false)["code"], "core_not_initialized");
    }
    let user = json!({"schema_version":1,"entries":[{"reading":"むう","surface":"FirstUser","part_of_speech":"NOUN","priority":"high"}]});
    let config = json!({"dictionary_path":dictionary.path(),"user_dictionary":user});
    let (status, report) = initialize(config.clone(), Some(b"invalid learning"));
    assert_eq!(status, MUU_RESULT_OK);
    assert_eq!(report["degraded"]["personal_frequency_invalid"], true);
    let mut error = EMPTY;
    load_imported_fixture();
    load_postal_fixture();
    // A repeated initialization with the same configuration returns the
    // first report without reopening anything (the file is gone) and without
    // reapplying startup data.
    std::fs::remove_file(dictionary.path()).unwrap();
    assert_eq!(initialize(config.clone(), None).1, report);
    upsert_user("BeforeFirst");
    let first = create().unwrap();
    assert_eq!(surface(&convert(&first, "kyou")), "ImportedToday");
    cancel(&first);
    assert_eq!(surface(&convert(&first, "1000001")), "PostalAddress");
    cancel(&first);
    assert_eq!(surface(&convert(&first, "muu")), "BeforeFirst");
    cancel(&first);
    let view = convert(&first, "kyou");
    let internal_id = view["candidates"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["text"] == "今日")
        .unwrap()["id"]
        .clone();
    event(
        &first,
        json!({"kind":"command","command":"select_candidate","arg":{"id":internal_id}}),
    );
    event(
        &first,
        json!({"kind":"command","command":"commit","arg":null}),
    );
    assert_eq!(learning().0["observation_count"], 1);
    drop(first);
    // Editing and clear do not require a live session.
    upsert_user("NewUser");
    let old_version = learning().1;
    assert_eq!(unsafe { muu_core_clear_personal_frequency(&mut error) }, 0);
    let new_version = take(error, false);
    assert_ne!(old_version["generation"], new_version["generation"]);
    assert_eq!(learning().0["observation_count"], 0);
    let second = create().unwrap();
    assert_eq!(surface(&convert(&second, "muu")), "NewUser");
    cancel(&second);
    // Reinitialization cannot restore an old initial user snapshot.
    assert_eq!(initialize(config, None).1, report);
    assert_eq!(surface(&convert(&second, "muu")), "NewUser");
}
