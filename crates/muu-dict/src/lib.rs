//! Memory-mapped access to Muu's frozen conversion dictionary format 1.1.
//!
//! The connection matrix deliberately names three different concepts:
//!
//! - Source-file columns are `(right_id, left_id, cost)`.
//! - The public API is
//!   [`Dictionary::connection_cost(previous_right_id, next_left_id)`].
//! - Physical storage is a dense array at
//!   `previous_right_id + right_axis_size * next_left_id`.
//!
//! Keeping those concepts separate prevents a plausible-looking matrix
//! transpose from silently changing conversion paths.

mod builder;
mod format;

use std::fmt;
use std::fs::File;
use std::path::Path;
use std::str;

pub use builder::{DictionaryBuilder, LexiconEntry};
pub use format::IntegerWidths;
use format::{Header, SectionKind, read_signed, read_unsigned};
use memmap2::Mmap;
use sha2::{Digest, Sha256};
use yada::DoubleArray;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeftContextId(u16);

impl LeftContextId {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RightContextId(u16);

impl RightContextId {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PosId(u16);

impl PosId {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Stable semantic class used by conversion ranking.
///
/// This is deliberately separate from [`PosId`]: POS describes grammatical
/// connection behavior, while a lexical class describes what kind of name or
/// symbol the token represents.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum LexicalClass {
    #[default]
    General = 0,
    ProperOther = 1,
    Person = 2,
    Place = 3,
    Organization = 4,
    Symbol = 5,
}

impl LexicalClass {
    pub const ALL: &'static [Self] = &[
        Self::General,
        Self::ProperOther,
        Self::Person,
        Self::Place,
        Self::Organization,
        Self::Symbol,
    ];

    pub const fn stable_name(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::ProperOther => "proper_other",
            Self::Person => "person",
            Self::Place => "place",
            Self::Organization => "organization",
            Self::Symbol => "symbol",
        }
    }

    pub fn from_stable_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|class| class.stable_name() == name)
    }

    fn from_raw(value: u8) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|class| *class as u8 == value)
    }
}

/// Stable coarse POS classes derived from the first field of Mozc `id.def`.
///
/// Raw Mozc context IDs are deliberately not exposed as segmenter POS IDs:
/// their roughly 2,700 inflection-specific values are too fine-grained for a
/// human-maintained boundary rule table. Left and right classes remain
/// separate so a compound token never loses either endpoint category.
macro_rules! define_coarse_pos {
    ($(($variant:ident, $id:literal, $stable:literal, $mozc_major:literal)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(u16)]
        pub enum CoarsePos {
            $($variant = $id,)+
        }

        impl CoarsePos {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            pub const fn id(self) -> PosId {
                PosId::new(self as u16)
            }

            pub const fn stable_name(self) -> &'static str {
                match self {
                    $(Self::$variant => $stable,)+
                }
            }

            pub fn from_stable_name(name: &str) -> Option<Self> {
                match name {
                    $($stable => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub fn from_mozc_major(name: &str) -> Option<Self> {
                match name {
                    $($mozc_major => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub const fn rust_variant_name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant),)+
                }
            }
        }
    };
}

define_coarse_pos!(
    (BosEos, 0, "bos_eos", "BOS/EOS"),
    (Other, 1, "other", "その他"),
    (Filler, 2, "filler", "フィラー"),
    (Adverb, 3, "adverb", "副詞"),
    (AuxiliaryVerb, 4, "auxiliary_verb", "助動詞"),
    (Particle, 5, "particle", "助詞"),
    (Verb, 6, "verb", "動詞"),
    (Noun, 7, "noun", "名詞"),
    (Adjective, 8, "adjective", "形容詞"),
    (Interjection, 9, "interjection", "感動詞"),
    (Conjunction, 10, "conjunction", "接続詞"),
    (Prefix, 11, "prefix", "接頭詞"),
    (Symbol, 12, "symbol", "記号"),
    (Prenominal, 13, "prenominal", "連体詞"),
);

// A token's index in the Tokens section. The builder orders tokens by reading
// bytes, then surface, ids, class and cost, so an id is meaningful only for
// the artifact that issued it; see `DictionaryFingerprint`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DictionaryTokenId(u64);

impl DictionaryTokenId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Stable identity of the immutable dictionary payload.
///
/// The binary header stays private. Consumers only receive the two values
/// required to reject data bound to a different dictionary artifact: saved
/// learning refers to token ids, which mean nothing in another build, and
/// conversion records name the dictionary they were produced with.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DictionaryFingerprint {
    payload_sha256: [u8; 32],
    token_count: u64,
}

impl DictionaryFingerprint {
    pub const fn new(payload_sha256: [u8; 32], token_count: u64) -> Self {
        Self {
            payload_sha256,
            token_count,
        }
    }

    pub const fn payload_sha256(&self) -> &[u8; 32] {
        &self.payload_sha256
    }

    pub const fn token_count(self) -> u64 {
        self.token_count
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum UnknownClass {
    Number = 1,
    AsciiAlphabetic = 2,
    Katakana = 3,
    Kanji = 4,
    Other = 255,
}

impl TryFrom<u8> for UnknownClass {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Number),
            2 => Ok(Self::AsciiAlphabetic),
            3 => Ok(Self::Katakana),
            4 => Ok(Self::Kanji),
            255 => Ok(Self::Other),
            _ => Err("unknown character class value"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownDefinition {
    pub class: UnknownClass,
    pub group: bool,
    pub max_length: u32,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos_id: PosId,
    pub right_pos_id: PosId,
    pub word_cost: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token<'a> {
    pub surface: &'a str,
    pub token_id: DictionaryTokenId,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos_id: PosId,
    pub right_pos_id: PosId,
    pub lexical_class: LexicalClass,
    pub word_cost: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixMatch<'a> {
    pub reading_length_bytes: usize,
    pub tokens: Vec<Token<'a>>,
}

#[derive(Debug)]
pub enum DictionaryError {
    Io(std::io::Error),
    Format(&'static str),
    Utf8(str::Utf8Error),
}

impl fmt::Display for DictionaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "dictionary I/O failed: {error}"),
            Self::Format(message) => write!(formatter, "invalid dictionary: {message}"),
            Self::Utf8(error) => write!(formatter, "dictionary string pool is not UTF-8: {error}"),
        }
    }
}

impl std::error::Error for DictionaryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Utf8(error) => Some(error),
            Self::Format(_) => None,
        }
    }
}

impl From<std::io::Error> for DictionaryError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<str::Utf8Error> for DictionaryError {
    fn from(value: str::Utf8Error) -> Self {
        Self::Utf8(value)
    }
}

pub struct Dictionary {
    mmap: Mmap,
    header: Header,
}

impl Dictionary {
    /// Opens and validates a read-only dictionary mapping.
    ///
    /// Installed dictionaries are immutable artifacts. Writers must publish a
    /// replacement with an atomic rename instead of modifying the mapped inode
    /// in place; this is the filesystem invariant required by `memmap2`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DictionaryError> {
        let file = File::open(path)?;
        // SAFETY: Muu owns installed dictionary artifacts and treats an inode as
        // immutable for the lifetime of every mapping. Updates use atomic rename.
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        let header = Header::decode(&mmap).map_err(DictionaryError::Format)?;
        let dictionary = Self { mmap, header };
        dictionary.validate()?;
        Ok(dictionary)
    }

    pub fn widths(&self) -> IntegerWidths {
        self.header.widths
    }

    pub fn fingerprint(&self) -> DictionaryFingerprint {
        DictionaryFingerprint::new(self.header.payload_sha256, self.header.token_count)
    }

    // Open validates both matrix membership and u16 range, including headers
    // from other writers whose declared context width is wider than u16, so
    // these conversions cannot fail on an opened dictionary.
    pub fn bos_right_id(&self) -> RightContextId {
        RightContextId::new(
            u16::try_from(self.header.bos_right_id).expect("BOS ID validated at open"),
        )
    }

    pub fn eos_left_id(&self) -> LeftContextId {
        LeftContextId::new(
            u16::try_from(self.header.eos_left_id).expect("EOS ID validated at open"),
        )
    }

    /// Returns the exact human-readable source provenance embedded at build time.
    pub fn provenance(&self) -> Result<&str, DictionaryError> {
        Ok(str::from_utf8(self.section(SectionKind::Provenance)?)?)
    }

    // Every dictionary reading that is a prefix of `input`, shortest first
    // (the DARTS order), each with all its tokens. Lengths are in bytes.
    pub fn common_prefix_search(
        &self,
        input: &str,
    ) -> Result<Vec<PrefixMatch<'_>>, DictionaryError> {
        let darts_bytes = self.section(SectionKind::Darts)?;
        // SAFETY: `Dictionary::open` validates this exact DARTS section with
        // `DoubleArray::new`, and the mmap inode is immutable for the lifetime
        // of `self`. Revalidating every prefix lookup would walk the complete
        // real-dictionary trie once per input position.
        let darts = unsafe { DoubleArray::new_unchecked(darts_bytes) };
        darts
            .common_prefix_search(input.as_bytes())
            .map(|(entry_index, reading_length_bytes)| {
                Ok(PrefixMatch {
                    reading_length_bytes,
                    tokens: self.tokens_for_entry(u64::from(entry_index))?,
                })
            })
            .collect()
    }

    /// Returns `matrix[previous.right_id][next.left_id]`.
    pub fn connection_cost(
        &self,
        previous_right_id: RightContextId,
        next_left_id: LeftContextId,
    ) -> Result<i32, DictionaryError> {
        let previous = u64::from(previous_right_id.get());
        let next = u64::from(next_left_id.get());
        if previous >= self.header.right_axis_size || next >= self.header.left_axis_size {
            return Err(DictionaryError::Format(
                "connection context ID is outside the matrix",
            ));
        }
        let index = previous
            .checked_add(
                self.header
                    .right_axis_size
                    .checked_mul(next)
                    .ok_or(DictionaryError::Format("connection matrix index overflow"))?,
            )
            .ok_or(DictionaryError::Format("connection matrix index overflow"))?;
        let byte_offset = usize::try_from(index)
            .ok()
            .and_then(|value| value.checked_mul(usize::from(self.header.widths.connection_cost)))
            .ok_or(DictionaryError::Format(
                "connection matrix byte offset overflow",
            ))?;
        let matrix = self.section(SectionKind::Connection)?;
        let mut cursor = byte_offset;
        read_signed(matrix, &mut cursor, self.header.widths.connection_cost)
            .map_err(DictionaryError::Format)
    }

    pub fn unknown_definition(
        &self,
        class: UnknownClass,
    ) -> Result<UnknownDefinition, DictionaryError> {
        self.find_unknown_definition(class)?
            .ok_or(DictionaryError::Format(
                "dictionary has no definition for the unknown class",
            ))
    }

    pub fn find_unknown_definition(
        &self,
        class: UnknownClass,
    ) -> Result<Option<UnknownDefinition>, DictionaryError> {
        let records = self.section(SectionKind::Unknown)?;
        let record_width = self.unknown_record_width()?;
        for index in 0..self.header.unknown_count {
            let start = usize::try_from(index)
                .ok()
                .and_then(|value| value.checked_mul(record_width))
                .ok_or(DictionaryError::Format(
                    "unknown definition offset overflow",
                ))?;
            let record = records
                .get(start..start + record_width)
                .ok_or(DictionaryError::Format("unknown definition is truncated"))?;
            let parsed = self.parse_unknown(record)?;
            if parsed.class == class {
                return Ok(Some(parsed));
            }
        }
        Ok(None)
    }

    fn tokens_for_entry(&self, entry_index: u64) -> Result<Vec<Token<'_>>, DictionaryError> {
        if entry_index >= self.header.entry_count {
            return Err(DictionaryError::Format(
                "DARTS value references a missing entry",
            ));
        }
        let record_width =
            usize::from(self.header.widths.token_index) + usize::from(self.header.widths.count);
        let start = usize::try_from(entry_index)
            .ok()
            .and_then(|value| value.checked_mul(record_width))
            .ok_or(DictionaryError::Format("entry record offset overflow"))?;
        let entries = self.section(SectionKind::Entries)?;
        let record = entries
            .get(start..start + record_width)
            .ok_or(DictionaryError::Format("entry record is truncated"))?;
        let mut cursor = 0;
        let token_start = read_unsigned(record, &mut cursor, self.header.widths.token_index)
            .map_err(DictionaryError::Format)?;
        let token_count = read_unsigned(record, &mut cursor, self.header.widths.count)
            .map_err(DictionaryError::Format)?;
        let end = token_start
            .checked_add(token_count)
            .ok_or(DictionaryError::Format("token span overflow"))?;
        if end > self.header.token_count {
            return Err(DictionaryError::Format(
                "entry references tokens outside the token array",
            ));
        }
        (token_start..end)
            .map(|index| self.parse_token(index))
            .collect()
    }

    fn parse_token(&self, index: u64) -> Result<Token<'_>, DictionaryError> {
        let record_width = self.token_record_width()?;
        let start = usize::try_from(index)
            .ok()
            .and_then(|value| value.checked_mul(record_width))
            .ok_or(DictionaryError::Format("token record offset overflow"))?;
        let tokens = self.section(SectionKind::Tokens)?;
        let record = tokens
            .get(start..start + record_width)
            .ok_or(DictionaryError::Format("token record is truncated"))?;
        let mut cursor = 0;
        let surface_offset = read_unsigned(record, &mut cursor, self.header.widths.offset)
            .map_err(DictionaryError::Format)?;
        let surface_length = read_unsigned(record, &mut cursor, self.header.widths.string_length)
            .map_err(DictionaryError::Format)?;
        let left_id = read_unsigned(record, &mut cursor, self.header.widths.context_id)
            .map_err(DictionaryError::Format)?;
        let right_id = read_unsigned(record, &mut cursor, self.header.widths.context_id)
            .map_err(DictionaryError::Format)?;
        let left_pos_id = read_unsigned(record, &mut cursor, self.header.widths.pos_id)
            .map_err(DictionaryError::Format)?;
        let right_pos_id = read_unsigned(record, &mut cursor, self.header.widths.pos_id)
            .map_err(DictionaryError::Format)?;
        let lexical_class = *record
            .get(cursor)
            .ok_or(DictionaryError::Format("token lexical class is truncated"))?;
        cursor += 1;
        let word_cost = read_signed(record, &mut cursor, self.header.widths.word_cost)
            .map_err(DictionaryError::Format)?;

        let strings = self.section(SectionKind::Strings)?;
        let start = usize::try_from(surface_offset)
            .map_err(|_| DictionaryError::Format("surface offset is too large"))?;
        let end = start
            .checked_add(
                usize::try_from(surface_length)
                    .map_err(|_| DictionaryError::Format("surface length is too large"))?,
            )
            .ok_or(DictionaryError::Format("surface range overflow"))?;
        let surface = str::from_utf8(strings.get(start..end).ok_or(DictionaryError::Format(
            "surface references bytes outside the string pool",
        ))?)?;
        Ok(Token {
            surface,
            token_id: DictionaryTokenId::new(index),
            left_id: LeftContextId::new(
                u16::try_from(left_id)
                    .map_err(|_| DictionaryError::Format("left context ID exceeds u16"))?,
            ),
            right_id: RightContextId::new(
                u16::try_from(right_id)
                    .map_err(|_| DictionaryError::Format("right context ID exceeds u16"))?,
            ),
            left_pos_id: PosId::new(
                u16::try_from(left_pos_id)
                    .map_err(|_| DictionaryError::Format("left POS ID exceeds u16"))?,
            ),
            right_pos_id: PosId::new(
                u16::try_from(right_pos_id)
                    .map_err(|_| DictionaryError::Format("right POS ID exceeds u16"))?,
            ),
            lexical_class: LexicalClass::from_raw(lexical_class)
                .ok_or(DictionaryError::Format("unknown token lexical class"))?,
            word_cost,
        })
    }

    fn parse_unknown(&self, record: &[u8]) -> Result<UnknownDefinition, DictionaryError> {
        let class = UnknownClass::try_from(record[0]).map_err(DictionaryError::Format)?;
        let group = match record[1] {
            0 => false,
            1 => true,
            _ => {
                return Err(DictionaryError::Format(
                    "unknown grouping flag is not boolean",
                ));
            }
        };
        let mut cursor = 2;
        let max_length = read_unsigned(record, &mut cursor, self.header.widths.count)
            .map_err(DictionaryError::Format)?;
        let left_id = read_unsigned(record, &mut cursor, self.header.widths.context_id)
            .map_err(DictionaryError::Format)?;
        let right_id = read_unsigned(record, &mut cursor, self.header.widths.context_id)
            .map_err(DictionaryError::Format)?;
        let left_pos_id = read_unsigned(record, &mut cursor, self.header.widths.pos_id)
            .map_err(DictionaryError::Format)?;
        let right_pos_id = read_unsigned(record, &mut cursor, self.header.widths.pos_id)
            .map_err(DictionaryError::Format)?;
        let word_cost = read_signed(record, &mut cursor, self.header.widths.word_cost)
            .map_err(DictionaryError::Format)?;
        Ok(UnknownDefinition {
            class,
            group,
            max_length: u32::try_from(max_length)
                .map_err(|_| DictionaryError::Format("unknown max length exceeds u32"))?,
            left_id: LeftContextId::new(
                u16::try_from(left_id)
                    .map_err(|_| DictionaryError::Format("left context ID exceeds u16"))?,
            ),
            right_id: RightContextId::new(
                u16::try_from(right_id)
                    .map_err(|_| DictionaryError::Format("right context ID exceeds u16"))?,
            ),
            left_pos_id: PosId::new(
                u16::try_from(left_pos_id)
                    .map_err(|_| DictionaryError::Format("left POS ID exceeds u16"))?,
            ),
            right_pos_id: PosId::new(
                u16::try_from(right_pos_id)
                    .map_err(|_| DictionaryError::Format("right POS ID exceeds u16"))?,
            ),
            word_cost,
        })
    }

    // The whole file is checked once at open, so later lookups can trust it:
    // size and payload hash, section bounds and overlap, each section's
    // record count, BOS/EOS and every token's context ids inside the matrix
    // (BOS/EOS also within u16, which their accessors return),
    // the DARTS index, UTF-8 strings, a non-empty provenance, and every entry
    // and unknown record. The payload hash only detects corruption; it is
    // declared by the file itself and authenticates nothing.
    fn validate(&self) -> Result<(), DictionaryError> {
        if self.header.file_size
            != u64::try_from(self.mmap.len())
                .map_err(|_| DictionaryError::Format("mapped file is too large"))?
        {
            return Err(DictionaryError::Format(
                "declared file size does not match the mapping",
            ));
        }
        let header_size = u64::from(self.header.header_size);
        let payload_start = usize::try_from(header_size)
            .map_err(|_| DictionaryError::Format("header size exceeds address space"))?;
        let payload = self
            .mmap
            .get(payload_start..)
            .ok_or(DictionaryError::Format("header extends beyond the file"))?;
        if Sha256::digest(payload).as_slice() != self.header.payload_sha256 {
            return Err(DictionaryError::Format(
                "dictionary payload SHA-256 does not match",
            ));
        }
        let mut ranges = Vec::new();
        for kind in [
            SectionKind::Darts,
            SectionKind::Entries,
            SectionKind::Tokens,
            SectionKind::Strings,
            SectionKind::Connection,
            SectionKind::Unknown,
            SectionKind::Provenance,
        ] {
            let section = self.header.section(kind);
            if section.offset < header_size {
                return Err(DictionaryError::Format("section overlaps the header"));
            }
            let range = section
                .range(self.mmap.len())
                .map_err(DictionaryError::Format)?;
            ranges.push(range);
        }
        ranges.sort_by_key(|range| range.start);
        if ranges.windows(2).any(|pair| pair[0].end > pair[1].start) {
            return Err(DictionaryError::Format("dictionary sections overlap"));
        }

        let entry_width =
            usize::from(self.header.widths.token_index) + usize::from(self.header.widths.count);
        self.validate_section_records(SectionKind::Entries, self.header.entry_count, entry_width)?;
        self.validate_section_records(
            SectionKind::Tokens,
            self.header.token_count,
            self.token_record_width()?,
        )?;
        self.validate_section_records(
            SectionKind::Unknown,
            self.header.unknown_count,
            self.unknown_record_width()?,
        )?;
        let matrix_count = self
            .header
            .right_axis_size
            .checked_mul(self.header.left_axis_size)
            .ok_or(DictionaryError::Format("connection matrix size overflow"))?;
        self.validate_section_records(
            SectionKind::Connection,
            matrix_count,
            usize::from(self.header.widths.connection_cost),
        )?;
        if self.header.bos_right_id >= self.header.right_axis_size
            || self.header.eos_left_id >= self.header.left_axis_size
        {
            return Err(DictionaryError::Format(
                "BOS/EOS context ID is outside the matrix",
            ));
        }
        if u16::try_from(self.header.bos_right_id).is_err()
            || u16::try_from(self.header.eos_left_id).is_err()
        {
            return Err(DictionaryError::Format("BOS/EOS context ID exceeds u16"));
        }
        DoubleArray::new(self.section(SectionKind::Darts)?)
            .map_err(|_| DictionaryError::Format("DARTS index is malformed"))?;
        str::from_utf8(self.section(SectionKind::Strings)?)?;
        let provenance = str::from_utf8(self.section(SectionKind::Provenance)?)?;
        if provenance.is_empty() {
            return Err(DictionaryError::Format("dictionary provenance is empty"));
        }

        for entry in 0..self.header.entry_count {
            self.tokens_for_entry(entry)?;
        }
        for token in 0..self.header.token_count {
            let parsed = self.parse_token(token)?;
            if u64::from(parsed.left_id.get()) >= self.header.left_axis_size
                || u64::from(parsed.right_id.get()) >= self.header.right_axis_size
            {
                return Err(DictionaryError::Format(
                    "token context ID is outside the matrix",
                ));
            }
        }
        for index in 0..self.header.unknown_count {
            let record_width = self.unknown_record_width()?;
            let start = usize::try_from(index)
                .ok()
                .and_then(|value| value.checked_mul(record_width))
                .ok_or(DictionaryError::Format(
                    "unknown definition offset overflow",
                ))?;
            let records = self.section(SectionKind::Unknown)?;
            let definition = self.parse_unknown(
                records
                    .get(start..start + record_width)
                    .ok_or(DictionaryError::Format("unknown definition is truncated"))?,
            )?;
            if u64::from(definition.left_id.get()) >= self.header.left_axis_size
                || u64::from(definition.right_id.get()) >= self.header.right_axis_size
            {
                return Err(DictionaryError::Format(
                    "unknown context ID is outside the matrix",
                ));
            }
        }
        Ok(())
    }

    fn validate_section_records(
        &self,
        kind: SectionKind,
        count: u64,
        record_width: usize,
    ) -> Result<(), DictionaryError> {
        let expected = usize::try_from(count)
            .ok()
            .and_then(|value| value.checked_mul(record_width))
            .ok_or(DictionaryError::Format("section record size overflow"))?;
        if self.section(kind)?.len() != expected {
            return Err(DictionaryError::Format(
                "section byte length does not match its record count",
            ));
        }
        Ok(())
    }

    // Token record: surface offset, surface length, left id, right id, left
    // POS, right POS, one lexical-class byte, word cost.
    fn token_record_width(&self) -> Result<usize, DictionaryError> {
        [
            self.header.widths.offset,
            self.header.widths.string_length,
            self.header.widths.context_id,
            self.header.widths.context_id,
            self.header.widths.pos_id,
            self.header.widths.pos_id,
            1,
            self.header.widths.word_cost,
        ]
        .into_iter()
        .try_fold(0_usize, |total, width| {
            total
                .checked_add(usize::from(width))
                .ok_or(DictionaryError::Format("token record width overflow"))
        })
    }

    // Unknown record: class byte, group flag byte, max length, left id, right
    // id, left POS, right POS, word cost.
    fn unknown_record_width(&self) -> Result<usize, DictionaryError> {
        [
            self.header.widths.count,
            self.header.widths.context_id,
            self.header.widths.context_id,
            self.header.widths.pos_id,
            self.header.widths.pos_id,
            self.header.widths.word_cost,
        ]
        .into_iter()
        .try_fold(2_usize, |total, width| {
            total
                .checked_add(usize::from(width))
                .ok_or(DictionaryError::Format("unknown definition width overflow"))
        })
    }

    fn section(&self, kind: SectionKind) -> Result<&[u8], DictionaryError> {
        let range = self
            .header
            .section(kind)
            .range(self.mmap.len())
            .map_err(DictionaryError::Format)?;
        Ok(&self.mmap[range])
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn coarse_pos_names_and_mozc_classes_share_one_round_trip_authority() {
        let expected = [
            ("BosEos", "bos_eos", "BOS/EOS"),
            ("Other", "other", "その他"),
            ("Filler", "filler", "フィラー"),
            ("Adverb", "adverb", "副詞"),
            ("AuxiliaryVerb", "auxiliary_verb", "助動詞"),
            ("Particle", "particle", "助詞"),
            ("Verb", "verb", "動詞"),
            ("Noun", "noun", "名詞"),
            ("Adjective", "adjective", "形容詞"),
            ("Interjection", "interjection", "感動詞"),
            ("Conjunction", "conjunction", "接続詞"),
            ("Prefix", "prefix", "接頭詞"),
            ("Symbol", "symbol", "記号"),
            ("Prenominal", "prenominal", "連体詞"),
        ];
        assert_eq!(CoarsePos::ALL.len(), expected.len());
        for (index, (coarse, expected)) in CoarsePos::ALL.iter().zip(expected).enumerate() {
            assert_eq!(coarse.id(), PosId::new(index as u16));
            assert_eq!(coarse.rust_variant_name(), expected.0);
            assert_eq!(coarse.stable_name(), expected.1);
            assert_eq!(CoarsePos::from_stable_name(expected.1), Some(*coarse));
            assert_eq!(CoarsePos::from_mozc_major(expected.2), Some(*coarse));
        }
        assert_eq!(CoarsePos::from_stable_name("unknown"), None);
        assert_eq!(CoarsePos::from_mozc_major("未知"), None);
    }

    fn open_bytes(bytes: &[u8]) -> Dictionary {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        Dictionary::open(file.path()).unwrap()
    }

    fn add_dummy_entry(builder: &mut DictionaryBuilder) {
        builder.provenance(b"name = \"muu-dict-test\"\n");
        builder.add_entry(LexiconEntry {
            reading: "むう".into(),
            surface: "Muu".into(),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 0,
        });
    }

    #[test]
    fn fingerprint_exposes_payload_identity_without_exposing_the_header() {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        add_dummy_entry(&mut builder);
        builder.add_unknown(UnknownDefinition::default());
        let dictionary = open_bytes(&builder.build().unwrap());

        let fingerprint = dictionary.fingerprint();
        assert_eq!(
            fingerprint.payload_sha256(),
            &dictionary.header.payload_sha256
        );
        assert_eq!(fingerprint.token_count(), 1);
        assert_ne!(fingerprint.payload_sha256(), &[0; 32]);
    }

    #[test]
    fn mmap_dictionary_finds_all_common_prefix_tokens() {
        let mut builder =
            DictionaryBuilder::new(4, 4, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"prefix-search-test\"\n");
        builder.add_entry(LexiconEntry {
            reading: "か".into(),
            surface: "蚊".into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(2),
            left_pos_id: PosId::new(10),
            right_pos_id: PosId::new(20),
            word_cost: 30,
        });
        builder.add_entry(LexiconEntry {
            reading: "かな".into(),
            surface: "仮名".into(),
            left_id: LeftContextId::new(2),
            right_id: RightContextId::new(3),
            left_pos_id: PosId::new(11),
            right_pos_id: PosId::new(21),
            word_cost: 10,
        });
        builder.add_unknown(UnknownDefinition::default());
        let dictionary = open_bytes(&builder.build().unwrap());

        let matches = dictionary.common_prefix_search("かな").unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].reading_length_bytes, "か".len());
        assert_eq!(matches[0].tokens[0].surface, "蚊");
        assert_eq!(matches[1].reading_length_bytes, "かな".len());
        assert_eq!(matches[1].tokens[0].surface, "仮名");
    }

    #[test]
    fn dictionary_format_round_trips_lexical_class() {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"lexical-class-test\"\n");
        builder.add_entry_with_lexical_class(
            LexiconEntry {
                reading: "さとう".into(),
                surface: "佐藤".into(),
                left_id: LeftContextId::new(0),
                right_id: RightContextId::new(0),
                left_pos_id: PosId::new(0),
                right_pos_id: PosId::new(0),
                word_cost: 10,
            },
            LexicalClass::Person,
        );
        builder.add_unknown(UnknownDefinition::default());
        let dictionary = open_bytes(&builder.build().unwrap());

        let matches = dictionary.common_prefix_search("さとう").unwrap();
        assert_eq!(matches[0].tokens[0].lexical_class, LexicalClass::Person);
    }

    #[test]
    fn connection_axes_are_directional_and_not_transposable() {
        let mut builder =
            DictionaryBuilder::new(3, 3, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder
            .set_connection_cost(RightContextId::new(1), LeftContextId::new(2), 37)
            .unwrap();
        builder
            .set_connection_cost(RightContextId::new(2), LeftContextId::new(1), -91)
            .unwrap();
        add_dummy_entry(&mut builder);
        builder.add_unknown(UnknownDefinition::default());
        let dictionary = open_bytes(&builder.build().unwrap());

        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(1), LeftContextId::new(2))
                .unwrap(),
            37
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(2), LeftContextId::new(1))
                .unwrap(),
            -91
        );
    }

    #[test]
    fn bos_and_eos_contexts_are_independent_endpoints() {
        let mut builder =
            DictionaryBuilder::new(4, 4, RightContextId::new(3), LeftContextId::new(2)).unwrap();
        builder
            .set_connection_cost(RightContextId::new(3), LeftContextId::new(1), 17)
            .unwrap();
        builder
            .set_connection_cost(RightContextId::new(1), LeftContextId::new(2), -23)
            .unwrap();
        add_dummy_entry(&mut builder);
        builder.add_unknown(UnknownDefinition::default());
        let dictionary = open_bytes(&builder.build().unwrap());

        assert_eq!(dictionary.bos_right_id(), RightContextId::new(3));
        assert_eq!(dictionary.eos_left_id(), LeftContextId::new(2));
        assert_eq!(
            dictionary
                .connection_cost(dictionary.bos_right_id(), LeftContextId::new(1))
                .unwrap(),
            17
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(1), dictionary.eos_left_id())
                .unwrap(),
            -23
        );
    }

    // A 4-byte context width lets the header hold ids above u16. Each of
    // BOS and EOS at exactly u16::MAX still opens and reads back; one more is
    // refused as a Format error at open, before an accessor could panic.
    #[test]
    fn wide_bos_and_eos_ids_are_rejected_before_the_u16_accessors() {
        for bos in [true, false] {
            let (right, left) = if bos { (65_537, 1) } else { (1, 65_537) };
            let mut builder =
                DictionaryBuilder::new(right, left, RightContextId::new(0), LeftContextId::new(0))
                    .unwrap()
                    .widths(IntegerWidths {
                        context_id: 4,
                        ..IntegerWidths::default()
                    });
            add_dummy_entry(&mut builder);
            builder.add_unknown(UnknownDefinition::default());
            let mut bytes = builder.build().unwrap();
            let mut header = format::Header::decode(&bytes).unwrap();
            let file = NamedTempFile::new().unwrap();
            for endpoint in [u64::from(u16::MAX), u64::from(u16::MAX) + 1] {
                if bos {
                    header.bos_right_id = endpoint;
                } else {
                    header.eos_left_id = endpoint;
                }
                let encoded = header.encode().unwrap();
                bytes[..encoded.len()].copy_from_slice(&encoded);
                fs::write(file.path(), &bytes).unwrap();
                let opened = Dictionary::open(file.path());
                if endpoint == u64::from(u16::MAX) {
                    let dictionary = opened.expect("largest supported endpoint");
                    assert_eq!(
                        if bos {
                            dictionary.bos_right_id().get()
                        } else {
                            dictionary.eos_left_id().get()
                        },
                        u16::MAX
                    );
                } else {
                    assert!(
                        matches!(opened, Err(DictionaryError::Format(_))),
                        "wide endpoint must be rejected during open"
                    );
                }
            }
        }
    }

    #[test]
    fn declared_integer_widths_drive_record_decoding() {
        let widths = IntegerWidths {
            offset: 4,
            count: 2,
            trie_value: 4,
            token_index: 2,
            string_length: 2,
            context_id: 1,
            word_cost: 2,
            connection_cost: 2,
            pos_id: 1,
        };
        let mut builder =
            DictionaryBuilder::new(3, 3, RightContextId::new(0), LeftContextId::new(0))
                .unwrap()
                .widths(widths);
        builder.provenance(b"name = \"integer-width-test\"\n");
        builder.add_entry(LexiconEntry {
            reading: "かな".into(),
            surface: "仮名".into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(2),
            left_pos_id: PosId::new(7),
            right_pos_id: PosId::new(8),
            word_cost: -123,
        });
        builder
            .set_connection_cost(RightContextId::new(2), LeftContextId::new(1), 321)
            .unwrap();
        builder.add_unknown(UnknownDefinition::default());
        let dictionary = open_bytes(&builder.build().unwrap());

        assert_eq!(dictionary.widths(), widths);
        assert_eq!(
            dictionary.common_prefix_search("かな").unwrap()[0].tokens[0].word_cost,
            -123
        );
        assert_eq!(
            dictionary.common_prefix_search("かな").unwrap()[0].tokens[0].left_pos_id,
            PosId::new(7)
        );
        assert_eq!(
            dictionary.common_prefix_search("かな").unwrap()[0].tokens[0].right_pos_id,
            PosId::new(8)
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(2), LeftContextId::new(1))
                .unwrap(),
            321
        );
    }

    #[test]
    fn builder_output_is_independent_of_lexicon_insertion_order() {
        fn build(entries: impl IntoIterator<Item = LexiconEntry>) -> Vec<u8> {
            let mut builder =
                DictionaryBuilder::new(3, 3, RightContextId::new(0), LeftContextId::new(0))
                    .unwrap();
            builder.provenance(b"name = \"determinism-test\"\n");
            for entry in entries {
                builder.add_entry(entry);
            }
            builder.add_unknown(UnknownDefinition::default());
            builder.build().unwrap()
        }

        let entries = vec![
            LexiconEntry {
                reading: "かな".into(),
                surface: "仮名".into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(2),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(2),
                word_cost: 20,
            },
            LexiconEntry {
                reading: "か".into(),
                surface: "蚊".into(),
                left_id: LeftContextId::new(2),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(2),
                right_pos_id: PosId::new(1),
                word_cost: 30,
            },
        ];
        let mut reversed = entries.clone();
        reversed.reverse();

        assert_eq!(build(entries), build(reversed));
    }

    #[test]
    fn format_1_1_embeds_provenance_and_rejects_payload_corruption() {
        let mut builder =
            DictionaryBuilder::new(1, 1, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        add_dummy_entry(&mut builder);
        builder.add_unknown(UnknownDefinition::default());
        let mut bytes = builder.build().unwrap();

        assert_eq!(u16::from_le_bytes(bytes[8..10].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[10..12].try_into().unwrap()), 1);
        assert_eq!(
            open_bytes(&bytes).provenance().unwrap(),
            "name = \"muu-dict-test\"\n"
        );

        *bytes.last_mut().unwrap() ^= 1;
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let error = match Dictionary::open(file.path()) {
            Ok(_) => panic!("corrupted payload was accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "invalid dictionary: dictionary payload SHA-256 does not match"
        );
    }
}
