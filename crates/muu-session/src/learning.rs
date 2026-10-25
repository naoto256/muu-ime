//! Shared learning, with immutable cost captures and generation-bound observations.
//!
//! The store carries a `(generation, revision)` pair
//! (docs/src/internals/conversion.md「学習」). The revision
//! advances by one for every accepted observation and
//! tells the persistence side that there is something newer to write. The
//! generation advances only when learning is cleared, and it is what makes a
//! clear final: a conversion remembers the generation it captured, and an
//! observation from an earlier generation is dropped instead of re-teaching
//! what the user just erased.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// An in-process cursor. The persisted blob contains no persistence status.
///
/// The derived ordering compares the generation first and the revision
/// second, so the first state after a clear, `(g + 1, 0)`, is newer than every
/// `(g, n)`. Both the update query and the Swift cursors rely on that order.
/// Values restart at `(0, 0)` in each process and must not be persisted.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LearningVersion {
    pub generation: u64,
    pub revision: u64,
}

/// The serialized learning state together with the version it belongs to.
pub struct PersonalFrequencyUpdate {
    pub version: LearningVersion,
    pub data: Vec<u8>,
}

use muu_dict::{DictionaryFingerprint, DictionaryTokenId};

use crate::PersonalFrequencySnapshot;
use crate::personal_frequency::{CapturedPersonalFrequency, PersonalFrequencyModel};

/// What one conversion captures: the cost adjustments of a single learning
/// state, plus the generation its commit must present to `observe`.
pub(crate) struct LearningSnapshot {
    pub generation: u64,
    pub revision: u64,
    pub frequency: CapturedPersonalFrequency,
}

// One published state. The model and the capture derived from it are built
// together and never mutated afterward, so a reader holding the `Arc` sees
// counts, serialized bytes and cost adjustments that all belong to one version.
struct LearningState {
    model: PersonalFrequencyModel,
    captured: Arc<LearningSnapshot>,
}

impl LearningState {
    fn new(model: PersonalFrequencyModel, generation: u64, revision: u64) -> Self {
        let frequency = CapturedPersonalFrequency::new(&model);
        Self {
            model,
            captured: Arc::new(LearningSnapshot {
                generation,
                revision,
                frequency,
            }),
        }
    }
}

pub(crate) struct LearningStore {
    // Readers clone the pointer and leave. Observation writers also check
    // the generation and build the next private state under this lock, so a
    // clear cannot interleave between the check and addition (docs/src/internals/conversion.md「学習」).
    state: Mutex<Arc<LearningState>>,
}

#[derive(Clone, Copy)]
pub(crate) enum PublicationPoint {
    Before,
    After,
}

impl Default for LearningStore {
    fn default() -> Self {
        let model = PersonalFrequencyModel::new(PersonalFrequencySnapshot::default(), None)
            .expect("empty learning is valid without a dictionary");
        Self {
            state: Mutex::new(Arc::new(LearningState::new(model, 0, 0))),
        }
    }
}

impl LearningStore {
    #[cfg(test)]
    pub(crate) fn clear(&self) -> LearningVersion {
        self.clear_guarded(|| ())
    }

    /// Publishes an empty state under the next generation. The generation
    /// advances even when learning was already empty: the caller has deleted
    /// the persisted file, and observations from conversions that started
    /// before that must still be refused.
    ///
    /// `publication_guard` is called once the lock is held and its result
    /// lives until the swap is done; the Core passes its `PublicationGuard`,
    /// tests pass `|| ()`.
    pub(crate) fn clear_guarded<G>(&self, publication_guard: impl Fn() -> G) -> LearningVersion {
        let model = PersonalFrequencyModel::new(PersonalFrequencySnapshot::default(), None)
            .expect("empty learning is valid");
        let mut current = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let _publication = publication_guard();
        let version = LearningVersion {
            generation: current
                .captured
                .generation
                .checked_add(1)
                .expect("learning generation overflow"),
            revision: 0,
        };
        *current = Arc::new(LearningState::new(
            model,
            version.generation,
            version.revision,
        ));
        version
    }

    pub(crate) fn version(&self) -> LearningVersion {
        let captured = self.capture();
        LearningVersion {
            generation: captured.generation,
            revision: captured.revision,
        }
    }

    /// Copy the immutable state reference under the lock, then serialize it
    /// without a lock. A later observation cannot change this version's bytes.
    ///
    /// Read-only: `received` is the caller's cursor and is never stored here,
    /// so repeating a query cannot hide an update from a later one. Something
    /// is returned only when the current version is strictly newer.
    pub(crate) fn update(
        &self,
        received: Option<LearningVersion>,
    ) -> Result<Option<PersonalFrequencyUpdate>, serde_json::Error> {
        let current = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let version = LearningVersion {
            generation: current.captured.generation,
            revision: current.captured.revision,
        };
        // A cursor from a later generation than the current one cannot occur
        // in one process (versions are not persisted); it would simply get
        // no update.
        if received.is_some_and(|received| version <= received) {
            return Ok(None);
        }
        Ok(Some(PersonalFrequencyUpdate {
            version,
            data: serde_json::to_vec(current.model.snapshot())?,
        }))
    }

    #[cfg(test)]
    pub(crate) fn observe_panicking<G>(
        &self,
        generation: u64,
        tokens: &[DictionaryTokenId],
        fingerprint: DictionaryFingerprint,
        after: bool,
        publication_guard: impl Fn() -> G,
    ) {
        self.observe_guarded(
            generation,
            tokens,
            &[],
            fingerprint,
            |point| {
                if matches!(point, PublicationPoint::After) == after {
                    panic!("learning publication test");
                }
            },
            publication_guard,
        );
    }

    /// Installs the startup model as `(0, 0)`. Only valid before any session
    /// exists; afterward it would discard observations and reuse a version
    /// that readers may already hold.
    pub(crate) fn initialize(&self, model: PersonalFrequencyModel) {
        let next = Arc::new(LearningState::new(model, 0, 0));
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = next;
    }

    pub(crate) fn capture(&self) -> Arc<LearningSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .captured
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn observe(
        &self,
        generation: u64,
        tokens: &[DictionaryTokenId],
        fingerprint: DictionaryFingerprint,
    ) -> bool {
        self.observe_with_hook(generation, tokens, fingerprint, |_| {})
    }

    #[cfg(test)]
    fn observe_with_hook(
        &self,
        generation: u64,
        tokens: &[DictionaryTokenId],
        fingerprint: DictionaryFingerprint,
        hook: impl Fn(PublicationPoint),
    ) -> bool {
        self.observe_guarded(generation, tokens, &[], fingerprint, hook, || ())
    }

    pub(crate) fn observe_guarded<G>(
        &self,
        generation: u64,
        tokens: &[DictionaryTokenId],
        rejected: &[DictionaryTokenId],
        fingerprint: DictionaryFingerprint,
        hook: impl Fn(PublicationPoint),
        publication_guard: impl Fn() -> G,
    ) -> bool {
        let mut current = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // The generation check and addition cannot be split (docs/src/internals/conversion.md「学習」). The
        // Core guard begins after acquiring this shared publication lock,
        // rather than around preparation or read-only work (docs/src/internals/architecture.md「失敗したとき」).
        let _publication = publication_guard();
        if generation != current.captured.generation {
            return false;
        }
        // Generation checking and addition share this exclusion interval.
        // Only a private copy is modified, so unwinding cannot damage the
        // published model, counts, or cost capture.
        //
        // If the check were made under one lock acquisition and the addition
        // under another, a clear could land between them and the observation
        // would be added to the freshly emptied generation.
        //
        // The observation is applied to the current model, not to the one the
        // conversion captured, so selections committed by other sessions in
        // the meantime are kept.
        // `observe_choices` refuses an observation with no dictionary token
        // either way (an overlay-only or auxiliary commit); the revision then
        // stays, so the persistence side sees nothing new.
        let mut model = current.model.clone();
        if !model.observe_choices(tokens, rejected, fingerprint) {
            return false;
        }
        let revision = current
            .captured
            .revision
            .checked_add(1)
            .expect("learning revision overflow");
        let next = Arc::new(LearningState::new(model, generation, revision));
        hook(PublicationPoint::Before);
        *current = next;
        hook(PublicationPoint::After);
        drop(current);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn fingerprint() -> DictionaryFingerprint {
        DictionaryFingerprint::new([0x5a; 32], 100)
    }

    #[test]
    fn version_queries_are_repeatable_and_retaining_v1_does_not_hide_v2() {
        let store = LearningStore::default();
        let initial = store.update(None).unwrap().unwrap();
        assert_eq!(initial.data, store.update(None).unwrap().unwrap().data);
        assert!(store.update(Some(initial.version)).unwrap().is_none());
        store.observe(
            initial.version.generation,
            &[DictionaryTokenId::new(0)],
            fingerprint(),
        );
        let v1 = store.update(Some(initial.version)).unwrap().unwrap();
        store.observe(
            v1.version.generation,
            &[DictionaryTokenId::new(0)],
            fingerprint(),
        );
        let v2 = store.update(Some(v1.version)).unwrap().unwrap();
        assert!(v2.version > v1.version);
        assert_eq!(
            serde_json::from_slice::<PersonalFrequencySnapshot>(&v1.data)
                .unwrap()
                .observation_count,
            1
        );
        assert_eq!(
            serde_json::from_slice::<PersonalFrequencySnapshot>(&v2.data)
                .unwrap()
                .observation_count,
            2
        );
        assert_eq!(
            store.update(Some(v1.version)).unwrap().unwrap().data,
            v2.data
        );
        assert!(store.update(Some(v2.version)).unwrap().is_none());
        let cleared = store.clear();
        assert!(cleared.generation > v2.version.generation);
        assert!(!store.observe(
            v2.version.generation,
            &[DictionaryTokenId::new(0)],
            fingerprint()
        ));
        let empty = store.update(Some(v2.version)).unwrap().unwrap();
        assert_eq!(empty.version, cleared);
        assert!(
            serde_json::from_slice::<PersonalFrequencySnapshot>(&empty.data)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn emitted_learning_blobs_keep_strict_v2_shape_without_process_versions() {
        fn assert_v2(bytes: &[u8], expected: serde_json::Value) {
            let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            // Exact equality fixes the six root keys and both entry keys,
            // including explicit nulls and no generation/revision in the blob.
            assert_eq!(value, expected);
            assert!(value["schema_version"].is_u64());
            assert!(value["observation_count"].is_u64());
            if !value["dictionary_token_count"].is_null() {
                assert!(value["dictionary_token_count"].is_u64());
            }
            for entry in value["entries"].as_array().unwrap() {
                assert!(entry["dictionary_token_id"].is_u64());
                assert!(entry["count"].is_u64());
            }
        }
        let empty = serde_json::json!({"schema_version":2,
            "dictionary_payload_sha256":null,"dictionary_token_count":null,
            "observation_count":0,"entries":[],"rejected_entries":[]});
        let store = LearningStore::default();
        assert_v2(&store.update(None).unwrap().unwrap().data, empty.clone());
        let generation = store.version().generation;
        assert!(store.observe(
            generation,
            &[DictionaryTokenId::new(2), DictionaryTokenId::new(17)],
            fingerprint()
        ));
        let mut expected = serde_json::json!({"schema_version":2,
            "dictionary_payload_sha256":"5a".repeat(32),"dictionary_token_count":100,
            "observation_count":1,"rejected_entries":[],"entries":[{"dictionary_token_id":2,"count":1},{"dictionary_token_id":17,"count":1}]});
        assert_v2(&store.update(None).unwrap().unwrap().data, expected.clone());
        assert!(store.observe(generation, &[DictionaryTokenId::new(17)], fingerprint()));
        expected["observation_count"] = serde_json::json!(2);
        expected["entries"][1]["count"] = serde_json::json!(2);
        assert_v2(&store.update(None).unwrap().unwrap().data, expected);
        store.clear();
        assert_v2(&store.update(None).unwrap().unwrap().data, empty);
    }

    #[test]
    fn both_observations_add_and_readers_do_not_consume_shared_updates() {
        let store = LearningStore::default();
        let a = store.capture();
        let b = store.capture();
        assert!(store.observe(a.generation, &[DictionaryTokenId::new(1)], fingerprint()));
        let first = store.update(None).unwrap().unwrap();
        assert_eq!(store.update(None).unwrap().unwrap().data, first.data);
        assert!(store.observe(b.generation, &[DictionaryTokenId::new(2)], fingerprint()));
        let update = store.update(Some(first.version)).unwrap().unwrap();
        let snapshot: PersonalFrequencySnapshot = serde_json::from_slice(&update.data).unwrap();
        assert_eq!(snapshot.observation_count, 2);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|e| e.dictionary_token_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn clearing_already_empty_learning_rejects_pre_clear_observations() {
        let store = LearningStore::default();
        let old = store.capture();
        let cleared = store.clear();
        assert_ne!(old.generation, store.capture().generation);
        assert!(!store.observe(old.generation, &[DictionaryTokenId::new(1)], fingerprint()));
        assert!(store.update(Some(cleared)).unwrap().is_none());
        assert!(store.observe(
            store.capture().generation,
            &[DictionaryTokenId::new(2)],
            fingerprint()
        ));
    }

    #[test]
    fn observation_panics_publish_complete_old_or_new_learning() {
        for after in [false, true] {
            let store = LearningStore::default();
            let generation = store.capture().generation;
            assert!(
                catch_unwind(AssertUnwindSafe(|| store.observe_with_hook(
                    generation,
                    &[DictionaryTokenId::new(1)],
                    fingerprint(),
                    |point| {
                        if matches!(point, PublicationPoint::After) == after {
                            panic!("learning publication test");
                        }
                    }
                )))
                .is_err()
            );
            let changed = store.update(Some(LearningVersion::default())).unwrap();
            assert_eq!(changed.is_some(), after);
            assert!(store.observe(generation, &[DictionaryTokenId::new(2)], fingerprint()));
            assert_eq!(
                serde_json::from_slice::<PersonalFrequencySnapshot>(
                    &store.update(None).unwrap().unwrap().data
                )
                .unwrap()
                .observation_count,
                if after { 2 } else { 1 }
            );
        }
    }
}

#[cfg(test)]
mod rejection_generation_tests {
    use super::*;
    #[test]
    fn clear_drops_positive_and_rejected_counts_and_old_generation_observations() {
        let store = LearningStore::default();
        let fp = DictionaryFingerprint::new([0x44; 32], 100);
        let before = store.version();
        assert!(store.observe_guarded(
            before.generation,
            &[DictionaryTokenId::new(1)],
            &[DictionaryTokenId::new(2)],
            fp,
            |_| {},
            || ()
        ));
        let data = store.update(None).unwrap().unwrap().data;
        let model: PersonalFrequencySnapshot = serde_json::from_slice(&data).unwrap();
        assert_eq!(model.entries.len(), 1);
        assert_eq!(model.rejected_entries.len(), 1);
        let version = store.clear();
        assert!(!store.observe_guarded(
            before.generation,
            &[DictionaryTokenId::new(1)],
            &[DictionaryTokenId::new(2)],
            fp,
            |_| {},
            || ()
        ));
        assert_eq!(store.version(), version);
        let data = store.update(None).unwrap().unwrap().data;
        let model: PersonalFrequencySnapshot = serde_json::from_slice(&data).unwrap();
        assert!(model.entries.is_empty());
        assert!(model.rejected_entries.is_empty());
        assert!(store.observe_guarded(
            version.generation,
            &[],
            &[DictionaryTokenId::new(2)],
            fp,
            |_| {},
            || ()
        ));
        let model: PersonalFrequencySnapshot =
            serde_json::from_slice(&store.update(None).unwrap().unwrap().data).unwrap();
        assert_eq!(model.observation_count, 1);
        assert!(model.entries.is_empty());
        assert_eq!(model.rejected_entries[0].count, 1);
    }
}
