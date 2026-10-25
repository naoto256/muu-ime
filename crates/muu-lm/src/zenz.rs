//! Fixed zenz GPT-2 with FP32 computation. A token-prefix DAG shares prompt
//! and candidate rows; only ancestor keys are visible. Accelerate handles GELU and attention.
use super::{
    gguf::Gguf,
    zenz_tokenizer::Tokenizer,
    zenz_weights::{Embedding, HalfMatrix},
};
use serde_json::{Value, json};
use std::path::Path;
const H: usize = 768;
const HEADS: usize = 12;
const D: usize = 64;
const FF: usize = 3072;
const L: usize = 12;
const V: usize = 6000;
const MAX: usize = 1024;
struct Layer {
    an: Vec<f32>,
    ab: Vec<f32>,
    qkv: Vec<f32>,
    qb: Vec<f32>,
    out: Vec<f32>,
    ob: Vec<f32>,
    fnorm: Vec<f32>,
    fb: Vec<f32>,
    // Only the FFN matrices are FP16 at rest. QKV, the attention output,
    // norms, biases, positions and the LM head stay FP32.
    up: HalfMatrix,
    ub: Vec<f32>,
    down: HalfMatrix,
    db: Vec<f32>,
}
pub struct Zenz {
    embedding: Embedding,
    position: Vec<f32>,
    output: Vec<f32>,
    norm: Vec<f32>,
    nb: Vec<f32>,
    layers: Vec<Layer>,
    tokenizer: Tokenizer,
    pub weights_sha: [u8; 32],
    pub tokenizer_sha: [u8; 32],
}
struct Span {
    rows: usize,
    start: usize,
}
type Kv = (Vec<f32>, Vec<f32>);

impl Zenz {
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut g = Gguf::load(path)?;
        for (key, expected) in [
            ("general.architecture", json!("gpt2")),
            ("gpt2.embedding_length", json!(H)),
            ("gpt2.block_count", json!(L)),
            ("gpt2.feed_forward_length", json!(FF)),
            ("gpt2.attention.head_count", json!(HEADS)),
            ("gpt2.context_length", json!(MAX)),
        ] {
            if g.metadata.get(key) != Some(&expected) {
                return Err(format!("unsupported architecture: {key}"));
            }
        }
        let eps = g
            .metadata
            .get("gpt2.attention.layer_norm_epsilon")
            .and_then(Value::as_f64)
            .ok_or("missing norm epsilon")?;
        if (eps - 1e-5).abs() > 1e-10 {
            return Err("unsupported norm epsilon".into());
        }
        let tokenizer = Tokenizer::new(&g.metadata)?;
        let mut layers = Vec::with_capacity(L);
        for i in 0..L {
            layers.push(Layer {
                an: g.tensor(&format!("blk.{i}.attn_norm.weight"), &[H])?,
                ab: g.tensor(&format!("blk.{i}.attn_norm.bias"), &[H])?,
                qkv: g.tensor(&format!("blk.{i}.attn_qkv.weight"), &[H, 3 * H])?,
                qb: g.tensor(&format!("blk.{i}.attn_qkv.bias"), &[3 * H])?,
                out: g.tensor(&format!("blk.{i}.attn_output.weight"), &[H, H])?,
                ob: g.tensor(&format!("blk.{i}.attn_output.bias"), &[H])?,
                fnorm: g.tensor(&format!("blk.{i}.ffn_norm.weight"), &[H])?,
                fb: g.tensor(&format!("blk.{i}.ffn_norm.bias"), &[H])?,
                up: g.half_matrix(&format!("blk.{i}.ffn_up.weight"), &[H, FF])?,
                ub: g.tensor(&format!("blk.{i}.ffn_up.bias"), &[FF])?,
                down: g.half_matrix(&format!("blk.{i}.ffn_down.weight"), &[FF, H])?,
                db: g.tensor(&format!("blk.{i}.ffn_down.bias"), &[H])?,
            });
        }
        let m = Self {
            embedding: g.embedding("token_embd.weight", &[H, V])?,
            position: g.tensor("position_embd.weight", &[H, MAX])?,
            output: g.tensor("output.weight", &[H, V])?,
            norm: g.tensor("output_norm.weight", &[H])?,
            nb: g.tensor("output_norm.bias", &[H])?,
            layers,
            weights_sha: g.sha,
            tokenizer_sha: tokenizer.sha,
            tokenizer,
        };
        if !g.tensors.is_empty() {
            return Err("unconsumed GGUF tensor".into());
        }
        Ok(m)
    }
    fn tokens(
        &self,
        context: &str,
        reading: &str,
        surfaces: &[&str],
    ) -> Result<(Vec<u32>, Vec<Vec<u32>>), String> {
        // More than MAX_SCORED_HYPOTHESES surfaces is refused too, under the
        // same message; the edge backend has no such cap.
        if reading.is_empty()
            || surfaces.is_empty()
            || surfaces.len() > crate::MAX_SCORED_HYPOTHESES
        {
            return Err("empty scoring input".into());
        }
        let prefix = self.tokenizer.encode(&super::prompt(context, reading))?;
        let candidates = surfaces
            .iter()
            .map(|s| {
                let mut t = self.tokenizer.encode(s)?;
                t.push(self.tokenizer.eos);
                Ok(t)
            })
            .collect::<Result<_, String>>()?;
        Ok((prefix, candidates))
    }
    fn embed(&self, ids: &[u32], spans: &[Span]) -> Vec<f32> {
        let mut x = Vec::with_capacity(ids.len() * H);
        let mut row = 0;
        for span in spans {
            for p in span.start..span.start + span.rows {
                let token = ids[row] as usize;
                // Holds for tokenizer output (exactly V tokens) and for the
                // positions `score` admits; a failure here is a bug, not input.
                assert!(token < V && p < MAX);
                self.embedding
                    .append(token, &self.position[p * H..(p + 1) * H], &mut x);
                row += 1;
            }
        }
        x
    }
}
// Means and variances accumulate in f64 to limit rounding error when
// summing 768 values; the result is used in f32.
fn layer_norm(x: &[f32], weight: &[f32], bias: &[f32]) -> Vec<f32> {
    let mut y = Vec::with_capacity(x.len());
    for row in x.chunks_exact(H) {
        let mean = (row.iter().map(|v| *v as f64).sum::<f64>() / H as f64) as f32;
        let var = (row
            .iter()
            .map(|v| ((v - mean) * (v - mean)) as f64)
            .sum::<f64>()
            / H as f64) as f32;
        let inv = (var + 1e-5).sqrt().recip();
        y.extend(
            row.iter()
                .zip(weight)
                .zip(bias)
                .map(|((v, w), b)| (v - mean) * inv * w + b),
        );
    }
    y
}
fn attention(q: &[f32], k: &[f32], v: &[f32], pk: &[f32], pv: &[f32], spans: &[Span]) -> Vec<f32> {
    let rows = q.len() / H;
    let past = pk.len() / H;
    let mut out = vec![0.0; rows * H];
    let mut start = 0;
    for span in spans {
        for r in 0..span.rows {
            for head in 0..HEADS {
                let qr = &q[(start + r) * H + head * D..(start + r) * H + (head + 1) * D];
                let mut scores = Vec::with_capacity(past + r + 1);
                for p in 0..past + r + 1 {
                    let keys = if p < past {
                        &pk[p * H + head * D..p * H + (head + 1) * D]
                    } else {
                        &k[(start + p - past) * H + head * D
                            ..(start + p - past) * H + (head + 1) * D]
                    };
                    scores.push(qr.iter().zip(keys).map(|(a, b)| a * b).sum::<f32>() / 8.0);
                }
                let max = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                for s in &mut scores {
                    *s = (*s - max).exp()
                }
                let sum = scores.iter().sum::<f32>();
                let dest = &mut out[(start + r) * H + head * D..(start + r) * H + (head + 1) * D];
                for (p, score) in scores.iter().enumerate() {
                    let values = if p < past {
                        &pv[p * H + head * D..p * H + (head + 1) * D]
                    } else {
                        &v[(start + p - past) * H + head * D
                            ..(start + p - past) * H + (head + 1) * D]
                    };
                    let a = score / sum;
                    for (o, v) in dest.iter_mut().zip(values) {
                        *o += a * v
                    }
                }
            }
        }
        start += span.rows;
    }
    out
}
// -log softmax(logits)[target], shifted by the max (log-sum-exp) so that
// exp cannot overflow.
fn nll(logits: &[f32], target: u32) -> f32 {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    max + logits.iter().map(|v| (v - max).exp()).sum::<f32>().ln() - logits[target as usize]
}
fn linear(
    x: &[f32],
    rows: usize,
    inputs: usize,
    w: &[f32],
    outputs: usize,
    bias: Option<&[f32]>,
) -> Vec<f32> {
    let mut y = vec![0.0; rows * outputs];
    if rows == 0 {
        return y;
    }
    assert_eq!(x.len(), rows * inputs);
    assert_eq!(w.len(), outputs * inputs);
    // Row-major output = X * W^T. All arrays own contiguous validated FP32.
    // 101/111/112 are CblasRowMajor/NoTrans/Trans. W is stored output-major
    // (see gguf.rs), so it is passed transposed with ldb = inputs.
    unsafe {
        cblas_sgemm(
            101,
            111,
            112,
            rows.try_into().unwrap(),
            outputs.try_into().unwrap(),
            inputs.try_into().unwrap(),
            1.0,
            x.as_ptr(),
            inputs as i32,
            w.as_ptr(),
            inputs as i32,
            0.0,
            y.as_mut_ptr(),
            outputs as i32,
        )
    }
    if let Some(b) = bias {
        for row in y.chunks_exact_mut(outputs) {
            for (v, b) in row.iter_mut().zip(b) {
                *v += b
            }
        }
    }
    y
}
#[cfg(target_os = "macos")]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn cblas_sgemm(
        order: i32,
        ta: i32,
        tb: i32,
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

const DENSE_MAX_ROWS: usize = 1024;

#[cfg(target_os = "macos")]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vvtanhf(output: *mut f32, input: *const f32, count: *const i32);
    fn vvexpf(output: *mut f32, input: *const f32, count: *const i32);
}

// GPT-2's tanh GELU: 0.5x(1 + tanh(sqrt(2/pi)(x + 0.044715x^3))). Only tanh
// is vectorized; the formula itself stays that of the reference.
fn vector_gelu(values: &mut [f32]) {
    let argument: Vec<_> = values
        .iter()
        .map(|&v| 0.797_884_6 * v * (1.0 + 0.044_715 * v * v))
        .collect();
    let mut tangent = vec![0.0; values.len()];
    #[cfg(target_os = "macos")]
    let count = i32::try_from(argument.len()).expect("bounded external GELU");
    #[cfg(target_os = "macos")]
    unsafe {
        vvtanhf(tangent.as_mut_ptr(), argument.as_ptr(), &count)
    };
    #[cfg(not(target_os = "macos"))]
    for (to, from) in tangent.iter_mut().zip(&argument) {
        *to = from.tanh();
    }
    for (v, t) in values.iter_mut().zip(tangent) {
        *v = 0.5 * *v * (1.0 + t);
    }
}

struct AttentionPlan {
    rows: usize,
    keys: usize,
    // -inf hides keys from other branches as well as future positions.
    mask: Vec<f32>,
}

impl AttentionPlan {
    fn from_visible(keys: usize, visible: &[Vec<usize>]) -> Self {
        let rows = visible.len();
        assert!(rows <= DENSE_MAX_ROWS && keys <= DENSE_MAX_ROWS);
        let mut mask = vec![f32::NEG_INFINITY; rows * keys];
        for (row, path) in visible.iter().enumerate() {
            assert!(!path.is_empty());
            for &key in path {
                assert!(key < keys);
                mask[row * keys + key] = 0.0;
            }
        }
        Self { rows, keys, mask }
    }

    fn spans(past: usize, spans: &[Span]) -> Option<Self> {
        let rows: usize = spans.iter().map(|s| s.rows).sum();
        let keys = past + rows;
        if rows > DENSE_MAX_ROWS || keys > DENSE_MAX_ROWS {
            return None;
        }
        let mut start = 0;
        let mut visible = Vec::with_capacity(rows);
        for span in spans {
            for r in 0..span.rows {
                visible.push((0..past).chain(past + start..=past + start + r).collect());
            }
            start += span.rows;
        }
        Some(Self::from_visible(keys, &visible))
    }

    fn apply(&self, q: &[f32], k: &[f32], v: &[f32]) -> Vec<f32> {
        assert_eq!(q.len(), self.rows * H);
        assert_eq!(k.len(), self.keys * H);
        assert_eq!(v.len(), self.keys * H);
        let mut out = vec![0.0; q.len()];
        let mut scores = vec![0.0; self.rows * self.keys];
        let mut exponential = vec![0.0; scores.len()];
        #[cfg(target_os = "macos")]
        let count = i32::try_from(scores.len()).expect("bounded external attention");
        for head in 0..HEADS {
            // Head columns are strided by H in the interleaved Q/K/V matrices.
            // alpha 0.125 = 1/sqrt(D). Masked keys become -inf, then exp 0;
            // every row sees at least itself, so its max stays finite.
            unsafe {
                cblas_sgemm(
                    101,
                    111,
                    112,
                    self.rows as i32,
                    self.keys as i32,
                    D as i32,
                    0.125,
                    q.as_ptr().add(head * D),
                    H as i32,
                    k.as_ptr().add(head * D),
                    H as i32,
                    0.0,
                    scores.as_mut_ptr(),
                    self.keys as i32,
                );
            }
            for (s, mask) in scores.iter_mut().zip(&self.mask) {
                *s += mask;
            }
            for row in scores.chunks_exact_mut(self.keys) {
                let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                for s in row {
                    *s -= max;
                }
            }
            #[cfg(target_os = "macos")]
            unsafe {
                vvexpf(exponential.as_mut_ptr(), scores.as_ptr(), &count)
            };
            #[cfg(not(target_os = "macos"))]
            for (to, from) in exponential.iter_mut().zip(&scores) {
                *to = from.exp();
            }
            for row in exponential.chunks_exact_mut(self.keys) {
                let sum = row.iter().sum::<f32>();
                for s in row {
                    *s /= sum;
                }
            }
            unsafe {
                cblas_sgemm(
                    101,
                    111,
                    111,
                    self.rows as i32,
                    D as i32,
                    self.keys as i32,
                    1.0,
                    exponential.as_ptr(),
                    self.keys as i32,
                    v.as_ptr().add(head * D),
                    H as i32,
                    0.0,
                    out.as_mut_ptr().add(head * D),
                    H as i32,
                );
            }
        }
        out
    }
}

struct TrieInput {
    ids: Vec<u32>,
    positions: Vec<usize>,
    visible: Vec<Vec<usize>>,
    // The hidden row predicting each candidate target token, including EOS.
    predictors: Vec<Vec<usize>>,
}

// A node is keyed by (parent row, token), so only identical token prefixes
// share rows; equal text that BPE splits differently stays separate. A
// candidate row sits at position prefix.len() + depth, as it would alone.
// Each target is predicted by its parent's row, the last prompt row for the
// first token. EOS is only predicted, never fed as an input row.
impl TrieInput {
    fn new(prefix: &[u32], candidates: &[Vec<u32>]) -> Option<Self> {
        if prefix.len() > DENSE_MAX_ROWS {
            return None;
        }
        let mut ids = prefix.to_vec();
        let mut positions: Vec<_> = (0..prefix.len()).collect();
        let mut visible: Vec<Vec<_>> = (0..prefix.len()).map(|r| (0..=r).collect()).collect();
        let root = prefix.len() - 1;
        let mut children = std::collections::HashMap::new();
        let mut predictors = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let mut parent = root;
            let mut path = Vec::with_capacity(candidate.len());
            for (depth, &token) in candidate.iter().enumerate() {
                path.push(parent);
                if depth + 1 == candidate.len() {
                    break; // EOS is predicted, never fed as an input row.
                }
                if let Some(&child) = children.get(&(parent, token)) {
                    parent = child;
                } else {
                    if ids.len() == DENSE_MAX_ROWS {
                        return None;
                    }
                    let index = ids.len();
                    ids.push(token);
                    positions.push(prefix.len() + depth);
                    let mut ancestry = visible[parent].clone();
                    ancestry.push(index);
                    visible.push(ancestry);
                    children.insert((parent, token), index);
                    parent = index;
                }
            }
            predictors.push(path);
        }
        Some(Self {
            ids,
            positions,
            visible,
            predictors,
        })
    }
}

impl Zenz {
    pub fn score(
        &self,
        context: &str,
        reading: &str,
        surfaces: &[&str],
    ) -> Result<Vec<f32>, String> {
        let (prefix, candidates) = self.tokens(context, reading, surfaces)?;
        // One candidate beyond the 1024-token context leaves every score
        // infinite. Selection then keeps the lattice choice; no candidate
        // is shortened or scored on its own to make it fit.
        // The prompt marks always encode to tokens, so an empty prefix does not
        // occur; the check keeps `TrieInput::new`'s `prefix.len() - 1` safe.
        if prefix.is_empty()
            || candidates
                .iter()
                .any(|c| c.is_empty() || prefix.len() + c.len() > MAX)
        {
            return Ok(vec![f32::INFINITY; candidates.len()]);
        }
        // The trie path needs prompt plus all distinct candidate rows within
        // DENSE_MAX_ROWS; otherwise the vector path scores the same targets
        // without sharing candidate prefixes.
        if let Some(trie) = TrieInput::new(&prefix, &candidates) {
            return Ok(self.score_trie(prefix.len(), &candidates, &trie));
        }
        Ok(self.score_vector(&prefix, &candidates))
    }

    fn score_vector(&self, prefix: &[u32], candidates: &[Vec<u32>]) -> Vec<f32> {
        let spans = [Span {
            rows: prefix.len(),
            start: 0,
        }];
        let mut weight_scratch = Vec::new();
        let (ph, past) = self.forward_vector(
            self.embed(prefix, &spans),
            &spans,
            None,
            true,
            &mut weight_scratch,
        );
        let spans: Vec<_> = candidates
            .iter()
            .map(|c| Span {
                rows: c.len() - 1,
                start: prefix.len(),
            })
            .collect();
        let inputs: Vec<_> = candidates
            .iter()
            .flat_map(|c| c[..c.len() - 1].iter().copied())
            .collect();
        let (ch, _) = self.forward_vector(
            self.embed(&inputs, &spans),
            &spans,
            Some(&past),
            false,
            &mut weight_scratch,
        );
        // Row 0 is the last prompt row, which predicts each first token. Then
        // every candidate's non-EOS rows follow, each predicting the next.
        let mut hidden = ph[(prefix.len() - 1) * H..].to_vec();
        hidden.extend(ch);

        let mut next_row = 1;
        let targets: Vec<Vec<_>> = candidates
            .iter()
            .map(|c| {
                let mut targets = vec![(0, c[0])];
                targets.extend(
                    c.iter()
                        .enumerate()
                        .skip(1)
                        .map(|(i, &t)| (next_row + i - 1, t)),
                );
                next_row += c.len() - 1;
                targets
            })
            .collect();
        self.score_hidden(&hidden, &targets)
    }

    fn forward_vector(
        &self,
        mut x: Vec<f32>,
        spans: &[Span],
        past: Option<&[Kv]>,
        keep: bool,
        weight_scratch: &mut Vec<f32>,
    ) -> (Vec<f32>, Vec<Kv>) {
        let rows = x.len() / H;
        let mut kept = Vec::new();
        if rows == 0 {
            return (x, kept);
        }
        let past_rows = past.map_or(0, |p| p[0].0.len() / H);
        // Past plus new rows over DENSE_MAX_ROWS gets no dense plan, and the
        // scalar `attention` computes the same causal per-span result.
        let plan = AttentionPlan::spans(past_rows, spans);
        for (i, l) in self.layers.iter().enumerate() {
            let norm = layer_norm(&x, &l.an, &l.ab);
            let qkv = linear(&norm, rows, H, &l.qkv, 3 * H, Some(&l.qb));
            let (q, k, v) = split_qkv(&qkv);
            let (pk, pv): (&[f32], &[f32]) = past
                .map(|p| (p[i].0.as_slice(), p[i].1.as_slice()))
                .unwrap_or((&[], &[]));
            let attended = if let Some(plan) = &plan {
                if pk.is_empty() {
                    plan.apply(&q, &k, &v)
                } else {
                    let mut all_k = pk.to_vec();
                    all_k.extend_from_slice(&k);
                    let mut all_v = pv.to_vec();
                    all_v.extend_from_slice(&v);
                    plan.apply(&q, &all_k, &all_v)
                }
            } else {
                attention(&q, &k, &v, pk, pv, spans)
            };

            drop(norm);
            drop(qkv);
            drop(q);
            if keep {
                kept.push((k, v));
            } else {
                drop(k);
                drop(v);
            }
            self.finish_layer(&mut x, &attended, l, weight_scratch);
        }
        (layer_norm(&x, &self.norm, &self.nb), kept)
    }

    fn finish_layer(
        &self,
        x: &mut [f32],
        attended: &[f32],
        l: &Layer,
        weight_scratch: &mut Vec<f32>,
    ) {
        let rows = x.len() / H;
        let projected = linear(attended, rows, H, &l.out, H, Some(&l.ob));
        for (a, b) in x.iter_mut().zip(projected) {
            *a += b;
        }
        let norm = layer_norm(x, &l.fnorm, &l.fb);
        // Each call widens the full up, then the full down matrix into the
        // same scratch: 2 per layer, 24 per forward pass. The vector path
        // makes two passes (prompt, then suffixes); the trie path makes one.
        let mut ff = linear(&norm, rows, H, l.up.as_f32(weight_scratch), FF, Some(&l.ub));

        drop(norm);
        for chunk in ff.chunks_mut(128 * FF) {
            vector_gelu(chunk);
        }

        let down = linear(&ff, rows, FF, l.down.as_f32(weight_scratch), H, Some(&l.db));
        for (a, b) in x.iter_mut().zip(down) {
            *a += b;
        }
    }

    fn score_hidden(&self, hidden: &[f32], targets: &[Vec<(usize, u32)>]) -> Vec<f32> {
        // Bound full-vocabulary logits to 128 rows, but retain every target's
        // NLL and sum it in the original candidate token order.
        let mut contributions: Vec<Vec<f32>> = targets.iter().map(|t| vec![0.0; t.len()]).collect();
        let mut first_row = 0;
        for chunk in hidden.chunks(128 * H) {
            let n = chunk.len() / H;
            let logits = linear(chunk, n, H, &self.output, V, None);
            for (to, targets) in contributions.iter_mut().zip(targets) {
                for (value, &(row, token)) in to.iter_mut().zip(targets) {
                    if (first_row..first_row + n).contains(&row) {
                        let r = row - first_row;
                        *value = nll(&logits[r * V..(r + 1) * V], token);
                    }
                }
            }
            first_row += n;
        }
        // Starting from 0.0 adds nothing (0 + a == a), so this keeps the
        // previous left-to-right rounding of each candidate's sum.
        contributions
            .into_iter()
            .map(|row| row.into_iter().fold(0.0, |sum, v| sum + v))
            .collect()
    }

    fn score_trie(
        &self,
        prefix_rows: usize,
        candidates: &[Vec<u32>],
        trie: &TrieInput,
    ) -> Vec<f32> {
        let rows = trie.ids.len();
        let mut weight_scratch = Vec::new();
        let mut x = Vec::with_capacity(rows * H);
        for (&token, &position) in trie.ids.iter().zip(&trie.positions) {
            self.embedding.append(
                token as usize,
                &self.position[position * H..(position + 1) * H],
                &mut x,
            );
        }
        let plan = AttentionPlan::from_visible(rows, &trie.visible);
        for layer in &self.layers {
            let norm = layer_norm(&x, &layer.an, &layer.ab);
            let qkv = linear(&norm, rows, H, &layer.qkv, 3 * H, Some(&layer.qb));
            let (q, k, v) = split_qkv(&qkv);
            let attended = plan.apply(&q, &k, &v);

            drop(norm);
            drop(qkv);
            drop(q);
            drop(k);
            drop(v);

            self.finish_layer(&mut x, &attended, layer, &mut weight_scratch);
        }
        let hidden = layer_norm(&x, &self.norm, &self.nb);
        // Earlier prompt rows predict no suffix target, so omit their LM head.
        let offset = prefix_rows - 1;

        let targets: Vec<Vec<_>> = candidates
            .iter()
            .zip(&trie.predictors)
            .map(|(c, p)| c.iter().zip(p).map(|(&t, &r)| (r - offset, t)).collect())
            .collect();
        self.score_hidden(&hidden[offset * H..], &targets)
    }
}

fn split_qkv(qkv: &[f32]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut q = Vec::with_capacity(qkv.len() / 3);
    let mut k = Vec::with_capacity(qkv.len() / 3);
    let mut v = Vec::with_capacity(qkv.len() / 3);
    for row in qkv.chunks_exact(3 * H) {
        q.extend_from_slice(&row[..H]);
        k.extend_from_slice(&row[H..2 * H]);
        v.extend_from_slice(&row[2 * H..]);
    }
    (q, k, v)
}

#[cfg(test)]
mod optimized_tests {
    use super::*;

    #[test]
    fn vector_gelu_preserves_the_tanh_definition() {
        let mut values: Vec<_> = (-2000..=2000).map(|i| i as f32 / 100.0).collect();
        let expected: Vec<_> = values
            .iter()
            .map(|&v| 0.5 * v * (1.0 + (0.797_884_6 * v * (1.0 + 0.044_715 * v * v)).tanh()))
            .collect();
        vector_gelu(&mut values);
        assert!(
            values
                .iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < 0.00001)
        );
    }

    #[test]
    fn dense_attention_preserves_prefix_and_separate_causal_branches() {
        let values = |n: usize, phase: f32| {
            (0..n)
                .map(|i| ((i as f32 * 0.17 + phase).sin()) * 0.3)
                .collect::<Vec<_>>()
        };
        let q = values(5 * H, 0.0);
        let k = values(5 * H, 1.0);
        let v = values(5 * H, 2.0);
        let pk = values(3 * H, 3.0);
        let pv = values(3 * H, 4.0);
        let spans = [Span { rows: 2, start: 3 }, Span { rows: 3, start: 3 }];
        let expected = attention(&q, &k, &v, &pk, &pv, &spans);
        let mut all_k = pk;
        all_k.extend(k);
        let mut all_v = pv;
        all_v.extend(v);
        let actual = AttentionPlan::spans(3, &spans)
            .unwrap()
            .apply(&q, &all_k, &all_v);
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < 0.00001)
        );
    }

    #[test]
    fn trie_shares_only_identical_token_prefix_and_keeps_predictors() {
        let prefix = [9, 8];
        let candidates = vec![vec![1, 2, 3], vec![1, 4, 3], vec![1, 2, 3], vec![3]];
        let trie = TrieInput::new(&prefix, &candidates).unwrap();
        assert_eq!(trie.ids, vec![9, 8, 1, 2, 4]);
        assert_eq!(trie.positions, vec![0, 1, 2, 3, 3]);
        assert_eq!(
            trie.predictors,
            vec![vec![1, 2, 3], vec![1, 2, 4], vec![1, 2, 3], vec![1]]
        );
        assert_eq!(trie.visible[4], vec![0, 1, 2, 4]);
        assert!(!trie.visible[4].contains(&3));
    }

    #[test]
    fn trie_stops_building_before_the_dense_row_limit_is_exceeded() {
        assert!(TrieInput::new(&vec![9; DENSE_MAX_ROWS], &[vec![1, 3]]).is_none());
        assert!(
            AttentionPlan::spans(
                DENSE_MAX_ROWS,
                &[Span {
                    rows: 1,
                    start: DENSE_MAX_ROWS
                }]
            )
            .is_none()
        );
    }
}

// Portable correctness path, like the edge backend's non-macOS fallback.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
unsafe fn cblas_sgemm(
    order: i32,
    ta: i32,
    tb: i32,
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
) {
    assert_eq!(order, 101);
    assert_eq!(ta, 111);
    assert!(tb == 111 || tb == 112);
    for row in 0..m as usize {
        for col in 0..n as usize {
            let mut sum = 0.0;
            for inner in 0..k as usize {
                let bi = if tb == 112 {
                    col * ldb as usize + inner
                } else {
                    inner * ldb as usize + col
                };
                // SAFETY: private callers validate dimensions and matrix lengths.
                unsafe {
                    sum += *a.add(row * lda as usize + inner) * *b.add(bi);
                }
            }
            // SAFETY: the caller allocated m rows at stride ldc.
            unsafe {
                let target = c.add(row * ldc as usize + col);
                *target = alpha * sum + beta * *target;
            }
        }
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;
    use crate::real_assets;
    #[test]
    #[ignore = "requires public zenz (check-assets)"]
    fn chunked_head_keeps_all_targets_and_trie_matches_independent_scoring() {
        let Some(dir) = real_assets::real_asset_path("MUU_ZENZ_MODEL_DIR") else {
            return;
        };
        let model = Zenz::load(&dir.join(crate::ZENZ_MODEL_FILE)).unwrap();
        let (prefix, candidates) = model
            .tokens(
                "日本語の",
                "へんかん",
                &["変換", "返還", "へんかん", "偏官"],
            )
            .unwrap();
        let trie = TrieInput::new(&prefix, &candidates).unwrap();
        let trie_scores = model.score_trie(prefix.len(), &candidates, &trie);
        let vector_scores = model.score_vector(&prefix, &candidates);
        // zip stops at the shorter side; without these, an empty or short
        // result would compare zero or fewer scores and still pass.
        assert_eq!(trie_scores.len(), 4);
        assert_eq!(vector_scores.len(), 4);
        for (a, b) in trie_scores.iter().zip(vector_scores) {
            assert!((*a - b).abs() <= 0.0001, "trie/vector: {a} {b}");
        }
        let hidden: Vec<f32> = (0..257 * H)
            .map(|i| ((i * 31 % 101) as f32 - 50.0) / 100.0)
            .collect();
        let targets = vec![
            vec![(0, 1), (127, 2), (128, 3), (255, 4), (256, 5)],
            vec![(128, 5999), (256, 0)],
        ];
        let logits = linear(&hidden, 257, H, &model.output, V, None);
        let expected: Vec<f32> = targets
            .iter()
            .map(|t| {
                t.iter().fold(0.0, |sum, &(r, t)| {
                    sum + nll(&logits[r * V..(r + 1) * V], t)
                })
            })
            .collect();
        let hidden_scores = model.score_hidden(&hidden, &targets);
        assert_eq!(expected.len(), 2);
        assert_eq!(hidden_scores.len(), 2);
        for (a, b) in expected.iter().zip(hidden_scores) {
            assert!((*a - b).abs() <= 0.0001, "head: {a} {b}");
        }
    }

    #[test]
    fn gelu_chunks_keep_values_across_the_row_boundary() {
        let mut full: Vec<f32> = (0..128 * FF + 17)
            .map(|i| (i % 1001) as f32 / 100.0 - 5.0)
            .collect();
        let mut chunks = full.clone();
        vector_gelu(&mut full);
        for chunk in chunks.chunks_mut(128 * FF) {
            vector_gelu(chunk);
        }
        assert!(
            full.iter()
                .zip(chunks)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
    }
}
