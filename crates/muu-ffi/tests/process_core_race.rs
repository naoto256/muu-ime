//! A fresh process for competing first initializations with different paths.
#[path = "support/process_core.rs"]
mod support;

use muu_ffi::{MUU_RESULT_CREATE_FAILED, MUU_RESULT_OK};
use std::sync::{Arc, Barrier};

// Two configurations race for the first initialization: exactly one wins,
// every caller with the same configuration succeeds, every other one gets
// core_configuration_mismatch, and conversion uses the winner's dictionary.
#[test]
fn only_one_dictionary_can_win_concurrent_first_initialization() {
    let first = support::dictionary("今日");
    let second = support::dictionary("本日");
    let barrier = Arc::new(Barrier::new(48));
    let results = std::thread::scope(|scope| {
        (0..48)
            .map(|index| {
                let barrier = barrier.clone();
                let path = if index % 2 == 0 {
                    first.path()
                } else {
                    second.path()
                };
                scope.spawn(move || {
                    barrier.wait();
                    support::initialize(path)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|job| job.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_ne!(results[0].0 == MUU_RESULT_OK, results[1].0 == MUU_RESULT_OK);
    let first_won = results[0].0 == MUU_RESULT_OK;
    for (index, (code, output)) in results.iter().enumerate() {
        if (index % 2 == 0) == first_won {
            assert_eq!(*code, MUU_RESULT_OK);
            assert!(output["personal_frequency_version"].is_object());
        } else {
            assert_eq!(*code, MUU_RESULT_CREATE_FAILED);
            assert_eq!(output["code"], "core_configuration_mismatch");
        }
    }
    let path = if first_won {
        first.path()
    } else {
        second.path()
    };
    let (code, view) = support::create_and_convert(path);
    assert_eq!(code, MUU_RESULT_OK);
    support::assert_surface(&view, if first_won { "今日" } else { "本日" });
}
