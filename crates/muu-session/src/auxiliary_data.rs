//! Pinned Mozc presentation data, outside the conversion lattice and its
//! dictionary fingerprint. All Sessions share this one immutable index.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use muu_conversion::{AuxiliaryEntry, AuxiliaryKind};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

const EMOTICONS: &str =
    include_str!("../../../third_party/mozc-oss/src/data/emoticon/emoticon.tsv");
const SINGLE_KANJI: &str =
    include_str!("../../../third_party/mozc-oss/src/data/single_kanji/single_kanji.tsv");
const EMOJI: &str = include_str!("../../../third_party/mozc-oss/src/data/emoji/emoji_data.tsv");

pub(crate) struct Catalog {
    pub kind: AuxiliaryKind,
    pub source_sha256: [u8; 32],
    pub data_sha256: [u8; 32],
    pub entries: Vec<Arc<AuxiliaryEntry>>,
    readings: BTreeMap<String, Vec<Arc<AuxiliaryEntry>>>,
}

impl Catalog {
    fn load(kind: AuxiliaryKind, source: &str) -> Result<Self, String> {
        // Preserve source priority, deduplicate NFC surfaces and aliases, then
        // fingerprint the canonical transform rather than the raw source only.
        let mut ids = BTreeMap::<String, usize>::new();
        let mut surfaces = Vec::<String>::new();
        let mut readings = BTreeMap::<String, Vec<usize>>::new();
        let mut pairs = BTreeSet::new();
        let mut add = |reading: &str, surface: &str| -> Result<(), String> {
            // Aliases name exact complete segment readings. In particular,
            // retain upstream case/width variants (ABCD and ＡＢＣＤ); do not
            // silently replace those aliases with a new lowercase spelling.
            let reading: String = reading.nfc().collect();
            let surface: String = surface.nfc().collect();
            if reading.is_empty() || reading.chars().any(char::is_control) {
                return Err("invalid auxiliary reading".into());
            }
            // Reuse the candidate surface contract, including byte bounds.
            AuxiliaryEntry::new(kind, [0; 32], 0, &surface).map_err(|e| e.to_string())?;
            let next = surfaces.len();
            let id = *ids.entry(surface.clone()).or_insert_with(|| {
                surfaces.push(surface);
                next
            });
            if pairs.insert((reading.clone(), id)) {
                readings.entry(reading).or_default().push(id);
            }
            Ok(())
        };
        for line in source.lines().filter(|line| !line.starts_with('#')) {
            let columns = line.split('\t').collect::<Vec<_>>();
            match kind {
                AuxiliaryKind::Emoticon => {
                    if columns.first() == Some(&"") {
                        continue; // upstream header
                    }
                    // Categories are optional upstream and are not used for
                    // candidate presentation (no random/category rewriter).
                    if !(2..=3).contains(&columns.len()) {
                        return Err("invalid pinned emoticon columns".into());
                    }
                    for reading in columns[1].split_whitespace() {
                        add(reading, columns[0])?;
                    }
                }
                AuxiliaryKind::SingleKanji => {
                    if columns.len() != 2 {
                        return Err("invalid pinned single-kanji columns".into());
                    }
                    for c in columns[1].chars() {
                        add(columns[0], c.encode_utf8(&mut [0; 4]))?;
                    }
                }
                AuxiliaryKind::Emoji => {
                    if columns.len() != 7 {
                        return Err("invalid pinned emoji columns".into());
                    }
                    for reading in columns[2].split_whitespace() {
                        add(reading, columns[1])?;
                    }
                }
            }
        }
        let mut canonical = Sha256::new();
        canonical.update(b"muu-auxiliary-data\t1\n");
        canonical.update(kind.stable_name());
        canonical.update(b"\n");
        // JSON length framing avoids ambiguity from tabs or punctuation in
        // display strings. BTreeMap order and per-reading source order are fixed.
        for surface in &surfaces {
            canonical.update(serde_json::to_vec(surface).map_err(|e| e.to_string())?);
            canonical.update(b"\n");
        }
        for pair in &readings {
            canonical.update(serde_json::to_vec(&pair).map_err(|e| e.to_string())?);
            canonical.update(b"\n");
        }
        let data_sha256: [u8; 32] = canonical.finalize().into();
        let entries = surfaces
            .iter()
            .enumerate()
            .map(|(i, surface)| {
                AuxiliaryEntry::new(kind, data_sha256, i as u32, surface)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            kind,
            source_sha256: Sha256::digest(source.as_bytes()).into(),
            data_sha256,
            readings: readings
                .into_iter()
                .map(|(key, ids)| (key, ids.into_iter().map(|i| entries[i].clone()).collect()))
                .collect(),
            entries,
        })
    }

    pub(crate) fn for_reading(&self, reading: &str) -> &[Arc<AuxiliaryEntry>] {
        self.readings.get(reading).map(Vec::as_slice).unwrap_or(&[])
    }
}

// Built once per process from the pinned Mozc files compiled in. A parse
// failure is a build defect, hence the expect; the unit tests load them.
pub(crate) fn catalogs() -> &'static [Catalog] {
    static DATA: OnceLock<Vec<Catalog>> = OnceLock::new();
    DATA.get_or_init(|| {
        [
            (AuxiliaryKind::Emoticon, EMOTICONS),
            (AuxiliaryKind::SingleKanji, SINGLE_KANJI),
            (AuxiliaryKind::Emoji, EMOJI),
        ]
        .into_iter()
        .map(|(kind, text)| Catalog::load(kind, text).expect("validated pinned auxiliary data"))
        .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_sources_and_canonical_pairs_are_complete_and_shared() {
        let tables = catalogs();
        assert_eq!(tables.len(), 3);
        for (table, count, pairs, sha) in [
            (
                &tables[0],
                437,
                971,
                "366558b380bef07dda26822c9100d1efabee539f7961b2c6363d4614c4a762c4",
            ),
            (
                &tables[1],
                6397,
                23568,
                "bfa6a4be1319a144f770ba281dfc04091a05320e334357f949b293dc5f9c3d0c",
            ),
            (
                &tables[2],
                1918,
                9382,
                "91e8d93f5e71b7fd821b30c8d72a7f5f22214fab5d8157d009600b304fe0cf08",
            ),
        ] {
            assert_eq!(table.entries.len(), count);
            assert_eq!(table.readings.values().map(Vec::len).sum::<usize>(), pairs);
            assert_eq!(
                format!(
                    "{:x}",
                    Sha256::digest(match table.kind {
                        AuxiliaryKind::Emoticon => EMOTICONS,
                        AuxiliaryKind::SingleKanji => SINGLE_KANJI,
                        AuxiliaryKind::Emoji => EMOJI,
                    })
                ),
                sha
            );
            for entries in table.readings.values() {
                for entry in entries {
                    assert!(Arc::ptr_eq(
                        entry,
                        &table.entries[entry.entry_id() as usize]
                    ));
                    assert_eq!(entry.surface(), entry.surface().nfc().collect::<String>());
                }
            }
        }
        assert!(std::ptr::eq(tables, catalogs()));
        assert!(
            tables[0]
                .entries
                .iter()
                .any(|entry| entry.surface() == "m(_ _)m")
        );
    }

    #[test]
    fn native_alias_case_and_width_remain_exact_reading_keys() {
        let emoji = catalogs()
            .iter()
            .find(|table| table.kind == AuxiliaryKind::Emoji)
            .unwrap();
        assert!(!emoji.for_reading("ABCD").is_empty());
        assert!(!emoji.for_reading("ＡＢＣＤ").is_empty());
        assert_eq!(emoji.for_reading("ABCD"), emoji.for_reading("ＡＢＣＤ"));
        assert!(!emoji.for_reading("abcd").is_empty());
        assert_ne!(emoji.for_reading("ABCD"), emoji.for_reading("abcd"));
        // Every source alias remains reachable, not just a transformed key
        // with the same count. Surface NFC deduplication keeps its identity.
        for line in EMOJI.lines().filter(|line| !line.starts_with('#')) {
            let columns = line.split('\t').collect::<Vec<_>>();
            let surface: String = columns[1].nfc().collect();
            for alias in columns[2].split_whitespace() {
                let key: String = alias.nfc().collect();
                assert!(
                    emoji
                        .for_reading(&key)
                        .iter()
                        .any(|entry| entry.surface() == surface)
                );
            }
        }
    }
}
