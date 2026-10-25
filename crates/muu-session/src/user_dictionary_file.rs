//! The user file is a Rust protocol, not a Swift model (rule 5 of docs/src/internals/architecture.md「2 つに分けている理由」; docs/src/internals/storage.md「Swift が書き、Rust が形式を決める」).
//! Management retains every decodable row even when the whole runtime layer
//! must be empty. Keeping those two states separate makes deletion a repair
//! operation without silently adopting only a prefix of a user's dictionary.

use crate::StoredFileState;
use std::sync::Arc;

use muu_conversion::{LexiconOverlay, UserPosBehavior, user_pos_definition};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::user_dictionary::{valid_user_dictionary_reading, valid_user_dictionary_surface};
use crate::{
    MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES, UserDictionaryError, UserDictionaryPartOfSpeech,
    UserDictionaryPriority, UserDictionaryRecord, UserDictionarySnapshot,
};

pub const MAX_USER_DICTIONARY_EDIT_BYTES: usize = 64 * 1024;
pub const MAX_USER_DICTIONARY_TICKET_BYTES: usize = 1024;
pub const MAX_USER_DICTIONARY_VIEW_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
// File format: this header line, then one row per entry:
// `reading TAB surface TAB pos TAB priority TAB crc32`, LF-terminated, where
// crc32 is eight hex digits over the first four fields and their tabs.
// Writers emit rows sorted and a final LF; readers accept unsorted rows and
// a missing final LF, but never CR.
const HEADER: &str = "# muu-user-dictionary\t1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserDictionaryFileError {
    Unreadable,
    TooLarge,
    Encoding,
    Header,
    Row,
    Checksum,
    Reading,
    Surface,
    PartOfSpeech,
    BaseForm,
    Duplicate,
    TooManyEntries,
}

impl UserDictionaryFileError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unreadable => "user_dictionary_unreadable",
            Self::TooLarge => "user_dictionary_file_too_large",
            Self::Encoding => "user_dictionary_invalid_encoding",
            Self::Header => "user_dictionary_invalid_header",
            Self::Row => "user_dictionary_invalid_row",
            Self::Checksum => "user_dictionary_checksum_mismatch",
            Self::Reading => "user_dictionary_invalid_reading",
            Self::Surface => "user_dictionary_invalid_surface",
            Self::PartOfSpeech => "user_dictionary_invalid_part_of_speech",
            Self::BaseForm => "user_dictionary_invalid_base_form",
            Self::Duplicate => "user_dictionary_duplicate_entry",
            Self::TooManyEntries => "user_dictionary_too_many_entries",
        }
    }
}

fn priority_id(priority: UserDictionaryPriority) -> &'static str {
    match priority {
        UserDictionaryPriority::High => "high",
        UserDictionaryPriority::Normal => "normal",
        UserDictionaryPriority::Low => "low",
    }
}

fn validate_record(entry: &UserDictionaryRecord) -> Result<(), UserDictionaryFileError> {
    let definition = user_pos_definition(entry.part_of_speech.as_id())
        .ok_or(UserDictionaryFileError::PartOfSpeech)?;
    let suppression = definition.behavior == UserPosBehavior::Suppression;
    if !(suppression && entry.reading.is_empty() || valid_user_dictionary_reading(&entry.reading)) {
        return Err(UserDictionaryFileError::Reading);
    }
    if !(suppression && entry.surface.is_empty() || valid_user_dictionary_surface(&entry.surface))
        || (entry.reading.is_empty() && entry.surface.is_empty())
    {
        return Err(UserDictionaryFileError::Surface);
    }
    if entry.surface.chars().any(legacy_control_character) {
        return Err(UserDictionaryFileError::Surface);
    }
    // Preserve the legacy file boundary independently of runtime expansion:
    // HA_GROUP1_VERB expects reading う/surface ふ in the old Swift editor,
    // while the pinned runtime table expects ふ/ふ. The file parser must not
    // silently widen acceptance. Such a readable file retains all management rows and
    // degrades as a whole; correcting the POS definition is a separate change
    // (the compatibility rule in docs/src/internals/storage.md「壊れたファイルは上書きしない」).
    let reading_suffix = if definition.id == "HA_GROUP1_VERB" {
        "う"
    } else {
        definition.base_reading_suffix
    };
    if !entry.reading.ends_with(reading_suffix)
        || !entry.surface.ends_with(definition.base_surface_suffix)
    {
        return Err(UserDictionaryFileError::BaseForm);
    }
    Ok(())
}

// The old Foundation CharacterSet.controlCharacters includes format scalars,
// not just Rust char::is_control (Cc). Pin that file-format boundary in Rust
// rather than delegating it to the installed OS (docs/src/internals/storage.md「Swift が書き、Rust が形式を決める」). These ranges are
// checked against the archived Swift codec oracle, including ZWJ/emoji cases.
pub(crate) fn legacy_control_character(ch: char) -> bool {
    matches!(ch as u32, 0x0..=0x1f | 0x7f..=0x9f | 0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x890..=0x891 | 0x8e2 | 0x180e | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x206f | 0xfeff | 0xfff9..=0xfffb | 0x110bd | 0x110cd | 0x13430..=0x1343f | 0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f)
}

fn sorted(
    mut entries: Vec<UserDictionaryRecord>,
) -> Result<UserDictionarySnapshot, UserDictionaryFileError> {
    if entries.len() > MAX_ENTRIES {
        return Err(UserDictionaryFileError::TooManyEntries);
    }
    for entry in &entries {
        validate_record(entry)?;
    }
    entries.sort_by(|a, b| {
        (
            &a.reading,
            &a.surface,
            a.part_of_speech.as_id(),
            priority_id(a.priority),
        )
            .cmp(&(
                &b.reading,
                &b.surface,
                b.part_of_speech.as_id(),
                priority_id(b.priority),
            ))
    });
    if entries
        .windows(2)
        .any(|pair| same_identity(&pair[0], &pair[1]))
    {
        return Err(UserDictionaryFileError::Duplicate);
    }
    Ok(UserDictionarySnapshot {
        schema_version: 1,
        entries,
    })
}

fn same_identity(a: &UserDictionaryRecord, b: &UserDictionaryRecord) -> bool {
    a.reading == b.reading
        && a.surface == b.surface
        && a.part_of_speech.as_id() == b.part_of_speech.as_id()
}

/// CRC32 is only the file's accidental-corruption check, not authentication.
/// This is the same IEEE polynomial and byte sequence as the legacy codec.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Reading never rewrites: callers keep the original bytes on disk. In
/// particular, accepting legacy `noun`, unsorted rows or missing terminal LF
/// does not imply migrating the file (docs/src/internals/storage.md「壊れたファイルは上書きしない」). CRLF is not Swift's LF Character
/// separator, and must not accidentally become accepted through `lines()`.
pub fn decode_user_dictionary_file(
    bytes: &[u8],
) -> Result<UserDictionarySnapshot, UserDictionaryFileError> {
    if bytes.len() > MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES {
        return Err(UserDictionaryFileError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| UserDictionaryFileError::Encoding)?;
    let mut lines: Vec<_> = text.splitn(MAX_ENTRIES + 2, '\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.len() > MAX_ENTRIES + 1 {
        return Err(UserDictionaryFileError::TooManyEntries);
    }
    if lines.first() != Some(&HEADER) {
        return Err(UserDictionaryFileError::Header);
    }
    let mut entries = Vec::with_capacity(lines.len().saturating_sub(1));
    for line in lines.into_iter().skip(1) {
        let fields: Vec<_> = line.splitn(6, '\t').collect();
        if fields.len() != 5 {
            return Err(UserDictionaryFileError::Row);
        }
        let part_of_speech = if fields[2] == "noun" {
            Some(UserDictionaryPartOfSpeech::noun())
        } else {
            UserDictionaryPartOfSpeech::from_id(fields[2])
        }
        .ok_or(UserDictionaryFileError::PartOfSpeech)?;
        let priority = match fields[3] {
            "high" => UserDictionaryPriority::High,
            "normal" => UserDictionaryPriority::Normal,
            "low" => UserDictionaryPriority::Low,
            _ => return Err(UserDictionaryFileError::Row),
        };
        // The old UInt32(radix:16) accepts more than eight digits when they
        // are leading zeroes, and a leading `+`; `from_str_radix` accepts the
        // same. The writer is canonical; the reader is not a second, narrower
        // file format.
        let expected =
            u32::from_str_radix(fields[4], 16).map_err(|_| UserDictionaryFileError::Row)?;
        let payload_end = line.rfind('\t').ok_or(UserDictionaryFileError::Row)?;
        if crc32(&line.as_bytes()[..payload_end]) != expected {
            return Err(UserDictionaryFileError::Checksum);
        }
        entries.push(UserDictionaryRecord {
            reading: fields[0].into(),
            surface: fields[1].into(),
            part_of_speech,
            priority,
        });
    }
    sorted(entries)
}

pub fn encode_user_dictionary_file(
    snapshot: &UserDictionarySnapshot,
) -> Result<Vec<u8>, UserDictionaryFileError> {
    if snapshot.schema_version != 1 {
        return Err(UserDictionaryFileError::Header);
    }
    let snapshot = sorted(snapshot.entries.clone())?;
    let mut text = format!("{HEADER}\n");
    for entry in snapshot.entries {
        let payload = format!(
            "{}\t{}\t{}\t{}",
            entry.reading,
            entry.surface,
            entry.part_of_speech.as_id(),
            priority_id(entry.priority)
        );
        use std::fmt::Write;
        writeln!(text, "{payload}\t{:08x}", crc32(payload.as_bytes()))
            .map_err(|_| UserDictionaryFileError::TooLarge)?;
        if text.len() > MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES {
            return Err(UserDictionaryFileError::TooLarge);
        }
    }
    Ok(text.into_bytes())
}

/// UI input is an explicit edit, so width/kana/NFC conversion belongs here.
/// Files instead require byte-for-byte NFC; normalizing them would conceal
/// corruption and turn an innocent read into a migration.
pub(crate) fn normalized_record(
    reading: String,
    surface: String,
    part_of_speech: UserDictionaryPartOfSpeech,
    priority: UserDictionaryPriority,
) -> Result<UserDictionaryRecord, UserDictionaryFileError> {
    if reading.len() > 512 {
        return Err(UserDictionaryFileError::Reading);
    }
    if surface.len() > 4096 {
        return Err(UserDictionaryFileError::Surface);
    }
    // Normalize half-width kana runs only. NFKC of the whole field would also
    // admit compatibility letters that the original editor rejects.
    let mut widened = String::new();
    let mut half = String::new();
    for ch in reading.chars() {
        if ('\u{ff61}'..='\u{ff9f}').contains(&ch) {
            half.push(ch);
            continue;
        }
        widened.extend(half.nfkc());
        half.clear();
        widened.push(ch);
    }
    widened.extend(half.nfkc());
    let reading: String = widened
        .chars()
        .map(|ch| match ch {
            '\u{30a1}'..='\u{30f6}' => char::from_u32(ch as u32 - 0x60).unwrap_or(ch),
            'ヽ' => 'ゝ',
            'ヾ' => 'ゞ',
            _ => ch,
        })
        .nfc()
        .collect();
    let entry = UserDictionaryRecord {
        reading,
        surface: surface.nfc().collect(),
        part_of_speech,
        priority,
    };
    validate_record(&entry)?;
    Ok(entry)
}

#[derive(Clone)]
pub(crate) struct UserDictionaryManagement {
    pub revision: u64,
    pub snapshot: Option<Arc<UserDictionarySnapshot>>,
    pub file_error: Option<UserDictionaryFileError>,
    pub layer_error: Option<UserDictionaryError>,
}

impl Default for UserDictionaryManagement {
    fn default() -> Self {
        Self {
            revision: 0,
            snapshot: Some(Arc::new(UserDictionarySnapshot::default())),
            file_error: None,
            layer_error: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UserDictionaryView {
    pub schema_version: u32,
    pub management_revision: u64,
    pub access: &'static str,
    pub reason: Option<&'static str>,
    pub layer_status: &'static str,
    pub entries: Option<Vec<UserDictionaryViewEntry>>,
    pub entry_count: Option<usize>,
    pub dormant_count: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct UserDictionaryViewEntry {
    pub entry_id: String,
    #[serde(flatten)]
    pub record: UserDictionaryRecord,
}

impl UserDictionaryManagement {
    pub fn view(&self) -> UserDictionaryView {
        UserDictionaryView {
            schema_version: 1,
            management_revision: self.revision,
            access: if self.snapshot.is_some() {
                "editable"
            } else {
                "read_only"
            },
            reason: self
                .file_error
                .map(UserDictionaryFileError::code)
                .or(self.layer_error.map(|_| "user_dictionary_layer_invalid")),
            layer_status: if self.layer_error.is_some() || self.file_error.is_some() {
                "empty_degraded"
            } else {
                "active"
            },
            entries: self.snapshot.as_ref().map(|s| {
                s.entries
                    .iter()
                    .enumerate()
                    // An ID names a row of this revision only. Edits carry
                    // the revision too, so an ID from a stale view is refused
                    // as a mismatch instead of naming whichever row now sits
                    // at that index.
                    .map(|(i, record)| UserDictionaryViewEntry {
                        entry_id: format!("{}:{i}", self.revision),
                        record: {
                            let mut row = record.clone();
                            // The legacy JSON entry may carry `noun`; the
                            // display protocol always uses the known POS ID.
                            if let Some(pos) =
                                UserDictionaryPartOfSpeech::from_id(row.part_of_speech.as_id())
                            {
                                row.part_of_speech = pos;
                            }
                            row
                        },
                    })
                    .collect()
            }),
            entry_count: self.snapshot.as_ref().map(|s| s.entries.len()),
            dormant_count: self.snapshot.as_ref().map(|s| {
                s.entries
                    .iter()
                    .filter(|e| {
                        user_pos_definition(e.part_of_speech.as_id())
                            .is_some_and(|d| d.behavior == UserPosBehavior::DormantSuggestion)
                    })
                    .count()
            }),
        }
    }

    pub(crate) fn from_file(
        state: StoredFileState,
        bytes: &[u8],
    ) -> (Self, UserDictionarySnapshot, Arc<LexiconOverlay>) {
        let result = match state {
            StoredFileState::Missing => Ok(UserDictionarySnapshot::default()),
            StoredFileState::Present => decode_user_dictionary_file(bytes),
            StoredFileState::Unreadable => Err(UserDictionaryFileError::Unreadable),
        };
        match result {
            Ok(snapshot) => Self::from_snapshot(snapshot, 0),
            Err(error) => (
                Self {
                    snapshot: None,
                    file_error: Some(error),
                    ..Self::default()
                },
                UserDictionarySnapshot::default(),
                Arc::default(),
            ),
        }
    }

    pub(crate) fn from_snapshot(
        snapshot: UserDictionarySnapshot,
        revision: u64,
    ) -> (Self, UserDictionarySnapshot, Arc<LexiconOverlay>) {
        let result = snapshot.clone().into_overlay();
        let layer_error = result.as_ref().err().copied();
        let active = if layer_error.is_none() {
            snapshot.clone()
        } else {
            UserDictionarySnapshot::default()
        };
        (
            Self {
                revision,
                snapshot: Some(Arc::new(snapshot)),
                file_error: None,
                layer_error,
            },
            active,
            Arc::new(result.unwrap_or_default()),
        )
    }

    // Applies one edit to this management state and returns the next state,
    // its runtime layer, the overlay and the canonical file bytes. Nothing is
    // published here; the caller reserves a ticket and publishes after the
    // host has written the file. An edit that would turn a valid dictionary
    // into an invalid layer is refused; on an already invalid one, only
    // removal is allowed.
    pub(crate) fn edit(
        &self,
        command: UserDictionaryEditCommand,
    ) -> Result<(Self, UserDictionarySnapshot, Arc<LexiconOverlay>, Vec<u8>), UserDictionaryEditError>
    {
        if command.expected_revision() != self.revision {
            return Err(UserDictionaryEditError::RevisionMismatch);
        }
        let source = self
            .snapshot
            .as_ref()
            .ok_or(UserDictionaryEditError::ReadOnly)?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(UserDictionaryEditError::RevisionOverflow)?;
        let mut entries = source.entries.clone();
        match command {
            UserDictionaryEditCommand::Remove { entry_id, .. } => {
                let index = self.entry_index(&entry_id)?;
                entries.remove(index);
            }
            UserDictionaryEditCommand::Upsert {
                entry_id,
                reading,
                surface,
                part_of_speech,
                priority,
                ..
            } => {
                // Degraded content is repaired only by deletion, even if an
                // upsert might happen to fix it (docs/src/internals/storage.md「壊れたファイルは上書きしない」). Otherwise an editor
                // could keep adding unrelated rows to an unusable dictionary.
                if self.layer_error.is_some() {
                    return Err(UserDictionaryEditError::DeletionRequired);
                }
                let entry = normalized_record(reading, surface, part_of_speech, priority)
                    .map_err(UserDictionaryEditError::File)?;
                if let Some(id) = entry_id {
                    entries.remove(self.entry_index(&id)?);
                }
                if let Some(old) = entries.iter_mut().find(|old| same_identity(old, &entry)) {
                    *old = entry;
                } else {
                    entries.push(entry);
                }
            }
        }
        let snapshot = sorted(entries).map_err(UserDictionaryEditError::File)?;
        let file = encode_user_dictionary_file(&snapshot).map_err(UserDictionaryEditError::File)?;
        let (next, active, overlay) = Self::from_snapshot(snapshot, revision);
        if self.layer_error.is_none() && next.layer_error.is_some() {
            return Err(UserDictionaryEditError::LayerInvalid);
        }
        Ok((next, active, overlay, file))
    }

    fn entry_index(&self, id: &str) -> Result<usize, UserDictionaryEditError> {
        let entries = &self
            .snapshot
            .as_ref()
            .ok_or(UserDictionaryEditError::ReadOnly)?
            .entries;
        (0..entries.len())
            .find(|i| id == format!("{}:{i}", self.revision))
            .ok_or(UserDictionaryEditError::EntryNotFound)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserDictionaryEditCommand {
    Upsert {
        expected_revision: u64,
        entry_id: Option<String>,
        reading: String,
        surface: String,
        part_of_speech: UserDictionaryPartOfSpeech,
        priority: UserDictionaryPriority,
    },
    Remove {
        expected_revision: u64,
        entry_id: String,
    },
}

impl UserDictionaryEditCommand {
    pub fn expected_revision(&self) -> u64 {
        match self {
            Self::Upsert {
                expected_revision, ..
            }
            | Self::Remove {
                expected_revision, ..
            } => *expected_revision,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDictionaryEditError {
    CoreInvalid,
    RevisionMismatch,
    Busy,
    ReadOnly,
    DeletionRequired,
    EntryNotFound,
    RevisionOverflow,
    LayerInvalid,
    InvalidTicket,
    OutputTooLarge,
    File(UserDictionaryFileError),
}

impl UserDictionaryEditError {
    pub fn class(&self) -> crate::EditErrorClass {
        use crate::EditErrorClass;
        match self {
            Self::Busy => EditErrorClass::Busy,
            Self::RevisionMismatch => EditErrorClass::RevisionMismatch,
            Self::CoreInvalid => EditErrorClass::CoreInvalid,
            Self::RevisionOverflow | Self::OutputTooLarge => EditErrorClass::Internal,
            Self::ReadOnly
            | Self::DeletionRequired
            | Self::EntryNotFound
            | Self::LayerInvalid
            | Self::InvalidTicket
            | Self::File(_) => EditErrorClass::Rejected,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::CoreInvalid => "core_invalid",
            Self::RevisionMismatch => "user_dictionary_revision_mismatch",
            Self::Busy => "user_dictionary_busy",
            Self::ReadOnly => "user_dictionary_read_only",
            Self::DeletionRequired => "user_dictionary_deletion_required",
            Self::EntryNotFound => "user_dictionary_entry_not_found",
            Self::RevisionOverflow => "user_dictionary_revision_overflow",
            Self::LayerInvalid => "user_dictionary_layer_invalid",
            Self::InvalidTicket => "user_dictionary_invalid_ticket",
            Self::OutputTooLarge => "user_dictionary_output_too_large",
            Self::File(error) => error.code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> UserDictionaryRecord {
        UserDictionaryRecord {
            reading: "むう".into(),
            surface: "Muu".into(),
            part_of_speech: UserDictionaryPartOfSpeech::noun(),
            priority: UserDictionaryPriority::High,
        }
    }
    fn file(payload: &str, crc: Option<String>, ending: &str) -> Vec<u8> {
        format!(
            "{HEADER}\n{payload}\t{}{ending}",
            crc.unwrap_or_else(|| format!("{:08x}", crc32(payload.as_bytes())))
        )
        .into_bytes()
    }
    #[test]
    fn legacy_noun_crc_spellings_and_unterminated_rows_are_read_without_migration() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let payload = "むう\tMuu\tnoun\thigh";
        let crc = crc32(payload.as_bytes());
        for spelling in [
            format!("{crc:08x}"),
            format!("{crc:08X}"),
            format!("000{crc:08x}"),
            format!("+{crc:08x}"),
        ] {
            for ending in ["", "\n"] {
                let input = file(payload, Some(spelling.clone()), ending);
                let snapshot = decode_user_dictionary_file(&input).unwrap();
                assert_eq!(snapshot.entries, vec![record()]);
                let canonical = encode_user_dictionary_file(&snapshot).unwrap();
                // Pinned legacy-writer bytes, including the canonical POS,
                // checksum spelling and final LF; not just a round trip.
                assert_eq!(
                    canonical,
                    "# muu-user-dictionary\t1\nむう\tMuu\tNOUN\thigh\t442110f2\n".as_bytes()
                );
                // Codec returns a value, never an IO instruction to rewrite
                // the noncanonical-but-valid input (docs/src/internals/storage.md「壊れたファイルは上書きしない」).
                assert!(String::from_utf8(input).unwrap().contains("\tnoun\t"));
            }
        }
    }
    #[test]
    fn malformed_structure_crc_nfc_and_duplicate_identity_are_read_only() {
        for bytes in [
            Vec::new(),
            b"# muu-user-dictionary\t1\r\n".to_vec(),
            file("むう\tMuu\tNOUN\thigh", None, "\r\n"),
            b"# muu-user-dictionary\t1\n\n".to_vec(),
            b"# muu-user-dictionary\t2\n".to_vec(),
            vec![0xff],
        ] {
            assert!(decode_user_dictionary_file(&bytes).is_err());
            let (management, active, _) =
                UserDictionaryManagement::from_file(StoredFileState::Present, &bytes);
            assert!(management.view().entries.is_none());
            assert!(active.entries.is_empty());
        }
        assert_eq!(
            decode_user_dictionary_file(&file("むう\tMuu\tNOUN\thigh", Some("0".into()), "\n")),
            Err(UserDictionaryFileError::Checksum)
        );
        assert_eq!(
            decode_user_dictionary_file(&file("か\u{3099}\t語\tNOUN\thigh", None, "\n")),
            Err(UserDictionaryFileError::Reading)
        );
        assert_eq!(
            decode_user_dictionary_file(&file("むう\te\u{301}\tNOUN\thigh", None, "\n")),
            Err(UserDictionaryFileError::Surface)
        );
        let mut duplicate = record();
        duplicate.priority = UserDictionaryPriority::Low;
        assert_eq!(
            sorted(vec![record(), duplicate]),
            Err(UserDictionaryFileError::Duplicate)
        );
        let (missing, _, _) = UserDictionaryManagement::from_file(StoredFileState::Missing, &[]);
        let (unreadable, _, _) =
            UserDictionaryManagement::from_file(StoredFileState::Unreadable, &[]);
        assert_eq!(missing.view().entries.unwrap().len(), 0);
        assert!(unreadable.view().entries.is_none());
    }

    #[test]
    fn foundation_control_boundaries_match_legacy_file_surface_acceptance() {
        // These observations were captured from Foundation. Rust's
        // is_control covers fewer format scalars; using it alone would widen
        // the file protocol. Pin both sides of every observed range boundary.
        let table = include_str!("../tests/data/user-dictionary-control-boundaries.tsv");
        let observations = table
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>();
        // Ignore the descriptive header, but retain every observation byte.
        assert_eq!(
            muu_digest::sha256_hex(observations.join("\n").as_bytes()),
            "cd63e9a72f6f7b577e954524ffe768cc4dfbc095e259e2d367f8aa9ea59d73ff"
        );
        let mut checked = 0;
        for line in observations {
            let (hex, acceptance) = line.split_once('\t').unwrap();
            let scalar = char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap();
            let rejected = match acceptance {
                "reject" => true,
                "accept" => false,
                other => panic!("invalid compatibility observation: {other}"),
            };
            assert_eq!(legacy_control_character(scalar), rejected, "U+{hex}");
            let payload = format!("むう\tA{scalar}B\tNOUN\thigh");
            assert_eq!(
                decode_user_dictionary_file(&file(&payload, None, "\n")).is_err(),
                rejected,
                "file surface U+{hex}"
            );
            checked += 1;
        }
        assert_eq!(checked, 80);
    }

    #[test]
    fn all_45_pos_match_legacy_canonical_bytes_and_preserve_the_ha_layer_difference() {
        // One self-written row per POS from the legacy Swift fixtures,
        // encoded by the fixed Swift oracle. Keep the expected IDs too:
        // a duplicate or omitted POS must not hide behind the row count.
        let golden = include_bytes!("../tests/data/user-dictionary-v1-all-pos.tsv");
        let mut snapshot = decode_user_dictionary_file(golden).unwrap();
        let expected_ids = "ABBREVIATION ADJECTIVE ADJECTIVE_VERBAL_NOUN ADVERB ALPHABET
            BA_GROUP1_VERB CONJUNCTION COUNTER_SUFFIX EMOTICON FAMILY_NAME FIRST_NAME
            FREE_STANDING_WORD GA_GROUP1_VERB GENERIC_SUFFIX GROUP2_VERB HA_GROUP1_VERB
            INTERJECTION KA_GROUP1_VERB KURU_GROUP3_VERB MA_GROUP1_VERB NA_GROUP1_VERB
            NOUN NO_POS NUMBER ORGANIZATION_NAME PERSONAL_NAME PERSON_NAME_SUFFIX
            PLACE_NAME PLACE_NAME_SUFFIX PREFIX PRENOUN_ADJECTIVAL PROPER_NOUN PUNCTUATION
            RA_GROUP1_VERB RU_GROUP3_VERB SA_GROUP1_VERB SA_IRREGULAR_CONJUGATION_NOUN
            SENTENCE_ENDING_PARTICLE SUGGESTION_ONLY SUPPRESSION_WORD SURU_GROUP3_VERB
            SYMBOL TA_GROUP1_VERB WA_GROUP1_VERB ZURU_GROUP3_VERB"
            .split_whitespace()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(snapshot.entries.len(), 45);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.part_of_speech.as_id())
                .collect::<std::collections::BTreeSet<_>>(),
            expected_ids
        );
        assert_eq!(encode_user_dictionary_file(&snapshot).unwrap(), golden);
        snapshot.entries.reverse();
        assert_eq!(encode_user_dictionary_file(&snapshot).unwrap(), golden);
        for entry in &snapshot.entries {
            let one = UserDictionarySnapshot {
                schema_version: 1,
                entries: vec![entry.clone()],
            };
            if entry.part_of_speech.as_id() == "HA_GROUP1_VERB" {
                // Codec compatibility (う/ふ) and the existing runtime (ふ/ふ)
                // stay independent until the separate POS ruling. Neither
                // silently widening the codec nor partially adopting is safe.
                assert_eq!(
                    one.into_overlay().err(),
                    Some(UserDictionaryError::InvalidBaseForm)
                );
                let mut runtime_entry = entry.clone();
                runtime_entry.reading = format!("{}ふ", entry.reading.strip_suffix('う').unwrap());
                let runtime = UserDictionarySnapshot {
                    schema_version: 1,
                    entries: vec![runtime_entry],
                };
                assert!(runtime.clone().into_overlay().is_ok());
                assert_eq!(
                    encode_user_dictionary_file(&runtime),
                    Err(UserDictionaryFileError::BaseForm)
                );
            } else {
                assert!(
                    one.into_overlay().is_ok(),
                    "{}",
                    entry.part_of_speech.as_id()
                );
            }
        }
        let (management, active, _) =
            UserDictionaryManagement::from_file(StoredFileState::Present, golden);
        assert_eq!(management.view().entry_count, Some(45));
        assert_eq!(management.view().layer_status, "empty_degraded");
        assert!(active.entries.is_empty());
    }

    #[test]
    fn explicit_editor_normalization_preserves_field_and_base_form_limits() {
        for reading in ["ﾑｳ", "ムウ", "むう"] {
            assert_eq!(
                normalized_record(
                    reading.into(),
                    "e\u{301}".into(),
                    UserDictionaryPartOfSpeech::noun(),
                    UserDictionaryPriority::High
                )
                .unwrap()
                .reading,
                "むう"
            );
        }
        assert_eq!(
            normalized_record(
                "ヴー".into(),
                "語".into(),
                UserDictionaryPartOfSpeech::noun(),
                UserDictionaryPriority::High
            )
            .unwrap()
            .reading,
            "ゔー"
        );
        for reading in ["むう\0", "ABC", "㌀", ""] {
            assert!(
                normalized_record(
                    reading.into(),
                    "語".into(),
                    UserDictionaryPartOfSpeech::noun(),
                    UserDictionaryPriority::High
                )
                .is_err()
            );
        }
        let mut entry = record();
        entry.reading = "あ".repeat(128);
        assert!(validate_record(&entry).is_ok());
        entry.reading.push('あ');
        assert_eq!(
            validate_record(&entry),
            Err(UserDictionaryFileError::Reading)
        );
        entry = record();
        entry.surface = "語".repeat(256);
        assert!(validate_record(&entry).is_ok());
        entry.surface.push('語');
        assert_eq!(
            validate_record(&entry),
            Err(UserDictionaryFileError::Surface)
        );
        entry = record();
        entry.surface = format!("a{}", "\u{301}".repeat(2100));
        assert_eq!(
            validate_record(&entry),
            Err(UserDictionaryFileError::Surface)
        );
    }
    #[test]
    fn file_count_and_size_limits_do_not_partially_adopt_rows() {
        let entries: Vec<_> = (0..MAX_ENTRIES)
            .map(|i| {
                let mut e = record();
                e.surface = format!("語{i:05}");
                e
            })
            .collect();
        let mut snapshot = sorted(entries).unwrap();
        assert_eq!(
            decode_user_dictionary_file(&encode_user_dictionary_file(&snapshot).unwrap()).unwrap(),
            snapshot
        );
        snapshot.entries.push(record());
        assert_eq!(
            encode_user_dictionary_file(&snapshot),
            Err(UserDictionaryFileError::TooManyEntries)
        );
        assert_eq!(
            decode_user_dictionary_file(&vec![b'a'; MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES + 1]),
            Err(UserDictionaryFileError::TooLarge)
        );
    }
}
