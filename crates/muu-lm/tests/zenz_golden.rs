//! Self-authored inputs, frozen scores from the independent FP32 reference.
#[path = "../../../test-support/real_assets.rs"]
mod real_assets;
use muu_lm::{ConversionLm, ModelKind};
use std::sync::OnceLock;

// Runs without the model (check-fast): the fixture must keep at least three
// cases with surfaces, so the asset test below cannot pass on an emptied file.
#[test]
fn reference_fixture_keeps_independent_nonempty_cases() {
    let cases = reference_cases();
    let cases = cases.as_array().unwrap();
    assert!(cases.len() >= 3, "keep all independent reference cases");
    for case in cases {
        assert!(!case["surfaces"].as_array().unwrap().is_empty());
    }
}

fn reference_cases() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/zenz-golden.json")).unwrap()
}

// The pinned zenz reports its own kind and rules version 4, and every score
// matches the FP32 reference within 0.01, with one score per surface.
#[test]
#[ignore = "requires public zenz (check-assets)"]
fn pinned_zenz_matches_reference_scores_and_identifies_its_policy() {
    let cases = reference_cases();
    let cases = cases.as_array().unwrap();
    static MODEL: OnceLock<Option<ConversionLm>> = OnceLock::new();
    let Some(model) = MODEL
        .get_or_init(|| {
            real_assets::real_asset_path("MUU_ZENZ_MODEL_DIR").map(|directory| {
                ConversionLm::open_for(ModelKind::Zenz, &directory).expect("pinned zenz model")
            })
        })
        .as_ref()
    else {
        return;
    };
    assert_eq!(model.kind(), ModelKind::Zenz);
    assert_eq!(model.kind().rules_version(), 4);
    for case in cases {
        let surfaces: Vec<_> = case["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        let scores = model
            .score(
                case["context"].as_str().unwrap(),
                case["reading"].as_str().unwrap(),
                &surfaces,
            )
            .unwrap();
        let expected = case["scores"].as_array().unwrap();
        assert_eq!(scores.len(), surfaces.len());
        assert_eq!(scores.len(), expected.len());
        for (actual, expected) in scores.iter().zip(expected) {
            assert!((*actual - expected.as_f64().unwrap() as f32).abs() <= 0.01);
        }
    }
}
