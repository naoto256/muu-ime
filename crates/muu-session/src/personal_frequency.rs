//! The persisted learning model: per dictionary token, how often the user
//! explicitly chose it and how often they replaced it, bound to one system
//! dictionary by payload SHA-256 and token count (docs/src/internals/conversion.md「学習」, docs/src/internals/storage.md「保存する場所」). Stage ① turns
//! the counts into node cost changes; stage ② uses the positive counts as a
//! score bias.
// Spells the dictionary's own payload digest. The identity is that digest
// with the token count, as above; this helper adds no meaning.
use muu_digest::hex_lower as hex_sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use muu_conversion::{CostAdjustments, SnapshotRevision, TokenKey};
use muu_dict::{DictionaryFingerprint, DictionaryTokenId};
use serde::{Deserialize, Serialize};

// Each map holds at most MAX_ENTRIES tokens with counts 1..=MAX_COUNT. Every
// DECAY_INTERVAL commits all counts halve (zero drops the entry), so old
// habits fade. A selection lowers a node's cost by COST_STEP per power of two
// of its count, a rejection raises it by COST_STEP per count, each capped at
// MAX_COST_BONUS.
const SCHEMA_VERSION: u32 = 2;
const MAX_ENTRIES: usize = 4_096;
const MAX_COUNT: u16 = 255;
const DECAY_INTERVAL: u64 = 64;
const COST_STEP: i32 = 350;
const MAX_COST_BONUS: i32 = 1_400;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalFrequencyEntry {
    pub dictionary_token_id: u64,
    pub count: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PersonalFrequencySnapshot {
    pub schema_version: u32,
    pub dictionary_payload_sha256: Option<String>,
    pub dictionary_token_count: Option<u64>,
    pub observation_count: u64,
    pub entries: Vec<PersonalFrequencyEntry>,
    pub rejected_entries: Vec<PersonalFrequencyEntry>,
}

// Preserve the strict v1 boundary while adding one required v2 array (docs/src/internals/storage.md「保存する場所」).
// `default` applies only to an absent key: null cannot stand for an empty
// rejection history. Keeping a presence marker also rejects a v1 blob that
// smuggles in v2 fields instead of silently broadening the old format.
#[derive(Default)]
enum RejectedEntriesWire {
    #[default]
    Missing,
    Present(Vec<PersonalFrequencyEntry>),
}
impl<'de> Deserialize<'de> for RejectedEntriesWire {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Vec::deserialize(d).map(Self::Present)
    }
}
impl<'de> Deserialize<'de> for PersonalFrequencySnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u32,
            #[serde(deserialize_with = "deserialize_required_nullable")]
            dictionary_payload_sha256: Option<String>,
            #[serde(deserialize_with = "deserialize_required_nullable")]
            dictionary_token_count: Option<u64>,
            observation_count: u64,
            entries: Vec<PersonalFrequencyEntry>,
            #[serde(default)]
            rejected_entries: RejectedEntriesWire,
        }
        let w = Wire::deserialize(d)?;
        let rejected_entries = match (w.schema_version, w.rejected_entries) {
            (1, RejectedEntriesWire::Missing) => Vec::new(),
            (SCHEMA_VERSION, RejectedEntriesWire::Present(entries)) => entries,
            _ => {
                return Err(serde::de::Error::custom(
                    "unsupported learning schema or key set",
                ));
            }
        };
        Ok(Self {
            schema_version: w.schema_version,
            dictionary_payload_sha256: w.dictionary_payload_sha256,
            dictionary_token_count: w.dictionary_token_count,
            observation_count: w.observation_count,
            entries: w.entries,
            rejected_entries,
        })
    }
}

// An optional value is still a required v1 key. Serde's normal Option
// handling accepts an absent key; this field decoder accepts only explicit
// null or a value, matching the previous Swift decodeStrict boundary.
fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl Default for PersonalFrequencySnapshot {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            dictionary_payload_sha256: None,
            dictionary_token_count: None,
            observation_count: 0,
            entries: Vec::new(),
            rejected_entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PersonalFrequencyError {
    UnsupportedSchema,
    MissingDictionaryAuthority,
    DictionaryAuthorityMismatch,
    TooManyEntries,
    InvalidEntry,
}

#[derive(Clone, Debug)]
pub(crate) struct PersonalFrequencyModel {
    snapshot: PersonalFrequencySnapshot,
    counts: BTreeMap<DictionaryTokenId, u16>,
    rejected: BTreeMap<DictionaryTokenId, u16>,
}

impl PersonalFrequencyModel {
    pub(crate) fn new(
        mut snapshot: PersonalFrequencySnapshot,
        fingerprint: Option<DictionaryFingerprint>,
    ) -> Result<Self, PersonalFrequencyError> {
        validate_snapshot(&snapshot, fingerprint.as_ref())?;
        let counts = snapshot
            .entries
            .iter()
            .map(|entry| {
                (
                    DictionaryTokenId::new(entry.dictionary_token_id),
                    entry.count,
                )
            })
            .collect();
        let rejected = snapshot
            .rejected_entries
            .iter()
            .map(|entry| {
                (
                    DictionaryTokenId::new(entry.dictionary_token_id),
                    entry.count,
                )
            })
            .collect();
        // v1 imports retain every positive count; all future output is v2.
        snapshot.schema_version = SCHEMA_VERSION;
        Ok(Self {
            snapshot,
            counts,
            rejected,
        })
    }

    #[cfg(test)]
    pub(crate) fn replace(
        &mut self,
        snapshot: PersonalFrequencySnapshot,
        fingerprint: Option<DictionaryFingerprint>,
    ) -> Result<(), PersonalFrequencyError> {
        *self = Self::new(snapshot, fingerprint)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn observe_explicit_selection<'a>(
        &mut self,
        token_ids: impl IntoIterator<Item = &'a DictionaryTokenId>,
        fingerprint: DictionaryFingerprint,
    ) -> bool {
        self.observe_choices(token_ids, std::iter::empty(), fingerprint)
    }

    /// One call is one explicit commit: one tick of the shared observation
    /// clock, however many tokens it carries on either side. The two maps are
    /// evicted independently, so a long history of rejections can never push
    /// out a positive entry. A token may appear on both sides of one call
    /// (chosen in one segment, rejected in another); each map then counts it.
    pub(crate) fn observe_choices<'a>(
        &mut self,
        token_ids: impl IntoIterator<Item = &'a DictionaryTokenId>,
        rejected_ids: impl IntoIterator<Item = &'a DictionaryTokenId>,
        fingerprint: DictionaryFingerprint,
    ) -> bool {
        let selected = token_ids.into_iter().copied().collect::<BTreeSet<_>>();
        let rejected = rejected_ids.into_iter().copied().collect::<BTreeSet<_>>();
        if selected.is_empty() && rejected.is_empty() {
            return false;
        }
        self.attach_authority(fingerprint);
        self.snapshot.observation_count = self.snapshot.observation_count.saturating_add(1);
        let decay = self.snapshot.observation_count % DECAY_INTERVAL == 0;
        // Independent bounds protect the existing positive vocabulary from
        // negative-history eviction. Both maps use this one commit clock,
        // including an observation containing only rejected dictionary words.
        observe_counts(&mut self.counts, &selected, decay);
        observe_counts(&mut self.rejected, &rejected, decay);
        self.rebuild_entries();
        true
    }

    pub(crate) fn cost_adjustments(&self) -> CostAdjustments {
        let mut adjustments =
            CostAdjustments::new(SnapshotRevision::new(self.snapshot.observation_count));
        let mut deltas = BTreeMap::new();
        for (id, count) in &self.counts {
            let levels = i32::try_from(count.ilog2() + 1).unwrap_or(i32::MAX);
            deltas.insert(*id, -COST_STEP.saturating_mul(levels).min(MAX_COST_BONUS));
        }
        // Rejections grow linearly with their count while selections grow
        // with its logarithm, so a few rejections reach the cap that many
        // selections need. A stored rejection came from an exactly replaced
        // node, rather than merely being part of a rejected segment's path.
        // Its penalty still follows the token wherever it occurs (docs/src/internals/conversion.md「学習」).
        for (id, count) in &self.rejected {
            *deltas.entry(*id).or_default() += COST_STEP
                .saturating_mul(i32::from(*count))
                .min(MAX_COST_BONUS);
        }
        // add_node_cost replaces an existing value; publish the sum once so a
        // rejection cannot erase a positive bonus for the same token (docs/src/internals/conversion.md「学習」).
        for (id, delta) in deltas {
            adjustments.add_node_cost(TokenKey::Dictionary(id), delta);
        }
        adjustments
    }

    pub(crate) fn snapshot(&self) -> &PersonalFrequencySnapshot {
        &self.snapshot
    }

    fn attach_authority(&mut self, fingerprint: DictionaryFingerprint) {
        self.snapshot.dictionary_payload_sha256 = Some(hex_sha256(fingerprint.payload_sha256()));
        self.snapshot.dictionary_token_count = Some(fingerprint.token_count());
    }

    fn rebuild_entries(&mut self) {
        self.snapshot.entries = self
            .counts
            .iter()
            .map(|(token_id, count)| PersonalFrequencyEntry {
                dictionary_token_id: token_id.get(),
                count: *count,
            })
            .collect();
        self.snapshot.rejected_entries = self
            .rejected
            .iter()
            .map(|(id, count)| PersonalFrequencyEntry {
                dictionary_token_id: id.get(),
                count: *count,
            })
            .collect();
    }
}

// When a map is full, a new token evicts the lowest count (ties: lowest id)
// among tokens not observed in this same commit; if every entry was observed
// now, the new token is dropped instead.
fn observe_counts(
    counts: &mut BTreeMap<DictionaryTokenId, u16>,
    selected: &BTreeSet<DictionaryTokenId>,
    decay: bool,
) {
    if decay {
        counts.retain(|_, c| {
            *c /= 2;
            *c > 0
        });
    }
    for id in selected {
        if !counts.contains_key(id) && counts.len() >= MAX_ENTRIES {
            let victim = counts
                .iter()
                .filter(|(id, _)| !selected.contains(id))
                .min_by_key(|(id, c)| (**c, id.get()))
                .map(|(id, _)| *id);
            if let Some(id) = victim {
                counts.remove(&id);
            } else {
                continue;
            }
        }
        let c = counts.entry(*id).or_default();
        *c = c.saturating_add(1).min(MAX_COUNT);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CapturedPersonalFrequency {
    costs: CostAdjustments,
    // Capture the same decayed counts used by stage ① once at publication.
    // Inference borrows them without consulting or locking the current store.
    counts: Arc<BTreeMap<DictionaryTokenId, u16>>,
}

impl CapturedPersonalFrequency {
    pub(crate) fn new(model: &PersonalFrequencyModel) -> Self {
        Self {
            costs: model.cost_adjustments(),
            counts: Arc::new(model.counts.clone()),
        }
    }

    /// The stage ② bonus in nats for one hypothesis: for each distinct
    /// learned dictionary token in the run, `min(log2(1 + c), 4)`. Only
    /// positive counts take part; rejections act through stage ① costs alone.
    /// The per-token cap does not bound the sum, so a hypothesis with several
    /// learned tokens can gain more than 4 nats.
    pub(crate) fn selection_bias(
        &self,
        hypothesis: &muu_conversion::FullHypothesis,
        range: muu_conversion::ScalarRange,
    ) -> f32 {
        if self.counts.is_empty() {
            return 0.;
        }
        hypothesis
            .nodes
            .iter()
            .filter(|n| n.start >= range.start && n.end <= range.end)
            .filter_map(|n| match n.token_key {
                TokenKey::Dictionary(id) => Some(id),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .iter()
            .filter_map(|id| self.counts.get(id))
            .map(|c| (1. + f32::from(*c)).log2().min(4.))
            .sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.costs.is_empty()
    }
}

impl muu_conversion::CostProvider for CapturedPersonalFrequency {
    fn capture(&self) -> CostAdjustments {
        self.costs.clone()
    }
}

// The whole persisted form is checked before use: schema, size, presence of
// the dictionary authority whenever there is an entry, an exact match with
// the open dictionary, and strictly increasing token ids inside the
// dictionary with counts in range. Anything else rejects the file; nothing
// is repaired silently.
fn validate_snapshot(
    snapshot: &PersonalFrequencySnapshot,
    fingerprint: Option<&DictionaryFingerprint>,
) -> Result<(), PersonalFrequencyError> {
    if snapshot.schema_version != 1 && snapshot.schema_version != SCHEMA_VERSION {
        return Err(PersonalFrequencyError::UnsupportedSchema);
    }
    if snapshot.entries.len() > MAX_ENTRIES || snapshot.rejected_entries.len() > MAX_ENTRIES {
        return Err(PersonalFrequencyError::TooManyEntries);
    }
    if snapshot.entries.is_empty() && snapshot.rejected_entries.is_empty() {
        if snapshot.dictionary_payload_sha256.is_some() != snapshot.dictionary_token_count.is_some()
        {
            return Err(PersonalFrequencyError::MissingDictionaryAuthority);
        }
    } else if snapshot.dictionary_payload_sha256.is_none()
        || snapshot.dictionary_token_count.is_none()
    {
        return Err(PersonalFrequencyError::MissingDictionaryAuthority);
    }

    if let (Some(expected_sha), Some(expected_count)) = (
        snapshot.dictionary_payload_sha256.as_deref(),
        snapshot.dictionary_token_count,
    ) {
        let Some(fingerprint) = fingerprint else {
            return Err(PersonalFrequencyError::DictionaryAuthorityMismatch);
        };
        if expected_sha != hex_sha256(fingerprint.payload_sha256())
            || expected_count != fingerprint.token_count()
        {
            return Err(PersonalFrequencyError::DictionaryAuthorityMismatch);
        }
    }

    if snapshot.schema_version == 1 && !snapshot.rejected_entries.is_empty() {
        return Err(PersonalFrequencyError::UnsupportedSchema);
    }
    let token_count = snapshot.dictionary_token_count.unwrap_or(0);
    for entries in [&snapshot.entries, &snapshot.rejected_entries] {
        let mut previous = None;
        for entry in entries {
            if entry.count == 0
                || entry.count > MAX_COUNT
                || entry.dictionary_token_id >= token_count
                || previous.is_some_and(|value| value >= entry.dictionary_token_id)
            {
                return Err(PersonalFrequencyError::InvalidEntry);
            }
            previous = Some(entry.dictionary_token_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint() -> DictionaryFingerprint {
        DictionaryFingerprint::new([0x5a; 32], 100)
    }

    #[test]
    fn explicit_frequency_is_bounded_and_empty_snapshot_restores_zero_delta() {
        let mut model =
            PersonalFrequencyModel::new(PersonalFrequencySnapshot::default(), Some(fingerprint()))
                .unwrap();
        let token = DictionaryTokenId::new(7);
        assert!(model.observe_explicit_selection([&token], fingerprint()));
        assert_eq!(model.snapshot().entries[0].count, 1);
        let first = model.cost_adjustments();
        let empty = PersonalFrequencySnapshot {
            dictionary_payload_sha256: Some(hex_sha256(fingerprint().payload_sha256())),
            dictionary_token_count: Some(fingerprint().token_count()),
            ..PersonalFrequencySnapshot::default()
        };
        model.replace(empty, Some(fingerprint())).unwrap();
        assert_ne!(first, model.cost_adjustments());
        assert_eq!(model.snapshot().entries, Vec::new());
    }

    #[test]
    fn new_tokens_evict_the_least_used_entry_at_the_snapshot_bound() {
        let fingerprint = DictionaryFingerprint::new([0x6b; 32], MAX_ENTRIES as u64 + 1);
        let snapshot = PersonalFrequencySnapshot {
            schema_version: SCHEMA_VERSION,
            rejected_entries: Vec::new(),
            dictionary_payload_sha256: Some(hex_sha256(fingerprint.payload_sha256())),
            dictionary_token_count: Some(fingerprint.token_count()),
            observation_count: 1,
            entries: (0..MAX_ENTRIES)
                .map(|id| PersonalFrequencyEntry {
                    dictionary_token_id: id as u64,
                    count: 1,
                })
                .collect(),
        };
        let mut model = PersonalFrequencyModel::new(snapshot, Some(fingerprint)).unwrap();
        let new_token = DictionaryTokenId::new(MAX_ENTRIES as u64);

        assert!(model.observe_explicit_selection([&new_token], fingerprint));
        assert_eq!(model.snapshot().entries.len(), MAX_ENTRIES);
        assert_eq!(
            model.snapshot().entries.last().unwrap().dictionary_token_id,
            MAX_ENTRIES as u64
        );
        assert_ne!(model.snapshot().entries[0].dictionary_token_id, 0);
    }

    #[test]
    fn repeated_use_grows_logarithmically_and_periodic_decay_forgets_old_counts() {
        let mut model =
            PersonalFrequencyModel::new(PersonalFrequencySnapshot::default(), Some(fingerprint()))
                .unwrap();
        let old = DictionaryTokenId::new(7);
        for _ in 0..8 {
            model.observe_explicit_selection([&old], fingerprint());
        }
        assert_eq!(model.snapshot().entries[0].count, 8);
        let recent = DictionaryTokenId::new(9);
        for _ in 8..64 {
            model.observe_explicit_selection([&recent], fingerprint());
        }
        assert_eq!(model.snapshot().observation_count, 64);
        assert_eq!(model.snapshot().entries[0].count, 4);
    }

    #[test]
    fn persisted_history_requires_the_current_dictionary_binding() {
        let current = fingerprint();
        for schema_version in [1, 2] {
            for empty in [false, true] {
                let mut wire = serde_json::json!({
                    "schema_version": schema_version,
                    "dictionary_payload_sha256": hex_sha256(current.payload_sha256()),
                    "dictionary_token_count": current.token_count(),
                    "observation_count": 63,
                    "entries": if empty { vec![] } else {
                        vec![PersonalFrequencyEntry { dictionary_token_id: 7, count: MAX_COUNT }]
                    },
                    "rejected_entries": if empty || schema_version == 1 { vec![] } else {
                        vec![PersonalFrequencyEntry { dictionary_token_id: 99, count: 3 }]
                    },
                });
                if schema_version == 1 {
                    wire.as_object_mut().unwrap().remove("rejected_entries");
                }
                let bytes = serde_json::to_vec(&wire).unwrap();
                let snapshot: PersonalFrequencySnapshot = serde_json::from_slice(&bytes).unwrap();
                let accepted =
                    PersonalFrequencyModel::new(snapshot.clone(), Some(current)).unwrap();
                let mut expected = snapshot.clone();
                expected.schema_version = SCHEMA_VERSION;
                assert_eq!(accepted.snapshot(), &expected);
                let saved = serde_json::to_vec(accepted.snapshot()).unwrap();
                let reopened = PersonalFrequencyModel::new(
                    serde_json::from_slice(&saved).unwrap(),
                    Some(current),
                )
                .unwrap();
                assert_eq!(reopened.snapshot(), &expected);

                // The token count alone cannot identify a dictionary revision.
                for target in [
                    Some(DictionaryFingerprint::new([42; 32], current.token_count())),
                    Some(DictionaryFingerprint::new(
                        *current.payload_sha256(),
                        current.token_count() + 1,
                    )),
                    None,
                ] {
                    assert_eq!(
                        PersonalFrequencyModel::new(snapshot.clone(), target).unwrap_err(),
                        PersonalFrequencyError::DictionaryAuthorityMismatch
                    );
                }
            }
        }
    }

    #[test]
    fn snapshot_rejects_wrong_dictionary_unsorted_entries_and_zero_counts() {
        let mut snapshot = PersonalFrequencySnapshot {
            schema_version: SCHEMA_VERSION,
            rejected_entries: Vec::new(),
            dictionary_payload_sha256: Some(hex_sha256(fingerprint().payload_sha256())),
            dictionary_token_count: Some(100),
            observation_count: 1,
            entries: vec![PersonalFrequencyEntry {
                dictionary_token_id: 7,
                count: 1,
            }],
        };
        assert!(PersonalFrequencyModel::new(snapshot.clone(), Some(fingerprint())).is_ok());

        let mut wrong_dictionary = snapshot.clone();
        wrong_dictionary.dictionary_payload_sha256 = Some("0".repeat(64));
        assert_eq!(
            PersonalFrequencyModel::new(wrong_dictionary, Some(fingerprint())).unwrap_err(),
            PersonalFrequencyError::DictionaryAuthorityMismatch
        );

        let mut unsorted = snapshot.clone();
        unsorted.entries = vec![
            PersonalFrequencyEntry {
                dictionary_token_id: 9,
                count: 1,
            },
            PersonalFrequencyEntry {
                dictionary_token_id: 7,
                count: 1,
            },
        ];
        assert_eq!(
            PersonalFrequencyModel::new(unsorted, Some(fingerprint())).unwrap_err(),
            PersonalFrequencyError::InvalidEntry
        );

        snapshot.entries[0].count = 0;
        assert_eq!(
            PersonalFrequencyModel::new(snapshot, Some(fingerprint())).unwrap_err(),
            PersonalFrequencyError::InvalidEntry
        );

        let oversized_count = PersonalFrequencySnapshot {
            schema_version: SCHEMA_VERSION,
            rejected_entries: Vec::new(),
            dictionary_payload_sha256: Some(hex_sha256(fingerprint().payload_sha256())),
            dictionary_token_count: Some(100),
            observation_count: 1,
            entries: vec![PersonalFrequencyEntry {
                dictionary_token_id: 7,
                count: MAX_COUNT + 1,
            }],
        };
        assert_eq!(
            PersonalFrequencyModel::new(oversized_count, Some(fingerprint())).unwrap_err(),
            PersonalFrequencyError::InvalidEntry
        );
    }
}

#[cfg(test)]
mod rejection_tests {
    use super::*;
    fn fingerprint() -> DictionaryFingerprint {
        DictionaryFingerprint::new([0x42; 32], 10_000)
    }
    #[test]
    fn v1_import_preserves_counts_and_v2_requires_both_strict_arrays() {
        let v1 = serde_json::json!({"schema_version":1,"dictionary_payload_sha256":"42".repeat(32),
            "dictionary_token_count":10000,"observation_count":63,
            "entries":[{"dictionary_token_id":7,"count":8}]});
        let imported: PersonalFrequencySnapshot = serde_json::from_value(v1.clone()).unwrap();
        let model = PersonalFrequencyModel::new(imported, Some(fingerprint())).unwrap();
        assert_eq!(model.snapshot().schema_version, 2);
        assert_eq!(model.snapshot().observation_count, 63);
        assert_eq!(model.snapshot().entries[0].count, 8);
        assert!(model.snapshot().rejected_entries.is_empty());
        let v2 = serde_json::to_value(model.snapshot()).unwrap();
        assert_eq!(v2.as_object().unwrap().len(), 6);
        for key in [
            "schema_version",
            "dictionary_payload_sha256",
            "dictionary_token_count",
            "observation_count",
            "entries",
            "rejected_entries",
        ] {
            let mut missing = v2.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<PersonalFrequencySnapshot>(missing).is_err(),
                "{key}"
            );
        }
        let mut null = v2.clone();
        null["rejected_entries"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<PersonalFrequencySnapshot>(null).is_err());
        let mut unknown = v2;
        unknown["extra"] = serde_json::json!(0);
        assert!(serde_json::from_value::<PersonalFrequencySnapshot>(unknown).is_err());
        let mut wrong_v1 = v1;
        wrong_v1["rejected_entries"] = serde_json::json!([]);
        assert!(serde_json::from_value::<PersonalFrequencySnapshot>(wrong_v1).is_err());
    }
    #[test]
    fn rejected_counts_decay_cap_and_combine_with_positive_costs() {
        let mut model =
            PersonalFrequencyModel::new(Default::default(), Some(fingerprint())).unwrap();
        let a = DictionaryTokenId::new(1);
        let b = DictionaryTokenId::new(2);
        for _ in 0..8 {
            assert!(model.observe_choices([&a], [&b], fingerprint()));
        }
        assert_eq!(model.snapshot().observation_count, 8);
        for _ in 8..64 {
            model.observe_choices([&a], [], fingerprint());
        }
        assert_eq!(model.snapshot().observation_count, 64);
        assert_eq!(model.rejected[&b], 4);
        model.snapshot.observation_count = 127;
        assert!(model.observe_choices([], [&b], fingerprint()));
        assert_eq!(model.snapshot().observation_count, 128);
        assert_eq!(model.counts[&a], 16);
        assert_eq!(model.rejected[&b], 3);
        model.counts.insert(a, 255);
        model.rejected.insert(a, 255);
        model.observe_choices([&a], [&a], fingerprint());
        assert_eq!(model.counts[&a], 255);
        assert_eq!(model.rejected[&a], 255);
        let mut expected = CostAdjustments::new(SnapshotRevision::new(129));
        expected.add_node_cost(TokenKey::Dictionary(a), 0);
        expected.add_node_cost(TokenKey::Dictionary(b), 1050);
        assert_eq!(model.cost_adjustments(), expected);
        assert!(!model.observe_choices([], [], fingerprint()));
        model
            .replace(Default::default(), Some(fingerprint()))
            .unwrap();
        assert!(model.counts.is_empty());
        assert!(model.rejected.is_empty());
        assert!(model.cost_adjustments().is_empty());
    }
    #[test]
    fn rejection_capacity_is_independent_and_validates_its_entries() {
        let mut model =
            PersonalFrequencyModel::new(Default::default(), Some(fingerprint())).unwrap();
        model.counts = (0..4096).map(|i| (DictionaryTokenId::new(i), 2)).collect();
        model.rejected = model.counts.clone();
        let positive = model.counts.clone();
        model.observe_choices([], [&DictionaryTokenId::new(4096)], fingerprint());
        assert_eq!(model.counts, positive);
        assert_eq!(model.rejected.len(), 4096);
        assert!(!model.rejected.contains_key(&DictionaryTokenId::new(0)));
        let mut snapshot = model.snapshot().clone();
        snapshot.rejected_entries[0].count = 0;
        assert_eq!(
            PersonalFrequencyModel::new(snapshot, Some(fingerprint())).unwrap_err(),
            PersonalFrequencyError::InvalidEntry
        );
        let mut snapshot = model.snapshot().clone();
        snapshot
            .rejected_entries
            .push(snapshot.rejected_entries[0].clone());
        assert_eq!(
            PersonalFrequencyModel::new(snapshot, Some(fingerprint())).unwrap_err(),
            PersonalFrequencyError::TooManyEntries
        );
    }
}
