//! Management retains summaries, not blob bytes or disabled dictionary rows.
//! Enabled runtime rows live in separately shared parts, within the existing
//! source/semantic bounds; a new operation reuses all unaffected parts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use muu_conversion::{LexiconOverlay, UserPosBehavior, user_pos_definition};
use serde::Serialize;

use crate::dictionary_catalog::{
    ImportedDictionaryInput, imported_overlay_from_parts, opaque_dictionary_key,
};
use crate::imported_dictionary_catalog_file::{
    ImportedDictionaryManifest, ImportedDictionaryManifestEntry,
};
use crate::imported_dictionary_file::{
    ImportedDictionaryDocument, ImportedDictionaryFileError, encode_imported_dictionary_file,
    sha256,
};
use crate::{ImportedDictionaryError, ImportedDictionaryRecord};

#[derive(Clone, Debug)]
pub enum ImportedDictionaryEditError {
    Busy,
    RevisionMismatch,
    RevisionOverflow,
    InvalidCommand,
    InvalidTicket,
    ReadOnly,
    DegradedRepairOnly,
    MissingDictionary,
    CoreInvalid,
    File(ImportedDictionaryFileError),
    Layer(&'static str),
}

impl ImportedDictionaryEditError {
    pub fn class(&self) -> crate::EditErrorClass {
        use crate::EditErrorClass;
        match self {
            Self::Busy => EditErrorClass::Busy,
            Self::RevisionMismatch => EditErrorClass::RevisionMismatch,
            Self::CoreInvalid => EditErrorClass::CoreInvalid,
            Self::RevisionOverflow => EditErrorClass::Internal,
            Self::InvalidCommand
            | Self::InvalidTicket
            | Self::ReadOnly
            | Self::DegradedRepairOnly
            | Self::MissingDictionary
            | Self::File(_)
            | Self::Layer(_) => EditErrorClass::Rejected,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::RevisionMismatch => "revision_mismatch",
            Self::RevisionOverflow => "revision_overflow",
            Self::InvalidCommand => "invalid_imported_dictionary_command",
            Self::InvalidTicket => "invalid_imported_dictionary_ticket",
            Self::ReadOnly => "imported_dictionary_read_only",
            Self::DegradedRepairOnly => "imported_dictionary_repair_required",
            Self::MissingDictionary => "missing_imported_dictionary",
            Self::CoreInvalid => "core_invalid",
            Self::File(e) => e.code,
            Self::Layer(code) => code,
        }
    }
}

impl From<ImportedDictionaryFileError> for ImportedDictionaryEditError {
    fn from(error: ImportedDictionaryFileError) -> Self {
        Self::File(error)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportedDictionarySummary {
    #[serde(flatten)]
    pub manifest: ImportedDictionaryManifestEntry,
    pub entry_count: usize,
    pub dormant_entry_count: usize,
    pub expanded_entry_count: usize,
    pub semantic_bytes: usize,
    pub canonical_blob_sha256: String,
    pub source_identity: String,
    pub runtime_error: Option<&'static str>,
}

/// This is execution data, not management or file bytes. Off dictionaries have
/// no such part. Original normalized row order binds the old snapshot identity;
/// the builder sorts borrowed references for stable token IDs, just as before.
pub(crate) struct ImportedRuntimePart {
    pub records: Vec<ImportedDictionaryRecord>,
}

#[derive(Clone)]
pub(crate) struct ImportedDictionaryManagement {
    pub revision: u64,
    pub initialized: bool,
    pub dictionaries: Option<Vec<ImportedDictionarySummary>>,
    pub file_error: Option<&'static str>,
    pub layer_error: Option<&'static str>,
    pub generation: Option<String>,
    pub legacy_sha256: Option<String>,
}

impl Default for ImportedDictionaryManagement {
    fn default() -> Self {
        Self {
            revision: 0,
            initialized: false,
            dictionaries: Some(Vec::new()),
            file_error: None,
            layer_error: None,
            generation: None,
            legacy_sha256: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ImportedDictionaryPublication {
    pub management: ImportedDictionaryManagement,
    pub parts: BTreeMap<String, Arc<ImportedRuntimePart>>,
    pub source_identity: String,
}

impl Default for ImportedDictionaryPublication {
    fn default() -> Self {
        Self {
            management: Default::default(),
            parts: BTreeMap::new(),
            source_identity: source_identity(std::iter::empty()),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportedDictionaryView {
    pub schema_version: u32,
    pub management_revision: u64,
    pub access: &'static str,
    pub layer_status: &'static str,
    pub reason: Option<&'static str>,
    pub dictionaries: Option<Vec<ImportedDictionarySummary>>,
}

impl ImportedDictionaryManagement {
    pub fn view(&self) -> ImportedDictionaryView {
        ImportedDictionaryView {
            schema_version: 1,
            management_revision: self.revision,
            access: if !self.initialized {
                "loading"
            } else if self.file_error.is_some() {
                "read_only"
            } else {
                "editable"
            },
            layer_status: if self.file_error.is_some() || self.layer_error.is_some() {
                "empty_degraded"
            } else {
                "active"
            },
            reason: self.file_error.or(self.layer_error),
            dictionaries: self.dictionaries.clone(),
        }
    }

    pub fn manifest(&self) -> ImportedDictionaryManifest {
        ImportedDictionaryManifest {
            schema_version: 2,
            dictionaries: self
                .dictionaries
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|item| item.manifest.clone())
                .collect(),
        }
    }
}

fn json_digest(value: &impl Serialize) -> String {
    // These bounded data serialize infallibly; the shared writer streams bytes.
    // The identity is the shape of what callers pass, as described below.
    muu_digest::json_sha256(value).expect("infallible source identity writer")
}

// The identity of what conversion would see: ordered (id, enabled, digest of
// the normalized rows). Metadata, timestamps and blob hashes are excluded, so
// an edit that changes only those reuses the live layer.
fn source_identity<'a>(dictionaries: impl IntoIterator<Item = (&'a str, bool, &'a str)>) -> String {
    json_digest(&dictionaries.into_iter().collect::<Vec<_>>())
}

pub(crate) fn summary_identity(dictionaries: &[ImportedDictionarySummary]) -> String {
    source_identity(dictionaries.iter().map(|d| {
        (
            d.manifest.metadata.id.as_str(),
            d.manifest.enabled,
            d.source_identity.as_str(),
        )
    }))
}

pub(crate) fn runtime_error_code(error: ImportedDictionaryError) -> &'static str {
    use ImportedDictionaryError::*;
    match error {
        Busy => ImportedDictionaryEditError::Busy.code(),
        UnsupportedSchema => "unsupported_catalog_schema",
        TooManyDictionaries => "too_many_dictionaries",
        TooManyEntries => "too_many_entries",
        TooManyExpandedEntries => "too_many_expanded_entries",
        SnapshotTooLarge => "catalog_semantic_too_large",
        RevisionOverflow => "revision_overflow",
        InvalidDictionaryId => "invalid_dictionary_id",
        DuplicateDictionary => "duplicate_dictionary",
        DuplicateEntry => "duplicate_entry",
        InvalidReading => "invalid_reading",
        InvalidSurface => "invalid_surface",
        InvalidPartOfSpeech => "invalid_part_of_speech",
        InvalidBaseForm => "invalid_base_form",
    }
}

/// Validation outputs bytes only to the current preparation step. The caller
/// either hands them to Swift for immutable staging, or drops them immediately
/// at startup. Nothing in Summary owns that Vec.
pub(crate) fn summarize_document(
    document: ImportedDictionaryDocument,
    mut manifest: ImportedDictionaryManifestEntry,
) -> Result<
    (ImportedDictionarySummary, Arc<ImportedRuntimePart>, Vec<u8>),
    ImportedDictionaryFileError,
> {
    let canonical = encode_imported_dictionary_file(&document)?;
    if document.metadata != manifest.metadata {
        return Err(ImportedDictionaryFileError::new("blob_metadata_mismatch"));
    }
    let canonical_hash = sha256(&canonical);
    if manifest.blob_sha256.is_empty() {
        manifest.blob_sha256 = canonical_hash.clone();
    }
    if manifest.source_sha256.is_empty() {
        manifest.source_sha256 = canonical_hash.clone();
    }
    let runtime_error = imported_overlay_from_parts(&[ImportedDictionaryInput {
        dictionary_id: &manifest.metadata.id,
        enabled: false,
        entries: &document.entries,
    }])
    .err()
    .map(runtime_error_code);
    let mut semantic_bytes = manifest.metadata.id.len() + 64;
    let mut dormant = 0;
    let mut expanded = 0;
    for row in &document.entries {
        semantic_bytes +=
            row.reading.len() + row.surface.len() + row.part_of_speech.as_id().len() + 64;
        let definition =
            user_pos_definition(row.part_of_speech.as_id()).expect("codec validated POS");
        match definition.behavior {
            UserPosBehavior::DormantSuggestion => dormant += 1,
            UserPosBehavior::Suppression => {}
            _ => expanded += definition.forms.len(),
        }
    }
    let summary = ImportedDictionarySummary {
        manifest,
        entry_count: document.entries.len(),
        dormant_entry_count: dormant,
        expanded_entry_count: expanded,
        semantic_bytes,
        canonical_blob_sha256: canonical_hash,
        source_identity: json_digest(&document.entries),
        runtime_error,
    };
    Ok((
        summary,
        Arc::new(ImportedRuntimePart {
            records: document.entries,
        }),
        canonical,
    ))
}

/// Management budgets include disabled source rows; expansion bounds include
/// enabled nodes only. Summaries let those checks run without rereading blobs.
// Bounds: at most 32 dictionaries, 250,000 rows and 32 MiB of semantic bytes
// counting disabled ones, 1,000,000 expanded entries counting enabled ones,
// and no two ids sharing an opaque key. The first failing bound names the
// whole layer's error.
pub(crate) fn summary_layer_error(
    dictionaries: &[ImportedDictionarySummary],
) -> Option<&'static str> {
    if dictionaries.len() > 32 {
        return Some("too_many_dictionaries");
    }
    if dictionaries.iter().map(|d| d.entry_count).sum::<usize>() > 250_000 {
        return Some("too_many_entries");
    }
    if dictionaries.iter().map(|d| d.semantic_bytes).sum::<usize>() > 32 * 1024 * 1024 {
        return Some("catalog_semantic_too_large");
    }
    if dictionaries
        .iter()
        .filter(|d| d.manifest.enabled)
        .map(|d| d.expanded_entry_count)
        .sum::<usize>()
        > 1_000_000
    {
        return Some("too_many_expanded_entries");
    }
    let mut keys = BTreeSet::new();
    let mut sorted: Vec<_> = dictionaries.iter().collect();
    sorted.sort_by(|a, b| a.manifest.metadata.id.cmp(&b.manifest.metadata.id));
    for dictionary in sorted {
        if !keys.insert(opaque_dictionary_key(&dictionary.manifest.metadata.id)) {
            return Some("duplicate_dictionary");
        }
        if let Some(reason) = dictionary.runtime_error {
            return Some(reason);
        }
    }
    None
}

// A layer error empties the whole runtime layer while keeping every summary,
// so the UI can still list and repair the dictionaries. `needs_blob` means an
// enabled dictionary's rows were not supplied; callers queue that read first.
pub(crate) fn build_publication(
    mut management: ImportedDictionaryManagement,
    mut parts: BTreeMap<String, Arc<ImportedRuntimePart>>,
) -> Result<(Arc<ImportedDictionaryPublication>, Arc<LexiconOverlay>), &'static str> {
    let dictionaries = management.dictionaries.as_deref().unwrap_or_default();
    management.layer_error = summary_layer_error(dictionaries);
    if management.file_error.is_some() || management.layer_error.is_some() {
        parts.clear();
        return Ok((
            Arc::new(ImportedDictionaryPublication {
                management,
                parts,
                source_identity: source_identity(std::iter::empty()),
            }),
            Arc::new(LexiconOverlay::default()),
        ));
    }
    parts.retain(|id, _| {
        dictionaries
            .iter()
            .any(|d| d.manifest.enabled && d.manifest.metadata.id == *id)
    });
    let mut inputs = Vec::new();
    for dictionary in dictionaries.iter().filter(|d| d.manifest.enabled) {
        let part = parts
            .get(&dictionary.manifest.metadata.id)
            .ok_or("needs_blob")?;
        inputs.push(ImportedDictionaryInput {
            dictionary_id: &dictionary.manifest.metadata.id,
            enabled: true,
            entries: &part.records,
        });
    }
    let overlay = match imported_overlay_from_parts(&inputs) {
        Ok(overlay) => Arc::new(overlay),
        Err(error) => {
            management.layer_error = Some(runtime_error_code(error));
            parts.clear();
            Arc::new(LexiconOverlay::default())
        }
    };
    let identity = if management.layer_error.is_some() {
        source_identity(std::iter::empty())
    } else {
        summary_identity(management.dictionaries.as_deref().unwrap_or_default())
    };
    Ok((
        Arc::new(ImportedDictionaryPublication {
            management,
            parts,
            source_identity: identity,
        }),
        overlay,
    ))
}
