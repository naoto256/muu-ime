//! The runtime Imported layer: validation and expansion of imported rows into
//! one overlay from file-based runtime parts.
use std::collections::{BTreeMap, BTreeSet};

use muu_conversion::{
    LexiconOverlay, LexiconOverlayEntry, LexiconOverlayTier, LexiconSuppression,
    MAX_CANDIDATE_PROVENANCE_PER_READING, MAX_CANDIDATE_SURFACES, UserPosBehavior,
    user_pos_definition,
};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::user_dictionary::{
    UserDictionaryPartOfSpeech, UserDictionaryPriority, valid_user_dictionary_reading,
    valid_user_dictionary_surface,
};

pub const IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION: u32 = 1;
pub const MAX_IMPORTED_DICTIONARIES: usize = 32;
const MAX_SOURCE_ENTRIES: usize = 250_000;
const MAX_EXPANDED_ENTRIES: usize = 1_000_000;
const MAX_SNAPSHOT_UTF8_BYTES: usize = 32 * 1024 * 1024;
const RECORD_BUDGET_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedDictionaryCatalogSnapshot {
    pub schema_version: u32,
    pub dictionaries: Vec<ImportedDictionarySnapshot>,
}

impl Default for ImportedDictionaryCatalogSnapshot {
    fn default() -> Self {
        Self {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedDictionarySnapshot {
    pub dictionary_id: String,
    pub enabled: bool,
    pub entries: Vec<ImportedDictionaryRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedDictionaryRecord {
    pub reading: String,
    pub surface: String,
    pub part_of_speech: UserDictionaryPartOfSpeech,
    pub priority: UserDictionaryPriority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedDictionaryError {
    Busy,
    UnsupportedSchema,
    TooManyDictionaries,
    TooManyEntries,
    TooManyExpandedEntries,
    SnapshotTooLarge,
    RevisionOverflow,
    InvalidDictionaryId,
    DuplicateDictionary,
    DuplicateEntry,
    InvalidReading,
    InvalidSurface,
    InvalidPartOfSpeech,
    InvalidBaseForm,
}

/// Borrowed, per-dictionary execution data let the loader reuse unchanged
/// parts without retaining disabled rows or flattening/copying all source blobs.
#[derive(Clone, Copy)]
pub(crate) struct ImportedDictionaryInput<'a> {
    pub dictionary_id: &'a str,
    pub enabled: bool,
    pub entries: &'a [ImportedDictionaryRecord],
}

// Synthetic catalogs exercise the same production builder. ABI 13 no longer
// accepts a whole runtime snapshot; persisted legacy files are decoded in the
// load transaction instead.
#[cfg(test)]
impl ImportedDictionaryCatalogSnapshot {
    pub(crate) fn into_overlay(self) -> Result<LexiconOverlay, ImportedDictionaryError> {
        if self.schema_version != IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION {
            return Err(ImportedDictionaryError::UnsupportedSchema);
        }
        let inputs: Vec<_> = self
            .dictionaries
            .iter()
            .map(|dictionary| ImportedDictionaryInput {
                dictionary_id: &dictionary.dictionary_id,
                enabled: dictionary.enabled,
                entries: &dictionary.entries,
            })
            .collect();
        imported_overlay_from_parts(&inputs)
    }
}

/// Stable sorting, IDs, expansion and bounds for the enabled runtime parts.
/// Ownership and file metadata stay in the individual load/edit transaction.
pub(crate) fn imported_overlay_from_parts(
    inputs: &[ImportedDictionaryInput<'_>],
) -> Result<LexiconOverlay, ImportedDictionaryError> {
    if inputs.len() > MAX_IMPORTED_DICTIONARIES {
        return Err(ImportedDictionaryError::TooManyDictionaries);
    }
    let mut dictionaries = inputs.to_vec();
    dictionaries.sort_by(|left, right| left.dictionary_id.cmp(right.dictionary_id));
    let mut dictionary_ids = BTreeSet::new();
    let mut dictionary_keys = BTreeSet::new();
    let mut source_count = 0usize;
    for dictionary in &dictionaries {
        if !valid_dictionary_id(dictionary.dictionary_id) {
            return Err(ImportedDictionaryError::InvalidDictionaryId);
        }
        if !dictionary_ids.insert(dictionary.dictionary_id) {
            return Err(ImportedDictionaryError::DuplicateDictionary);
        }
        if !dictionary_keys.insert(opaque_dictionary_key(dictionary.dictionary_id)) {
            return Err(ImportedDictionaryError::DuplicateDictionary);
        }
        source_count = source_count
            .checked_add(dictionary.entries.len())
            .ok_or(ImportedDictionaryError::TooManyEntries)?;
        if source_count > MAX_SOURCE_ENTRIES {
            return Err(ImportedDictionaryError::TooManyEntries);
        }
    }
    validate_active_expanded_reading_bound(&dictionaries)?;

    let mut snapshot_bytes = 0usize;
    // One counter across every dictionary, in id order and counting only
    // rows that reach the overlay. A row's entry_id therefore shifts when
    // another dictionary is enabled, disabled or removed.
    let mut next_entry_id = 0u32;
    let mut overlay_entries = Vec::new();
    let mut suppressions = Vec::new();

    for dictionary in dictionaries {
        let dictionary_key = opaque_dictionary_key(dictionary.dictionary_id);
        snapshot_bytes = snapshot_bytes
            .checked_add(dictionary.dictionary_id.len())
            .and_then(|size| size.checked_add(RECORD_BUDGET_BYTES))
            .ok_or(ImportedDictionaryError::SnapshotTooLarge)?;

        let mut entries: Vec<_> = dictionary.entries.iter().collect();
        entries.sort_by(|left, right| compare_imported_records(left, right));
        let mut identities = BTreeSet::new();
        for entry in entries {
            let definition = user_pos_definition(entry.part_of_speech.as_id())
                .ok_or(ImportedDictionaryError::InvalidPartOfSpeech)?;
            let is_suppression = definition.behavior == UserPosBehavior::Suppression;
            if (!entry.reading.is_empty() && !valid_user_dictionary_reading(&entry.reading))
                || (entry.reading.is_empty() && !is_suppression)
            {
                return Err(ImportedDictionaryError::InvalidReading);
            }
            if (!entry.surface.is_empty() && !valid_user_dictionary_surface(&entry.surface))
                || (entry.surface.is_empty() && !is_suppression)
                || (entry.reading.is_empty() && entry.surface.is_empty())
            {
                return Err(ImportedDictionaryError::InvalidSurface);
            }
            snapshot_bytes = snapshot_bytes
                .checked_add(entry.reading.len())
                .and_then(|size| size.checked_add(entry.surface.len()))
                .and_then(|size| size.checked_add(entry.part_of_speech.as_id().len()))
                .and_then(|size| size.checked_add(RECORD_BUDGET_BYTES))
                .ok_or(ImportedDictionaryError::SnapshotTooLarge)?;
            if snapshot_bytes > MAX_SNAPSHOT_UTF8_BYTES {
                return Err(ImportedDictionaryError::SnapshotTooLarge);
            }
            if !identities.insert((
                entry.reading.clone(),
                entry.surface.clone(),
                entry.part_of_speech.as_id().to_owned(),
            )) {
                return Err(ImportedDictionaryError::DuplicateEntry);
            }

            let expansions = definition
                .expand(&entry.reading, &entry.surface)
                .ok_or(ImportedDictionaryError::InvalidBaseForm)?;
            if !dictionary.enabled || definition.behavior == UserPosBehavior::DormantSuggestion {
                continue;
            }
            if definition.behavior == UserPosBehavior::Suppression {
                suppressions.push(LexiconSuppression {
                    tier: LexiconOverlayTier::Imported,
                    reading: (!entry.reading.is_empty()).then(|| entry.reading.clone()),
                    surface: (!entry.surface.is_empty()).then(|| entry.surface.clone()),
                });
                continue;
            }
            if overlay_entries
                .len()
                .checked_add(expansions.len())
                .is_none_or(|count| count > MAX_EXPANDED_ENTRIES)
            {
                return Err(ImportedDictionaryError::TooManyExpandedEntries);
            }
            let entry_id = next_entry_id;
            next_entry_id = next_entry_id
                .checked_add(1)
                .ok_or(ImportedDictionaryError::TooManyEntries)?;
            for expansion in expansions {
                overlay_entries.push(LexiconOverlayEntry {
                    tier: LexiconOverlayTier::Imported,
                    dictionary_key,
                    entry_id,
                    form_id: u16::try_from(expansion.form_id)
                        .map_err(|_| ImportedDictionaryError::TooManyExpandedEntries)?,
                    reading: expansion.reading,
                    surface: expansion.surface,
                    left_id: expansion.left_id,
                    right_id: expansion.right_id,
                    left_pos_id: expansion.left_pos.id(),
                    right_pos_id: expansion.right_pos.id(),
                    word_cost: expansion.word_cost.saturating_sub(
                        entry
                            .priority
                            .word_cost_bonus_for_tier(LexiconOverlayTier::Imported),
                    ),
                });
            }
        }
    }
    Ok(LexiconOverlay::with_suppressions(
        overlay_entries,
        suppressions,
    ))
}

fn validate_active_expanded_reading_bound(
    dictionaries: &[ImportedDictionaryInput<'_>],
) -> Result<(), ImportedDictionaryError> {
    let mut expanded_count = 0usize;
    let mut expanded_readings = BTreeMap::<String, (usize, BTreeSet<String>)>::new();
    for dictionary in dictionaries.iter().filter(|dictionary| dictionary.enabled) {
        for entry in dictionary.entries {
            let definition = user_pos_definition(entry.part_of_speech.as_id())
                .ok_or(ImportedDictionaryError::InvalidPartOfSpeech)?;
            let is_suppression = definition.behavior == UserPosBehavior::Suppression;
            if (!entry.reading.is_empty() && !valid_user_dictionary_reading(&entry.reading))
                || (entry.reading.is_empty() && !is_suppression)
            {
                return Err(ImportedDictionaryError::InvalidReading);
            }
            if (!entry.surface.is_empty() && !valid_user_dictionary_surface(&entry.surface))
                || (entry.surface.is_empty() && !is_suppression)
                || (entry.reading.is_empty() && entry.surface.is_empty())
            {
                return Err(ImportedDictionaryError::InvalidSurface);
            }
            let expansions = definition
                .expand(&entry.reading, &entry.surface)
                .ok_or(ImportedDictionaryError::InvalidBaseForm)?;
            if matches!(
                definition.behavior,
                UserPosBehavior::DormantSuggestion | UserPosBehavior::Suppression
            ) {
                continue;
            }
            expanded_count = expanded_count
                .checked_add(expansions.len())
                .ok_or(ImportedDictionaryError::TooManyExpandedEntries)?;
            if expanded_count > MAX_EXPANDED_ENTRIES {
                return Err(ImportedDictionaryError::TooManyExpandedEntries);
            }
            for expansion in expansions {
                let (provenance_count, surfaces) = expanded_readings
                    .entry(expansion.reading)
                    .or_insert_with(|| (0, BTreeSet::new()));
                *provenance_count = provenance_count
                    .checked_add(1)
                    .ok_or(ImportedDictionaryError::TooManyExpandedEntries)?;
                if *provenance_count > MAX_CANDIDATE_PROVENANCE_PER_READING {
                    return Err(ImportedDictionaryError::TooManyExpandedEntries);
                }
                surfaces.insert(expansion.surface.nfc().collect());
                if surfaces.len() > MAX_CANDIDATE_SURFACES {
                    return Err(ImportedDictionaryError::TooManyExpandedEntries);
                }
            }
        }
    }
    Ok(())
}

fn compare_imported_records(
    left: &ImportedDictionaryRecord,
    right: &ImportedDictionaryRecord,
) -> std::cmp::Ordering {
    #[cfg(test)]
    ENTRY_SORT_COMPARISONS.with(|count| count.set(count.get() + 1));
    left.reading
        .cmp(&right.reading)
        .then_with(|| left.surface.cmp(&right.surface))
        .then_with(|| {
            left.part_of_speech
                .as_id()
                .cmp(right.part_of_speech.as_id())
        })
}

#[cfg(test)]
thread_local! {
    static ENTRY_SORT_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Stricter than the file codec's `readable_dictionary_id`: no uppercase. A
// legacy uppercase id stays listed and removable but cannot become live.
fn valid_dictionary_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

pub(crate) fn opaque_dictionary_key(id: &str) -> u64 {
    // Stable FNV-1a keeps the catalog identifier out of candidate/telemetry
    // payloads while preserving deterministic provenance. Collisions inside
    // one bounded catalog are rejected above.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // The User and Postal layers use key zero; an imported key never does.
    hash.max(1)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn record(reading: &str, surface: &str, pos: &str) -> ImportedDictionaryRecord {
        ImportedDictionaryRecord {
            reading: reading.to_owned(),
            surface: surface.to_owned(),
            part_of_speech: UserDictionaryPartOfSpeech::from_id(pos).unwrap(),
            priority: UserDictionaryPriority::Normal,
        }
    }

    #[test]
    fn stored_and_runtime_catalogs_share_the_dictionary_count_bound() {
        use crate::imported_dictionary_catalog_file::{
            decode_imported_dictionary_manifest, decode_legacy_imported_catalog,
        };

        for count in [32, 33] {
            let dictionaries: Vec<_> = (0..count)
                .map(|i| ImportedDictionarySnapshot {
                    dictionary_id: format!("example.{i}"),
                    enabled: false,
                    entries: vec![],
                })
                .collect();
            let runtime = ImportedDictionaryCatalogSnapshot {
                schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
                dictionaries,
            }
            .into_overlay();
            let rows: Vec<_> = (0..count)
                .map(|i| serde_json::json!({
                    "metadata": {"id": format!("example.{i}"), "name": "Example", "version": "1",
                        "author": "Example", "license": "MIT", "provenance": "synthetic"},
                    "enabled": false, "entries": [], "imported_at_unix_ms": 0,
                    "source_sha256": "", "blob_sha256": "0".repeat(64), "order": i,
                }))
                .collect();
            let manifest = serde_json::to_vec(&serde_json::json!({
                "schema_version": 2, "dictionaries": rows,
            }))
            .unwrap();
            let legacy = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1, "dictionaries": rows,
            }))
            .unwrap();
            let manifest = decode_imported_dictionary_manifest(&manifest);
            let legacy = decode_legacy_imported_catalog(&legacy, 1);
            if count == 32 {
                assert!(runtime.unwrap().is_empty());
                assert_eq!(manifest.unwrap().dictionaries.len(), 32);
                assert_eq!(legacy.unwrap().len(), 32);
            } else {
                assert_eq!(
                    runtime.err(),
                    Some(ImportedDictionaryError::TooManyDictionaries)
                );
                assert_eq!(manifest.err().unwrap().code, "invalid_catalog");
                assert_eq!(legacy.err().unwrap().code, "invalid_catalog");
            }
        }
    }

    #[test]
    fn disabled_and_dormant_entries_validate_without_runtime_nodes() {
        let catalog = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: vec![ImportedDictionarySnapshot {
                dictionary_id: "example.words".to_owned(),
                enabled: false,
                entries: vec![record("かく", "書く", "KA_GROUP1_VERB")],
            }],
        };
        assert!(catalog.into_overlay().unwrap().is_empty());

        let dormant = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: vec![ImportedDictionarySnapshot {
                dictionary_id: "example.suggestions".to_owned(),
                enabled: true,
                entries: vec![record("むう", "Muu候補", "SUGGESTION_ONLY")],
            }],
        };
        assert!(dormant.into_overlay().unwrap().is_empty());
    }

    #[test]
    fn invalid_inflection_is_rejected_even_while_dictionary_is_disabled() {
        let catalog = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: vec![ImportedDictionarySnapshot {
                dictionary_id: "example.invalid".to_owned(),
                enabled: false,
                entries: vec![record("かける", "書ける", "KA_GROUP1_VERB")],
            }],
        };
        assert_eq!(
            catalog.into_overlay(),
            Err(ImportedDictionaryError::InvalidBaseForm)
        );
    }

    #[test]
    fn active_catalog_rejects_more_than_the_candidate_guard_for_one_reading() {
        let entries = (0..=MAX_CANDIDATE_SURFACES)
            .map(|index| record("きょだい", &format!("語{index:04}"), "NOUN"))
            .collect();
        let catalog = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: vec![ImportedDictionarySnapshot {
                dictionary_id: "example.concentrated".to_owned(),
                enabled: true,
                entries,
            }],
        };

        assert_eq!(
            catalog.into_overlay(),
            Err(ImportedDictionaryError::TooManyExpandedEntries)
        );
    }

    #[test]
    fn active_catalog_counts_nfc_surface_groups_across_dictionaries() {
        ENTRY_SORT_COMPARISONS.with(|count| count.set(0));
        let count = MAX_CANDIDATE_SURFACES / 2 + 1;
        let first = (0..count)
            .map(|index| record("きょだい", &format!("語{index:04}"), "NOUN"))
            .collect();
        let second = (0..count)
            .map(|index| record("きょだい", &format!("語{index:04}"), "NOUN"))
            .collect();
        let catalog = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: vec![
                ImportedDictionarySnapshot {
                    dictionary_id: "example.first".to_owned(),
                    enabled: true,
                    entries: first,
                },
                ImportedDictionarySnapshot {
                    dictionary_id: "example.second".to_owned(),
                    enabled: true,
                    entries: second,
                },
            ],
        };

        let overlay = catalog
            .into_overlay()
            .expect("duplicate NFC surfaces from distinct dictionaries are valid provenance");
        assert!(!overlay.is_empty());
    }

    #[test]
    fn active_catalog_bounds_provenance_separately_from_surface_groups() {
        ENTRY_SORT_COMPARISONS.with(|count| count.set(0));
        let part_of_speech_ids = [
            "NOUN",
            "PROPER_NOUN",
            "PLACE_NAME",
            "ADVERB",
            "PRENOUN_ADJECTIVAL",
        ];
        let entries = part_of_speech_ids
            .into_iter()
            .flat_map(|part_of_speech| {
                (0..MAX_CANDIDATE_SURFACES).map(move |index| ImportedDictionaryRecord {
                    reading: "きょだい".to_owned(),
                    surface: format!("語{index:04}"),
                    part_of_speech: UserDictionaryPartOfSpeech::from_id(part_of_speech)
                        .expect("declared non-inflecting POS"),
                    priority: UserDictionaryPriority::Normal,
                })
            })
            .collect();
        let catalog = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: vec![ImportedDictionarySnapshot {
                dictionary_id: "example.provenance-bound".to_owned(),
                enabled: true,
                entries,
            }],
        };

        assert_eq!(
            catalog.into_overlay(),
            Err(ImportedDictionaryError::TooManyExpandedEntries)
        );
        assert_eq!(ENTRY_SORT_COMPARISONS.with(std::cell::Cell::get), 0);
    }

    #[test]
    #[ignore = "product-scale release gate"]
    fn product_scale_same_reading_catalog_is_rejected_before_runtime_materialization() {
        ENTRY_SORT_COMPARISONS.with(|count| count.set(0));
        let entries = (0..MAX_SOURCE_ENTRIES)
            .rev()
            .map(|index| record("きょだい", &format!("語{index:06}"), "NOUN"))
            .collect();
        let catalog = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries: vec![ImportedDictionarySnapshot {
                dictionary_id: "example.concentrated-scale".to_owned(),
                enabled: true,
                entries,
            }],
        };

        assert_eq!(
            catalog.into_overlay(),
            Err(ImportedDictionaryError::TooManyExpandedEntries)
        );
        assert_eq!(
            ENTRY_SORT_COMPARISONS.with(std::cell::Cell::get),
            0,
            "the concentrated-reading guard must reject before sorting 250k records"
        );
    }

    #[test]
    #[ignore = "product-scale release gate"]
    fn product_scale_catalog_builds_within_the_declared_budget() {
        const KANA: &[char] = &[
            'あ', 'い', 'う', 'え', 'お', 'か', 'き', 'く', 'け', 'こ', 'さ', 'し', 'す', 'せ',
            'そ', 'た', 'ち', 'つ', 'て', 'と', 'な', 'に', 'ぬ', 'ね', 'の', 'は', 'ひ', 'ふ',
            'へ', 'ほ', 'ま', 'み', 'む', 'め', 'も', 'や', 'ゆ', 'よ', 'ら', 'り', 'る', 'れ',
            'ろ', 'わ', 'を', 'ん',
        ];
        fn reading(mut index: usize) -> String {
            let mut value = ['あ'; 4];
            for character in value.iter_mut().rev() {
                *character = KANA[index % KANA.len()];
                index /= KANA.len();
            }
            value.into_iter().collect()
        }

        let started = Instant::now();
        let mut next_entry = 0usize;
        let dictionaries = [100_000usize, 100_000, 50_000]
            .into_iter()
            .enumerate()
            .map(|(dictionary_index, count)| {
                let entries = (0..count)
                    .map(|_| {
                        let index = next_entry;
                        next_entry += 1;
                        ImportedDictionaryRecord {
                            reading: reading(index),
                            surface: format!("語{index}"),
                            part_of_speech: UserDictionaryPartOfSpeech::noun(),
                            priority: UserDictionaryPriority::Normal,
                        }
                    })
                    .collect();
                ImportedDictionarySnapshot {
                    dictionary_id: format!("example.scale.{dictionary_index}"),
                    enabled: true,
                    entries,
                }
            })
            .collect();
        let catalog = ImportedDictionaryCatalogSnapshot {
            schema_version: IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION,
            dictionaries,
        };
        let overlay = catalog.into_overlay().expect("declared maximum catalog");
        let elapsed = started.elapsed();
        assert!(!overlay.is_empty());
        assert!(
            elapsed < Duration::from_secs(10),
            "product-scale catalog build took {elapsed:?}"
        );
        eprintln!("imported dictionary product-scale build={elapsed:?}");
    }
}
