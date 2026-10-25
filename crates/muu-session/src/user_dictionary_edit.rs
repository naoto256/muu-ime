//! A private prepared edit spans the host's disk transaction. It never holds
//! the vocabulary lock across that IO (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」), and publication returns only
//! an ACK: a failed response allocation must not undo a committed dictionary.

use muu_conversion::LexiconOverlay;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Mutex, TryLockError,
    atomic::{AtomicU64, Ordering},
};

use crate::user_dictionary_file::UserDictionaryManagement;
use crate::{
    Core, MAX_USER_DICTIONARY_VIEW_BYTES, StoredFileState, UserDictionaryEditCommand,
    UserDictionaryEditError, UserDictionarySnapshot, UserDictionaryView,
};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// Opaque process-local capability; never persisted. A revision alone cannot
/// distinguish an edit prepared by another Core or a cancelled prior edit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDictionaryTicket {
    owner: u64,
    operation: u64,
    expected_revision: u64,
}

struct Pending {
    ticket: UserDictionaryTicket,
    management: Arc<UserDictionaryManagement>,
    source: Arc<UserDictionarySnapshot>,
    overlay: Arc<LexiconOverlay>,
}

// Invalidation cannot wait for this private mutex: a preparer may be waiting
// on the publication lock held by the unwinding invalidator. Check again
// after releasing private ownership, including early returns and panics.
// This closes the interval between the preparer's last flag check and ticket
// registration without putting file IO or main behind either lock (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
struct PrivateEditCleanup<'a>(&'a Core);
impl Drop for PrivateEditCleanup<'_> {
    fn drop(&mut self) {
        if self.0.is_invalid() {
            self.0.discard_idle_user_dictionary_preparation();
        }
    }
}

// At most one prepared edit per Core. `owner` is unique per Core in this
// process, `sequence` per prepared edit, so a ticket from another Core or
// from an edit that was cancelled can never match the pending one.
//
// The host's sequence: prepare (Rust builds the file and plan) -> write the
// file atomically -> publish (ACK only) or cancel. A refused publish keeps the
// reservation until the host cancels it.
pub(crate) struct UserDictionaryEdits {
    owner: u64,
    sequence: u64,
    pending: Option<Pending>,
}

impl Default for UserDictionaryEdits {
    fn default() -> Self {
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            sequence: 0,
            pending: None,
        }
    }
}

/// Both owned outputs are ready before a ticket can reach C/Swift. The FFI
/// adapter must cancel this ticket if making either MuuBuffer fails.
pub struct PreparedUserDictionaryEdit {
    pub file: Vec<u8>,
    pub plan: Vec<u8>,
    pub ticket: UserDictionaryTicket,
}

#[derive(Serialize)]
struct Plan<'a> {
    schema_version: u32,
    ticket: &'a UserDictionaryTicket,
    expected_revision: u64,
    next_revision: u64,
    view: UserDictionaryView,
}

impl Core {
    pub fn user_dictionary_view(&self) -> UserDictionaryView {
        self.vocabulary.user_management().view()
    }

    /// Called only before publishing Core. Codec failure preserves the file
    /// as read-only; layer failure preserves all rows with an empty layer.
    pub fn initialize_user_dictionary_file(&self, state: StoredFileState, bytes: &[u8]) {
        let (management, source, overlay) = UserDictionaryManagement::from_file(state, bytes);
        let expected = self.vocabulary.user_management().revision;
        // No other caller can observe or update this unpublished Core. The
        // ordinary CAS path keeps both pieces in the same publication.
        self.vocabulary
            .publish_user_management(
                expected,
                Arc::new(management),
                Arc::new(source),
                overlay,
                || (),
                |_| {},
            )
            .expect("unpublished Core user dictionary initialization");
    }

    pub fn prepare_user_dictionary(
        &self,
        command: UserDictionaryEditCommand,
    ) -> Result<PreparedUserDictionaryEdit, UserDictionaryEditError> {
        let _cleanup = PrivateEditCleanup(self);
        let mut edits = match self.user_dictionary_edits.try_lock() {
            Ok(edits) => edits,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(UserDictionaryEditError::Busy),
        };
        if edits.pending.is_some() {
            return Err(UserDictionaryEditError::Busy);
        }
        if self.is_invalid() {
            edits.pending = None;
            return Err(UserDictionaryEditError::CoreInvalid);
        }
        let expected_revision = command.expected_revision();
        let current = self.vocabulary.user_management();
        let (management, source, overlay, file) = current.edit(command)?;
        let operation = edits
            .sequence
            .checked_add(1)
            .ok_or(UserDictionaryEditError::RevisionOverflow)?;
        let ticket = UserDictionaryTicket {
            owner: edits.owner,
            operation,
            expected_revision,
        };
        let plan = serde_json::to_vec(&Plan {
            schema_version: 1,
            ticket: &ticket,
            expected_revision,
            next_revision: management.revision,
            view: management.view(),
        })
        .map_err(|_| UserDictionaryEditError::OutputTooLarge)?;
        if plan.len() > MAX_USER_DICTIONARY_VIEW_BYTES {
            return Err(UserDictionaryEditError::OutputTooLarge);
        }
        // Fallible expansion/serialization precede reservation. An unwind
        // there leaves no inaccessible ticket, and never invalidates Core.
        if self.is_invalid() {
            return Err(UserDictionaryEditError::CoreInvalid);
        }
        edits.sequence = operation;
        edits.pending = Some(Pending {
            ticket: ticket.clone(),
            management: Arc::new(management),
            source: Arc::new(source),
            overlay,
        });
        Ok(PreparedUserDictionaryEdit { file, plan, ticket })
    }

    pub fn publish_user_dictionary(
        &self,
        ticket: &UserDictionaryTicket,
    ) -> Result<(), UserDictionaryEditError> {
        self.publish_user_dictionary_with_hook(ticket, |_| {})
    }

    fn publish_user_dictionary_with_hook(
        &self,
        ticket: &UserDictionaryTicket,
        hook: impl Fn(bool),
    ) -> Result<(), UserDictionaryEditError> {
        let _cleanup = PrivateEditCleanup(self);
        let mut edits = self
            .user_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.is_invalid() {
            edits.pending = None;
            return Err(UserDictionaryEditError::CoreInvalid);
        }
        if !edits.pending.as_ref().is_some_and(|p| &p.ticket == ticket) {
            return Err(UserDictionaryEditError::InvalidTicket);
        }
        // Remove private ownership before entering publication. An unwind
        // drops this local, so an invalid Core cannot retain a live ticket.
        let pending = edits
            .pending
            .take()
            .ok_or(UserDictionaryEditError::InvalidTicket)?;
        let result = self.vocabulary.publish_user_management(
            ticket.expected_revision,
            pending.management.clone(),
            pending.source.clone(),
            pending.overlay.clone(),
            || self.publication_guard(),
            hook,
        );
        // A refused publication (revision mismatch) keeps the ticket
        // reserved: the host still owns it and must cancel it after restoring
        // its file. Until then the next prepare reports Busy, and nothing
        // expires the reservation on its own.
        if result.is_err() {
            edits.pending = Some(pending);
        }
        result
    }

    /// Cancellation releases only private work, including after Core becomes
    /// invalid. It is idempotent and cannot roll back an already published edit.
    pub fn cancel_user_dictionary(&self, ticket: &UserDictionaryTicket) {
        let _cleanup = PrivateEditCleanup(self);
        let mut edits = self
            .user_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if edits.pending.as_ref().is_some_and(|p| &p.ticket == ticket) {
            edits.pending = None;
        }
    }

    pub(crate) fn discard_idle_user_dictionary_preparation(&self) {
        match self.user_dictionary_edits.try_lock() {
            Ok(mut edits) => edits.pending = None,
            Err(TryLockError::Poisoned(error)) => error.into_inner().pending = None,
            Err(TryLockError::WouldBlock) => {}
        }
    }

    pub fn discard_user_dictionary_preparation(&self) {
        // A terminal-status read can arrive on main. It must not wait for
        // another thread's private expansion/serialization; an in-flight
        // preparer checks the invalid flag before reserving its ticket.
        self.discard_idle_user_dictionary_preparation();
    }
}

// The mutex is private transaction ownership, not a publication lock. Its
// poison can be recovered after a preparation panic without invalidating Core.
pub(crate) fn new_user_dictionary_edits() -> Mutex<UserDictionaryEdits> {
    Mutex::new(UserDictionaryEdits::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CoreConfig, UserDictionaryPartOfSpeech, UserDictionaryPriority, UserDictionaryRecord,
    };
    fn core() -> Core {
        Core::new(CoreConfig {
            dictionary_path: None,
        })
        .unwrap()
    }
    fn upsert(revision: u64, surface: &str) -> UserDictionaryEditCommand {
        UserDictionaryEditCommand::Upsert {
            expected_revision: revision,
            entry_id: None,
            reading: "むう".into(),
            surface: surface.into(),
            part_of_speech: UserDictionaryPartOfSpeech::noun(),
            priority: UserDictionaryPriority::High,
        }
    }
    #[test]
    fn preparation_is_private_busy_and_cancel_is_idempotent() {
        let core = core();
        let prepared = core.prepare_user_dictionary(upsert(0, "Muu")).unwrap();
        assert_eq!(core.user_dictionary_view().entry_count, Some(0));
        assert!(matches!(
            core.prepare_user_dictionary(upsert(0, "Other")),
            Err(UserDictionaryEditError::Busy)
        ));
        core.cancel_user_dictionary(&prepared.ticket);
        core.cancel_user_dictionary(&prepared.ticket);
        let prepared = core.prepare_user_dictionary(upsert(0, "Other")).unwrap();
        core.publish_user_dictionary(&prepared.ticket).unwrap();
        assert_eq!(core.user_dictionary_view().management_revision, 1);
        assert_eq!(
            core.user_dictionary_view().entries.unwrap()[0]
                .record
                .surface,
            "Other"
        );
        core.cancel_user_dictionary(&prepared.ticket);
        assert!(matches!(
            core.publish_user_dictionary(&prepared.ticket),
            Err(UserDictionaryEditError::InvalidTicket)
        ));
    }
    #[test]
    fn management_and_layer_publish_together_and_preserve_captured_snapshots() {
        let core = core();
        let old = core.vocabulary.snapshot();
        let prepared = core.prepare_user_dictionary(upsert(0, "Muu")).unwrap();
        core.publish_user_dictionary(&prepared.ticket).unwrap();
        assert_eq!(old.user.revision, 0);
        assert_eq!(core.vocabulary.snapshot().user.revision, 1);
        let prepared = core.prepare_user_dictionary(upsert(1, "Muu")).unwrap();
        core.publish_user_dictionary(&prepared.ticket).unwrap();
        assert_eq!(core.user_dictionary_view().management_revision, 2);
        assert_eq!(core.vocabulary.snapshot().user.revision, 1);
    }
    #[test]
    fn degraded_content_accepts_only_deletions_until_whole_layer_is_valid() {
        let core = core();
        let snapshot = UserDictionarySnapshot {
            schema_version: 1,
            entries: (0..4098)
                .map(|i| UserDictionaryRecord {
                    reading: "むう".into(),
                    surface: format!("語{i:04}"),
                    part_of_speech: UserDictionaryPartOfSpeech::noun(),
                    priority: UserDictionaryPriority::High,
                })
                .collect(),
        };
        core.initialize_user_dictionary_file(
            StoredFileState::Present,
            &crate::encode_user_dictionary_file(&snapshot).unwrap(),
        );
        assert_eq!(core.user_dictionary_view().entry_count, Some(4098));
        assert_eq!(core.vocabulary.snapshot().user.revision, 0);
        assert!(matches!(
            core.prepare_user_dictionary(upsert(0, "Muu")),
            Err(UserDictionaryEditError::DeletionRequired)
        ));
        for revision in 0..2 {
            let prepared = core
                .prepare_user_dictionary(UserDictionaryEditCommand::Remove {
                    expected_revision: revision,
                    entry_id: format!("{revision}:0"),
                })
                .unwrap();
            core.publish_user_dictionary(&prepared.ticket).unwrap();
            assert_eq!(
                core.user_dictionary_view().entry_count,
                Some(4097 - revision as usize)
            );
            assert_eq!(
                core.user_dictionary_view().layer_status,
                if revision == 0 {
                    "empty_degraded"
                } else {
                    "active"
                }
            );
        }
        assert_eq!(core.vocabulary.snapshot().user.revision, 1);
    }
    #[test]
    fn only_publication_panics_invalidate_core_and_leave_complete_states() {
        for after in [false, true] {
            let core = core();
            let prepared = core.prepare_user_dictionary(upsert(0, "Muu")).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                core.publish_user_dictionary_with_hook(&prepared.ticket, |point| {
                    if point == after {
                        panic!("publication test");
                    }
                })
            }));
            assert!(result.is_err());
            assert!(core.is_invalid());
            assert_eq!(
                core.user_dictionary_view().entry_count,
                Some(usize::from(after))
            );
            assert_eq!(core.vocabulary.snapshot().user.revision, u64::from(after));
            core.cancel_user_dictionary(&prepared.ticket);
        }
    }

    #[test]
    fn legacy_ha_base_form_remains_manageable_but_is_not_adopted_by_the_runtime() {
        let entry = UserDictionaryRecord {
            reading: "むうかう".into(),
            surface: "語かふ".into(),
            part_of_speech: UserDictionaryPartOfSpeech::from_id("HA_GROUP1_VERB").unwrap(),
            priority: UserDictionaryPriority::High,
        };
        let snapshot = UserDictionarySnapshot {
            schema_version: 1,
            entries: vec![entry.clone()],
        };
        let bytes = crate::encode_user_dictionary_file(&snapshot).unwrap();
        // Old Swift accepted this exact file, but the old runtime refused its
        // whole layer. Preserve that boundary rather than changing the accepted
        // POS definitions.
        assert_eq!(
            snapshot.clone().into_overlay(),
            Err(crate::UserDictionaryError::InvalidBaseForm)
        );
        let core = core();
        core.initialize_user_dictionary_file(StoredFileState::Present, &bytes);
        assert_eq!(core.user_dictionary_view().entry_count, Some(1));
        assert_eq!(core.user_dictionary_view().layer_status, "empty_degraded");
        assert_eq!(core.vocabulary.snapshot().user.revision, 0);
        let normal = super::tests::core();
        assert!(matches!(
            normal.prepare_user_dictionary(UserDictionaryEditCommand::Upsert {
                expected_revision: 0,
                entry_id: None,
                reading: entry.reading,
                surface: entry.surface,
                part_of_speech: entry.part_of_speech,
                priority: entry.priority,
            }),
            Err(UserDictionaryEditError::LayerInvalid)
        ));
        let prepared = core
            .prepare_user_dictionary(UserDictionaryEditCommand::Remove {
                expected_revision: 0,
                entry_id: "0:0".into(),
            })
            .unwrap();
        core.publish_user_dictionary(&prepared.ticket).unwrap();
        assert_eq!(core.user_dictionary_view().layer_status, "active");
    }

    #[test]
    fn invalidation_during_private_work_releases_the_ticket_after_unlocking() {
        let core = core();
        core.prepare_user_dictionary(upsert(0, "Muu")).unwrap();
        {
            let _cleanup = PrivateEditCleanup(&core);
            let edits = core.user_dictionary_edits.lock().unwrap();
            // Invalidation returns while private ownership is held. The
            // cleanup must run after this guard, not recursively lock it.
            core.invalidate();
            assert!(edits.pending.is_some());
        }
        assert!(core.user_dictionary_edits.lock().unwrap().pending.is_none());
    }
}
