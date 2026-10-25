//! GPT-2 byte BPE with the pinned Japanese-character vocabulary. The public
//! ZenzContext preprocessing is applied before encoding (space -> U+3000,
//! newline removed). No BOS is inserted: this GGUF declares add_bos=false.
use fancy_regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

pub struct Tokenizer {
    vocab: HashMap<String, u32>,
    ranks: HashMap<(String, String), usize>,
    byte_chars: [char; 256],
    regex: Regex,
    pub eos: u32,
    pub sha: [u8; 32],
}

impl Tokenizer {
    pub fn new(meta: &BTreeMap<String, Value>) -> Result<Self, String> {
        if meta.get("tokenizer.ggml.model").and_then(Value::as_str) != Some("gpt2")
            || meta.get("tokenizer.ggml.pre").and_then(Value::as_str)
                != Some("gpt2-small-japanese-char")
        {
            return Err("unsupported tokenizer".into());
        }
        if meta
            .get("tokenizer.ggml.add_bos_token")
            .and_then(Value::as_bool)
            != Some(false)
        {
            return Err("unexpected BOS convention".into());
        }
        let tokens = meta
            .get("tokenizer.ggml.tokens")
            .and_then(Value::as_array)
            .ok_or("missing vocabulary")?;
        let vocab: HashMap<String, u32> = tokens
            .iter()
            .enumerate()
            .map(|(i, t)| Ok((t.as_str().ok_or("invalid token")?.to_owned(), i as u32)))
            .collect::<Result<_, String>>()?;
        if tokens.len() != 6000 || vocab.len() != 6000 || !vocab.contains_key("[UNK]") {
            return Err("unexpected vocabulary".into());
        }
        let merges = meta
            .get("tokenizer.ggml.merges")
            .and_then(Value::as_array)
            .ok_or("missing BPE merges")?;
        let ranks = merges
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (a, b) = t
                    .as_str()
                    .and_then(|s| s.split_once(' '))
                    .ok_or("invalid merge")?;
                Ok(((a.to_owned(), b.to_owned()), i))
            })
            .collect::<Result<_, String>>()?;
        // GPT-2's bytes_to_unicode: printable Latin-1 bytes map to themselves
        // and the rest to U+0100 upward, so every byte has a visible symbol.
        let mut byte_chars = ['\0'; 256];
        let mut extra = 0;
        for (b, c) in byte_chars.iter_mut().enumerate() {
            *c = if (33..=126).contains(&b) || (161..=172).contains(&b) || (174..=255).contains(&b)
            {
                char::from_u32(b as u32).unwrap()
            } else {
                let c = char::from_u32(256 + extra).unwrap();
                extra += 1;
                c
            };
        }
        // The GGUF has no separate tokenizer file, so this identifies the
        // tokenizer by the `tokenizer.*` metadata as serde_json serializes it.
        // It is not a file digest (docs/src/internals/storage.md「記録」 says SHA-256 of the
        // tokenizer); the whole file is `weights_sha`.
        let tokenizer_meta: BTreeMap<_, _> = meta
            .iter()
            .filter(|(k, _)| k.starts_with("tokenizer."))
            .collect();
        let sha =
            Sha256::digest(serde_json::to_vec(&tokenizer_meta).map_err(|e| e.to_string())?).into();
        // The GGUF metadata incorrectly calls token 2 (<s>) EOS. The pinned
        // azooKey llama fork ignores these special-id keys and uses GPT-2's
        // EOS=3 (</s>). Match that public implementation and muu's explicit
        // </s> convention, not the inconsistent metadata integer.
        let eos = *vocab.get("</s>").ok_or("missing </s>")?;
        if eos != 3 {
            return Err("invalid EOS".into());
        }
        let regex =
            Regex::new(r"'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)")
                .map_err(|e| e.to_string())?;
        Ok(Self {
            vocab,
            ranks,
            byte_chars,
            regex,
            eos,
            sha,
        })
    }
    pub fn encode(&self, text: &str) -> Result<Vec<u32>, String> {
        if text.len() > 16 * 1024 {
            return Err("zenz tokenizer input limit".into());
        }
        let text = text.replace(' ', "\u{3000}").replace('\n', "");
        let mut result = Vec::new();
        let mut end = 0;
        for m in self.regex.find_iter(&text) {
            let m = m.map_err(|e| e.to_string())?;
            if m.start() > end {
                self.word(&text[end..m.start()], &mut result)
            }
            self.word(m.as_str(), &mut result);
            end = m.end();
        }
        if end < text.len() {
            self.word(&text[end..], &mut result)
        }
        Ok(result)
    }
    fn word(&self, word: &str, out: &mut Vec<u32>) {
        let mut pieces: Vec<String> = word
            .bytes()
            .map(|b| self.byte_chars[b as usize].to_string())
            .collect();
        while pieces.len() > 1 {
            let best = pieces
                .windows(2)
                .enumerate()
                .filter_map(|(i, p)| {
                    self.ranks
                        .get(&(p[0].clone(), p[1].clone()))
                        .map(|&r| (r, i))
                })
                .min();
            let Some((_, i)) = best else { break };
            let right = pieces.remove(i + 1);
            pieces[i].push_str(&right);
        }
        for p in pieces {
            // Ċ/Ġ are the mapped newline and space bytes. After the
            // preprocessing above they cannot arise; kept to mirror b4846.
            if p == "Ċ" || p == "Ġ" {
                out.push(self.vocab["[UNK]"])
            } else if let Some(&id) = self.vocab.get(&p) {
                out.push(id)
            } else {
                // Match b4846's fallback: search vocabulary for each raw UTF-8
                // byte, not for its byte-to-Unicode remapping a second time.
                // A character outside the vocabulary therefore yields no token
                // at all, and surfaces differing only by it score the same.
                // This branch is rare with the pinned vocabulary: 232 of the
                // 256 byte symbols are tokens, so an unmerged character falls
                // back to byte tokens instead. Every JIS X 0208 kanji encodes
                // to at least one token; characters whose UTF-8 needs a
                // missing byte (some C0 controls, 0xF2, 0xF4 and above) drop.
                for b in p.bytes().filter(u8::is_ascii) {
                    if let Some(&id) = self.vocab.get(&(b as char).to_string()) {
                        out.push(id)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn regex_runtime_error_is_returned_instead_of_tokenizing_a_partial_tail() {
        let regex = fancy_regex::RegexBuilder::new(r"(?=(a+))\1b")
            .backtrack_limit(0)
            .build()
            .unwrap();
        let tokenizer = Tokenizer {
            vocab: HashMap::from([("[UNK]".into(), 0)]),
            ranks: HashMap::new(),
            byte_chars: std::array::from_fn(|i| char::from_u32(i as u32).unwrap()),
            regex,
            eos: 3,
            sha: [0; 32],
        };
        assert!(tokenizer.encode("aaaaaaaaaaaa!").is_err());
    }
}
