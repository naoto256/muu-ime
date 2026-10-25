//! Complete overlay catalogs published under one short exclusion interval.
//!
//! The User, Imported and Postal layers are published together as one
//! immutable `VocabularySnapshot` (docs/src/internals/architecture.md「Core と Session」). A conversion
//! therefore always sees three layers that were current at the same instant.
//! Replacing a layer builds that one layer without any lock, then enters the
//! lock shared by all three layers only to combine it with whatever the other
//! two layers are at that moment and swap the result in.

use std::sync::{Arc, Mutex};

use muu_conversion::LexiconOverlay;

use crate::imported_dictionary_management::{
    ImportedDictionaryEditError, ImportedDictionaryPublication,
};
use crate::postal_code_edit::{PostalCodeEditError, PostalCodeManagement};
use crate::user_dictionary_file::{UserDictionaryEditError, UserDictionaryManagement};

use crate::UserDictionarySnapshot;

/// One published layer. `revision` counts publications of this layer only.
pub struct VocabularyLayer<S> {
    // Only User needs active source rows. Other layers retain their identity
    // in management and use unit here, without compatibility snapshots.
    source: S,
    pub overlay: Arc<LexiconOverlay>,
    pub revision: u64,
}

impl<S: Default> Default for VocabularyLayer<S> {
    fn default() -> Self {
        Self {
            source: S::default(),
            overlay: Arc::new(LexiconOverlay::default()),
            revision: 0,
        }
    }
}

/// The three layers as they were published together. Cloning copies three
/// reference counts.
#[derive(Clone, Default)]
pub struct VocabularySnapshot {
    pub user: Arc<VocabularyLayer<Arc<UserDictionarySnapshot>>>,
    pub imported: Arc<VocabularyLayer<()>>,
    pub postal: Arc<VocabularyLayer<()>>,
}

/// The process-wide vocabulary store. Publications may come from any thread;
/// `snapshot` is what a conversion captures.
#[derive(Clone, Default)]
pub struct SharedVocabulary {
    state: Arc<Mutex<PublishedVocabulary>>,
}

// The management state and active User layer form one publication (docs/src/internals/storage.md「壊れたファイルは上書きしない」).
// ConversionSnapshot retains only layers, never UI-only management rows.
//
// Two kinds of revision live here. A layer's `revision` counts changes to
// what conversion sees; a management `revision` counts accepted edits and is
// what an edit's `expected_revision` is checked against. Every production
// change to a layer goes through its management revision, which is why each
// publish below can read the layer before taking the lock and only recheck
// the management revision under it.
#[derive(Default)]
struct PublishedVocabulary {
    layers: Arc<VocabularySnapshot>,
    user: Arc<UserDictionaryManagement>,
    imported: Arc<ImportedDictionaryPublication>,
    postal: Arc<PostalCodeManagement>,
}

// A slot describes only its typed management/layer projection. Content
// comparison, planning, rollback and disable remain in their domain wrappers.
//
// Shared, in `publish_slot`: the part that is the same for all three and must
// stay one guarded interval. Under the state lock: the final revision check,
// entering the publication guard, rebuilding from the current OTHER layers,
// and a single swap.
//
// Deliberately not shared: everything before the lock that differs by
// dictionary and is part of each one's contract.
// - What "unchanged" means: User compares its active source snapshot, which
//   can differ from its management snapshot when the layer is invalid;
//   Imported and Postal compare `source_identity`.
// - Error precedence: User plans its layer revision first, so an exhausted
//   revision reports RevisionOverflow even for a stale `expected`; Imported
//   and Postal check the management revision first, so a stale `expected`
//   reports RevisionMismatch and only a current one can overflow.
// - Rollback and Postal's disable, which publish through the same interval
//   but plan their own layers.
// Folding these into the trait would turn each difference into a parameter
// and risk changing one dictionary's observable behavior.
trait PublicationSlot {
    type Source;
    type Management;
    type Error;
    fn revision(current: &PublishedVocabulary) -> u64;
    fn mismatch() -> Self::Error;
    fn combine(
        current: &PublishedVocabulary,
        layers: VocabularySnapshot,
        management: Arc<Self::Management>,
        layer: Arc<VocabularyLayer<Self::Source>>,
    ) -> PublishedVocabulary;
}

struct UserSlot;
impl PublicationSlot for UserSlot {
    type Source = Arc<UserDictionarySnapshot>;
    type Management = UserDictionaryManagement;
    type Error = UserDictionaryEditError;
    fn revision(current: &PublishedVocabulary) -> u64 {
        current.user.revision
    }
    fn mismatch() -> Self::Error {
        UserDictionaryEditError::RevisionMismatch
    }
    fn combine(
        current: &PublishedVocabulary,
        mut layers: VocabularySnapshot,
        management: Arc<Self::Management>,
        layer: Arc<VocabularyLayer<Self::Source>>,
    ) -> PublishedVocabulary {
        layers.user = layer;
        PublishedVocabulary {
            layers: Arc::new(layers),
            user: management,
            imported: current.imported.clone(),
            postal: current.postal.clone(),
        }
    }
}

struct ImportedSlot;
impl PublicationSlot for ImportedSlot {
    type Source = ();
    type Management = ImportedDictionaryPublication;
    type Error = ImportedDictionaryEditError;
    fn revision(current: &PublishedVocabulary) -> u64 {
        current.imported.management.revision
    }
    fn mismatch() -> Self::Error {
        ImportedDictionaryEditError::RevisionMismatch
    }
    fn combine(
        current: &PublishedVocabulary,
        mut layers: VocabularySnapshot,
        management: Arc<Self::Management>,
        layer: Arc<VocabularyLayer<Self::Source>>,
    ) -> PublishedVocabulary {
        layers.imported = layer;
        PublishedVocabulary {
            layers: Arc::new(layers),
            user: current.user.clone(),
            imported: management,
            postal: current.postal.clone(),
        }
    }
}

struct PostalSlot;
impl PublicationSlot for PostalSlot {
    type Source = ();
    type Management = PostalCodeManagement;
    type Error = PostalCodeEditError;
    fn revision(current: &PublishedVocabulary) -> u64 {
        current.postal.revision
    }
    fn mismatch() -> Self::Error {
        PostalCodeEditError::RevisionMismatch
    }
    fn combine(
        current: &PublishedVocabulary,
        mut layers: VocabularySnapshot,
        management: Arc<Self::Management>,
        layer: Arc<VocabularyLayer<Self::Source>>,
    ) -> PublishedVocabulary {
        layers.postal = layer;
        PublishedVocabulary {
            layers: Arc::new(layers),
            user: current.user.clone(),
            imported: current.imported.clone(),
            postal: management,
        }
    }
}

impl SharedVocabulary {
    // This is the whole guarded publication interval. It reads OTHER layers
    // from the final locked state, not the snapshot used for preparation.
    fn publish_slot<S: PublicationSlot, G>(
        &self,
        expected: u64,
        management: Arc<S::Management>,
        layer: Arc<VocabularyLayer<S::Source>>,
        guard: impl FnOnce() -> G,
        hook: impl Fn(bool),
    ) -> Result<(), S::Error> {
        let mut current = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if S::revision(&current) != expected {
            return Err(S::mismatch());
        }
        let _publication = guard();
        let next = S::combine(&current, current.layers.as_ref().clone(), management, layer);
        hook(false);
        *current = next;
        hook(true);
        Ok(())
    }

    pub fn snapshot(&self) -> Arc<VocabularySnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .layers
            .clone()
    }

    pub(crate) fn user_management(&self) -> Arc<UserDictionaryManagement> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .user
            .clone()
    }

    pub fn imported_snapshot(&self) -> (u64, Arc<LexiconOverlay>) {
        let snapshot = self.snapshot();
        (
            snapshot.imported.revision,
            snapshot.imported.overlay.clone(),
        )
    }
    pub fn postal_snapshot(&self) -> (u64, Arc<LexiconOverlay>) {
        let snapshot = self.snapshot();
        (snapshot.postal.revision, snapshot.postal.overlay.clone())
    }
    #[cfg(test)]
    pub(crate) fn with_postal_snapshot<R>(
        &self,
        operation: impl FnOnce(u64, &LexiconOverlay) -> R,
    ) -> R {
        let (revision, overlay) = self.postal_snapshot();
        operation(revision, &overlay)
    }

    /// No file IO, validation or response construction belongs in this
    /// interval. A successful return is the only ACK needed after rename;
    /// the caller already owns the prepared next view (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
    ///
    /// Every live User update advances management revision; the final check
    /// therefore rejects a layer changed since the snapshot was captured.
    pub(crate) fn publish_user_management<G>(
        &self,
        expected: u64,
        management: Arc<UserDictionaryManagement>,
        source: Arc<UserDictionarySnapshot>,
        overlay: Arc<LexiconOverlay>,
        publication_guard: impl FnOnce() -> G,
        hook: impl Fn(bool),
    ) -> Result<(), UserDictionaryEditError> {
        let observed = self.snapshot();
        let same = observed.user.source == source;
        let revision = if same {
            observed.user.revision
        } else {
            observed
                .user
                .revision
                .checked_add(1)
                .ok_or(UserDictionaryEditError::RevisionOverflow)?
        };
        let layer = if same {
            observed.user.clone()
        } else {
            Arc::new(VocabularyLayer {
                source,
                overlay,
                revision,
            })
        };
        self.publish_slot::<UserSlot, _>(expected, management, layer, publication_guard, hook)
    }
    pub(crate) fn imported_management(&self) -> Arc<ImportedDictionaryPublication> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .imported
            .clone()
    }

    pub(crate) fn imported_authorities(
        &self,
    ) -> (Arc<ImportedDictionaryPublication>, Arc<VocabularyLayer<()>>) {
        // Recovery must retain the overlay from the SAME publication as the
        // management revision. Two reads could straddle a live publication.
        let current = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (current.imported.clone(), current.layers.imported.clone())
    }

    /// Capture summaries and the active layer in one publication. Preparation
    /// has already built the overlay. The current OTHER layers are retained,
    /// including when this is a rollback after a pointer-write failure.
    pub(crate) fn publish_imported_management<G>(
        &self,
        expected: u64,
        management: Arc<ImportedDictionaryPublication>,
        overlay: Arc<LexiconOverlay>,
        publication_guard: impl FnOnce() -> G,
        hook: impl Fn(bool),
    ) -> Result<u64, ImportedDictionaryEditError> {
        let (previous, old_layer) = {
            let current = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (current.imported.clone(), current.layers.imported.clone())
        };
        if previous.management.revision != expected {
            return Err(ImportedDictionaryEditError::RevisionMismatch);
        }
        let same = previous.source_identity == management.source_identity;
        let revision = if same {
            old_layer.revision
        } else {
            old_layer
                .revision
                .checked_add(1)
                .ok_or(ImportedDictionaryEditError::RevisionOverflow)?
        };
        let layer = Arc::new(VocabularyLayer {
            source: (),
            overlay: if same {
                old_layer.overlay.clone()
            } else {
                overlay
            },
            revision,
        });
        self.publish_slot::<ImportedSlot, _>(expected, management, layer, publication_guard, hook)?;
        Ok(revision)
    }
}

impl SharedVocabulary {
    pub(crate) fn postal_management(&self) -> Arc<PostalCodeManagement> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .postal
            .clone()
    }
    pub(crate) fn postal_authorities(
        &self,
    ) -> (Arc<PostalCodeManagement>, Arc<VocabularyLayer<()>>) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (s.postal.clone(), s.layers.postal.clone())
    }
    /// Postal management and its active overlay are one atomic authority.
    /// Take the CURRENT other layers under the lock, including for rollback
    /// and Postal-only disable. No IO, codec or allocating reply follows ACK.
    pub(crate) fn publish_postal_management<G>(
        &self,
        expected: u64,
        management: Arc<PostalCodeManagement>,
        overlay: Arc<LexiconOverlay>,
        guard: impl FnOnce() -> G,
        hook: impl Fn(bool),
    ) -> Result<u64, PostalCodeEditError> {
        let (old, layer) = self.postal_authorities();
        if old.revision != expected {
            return Err(PostalCodeEditError::RevisionMismatch);
        }
        let same = old.source_identity == management.source_identity;
        let revision = if same {
            layer.revision
        } else {
            layer
                .revision
                .checked_add(1)
                .ok_or(PostalCodeEditError::RevisionOverflow)?
        };
        let layer = Arc::new(VocabularyLayer {
            source: (),
            overlay: if same { layer.overlay.clone() } else { overlay },
            revision,
        });
        self.publish_slot::<PostalSlot, _>(expected, management, layer, guard, hook)?;
        Ok(revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imported_dictionary_management::{
        ImportedDictionaryManagement, build_publication, summarize_document,
    };
    use crate::{
        ImportedDictionaryDocument, ImportedDictionaryManifestEntry, ImportedDictionaryMetadata,
        ImportedDictionaryRecord, UserDictionaryPartOfSpeech, UserDictionaryPriority,
        UserDictionaryRecord,
    };
    use std::collections::BTreeMap;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn user() -> UserDictionarySnapshot {
        UserDictionarySnapshot {
            schema_version: 1,
            entries: vec![UserDictionaryRecord {
                reading: "むう".into(),
                surface: "MuuUser".into(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            }],
        }
    }
    fn user_publication(store: &SharedVocabulary, hook: impl Fn(bool)) {
        let revision = store.user_management().revision;
        let (management, source, overlay) =
            UserDictionaryManagement::from_snapshot(user(), revision + 1);
        store
            .publish_user_management(
                revision,
                Arc::new(management),
                Arc::new(source),
                overlay,
                || (),
                hook,
            )
            .unwrap();
    }
    fn imported() -> (Arc<ImportedDictionaryPublication>, Arc<LexiconOverlay>) {
        let metadata = ImportedDictionaryMetadata {
            id: "test".into(),
            name: "Fixture".into(),
            version: "1".into(),
            author: "Fixture".into(),
            license: "CC0".into(),
            provenance: "Self-authored".into(),
        };
        let document = ImportedDictionaryDocument {
            metadata: metadata.clone(),
            entries: vec![ImportedDictionaryRecord {
                reading: "むう".into(),
                surface: "MuuImported".into(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority: UserDictionaryPriority::High,
            }],
        };
        let (summary, part, _) = summarize_document(
            document,
            ImportedDictionaryManifestEntry {
                metadata,
                enabled: true,
                imported_at_unix_ms: 1,
                source_sha256: "0".repeat(64),
                blob_sha256: "0".repeat(64),
                order: 0,
            },
        )
        .unwrap();
        let management = ImportedDictionaryManagement {
            revision: 1,
            initialized: true,
            dictionaries: Some(vec![summary]),
            ..ImportedDictionaryManagement::default()
        };
        build_publication(management, BTreeMap::from([("test".into(), part)])).unwrap()
    }

    #[test]
    fn publishing_independently_prepared_layers_preserves_both_orders() {
        for user_first in [false, true] {
            let store = SharedVocabulary::default();
            let (management, overlay) = imported();
            let expected_imported = overlay.clone();
            if user_first {
                user_publication(&store, |_| {});
            }
            store
                .publish_imported_management(0, management, overlay, || (), |_| {})
                .unwrap();
            if !user_first {
                user_publication(&store, |_| {});
            }
            let captured = store.snapshot();
            assert_eq!(captured.user.revision, 1);
            assert_eq!(captured.imported.revision, 1);
            assert_eq!(*captured.user.source, user());
            assert!(Arc::ptr_eq(&captured.imported.overlay, &expected_imported));
            assert_eq!(captured.postal.revision, 0);
        }
    }

    #[test]
    fn identical_publication_keeps_the_runtime_layer_and_revision() {
        let store = SharedVocabulary::default();
        user_publication(&store, |_| {});
        let first = store.snapshot().user.clone();
        user_publication(&store, |_| {});
        assert!(Arc::ptr_eq(&first, &store.snapshot().user));
        assert_eq!(store.user_management().revision, 2);
        let (management, overlay) = imported();
        store
            .publish_imported_management(0, management.clone(), overlay, || (), |_| {})
            .unwrap();
        let old = store.snapshot().imported.clone();
        let mut next = management.as_ref().clone();
        next.management.revision += 1;
        store
            .publish_imported_management(
                1,
                Arc::new(next),
                Arc::new(LexiconOverlay::default()),
                || (),
                |_| {},
            )
            .unwrap();
        let new = store.snapshot().imported.clone();
        assert_eq!(new.revision, old.revision);
        assert!(Arc::ptr_eq(&old.overlay, &new.overlay));
    }

    #[test]
    fn publication_panics_leave_complete_old_or_new_catalogs() {
        for after in [false, true] {
            let store = SharedVocabulary::default();
            let (management, overlay) = imported();
            store
                .publish_imported_management(0, management, overlay.clone(), || (), |_| {})
                .unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                user_publication(&store, |published| {
                    if published == after {
                        panic!("publication fixture");
                    }
                })
            }));
            assert!(result.is_err());
            let captured = store.snapshot();
            assert_eq!(captured.imported.revision, 1);
            assert!(Arc::ptr_eq(&captured.imported.overlay, &overlay));
            assert_eq!(
                *captured.user.source,
                if after {
                    user()
                } else {
                    UserDictionarySnapshot::default()
                }
            );
            assert_eq!(captured.user.revision, u64::from(after));
        }
    }
    // With every layer revision at u64::MAX, a current and a stale `expected`
    // give: User, overflow in both cases; Imported and Postal, overflow when
    // current and mismatch when stale. No case reaches the swap hook, and no
    // management revision moves.
    #[test]
    fn stale_and_overflow_precedence_preserves_each_dictionary_contract() {
        let store = SharedVocabulary::default();
        {
            let mut state = store.state.lock().unwrap();
            let mut layers = state.layers.as_ref().clone();
            layers.user = Arc::new(VocabularyLayer {
                source: layers.user.source.clone(),
                overlay: layers.user.overlay.clone(),
                revision: u64::MAX,
            });
            layers.imported = Arc::new(VocabularyLayer {
                source: (),
                overlay: layers.imported.overlay.clone(),
                revision: u64::MAX,
            });
            layers.postal = Arc::new(VocabularyLayer {
                source: (),
                overlay: layers.postal.overlay.clone(),
                revision: u64::MAX,
            });
            state.layers = Arc::new(layers);
        }
        for expected in [0, 1] {
            // User plans its layer before the final mismatch check; the other
            // domains also check management revision before planning.
            let result = store.publish_user_management(
                expected,
                Arc::new(UserDictionaryManagement::default()),
                Arc::new(user()),
                Arc::default(),
                || (),
                |_| panic!("rejected publication entered guard"),
            );
            assert!(matches!(
                result,
                Err(UserDictionaryEditError::RevisionOverflow)
            ));
            let imported = ImportedDictionaryPublication {
                source_identity: "different fixture".into(),
                ..Default::default()
            };
            let result = store.publish_imported_management(
                expected,
                Arc::new(imported),
                Arc::default(),
                || (),
                |_| panic!("rejected publication entered guard"),
            );
            assert!(
                matches!(result, Err(ImportedDictionaryEditError::RevisionOverflow))
                    && expected == 0
                    || matches!(result, Err(ImportedDictionaryEditError::RevisionMismatch))
                        && expected == 1
            );
            let postal = PostalCodeManagement {
                source_identity: "different fixture".into(),
                ..Default::default()
            };
            let result = store.publish_postal_management(
                expected,
                Arc::new(postal),
                Arc::default(),
                || (),
                |_| panic!("rejected publication entered guard"),
            );
            assert!(
                matches!(result, Err(PostalCodeEditError::RevisionOverflow)) && expected == 0
                    || matches!(result, Err(PostalCodeEditError::RevisionMismatch))
                        && expected == 1
            );
        }
        let current = store.state.lock().unwrap();
        assert_eq!(current.user.revision, 0);
        assert_eq!(current.imported.management.revision, 0);
        assert_eq!(current.postal.revision, 0);
    }
}
