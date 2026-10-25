//! The runtime User layer: validation of user rows and their expansion into
//! overlay entries. Suppression rows hide matching candidates instead of
//! adding any; dormant-suggestion rows are kept but not used for conversion.
use std::collections::{BTreeMap, BTreeSet};

use muu_conversion::{
    LexiconOverlay, LexiconOverlayEntry, LexiconOverlayTier, LexiconSuppression,
    MAX_CANDIDATE_PROVENANCE_PER_READING, MAX_CANDIDATE_SURFACES, UserPosBehavior,
    user_pos_definition,
};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub const USER_DICTIONARY_SCHEMA_VERSION: u32 = 1;
const MAX_USER_DICTIONARY_ENTRIES: usize = 10_000;
const MAX_USER_DICTIONARY_READING_GRAPHEMES: usize = 128;
const MAX_USER_DICTIONARY_SURFACE_GRAPHEMES: usize = 256;
pub const MAX_USER_DICTIONARY_READING_UTF8_BYTES: usize = 512;
pub const MAX_USER_DICTIONARY_SURFACE_UTF8_BYTES: usize = 4 * 1024;
pub const MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES: usize = 8 * 1024 * 1024;
const USER_DICTIONARY_RECORD_BUDGET_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDictionarySnapshot {
    pub schema_version: u32,
    pub entries: Vec<UserDictionaryRecord>,
}

impl Default for UserDictionarySnapshot {
    fn default() -> Self {
        Self {
            schema_version: USER_DICTIONARY_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDictionaryRecord {
    pub reading: String,
    pub surface: String,
    pub part_of_speech: UserDictionaryPartOfSpeech,
    pub priority: UserDictionaryPriority,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserDictionaryPartOfSpeech(String);

impl UserDictionaryPartOfSpeech {
    pub fn noun() -> Self {
        Self("NOUN".to_owned())
    }

    pub fn from_id(id: &str) -> Option<Self> {
        user_pos_definition(id).map(|definition| Self(definition.id.to_owned()))
    }

    pub fn as_id(&self) -> &str {
        if self.0 == "noun" {
            // Compatibility with legacy Swift snapshots.
            "NOUN"
        } else {
            &self.0
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserDictionaryPriority {
    High,
    Normal,
    Low,
}

impl UserDictionaryPriority {
    const fn word_cost_bonus(self) -> i32 {
        match self {
            Self::High => 12_000,
            Self::Normal => 9_000,
            Self::Low => 6_000,
        }
    }

    // Subtracted from the expanded word cost: user and imported words get a
    // tier bonus on top of their priority, so they outrank the system
    // dictionary; postal entries get the priority bonus only.
    pub(crate) const fn word_cost_bonus_for_tier(self, tier: LexiconOverlayTier) -> i32 {
        let tier_bonus = match tier {
            LexiconOverlayTier::User => 12_000,
            LexiconOverlayTier::Imported => 12_000,
            LexiconOverlayTier::PostalCode => 0,
        };
        self.word_cost_bonus().saturating_add(tier_bonus)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserDictionaryError {
    RevisionOverflow,
    UnsupportedSchema,
    TooManyEntries,
    SnapshotTooLarge,
    InvalidReading,
    InvalidSurface,
    InvalidPartOfSpeech,
    InvalidBaseForm,
    TooManyExpandedEntries,
    DuplicateEntry,
}

// This observes expansion itself, including work discarded before publication.
// Thread-local accounting isolates parallel unit tests without changing production builds.
//
// It guards a stage-2 resource contract: the User dictionary is expanded into
// an overlay once per change, into the Core's shared layer, and creating a
// Session (a process may hold about 48) must not expand it again. Comparing
// the published Arc cannot see a Session that expands a private copy and then
// drops it, since the shared layer stays the same; counting `into_overlay`
// calls can. The count is per thread, so it covers only expansion on the
// test's own thread.
#[cfg(test)]
std::thread_local! {
    static OVERLAY_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
pub(crate) fn overlay_build_count_for_testing() -> usize {
    OVERLAY_BUILDS.with(std::cell::Cell::get)
}

impl UserDictionarySnapshot {
    pub(crate) fn into_overlay(self) -> Result<LexiconOverlay, UserDictionaryError> {
        #[cfg(test)]
        OVERLAY_BUILDS.with(|count| count.set(count.get() + 1));
        if self.schema_version != USER_DICTIONARY_SCHEMA_VERSION {
            return Err(UserDictionaryError::UnsupportedSchema);
        }
        if self.entries.len() > MAX_USER_DICTIONARY_ENTRIES {
            return Err(UserDictionaryError::TooManyEntries);
        }
        let mut snapshot_bytes = 0usize;
        for entry in &self.entries {
            let definition = user_pos_definition(entry.part_of_speech.as_id())
                .ok_or(UserDictionaryError::InvalidPartOfSpeech)?;
            let is_suppression = definition.behavior == UserPosBehavior::Suppression;
            if (!entry.reading.is_empty() && !valid_user_dictionary_reading(&entry.reading))
                || (entry.reading.is_empty() && !is_suppression)
            {
                return Err(UserDictionaryError::InvalidReading);
            }
            if (!entry.surface.is_empty() && !valid_user_dictionary_surface(&entry.surface))
                || (entry.surface.is_empty() && !is_suppression)
                || (entry.reading.is_empty() && entry.surface.is_empty())
            {
                return Err(UserDictionaryError::InvalidSurface);
            }
            snapshot_bytes = snapshot_bytes
                .checked_add(entry.reading.len())
                .and_then(|size| size.checked_add(entry.surface.len()))
                .and_then(|size| size.checked_add(entry.part_of_speech.as_id().len()))
                .and_then(|size| size.checked_add(USER_DICTIONARY_RECORD_BUDGET_BYTES))
                .ok_or(UserDictionaryError::SnapshotTooLarge)?;
            if snapshot_bytes > MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES {
                return Err(UserDictionaryError::SnapshotTooLarge);
            }
        }
        let mut entries = self.entries;
        entries.sort_by(|left, right| {
            left.reading
                .cmp(&right.reading)
                .then_with(|| left.surface.cmp(&right.surface))
                .then_with(|| {
                    left.part_of_speech
                        .as_id()
                        .cmp(right.part_of_speech.as_id())
                })
        });
        // Rows are sorted, so a duplicate (reading, surface, POS) is adjacent.
        // `entry_id` below is the row's index in this sorted order.
        let mut previous: Option<(&str, &str, &str)> = None;
        let mut overlay_entries = Vec::new();
        let mut expanded_readings = BTreeMap::<String, (usize, BTreeSet<String>)>::new();
        let mut suppressions = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let identity = (
                entry.reading.as_str(),
                entry.surface.as_str(),
                entry.part_of_speech.as_id(),
            );
            if previous == Some(identity) {
                return Err(UserDictionaryError::DuplicateEntry);
            }
            previous = Some(identity);
            let definition = user_pos_definition(entry.part_of_speech.as_id())
                .ok_or(UserDictionaryError::InvalidPartOfSpeech)?;
            if definition.behavior == UserPosBehavior::DormantSuggestion {
                continue;
            }
            if definition.behavior == UserPosBehavior::Suppression {
                suppressions.push(LexiconSuppression {
                    tier: LexiconOverlayTier::User,
                    reading: (!entry.reading.is_empty()).then(|| entry.reading.clone()),
                    surface: (!entry.surface.is_empty()).then(|| entry.surface.clone()),
                });
                continue;
            }
            let expansions = definition
                .expand(&entry.reading, &entry.surface)
                .ok_or(UserDictionaryError::InvalidBaseForm)?;
            if overlay_entries
                .len()
                .checked_add(expansions.len())
                .is_none_or(|count| count > 1_000_000)
            {
                return Err(UserDictionaryError::TooManyExpandedEntries);
            }
            for expansion in expansions {
                let (provenance_count, surfaces) = expanded_readings
                    .entry(expansion.reading.clone())
                    .or_insert_with(|| (0, BTreeSet::new()));
                *provenance_count = provenance_count
                    .checked_add(1)
                    .ok_or(UserDictionaryError::TooManyExpandedEntries)?;
                if *provenance_count > MAX_CANDIDATE_PROVENANCE_PER_READING {
                    return Err(UserDictionaryError::TooManyExpandedEntries);
                }
                surfaces.insert(expansion.surface.nfc().collect());
                if surfaces.len() > MAX_CANDIDATE_SURFACES {
                    return Err(UserDictionaryError::TooManyExpandedEntries);
                }
                overlay_entries.push(LexiconOverlayEntry {
                    tier: LexiconOverlayTier::User,
                    dictionary_key: 0,
                    entry_id: u32::try_from(index)
                        .map_err(|_| UserDictionaryError::TooManyEntries)?,
                    form_id: u16::try_from(expansion.form_id)
                        .map_err(|_| UserDictionaryError::TooManyExpandedEntries)?,
                    reading: expansion.reading,
                    surface: expansion.surface,
                    left_id: expansion.left_id,
                    right_id: expansion.right_id,
                    left_pos_id: expansion.left_pos.id(),
                    right_pos_id: expansion.right_pos.id(),
                    word_cost: expansion.word_cost.saturating_sub(
                        entry
                            .priority
                            .word_cost_bonus_for_tier(LexiconOverlayTier::User),
                    ),
                });
            }
        }
        Ok(LexiconOverlay::with_suppressions(
            overlay_entries,
            suppressions,
        ))
    }
}

pub(crate) fn valid_user_dictionary_reading(reading: &str) -> bool {
    if reading.is_empty()
        || reading.len() > MAX_USER_DICTIONARY_READING_UTF8_BYTES
        || reading.nfc().collect::<String>() != reading
        || reading.graphemes(true).count() > MAX_USER_DICTIONARY_READING_GRAPHEMES
    {
        return false;
    }
    reading
        .chars()
        .all(|character| matches!(character, '\u{3041}'..='\u{3096}' | 'ゝ' | 'ゞ' | 'ー'))
}

pub(crate) fn valid_user_dictionary_surface(surface: &str) -> bool {
    !surface.is_empty()
        && surface.len() <= MAX_USER_DICTIONARY_SURFACE_UTF8_BYTES
        && surface.nfc().collect::<String>() == surface
        && surface.graphemes(true).count() <= MAX_USER_DICTIONARY_SURFACE_GRAPHEMES
        && !surface
            .chars()
            .any(|character| character.is_control() || matches!(character, '\t' | '\n' | '\r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_reading_cannot_expand_beyond_the_candidate_guard() {
        let entries = (0..=MAX_CANDIDATE_SURFACES)
            .map(|index| UserDictionaryRecord {
                reading: "きょだい".to_owned(),
                surface: format!("語{index:04}"),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::Normal,
            })
            .collect();
        let snapshot = UserDictionarySnapshot {
            schema_version: USER_DICTIONARY_SCHEMA_VERSION,
            entries,
        };

        assert_eq!(
            snapshot.into_overlay(),
            Err(UserDictionaryError::TooManyExpandedEntries)
        );
    }
}
