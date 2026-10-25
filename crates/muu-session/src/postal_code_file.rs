//! Postal file protocols. The host transports bytes; only Core interprets them.
//! Reading is deliberately distinct from canonical encoding: accepted legacy
//! spellings must not cause an automatic rewrite of an existing generation.
use crate::postal_code::{PostalCodeRecord, PostalCodeSnapshot};
use crate::stored_json::{catalog_encoding, fields, integer, required, string};
use crate::user_dictionary_file::legacy_control_character;
// The Postal names for the shared mechanism. The manifest decides what each
// hash covers: the ZIP bytes, the decompressed CSV and the canonical TSV.
pub(crate) use muu_digest::{is_sha256_hex as valid_hash, sha256_hex as hash};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{BufReader, Read};
use unicode_normalization::UnicodeNormalization;

pub const MAX_POSTAL_ZIP_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_POSTAL_CSV_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_POSTAL_TSV_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_POSTAL_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_POSTAL_ENTRIES: usize = 150_000;
pub const MAX_POSTAL_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_POSTAL_TICKET_BYTES: usize = 1024;
pub const MAX_POSTAL_VIEW_BYTES: usize = 1024 * 1024;
pub const POSTAL_SOURCE_URL: &str =
    "https://www.post.japanpost.jp/service/search/zipcode/download/utf/zip/utf_ken_all.zip";
pub const POSTAL_SOURCE_PAGE_URL: &str =
    "https://www.post.japanpost.jp/service/search/zipcode/download/utf-zip.html";
// Snapshot TSV: this header line, then `code TAB address` rows, LF only. Blank
// lines are skipped on read; an exact duplicate row is invalid.
const HEADER: &str = "# muu-postal-code\t1\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostalCodeFileError(pub &'static str);
impl std::fmt::Display for PostalCodeFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for PostalCodeFileError {}
impl From<crate::stored_json::StoredJsonError> for PostalCodeFileError {
    fn from(_: crate::stored_json::StoredJsonError) -> Self {
        Self("invalid_manifest")
    }
}

fn valid_reading(s: &str) -> bool {
    s.len() == 7 && s.bytes().all(|b| b.is_ascii_digit())
}
fn valid_surface(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 4096
        && s.nfc().eq(s.chars())
        && !s.chars().any(legacy_control_character)
}

pub fn decode_postal_code_tsv(bytes: &[u8]) -> Result<PostalCodeSnapshot, PostalCodeFileError> {
    if bytes.len() > MAX_POSTAL_TSV_BYTES {
        return Err(PostalCodeFileError("file_too_large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| PostalCodeFileError("invalid_snapshot"))?;
    let body = text
        .strip_prefix(HEADER)
        .ok_or(PostalCodeFileError("invalid_snapshot"))?;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for line in body.split('\n').filter(|s| !s.is_empty()) {
        let (reading, surface) = line
            .split_once('\t')
            .ok_or(PostalCodeFileError("invalid_snapshot"))?;
        if entries.len() >= MAX_POSTAL_ENTRIES
            || !valid_reading(reading)
            || !valid_surface(surface)
            || !seen.insert((reading, surface))
        {
            return Err(PostalCodeFileError("invalid_snapshot"));
        }
        entries.push(PostalCodeRecord {
            reading: reading.into(),
            surface: surface.into(),
        });
    }
    Ok(PostalCodeSnapshot {
        schema_version: 1,
        entries,
    })
}

pub fn encode_postal_code_tsv(
    snapshot: &PostalCodeSnapshot,
) -> Result<Vec<u8>, PostalCodeFileError> {
    if snapshot.schema_version != 1 || snapshot.entries.len() > MAX_POSTAL_ENTRIES {
        return Err(PostalCodeFileError("invalid_snapshot"));
    }
    let mut bytes = HEADER.as_bytes().to_vec();
    let mut seen = BTreeSet::new();
    for row in &snapshot.entries {
        if !valid_reading(&row.reading)
            || !valid_surface(&row.surface)
            || !seen.insert((&row.reading, &row.surface))
        {
            return Err(PostalCodeFileError("invalid_snapshot"));
        }
        if bytes
            .len()
            .saturating_add(row.reading.len() + row.surface.len() + 2)
            > MAX_POSTAL_TSV_BYTES
        {
            return Err(PostalCodeFileError("file_too_large"));
        }
        bytes.extend_from_slice(row.reading.as_bytes());
        bytes.push(b'\t');
        bytes.extend_from_slice(row.surface.as_bytes());
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Streaming CSV keeps one row, not the old [[String]] copy of the entire CSV.
/// Count nonempty source rows BEFORE deduplication, including the legacy EOF
/// behavior of an empty quoted field. Every column is checked as UTF-8.
//
// Japan Post's utf_ken_all.csv: 15 columns, the code in column 3 and the
// address as prefecture + city + town (columns 7-9). The town is dropped
// when it only says that no town follows ("以下に掲載がない場合", "…の次に番地が
// くる場合", "…一円"). CRLF and LF are accepted; a bare CR is not.
pub fn decode_postal_code_csv(
    reader: impl Read,
) -> Result<PostalCodeSnapshot, PostalCodeFileError> {
    let invalid = || PostalCodeFileError("invalid_csv");
    let mut bytes =
        BufReader::with_capacity(64 * 1024, reader.take((MAX_POSTAL_CSV_BYTES + 1) as u64))
            .bytes()
            .peekable();
    let mut size = 0;
    let mut row = Vec::<String>::new();
    let mut field = Vec::new();
    let mut quoted = false;
    let mut closed = false;
    let mut rows = 0;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let finish = |field: &mut Vec<u8>| -> Result<String, PostalCodeFileError> {
        String::from_utf8(std::mem::take(field))
            .map_err(|_| PostalCodeFileError("invalid_encoding"))
    };
    let mut consume = |row: &mut Vec<String>| -> Result<(), PostalCodeFileError> {
        rows += 1;
        if rows > MAX_POSTAL_ENTRIES || row.len() != 15 || !valid_reading(&row[2]) {
            return Err(invalid());
        }
        // Swift's String equality/hasSuffix compare canonical equivalents.
        // Normalize for the special-town predicate too, not just the eventual
        // concatenated surface (e.g. decomposed が in the omission phrase).
        let normalized_town: String = row[8].nfc().collect();
        let town = normalized_town.as_str();
        let town = if town == "以下に掲載がない場合"
            || town.ends_with("の次に番地がくる場合")
            || town.ends_with("一円")
        {
            ""
        } else {
            town
        };
        let surface: String = format!("{}{}{}", row[6], row[7], town).nfc().collect();
        if !valid_surface(&surface) {
            return Err(invalid());
        }
        if seen.insert((row[2].clone(), surface.clone())) {
            entries.push(PostalCodeRecord {
                reading: row[2].clone(),
                surface,
            });
        }
        row.clear();
        Ok(())
    };
    while let Some(byte) = bytes.next() {
        let byte = byte.map_err(|_| invalid())?;
        size += 1;
        if size > MAX_POSTAL_CSV_BYTES {
            return Err(PostalCodeFileError("file_too_large"));
        }
        if quoted {
            if byte == b'"' {
                if matches!(bytes.peek(), Some(Ok(b'"'))) {
                    bytes.next();
                    size += 1;
                    field.push(b'"');
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                field.push(byte);
            }
        } else {
            match byte {
                b'"' if !closed && field.is_empty() => quoted = true,
                b',' => {
                    row.push(finish(&mut field)?);
                    closed = false;
                    if row.len() > 15 {
                        return Err(invalid());
                    }
                }
                b'\r' => {
                    if !matches!(bytes.peek(), Some(Ok(b'\n'))) {
                        return Err(invalid());
                    }
                }
                b'\n' => {
                    row.push(finish(&mut field)?);
                    if row.iter().any(|s| !s.is_empty()) {
                        consume(&mut row)?;
                    } else {
                        row.clear();
                    }
                    closed = false;
                }
                _ if closed || byte == b'"' => return Err(invalid()),
                _ => field.push(byte),
            }
        }
        if size > MAX_POSTAL_CSV_BYTES {
            return Err(PostalCodeFileError("file_too_large"));
        }
    }
    if quoted {
        return Err(invalid());
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(finish(&mut field)?);
        consume(&mut row)?;
    }
    Ok(PostalCodeSnapshot {
        schema_version: 1,
        entries,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostalCodeManifest {
    pub schema_version: u32,
    pub source_url: String,
    pub source_page_url: String,
    pub source_last_modified: String,
    pub retrieved_at: String,
    pub zip_sha256: String,
    pub csv_sha256: String,
    pub snapshot_sha256: String,
    pub entry_count: usize,
}

pub fn decode_postal_code_manifest(
    bytes: &[u8],
    tsv: &[u8],
    count: usize,
) -> Result<PostalCodeManifest, PostalCodeFileError> {
    if bytes.len() > MAX_POSTAL_MANIFEST_BYTES {
        return Err(PostalCodeFileError("file_too_large"));
    }
    let text = catalog_encoding(bytes)?;
    let f = fields(
        &text,
        &[
            "schema_version",
            "source_url",
            "source_page_url",
            "source_last_modified",
            "retrieved_at",
            "zip_sha256",
            "csv_sha256",
            "snapshot_sha256",
            "entry_count",
        ],
    )?;
    let m = PostalCodeManifest {
        schema_version: u32::try_from(integer(required(&f, "schema_version")?)?)
            .map_err(|_| PostalCodeFileError("invalid_manifest"))?,
        source_url: string(required(&f, "source_url")?)?,
        source_page_url: string(required(&f, "source_page_url")?)?,
        source_last_modified: string(required(&f, "source_last_modified")?)?,
        retrieved_at: string(required(&f, "retrieved_at")?)?,
        zip_sha256: string(required(&f, "zip_sha256")?)?,
        csv_sha256: string(required(&f, "csv_sha256")?)?,
        snapshot_sha256: string(required(&f, "snapshot_sha256")?)?,
        entry_count: usize::try_from(integer(required(&f, "entry_count")?)?)
            .map_err(|_| PostalCodeFileError("invalid_manifest"))?,
    };
    validate_manifest(&m, tsv, count)?;
    Ok(m)
}
// A manifest describes exactly one TSV: fixed source URLs, a single-line
// Last-Modified, a canonical UTC retrieval time, three SHA-256 values of which
// the snapshot hash must match these TSV bytes, and the row count.
fn validate_manifest(
    m: &PostalCodeManifest,
    tsv: &[u8],
    count: usize,
) -> Result<(), PostalCodeFileError> {
    let lm = &m.source_last_modified;
    if m.schema_version != 1
        || m.source_url != POSTAL_SOURCE_URL
        || m.source_page_url != POSTAL_SOURCE_PAGE_URL
        || lm.is_empty()
        || lm.len() > 256
        || lm.chars().any(|c| {
            matches!(
                c,
                '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        })
        || !canonical_utc_seconds(&m.retrieved_at)
        || !valid_hash(&m.zip_sha256)
        || !valid_hash(&m.csv_sha256)
        || !valid_hash(&m.snapshot_sha256)
        || m.snapshot_sha256 != hash(tsv)
        || m.entry_count != count
    {
        return Err(PostalCodeFileError("invalid_manifest"));
    }
    Ok(())
}
pub fn encode_postal_code_manifest(m: &PostalCodeManifest) -> Result<Vec<u8>, PostalCodeFileError> {
    let bytes = crate::stored_json::foundation_json(m)?;
    if bytes.len() > MAX_POSTAL_MANIFEST_BYTES {
        return Err(PostalCodeFileError("file_too_large"));
    }
    Ok(bytes)
}

/// Saved dates follow the old writer, not Foundation's permissive parser:
/// UTC, ASCII digits, whole seconds, and an actual Gregorian calendar date.
/// The default ISO8601DateFormatter has GMT offset 0 and no fractional-second
/// option. It emits four-digit years (more digits after year 9999), never an
/// offset, variable-width fields, normalized invalid days, or trailing text.
fn canonical_utc_seconds(text: &str) -> bool {
    let Some((year, rest)) = text.split_once('-') else {
        return false;
    };
    if year.len() < 4
        || !year.bytes().all(|b| b.is_ascii_digit())
        || (year.len() > 4 && year.starts_with('0'))
    {
        return false;
    }
    let Ok(year) = year.parse::<u64>() else {
        return false;
    };
    let b = rest.as_bytes();
    if b.len() != 15
        || b[2] != b'-'
        || b[5] != b'T'
        || b[8] != b':'
        || b[11] != b':'
        || b[14] != b'Z'
        || ![0, 1, 3, 4, 6, 7, 9, 10, 12, 13]
            .into_iter()
            .all(|i| b[i].is_ascii_digit())
    {
        return false;
    }
    let pair = |i: usize| u16::from(b[i] - b'0') * 10 + u16::from(b[i + 1] - b'0');
    let month = pair(0);
    let day = pair(3);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + u16::from(leap),
        _ => return false,
    };
    (1..=days).contains(&day) && pair(6) < 24 && pair(9) < 60 && pair(12) < 60
}

/// Proleptic Gregorian civil date from UTC epoch days (400-year eras). Only
/// generation needs a canonical date; no locale, timezone database or `time`
/// dependency is needed. Euclidean division handles pre-epoch seconds.
pub fn postal_utc_timestamp(seconds: i64) -> Result<String, PostalCodeFileError> {
    let days = seconds.div_euclid(86400);
    let tod = seconds.rem_euclid(86400);
    let z = days
        .checked_add(719468)
        .ok_or(PostalCodeFileError("invalid_timestamp"))?;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(0..=9999).contains(&year) {
        return Err(PostalCodeFileError("invalid_timestamp"));
    }
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        tod / 60 % 60,
        tod % 60
    ))
}

pub struct PostalCodePreparedFile {
    pub snapshot: PostalCodeSnapshot,
    pub tsv: Vec<u8>,
    pub manifest: PostalCodeManifest,
    pub manifest_bytes: Vec<u8>,
}
// An explicit update: the downloaded ZIP becomes a new TSV and manifest. The
// ZIP is bounded and checked by `postal_zip` before anything is inflated.
pub fn prepare_postal_code_zip(
    bytes: &[u8],
    seconds: i64,
    last_modified: Option<String>,
) -> Result<PostalCodePreparedFile, PostalCodeFileError> {
    let mut csv = crate::postal_code_zip::postal_zip(bytes)?;
    // Hash exactly the decompressed bytes, without retaining a second CSV copy.
    // The streaming read stays here; only the spelling is shared.
    struct HashReader<R>(R, Sha256);
    impl<R: Read> Read for HashReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.update(&buf[..n]);
            Ok(n)
        }
    }
    let mut reader = HashReader(&mut csv, Sha256::new());
    let snapshot = decode_postal_code_csv(&mut reader)?;
    let csv_sha256 = muu_digest::hex_lower(&reader.1.finalize());
    let tsv = encode_postal_code_tsv(&snapshot)?;
    let manifest = PostalCodeManifest {
        schema_version: 1,
        source_url: POSTAL_SOURCE_URL.into(),
        source_page_url: POSTAL_SOURCE_PAGE_URL.into(),
        source_last_modified: last_modified.unwrap_or_else(|| "unknown".into()),
        retrieved_at: postal_utc_timestamp(seconds)?,
        zip_sha256: hash(bytes),
        csv_sha256,
        snapshot_sha256: hash(&tsv),
        entry_count: snapshot.entries.len(),
    };
    validate_manifest(&manifest, &tsv, snapshot.entries.len())?;
    let manifest_bytes = encode_postal_code_manifest(&manifest)?;
    Ok(PostalCodePreparedFile {
        snapshot,
        tsv,
        manifest,
        manifest_bytes,
    })
}

/// Bundled seed's legacy pretty JSON differs from the compact persisted file.
pub fn encode_postal_code_seed_manifest(
    m: &PostalCodeManifest,
) -> Result<Vec<u8>, PostalCodeFileError> {
    let value = serde_json::to_value(m).map_err(|_| PostalCodeFileError("invalid_manifest"))?;
    let fields = value
        .as_object()
        .ok_or(PostalCodeFileError("invalid_manifest"))?;
    let mut out = b"{\n".to_vec();
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b",\n");
        }
        out.extend_from_slice(b"  ");
        serde_json::to_writer(&mut out, key)
            .map_err(|_| PostalCodeFileError("invalid_manifest"))?;
        // Foundation pretty JSON has whitespace BEFORE the colon as well.
        out.extend_from_slice(b" : ");
        serde_json::to_writer(&mut out, value)
            .map_err(|_| PostalCodeFileError("invalid_manifest"))?;
    }
    out.extend_from_slice(b"\n}\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(code: &str, town: &str) -> String {
        let mut f = vec![""; 15];
        f[2] = code;
        f[6] = "東京都";
        f[7] = "千代田区";
        f[8] = town;
        f.into_iter()
            .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(",")
    }
    fn manifest(tsv: &[u8], count: usize) -> PostalCodeManifest {
        PostalCodeManifest {
            schema_version: 1,
            source_url: POSTAL_SOURCE_URL.into(),
            source_page_url: POSTAL_SOURCE_PAGE_URL.into(),
            source_last_modified: "unknown".into(),
            retrieved_at: "2026-07-31T07:59:02Z".into(),
            zip_sha256: "0".repeat(64),
            csv_sha256: "1".repeat(64),
            snapshot_sha256: hash(tsv),
            entry_count: count,
        }
    }
    #[test]
    fn csv_preserves_quote_transitions_all_columns_dedup_and_special_towns() {
        let csv = format!(
            "{}\r\n{}\n{}\n{}\n{}\n",
            row("1000001", "以下に掲載がない場合"),
            row("1000001", "以下に掲載がない場合"),
            row("1000002", "一円"),
            row("1000003", "霞が関の次に番地がくる場合"),
            row("1000004", "丸の内")
        );
        let s = decode_postal_code_csv(csv.as_bytes()).unwrap();
        assert_eq!(s.entries.len(), 4);
        assert_eq!(s.entries[0].surface, "東京都千代田区");
        assert_eq!(s.entries[1].reading, "1000002");
        assert_eq!(s.entries[3].surface, "東京都千代田区丸の内");
        for town in [
            "以下に掲載か\u{3099}ない場合",
            "霞か\u{3099}関の次に番地か\u{3099}くる場合",
        ] {
            let s = decode_postal_code_csv(row("1000001", town).as_bytes()).unwrap();
            assert_eq!(s.entries[0].surface, "東京都千代田区");
        }
        let escaped = row("1000001", "丸,\"内");
        assert!(decode_postal_code_csv(escaped.as_bytes()).is_ok());
        for bad in [
            "\"x\"z",
            "\"unclosed",
            "a\rb",
            "\"x\"\ry",
            ",,,,,,,,,,,,,,,,",
        ] {
            assert!(decode_postal_code_csv(bad.as_bytes()).is_err(), "{bad:?}");
        }
        // Historical EOF quirk: a sole empty quoted field without LF is ignored.
        assert!(
            decode_postal_code_csv(&b"\"\""[..])
                .unwrap()
                .entries
                .is_empty()
        );
        let mut invalid = row("1000001", "町").into_bytes();
        invalid[1] = 0xff;
        assert!(decode_postal_code_csv(&invalid[..]).is_err());
    }
    #[test]
    fn source_row_limit_is_before_dedup_and_tsv_limits_are_whole_file() {
        let line = row("1000001", "町") + "\n";
        assert_eq!(
            decode_postal_code_csv(line.repeat(MAX_POSTAL_ENTRIES).as_bytes())
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(decode_postal_code_csv(line.repeat(MAX_POSTAL_ENTRIES + 1).as_bytes()).is_err());
        let mut bytes = HEADER.as_bytes().to_vec();
        bytes.extend_from_slice(b"1000001\t");
        bytes.extend_from_slice("a".repeat(4096).as_bytes());
        bytes.push(b'\n');
        assert_eq!(
            decode_postal_code_tsv(&bytes).unwrap().entries[0]
                .surface
                .len(),
            4096
        );
        bytes.insert(bytes.len() - 1, b'a');
        assert!(decode_postal_code_tsv(&bytes).is_err());
        assert!(decode_postal_code_tsv(&vec![b'a'; MAX_POSTAL_TSV_BYTES + 1]).is_err());
        assert!(decode_postal_code_csv(&vec![b'\n'; MAX_POSTAL_CSV_BYTES + 1][..]).is_err());
    }
    #[test]
    fn tsv_requires_nfc_foundation_controls_and_preserves_input_order() {
        let s = decode_postal_code_tsv(
            format!("{HEADER}1000002\t霞が関\n\n1000001\t丸の内").as_bytes(),
        )
        .unwrap();
        assert_eq!(s.entries[0].reading, "1000002");
        assert_eq!(
            encode_postal_code_tsv(&s).unwrap(),
            format!("{HEADER}1000002\t霞が関\n1000001\t丸の内\n").as_bytes()
        );
        for text in [
            format!("{HEADER}1000001\tか\u{3099}\n"),
            format!("{HEADER}1000001\t町\r\n"),
            format!("{HEADER}1000001\t町\n1000001\t町\n"),
        ] {
            assert!(decode_postal_code_tsv(text.as_bytes()).is_err());
        }
        for (ch, ok) in [
            ('\u{1f}', false),
            ('\u{20}', true),
            ('\u{7f}', false),
            ('\u{ad}', false),
            ('\u{200b}', false),
            ('\u{2028}', true),
            ('\u{2065}', true),
            ('\u{2066}', false),
        ] {
            assert_eq!(
                decode_postal_code_tsv(format!("{HEADER}1000001\t町{ch}").as_bytes()).is_ok(),
                ok,
                "{ch:?}"
            );
        }
    }
    #[test]
    fn manifest_preserves_json_bytes_but_limits_dates_to_writer_output() {
        let tsv = HEADER.as_bytes();
        let m = manifest(tsv, 0);
        let bytes = encode_postal_code_manifest(&m).unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains("https:\\/\\/")
        );
        assert_eq!(decode_postal_code_manifest(&bytes, tsv, 0).unwrap(), m);
        let s = String::from_utf8(bytes).unwrap();
        for n in ["1.0", "1e0"] {
            assert!(
                decode_postal_code_manifest(
                    s.replace("\"schema_version\":1", &format!("\"schema_version\":{n}"))
                        .as_bytes(),
                    tsv,
                    0
                )
                .is_ok()
            );
        }
        assert!(
            decode_postal_code_manifest(
                s.replace(
                    "\"schema_version\":1",
                    "\"schema_version\":1.00000000000000001"
                )
                .as_bytes(),
                tsv,
                0
            )
            .is_err()
        );
        let mut utf16 = vec![0xff, 0xfe];
        for u in s.encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_postal_code_manifest(&utf16, tsv, 0).unwrap(), m);
        // Old JSONEncoder never writes trailing commas. The shared parser
        // remains strict; no preprocessor tries to repair hand-edited JSON.
        assert!(
            decode_postal_code_manifest(format!("{},}}", &s[..s.len() - 1]).as_bytes(), tsv, 0)
                .is_err()
        );
        for date in [
            "1970-01-01T00:00:00Z",
            "1969-12-31T23:59:59Z",
            "2000-02-29T00:00:00Z",
            "2026-07-31T07:59:02Z", // bundled seed
            "2100-03-01T00:00:00Z",
            "9999-12-31T23:59:59Z",
            "10000-01-01T00:00:00Z", // default formatter's extended year
        ] {
            let mut dated = m.clone();
            dated.retrieved_at = date.into();
            assert!(canonical_utc_seconds(date), "{date}");
            assert!(
                decode_postal_code_manifest(&encode_postal_code_manifest(&dated).unwrap(), tsv, 0)
                    .is_ok(),
                "{date}"
            );
        }
        for date in [
            "2026-02-30T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "-001-07-31T07:59:02Z",
            "+2026-01-01T00:00:00Z",
            "٢٠٢٦-٠٧-٣١T٠٧:٥٩:٠٢Z",
            "2026-7-3T7:9:2Z",
            "2026/07/31T07:59:02Z",
            "2026.07.31T07:59:02Z",
            "2026-07-31T07:59:02GMT",
            "2026-07-31T07:59:02UTC",
            "2026-07-31T07:59:02Ztail",
            "2026-07-31T07:59:02Z\n",
            "2026-07-31T07:59:02+09:00",
            "2026-07-31T07:59:02+0000",
            "2026-07-31T07:59:02.0Z",
            "2026-07-31T07:59:02.123Z",
            "2026-07-31t07:59:02z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T25:00:00Z",
            "2026-01-01T23:59:60Z",
            "2026-07-31",
        ] {
            let mut dated = m.clone();
            dated.retrieved_at = date.into();
            let bytes = crate::stored_json::foundation_json(&dated).unwrap();
            assert!(!canonical_utc_seconds(date), "{date}");
            assert!(
                decode_postal_code_manifest(&bytes, tsv, 0).is_err(),
                "{date}"
            );
        }
    }
    #[test]
    fn utc_generation_handles_negative_seconds_leap_centuries_and_bounds() {
        for (sec, date) in [
            (0, "1970-01-01T00:00:00Z"),
            (-1, "1969-12-31T23:59:59Z"),
            (951782400, "2000-02-29T00:00:00Z"),
            (4107542400, "2100-03-01T00:00:00Z"),
            (-62167219200, "0000-01-01T00:00:00Z"),
            (253402300799, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(postal_utc_timestamp(sec).unwrap(), date);
        }
        assert!(postal_utc_timestamp(i64::MAX).is_err());
        assert!(postal_utc_timestamp(i64::MIN).is_err());
    }
    #[test]
    #[ignore = "requires public public_seed (check-assets)"]
    fn public_seed_bytes_count_order_and_manifest_are_unchanged() {
        let tsv = include_bytes!("../../../apple/MuuIME/PostalCodeSeed/postal-code-seed.tsv");
        let json =
            include_bytes!("../../../apple/MuuIME/PostalCodeSeed/postal-code-seed-manifest.json");
        let s = decode_postal_code_tsv(tsv).unwrap();
        assert_eq!(s.entries.len(), 124512);
        assert_eq!(
            hash(tsv),
            "6752f105b4d6654021910c25e24f5aa2c143b5f204835b4b69fab0e11428f469"
        );
        assert_eq!(encode_postal_code_tsv(&s).unwrap(), tsv);
        assert_eq!(
            decode_postal_code_manifest(json, tsv, s.entries.len())
                .unwrap()
                .entry_count,
            124512
        );
        let m = decode_postal_code_manifest(json, tsv, s.entries.len()).unwrap();
        assert_eq!(encode_postal_code_seed_manifest(&m).unwrap(), json);
    }
}
