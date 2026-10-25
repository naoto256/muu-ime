//! Reading-conditioned language model used to choose among conversion
//! hypotheses the lattice has already produced.
//!
//! Fixed edge (Llama) and zenz (GPT-2) models score
//! `context, reading -> surface` pairs. This crate only scores candidates and
//! applies a conservative switching policy; it never generates text, so the
//! dictionary stays the authority on what a reading may become.

mod edge;
mod gguf;
mod model;
mod policy;
mod zenz;
mod zenz_tokenizer;
mod zenz_weights;

#[cfg(test)]
#[path = "../../../test-support/allocation_probe.rs"]
mod allocation_probe;

use std::path::Path;

pub use policy::{MIN_SWITCH_MARGIN, SELECTION_RULES_VERSION, select_for};
pub const ZENZ_MODEL_FILE: &str = "ggml-model-Q5_K_M.gguf";

/// Hypotheses scored per conversion. Measured on held-out text: 16 keeps
/// nearly all of the benefit of 64 at a third of the latency.
/// The zenz backend refuses a larger batch; edge has no cap.
pub const MAX_SCORED_HYPOTHESES: usize = 16;
/// Characters of preceding committed text given to the model.
pub const MAX_CONTEXT_CHARS: usize = 40;

// Prompt layout shared by both backends (see `prompt`): optional
// CONTEXT_MARK + context, INPUT_MARK + katakana reading, OUTPUT_MARK, then the
// scored surface and EOS. The edge tokenizer has the three marks as added
// tokens; the zenz vocabulary has none of them, so each encodes as its three
// UTF-8 byte tokens.

const CONTEXT_MARK: char = '\u{EE02}';
const INPUT_MARK: char = '\u{EE00}';
const OUTPUT_MARK: char = '\u{EE01}';
const MODEL_FILE: &str = "model.safetensors";
const TOKENIZER_FILE: &str = "tokenizer.json";

#[derive(Debug)]
pub enum LmError {
    Model(String),
    Tokenizer(String),
    EmptyInput,
}

impl std::fmt::Display for LmError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(message) => write!(formatter, "conversion model: {message}"),
            Self::Tokenizer(message) => write!(formatter, "conversion tokenizer: {message}"),
            Self::EmptyInput => formatter.write_str("conversion model: empty input"),
        }
    }
}

impl std::error::Error for LmError {}

/// The two fixed model profiles. User learning stays bound to the dictionary,
/// not this choice; only the scoring policy depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    Edge,
    Zenz,
}
impl ModelKind {
    pub fn parse(value: &str) -> Result<Self, LmError> {
        match value {
            "edge" => Ok(Self::Edge),
            "zenz" => Ok(Self::Zenz),
            _ => Err(LmError::Model("unknown model profile".into())),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Zenz => "zenz",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Edge => "kanakanji-edge-33m",
            Self::Zenz => "zenz-v3.2-small",
        }
    }
    // Zenz's rule version 4 and margin 0.5 were chosen with the zenz scorer
    // (docs/src/internals/conversion.md「言語モデルの読み込み」); edge keeps the policy constants. muu-session
    // reports the version of the model that actually scored.
    pub fn rules_version(self) -> u32 {
        match self {
            Self::Edge => policy::SELECTION_RULES_VERSION,
            Self::Zenz => 4,
        }
    }
    pub fn switch_margin(self) -> f32 {
        match self {
            Self::Edge => policy::MIN_SWITCH_MARGIN,
            Self::Zenz => 0.5,
        }
    }
}

enum Backend {
    Edge(Box<edge::Edge>),
    Zenz(Box<zenz::Zenz>),
}
pub struct ConversionLm {
    backend: Backend,
}
impl ConversionLm {
    /// Compatibility entry: the original edge directory and policy.
    pub fn open(directory: &Path) -> Result<Self, LmError> {
        Self::open_for(ModelKind::Edge, directory)
    }
    // Zenz accepts only the pinned GGUF (SHA-256 checked before parsing).
    // Edge's pins (revision and file hashes) live in
    // scripts/fetch-conversion-lm.sh; at load its SHA-256 is computed for the
    // records but not compared with any pin. What the edge loader does refuse
    // is a different architecture: a tensor outside its tied layout (an
    // untied `lm_head.weight`, say), a wrong dtype or shape, and a tokenizer
    // whose `</s>` lies outside the model vocabulary.
    pub fn open_for(kind: ModelKind, directory: &Path) -> Result<Self, LmError> {
        let backend = match kind {
            ModelKind::Edge => Backend::Edge(Box::new(edge::Edge::open(directory)?)),
            ModelKind::Zenz => Backend::Zenz(Box::new(
                zenz::Zenz::load(&directory.join(ZENZ_MODEL_FILE)).map_err(LmError::Model)?,
            )),
        };
        Ok(Self { backend })
    }
    /// Presence only; decoding and checksums remain in the one background load.
    pub fn files_present(kind: ModelKind, directory: &Path) -> bool {
        match kind {
            ModelKind::Edge => {
                directory.join(MODEL_FILE).is_file() && directory.join(TOKENIZER_FILE).is_file()
            }
            ModelKind::Zenz => directory.join(ZENZ_MODEL_FILE).is_file(),
        }
    }
    pub fn kind(&self) -> ModelKind {
        match self.backend {
            Backend::Edge(_) => ModelKind::Edge,
            Backend::Zenz(_) => ModelKind::Zenz,
        }
    }
    pub fn artifact_sha256(&self) -> (&[u8; 32], &[u8; 32]) {
        match &self.backend {
            Backend::Edge(m) => m.artifact_sha256(),
            Backend::Zenz(m) => (&m.weights_sha, &m.tokenizer_sha),
        }
    }
    /// Negative log-likelihood per surface, in the order given; lower is
    /// more likely. `reading` is hiragana. An input too long for the model's
    /// positions scores every surface as infinity rather than failing, and
    /// `select_for` never picks a non-finite score.
    pub fn score(
        &self,
        context: &str,
        reading: &str,
        surfaces: &[&str],
    ) -> Result<Vec<f32>, LmError> {
        if reading.is_empty() || surfaces.is_empty() {
            return Err(LmError::EmptyInput);
        }
        match &self.backend {
            Backend::Edge(m) => m.score(context, reading, surfaces),
            Backend::Zenz(m) => m.score(context, reading, surfaces).map_err(LmError::Model),
        }
    }
}

// Keeps the last MAX_CONTEXT_CHARS code points of `context`. muu-session
// already cuts its NFC context to the same bound and reports that count, so
// for it this cut never removes anything.
fn prompt(context: &str, reading: &str) -> String {
    let skip = context.chars().count().saturating_sub(MAX_CONTEXT_CHARS);
    let context: String = context.chars().skip(skip).collect();
    let mut prompt = String::new();
    if !context.is_empty() {
        prompt.push(CONTEXT_MARK);
        prompt.push_str(&context);
    }
    prompt.push(INPUT_MARK);
    prompt.extend(reading.chars().map(hiragana_to_katakana));
    prompt.push(OUTPUT_MARK);
    prompt
}

/// The model was trained on katakana readings.
fn hiragana_to_katakana(character: char) -> char {
    match character {
        'ぁ'..='ゖ' => char::from_u32(character as u32 + 0x60).unwrap_or(character),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_uses_katakana_reading_and_bounded_context() {
        assert_eq!(prompt("", "へんかんー"), "\u{EE00}ヘンカンー\u{EE01}");
        let long = "あ".repeat(MAX_CONTEXT_CHARS + 5) + "末尾";
        let built = prompt(&long, "よみ");
        assert!(built.starts_with('\u{EE02}'));
        assert!(built.ends_with("末尾\u{EE00}ヨミ\u{EE01}"));
        assert_eq!(built.chars().count(), 1 + MAX_CONTEXT_CHARS + 1 + 2 + 1);
    }
}

#[cfg(test)]
#[path = "../../../test-support/real_assets.rs"]
mod real_assets;
