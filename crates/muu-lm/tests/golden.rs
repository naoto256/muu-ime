//! Checks the CPU implementation against scores computed by the reference
//! PyTorch model. Needs the model files, which are not in the repository:
//! set `MUU_LM_MODEL_DIR` to a directory holding `model.safetensors` and
//! `tokenizer.json` (see `scripts/fetch-conversion-lm.sh`).

#[path = "../../../test-support/real_assets.rs"]
mod real_assets;

use muu_lm::{ConversionLm, ModelKind, select_for};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

struct ModelFixture(PathBuf);
impl ModelFixture {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("muu-edge-load-{}-{stamp}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for ModelFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn model_header(file: &mut fs::File) -> (u64, serde_json::Value) {
    let mut prefix = [0; 8];
    file.read_exact(&mut prefix).unwrap();
    let len = u64::from_le_bytes(prefix);
    assert!(len <= 65_536, "bounded public model header fixture");
    let mut header = vec![0; len as usize];
    file.read_exact(&mut header).unwrap();
    (8 + len, serde_json::from_slice(&header).unwrap())
}

// With the real weights, a tokenizer whose `</s>` id equals the vocabulary
// size is refused at open with a tokenizer error; an in-range id (3) opens.
#[test]
#[ignore = "requires public edge (check-assets)"]
fn edge_rejects_an_eos_outside_its_embedding_vocabulary() {
    let Some(directory) = real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
        return;
    };
    let fixture = ModelFixture::new();
    fs::copy(
        directory.join("model.safetensors"),
        fixture.0.join("model.safetensors"),
    )
    .unwrap();
    let (_, header) =
        model_header(&mut fs::File::open(fixture.0.join("model.safetensors")).unwrap());
    let vocabulary = header["model.embed_tokens.weight"]["shape"][0]
        .as_u64()
        .unwrap();
    for eos in [3, vocabulary] {
        // A self-written tokenizer isolates the EOS contract from BPE rules.
        let tokenizer = serde_json::json!({
            "version":"1.0", "truncation":null, "padding":null, "added_tokens":[],
            "normalizer":null, "pre_tokenizer":null, "post_processor":null, "decoder":null,
            "model":{"type":"WordLevel", "vocab":{"</s>":eos}, "unk_token":"</s>"}
        });
        fs::write(
            fixture.0.join("tokenizer.json"),
            serde_json::to_vec(&tokenizer).unwrap(),
        )
        .unwrap();
        let loaded = ConversionLm::open(&fixture.0);
        if eos == 3 {
            assert!(loaded.is_ok(), "valid endpoint control");
        } else {
            assert!(
                matches!(loaded, Err(muu_lm::LmError::Tokenizer(message)) if message.contains("EOS") && message.contains("vocabulary")),
                "reject EOS during load"
            );
        }
    }
}

// The real model with one extra tensor, `lm_head.weight`, given its own
// payload copied from the embedding. Every tensor the loader reads still fits,
// so only the unsupported-tensor check can refuse it.
#[test]
#[ignore = "requires public edge (check-assets)"]
fn edge_rejects_an_unsupported_untied_head() {
    let Some(directory) = real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
        return;
    };
    let fixture = ModelFixture::new();
    let mut source = fs::File::open(directory.join("model.safetensors")).unwrap();
    let (payload_start, mut header) = model_header(&mut source);
    let payload_len = source.metadata().unwrap().len() - payload_start;
    let mut head = header["model.embed_tokens.weight"].clone();
    let start = head["data_offsets"][0].as_u64().unwrap();
    let length = head["data_offsets"][1].as_u64().unwrap() - start;
    head["data_offsets"] = serde_json::json!([payload_len, payload_len + length]);
    header["lm_head.weight"] = head;
    let mut encoded = serde_json::to_vec(&header).unwrap();
    encoded.resize((encoded.len() + 7) & !7, b' ');
    let mut output = fs::File::create(fixture.0.join("model.safetensors")).unwrap();
    output
        .write_all(&(encoded.len() as u64).to_le_bytes())
        .unwrap();
    output.write_all(&encoded).unwrap();
    assert_eq!(
        std::io::copy(&mut source, &mut output).unwrap(),
        payload_len
    );
    source.seek(SeekFrom::Start(payload_start + start)).unwrap();
    assert_eq!(
        std::io::copy(&mut source.take(length), &mut output).unwrap(),
        length
    );
    drop(output);
    fs::copy(
        directory.join("tokenizer.json"),
        fixture.0.join("tokenizer.json"),
    )
    .unwrap();
    // The extra head has its own non-overlapping payload. An implementation
    // that silently ties it to embedding must not accept this variant.
    assert!(
        matches!(ConversionLm::open(&fixture.0), Err(muu_lm::LmError::Model(message)) if message.contains("unsupported tensor lm_head.weight")),
        "reject untied head during load"
    );
}

// The model opened is exactly the files on disk (both SHA-256 match), and each
// candidate has one score and one PyTorch reference NLL within 2e-3. Check
// cardinality before zip so a truncated result or reference cannot pass.
#[test]
#[ignore = "requires public edge (check-assets)"]
fn scores_match_the_reference_implementation() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/golden.json")).expect("fixture");
    let cases = cases.as_array().expect("cases");
    // The fixture holds four cases today. The floor is checked before the
    // asset lookup, so it also holds when the model is absent and the test
    // returns early: dropping a reference case fails even without the asset.
    assert!(cases.len() >= 4, "keep all independent reference cases");
    let Some(directory) = real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
        return;
    };
    let lm = ConversionLm::open(&directory).expect("conversion model");
    use sha2::{Digest, Sha256};
    let (weights, tokenizer) = lm.artifact_sha256();
    assert_eq!(
        *weights,
        <[u8; 32]>::from(Sha256::digest(
            std::fs::read(directory.join("model.safetensors")).unwrap()
        ))
    );
    assert_eq!(
        *tokenizer,
        <[u8; 32]>::from(Sha256::digest(
            std::fs::read(directory.join("tokenizer.json")).unwrap()
        ))
    );
    for case in cases {
        let surfaces: Vec<&str> = case["surfaces"]
            .as_array()
            .expect("surfaces")
            .iter()
            .map(|surface| surface.as_str().expect("surface"))
            .collect();
        let scores = lm
            .score(
                case["context"].as_str().expect("context"),
                case["reading"].as_str().expect("reading"),
                &surfaces,
            )
            .expect("score");
        let expected = case["nll"].as_array().expect("nll");
        assert_eq!(scores.len(), surfaces.len(), "one score per candidate");
        assert_eq!(
            scores.len(),
            expected.len(),
            "one reference NLL per candidate"
        );
        for (score, expected) in scores.iter().zip(expected) {
            let expected = expected.as_f64().expect("number") as f32;
            assert!(
                (score - expected).abs() < 2e-3,
                "{surfaces:?}: {score} vs {expected}"
            );
        }
    }
}

// End to end through the selection policy: with no context and 変換制度 as
// the baseline, edge switches to 変換精度 rather than keeping it or taking 返還.
// The policy is given edge's profile, matching the model that scored.
#[test]
#[ignore = "requires public edge (check-assets)"]
fn prefers_the_homophone_that_fits_the_sentence() {
    let Some(directory) = real_assets::real_asset_path("MUU_LM_MODEL_DIR") else {
        return;
    };
    let lm = ConversionLm::open(&directory).expect("conversion model");
    let surfaces = ["変換制度が悪い", "変換精度が悪い", "返還制度が悪い"];
    let scores = lm
        .score("", "へんかんせいどがわるい", &surfaces)
        .expect("score");
    assert_eq!(select_for(ModelKind::Edge, 0, &surfaces, &scores), Some(1));
}
