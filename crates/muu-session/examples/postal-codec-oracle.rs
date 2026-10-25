//! Offline, explicit-input codec comparison tool; no paths, network or writes.
//! `--inspect` emits only counts and hashes for format comparisons.
use muu_session::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead};
fn bytes(v: &Value, key: &str) -> Result<Vec<u8>, ()> {
    base64::decode(v[key].as_str().ok_or(())?).map_err(|_| ())
}
fn run(v: Value, inspect: bool) -> Result<Value, ()> {
    let input = bytes(&v, "bytes")?;
    let mode = v["mode"].as_str().ok_or(())?;
    if mode == "manifest" {
        let tsv = bytes(&v, "tsv")?;
        let s = decode_postal_code_tsv(&tsv).map_err(|_| ())?;
        let m = decode_postal_code_manifest(&input, &tsv, s.entries.len()).map_err(|_| ())?;
        let out = encode_postal_code_manifest(&m).map_err(|_| ())?;
        return Ok(if inspect {
            json!({"ok":true,"count":m.entry_count,"snapshot_sha256":format!("{:x}",Sha256::digest(&tsv)),"manifest_sha256":format!("{:x}",Sha256::digest(&input)),"canonical_manifest_sha256":format!("{:x}",Sha256::digest(&out))})
        } else {
            json!({"ok":true,"bytes":base64::encode(out)})
        });
    }
    let s = match mode {
        "csv" => decode_postal_code_csv(&input[..]),
        "tsv" => decode_postal_code_tsv(&input),
        "zip" => prepare_postal_code_zip(&input, 0, None).map(|p| p.snapshot),
        _ => return Err(()),
    }
    .map_err(|_| ())?;
    let out = encode_postal_code_tsv(&s).map_err(|_| ())?;
    Ok(if inspect {
        json!({"ok":true,"count":s.entries.len(),"snapshot_sha256":format!("{:x}",Sha256::digest(out))})
    } else {
        json!({"ok":true,"count":s.entries.len(),"bytes":base64::encode(out)})
    })
}
fn main() {
    let inspect = std::env::args().any(|s| s == "--inspect");
    for line in io::stdin().lock().lines() {
        let result = line
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .and_then(|v| run(v, inspect).ok())
            .unwrap_or_else(|| json!({"ok":false}));
        println!("{result}");
    }
}
