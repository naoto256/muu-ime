//! Bounded GGUF v3 reader for the pinned GPT-2 artifact. Quantization equations
//! follow llama.cpp b4846 ggml-quants.c (MIT); only F32/Q5_K/Q6_K are accepted.
use crate::zenz_weights::{Embedding, HalfMatrix, TensorData, decode_storage};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

// 29c223d4…db673, the pinned ggml-model-Q5_K_M.gguf (NOTICE.txt).
const EXPECTED_SHA: [u8; 32] = [
    41, 194, 35, 212, 194, 51, 39, 184, 15, 209, 62, 187, 90, 178, 85, 80, 87, 164, 99, 23, 153,
    125, 93, 163, 145, 88, 79, 251, 239, 13, 182, 115,
];

pub struct Tensor {
    pub dims: Vec<usize>,
    pub values: TensorData,
}
pub struct Gguf {
    pub metadata: BTreeMap<String, Value>,
    pub tensors: BTreeMap<String, Tensor>,
    pub sha: [u8; 32],
}
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("GGUF offset overflow")?;
        let s = self.bytes.get(self.pos..end).ok_or("truncated GGUF")?;
        self.pos = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn size(&mut self) -> Result<usize, String> {
        usize::try_from(self.u64()?).map_err(|_| "GGUF size overflow".into())
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.size()?;
        if n > 16 * 1024 * 1024 {
            return Err("GGUF string limit".into());
        }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| "GGUF invalid UTF-8".into())
    }
    fn value(&mut self, t: u32, depth: usize) -> Result<Value, String> {
        if depth > 2 {
            return Err("GGUF nested array limit".into());
        }
        Ok(match t {
            0 => Value::from(self.take(1)?[0]),
            1 => Value::from(self.take(1)?[0] as i8),
            2 => Value::from(u16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            3 => Value::from(i16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            4 => Value::from(self.u32()?),
            5 => Value::from(self.u32()? as i32),
            6 => Value::from(f32::from_bits(self.u32()?)),
            7 => Value::from(self.take(1)?[0] != 0),
            8 => Value::from(self.string()?),
            9 => {
                let t = self.u32()?;
                let n = self.size()?;
                if n > 100_000 {
                    return Err("GGUF array limit".into());
                }
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(self.value(t, depth + 1)?)
                }
                Value::from(v)
            }
            10 => Value::from(self.u64()?),
            11 => Value::from(self.u64()? as i64),
            12 => Value::from(f64::from_bits(self.u64()?)),
            _ => return Err("unsupported GGUF metadata type".into()),
        })
    }
}
impl Gguf {
    pub fn load(path: &Path) -> Result<Self, String> {
        use std::io::Read;
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("GGUF is not a regular file".into());
        }
        let length = usize::try_from(metadata.len()).map_err(|_| "GGUF file limit")?;
        if length > 128 * 1024 * 1024 {
            return Err("GGUF file limit".into());
        }
        // Same-FD, bounded size hint avoids growing a 74 MB pinned artifact
        // geometrically. The read is still bounded if the file grows meanwhile.
        let mut bytes = Vec::with_capacity(length);
        file.take(128 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 128 * 1024 * 1024 {
            return Err("GGUF file limit".into());
        }
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        // Reject a different artifact before allocating any decoded tensors.
        // This backend has one fixed architecture, vocabulary and EOS profile.
        if sha != EXPECTED_SHA {
            return Err("unrecognized zenz artifact SHA-256".into());
        }
        Self::parse(&bytes, sha)
    }
    // `load` only passes the pinned bytes here, so the limits below
    // guard tests and future artifacts rather than the shipped file.
    fn parse(bytes: &[u8], sha: [u8; 32]) -> Result<Self, String> {
        let mut r = Reader { bytes, pos: 0 };
        if r.take(4)? != b"GGUF" || r.u32()? != 3 {
            return Err("expected GGUF v3".into());
        }
        let nt = r.size()?;
        let nm = r.size()?;
        if nt > 512 || nm > 128 {
            return Err("GGUF header limit".into());
        }
        let mut metadata = BTreeMap::new();
        for _ in 0..nm {
            let k = r.string()?;
            let t = r.u32()?;
            if metadata.insert(k, r.value(t, 0)?).is_some() {
                return Err("duplicate GGUF key".into());
            }
        }
        let mut descriptors = Vec::new();
        for _ in 0..nt {
            let name = r.string()?;
            let nd = r.u32()?;
            if nd == 0 || nd > 2 {
                return Err("unsupported tensor dimensions".into());
            }
            let mut dims = Vec::new();
            for _ in 0..nd {
                dims.push(r.size()?)
            }
            let t = r.u32()?;
            let offset = r.size()?;
            descriptors.push((name, dims, t, offset));
        }
        let alignment = metadata
            .get("general.alignment")
            .and_then(Value::as_u64)
            .unwrap_or(32) as usize;
        if alignment == 0 || !alignment.is_power_of_two() || alignment > 4096 {
            return Err("invalid GGUF alignment".into());
        }
        let start = r
            .pos
            .checked_add(alignment - 1)
            .ok_or("GGUF offset overflow")?
            & !(alignment - 1);
        let mut tensors = BTreeMap::new();
        let mut regions = Vec::new();
        let mut decoded_bytes = 0usize;
        for (name, dims, t, offset) in descriptors {
            let count = dims
                .iter()
                .try_fold(1usize, |a, b| a.checked_mul(*b))
                .ok_or("tensor size overflow")?;
            if count == 0 || count > 16_000_000 {
                return Err("tensor size limit".into());
            }
            // GGUF lists dims fastest-first, so [inputs, outputs] is stored
            // as `outputs` rows of `inputs` values. K-quants pack 256 values
            // per block: Q5_K = d, dmin (f16) + 12 scale bytes + 32 qh + 128 qs;
            // Q6_K = 128 ql + 64 qh + 16 int8 scales + d (f16).
            let n = match t {
                0 => count.checked_mul(4),
                13 if count % 256 == 0 => Some(count / 256 * 176),
                14 if count % 256 == 0 => Some(count / 256 * 210),
                _ => return Err("unsupported GGUF tensor type".into()),
            }
            .ok_or("tensor byte overflow")?;
            decoded_bytes = decoded_bytes
                .checked_add(count.checked_mul(4).ok_or("decoded size overflow")?)
                .ok_or("decoded size overflow")?;
            if decoded_bytes > 400 * 1024 * 1024 {
                return Err("decoded GGUF limit".into());
            }
            if offset % alignment != 0 {
                return Err("unaligned GGUF tensor".into());
            }
            let begin = start.checked_add(offset).ok_or("tensor offset overflow")?;
            let end = begin.checked_add(n).ok_or("tensor offset overflow")?;
            let data = bytes.get(begin..end).ok_or("truncated GGUF tensor")?;
            if regions.iter().any(|&(a, b)| begin < b && a < end) {
                return Err("overlapping GGUF tensors".into());
            }
            regions.push((begin, end));
            let values = decode_storage(&name, t, data, count)?;
            if tensors.insert(name, Tensor { dims, values }).is_some() {
                return Err("duplicate tensor".into());
            }
        }
        Ok(Self {
            metadata,
            tensors,
            sha,
        })
    }
    // Each accessor removes the tensor it returns, so `Zenz::load` can reject
    // a file with tensors it never asked for. The storage form was fixed by
    // name in `decode_storage`; asking with the wrong accessor is an error.
    pub fn tensor(&mut self, name: &str, dims: &[usize]) -> Result<Vec<f32>, String> {
        let t = self
            .tensors
            .remove(name)
            .ok_or_else(|| format!("missing tensor {name}"))?;
        if t.dims != dims {
            return Err(format!("wrong shape for {name}"));
        }
        match t.values {
            TensorData::F32(values) => Ok(values),
            _ => Err(format!("expected FP32 tensor {name}")),
        }
    }
    pub fn embedding(&mut self, name: &str, dims: &[usize]) -> Result<Embedding, String> {
        let t = self
            .tensors
            .remove(name)
            .ok_or_else(|| format!("missing tensor {name}"))?;
        if t.dims != dims {
            return Err(format!("wrong shape for {name}"));
        }
        match t.values {
            TensorData::Q5(bytes) => Ok(Embedding::new(bytes)),
            _ => Err(format!("expected packed embedding {name}")),
        }
    }
    pub fn half_matrix(&mut self, name: &str, dims: &[usize]) -> Result<HalfMatrix, String> {
        let t = self
            .tensors
            .remove(name)
            .ok_or_else(|| format!("missing tensor {name}"))?;
        if t.dims != dims {
            return Err(format!("wrong shape for {name}"));
        }
        match t.values {
            TensorData::F16(values) => Ok(HalfMatrix::new(values)),
            _ => Err(format!("expected FP16 matrix {name}")),
        }
    }
}
// IEEE binary16 by hand: exponent 0 is subnormal (frac * 2^-24), 31 is
// inf/NaN. Non-finite results are rejected after dequantization.
pub(crate) fn half(b: &[u8]) -> f32 {
    let h = u16::from_le_bytes(b[..2].try_into().unwrap());
    let sign = if h & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exp = (h >> 10) & 31;
    let frac = h & 1023;
    match exp {
        0 => sign * (frac as f32) * 2.0f32.powi(-24),
        31 => {
            if frac == 0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + frac as f32 / 1024.0) * 2.0f32.powi(exp as i32 - 15),
    }
}
pub fn dequantize(t: u32, bytes: &[u8], count: usize) -> Result<Vec<f32>, String> {
    if t == 0 {
        return Ok(bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect());
    }
    let mut out = vec![0.0; count];
    match t {
        13 => {
            for (b, y) in bytes.chunks_exact(176).zip(out.chunks_exact_mut(256)) {
                let d = half(b);
                let min = half(&b[2..]);
                let sc = &b[4..16];
                let qh = &b[16..48];
                let ql = &b[48..];
                // Eight 32-value sub-blocks, each with a 6-bit scale and
                // 6-bit min. Sub-blocks 0..3 use the low 6 bits of bytes 0..3
                // (scale) and 4..7 (min). Sub-blocks 4..7 take their low
                // nibbles from bytes 8..11 and the top 2 bits from bytes 0..7.
                let scale = |j: usize| -> (f32, f32) {
                    let (s, m) = if j < 4 {
                        (sc[j] & 63, sc[j + 4] & 63)
                    } else {
                        (
                            (sc[j + 4] & 15) | ((sc[j - 4] >> 6) << 4),
                            (sc[j + 4] >> 4) | ((sc[j] >> 6) << 4),
                        )
                    };
                    (d * s as f32, min * m as f32)
                };
                // Per 64 values, qs gives the low 4 bits (low nibble for the
                // first 32, high nibble for the next 32) and qh bit 2j / 2j+1
                // the fifth. The result is asymmetric: d*scale*q - dmin*min.
                for j in 0..4 {
                    let (d1, m1) = scale(j * 2);
                    let (d2, m2) = scale(j * 2 + 1);
                    for l in 0..32 {
                        y[j * 64 + l] = d1
                            * ((ql[j * 32 + l] & 15)
                                + if qh[l] & (1 << (j * 2)) != 0 { 16 } else { 0 })
                                as f32
                            - m1;
                        y[j * 64 + l + 32] = d2
                            * ((ql[j * 32 + l] >> 4)
                                + if qh[l] & (2 << (j * 2)) != 0 { 16 } else { 0 })
                                as f32
                            - m2;
                    }
                }
            }
        }
        14 => {
            for (b, y) in bytes.chunks_exact(210).zip(out.chunks_exact_mut(256)) {
                let d = half(&b[208..]);
                let ql = &b[..128];
                let qh = &b[128..192];
                let sc = &b[192..208];
                // Two 128-value halves. Each 6-bit q is a 4-bit ql nibble plus
                // 2 bits of qh, centred by -32 (symmetric, no min). One int8
                // scale covers 16 values, hence sc[n*8 + l/16 + 2k].
                for n in 0..2 {
                    for l in 0..32 {
                        let q = [
                            (ql[n * 64 + l] & 15) | ((qh[n * 32 + l] & 3) << 4),
                            (ql[n * 64 + l + 32] & 15) | (((qh[n * 32 + l] >> 2) & 3) << 4),
                            (ql[n * 64 + l] >> 4) | (((qh[n * 32 + l] >> 4) & 3) << 4),
                            (ql[n * 64 + l + 32] >> 4) | (((qh[n * 32 + l] >> 6) & 3) << 4),
                        ];
                        for k in 0..4 {
                            y[n * 128 + k * 32 + l] = d
                                * (sc[n * 8 + l / 16 + k * 2] as i8) as f32
                                * (q[k] as i32 - 32) as f32;
                        }
                    }
                }
            }
        }
        _ => return Err("unsupported quantization".into()),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every negative parse goes through the allocation budget. Only the
    // `parse` call is measured, not building the fixture.
    fn parse_invalid(bytes: &[u8]) -> Result<Gguf, String> {
        crate::allocation_probe::for_small_invalid(bytes, || Gguf::parse(bytes, [0; 32]))
    }

    // One-tensor GGUF v3 with no metadata. Layout: magic 0..4, version 4..8,
    // tensor count 8..16, metadata count 16..24, name length 24..32, name
    // from 32. The payload always sits at the first aligned data offset,
    // whatever `offset` declares, so a negative can lie about the offset
    // without the fixture growing to match.
    fn minimal_gguf(kind: u32, dimensions: &[u64], offset: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(1u64.to_le_bytes()); // One tensor, no metadata.
        bytes.extend(0u64.to_le_bytes());
        let name = b"fixture.weight";
        bytes.extend((name.len() as u64).to_le_bytes());
        bytes.extend(name);
        bytes.extend((dimensions.len() as u32).to_le_bytes());
        for dimension in dimensions {
            bytes.extend(dimension.to_le_bytes());
        }
        bytes.extend(kind.to_le_bytes());
        bytes.extend(offset.to_le_bytes());
        bytes.resize(bytes.len().next_multiple_of(32), 0);
        // A malformed declared offset must never drive fixture allocation.
        bytes.extend(payload);
        bytes
    }

    // Each accepted tensor type first parses and decodes from the full bytes,
    // so the negatives below start from a fixture known to be valid. Then
    // every proper prefix must fail: the reader is sequential, so each cut
    // lands in a different field (header, name, dims, type, offset, padding,
    // payload) and catches any read that trusts bytes it does not have. The
    // payload ends the file, so no prefix still holds a whole tensor. `parse`
    // is called directly, past the pinned-SHA check in `load`.
    #[test]
    fn supported_tensor_fixtures_reject_every_truncation() {
        let fp32 = [1.25f32.to_le_bytes(), (-2.0f32).to_le_bytes()].concat();
        for (kind, count, payload) in [
            (0, 2, fp32),
            (13, 256, vec![0; 176]),
            (14, 256, vec![0; 210]),
        ] {
            let bytes = minimal_gguf(kind, &[count], 0, &payload);
            let mut parsed = Gguf::parse(&bytes, [0; 32]).unwrap();
            let values = parsed.tensor("fixture.weight", &[count as usize]).unwrap();
            assert_eq!(values.len(), count as usize);
            if kind == 0 {
                assert_eq!(values, [1.25, -2.0]);
            } else {
                assert_eq!(values, vec![0.0; count as usize]);
            }
            for cut in 0..bytes.len() {
                assert!(
                    parse_invalid(&bytes[..cut]).is_err(),
                    "kind={kind}, cut={cut}"
                );
            }
        }
    }

    // Fourteen malformed descriptors, each matched to its exact error so a
    // negative cannot pass by failing an earlier, unrelated check. A K-quant
    // count that is not a multiple of 256 (255 here) reports as an
    // unsupported type, because that is the arm it falls to. The offset
    // cases cover a misaligned offset, `start + offset` overflow, and an
    // aligned offset past the end of the bytes.
    #[test]
    fn malformed_tensor_lengths_types_and_offsets_are_rejected_before_decoding() {
        for (bytes, reason) in [
            (
                minimal_gguf(0, &[], 0, &[]),
                "unsupported tensor dimensions",
            ),
            (
                minimal_gguf(0, &[1, 1, 1], 0, &[0; 4]),
                "unsupported tensor dimensions",
            ),
            (minimal_gguf(0, &[0], 0, &[]), "tensor size limit"),
            (minimal_gguf(0, &[16_000_001], 0, &[]), "tensor size limit"),
            (
                minimal_gguf(0, &[u64::MAX, 2], 0, &[]),
                "tensor size overflow",
            ),
            (
                minimal_gguf(99, &[1], 0, &[0; 4]),
                "unsupported GGUF tensor type",
            ),
            (
                minimal_gguf(13, &[255], 0, &[0; 176]),
                "unsupported GGUF tensor type",
            ),
            (
                minimal_gguf(14, &[255], 0, &[0; 210]),
                "unsupported GGUF tensor type",
            ),
            (minimal_gguf(0, &[1], 1, &[0; 4]), "unaligned GGUF tensor"),
            (
                minimal_gguf(0, &[1], u64::MAX - 31, &[0; 4]),
                "tensor offset overflow",
            ),
            (minimal_gguf(0, &[1], 32, &[0; 4]), "truncated GGUF tensor"),
        ] {
            assert!(
                matches!(parse_invalid(&bytes), Err(actual) if actual == reason),
                "{reason}"
            );
        }
        // Header fields patched in place on a valid fixture: a metadata count
        // above 128, then the tensor name's length and first byte. Names and
        // metadata keys go through the same string reader.
        let original = minimal_gguf(0, &[1], 0, &[0; 4]);
        for (range, replacement, reason) in [
            (16..24, 129u64.to_le_bytes().to_vec(), "GGUF header limit"),
            (24..32, u64::MAX.to_le_bytes().to_vec(), "GGUF string limit"),
            (32..33, vec![0xff], "GGUF invalid UTF-8"),
        ] {
            let mut bytes = original.clone();
            bytes[range].copy_from_slice(&replacement);
            assert!(
                matches!(parse_invalid(&bytes), Err(actual) if actual == reason),
                "{reason}"
            );
        }
    }

    #[test]
    fn quantized_constant_blocks_preserve_scale_minimum_and_signed_offset() {
        let mut q5 = vec![0u8; 176];
        q5[..2].copy_from_slice(&0x3c00u16.to_le_bytes()); // d=1
        q5[2..4].copy_from_slice(&0x4000u16.to_le_bytes()); // min scale=2
        q5[4..16].copy_from_slice(&[1, 1, 1, 1, 3, 3, 3, 3, 0x31, 0x31, 0x31, 0x31]);
        q5[48..].fill(0x55); // each unsigned quant=5: 1*5 - 2*3 = -1
        assert_eq!(dequantize(13, &q5, 256).unwrap(), vec![-1.0; 256]);
        let mut q6 = vec![0u8; 210];
        q6[..128].fill(0xdd);
        q6[128..192].fill(0x55); // unsigned quant 29, signed quant -3
        q6[192..208].fill(2);
        q6[208..].copy_from_slice(&0x3800u16.to_le_bytes()); // d=.5
        assert_eq!(dequantize(14, &q6, 256).unwrap(), vec![-3.0; 256]);
    }
    #[test]
    fn malformed_container_counts_and_truncation_are_rejected() {
        assert!(parse_invalid(b"GGUF").is_err());
        let mut data = b"GGUF".to_vec();
        data.extend(3u32.to_le_bytes());
        data.extend(513u64.to_le_bytes());
        data.extend(0u64.to_le_bytes());
        assert!(parse_invalid(&data).is_err());
        let mut reader = Reader {
            bytes: &[0; 8],
            pos: usize::MAX,
        };
        assert!(crate::allocation_probe::for_small_invalid(&[0; 8], || reader.take(1)).is_err());
    }
}
