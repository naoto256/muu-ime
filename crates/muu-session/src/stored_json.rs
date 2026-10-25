//! Foundation-compatible stored JSON primitives shared by Imported and Postal.
//! These are file protocols, not the strict command protocol at the C ABI.
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserializer, Serialize};
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
#[derive(Debug)]
pub(crate) struct StoredJsonError;
fn invalid() -> StoredJsonError {
    StoredJsonError
}
/// JSONDecoder also recognizes UTF-16/32 (with or without a BOM). Only stored
/// JSON uses this compatibility step; exchange .muudict remains UTF-8 only.
//
// Detection follows JSONDecoder: a BOM first, then the NUL pattern of the
// first four bytes (JSON text starts with an ASCII character), then UTF-8 with
// an optional BOM. The result is always UTF-8.
pub(crate) fn catalog_encoding(bytes: &[u8]) -> Result<Cow<'_, [u8]>, StoredJsonError> {
    let (width, little, skip) = if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
        // The legacy Foundation JSONDecoder rejects UTF-32LE WITH a BOM,
        // although it accepts LE without BOM and BE with/without BOM. Do not
        // widen stored-file acceptance when sharing this codec.
        return Err(invalid());
    } else if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        (4, false, 4)
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        (2, true, 2)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (2, false, 2)
    } else if bytes.len() >= 4 && bytes[1..4] == [0, 0, 0] {
        (4, true, 0)
    } else if bytes.len() >= 4 && bytes[..3] == [0, 0, 0] {
        (4, false, 0)
    } else if bytes.len() >= 4 && bytes[1] == 0 && bytes[3] == 0 {
        (2, true, 0)
    } else if bytes.len() >= 4 && bytes[0] == 0 && bytes[2] == 0 {
        (2, false, 0)
    } else {
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        std::str::from_utf8(bytes).map_err(|_| invalid())?;
        return Ok(Cow::Borrowed(bytes));
    };
    let bytes = &bytes[skip..];
    if bytes.len() % width != 0 {
        return Err(invalid());
    }
    let mut text = String::new();
    if width == 2 {
        let units = bytes.chunks_exact(2).map(|pair| {
            if little {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        });
        for ch in char::decode_utf16(units) {
            text.push(ch.map_err(|_| invalid())?);
        }
    } else {
        for part in bytes.chunks_exact(4) {
            let a = [part[0], part[1], part[2], part[3]];
            let code = if little {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            };
            text.push(char::from_u32(code).ok_or_else(invalid)?);
        }
    }
    Ok(Cow::Owned(text.into_bytes()))
}

struct FirstFields<'a>(&'a [&'a str]);

impl<'de> Visitor<'de> for FirstFields<'_> {
    type Value = BTreeMap<String, &'de RawValue>;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a stored JSON object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut fields = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if self.0.contains(&key.as_str()) && !fields.contains_key(&key) {
                fields.insert(key, map.next_value::<&RawValue>()?);
            } else {
                // JSONDecoder keeps the FIRST known key. Unknown fields are
                // accepted but never copied into management or a new save.
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(fields)
    }
}

pub(crate) fn fields<'a>(
    bytes: &'a [u8],
    names: &[&str],
) -> Result<BTreeMap<String, &'a RawValue>, StoredJsonError> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let result = decoder
        .deserialize_map(FirstFields(names))
        .map_err(|_| invalid())?;
    decoder.end().map_err(|_| invalid())?;
    Ok(result)
}

pub(crate) fn required<'a>(
    map: &BTreeMap<String, &'a RawValue>,
    key: &str,
) -> Result<&'a RawValue, StoredJsonError> {
    map.get(key).copied().ok_or_else(invalid)
}

pub(crate) fn string(raw: &RawValue) -> Result<String, StoredJsonError> {
    serde_json::from_str(raw.get()).map_err(|_| invalid())
}

pub(crate) fn boolean(raw: &RawValue) -> Result<bool, StoredJsonError> {
    serde_json::from_str(raw.get()).map_err(|_| invalid())
}

/// Integral JSON decimals/exponents are accepted by Foundation. Do not round
/// through f64: 9007199254740993.0 must remain an exact Int64, including hashes
/// of the resulting generation. JSON token syntax was checked by RawValue.
pub(crate) fn integer(raw: &RawValue) -> Result<i64, StoredJsonError> {
    let text = raw.get();
    let (negative, text) = text.strip_prefix('-').map_or((false, text), |s| (true, s));
    if !text.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(invalid());
    }
    let (mantissa, exponent) = text
        .find(['e', 'E'])
        .map_or((text, "0"), |i| (&text[..i], &text[i + 1..]));
    let digits = mantissa.replace('.', "");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    let exponent: i64 = exponent.parse().map_err(|_| invalid())?;
    let fraction = mantissa.split_once('.').map_or(0, |(_, s)| s.len());
    let scale = exponent
        .checked_sub(i64::try_from(fraction).map_err(|_| invalid())?)
        .ok_or_else(invalid)?;
    let (digits, zeros) = if scale < 0 {
        let trim = usize::try_from(scale.unsigned_abs()).map_err(|_| invalid())?;
        if trim > digits.len() || !digits[digits.len() - trim..].bytes().all(|b| b == b'0') {
            return Err(invalid());
        }
        (&digits[..digits.len() - trim], 0)
    } else {
        (digits, usize::try_from(scale).map_err(|_| invalid())?)
    };
    if digits.len().saturating_add(zeros) > 19 {
        return Err(invalid());
    }
    let mut value = 0u64;
    for digit in digits.bytes().chain(std::iter::repeat_n(b'0', zeros)) {
        value = value
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(digit - b'0')))
            .ok_or_else(invalid)?;
    }
    if negative {
        if value == 1u64 << 63 {
            return Ok(i64::MIN);
        }
        i64::try_from(value).map(|n| -n).map_err(|_| invalid())
    } else {
        i64::try_from(value).map_err(|_| invalid())
    }
}

struct FoundationFormatter;
impl serde_json::ser::Formatter for FoundationFormatter {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        for (index, part) in fragment.split('/').enumerate() {
            if index != 0 {
                writer.write_all(b"\\/")?;
            }
            writer.write_all(part.as_bytes())?;
        }
        Ok(())
    }
}

/// sortedKeys JSONEncoder escapes solidus. A different valid JSON spelling
/// would rename every generation and violate file-byte compatibility.
pub(crate) fn foundation_json(value: &impl Serialize) -> Result<Vec<u8>, StoredJsonError> {
    let value = serde_json::to_value(value).map_err(|_| invalid())?;
    let mut serializer = serde_json::Serializer::with_formatter(Vec::new(), FoundationFormatter);
    value.serialize(&mut serializer).map_err(|_| invalid())?;
    Ok(serializer.into_inner())
}
