//! CPU inference for the conversion model: a decoder-only transformer with
//! RMSNorm, rotary positions, grouped-query attention and a SwiGLU MLP
//! (Hugging Face `LlamaForCausalLM` weights in safetensors form).
//!
//! Scoring computes the shared prompt once, then every candidate as one
//! batch attending to the prompt's cached keys and values.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};

const HIDDEN: usize = 512;
const HEADS: usize = 8;
const KEY_VALUE_HEADS: usize = 2;
const HEAD_DIM: usize = HIDDEN / HEADS;
const KEY_VALUE_DIM: usize = KEY_VALUE_HEADS * HEAD_DIM;
const MLP_DIM: usize = 2048;
const LAYERS: usize = 8;
const NORM_EPSILON: f32 = 1e-5;
const ROPE_THETA: f32 = 10_000.0;
/// Positions the model was trained with; longer inputs are not scored.
const MAX_POSITIONS: usize = 1024;

struct Layer {
    attention_norm: Vec<f32>,
    query: Vec<f32>,
    key: Vec<f32>,
    value: Vec<f32>,
    output: Vec<f32>,
    mlp_norm: Vec<f32>,
    gate: Vec<f32>,
    up: Vec<f32>,
    down: Vec<f32>,
}

pub(crate) struct Model {
    embedding: Vec<f32>,
    layers: Vec<Layer>,
    final_norm: Vec<f32>,
    inverse_frequency: Vec<f32>,
    vocabulary: usize,
}

/// Rows that attend to the shared past plus their own earlier rows.
struct Span {
    rows: usize,
    start: usize,
}

type KeysValues = (Vec<f32>, Vec<f32>);

impl Model {
    // Validate this backend's tensor layout. The fetch tool owns artifact
    // pins; the loader rejects unsupported architecture variants.
    //
    // Refused: a header naming any tensor outside the fixed set below
    // (embedding, final norm, and the nine per-layer weights for each of
    // LAYERS layers), and, in `tensor`, a listed tensor that is missing, not
    // F32, of the wrong shape, or out of the payload. The digest is of the
    // bytes read and is reported, not checked against a pin.
    pub(crate) fn load(path: &Path) -> Result<(Self, [u8; 32]), String> {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        // Hash the same bytes that supply tensors (docs/src/internals/storage.md「記録」). Reopening the path
        // later could attribute a loaded model to a replacement artifact.
        let digest = Sha256::digest(&bytes).into();
        let header_len = bytes
            .get(..8)
            .and_then(|prefix| prefix.try_into().ok())
            .map(u64::from_le_bytes)
            .and_then(|len| usize::try_from(len).ok())
            .ok_or("truncated header")?;
        let header_end = 8usize
            .checked_add(header_len)
            .ok_or("invalid header length")?;
        let header: HashMap<String, serde_json::Value> =
            serde_json::from_slice(bytes.get(8..header_end).ok_or("truncated header")?)
                .map_err(|error| error.to_string())?;
        // This backend ties its output head to embedding. Do not silently
        // accept another architecture just because the tensors we read fit.
        let mut supported = std::collections::HashSet::from([
            "__metadata__".to_owned(),
            "model.embed_tokens.weight".to_owned(),
            "model.norm.weight".to_owned(),
        ]);
        for index in 0..LAYERS {
            for part in [
                "input_layernorm",
                "self_attn.q_proj",
                "self_attn.k_proj",
                "self_attn.v_proj",
                "self_attn.o_proj",
                "post_attention_layernorm",
                "mlp.gate_proj",
                "mlp.up_proj",
                "mlp.down_proj",
            ] {
                supported.insert(format!("model.layers.{index}.{part}.weight"));
            }
        }
        if let Some(name) = header.keys().find(|name| !supported.contains(*name)) {
            return Err(format!("unsupported tensor {name}"));
        }
        let data = &bytes[header_end..];
        let tensor = |name: &str, shape: &[usize]| -> Result<Vec<f32>, String> {
            let entry = header
                .get(name)
                .ok_or_else(|| format!("missing tensor {name}"))?;
            let actual: Option<Vec<usize>> = entry["shape"].as_array().map(|dims| {
                dims.iter()
                    .filter_map(|dim| dim.as_u64().and_then(|dim| usize::try_from(dim).ok()))
                    .collect()
            });
            if entry["dtype"] != "F32" || actual.as_deref() != Some(shape) {
                return Err(format!("unexpected layout for {name}"));
            }
            let bound = |index: usize| {
                entry["data_offsets"][index]
                    .as_u64()
                    .and_then(|offset| usize::try_from(offset).ok())
            };
            let range = bound(0).zip(bound(1)).ok_or("invalid offsets")?;
            let raw = data.get(range.0..range.1).ok_or("tensor outside file")?;
            if raw.len() != shape.iter().product::<usize>() * 4 {
                return Err(format!("unexpected size for {name}"));
            }
            Ok(raw
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect())
        };
        let embedding_shape = header
            .get("model.embed_tokens.weight")
            .and_then(|entry| entry["shape"][0].as_u64())
            .and_then(|rows| usize::try_from(rows).ok())
            .ok_or("missing embedding")?;
        let mut layers = Vec::with_capacity(LAYERS);
        for index in 0..LAYERS {
            let name = |part: &str| format!("model.layers.{index}.{part}.weight");
            layers.push(Layer {
                attention_norm: tensor(&name("input_layernorm"), &[HIDDEN])?,
                query: tensor(&name("self_attn.q_proj"), &[HIDDEN, HIDDEN])?,
                key: tensor(&name("self_attn.k_proj"), &[KEY_VALUE_DIM, HIDDEN])?,
                value: tensor(&name("self_attn.v_proj"), &[KEY_VALUE_DIM, HIDDEN])?,
                output: tensor(&name("self_attn.o_proj"), &[HIDDEN, HIDDEN])?,
                mlp_norm: tensor(&name("post_attention_layernorm"), &[HIDDEN])?,
                gate: tensor(&name("mlp.gate_proj"), &[MLP_DIM, HIDDEN])?,
                up: tensor(&name("mlp.up_proj"), &[MLP_DIM, HIDDEN])?,
                down: tensor(&name("mlp.down_proj"), &[HIDDEN, MLP_DIM])?,
            });
        }
        Ok((
            Self {
                embedding: tensor("model.embed_tokens.weight", &[embedding_shape, HIDDEN])?,
                layers,
                final_norm: tensor("model.norm.weight", &[HIDDEN])?,
                inverse_frequency: (0..HEAD_DIM / 2)
                    .map(|index| ROPE_THETA.powf(-(2.0 * index as f32) / HEAD_DIM as f32))
                    .collect(),
                vocabulary: embedding_shape,
            },
            digest,
        ))
    }

    // Rows of the input embedding: 6,003 for the pinned checkpoint, the
    // 6,000 BPE tokens plus the tokenizer's three added prompt marks.
    pub(crate) fn vocabulary(&self) -> usize {
        self.vocabulary
    }

    /// Negative log-likelihood of each candidate's tokens after `prefix`.
    /// Token ids must be below `vocabulary()`; candidates must be non-empty.
    /// Inputs beyond the trained position range score as infinitely unlikely.
    pub(crate) fn score(&self, prefix: &[u32], candidates: &[Vec<u32>]) -> Vec<f32> {
        // Conservative by one: a candidate's last token (EOS) is only a
        // target, so the highest position used is prefix + longest - 2.
        let longest = candidates.iter().map(Vec::len).max().unwrap_or(0);
        if prefix.is_empty()
            || candidates.iter().any(Vec::is_empty)
            || prefix.len() + longest > MAX_POSITIONS
        {
            return vec![f32::INFINITY; candidates.len()];
        }
        let (prefix_hidden, past) = self.forward(
            self.embed(prefix),
            &[Span {
                rows: prefix.len(),
                start: 0,
            }],
            None,
            true,
        );
        // Each candidate token is predicted from the row before it, so the
        // candidate pass takes every token except the last as input.
        let spans: Vec<Span> = candidates
            .iter()
            .map(|candidate| Span {
                rows: candidate.len() - 1,
                start: prefix.len(),
            })
            .collect();
        let inputs: Vec<u32> = candidates
            .iter()
            .flat_map(|candidate| candidate[..candidate.len() - 1].iter().copied())
            .collect();
        let (candidate_hidden, _) = self.forward(self.embed(&inputs), &spans, Some(&past), false);

        let mut hidden = prefix_hidden[(prefix.len() - 1) * HIDDEN..].to_vec();
        hidden.extend_from_slice(&candidate_hidden);
        let rows = hidden.len() / HIDDEN;
        // The LM head is tied to the input embedding. `load` refuses any
        // tensor outside that layout, `lm_head.weight` included, so a model
        // with an untied head never reaches this point.
        let logits = linear(&hidden, rows, HIDDEN, &self.embedding, self.vocabulary);
        let token_nll = |row: usize, target: u32| -> f32 {
            let logits = &logits[row * self.vocabulary..(row + 1) * self.vocabulary];
            let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let log_sum = max
                + logits
                    .iter()
                    .map(|logit| (logit - max).exp())
                    .sum::<f32>()
                    .ln();
            log_sum - logits[target as usize]
        };
        let mut scores = Vec::with_capacity(candidates.len());
        let mut row = 1;
        for candidate in candidates {
            let mut total = token_nll(0, candidate[0]);
            for (offset, token) in candidate.iter().enumerate().skip(1) {
                total += token_nll(row + offset - 1, *token);
            }
            row += candidate.len() - 1;
            scores.push(total);
        }
        scores
    }

    fn embed(&self, ids: &[u32]) -> Vec<f32> {
        ids.iter()
            .flat_map(|id| {
                let start = *id as usize * HIDDEN;
                self.embedding[start..start + HIDDEN].iter().copied()
            })
            .collect()
    }

    /// Runs every layer over `x`, whose rows are the spans laid end to end.
    /// With `keep`, also returns each layer's keys and values for reuse.
    fn forward(
        &self,
        mut x: Vec<f32>,
        spans: &[Span],
        past: Option<&[KeysValues]>,
        keep: bool,
    ) -> (Vec<f32>, Vec<KeysValues>) {
        let rows: usize = spans.iter().map(|span| span.rows).sum();
        let mut kept = Vec::new();
        for (index, layer) in self.layers.iter().enumerate() {
            let normed = rms_norm(&x, &layer.attention_norm);
            let mut queries = linear(&normed, rows, HIDDEN, &layer.query, HIDDEN);
            let mut keys = linear(&normed, rows, HIDDEN, &layer.key, KEY_VALUE_DIM);
            let values = linear(&normed, rows, HIDDEN, &layer.value, KEY_VALUE_DIM);
            let mut row = 0;
            for span in spans {
                for offset in 0..span.rows {
                    let position = span.start + offset;
                    self.rotate(&mut queries[row * HIDDEN..(row + 1) * HIDDEN], position);
                    self.rotate(
                        &mut keys[row * KEY_VALUE_DIM..(row + 1) * KEY_VALUE_DIM],
                        position,
                    );
                    row += 1;
                }
            }
            let (past_keys, past_values): (&[f32], &[f32]) = past
                .map(|past| (past[index].0.as_slice(), past[index].1.as_slice()))
                .unwrap_or((&[], &[]));
            let attended = attend(
                &queries,
                &keys,
                &values,
                past_keys,
                past_values,
                spans,
                rows,
            );
            let projected = linear(&attended, rows, HIDDEN, &layer.output, HIDDEN);
            add_assign(&mut x, &projected);

            let normed = rms_norm(&x, &layer.mlp_norm);
            let mut gated = linear(&normed, rows, HIDDEN, &layer.gate, MLP_DIM);
            let up = linear(&normed, rows, HIDDEN, &layer.up, MLP_DIM);
            for (gate, up) in gated.iter_mut().zip(&up) {
                // SiLU(gate) * up
                *gate = *gate / (1.0 + (-*gate).exp()) * up;
            }
            let down = linear(&gated, rows, MLP_DIM, &layer.down, HIDDEN);
            add_assign(&mut x, &down);
            if keep {
                kept.push((keys, values));
            }
        }
        (rms_norm(&x, &self.final_norm), kept)
    }

    /// Rotary position embedding, half-split layout, applied per head.
    fn rotate(&self, row: &mut [f32], position: usize) {
        for head in row.chunks_exact_mut(HEAD_DIM) {
            let (low, high) = head.split_at_mut(HEAD_DIM / 2);
            for (index, frequency) in self.inverse_frequency.iter().enumerate() {
                let (sin, cos) = (position as f32 * frequency).sin_cos();
                let (a, b) = (low[index], high[index]);
                low[index] = a * cos - b * sin;
                high[index] = b * cos + a * sin;
            }
        }
    }
}

/// Causal attention where each span sees the shared past and its own rows.
fn attend(
    queries: &[f32],
    keys: &[f32],
    values: &[f32],
    past_keys: &[f32],
    past_values: &[f32],
    spans: &[Span],
    rows: usize,
) -> Vec<f32> {
    let past_len = past_keys.len() / KEY_VALUE_DIM;
    let scale = (HEAD_DIM as f32).sqrt().recip();
    let mut attended = vec![0.0f32; rows * HIDDEN];
    let mut first = 0;
    for span in spans {
        let own = first * KEY_VALUE_DIM..(first + span.rows) * KEY_VALUE_DIM;
        let visible_keys = [past_keys, &keys[own.clone()]].concat();
        let visible_values = [past_values, &values[own]].concat();
        let mut weights = vec![0.0f32; past_len + span.rows];
        for offset in 0..span.rows {
            let row = first + offset;
            let visible = past_len + offset + 1;
            for head in 0..HEADS {
                let group = head / (HEADS / KEY_VALUE_HEADS);
                let query = &queries[row * HIDDEN + head * HEAD_DIM..][..HEAD_DIM];
                let mut max = f32::NEG_INFINITY;
                for (step, weight) in weights[..visible].iter_mut().enumerate() {
                    let key = &visible_keys[step * KEY_VALUE_DIM + group * HEAD_DIM..][..HEAD_DIM];
                    *weight = dot(query, key) * scale;
                    max = max.max(*weight);
                }
                let mut sum = 0.0;
                for weight in &mut weights[..visible] {
                    *weight = (*weight - max).exp();
                    sum += *weight;
                }
                let out = &mut attended[row * HIDDEN + head * HEAD_DIM..][..HEAD_DIM];
                for (step, weight) in weights[..visible].iter().enumerate() {
                    let value =
                        &visible_values[step * KEY_VALUE_DIM + group * HEAD_DIM..][..HEAD_DIM];
                    let weight = weight / sum;
                    for (out, value) in out.iter_mut().zip(value) {
                        *out += weight * value;
                    }
                }
            }
        }
        first += span.rows;
    }
    attended
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn add_assign(target: &mut [f32], source: &[f32]) {
    for (target, source) in target.iter_mut().zip(source) {
        *target += source;
    }
}

fn rms_norm(x: &[f32], weight: &[f32]) -> Vec<f32> {
    let mut normed = vec![0.0f32; x.len()];
    for (row, out) in x.chunks_exact(HIDDEN).zip(normed.chunks_exact_mut(HIDDEN)) {
        let scale = (row.iter().map(|value| value * value).sum::<f32>() / HIDDEN as f32
            + NORM_EPSILON)
            .sqrt()
            .recip();
        for ((out, value), weight) in out.iter_mut().zip(row).zip(weight) {
            *out = value * scale * weight;
        }
    }
    normed
}

/// `y[rows, outputs] = x[rows, inputs] * weight[outputs, inputs]^T`
fn linear(x: &[f32], rows: usize, inputs: usize, weight: &[f32], outputs: usize) -> Vec<f32> {
    assert_eq!(x.len(), rows * inputs, "linear input shape mismatch");
    assert_eq!(
        weight.len(),
        outputs * inputs,
        "linear weight shape mismatch"
    );
    let mut y = vec![0.0f32; rows * outputs];
    if rows > 0 {
        matmul_transposed(x, rows, inputs, weight, outputs, &mut y);
    }
    y
}

#[cfg(target_os = "macos")]
fn matmul_transposed(
    x: &[f32],
    rows: usize,
    inputs: usize,
    weight: &[f32],
    outputs: usize,
    y: &mut [f32],
) {
    const ROW_MAJOR: i32 = 101;
    const NO_TRANSPOSE: i32 = 111;
    const TRANSPOSE: i32 = 112;
    #[link(name = "Accelerate", kind = "framework")]
    unsafe extern "C" {
        fn cblas_sgemm(
            order: i32,
            transpose_a: i32,
            transpose_b: i32,
            m: i32,
            n: i32,
            k: i32,
            alpha: f32,
            a: *const f32,
            lda: i32,
            b: *const f32,
            ldb: i32,
            beta: f32,
            c: *mut f32,
            ldc: i32,
        );
    }
    // SAFETY: the slices hold exactly rows*inputs, outputs*inputs and
    // rows*outputs elements, matching the dimensions passed. Every caller
    // builds them that way; `linear` asserts the lengths in release too,
    // before reaching BLAS. Inputs and outputs are the model's
    // fixed sizes or its vocabulary, and rows are at most MAX_POSITIONS per
    // candidate times the candidates scored, so all fit in i32.
    unsafe {
        cblas_sgemm(
            ROW_MAJOR,
            NO_TRANSPOSE,
            TRANSPOSE,
            rows as i32,
            outputs as i32,
            inputs as i32,
            1.0,
            x.as_ptr(),
            inputs as i32,
            weight.as_ptr(),
            inputs as i32,
            0.0,
            y.as_mut_ptr(),
            outputs as i32,
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn matmul_transposed(
    x: &[f32],
    _rows: usize,
    inputs: usize,
    weight: &[f32],
    outputs: usize,
    y: &mut [f32],
) {
    for (row, out) in x.chunks_exact(inputs).zip(y.chunks_exact_mut(outputs)) {
        for (out, weights) in out.iter_mut().zip(weight.chunks_exact(inputs)) {
            *out = dot(row, weights);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::linear;

    // Zero rows never enter BLAS, even before the release guards existed.
    // This detects missing checks without invoking undefined native access.
    #[test]
    #[ignore = "release-only shape contract; run by check-scale"]
    #[should_panic(expected = "linear input shape mismatch")]
    fn linear_rejects_input_shape_in_release() {
        let _ = linear(&[0.0], 0, 2, &[0.0; 4], 2);
    }

    #[test]
    #[ignore = "release-only shape contract; run by check-scale"]
    #[should_panic(expected = "linear weight shape mismatch")]
    fn linear_rejects_weight_shape_in_release() {
        let _ = linear(&[], 0, 2, &[0.0], 2);
    }
}
