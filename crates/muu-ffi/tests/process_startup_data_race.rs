//! An independent process using the shipped registry, not cfg(test) state.
#[path = "support/process_core.rs"]
mod dictionary_support;
#[path = "support/shared_stores.rs"]
mod support;
use muu_dict::Dictionary;
use muu_ffi::*;
use serde_json::json;
use std::sync::Barrier;
use support::*;

// Same configuration, different startup learning: both calls succeed with the
// same report, so only the winner's learning was applied (an invalid blob
// degrades, a valid one starts at 8 observations). Later commits build on
// whichever was accepted.
#[test]
fn competing_startup_initializations_accept_only_the_winners_initial_learning() {
    let dictionary = dictionary_support::dictionary("今日");
    let identity = Dictionary::open(dictionary.path()).unwrap().fingerprint();
    let sha = identity
        .payload_sha256()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let config = json!({"dictionary_path":dictionary.path()});
    let valid = json!({"schema_version":1,"dictionary_payload_sha256":sha,"dictionary_token_count":identity.token_count(),"observation_count":8,"entries":[{"dictionary_token_id":0,"count":8}]}).to_string();
    let barrier = Barrier::new(2);
    let (bad, good) = std::thread::scope(|scope| {
        let bad = scope.spawn(|| {
            barrier.wait();
            initialize(config.clone(), Some(b"invalid"))
        });
        let good = scope.spawn(|| {
            barrier.wait();
            initialize(config.clone(), Some(valid.as_bytes()))
        });
        (bad.join().unwrap(), good.join().unwrap())
    });
    assert_eq!(bad.0, MUU_RESULT_OK);
    assert_eq!(good.0, MUU_RESULT_OK);
    assert_eq!(bad.1, good.1);
    let starting_count = if bad.1["degraded"]["personal_frequency_invalid"] == true {
        0
    } else {
        8
    };
    assert_eq!(learning().0["observation_count"], starting_count);
    let engine = create().unwrap();
    let view = convert(&engine, "kyou");
    assert_eq!(surface(&view), "今日");
    let items = view["candidates"]["items"].as_array().unwrap();
    let today = items.iter().find(|item| item["text"] == "今日").unwrap()["id"].clone();
    let other = items.iter().find(|item| item["text"] != "今日").unwrap()["id"].clone();
    event(
        &engine,
        json!({"kind":"command","command":"select_candidate","arg":{"id":other}}),
    );
    event(
        &engine,
        json!({"kind":"command","command":"select_candidate","arg":{"id":today}}),
    );
    event(
        &engine,
        json!({"kind":"command","command":"commit","arg":null}),
    );

    let snapshot = learning().0;
    assert_eq!(snapshot["observation_count"], starting_count + 1);
    assert_eq!(snapshot["entries"][0]["count"], starting_count + 1);
    cancel(&engine);
    let (status, view) = dictionary_support::create_and_convert(dictionary.path());
    assert_eq!(status, MUU_RESULT_OK);
    dictionary_support::assert_surface(&view, "今日");
}
