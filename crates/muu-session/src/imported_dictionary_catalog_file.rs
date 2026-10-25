//! Stored catalogs are a file protocol, distinct from the strict in-memory
//! `ImportedDictionaryCatalogSnapshot` (once also the C ABI's replacement
//! snapshot; that entry was removed in ABI 13). Foundation accepted unknown/duplicate fields and integral decimal
//! numbers; using that snapshot's deny_unknown_fields here would lose files.

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::imported_dictionary_file::{
    ImportedDictionaryDocument, ImportedDictionaryFileError, ImportedDictionaryMetadata,
    MAX_IMPORTED_FILE_ENTRIES, is_hash,
};
use crate::{ImportedDictionaryRecord, UserDictionaryPartOfSpeech, UserDictionaryPriority};

pub const MAX_IMPORTED_CATALOG_BYTES: usize = 32 * 1024 * 1024;
pub use crate::dictionary_catalog::MAX_IMPORTED_DICTIONARIES;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportedDictionaryManifestEntry {
    pub metadata: ImportedDictionaryMetadata,
    pub enabled: bool,
    pub imported_at_unix_ms: i64,
    pub source_sha256: String,
    pub blob_sha256: String,
    pub order: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportedDictionaryManifest {
    pub schema_version: u32,
    pub dictionaries: Vec<ImportedDictionaryManifestEntry>,
}

/// Used only while reading the bounded legacy file. It is consumed into
/// summaries and enabled runtime parts, not retained as management contents.
pub struct LegacyImportedDictionary {
    pub document: ImportedDictionaryDocument,
    pub enabled: bool,
    pub imported_at_unix_ms: i64,
    pub source_sha256: String,
    pub order: i64,
}

fn invalid() -> ImportedDictionaryFileError {
    ImportedDictionaryFileError::new("invalid_catalog")
}

// Preserve this codec's domain-specific errors at the shared primitive boundary.
use crate::stored_json::{boolean, catalog_encoding, fields, integer, required, string};
impl From<crate::stored_json::StoredJsonError> for ImportedDictionaryFileError {
    fn from(_: crate::stored_json::StoredJsonError) -> Self {
        invalid()
    }
}

struct BoundedArray(usize);
impl<'de> Visitor<'de> for BoundedArray {
    type Value = Vec<&'de RawValue>;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a bounded catalog array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut result = Vec::new();
        while let Some(value) = seq.next_element::<&RawValue>()? {
            if result.len() == self.0 {
                return Err(serde::de::Error::custom("catalog array limit"));
            }
            result.push(value);
        }
        Ok(result)
    }
}

fn array(raw: &RawValue, max: usize) -> Result<Vec<&RawValue>, ImportedDictionaryFileError> {
    let mut decoder = serde_json::Deserializer::from_str(raw.get());
    let result = decoder
        .deserialize_seq(BoundedArray(max))
        .map_err(|_| invalid())?;
    decoder.end().map_err(|_| invalid())?;
    Ok(result)
}

fn metadata(raw: &RawValue) -> Result<ImportedDictionaryMetadata, ImportedDictionaryFileError> {
    let f = fields(
        raw.get().as_bytes(),
        &["id", "name", "version", "author", "license", "provenance"],
    )?;
    ImportedDictionaryMetadata {
        id: string(required(&f, "id")?)?,
        name: string(required(&f, "name")?)?,
        version: string(required(&f, "version")?)?,
        author: string(required(&f, "author")?)?,
        license: string(required(&f, "license")?)?,
        provenance: string(required(&f, "provenance")?)?,
    }
    .normalized()
}

/// Parse existing pointer bytes only. The host opens a generation using the
/// returned hash and its own fixed directory, never a path supplied by Rust.
pub fn decode_imported_dictionary_pointer(
    bytes: &[u8],
) -> Result<String, ImportedDictionaryFileError> {
    if bytes.len() > 128 {
        return Err(ImportedDictionaryFileError::new("invalid_pointer"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ImportedDictionaryFileError::new("invalid_pointer"))?;
    let hash = text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{200b}');
    if !is_hash(hash) {
        return Err(ImportedDictionaryFileError::new("invalid_pointer"));
    }
    Ok(hash.to_owned())
}

// Generation manifest, schema 2: at most 32 dictionaries with unique ids and
// unique non-negative orders, each naming its canonical blob by SHA-256.
// Returned sorted by `order`, which is the display and layering order.
pub fn decode_imported_dictionary_manifest(
    bytes: &[u8],
) -> Result<ImportedDictionaryManifest, ImportedDictionaryFileError> {
    if bytes.len() > MAX_IMPORTED_CATALOG_BYTES {
        return Err(ImportedDictionaryFileError::new("catalog_too_large"));
    }
    let bytes = catalog_encoding(bytes)?;
    let f = fields(&bytes, &["schema_version", "dictionaries"])?;
    if integer(required(&f, "schema_version")?)? != 2 {
        return Err(invalid());
    }
    let mut dictionaries = Vec::new();
    let mut ids = BTreeSet::new();
    let mut orders = BTreeSet::new();
    for raw in array(required(&f, "dictionaries")?, MAX_IMPORTED_DICTIONARIES)? {
        let f = fields(
            raw.get().as_bytes(),
            &[
                "metadata",
                "enabled",
                "imported_at_unix_ms",
                "source_sha256",
                "blob_sha256",
                "order",
            ],
        )?;
        let entry = ImportedDictionaryManifestEntry {
            metadata: metadata(required(&f, "metadata")?)?,
            enabled: boolean(required(&f, "enabled")?)?,
            imported_at_unix_ms: integer(required(&f, "imported_at_unix_ms")?)?,
            source_sha256: string(required(&f, "source_sha256")?)?,
            blob_sha256: string(required(&f, "blob_sha256")?)?,
            order: integer(required(&f, "order")?)?,
        };
        if !ids.insert(entry.metadata.id.clone())
            || !orders.insert(entry.order)
            || entry.order < 0
            || !is_hash(&entry.blob_sha256)
            || (!entry.source_sha256.is_empty() && !is_hash(&entry.source_sha256))
        {
            return Err(invalid());
        }
        dictionaries.push(entry);
    }
    dictionaries.sort_by_key(|entry| entry.order);
    Ok(ImportedDictionaryManifest {
        schema_version: 2,
        dictionaries,
    })
}

// The legacy single-file catalog, schema 1, with every dictionary's rows
// inline. Read at startup to serve as is, and again at the first explicit
// edit to migrate into blobs. Stored orders are validated but replaced by the
// array position, as the old migration did.
pub fn decode_legacy_imported_catalog(
    bytes: &[u8],
    now_unix_ms: i64,
) -> Result<Vec<LegacyImportedDictionary>, ImportedDictionaryFileError> {
    if bytes.len() > MAX_IMPORTED_CATALOG_BYTES {
        return Err(ImportedDictionaryFileError::new("catalog_too_large"));
    }
    let bytes = catalog_encoding(bytes)?;
    let f = fields(&bytes, &["schema_version", "dictionaries"])?;
    if integer(required(&f, "schema_version")?)? != 1 {
        return Err(invalid());
    }
    let mut dictionaries = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, raw) in array(required(&f, "dictionaries")?, MAX_IMPORTED_DICTIONARIES)?
        .into_iter()
        .enumerate()
    {
        let f = fields(
            raw.get().as_bytes(),
            &[
                "metadata",
                "enabled",
                "entries",
                "imported_at_unix_ms",
                "source_sha256",
                "order",
            ],
        )?;
        let metadata = metadata(required(&f, "metadata")?)?;
        if !ids.insert(metadata.id.clone()) {
            return Err(invalid());
        }
        let mut entries = Vec::new();
        for raw in array(required(&f, "entries")?, MAX_IMPORTED_FILE_ENTRIES)? {
            let f = fields(
                raw.get().as_bytes(),
                &["reading", "surface", "partOfSpeech", "priority"],
            )?;
            let pos = string(required(&f, "partOfSpeech")?)?;
            if pos == "noun" {
                return Err(invalid());
            }
            let priority: UserDictionaryPriority =
                serde_json::from_str(required(&f, "priority")?.get()).map_err(|_| invalid())?;
            entries.push(ImportedDictionaryRecord {
                reading: string(required(&f, "reading")?)?,
                surface: string(required(&f, "surface")?)?,
                part_of_speech: UserDictionaryPartOfSpeech::from_id(&pos).ok_or_else(invalid)?,
                priority,
            });
        }
        let timestamp = f
            .get("imported_at_unix_ms")
            .filter(|raw| raw.get() != "null")
            .map(|raw| integer(raw))
            .transpose()?
            .unwrap_or(0);
        let source = f
            .get("source_sha256")
            .filter(|raw| raw.get() != "null")
            .map(|raw| string(raw))
            .transpose()?
            .unwrap_or_default();
        if !source.is_empty() && !is_hash(&source) {
            return Err(invalid());
        }
        // decodeIfPresent still validates a present stored order, even though
        // migration subsequently replaces it by source-array position.
        if let Some(raw) = f.get("order").filter(|raw| raw.get() != "null") {
            integer(raw)?;
        }
        // Old migration ignores stored order and enumerates the source array.
        // A missing timestamp is a display default until the first explicit edit.
        dictionaries.push(LegacyImportedDictionary {
            document: ImportedDictionaryDocument { metadata, entries },
            enabled: boolean(required(&f, "enabled")?)?,
            imported_at_unix_ms: if timestamp > 0 {
                timestamp
            } else {
                now_unix_ms
            },
            source_sha256: source,
            order: index as i64,
        });
    }
    Ok(dictionaries)
}

pub(crate) fn foundation_json(
    value: &impl Serialize,
) -> Result<Vec<u8>, ImportedDictionaryFileError> {
    crate::stored_json::foundation_json(value).map_err(Into::into)
}

pub fn encode_imported_dictionary_manifest(
    manifest: &ImportedDictionaryManifest,
) -> Result<Vec<u8>, ImportedDictionaryFileError> {
    let bytes = foundation_json(manifest)?;
    if bytes.len() > MAX_IMPORTED_CATALOG_BYTES {
        return Err(ImportedDictionaryFileError::new("catalog_too_large"));
    }
    // Canonicalizing the validated value also normalizes metadata and array
    // order. Merely validating then returning the input would retain NFD or a
    // caller's unsorted dictionary array, unlike the old store's validated().
    let canonical = decode_imported_dictionary_manifest(&bytes)?;
    foundation_json(&canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foundation_file_keys_and_exact_integer_spellings_are_preserved() {
        for (input, expected) in [
            ("1.0", 1),
            ("1e3", 1000),
            ("9007199254740993.0", 9007199254740993),
            ("-9223372036854775808", i64::MIN),
            ("9223372036854775807", i64::MAX),
            ("9223372036854775807.0", i64::MAX),
            ("-9223372036854775808.0", i64::MIN),
        ] {
            let raw: &RawValue = serde_json::from_str(input).unwrap();
            assert_eq!(integer(raw).unwrap(), expected);
        }
        for input in [
            "1.5",
            "9007199254740993.5",
            "9223372036854775808",
            "-9223372036854775809",
            "1e10000",
        ] {
            assert!(integer(serde_json::from_str(input).unwrap()).is_err());
        }
        let value = decode_imported_dictionary_manifest(
            br#"{"schema_version":2.0,"schema_version":9,"unknown":true,"dictionaries":[]}"#,
        )
        .unwrap();
        assert_eq!(value.schema_version, 2);
        // JSONEncoder never emits trailing commas. Keep stored dictionary JSON
        // strict instead of repairing forms accepted only by Foundation's reader.
        for bytes in [
            br#"{"schema_version":2,"dictionaries":[],}"#.as_slice(),
            br#"{"schema_version":2,"dictionaries":[,]}"#.as_slice(),
        ] {
            assert!(decode_imported_dictionary_manifest(bytes).is_err());
        }
        assert_eq!(
            encode_imported_dictionary_manifest(&value).unwrap(),
            br#"{"dictionaries":[],"schema_version":2}"#
        );
        assert_eq!(
            foundation_json(&serde_json::json!({"b":"a/b","a":1})).unwrap(),
            br#"{"a":1,"b":"a\/b"}"#
        );
    }

    #[test]
    fn pointer_accepts_only_hash_names_but_preserves_foundation_whitespace() {
        let hash = "1".repeat(64);
        assert_eq!(
            decode_imported_dictionary_pointer(format!("\u{200b}{hash}\n").as_bytes()).unwrap(),
            hash
        );
        for name in [
            "../file".into(),
            "A".repeat(64),
            String::new(),
            "1".repeat(63),
        ] {
            assert!(decode_imported_dictionary_pointer(name.as_bytes()).is_err());
        }
    }

    #[test]
    fn stored_json_preserves_foundation_unicode_encoding_acceptance() {
        let text = r#"{"schema_version":2,"dictionaries":[]}"#;
        let utf8_bom = [&[0xef, 0xbb, 0xbf][..], text.as_bytes()].concat();
        let mut inputs = vec![text.as_bytes().to_vec(), utf8_bom];
        for little in [false, true] {
            let u16bytes: Vec<_> = text
                .encode_utf16()
                .flat_map(|n| {
                    if little {
                        n.to_le_bytes()
                    } else {
                        n.to_be_bytes()
                    }
                })
                .collect();
            let bom = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            inputs.push([bom, u16bytes.clone()].concat());
            inputs.push(u16bytes);
            let u32bytes: Vec<_> = text
                .chars()
                .flat_map(|ch| {
                    if little {
                        (ch as u32).to_le_bytes()
                    } else {
                        (ch as u32).to_be_bytes()
                    }
                })
                .collect();
            let bom = if little {
                vec![0xff, 0xfe, 0, 0]
            } else {
                vec![0, 0, 0xfe, 0xff]
            };
            let with_bom = [bom, u32bytes.clone()].concat();
            if little {
                assert!(decode_imported_dictionary_manifest(&with_bom).is_err());
            } else {
                inputs.push(with_bom);
            }
            inputs.push(u32bytes);
        }
        for input in inputs {
            let result = decode_imported_dictionary_manifest(&input).unwrap();
            assert_eq!(result.schema_version, 2);
            assert!(result.dictionaries.is_empty());
        }
        assert!(decode_imported_dictionary_manifest(&[0xff, 0xfe, 0x7b]).is_err());
    }

    #[test]
    fn legacy_camelcase_rows_defaults_and_source_order_match_swift() {
        let bytes=br#"{"schema_version":1,"dictionaries":[{"metadata":{"id":"example","name":"name","version":"1","author":"a","license":"l","provenance":"p"},"enabled":true,"entries":[{"reading":"a","surface":"A","partOfSpeech":"NOUN","priority":"normal"}],"order":-1,"imported_at_unix_ms":null,"source_sha256":null}]}"#;
        let result = decode_legacy_imported_catalog(bytes, 123).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].imported_at_unix_ms, 123);
        assert_eq!(result[0].order, 0);
        assert!(result[0].source_sha256.is_empty());
        assert_eq!(result[0].document.entries[0].part_of_speech.as_id(), "NOUN");
        let bad = String::from_utf8(bytes.to_vec())
            .unwrap()
            .replace("\"order\":-1", "\"order\":\"bad\"");
        assert!(decode_legacy_imported_catalog(bad.as_bytes(), 123).is_err());
    }
}
