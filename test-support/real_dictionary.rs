//! Read-only production dictionary fixture, shared within one test executable.
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use muu_dict::Dictionary;

pub fn shared() -> Option<Arc<Dictionary>> {
    let path = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH")?;
    Some(shared_at(&path))
}

pub fn shared_at(path: &Path) -> Arc<Dictionary> {
    static DICTIONARY: OnceLock<(PathBuf, Arc<Dictionary>)> = OnceLock::new();
    // Open/validate the pinned file once. Only immutable resources cross test
    // cases; each Core still owns independent learning, vocabulary and records.
    // Do not use this for temporary files or tests of opening/degradation.
    let (opened_path, dictionary) = DICTIONARY.get_or_init(|| {
        (
            path.to_owned(),
            Arc::new(Dictionary::open(path).expect("production dictionary fixture")),
        )
    });
    assert_eq!(
        opened_path, path,
        "real dictionary fixture path changed mid-run"
    );
    Arc::clone(dictionary)
}
