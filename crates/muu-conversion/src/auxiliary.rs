//! Catalog-only candidates are not lattice tokens. Their identity names an
//! immutable data table, never an invented system dictionary token or cost.
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use crate::SegmentsError;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AuxiliaryKind {
    Emoticon,
    Emoji,
    SingleKanji,
}

impl AuxiliaryKind {
    pub const fn stable_name(self) -> &'static str {
        match self {
            Self::Emoticon => "emoticon",
            Self::Emoji => "emoji",
            Self::SingleKanji => "single_kanji",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuxiliaryEntry {
    kind: AuxiliaryKind,
    data_sha256: [u8; 32],
    entry_id: u32,
    surface: Arc<str>,
}

impl AuxiliaryEntry {
    // The stored surface is NFC. `data_sha256` and `entry_id` become the
    // candidate ID (`Candidate::from_auxiliary`), so the same table row keeps
    // its ID across builds only while the table bytes are unchanged.
    pub fn new(
        kind: AuxiliaryKind,
        data_sha256: [u8; 32],
        entry_id: u32,
        surface: &str,
    ) -> Result<Self, SegmentsError> {
        if surface.is_empty() || surface.len() > 4_096 || surface.chars().any(char::is_control) {
            return Err(SegmentsError::InvalidAuxiliarySurface);
        }
        let surface: String = surface.nfc().collect();
        // A few scalars expand even under NFC (for example U+0344). Bound
        // the actual displayed value as well as the supplied representation.
        if surface.len() > 4_096 {
            return Err(SegmentsError::InvalidAuxiliarySurface);
        }
        Ok(Self {
            kind,
            data_sha256,
            entry_id,
            surface: surface.into(),
        })
    }

    pub fn kind(&self) -> AuxiliaryKind {
        self.kind
    }
    pub fn data_sha256(&self) -> &[u8; 32] {
        &self.data_sha256
    }
    pub fn entry_id(&self) -> u32 {
        self.entry_id
    }
    pub fn surface(&self) -> &str {
        &self.surface
    }
}
