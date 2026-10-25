//! Exchange formats only: neither a path nor a persistence policy belongs here.
//!
//! Unlike User TSV, these formats deliberately normalize fields when reading.
//! The runtime still validates its own stricter layer (notably IDs and HA).

// The Imported names for the shared mechanism, used by the catalog, edit and
// test support. What each hash covers (blob, source, the `google.` ID prefix)
// stays with those uses.
pub(crate) use muu_digest::{is_sha256_hex as is_hash, sha256_hex as sha256};
use std::collections::BTreeSet;

use muu_conversion::{UserPosBehavior, user_pos_definition};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use crate::user_dictionary_file::{
    UserDictionaryFileError, legacy_control_character, normalized_record,
};
use crate::{ImportedDictionaryRecord, UserDictionaryPartOfSpeech, UserDictionaryPriority};

pub const MAX_IMPORTED_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMPORTED_FILE_ENTRIES: usize = 100_000;
pub const MAX_IMPORTED_SEMANTIC_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMPORTED_EXPANDED_ENTRIES: usize = 1_000_000;
// Native `.muudict` layout: line 1 the header, lines 2-7 the six `#key TAB
// value` metadata lines in any order, line 8 the column line, then one
// `reading TAB surface TAB pos TAB priority` row per line from line 9 (the
// line numbers in errors). UTF-8 only, one optional BOM, LF separators.
const HEADER: &str = "#!muu-dictionary\t1";
const COLUMNS: &str = "reading\tsurface\tpos\tpriority";
const METADATA: [(&str, usize); 6] = [
    ("id", 128),
    ("name", 512),
    ("version", 256),
    ("author", 512),
    ("license", 512),
    ("provenance", 4096),
];

/// Display and file metadata, not an authority for conversion token identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedDictionaryMetadata {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub license: String,
    pub provenance: String,
}

impl ImportedDictionaryMetadata {
    fn values(&self) -> [&str; 6] {
        [
            &self.id,
            &self.name,
            &self.version,
            &self.author,
            &self.license,
            &self.provenance,
        ]
    }

    pub(crate) fn normalized(&self) -> Result<Self, ImportedDictionaryFileError> {
        let mut values = Vec::with_capacity(6);
        for ((key, limit), value) in METADATA.iter().zip(self.values()) {
            let value: String = value.nfc().collect();
            if value.is_empty()
                || value.len() > *limit
                || value.chars().any(legacy_control_character)
            {
                return Err(ImportedDictionaryFileError::field("invalid_metadata", key));
            }
            values.push(value);
        }
        if !readable_dictionary_id(&values[0]) {
            return Err(ImportedDictionaryFileError::field("invalid_metadata", "id"));
        }
        let mut values = values.into_iter();
        Ok(Self {
            id: values.next().unwrap(),
            name: values.next().unwrap(),
            version: values.next().unwrap(),
            author: values.next().unwrap(),
            license: values.next().unwrap(),
            provenance: values.next().unwrap(),
        })
    }

    pub(crate) fn semantic_bytes(&self) -> usize {
        self.values().iter().map(|value| value.len()).sum()
    }
}

/// Temporary decoded source. Management retains its summary, never these bytes
/// or rows for disabled dictionaries. A caller must consume/drop it promptly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedDictionaryDocument {
    pub metadata: ImportedDictionaryMetadata,
    pub entries: Vec<ImportedDictionaryRecord>,
}

/// Reason codes contain no source text; even an unsupported POS is not echoed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportedDictionaryFileError {
    pub code: &'static str,
    pub line: Option<usize>,
    pub field: Option<String>,
}

impl ImportedDictionaryFileError {
    pub(crate) fn new(code: &'static str) -> Self {
        Self {
            code,
            line: None,
            field: None,
        }
    }

    fn row(code: &'static str, line: usize) -> Self {
        Self {
            code,
            line: Some(line),
            field: None,
        }
    }

    fn field(code: &'static str, field: &str) -> Self {
        Self {
            code,
            line: None,
            field: Some(field.to_owned()),
        }
    }
}

/// Legacy uppercase IDs remain manageable, but runtime adoption rejects them.
/// Leading punctuation is accepted in lowercase dictionary IDs.
pub(crate) fn readable_dictionary_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && (id.as_bytes()[0].is_ascii_alphanumeric() || !id.bytes().any(|b| b.is_ascii_uppercase()))
}

/// Swift split(separator: Character("\n")) does NOT split a CRLF grapheme.
/// Rust str::lines would silently change the accepted files (Google included).
// A CRLF pair is one grapheme, so it never splits here: a CRLF file is one
// long line and fails its header check, as it did before. The callers'
// `strip_suffix('\r')` therefore only affects a bare CR at the very end.
fn legacy_lines(text: &str, max_lines: usize) -> Result<Vec<&str>, ImportedDictionaryFileError> {
    let mut result = Vec::new();
    let mut start = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            result.push(&text[start..index]);
            if result.len() >= max_lines {
                return Err(ImportedDictionaryFileError::new("too_many_entries"));
            }
            start = index + 1;
        }
    }
    result.push(&text[start..]);
    Ok(result)
}

fn source_text(bytes: &[u8]) -> Result<&str, ImportedDictionaryFileError> {
    if bytes.len() > MAX_IMPORTED_SOURCE_BYTES {
        return Err(ImportedDictionaryFileError::new("source_too_large"));
    }
    // String(data:encoding:.utf8) consumes one UTF-8 BOM, even for the native
    // format. Google's explicit BOM step can then consume a second one.
    std::str::from_utf8(bytes)
        .map(|text| text.strip_prefix('\u{feff}').unwrap_or(text))
        .map_err(|_| ImportedDictionaryFileError::new("invalid_encoding"))
}

fn normalize_entry(
    reading: &str,
    surface: &str,
    pos: &str,
    priority: UserDictionaryPriority,
    line: usize,
) -> Result<ImportedDictionaryRecord, ImportedDictionaryFileError> {
    // The old Imported enum rejects the User snapshot's historic "noun" alias.
    if pos == "noun" {
        return Err(ImportedDictionaryFileError::row(
            "unsupported_part_of_speech",
            line,
        ));
    }
    let pos = UserDictionaryPartOfSpeech::from_id(pos)
        .ok_or_else(|| ImportedDictionaryFileError::row("unsupported_part_of_speech", line))?;
    let record = normalized_record(reading.to_owned(), surface.to_owned(), pos, priority).map_err(
        |error| {
            ImportedDictionaryFileError::row(
                if error == UserDictionaryFileError::BaseForm {
                    "invalid_base_form"
                } else {
                    "invalid_row"
                },
                line,
            )
        },
    )?;
    Ok(ImportedDictionaryRecord {
        reading: record.reading,
        surface: record.surface,
        part_of_speech: record.part_of_speech,
        priority: record.priority,
    })
}

fn priority(value: &str) -> Option<UserDictionaryPriority> {
    match value {
        "high" => Some(UserDictionaryPriority::High),
        "normal" => Some(UserDictionaryPriority::Normal),
        "low" => Some(UserDictionaryPriority::Low),
        _ => None,
    }
}

fn priority_id(value: UserDictionaryPriority) -> &'static str {
    match value {
        UserDictionaryPriority::High => "high",
        UserDictionaryPriority::Normal => "normal",
        UserDictionaryPriority::Low => "low",
    }
}

fn expanded_count(record: &ImportedDictionaryRecord) -> usize {
    let definition = user_pos_definition(record.part_of_speech.as_id()).unwrap();
    match definition.behavior {
        UserPosBehavior::DormantSuggestion | UserPosBehavior::Suppression => 0,
        _ => definition.forms.len(),
    }
}

fn validate_entries(
    metadata: &ImportedDictionaryMetadata,
    entries: impl IntoIterator<Item = ImportedDictionaryRecord>,
) -> Result<Vec<ImportedDictionaryRecord>, ImportedDictionaryFileError> {
    let mut identities = BTreeSet::new();
    let mut semantic = metadata.semantic_bytes();
    let mut expanded = 0usize;
    let mut result = Vec::new();
    for entry in entries {
        let line = result.len() + 9;
        if result.len() >= MAX_IMPORTED_FILE_ENTRIES {
            return Err(ImportedDictionaryFileError::new("too_many_entries"));
        }
        let entry = normalize_entry(
            &entry.reading,
            &entry.surface,
            entry.part_of_speech.as_id(),
            entry.priority,
            line,
        )?;
        if !identities.insert((
            entry.reading.clone(),
            entry.surface.clone(),
            entry.part_of_speech.as_id().to_owned(),
        )) {
            return Err(ImportedDictionaryFileError::row("duplicate_entry", line));
        }
        semantic += entry.reading.len() + entry.surface.len() + 48;
        if semantic > MAX_IMPORTED_SEMANTIC_BYTES {
            return Err(ImportedDictionaryFileError::new("snapshot_too_large"));
        }
        expanded += expanded_count(&entry);
        if expanded > MAX_IMPORTED_EXPANDED_ENTRIES {
            return Err(ImportedDictionaryFileError::new("too_many_expanded_tokens"));
        }
        result.push(entry);
    }
    Ok(result)
}

/// Decode exchange bytes without IO; reading alone never generates a save.
pub fn decode_imported_dictionary_file(
    bytes: &[u8],
) -> Result<ImportedDictionaryDocument, ImportedDictionaryFileError> {
    let mut text = source_text(bytes)?;
    if text.graphemes(true).next_back() == Some("\n") {
        text = &text[..text.len() - 1];
    }
    let lines = legacy_lines(text, MAX_IMPORTED_FILE_ENTRIES + 8)?;
    let lines: Vec<_> = lines
        .into_iter()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.first().copied() != Some(HEADER) {
        return Err(ImportedDictionaryFileError::new("invalid_header"));
    }
    if lines.get(7).copied() != Some(COLUMNS) {
        return Err(ImportedDictionaryFileError::new("invalid_columns"));
    }
    let mut metadata: [Option<String>; 6] = Default::default();
    for (index, line) in lines[1..7].iter().enumerate() {
        let (key, value) = line
            .split_once('\t')
            .ok_or_else(|| ImportedDictionaryFileError::row("invalid_row", index + 2))?;
        let key = key
            .strip_prefix('#')
            .ok_or_else(|| ImportedDictionaryFileError::row("invalid_row", index + 2))?;
        let slot = METADATA
            .iter()
            .position(|(name, _)| *name == key)
            .ok_or_else(|| ImportedDictionaryFileError::new("invalid_metadata"))?;
        if metadata[slot].replace(value.to_owned()).is_some() {
            return Err(ImportedDictionaryFileError::field(
                "duplicate_metadata",
                METADATA[slot].0,
            ));
        }
    }
    let mut values = metadata.into_iter().enumerate().map(|(index, value)| {
        value.ok_or_else(|| {
            ImportedDictionaryFileError::field("missing_metadata", METADATA[index].0)
        })
    });
    let metadata = ImportedDictionaryMetadata {
        id: values.next().unwrap()?,
        name: values.next().unwrap()?,
        version: values.next().unwrap()?,
        author: values.next().unwrap()?,
        license: values.next().unwrap()?,
        provenance: values.next().unwrap()?,
    }
    .normalized()?;
    let mut entries = Vec::with_capacity(lines.len() - 8);
    for (index, line) in lines[8..].iter().enumerate() {
        let number = index + 9;
        let fields: Vec<_> = line.splitn(5, '\t').collect();
        if fields.len() != 4 {
            return Err(ImportedDictionaryFileError::row("invalid_row", number));
        }
        let priority = priority(fields[3])
            .ok_or_else(|| ImportedDictionaryFileError::row("invalid_row", number))?;
        entries.push(normalize_entry(
            fields[0], fields[1], fields[2], priority, number,
        )?);
    }
    Ok(ImportedDictionaryDocument {
        entries: validate_entries(&metadata, entries)?,
        metadata,
    })
}

/// Preserve source row order. Runtime sorting has a different purpose: token IDs.
pub fn encode_imported_dictionary_file(
    document: &ImportedDictionaryDocument,
) -> Result<Vec<u8>, ImportedDictionaryFileError> {
    let metadata = document.metadata.normalized()?;
    if document.entries.len() > MAX_IMPORTED_FILE_ENTRIES {
        return Err(ImportedDictionaryFileError::new("too_many_entries"));
    }
    let entries = validate_entries(&metadata, document.entries.iter().cloned())?;
    let mut output = format!("{HEADER}\n");
    for ((key, _), value) in METADATA.iter().zip(metadata.values()) {
        output.push_str(&format!("#{key}\t{value}\n"));
    }
    output.push_str(COLUMNS);
    output.push('\n');
    for entry in entries {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            entry.reading,
            entry.surface,
            entry.part_of_speech.as_id(),
            priority_id(entry.priority)
        ));
        if output.len() > MAX_IMPORTED_SOURCE_BYTES {
            return Err(ImportedDictionaryFileError::new("source_too_large"));
        }
    }
    Ok(output.into_bytes())
}

const GOOGLE_POS: [(&str, &str); 45] = [
    ("品詞なし", "NO_POS"),
    ("名詞", "NOUN"),
    ("短縮よみ", "ABBREVIATION"),
    ("サジェストのみ", "SUGGESTION_ONLY"),
    ("固有名詞", "PROPER_NOUN"),
    ("人名", "PERSONAL_NAME"),
    ("姓", "FAMILY_NAME"),
    ("名", "FIRST_NAME"),
    ("組織", "ORGANIZATION_NAME"),
    ("地名", "PLACE_NAME"),
    ("名詞サ変", "SA_IRREGULAR_CONJUGATION_NOUN"),
    ("名詞形動", "ADJECTIVE_VERBAL_NOUN"),
    ("数", "NUMBER"),
    ("アルファベット", "ALPHABET"),
    ("記号", "SYMBOL"),
    ("顔文字", "EMOTICON"),
    ("副詞", "ADVERB"),
    ("連体詞", "PRENOUN_ADJECTIVAL"),
    ("接続詞", "CONJUNCTION"),
    ("感動詞", "INTERJECTION"),
    ("接頭語", "PREFIX"),
    ("助数詞", "COUNTER_SUFFIX"),
    ("接尾一般", "GENERIC_SUFFIX"),
    ("接尾人名", "PERSON_NAME_SUFFIX"),
    ("接尾地名", "PLACE_NAME_SUFFIX"),
    ("動詞ワ行五段", "WA_GROUP1_VERB"),
    ("動詞カ行五段", "KA_GROUP1_VERB"),
    ("動詞サ行五段", "SA_GROUP1_VERB"),
    ("動詞タ行五段", "TA_GROUP1_VERB"),
    ("動詞ナ行五段", "NA_GROUP1_VERB"),
    ("動詞マ行五段", "MA_GROUP1_VERB"),
    ("動詞ラ行五段", "RA_GROUP1_VERB"),
    ("動詞ガ行五段", "GA_GROUP1_VERB"),
    ("動詞バ行五段", "BA_GROUP1_VERB"),
    ("動詞ハ行四段", "HA_GROUP1_VERB"),
    ("動詞一段", "GROUP2_VERB"),
    ("動詞カ変", "KURU_GROUP3_VERB"),
    ("動詞サ変", "SURU_GROUP3_VERB"),
    ("動詞ザ変", "ZURU_GROUP3_VERB"),
    ("動詞ラ変", "RU_GROUP3_VERB"),
    ("形容詞", "ADJECTIVE"),
    ("終助詞", "SENTENCE_ENDING_PARTICLE"),
    ("句読点", "PUNCTUATION"),
    ("独立語", "FREE_STANDING_WORD"),
    ("抑制単語", "SUPPRESSION_WORD"),
];

fn foundation_whitespace(ch: char) -> bool {
    ch.is_whitespace() || ch == '\u{200b}'
}

/// Detection is content-based. A picker extension cannot bypass validation.
//
// Google export (first line `!!DICUT…`): rows are `reading TAB surface TAB
// Japanese POS label [TAB comment]`, blank lines skipped, exact duplicates
// dropped rather than rejected, every row at normal priority. The result is
// converted to a native document with a content-derived id, then re-read
// through the native decoder so both paths share every check.
pub fn decode_import_source(
    bytes: &[u8],
    file_name: &str,
) -> Result<ImportedDictionaryDocument, ImportedDictionaryFileError> {
    let text = source_text(bytes)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let first_end = text
        .grapheme_indices(true)
        .find(|(_, g)| *g == "\n")
        .map_or(text.len(), |(index, _)| index);
    let first = &text[..first_end];
    if !first
        .trim_matches(foundation_whitespace)
        .starts_with("!!DICUT")
    {
        return decode_imported_dictionary_file(bytes);
    }
    let mut lines = legacy_lines(text, MAX_IMPORTED_FILE_ENTRIES + 2)?;
    if lines.last().copied() == Some("") {
        lines.pop();
    }
    if lines.len() > MAX_IMPORTED_FILE_ENTRIES + 1 {
        return Err(ImportedDictionaryFileError::new("too_many_entries"));
    }
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, line) in lines.into_iter().enumerate().skip(1) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.splitn(5, '\t').collect();
        if fields.len() != 3 && fields.len() != 4 {
            return Err(ImportedDictionaryFileError::row("invalid_row", index + 1));
        }
        let pos = GOOGLE_POS
            .iter()
            .find(|(label, _)| *label == fields[2])
            .ok_or_else(|| {
                ImportedDictionaryFileError::row("unsupported_part_of_speech", index + 1)
            })?
            .1;
        let entry = normalize_entry(
            fields[0],
            fields[1],
            pos,
            UserDictionaryPriority::Normal,
            index + 1,
        )?;
        if seen.insert((
            entry.reading.clone(),
            entry.surface.clone(),
            entry.part_of_speech.as_id().to_owned(),
        )) {
            entries.push(entry);
        }
    }
    let base = if let Some((stem, extension)) = file_name.rsplit_once('.') {
        if stem.is_empty() || extension.is_empty() {
            file_name
        } else {
            stem
        }
    } else {
        file_name
    };
    let document = ImportedDictionaryDocument {
        metadata: ImportedDictionaryMetadata {
            id: format!("google.{}", &sha256(bytes)[..32]),
            name: if base.is_empty() {
                "Google日本語入力辞書".into()
            } else {
                base.into()
            },
            version: "imported".into(),
            author: "Google日本語入力ユーザー辞書".into(),
            license: "未指定".into(),
            provenance: format!("Google日本語入力エクスポート: {file_name}"),
        },
        entries,
    };
    decode_imported_dictionary_file(&encode_imported_dictionary_file(&document)?)
}

pub fn imported_dictionary_template() -> Vec<u8> {
    // These fixed data are covered by an old-Swift byte golden, not synthesized
    // by Swift when exporting the template after the format moves into Core.
    format!("{HEADER}\n#id\torg.example.dictionary\n#name\t辞書名\n#version\t1.0\n#author\t作者名\n#license\tライセンス\n#provenance\t出典または説明\n{COLUMNS}\nむう\tMuu\tNOUN\tnormal\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_and_google_acceptance_and_canonical_bytes_match_the_swift_oracle() {
        #[derive(Deserialize)]
        struct Corpus {
            oracle_commit: String,
            cases: Vec<Case>,
        }
        #[derive(Deserialize)]
        struct Case {
            id: String,
            kind: String,
            file_name: String,
            source: String,
            expected_accepted: bool,
            canonical: Option<String>,
        }
        // Captured from the legacy Swift codec, not regenerated by the codec under test.
        // Three leading-punctuation IDs intentionally extend that acceptance.
        let corpus: Corpus = serde_json::from_str(include_str!(
            "../tests/data/imported-dictionary-file-compat.json"
        ))
        .unwrap();
        assert_eq!(corpus.oracle_commit, "legacy Swift codec oracle");
        assert_eq!(corpus.cases.len(), 130);
        for case in corpus.cases {
            let decoded = if case.kind == "google" {
                decode_import_source(case.source.as_bytes(), &case.file_name)
            } else {
                decode_imported_dictionary_file(case.source.as_bytes())
            };
            let result = decoded.and_then(|document| encode_imported_dictionary_file(&document));
            assert_eq!(result.is_ok(), case.expected_accepted, "{}", case.id);
            if let Some(expected) = case.canonical {
                assert_eq!(result.unwrap(), expected.as_bytes(), "{}", case.id);
            }
        }
    }

    #[test]
    fn native_canonical_bytes_and_source_order_match_legacy() {
        let bytes = imported_dictionary_template();
        let mut document = decode_imported_dictionary_file(&bytes).unwrap();
        assert_eq!(encode_imported_dictionary_file(&document).unwrap(), bytes);
        assert!(decode_imported_dictionary_file(&bytes[..bytes.len() - 1]).is_ok());
        assert!(
            decode_imported_dictionary_file(
                &String::from_utf8(bytes.clone())
                    .unwrap()
                    .replace('\n', "\r\n")
                    .into_bytes()
            )
            .is_err()
        );
        document.entries.push(ImportedDictionaryRecord {
            reading: "あ".into(),
            surface: "亜".into(),
            part_of_speech: UserDictionaryPartOfSpeech::noun(),
            priority: UserDictionaryPriority::High,
        });
        let encoded = encode_imported_dictionary_file(&document).unwrap();
        assert_eq!(
            decode_imported_dictionary_file(&encoded).unwrap().entries,
            document.entries
        );
    }

    #[test]
    fn google_normalizes_deduplicates_and_keeps_raw_source_hash() {
        let bytes =
            "\u{feff}!!DICUT1\nムウ\tMuu\t名詞\tfirst\nむう\tMuu\t名詞\tsecond\n".as_bytes();
        let document = decode_import_source(bytes, "例.txt").unwrap();
        assert_eq!(document.entries.len(), 1);
        assert_eq!(document.entries[0].reading, "むう");
        assert_eq!(
            document.metadata.id,
            format!("google.{}", &sha256(bytes)[..32])
        );
        assert_eq!(document.metadata.name, "例");
        assert!(decode_import_source(b"!!DICUT\na\tb\tunknown\n", "test").is_err());
    }

    #[test]
    fn legacy_ha_and_uppercase_remain_readable_but_runtime_rejects_them() {
        let mut document =
            decode_imported_dictionary_file(&imported_dictionary_template()).unwrap();
        document.metadata.id = "Example.upper".into();
        let snapshot = crate::ImportedDictionaryCatalogSnapshot {
            schema_version: 1,
            dictionaries: vec![crate::ImportedDictionarySnapshot {
                dictionary_id: document.metadata.id.clone(),
                enabled: true,
                entries: document.entries.clone(),
            }],
        };
        assert!(
            decode_imported_dictionary_file(&encode_imported_dictionary_file(&document).unwrap())
                .is_ok()
        );
        assert_eq!(
            snapshot.into_overlay().unwrap_err(),
            crate::ImportedDictionaryError::InvalidDictionaryId
        );
        document.metadata.id = "example.lower".into();
        document.entries[0] = normalize_entry(
            "ならう",
            "習ふ",
            "HA_GROUP1_VERB",
            UserDictionaryPriority::Normal,
            9,
        )
        .unwrap();
        assert!(encode_imported_dictionary_file(&document).is_ok());
        let runtime = crate::ImportedDictionaryCatalogSnapshot {
            schema_version: 1,
            dictionaries: vec![crate::ImportedDictionarySnapshot {
                dictionary_id: document.metadata.id.clone(),
                enabled: true,
                entries: document.entries,
            }],
        };
        assert_eq!(
            runtime.into_overlay().unwrap_err(),
            crate::ImportedDictionaryError::InvalidBaseForm
        );
    }

    #[test]
    fn all_google_labels_are_unique_and_backed_by_runtime_pos() {
        let mut labels = BTreeSet::new();
        for (label, id) in GOOGLE_POS {
            assert!(labels.insert(label));
            assert!(user_pos_definition(id).is_some());
        }
        assert_eq!(labels.len(), 45);
    }
}
