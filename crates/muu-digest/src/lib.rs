//! Byte-level digest formatting only. Domains own their serialization order,
//! tags, normalization and interpretation of identities.
//!
//! Shared because three crates spell SHA-256 the same way: muu-conversion
//! (overlay fingerprint), muu-session (Imported, Postal, record and
//! presentation identities) and muu-dict-cli (build output and Postal
//! manifests). The spellings must agree across them: session validates the
//! Postal manifest hashes that dict-cli writes, and compares them as strings.
//! One copy of the rule cannot drift.
//!
//! What a digest covers is not decided here. Each caller chooses the bytes;
//! crates that keep only a raw 32-byte digest (muu-dict payload, muu-lm
//! artifacts) and the Postal streaming CSV read use sha2 directly.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{self, Write};

/// Two lowercase digits per byte: the only spelling `is_sha256_hex` accepts.
pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

/// Exactly 64 lowercase hex digits. Uppercase is rejected, not folded, so one
/// digest has one spelling wherever it is compared or used as a file name.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

struct DigestWriter(Sha256);
impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Hash the same bytes as serde_json::to_vec without allocating the whole JSON.
/// Serialization errors stay with the caller, which knows its data contract.
/// The digest therefore depends on the caller's type: its field order and
/// serde attributes define the identity, not this function.
pub fn json_sha256(value: &impl Serialize) -> serde_json::Result<String> {
    let mut writer = DigestWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(hex_lower(&writer.0.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_bytes_and_lowercase_validation() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        for byte in 0..=255u8 {
            assert_eq!(hex_lower(&[byte]), format!("{byte:02x}"));
        }
        assert!(is_sha256_hex(&sha256_hex(b"abc")));
        for bad in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            "é".repeat(32),
        ] {
            assert!(!is_sha256_hex(&bad));
        }
    }

    #[test]
    fn streaming_json_keeps_bytes_and_propagates_errors() {
        let value = serde_json::json!({"語": ["\\\"", "改行\n", "é"], "null": null, "number": 17});
        assert_eq!(
            json_sha256(&value).unwrap(),
            sha256_hex(&serde_json::to_vec(&value).unwrap())
        );
        struct Invalid;
        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("fixture serialization failure"))
            }
        }
        assert!(
            json_sha256(&Invalid)
                .unwrap_err()
                .to_string()
                .contains("fixture serialization failure")
        );
    }
}
