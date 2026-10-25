//! Original edge backend; scoring and tokenization stay unchanged.
use super::{LmError, MODEL_FILE, TOKENIZER_FILE, model, prompt};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokenizers::Tokenizer;
pub struct Edge {
    model: model::Model,
    tokenizer: Tokenizer,
    eos: u32,
    weights_sha256: [u8; 32],
    tokenizer_sha256: [u8; 32],
}

impl Edge {
    /// Loads `model.safetensors` and `tokenizer.json` from `directory`.
    pub fn open(directory: &Path) -> Result<Self, LmError> {
        let (model, weights_sha256) =
            model::Model::load(&directory.join(MODEL_FILE)).map_err(LmError::Model)?;
        let bytes = std::fs::read(directory.join(TOKENIZER_FILE))
            .map_err(|error| LmError::Tokenizer(error.to_string()))?;
        // Parse and fingerprint one buffer, so v5 records identify the exact
        // tokenizer used, not whatever is on disk when a conversion commits.
        let tokenizer_sha256 = Sha256::digest(&bytes).into();
        let tokenizer =
            Tokenizer::from_bytes(&bytes).map_err(|error| LmError::Tokenizer(error.to_string()))?;
        let eos = tokenizer
            .token_to_id("</s>")
            .ok_or_else(|| LmError::Tokenizer("missing </s>".to_owned()))?;
        // Candidate scoring appends EOS after the ordinary token checks.
        // Reject a mismatched tokenizer here, before it can index logits.
        // This is a consistency check between the two files, not a pin: a
        // tokenizer from another release with an in-range EOS still loads.
        if eos as usize >= model.vocabulary() {
            return Err(LmError::Tokenizer(format!(
                "EOS id {eos} is outside model vocabulary {}",
                model.vocabulary()
            )));
        }
        Ok(Self {
            model,
            tokenizer,
            eos,
            weights_sha256,
            tokenizer_sha256,
        })
    }

    /// Digests of the actual loaded artifacts (docs/src/internals/storage.md「記録」). Computing them at load
    /// time keeps recording free of file IO and hashing on the key path.
    pub fn artifact_sha256(&self) -> (&[u8; 32], &[u8; 32]) {
        (&self.weights_sha256, &self.tokenizer_sha256)
    }

    /// Negative log-likelihood of each surface as the conversion of
    /// `reading` (hiragana) after `context`. Lower is more likely.
    pub fn score(
        &self,
        context: &str,
        reading: &str,
        surfaces: &[&str],
    ) -> Result<Vec<f32>, LmError> {
        if reading.is_empty() || surfaces.is_empty() {
            return Err(LmError::EmptyInput);
        }
        let prefix = self.encode(&prompt(context, reading))?;
        let mut candidates = Vec::with_capacity(surfaces.len());
        for surface in surfaces {
            let mut ids = self.encode(surface)?;
            // Scoring the end marker keeps a truncated surface from winning
            // merely because it has fewer tokens to pay for. `eos` was checked
            // against the model vocabulary at open, so as a target it always
            // indexes a real logit row.
            ids.push(self.eos);
            candidates.push(ids);
        }
        Ok(self.model.score(&prefix, &candidates))
    }

    fn encode(&self, text: &str) -> Result<Vec<u32>, LmError> {
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|error| LmError::Tokenizer(error.to_string()))?;
        let ids = encoding.get_ids();
        if ids.iter().any(|id| *id as usize >= self.model.vocabulary()) {
            return Err(LmError::Tokenizer(
                "token outside model vocabulary".to_owned(),
            ));
        }
        Ok(ids.to_vec())
    }
}
