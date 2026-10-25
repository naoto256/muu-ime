//! Storage for the pinned zenz weights; computation remains FP32.
//!
//! FP16 conversion happens in exactly two places:
//! - Load (`decode_storage`): each FFN up/down tensor is dequantized in
//!   chunks of 262,144 values (1 MiB of FP32), converted to FP16 and the
//!   chunk is dropped before the next one. No full FP32 copy ever exists.
//! - Scoring (`HalfMatrix::as_f32`): before each FFN matmul, the whole
//!   matrix (768x3072 = 2,359,296 values) is widened into a scratch buffer
//!   owned by one `score_*` call. Up and then down reuse the same buffer in
//!   every layer and forward pass; it is freed when the call returns.
use crate::gguf::dequantize;

pub(crate) enum TensorData {
    F32(Vec<f32>),
    F16(Vec<u16>),
    Q5(Vec<u8>),
}

pub(crate) struct Embedding(Vec<u8>);
impl Embedding {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
    // `token` must be below the 6,000 rows; tokenizer output always is, and
    // the slice below panics otherwise.
    pub(crate) fn append(&self, token: usize, position: &[f32], out: &mut Vec<f32>) {
        // One 768-value row is exactly three original Q5_K blocks. Use the
        // same decoder as eager loading, without retaining all 6,000 rows.
        assert_eq!(position.len(), 768);
        let begin = token * 3 * 176;
        let row = dequantize(13, &self.0[begin..begin + 3 * 176], 768)
            .expect("validated packed embedding row");
        out.extend(
            row.iter()
                .zip(position)
                .map(|(value, position)| value + position),
        );
    }
}

pub(crate) fn decode_storage(
    name: &str,
    t: u32,
    bytes: &[u8],
    count: usize,
) -> Result<TensorData, String> {
    let packed = name == "token_embd.weight";
    let half = is_half_matrix(name);
    if !packed && !half {
        let values = dequantize(t, bytes, count)?;
        finite(&values)?;
        return Ok(TensorData::F32(values));
    }
    if packed && t != 13 {
        return Err("input embedding must be original Q5_K".into());
    }
    let (block_bytes, block_values) = match t {
        0 => (4, 1),
        13 => (176, 256),
        14 => (210, 256),
        _ => return Err("unsupported quantization".into()),
    };
    // Never construct the full FP32 tensor and then pack it. Each temporary
    // decoded chunk is at most 1 MiB, including during initialization.
    // 262,144 values are 1,024 K-quant blocks. For the packed embedding the
    // chunks are decoded only to check finiteness; the raw bytes are kept.
    let mut result = if half {
        Vec::with_capacity(count)
    } else {
        Vec::new()
    };
    for chunk in bytes.chunks(block_bytes * (262_144 / block_values)) {
        let n = chunk.len() / block_bytes * block_values;
        let values = dequantize(t, chunk, n)?;
        finite(&values)?;
        if half {
            let begin = result.len();
            result.resize(begin + n, 0);
            f32_to_half(&values, &mut result[begin..])?;
            // Exponent bits all set means inf/NaN: a finite FP32 above
            // the FP16 maximum (65504) overflowed. Reject, never clamp.
            // Values below the FP16 subnormal range round to zero silently.
            if result[begin..].iter().any(|b| b & 0x7c00 == 0x7c00) {
                return Err("non-finite half tensor".into());
            }
        }
    }
    if half {
        assert_eq!(result.len(), count);
        Ok(TensorData::F16(result))
    } else {
        Ok(TensorData::Q5(bytes.to_vec()))
    }
}

fn is_half_matrix(name: &str) -> bool {
    name.starts_with("blk.")
        && (name.ends_with("ffn_up.weight") || name.ends_with("ffn_down.weight"))
}

pub(crate) struct HalfMatrix(Vec<u16>);
impl HalfMatrix {
    pub(crate) fn new(values: Vec<u16>) -> Self {
        Self(values)
    }
    pub(crate) fn as_f32<'a>(&self, scratch: &'a mut Vec<f32>) -> &'a [f32] {
        // This buffer belongs to one scoring call, shared across its layers
        // and prompt/suffix. Keep its initialized maximum length; shrinking
        // between matrices would re-zero-fill it. No model-wide mutable cache.
        if scratch.capacity() < self.0.len() {
            scratch.reserve_exact(self.0.len().saturating_sub(scratch.len()));
        }
        if scratch.len() < self.0.len() {
            scratch.resize(self.0.len(), 0.0);
        }
        half_to_f32(&self.0, &mut scratch[..self.0.len()])
            .expect("validated FP16 matrix conversion");
        &scratch[..self.0.len()]
    }
}

fn finite(values: &[f32]) -> Result<(), String> {
    if values.iter().all(|x| x.is_finite()) {
        Ok(())
    } else {
        Err("non-finite tensor".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::real_assets;

    #[test]
    #[ignore = "requires public zenz (check-assets)"]
    fn every_packed_embedding_row_matches_eager_decoding() {
        let Some(dir) = real_assets::real_asset_path("MUU_ZENZ_MODEL_DIR") else {
            return;
        };
        let mut g = crate::gguf::Gguf::load(&dir.join(crate::ZENZ_MODEL_FILE)).unwrap();
        let embedding = g.embedding("token_embd.weight", &[768, 6000]).unwrap();
        let eager = dequantize(13, &embedding.0, 768 * 6000).unwrap();
        // Include the position addition used by the scorer, and compare bits.
        let position: Vec<_> = (0..768).map(|i| i as f32 * 0.0001 - 0.02).collect();
        for token in 0..6000 {
            let mut row = Vec::new();
            embedding.append(token, &position, &mut row);
            for i in 0..768 {
                assert_eq!(
                    row[i].to_bits(),
                    (eager[token * 768 + i] + position[i]).to_bits()
                );
            }
        }
    }

    #[test]
    fn packed_storage_rejects_wrong_type_and_non_finite_blocks() {
        assert!(decode_storage("token_embd.weight", 0, &[0; 4], 1).is_err());
        let mut bytes = [0; 176];
        bytes[..2].copy_from_slice(&0x7c00u16.to_le_bytes());
        assert!(decode_storage("token_embd.weight", 13, &bytes, 256).is_err());
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct VImageBuffer {
    data: *mut std::ffi::c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}
#[cfg(target_os = "macos")]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vImageConvert_PlanarFtoPlanar16F(
        src: *const VImageBuffer,
        dst: *const VImageBuffer,
        flags: u32,
    ) -> isize;
    fn vImageConvert_Planar16FtoPlanarF(
        src: *const VImageBuffer,
        dst: *const VImageBuffer,
        flags: u32,
    ) -> isize;
}
#[cfg(target_os = "macos")]
fn f32_to_half(src: &[f32], dst: &mut [u16]) -> Result<(), String> {
    if src.len() != dst.len() {
        return Err("half conversion length mismatch".into());
    }
    if src.is_empty() {
        return Ok(());
    }
    let a = VImageBuffer {
        data: src.as_ptr().cast_mut().cast(),
        height: 1,
        width: src.len(),
        row_bytes: src.len() * 4,
    };
    let b = VImageBuffer {
        data: dst.as_mut_ptr().cast(),
        height: 1,
        width: dst.len(),
        row_bytes: dst.len() * 2,
    };
    // kvImageDoNotTile: conversion does not create extra worker threads.
    let status = unsafe { vImageConvert_PlanarFtoPlanar16F(&a, &b, 16) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("vImage F32->F16: {status}"))
    }
}
#[cfg(target_os = "macos")]
fn half_to_f32(src: &[u16], dst: &mut [f32]) -> Result<(), String> {
    if src.len() != dst.len() {
        return Err("half conversion length mismatch".into());
    }
    if src.is_empty() {
        return Ok(());
    }
    let a = VImageBuffer {
        data: src.as_ptr().cast_mut().cast(),
        height: 1,
        width: src.len(),
        row_bytes: src.len() * 2,
    };
    let b = VImageBuffer {
        data: dst.as_mut_ptr().cast(),
        height: 1,
        width: dst.len(),
        row_bytes: dst.len() * 4,
    };
    let status = unsafe { vImageConvert_Planar16FtoPlanarF(&a, &b, 16) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("vImage F16->F32: {status}"))
    }
}

// Keep the existing non-macOS correctness path buildable. Supported IME
// builds use vImage above; the fallback preserves portable correctness.
#[cfg(not(target_os = "macos"))]
fn f32_to_half(src: &[f32], dst: &mut [u16]) -> Result<(), String> {
    if src.len() != dst.len() {
        return Err("half conversion length mismatch".into());
    }
    for (&value, out) in src.iter().zip(dst) {
        *out = rounded_half(value);
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn half_to_f32(src: &[u16], dst: &mut [f32]) -> Result<(), String> {
    if src.len() != dst.len() {
        return Err("half conversion length mismatch".into());
    }
    for (value, out) in src.iter().zip(dst) {
        *out = crate::gguf::half(&value.to_le_bytes());
    }
    Ok(())
}
#[cfg(any(test, not(target_os = "macos")))]
fn rounded_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let mantissa = bits & 0x7fffff;
    let exponent = ((bits >> 23) & 255) as i32;
    if exponent == 255 {
        return sign | 0x7c00 | u16::from(mantissa != 0);
    }
    let exponent = exponent - 127 + 15;
    if exponent >= 31 {
        return sign | 0x7c00;
    }
    let (truncated, remainder, halfway) = if exponent <= 0 {
        if exponent < -10 {
            return sign;
        }
        let shift = (14 - exponent) as u32;
        let value = mantissa | 0x800000;
        (value >> shift, value & ((1 << shift) - 1), 1 << (shift - 1))
    } else {
        (
            ((exponent as u32) << 10) | (mantissa >> 13),
            mantissa & 0x1fff,
            0x1000,
        )
    };
    sign | (truncated
        + u32::from(remainder > halfway || (remainder == halfway && truncated & 1 != 0)))
        as u16
}

#[cfg(test)]
mod half_tests {
    use super::*;
    #[test]
    fn half_conversion_rounds_ties_and_handles_boundaries() {
        let src = [
            0.0,
            -0.0,
            1.0,
            -2.0,
            65504.0,
            2f32.powi(-14),
            2f32.powi(-24),
            1.0 + 2f32.powi(-11),
        ];
        let mut half = [0; 8];
        f32_to_half(&src, &mut half).unwrap();
        assert_eq!(half, [0, 0x8000, 0x3c00, 0xc000, 0x7bff, 0x0400, 1, 0x3c00]);
        assert_eq!(half, src.map(rounded_half));
        let mut result = [0.0; 8];
        half_to_f32(&half, &mut result).unwrap();
        for i in 0..7 {
            assert_eq!(result[i].to_bits(), src[i].to_bits());
        }
        assert_eq!(result[7], 1.0);
        assert!(half_to_f32(&half, &mut result[..7]).is_err());
        assert!(f32_to_half(&src, &mut half[..7]).is_err());
        f32_to_half(&[], &mut []).unwrap();
        half_to_f32(&[], &mut []).unwrap();
        // Check every finite half value and both sides of each midpoint.
        let source: Vec<f32> = (0u16..=u16::MAX)
            .filter(|v| v & 0x7c00 != 0x7c00)
            .map(|v| crate::gguf::half(&v.to_le_bytes()))
            .collect();
        let mut actual = vec![0; source.len()];
        f32_to_half(&source, &mut actual).unwrap();
        for (v, bits) in source.iter().zip(actual) {
            assert_eq!(rounded_half(*v), bits);
        }
        let source: Vec<f32> = (0u16..0x7bff)
            .flat_map(|b| {
                let lo = crate::gguf::half(&b.to_le_bytes());
                let hi = crate::gguf::half(&(b + 1).to_le_bytes());
                let mid = (lo + hi) * 0.5;
                [
                    f32::from_bits(mid.to_bits() - 1),
                    mid,
                    f32::from_bits(mid.to_bits() + 1),
                    -f32::from_bits(mid.to_bits() - 1),
                    -mid,
                    -f32::from_bits(mid.to_bits() + 1),
                ]
            })
            .collect();
        let mut actual = vec![0; source.len()];
        f32_to_half(&source, &mut actual).unwrap();
        for (v, bits) in source.iter().zip(actual) {
            assert_eq!(rounded_half(*v), bits, "value {v}");
        }
    }
    #[test]
    fn only_ffn_weights_use_half_and_loading_is_bounded() {
        for name in [
            "output.weight",
            "token_embd.weight",
            "position_embd.weight",
            "blk.0.attn_output.weight",
            "blk.0.attn_qkv.weight",
            "blk.0.ffn_norm.weight",
            "blk.0.ffn_up.bias",
        ] {
            assert!(!is_half_matrix(name), "{name}");
        }
        for name in ["blk.0.ffn_up.weight", "blk.11.ffn_down.weight"] {
            assert!(is_half_matrix(name));
        }
        let source: Vec<u8> = (0..262_144 + 256)
            .flat_map(|i| ((i % 1000) as f32 / 100.0).to_le_bytes())
            .collect();
        let TensorData::F16(half) =
            decode_storage("blk.0.ffn_up.weight", 0, &source, 262_144 + 256).unwrap()
        else {
            panic!("expected half")
        };
        assert_eq!(half.len(), 262_144 + 256);
        assert_eq!(half[262_144], rounded_half(1.44));
        assert!(decode_storage("blk.0.ffn_up.weight", 0, &70000f32.to_le_bytes(), 1).is_err());
        assert!(decode_storage("blk.0.ffn_up.weight", 0, &f32::NAN.to_le_bytes(), 1).is_err());
        let matrix = HalfMatrix::new(half);
        let mut scratch = Vec::new();
        assert_eq!(matrix.as_f32(&mut scratch).len(), 262_144 + 256);
        let ptr = scratch.as_ptr();
        let capacity = scratch.capacity();
        HalfMatrix::new(vec![0; 17]).as_f32(&mut scratch);
        assert_eq!(scratch.capacity(), capacity);
        assert_eq!(scratch.as_ptr(), ptr);
    }
}
