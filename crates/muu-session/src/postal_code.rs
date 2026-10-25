//! The runtime Postal layer: seven-digit codes as readings and addresses as
//! surfaces, entered as one fixed "other" part of speech.
use muu_conversion::{LexiconOverlay, LexiconOverlayEntry, LexiconOverlayTier};
use muu_dict::{CoarsePos, LeftContextId, RightContextId};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

pub const POSTAL_CODE_SCHEMA_VERSION: u32 = 1;
const MAX_POSTAL_CODE_ENTRIES: usize = 150_000;
const MAX_POSTAL_CODE_SURFACE_UTF8_BYTES: usize = 4 * 1024;
const MAX_POSTAL_CODE_SNAPSHOT_UTF8_BYTES: usize = 12 * 1024 * 1024;
const POSTAL_CODE_RECORD_BUDGET_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostalCodeSnapshot {
    pub schema_version: u32,
    pub entries: Vec<PostalCodeRecord>,
}

impl Default for PostalCodeSnapshot {
    fn default() -> Self {
        Self {
            schema_version: POSTAL_CODE_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostalCodeRecord {
    pub reading: String,
    pub surface: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostalCodeError {
    Busy,
    UnsupportedSchema,
    TooManyEntries,
    SnapshotTooLarge,
    InvalidReading,
    InvalidSurface,
    DuplicateEntry,
    RevisionOverflow,
}

impl PostalCodeSnapshot {
    pub(crate) fn into_overlay(self) -> Result<LexiconOverlay, PostalCodeError> {
        if self.schema_version != POSTAL_CODE_SCHEMA_VERSION {
            return Err(PostalCodeError::UnsupportedSchema);
        }
        if self.entries.len() > MAX_POSTAL_CODE_ENTRIES {
            return Err(PostalCodeError::TooManyEntries);
        }
        let mut bytes = 0usize;
        let mut seen = std::collections::BTreeSet::new();
        let mut rank_by_reading = std::collections::BTreeMap::<String, i32>::new();
        let mut overlay = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.into_iter().enumerate() {
            if entry.reading.len() != 7 || !entry.reading.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(PostalCodeError::InvalidReading);
            }
            if !valid_postal_code_surface(&entry.surface) {
                return Err(PostalCodeError::InvalidSurface);
            }
            bytes = bytes
                .checked_add(entry.reading.len())
                .and_then(|value| value.checked_add(entry.surface.len()))
                .and_then(|value| value.checked_add(POSTAL_CODE_RECORD_BUDGET_BYTES))
                .ok_or(PostalCodeError::SnapshotTooLarge)?;
            if bytes > MAX_POSTAL_CODE_SNAPSHOT_UTF8_BYTES {
                return Err(PostalCodeError::SnapshotTooLarge);
            }
            if !seen.insert((entry.reading.clone(), entry.surface.clone())) {
                return Err(PostalCodeError::DuplicateEntry);
            }
            // Several addresses share a code: keep their source order by
            // making each later one slightly more expensive (up to 100).
            let rank = rank_by_reading.entry(entry.reading.clone()).or_default();
            let word_cost_bonus = 20_000 - (*rank).min(100);
            *rank += 1;
            overlay.push(LexiconOverlayEntry {
                tier: LexiconOverlayTier::PostalCode,
                dictionary_key: 0,
                entry_id: u32::try_from(index).map_err(|_| PostalCodeError::TooManyEntries)?,
                form_id: 0,
                reading: entry.reading,
                surface: entry.surface,
                left_id: LeftContextId::new(2_672),
                right_id: RightContextId::new(2_672),
                left_pos_id: CoarsePos::Other.id(),
                right_pos_id: CoarsePos::Other.id(),
                word_cost: 14_094_i32.saturating_sub(word_cost_bonus),
            });
        }
        Ok(LexiconOverlay::new(overlay))
    }
}

fn valid_postal_code_surface(surface: &str) -> bool {
    !surface.is_empty()
        && surface.len() <= MAX_POSTAL_CODE_SURFACE_UTF8_BYTES
        && surface.nfc().collect::<String>() == surface
        && !surface
            .chars()
            .any(|character| character.is_control() || matches!(character, '\t' | '\n' | '\r'))
}
