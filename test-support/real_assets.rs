//! Shared gate for optional developer tests and required real-asset runs.

use std::path::PathBuf;

pub fn real_asset_path(variable: &str) -> Option<PathBuf> {
    let (files, remedy): (&[&str], &str) = match variable {
        "MUU_REAL_DICT_PATH" => (&[], "Run `just dict-build` and set MUU_REAL_DICT_PATH."),
        "MUU_LM_MODEL_DIR" => (
            &["model.safetensors", "tokenizer.json"],
            "Run `scripts/fetch-conversion-lm.sh --model edge` and set MUU_LM_MODEL_DIR.",
        ),
        "MUU_ZENZ_MODEL_DIR" => (
            &["ggml-model-Q5_K_M.gguf"],
            "Run `scripts/fetch-conversion-lm.sh --model zenz` and set MUU_ZENZ_MODEL_DIR.",
        ),
        _ => panic!("unknown real asset variable: {variable}"),
    };
    let path = std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let missing = match path.as_ref() {
        None => Some(format!("{variable} is not set")),
        Some(path) if files.is_empty() && !path.is_file() => {
            Some(format!("{variable}: {} is not a file", path.display()))
        }
        Some(path) => files.iter().find_map(|file| {
            let file = path.join(file);
            (!file.is_file()).then(|| format!("{variable}: {} is not a file", file.display()))
        }),
    };
    if let Some(reason) = missing {
        if std::env::var_os("MUU_REQUIRE_REAL_ASSETS").is_some_and(|value| value == "1") {
            panic!("required real asset unavailable: {reason}. {remedy}");
        }
        eprintln!("SKIP real-asset test: {reason}. {remedy}");
        return None;
    }
    path
}
