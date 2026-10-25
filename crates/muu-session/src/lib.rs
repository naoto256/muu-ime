//! Stateful request handling and schema-v1 view construction.
//!
//! This crate is the only layer that projects Core scalar offsets into UTF-16
//! offsets for the Swift surface.

#[cfg(test)]
#[path = "../../../test-support/allocation_probe.rs"]
mod allocation_probe;
mod auxiliary_data;
mod candidate_presentation;
mod commit_stream;
mod core;
mod dictionary_catalog;
mod dictionary_edit;
mod imported_dictionary_catalog_file;
mod imported_dictionary_edit;
mod imported_dictionary_file;
mod imported_dictionary_management;
mod keymap;
mod learning;
mod personal_frequency;
mod postal_code;
mod postal_code_edit;
mod postal_code_file;
mod postal_code_zip;
mod preceding_context;
mod protocol;
#[cfg(test)]
#[path = "../../../test-support/real_assets.rs"]
mod real_assets;
#[cfg(test)]
#[path = "../../../test-support/real_dictionary.rs"]
mod real_dictionary;
mod recording;
mod selection;
mod session;
mod stored_file;
mod stored_json;
pub use stored_file::StoredFileState;
#[cfg(test)]
mod test_support;
mod user_dictionary;
mod user_dictionary_edit;
mod user_dictionary_file;
mod view;
mod vocabulary;
pub use commit_stream::CommitStreamConfig;
pub use core::{
    ConversionSnapshot, Core, CoreConfig, CoreRuntimeStatus, CoreSessionSettings,
    LanguageModelInfo, LanguageModelSnapshot, LanguageModelState,
};
pub use dictionary_catalog::{
    IMPORTED_DICTIONARY_CATALOG_SCHEMA_VERSION, ImportedDictionaryCatalogSnapshot,
    ImportedDictionaryError, ImportedDictionaryRecord, ImportedDictionarySnapshot,
};
pub use dictionary_edit::EditErrorClass;
pub use imported_dictionary_catalog_file::{
    ImportedDictionaryManifest, ImportedDictionaryManifestEntry, LegacyImportedDictionary,
    MAX_IMPORTED_CATALOG_BYTES, decode_imported_dictionary_manifest,
    decode_imported_dictionary_pointer, decode_legacy_imported_catalog,
    encode_imported_dictionary_manifest,
};
pub use imported_dictionary_edit::{
    ImportedDictionaryEditCommand, ImportedDictionaryFinishCommand, ImportedDictionaryFinishKind,
    ImportedDictionaryLoadCommand, ImportedDictionarySupplyCommand, ImportedDictionaryTicket,
    MAX_IMPORTED_COMMAND_BYTES, MAX_IMPORTED_REPLY_BYTES, MAX_IMPORTED_TICKET_BYTES,
    PreparedImportedDictionaryReply,
};
pub use imported_dictionary_file::{
    ImportedDictionaryDocument, ImportedDictionaryFileError, ImportedDictionaryMetadata,
    MAX_IMPORTED_SOURCE_BYTES, decode_import_source, decode_imported_dictionary_file,
    encode_imported_dictionary_file, imported_dictionary_template,
};
pub use imported_dictionary_management::{
    ImportedDictionaryEditError, ImportedDictionarySummary, ImportedDictionaryView,
};
pub use learning::{LearningVersion, PersonalFrequencyUpdate};
pub use muu_lm::ModelKind as ConversionModelKind;
pub use personal_frequency::{
    PersonalFrequencyEntry, PersonalFrequencyError, PersonalFrequencySnapshot,
};
pub use postal_code::{
    POSTAL_CODE_SCHEMA_VERSION, PostalCodeError, PostalCodeRecord, PostalCodeSnapshot,
};
pub use selection::{
    ScoredHypothesis, SelectionHypothesis, SelectionReport, SelectionRunReport, SelectionSkipped,
};
pub use user_dictionary::{
    MAX_USER_DICTIONARY_READING_UTF8_BYTES, MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES,
    MAX_USER_DICTIONARY_SURFACE_UTF8_BYTES, USER_DICTIONARY_SCHEMA_VERSION, UserDictionaryError,
    UserDictionaryPartOfSpeech, UserDictionaryPriority, UserDictionaryRecord,
    UserDictionarySnapshot,
};
pub use vocabulary::{SharedVocabulary, VocabularySnapshot};

pub(crate) use core::conversion_snapshot;
pub use core::default_segmenter;
pub use protocol::*;
pub use session::{Session, SessionError};

#[cfg(test)]
pub(crate) use session::generate_conversion;

pub use view::{view_from_preedit, view_from_segments};

pub use user_dictionary_edit::{PreparedUserDictionaryEdit, UserDictionaryTicket};
pub use user_dictionary_file::{
    MAX_USER_DICTIONARY_EDIT_BYTES, MAX_USER_DICTIONARY_TICKET_BYTES,
    MAX_USER_DICTIONARY_VIEW_BYTES, UserDictionaryEditCommand, UserDictionaryEditError,
    UserDictionaryFileError, UserDictionaryView, decode_user_dictionary_file,
    encode_user_dictionary_file,
};

pub use postal_code_file::{
    MAX_POSTAL_COMMAND_BYTES, MAX_POSTAL_CSV_BYTES, MAX_POSTAL_ENTRIES, MAX_POSTAL_MANIFEST_BYTES,
    MAX_POSTAL_TICKET_BYTES, MAX_POSTAL_TSV_BYTES, MAX_POSTAL_VIEW_BYTES, MAX_POSTAL_ZIP_BYTES,
    POSTAL_SOURCE_PAGE_URL, POSTAL_SOURCE_URL, PostalCodeFileError, PostalCodeManifest,
    PostalCodePreparedFile, decode_postal_code_csv, decode_postal_code_manifest,
    decode_postal_code_tsv, encode_postal_code_manifest, encode_postal_code_seed_manifest,
    encode_postal_code_tsv, postal_utc_timestamp, prepare_postal_code_zip,
};

pub use postal_code_edit::{
    PostalCodeEditError, PostalCodeFinishCommand, PostalCodeFinishKind, PostalCodeLoadCommand,
    PostalCodePrepareCommand, PostalCodeSupplyCommand, PostalCodeTicket, PostalCodeView,
    PreparedPostalCodeReply,
};
