//! One process for same-resource creation and later cross-thread rejection.
#[path = "support/process_core.rs"]
mod support;

use muu_ffi::{MUU_RESULT_CREATE_FAILED, MUU_RESULT_OK};
use std::sync::{Arc, Barrier};

// 48 concurrent initializations with one configuration all succeed on the
// same Core; a later different dictionary path is refused as
// core_configuration_mismatch instead of replacing it.
#[test]
fn all_threads_use_the_same_production_core() {
    let dictionary = support::dictionary("今日");
    let barrier = Arc::new(Barrier::new(48));
    std::thread::scope(|scope| {
        let jobs = (0..48)
            .map(|_| {
                let barrier = barrier.clone();
                let path = dictionary.path();
                scope.spawn(move || {
                    barrier.wait();
                    support::initialize(path)
                })
            })
            .collect::<Vec<_>>();
        for job in jobs {
            let (code, view) = job.join().unwrap();
            assert_eq!(code, MUU_RESULT_OK);
            assert!(view["personal_frequency_version"].is_object());
        }
    });
    let other = support::dictionary("本日");
    std::thread::scope(|scope| {
        let (code, error) = scope
            .spawn(|| support::initialize(other.path()))
            .join()
            .unwrap();
        assert_eq!(code, MUU_RESULT_CREATE_FAILED);
        assert_eq!(error["code"], "core_configuration_mismatch");
    });
    let (code, view) = support::create_and_convert(dictionary.path());
    assert_eq!(code, MUU_RESULT_OK);
    support::assert_surface(&view, "今日");
}
