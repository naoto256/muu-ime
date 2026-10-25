//! Bounded ZIP container parser and streaming raw inflate. There is no path
//! extraction and only the root CSV is inflated. Header/descriptor spans are
//! checked together, before allocating any decompressor state. Deflate and CRC
//! remain the responsibility of the pure Rust miniz_oxide/crc32fast libraries.
use crate::postal_code_file::{MAX_POSTAL_CSV_BYTES, MAX_POSTAL_ZIP_BYTES, PostalCodeFileError};
use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};
use std::io::{self, Read};
const MAX_MEMBERS: usize = 64;
const MAX_DIRECTORY: usize = 256 * 1024;
const MAX_NAME: usize = 1024;
const TARGET: &[u8] = b"utf_ken_all.csv";
fn invalid() -> PostalCodeFileError {
    PostalCodeFileError("invalid_zip")
}
fn add(a: usize, b: usize) -> Result<usize, PostalCodeFileError> {
    a.checked_add(b).ok_or_else(invalid)
}
fn part(b: &[u8], base: usize, offset: usize, n: usize) -> Result<&[u8], PostalCodeFileError> {
    let start = add(base, offset)?;
    b.get(start..add(start, n)?).ok_or_else(invalid)
}
fn u16le(b: &[u8], base: usize, offset: usize) -> Result<u16, PostalCodeFileError> {
    Ok(u16::from_le_bytes(
        part(b, base, offset, 2)?
            .try_into()
            .map_err(|_| invalid())?,
    ))
}
fn u32le(b: &[u8], base: usize, offset: usize) -> Result<u32, PostalCodeFileError> {
    Ok(u32::from_le_bytes(
        part(b, base, offset, 4)?
            .try_into()
            .map_err(|_| invalid())?,
    ))
}
fn no_zip64(mut extra: &[u8]) -> Result<(), PostalCodeFileError> {
    while !extra.is_empty() {
        let tag = u16le(extra, 0, 0)?;
        let n = usize::from(u16le(extra, 0, 2)?);
        if tag == 1 {
            return Err(invalid());
        }
        part(extra, 0, 4, n)?;
        extra = &extra[add(4, n)?..];
    }
    Ok(())
}
struct Member<'a> {
    data: &'a [u8],
    method: u16,
    crc: u32,
    expanded: usize,
}

fn target(input: &[u8]) -> Result<Member<'_>, PostalCodeFileError> {
    if input.len() > MAX_POSTAL_ZIP_BYTES {
        return Err(PostalCodeFileError("file_too_large"));
    }
    let end = (0..=input.len().saturating_sub(22))
        .rev()
        .take(65536)
        .find(|&i| {
            part(input, i, 0, 4).is_ok_and(|p| p == b"PK\x05\x06")
                && u16le(input, i, 20).is_ok_and(|n| {
                    add(i, 22)
                        .and_then(|p| add(p, usize::from(n)))
                        .is_ok_and(|p| p == input.len())
                })
        })
        .ok_or_else(invalid)?;
    let count = usize::from(u16le(input, end, 10)?);
    let directory_size = u32le(input, end, 12)? as usize;
    let directory = u32le(input, end, 16)? as usize;
    if u16le(input, end, 4)? != 0
        || u16le(input, end, 6)? != 0
        || usize::from(u16le(input, end, 8)?) != count
        || count == 0
        || count > MAX_MEMBERS
        || directory_size > MAX_DIRECTORY
        || add(directory, directory_size)? != end
    {
        return Err(invalid());
    }
    let mut pos = directory;
    let mut ranges = Vec::with_capacity(count);
    let mut target = None;
    for _ in 0..count {
        if part(input, pos, 0, 4)? != b"PK\x01\x02" {
            return Err(invalid());
        }
        part(input, pos, 0, 46)?;
        let flags = u16le(input, pos, 8)?;
        let method = u16le(input, pos, 10)?;
        let crc = u32le(input, pos, 16)?;
        let compressed = u32le(input, pos, 20)? as usize;
        let expanded = u32le(input, pos, 24)? as usize;
        let name_len = usize::from(u16le(input, pos, 28)?);
        let extra_len = usize::from(u16le(input, pos, 30)?);
        let comment_len = usize::from(u16le(input, pos, 32)?);
        let offset = u32le(input, pos, 42)? as usize;
        if flags & (1 | 64 | 8192) != 0
            || !matches!(method, 0 | 8)
            || name_len == 0
            || name_len > MAX_NAME
            || u16le(input, pos, 34)? != 0
            || compressed == u32::MAX as usize
            || expanded == u32::MAX as usize
            || offset >= directory
        {
            return Err(invalid());
        }
        let name = part(input, pos, 46, name_len)?;
        no_zip64(part(input, pos, add(46, name_len)?, extra_len)?)?;
        let mode = u32le(input, pos, 38)? >> 16;
        pos = add(pos, add(add(add(46, name_len)?, extra_len)?, comment_len)?)?;
        if pos > end {
            return Err(invalid());
        }
        if part(input, offset, 0, 4)? != b"PK\x03\x04"
            || u16le(input, offset, 6)? != flags
            || u16le(input, offset, 8)? != method
        {
            return Err(invalid());
        }
        let local_name = usize::from(u16le(input, offset, 26)?);
        let local_extra = usize::from(u16le(input, offset, 28)?);
        if local_name != name_len || part(input, offset, 30, local_name)? != name {
            return Err(invalid());
        }
        no_zip64(part(input, offset, add(30, local_name)?, local_extra)?)?;
        for (at, expected) in [(14, crc), (18, compressed as u32), (22, expanded as u32)] {
            let actual = u32le(input, offset, at)?;
            if actual != expected && !(flags & 8 != 0 && actual == 0) {
                return Err(invalid());
            }
        }
        let start = add(offset, add(add(30, local_name)?, local_extra)?)?;
        let mut stop = add(start, compressed)?;
        if flags & 8 != 0 {
            let matches = |base| -> bool {
                u32le(input, base, 0) == Ok(crc)
                    && u32le(input, base, 4) == Ok(compressed as u32)
                    && u32le(input, base, 8) == Ok(expanded as u32)
            };
            // A signature-less descriptor's CRC may itself equal the optional
            // signature. Compare the whole tuple, not just those four bytes.
            if matches(stop) {
                stop = add(stop, 12)?;
            } else if part(input, stop, 0, 4)? == b"PK\x07\x08" && matches(add(stop, 4)?) {
                stop = add(stop, 16)?;
            } else {
                return Err(invalid());
            }
        }
        if stop > directory {
            return Err(invalid());
        }
        ranges.push((offset, stop));
        if name == TARGET {
            if target.is_some()
                || mode & 0o170000 == 0o120000
                || mode & 0o170000 == 0o040000
                || expanded == 0
                || expanded > MAX_POSTAL_CSV_BYTES
            {
                return Err(invalid());
            }
            target = Some(Member {
                data: part(input, start, 0, compressed)?,
                method,
                crc,
                expanded,
            });
        }
    }
    if pos != end {
        return Err(invalid());
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(invalid());
    }
    target.ok_or_else(invalid)
}

/// The output window is clamped to the remaining absolute cap plus ONE byte.
/// Sizes in the ZIP header never reserve memory. EOF checks both actual length
/// and CRC, and a raw stream must consume precisely the declared compressed span.
pub(crate) struct PostalZipReader<'a> {
    member: Member<'a>,
    state: Option<Box<InflateState>>,
    consumed: usize,
    produced: usize,
    crc: crc32fast::Hasher,
    done: bool,
}
pub(crate) fn postal_zip(input: &[u8]) -> Result<PostalZipReader<'_>, PostalCodeFileError> {
    let member = target(input)?;
    let state = (member.method == 8).then(|| InflateState::new_boxed(DataFormat::Raw));
    Ok(PostalZipReader {
        member,
        state,
        consumed: 0,
        produced: 0,
        crc: crc32fast::Hasher::new(),
        done: false,
    })
}
impl Read for PostalZipReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "invalid_zip");
        if output.is_empty() {
            return Ok(0);
        }
        if self.done {
            return Ok(0);
        }
        let remaining = MAX_POSTAL_CSV_BYTES
            .checked_sub(self.produced)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(bad)?;
        let size = output.len().min(remaining);
        let output = &mut output[..size];
        loop {
            let n = if let Some(state) = &mut self.state {
                let result = inflate(
                    state,
                    &self.member.data[self.consumed..],
                    output,
                    MZFlush::None,
                );
                self.consumed = self
                    .consumed
                    .checked_add(result.bytes_consumed)
                    .ok_or_else(bad)?;
                match result.status {
                    Ok(MZStatus::StreamEnd) => self.done = true,
                    Ok(MZStatus::Ok) | Err(MZError::Buf)
                        if result.bytes_written > 0 || result.bytes_consumed > 0 => {}
                    _ => return Err(bad()),
                }
                result.bytes_written
            } else {
                let n = output.len().min(self.member.data.len() - self.consumed);
                output[..n].copy_from_slice(
                    &self.member.data
                        [self.consumed..self.consumed.checked_add(n).ok_or_else(bad)?],
                );
                self.consumed = self.consumed.checked_add(n).ok_or_else(bad)?;
                self.done = self.consumed == self.member.data.len();
                n
            };
            self.produced = self.produced.checked_add(n).ok_or_else(bad)?;
            if self.produced > MAX_POSTAL_CSV_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "file_too_large"));
            }
            self.crc.update(&output[..n]);
            if self.done
                && (self.consumed != self.member.data.len()
                    || self.produced != self.member.expanded
                    || self.crc.clone().finalize() != self.member.crc)
            {
                return Err(bad());
            }
            if n > 0 || self.done {
                return Ok(n);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Opening a malformed archive must stay within the allocation budget. These
    // inputs fail in `target` before any inflate state exists, so the inflate
    // path is not measured here.
    fn invalid_zip(input: &[u8]) -> Result<PostalZipReader<'_>, PostalCodeFileError> {
        crate::allocation_probe::for_small_invalid(input, || postal_zip(input))
    }
    // Self-written container fixture. The compressor is the library under test,
    // while structural mutations below exercise our bounded ZIP envelope.
    pub(crate) fn fixture(
        method: u16,
        names: &[&str],
        contents: &[u8],
        descriptor: bool,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for name in names {
            let data = if method == 8 {
                miniz_oxide::deflate::compress_to_vec(contents, 6)
            } else {
                contents.to_vec()
            };
            let offset = out.len() as u32;
            let crc = crc32fast::hash(contents);
            let flags = if descriptor { 8u16 } else { 0 };
            out.extend_from_slice(b"PK\x03\x04");
            for n in [20u16, flags, method, 0, 0] {
                out.extend_from_slice(&n.to_le_bytes());
            }
            for n in if descriptor {
                [0, 0, 0]
            } else {
                [crc, data.len() as u32, contents.len() as u32]
            } {
                out.extend_from_slice(&n.to_le_bytes());
            }
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&data);
            if descriptor {
                out.extend_from_slice(b"PK\x07\x08");
                for n in [crc, data.len() as u32, contents.len() as u32] {
                    out.extend_from_slice(&n.to_le_bytes());
                }
            }
            central.extend_from_slice(b"PK\x01\x02");
            for n in [20u16, 20, flags, method, 0, 0] {
                central.extend_from_slice(&n.to_le_bytes());
            }
            for n in [crc, data.len() as u32, contents.len() as u32] {
                central.extend_from_slice(&n.to_le_bytes());
            }
            for n in [name.len() as u16, 0, 0, 0, 0] {
                central.extend_from_slice(&n.to_le_bytes());
            }
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06");
        for n in [0u16, 0, names.len() as u16, names.len() as u16] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
    #[test]
    fn stored_deflate_descriptor_and_crc_validate_actual_stream() {
        for method in [0, 8] {
            for descriptor in [false, true] {
                let b = fixture(
                    method,
                    &["utf_ken_all.csv", "harmless"],
                    b"test",
                    descriptor,
                );
                let mut s = String::new();
                postal_zip(&b).unwrap().read_to_string(&mut s).unwrap();
                assert_eq!(s, "test");
            }
        }
        let mut b = fixture(0, &["utf_ken_all.csv"], b"test", false);
        b[30 + TARGET.len()] = b'!';
        assert!(
            postal_zip(&b)
                .unwrap()
                .read_to_end(&mut Vec::new())
                .is_err()
        );
        let mut b = fixture(8, &["utf_ken_all.csv"], b"test", false);
        b[30 + TARGET.len()] = 255;
        assert!(
            postal_zip(&b)
                .unwrap()
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }
    #[test]
    fn caps_and_target_names_are_checked_before_inflation() {
        let names: Vec<_> = (0..64)
            .map(|i| {
                if i == 0 {
                    "utf_ken_all.csv".into()
                } else {
                    format!("extra{i}")
                }
            })
            .collect();
        let mut refs: Vec<_> = names.iter().map(String::as_str).collect();
        assert!(postal_zip(&fixture(0, &refs, b"x", false)).is_ok());
        refs.push("last");
        assert!(postal_zip(&fixture(0, &refs, b"x", false)).is_err());
        for names in [
            vec!["missing"],
            vec!["x/utf_ken_all.csv"],
            vec!["utf_ken_all.csv", "utf_ken_all.csv"],
        ] {
            assert!(postal_zip(&fixture(0, &names, b"x", false)).is_err());
        }
        assert!(postal_zip(&fixture(0, &[&"a".repeat(1025)], b"x", false)).is_err());
        let b = fixture(
            8,
            &["utf_ken_all.csv"],
            &vec![b'\n'; MAX_POSTAL_CSV_BYTES + 1],
            false,
        );
        assert!(b.len() < MAX_POSTAL_ZIP_BYTES);
        assert!(postal_zip(&b).is_err());
    }
    #[test]
    fn actual_output_cap_stops_a_lying_high_ratio_stream_at_one_extra_byte() {
        let mut b = fixture(
            8,
            &["utf_ken_all.csv"],
            &vec![b'\n'; MAX_POSTAL_CSV_BYTES + 1],
            false,
        );
        let cd = u32le(&b, b.len() - 22, 16).unwrap() as usize;
        b[22..26].copy_from_slice(&1u32.to_le_bytes());
        b[cd + 24..cd + 28].copy_from_slice(&1u32.to_le_bytes());
        let mut r = postal_zip(&b).unwrap();
        let mut buf = [0; 65536];
        while r.read(&mut buf).is_ok_and(|n| n > 0) {}
        assert_eq!(r.produced, MAX_POSTAL_CSV_BYTES + 1);
    }
    #[test]
    fn malformed_offsets_overlaps_local_headers_and_descriptors_are_rejected() {
        let original = fixture(0, &["utf_ken_all.csv"], b"test", false);
        for (offset, value) in [(6, 1), (8, 99), (26, 0)] {
            let mut b = original.clone();
            b[offset] = value;
            assert!(invalid_zip(&b).is_err());
        }
        let mut b = original.clone();
        let e = b.len() - 22;
        b[e + 4] = 1;
        assert!(invalid_zip(&b).is_err());
        let mut b = original.clone();
        let cd = u32le(&b, b.len() - 22, 16).unwrap() as usize;
        b[cd + 42..cd + 46].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(invalid_zip(&b).is_err());
        let mut b = fixture(0, &["utf_ken_all.csv"], b"test", true);
        let d = 30 + TARGET.len() + 4 + 4;
        b[d] = 0;
        assert!(invalid_zip(&b).is_err());
        let mut b = fixture(0, &["utf_ken_all.csv", "extra"], b"x", false);
        let cd = u32le(&b, b.len() - 22, 16).unwrap() as usize;
        let second = cd + 46 + TARGET.len();
        b[second + 42..second + 46].copy_from_slice(&0u32.to_le_bytes());
        assert!(invalid_zip(&b).is_err());
        // ZIP64 extra tag, even when ordinary small sizes are also present.
        assert!(no_zip64(&[1, 0, 0, 0]).is_err());
        assert!(no_zip64(&[2, 0, 255, 255]).is_err());
    }

    // Every accepted form (Stored / Deflate, with and without a data
    // descriptor) first decodes to the exact payload, then every proper
    // prefix must fail to open. `target` requires the EOCD to end exactly at
    // the input's end, so any cut drops it; this pins that no prefix is
    // accepted. The bounds of the fields inside are covered by the
    // malformed-offset test above.
    //
    // Takes over the three cuts formerly in
    // malformed_offsets_overlaps_local_headers_and_descriptors_are_rejected,
    // on Stored without a descriptor: length 0, 20 (inside the local header)
    // and len - 1 (the last byte of the EOCD). That form is method 0 /
    // descriptor false here. The payload differs ("test" before), but those
    // cuts sit at the same header or EOCD positions.
    #[test]
    fn all_supported_zip_forms_reject_every_truncation() {
        let payload = b"synthetic boundary payload";
        for method in [0, 8] {
            for descriptor in [false, true] {
                let bytes = fixture(method, &["utf_ken_all.csv"], payload, descriptor);
                let mut decoded = Vec::new();
                postal_zip(&bytes)
                    .unwrap()
                    .read_to_end(&mut decoded)
                    .unwrap();
                assert_eq!(decoded, payload);
                for cut in 0..bytes.len() {
                    assert!(
                        invalid_zip(&bytes[..cut]).is_err(),
                        "method={method}, descriptor={descriptor}, cut={cut}"
                    );
                }
            }
        }
    }
}
