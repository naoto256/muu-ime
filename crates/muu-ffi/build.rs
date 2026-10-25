fn main() {
    println!("cargo:rerun-if-env-changed=MUU_RUST_SOURCE_COMMIT");
    let source = std::env::var("MUU_RUST_SOURCE_COMMIT").unwrap_or_else(|_| "unknown".into());
    assert!(
        source == "unknown"
            || (source.len() == 40 && source.bytes().all(|b| b.is_ascii_hexdigit())),
        "MUU_RUST_SOURCE_COMMIT must be a full commit hash or unknown"
    );
    println!("cargo:rustc-env=MUU_RUST_SOURCE_COMMIT={source}");
}
