//! Postal named-file transaction. Management owns only a manifest and status;
//! large source bytes exist only while preparing a reply, never while idle.
//! The host owns all IO. Published recovery authority remains reserved until
//! finish, GC and UI completion, then cancel releases it (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
use crate::Core;
use crate::StoredFileState;
use crate::postal_code::PostalCodeSnapshot;
use crate::postal_code_file::*;
use muu_conversion::LexiconOverlay;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostalCodeEditError {
    Busy,
    RevisionMismatch,
    RevisionOverflow,
    InvalidCommand,
    InvalidTicket,
    CoreInvalid,
    File(PostalCodeFileError),
}
impl PostalCodeEditError {
    pub fn class(&self) -> crate::EditErrorClass {
        use crate::EditErrorClass;
        match self {
            Self::Busy => EditErrorClass::Busy,
            Self::RevisionMismatch => EditErrorClass::RevisionMismatch,
            Self::CoreInvalid => EditErrorClass::CoreInvalid,
            Self::RevisionOverflow => EditErrorClass::Internal,
            Self::InvalidCommand | Self::InvalidTicket | Self::File(_) => EditErrorClass::Rejected,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::RevisionMismatch => "revision_mismatch",
            Self::RevisionOverflow => "revision_overflow",
            Self::InvalidCommand => "invalid_postal_code_command",
            Self::InvalidTicket => "invalid_postal_code_ticket",
            Self::CoreInvalid => "core_invalid",
            Self::File(e) => e.0,
        }
    }
}
impl From<PostalCodeFileError> for PostalCodeEditError {
    fn from(e: PostalCodeFileError) -> Self {
        Self::File(e)
    }
}
type Error = PostalCodeEditError;

#[derive(Clone, Debug, Serialize)]
pub struct PostalCodeView {
    pub schema_version: u32,
    pub management_revision: u64,
    pub access: &'static str,
    pub layer_status: &'static str,
    pub reason: Option<&'static str>,
    pub manifest: Option<PostalCodeManifest>,
    pub generation: Option<String>,
}
#[derive(Clone)]
pub(crate) struct PostalCodeManagement {
    pub source_identity: String,
    pub revision: u64,
    pub manifest: Option<PostalCodeManifest>,
    pub generation: Option<String>,
    pub file_error: Option<&'static str>,
    pub layer_error: Option<&'static str>,
}
// Stable logical identity preserves identical-update behavior without
// retaining all TSV rows in management. Hashing streams JSON
// into SHA rather than allocating another whole snapshot buffer.
// PostalCodeSnapshot's serde shape defines this identity.
pub(crate) fn snapshot_identity(snapshot: &PostalCodeSnapshot) -> String {
    muu_digest::json_sha256(snapshot).expect("postal hash writer cannot fail")
}
impl Default for PostalCodeManagement {
    fn default() -> Self {
        Self {
            source_identity: snapshot_identity(&PostalCodeSnapshot::default()),
            revision: 0,
            manifest: None,
            generation: None,
            file_error: None,
            layer_error: None,
        }
    }
}
impl PostalCodeManagement {
    pub fn view(&self) -> PostalCodeView {
        PostalCodeView {
            schema_version: 1,
            management_revision: self.revision,
            access: if self.file_error.is_some() {
                "read_only"
            } else {
                "editable"
            },
            layer_status: if self.layer_error.is_some() {
                "empty_degraded"
            } else if self.manifest.is_some() {
                "available"
            } else {
                "empty"
            },
            reason: self.file_error.or(self.layer_error),
            manifest: self.manifest.clone(),
            generation: self.generation.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostalCodeTicket {
    pub domain: String,
    pub owner: u64,
    pub operation: u64,
    pub expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostalCodeLoadCommand {
    pub expected_revision: u64,
    pub current_state: StoredFileState,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostalCodeSupplyCommand {
    pub ticket: PostalCodeTicket,
    pub request_id: u64,
    pub file_state: StoredFileState,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostalCodePrepareCommand {
    pub expected_revision: u64,
    pub source: String,
    pub retrieved_unix_seconds: i64,
    pub last_modified: Option<String>,
}
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostalCodeFinishKind {
    Loaded,
    NewPointerDurable,
    NewPointerWarning,
    RolledBack,
    Disabled,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostalCodeFinishCommand {
    pub ticket: PostalCodeTicket,
    pub outcome: PostalCodeFinishKind,
    pub revision: u64,
    pub generation: Option<String>,
}

pub struct PreparedPostalCodeReply {
    pub tsv: Vec<u8>,
    pub manifest: Vec<u8>,
    pub plan: Vec<u8>,
    pub ticket: PostalCodeTicket,
}
// A load asks for two files in turn: the manifest, then its TSV. From the
// stored generation when a pointer exists, else from the bundled seed, which
// is then installed as the first generation.
#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RequestKind {
    GenerationManifest,
    GenerationTsv,
    SeedManifest,
    SeedTsv,
}
#[derive(Serialize)]
struct Request {
    request_id: u64,
    kind: RequestKind,
    generation: Option<String>,
}
struct Work {
    old: Arc<PostalCodeManagement>,
    old_overlay: Arc<LexiconOverlay>,
    generation: Option<String>,
    manifest: Option<Vec<u8>>,
    request: Request,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Loaded,
    InstallSeed,
    Update,
}
struct Ready {
    kind: Kind,
    next: Arc<PostalCodeManagement>,
    rollback: Arc<PostalCodeManagement>,
    disabled: Arc<PostalCodeManagement>,
    new_overlay: Arc<LexiconOverlay>,
    old_overlay: Arc<LexiconOverlay>,
    empty_overlay: Arc<LexiconOverlay>,
    // File bytes are moved to the reply before publishing; no raw blob is
    // retained through a Published or idle management state.
}
// Unlike imported dictionary edits, Core publishes before the host writes
// anything: Ready -> Published happens first, then the host writes the generation
// and pointer.
// From Published the host either finishes (pointer written), rolls back (old
// content at r+2) or, as a last resort, disables (old management, empty
// layer, also at r+2). Cancel is refused on a live Core while any of those
// three outcomes is still unfinished, so the reservation cannot be released
// while the live state and the disk pointer may disagree.
enum Phase {
    Working(Work),
    Ready(Ready),
    Published(Ready),
    RolledBack(Ready),
    Disabled(Ready),
    Resolved { _retained: Ready },
}
struct Pending {
    ticket: PostalCodeTicket,
    phase: Phase,
}
pub(crate) struct PostalCodeEdits {
    owner: u64,
    sequence: u64,
    pending: Option<Pending>,
}
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
impl Default for PostalCodeEdits {
    fn default() -> Self {
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            sequence: 0,
            pending: None,
        }
    }
}
impl PostalCodeEdits {
    fn ticket(&mut self, revision: u64) -> Result<PostalCodeTicket, Error> {
        if self.pending.is_some() {
            return Err(Error::Busy);
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(Error::RevisionOverflow)?;
        Ok(PostalCodeTicket {
            domain: "postal_code".into(),
            owner: self.owner,
            operation: self.sequence,
            expected_revision: revision,
        })
    }
    fn matching(&mut self, t: &PostalCodeTicket) -> Result<&mut Pending, Error> {
        self.pending
            .as_mut()
            .filter(|p| p.ticket == *t)
            .ok_or(Error::InvalidTicket)
    }
}
struct Cleanup<'a>(&'a Core);
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() || self.0.is_invalid() {
            let mut edits = match self.0.postal_code_edits.try_lock() {
                Ok(e) => e,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(_) => return,
            };
            if self.0.is_invalid()
                || edits
                    .pending
                    .as_ref()
                    .is_some_and(|p| matches!(p.phase, Phase::Working(_) | Phase::Ready(_)))
            {
                edits.pending = None;
            }
        }
    }
}
fn revisions(revision: u64) -> Result<(u64, u64), Error> {
    Ok((
        revision.checked_add(1).ok_or(Error::RevisionOverflow)?,
        revision.checked_add(2).ok_or(Error::RevisionOverflow)?,
    ))
}
fn readonly(
    old: &PostalCodeManagement,
    reason: &'static str,
    generation: Option<String>,
) -> Result<PostalCodeManagement, Error> {
    Ok(PostalCodeManagement {
        source_identity: snapshot_identity(&PostalCodeSnapshot::default()),
        revision: revisions(old.revision)?.0,
        manifest: None,
        generation,
        file_error: Some(reason),
        layer_error: None,
    })
}
fn ready(
    old: Arc<PostalCodeManagement>,
    old_overlay: Arc<LexiconOverlay>,
    next: PostalCodeManagement,
    new_overlay: Arc<LexiconOverlay>,
    kind: Kind,
) -> Result<Ready, Error> {
    // Rollback and disable share r+2: they are alternatives, never both
    // published. Disable keeps the old manifest and generation, so the disk
    // pointer (still old) describes it, and the next launch restores the old
    // postal layer from that pointer.
    let recovery = revisions(old.revision)?.1;
    let mut rollback = (*old).clone();
    rollback.revision = recovery;
    let mut disabled = rollback.clone();
    disabled.layer_error = Some("runtime_unavailable");
    disabled.source_identity = snapshot_identity(&PostalCodeSnapshot::default());
    Ok(Ready {
        kind,
        next: Arc::new(next),
        rollback: Arc::new(rollback),
        disabled: Arc::new(disabled),
        old_overlay,
        new_overlay,
        empty_overlay: Arc::new(LexiconOverlay::default()),
    })
}
fn decoded(
    old: &PostalCodeManagement,
    old_overlay: &Arc<LexiconOverlay>,
    snapshot: PostalCodeSnapshot,
    manifest: PostalCodeManifest,
    generation: Option<String>,
) -> Result<(PostalCodeManagement, Arc<LexiconOverlay>), Error> {
    let identity = snapshot_identity(&snapshot);
    let reuse = old.file_error.is_none()
        && old.layer_error.is_none()
        && old
            .manifest
            .as_ref()
            .is_some_and(|m| m.snapshot_sha256 == manifest.snapshot_sha256);
    let (overlay, layer_error) = if reuse {
        (old_overlay.clone(), None)
    } else {
        match snapshot.into_overlay() {
            Ok(o) => (Arc::new(o), None),
            Err(_) => (
                Arc::new(LexiconOverlay::default()),
                Some("invalid_postal_code_layer"),
            ),
        }
    };
    Ok((
        PostalCodeManagement {
            source_identity: if layer_error.is_none() {
                identity
            } else {
                snapshot_identity(&PostalCodeSnapshot::default())
            },
            revision: revisions(old.revision)?.0,
            manifest: Some(manifest),
            generation,
            file_error: None,
            layer_error,
        },
        overlay,
    ))
}
fn reply(
    ticket: &PostalCodeTicket,
    phase: &Phase,
    tsv: Vec<u8>,
    manifest: Vec<u8>,
) -> Result<PreparedPostalCodeReply, Error> {
    let value = match phase {
        Phase::Working(w) => {
            json!({"schema_version":1,"state":"needs_file","ticket":ticket,"request":w.request,"ready":null})
        }
        Phase::Ready(r) => {
            json!({"schema_version":1,"state":"ready","ticket":ticket,"request":null,"ready":{
            "kind":match r.kind{Kind::Loaded=>"loaded",Kind::InstallSeed=>"install_seed",Kind::Update=>"update"},
            "published_revision":r.next.revision,"recovery_revision":r.rollback.revision,"generation":r.next.generation,
            "previous_generation":r.rollback.generation,"current_bytes_base64":r.next.generation.as_ref().map(|s|base64::encode(format!("{s}\n"))),
            "next_view":r.next.view(),"rollback_view":r.rollback.view(),"disabled_view":r.disabled.view()}})
        }
        _ => return Err(Error::InvalidCommand),
    };
    let plan = serde_json::to_vec(&value).map_err(|_| Error::InvalidCommand)?;
    if plan.len() > MAX_POSTAL_VIEW_BYTES
        || tsv.len() > MAX_POSTAL_TSV_BYTES
        || manifest.len() > MAX_POSTAL_MANIFEST_BYTES
    {
        return Err(Error::InvalidCommand);
    }
    Ok(PreparedPostalCodeReply {
        tsv,
        manifest,
        plan,
        ticket: ticket.clone(),
    })
}
impl Core {
    pub fn postal_code_view(&self) -> PostalCodeView {
        self.vocabulary.postal_management().view()
    }
    pub fn begin_postal_code_load(
        &self,
        command: PostalCodeLoadCommand,
        bytes: &[u8],
    ) -> Result<PreparedPostalCodeReply, Error> {
        let _cleanup = Cleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .postal_code_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (old, layer) = self.vocabulary.postal_authorities();
        if old.revision != command.expected_revision {
            return Err(Error::RevisionMismatch);
        }
        revisions(old.revision)?;
        let ticket = edits.ticket(old.revision)?;
        let phase = match command.current_state {
            StoredFileState::Missing => Phase::Working(Work {
                old,
                old_overlay: layer.overlay.clone(),
                generation: None,
                manifest: None,
                request: Request {
                    request_id: 1,
                    kind: RequestKind::SeedManifest,
                    generation: None,
                },
            }),
            StoredFileState::Present => match crate::decode_imported_dictionary_pointer(bytes) {
                Ok(generation) => Phase::Working(Work {
                    old,
                    old_overlay: layer.overlay.clone(),
                    generation: Some(generation.clone()),
                    manifest: None,
                    request: Request {
                        request_id: 1,
                        kind: RequestKind::GenerationManifest,
                        generation: Some(generation),
                    },
                }),
                Err(_) => {
                    let next = readonly(&old, "invalid_current", None)?;
                    Phase::Ready(ready(
                        old,
                        layer.overlay.clone(),
                        next,
                        Arc::new(LexiconOverlay::default()),
                        Kind::Loaded,
                    )?)
                }
            },
            StoredFileState::Unreadable => {
                let next = readonly(&old, "current_unreadable", None)?;
                Phase::Ready(ready(
                    old,
                    layer.overlay.clone(),
                    next,
                    Arc::new(LexiconOverlay::default()),
                    Kind::Loaded,
                )?)
            }
        };
        let out = reply(&ticket, &phase, Vec::new(), Vec::new())?;
        edits.pending = Some(Pending { ticket, phase });
        Ok(out)
    }
    pub fn supply_postal_code_load(
        &self,
        command: PostalCodeSupplyCommand,
        bytes: &[u8],
    ) -> Result<PreparedPostalCodeReply, Error> {
        let _cleanup = Cleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .postal_code_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = edits.matching(&command.ticket)?;
        let Phase::Working(w) = &mut pending.phase else {
            return Err(Error::InvalidCommand);
        };
        if command.request_id != w.request.request_id {
            return Err(Error::InvalidCommand);
        }
        let kind = w.request.kind;
        let mut file_error = None;
        if command.file_state != StoredFileState::Present {
            file_error = Some(if command.file_state == StoredFileState::Missing {
                "postal_file_missing"
            } else {
                "postal_file_unreadable"
            });
        }
        let mut tsv = Vec::new();
        let mut manifest_out = Vec::new();
        if file_error.is_none()
            && matches!(
                kind,
                RequestKind::GenerationManifest | RequestKind::SeedManifest
            )
        {
            if bytes.len() > MAX_POSTAL_MANIFEST_BYTES {
                file_error = Some("file_too_large");
            } else {
                w.manifest = Some(bytes.to_vec());
                w.request.request_id = w
                    .request
                    .request_id
                    .checked_add(1)
                    .ok_or(Error::RevisionOverflow)?;
                w.request.kind = if kind == RequestKind::SeedManifest {
                    RequestKind::SeedTsv
                } else {
                    RequestKind::GenerationTsv
                };
                return reply(&command.ticket, &pending.phase, Vec::new(), Vec::new());
            }
        }
        let next = if let Some(reason) = file_error {
            Err(PostalCodeFileError(reason))
        } else {
            decode_postal_code_tsv(bytes).and_then(|snapshot| {
                let m = decode_postal_code_manifest(
                    w.manifest
                        .as_deref()
                        .ok_or(PostalCodeFileError("invalid_manifest"))?,
                    bytes,
                    snapshot.entries.len(),
                )?;
                Ok((snapshot, m))
            })
        };
        let (next, new_overlay, mode) = match next {
            Ok((snapshot, manifest)) => {
                let seed = kind == RequestKind::SeedTsv;
                let generation = if seed {
                    manifest_out = encode_postal_code_manifest(&manifest)?;
                    tsv = bytes.to_vec();
                    Some(hash(&manifest_out))
                } else {
                    w.generation.clone()
                };
                let (n, o) = decoded(&w.old, &w.old_overlay, snapshot, manifest, generation)?;
                (
                    n,
                    o,
                    if seed {
                        Kind::InstallSeed
                    } else {
                        Kind::Loaded
                    },
                )
            }
            Err(e) => (
                readonly(&w.old, e.0, w.generation.clone())?,
                Arc::new(LexiconOverlay::default()),
                Kind::Loaded,
            ),
        };
        let r = ready(
            w.old.clone(),
            w.old_overlay.clone(),
            next,
            new_overlay,
            mode,
        )?;
        let phase = Phase::Ready(r);
        let out = reply(&command.ticket, &phase, tsv, manifest_out)?;
        pending.phase = phase;
        Ok(out)
    }
    // Allowed in a read-only or failed state too: an explicit update is the
    // way out of both. Only the Japan Post source is accepted.
    pub fn prepare_postal_code(
        &self,
        command: PostalCodePrepareCommand,
        bytes: &[u8],
    ) -> Result<PreparedPostalCodeReply, Error> {
        let _cleanup = Cleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        if command.source != "japan_post" {
            return Err(Error::InvalidCommand);
        }
        let mut edits = self
            .postal_code_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (old, layer) = self.vocabulary.postal_authorities();
        if old.revision != command.expected_revision {
            return Err(Error::RevisionMismatch);
        }
        revisions(old.revision)?;
        let ticket = edits.ticket(old.revision)?;
        let file =
            prepare_postal_code_zip(bytes, command.retrieved_unix_seconds, command.last_modified)?;
        let (next, new_overlay) = decoded(
            &old,
            &layer.overlay,
            file.snapshot,
            file.manifest,
            Some(hash(&file.manifest_bytes)),
        )?;
        let phase = Phase::Ready(ready(
            old,
            layer.overlay.clone(),
            next,
            new_overlay,
            Kind::Update,
        )?);
        let out = reply(&ticket, &phase, file.tsv, file.manifest_bytes)?;
        edits.pending = Some(Pending { ticket, phase });
        Ok(out)
    }
    pub fn publish_postal_code(&self, ticket: &PostalCodeTicket) -> Result<(), Error> {
        self.change_postal_code(ticket, 0)
    }
    pub fn rollback_postal_code(&self, ticket: &PostalCodeTicket) -> Result<(), Error> {
        self.change_postal_code(ticket, 1)
    }
    pub fn disable_postal_code(&self, ticket: &PostalCodeTicket) -> Result<(), Error> {
        self.change_postal_code(ticket, 2)
    }
    // `action`: 0 publishes Ready, 1 rolls a publication back, 2 disables it.
    // Recovery actions are offered only after a real update or seed install;
    // a plain load wrote nothing and has nothing to undo.
    fn change_postal_code(&self, ticket: &PostalCodeTicket, action: u8) -> Result<(), Error> {
        let _cleanup = Cleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .postal_code_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = edits.matching(ticket)?;
        let (r, expected, management, overlay) = match (&pending.phase, action) {
            (Phase::Ready(r), 0) => (
                r,
                ticket.expected_revision,
                r.next.clone(),
                r.new_overlay.clone(),
            ),
            (Phase::Published(r), 1) if r.kind != Kind::Loaded => (
                r,
                r.next.revision,
                r.rollback.clone(),
                r.old_overlay.clone(),
            ),
            (Phase::Published(r), 2) if r.kind != Kind::Loaded => (
                r,
                r.next.revision,
                r.disabled.clone(),
                r.empty_overlay.clone(),
            ),
            _ => return Err(Error::InvalidCommand),
        };
        let _ = r;
        self.vocabulary.publish_postal_management(
            expected,
            management,
            overlay,
            || self.publication_guard(),
            |_| {},
        )?;
        // The successful swap is followed only by moves: ACK cannot fail while
        // serializing a response and trick the host into undoing a publication.
        let pending = edits.pending.take().expect("matched reservation");
        let r = match pending.phase {
            Phase::Ready(r) | Phase::Published(r) => r,
            _ => unreachable!(),
        };
        edits.pending = Some(Pending {
            ticket: pending.ticket,
            phase: match action {
                0 => Phase::Published(r),
                1 => Phase::RolledBack(r),
                _ => Phase::Disabled(r),
            },
        });
        Ok(())
    }
    pub fn finish_postal_code(&self, command: PostalCodeFinishCommand) -> Result<(), Error> {
        let _cleanup = Cleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .postal_code_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = edits.matching(&command.ticket)?;
        let expected = match &pending.phase {
            Phase::Published(r)
                if (r.kind == Kind::Loaded && command.outcome == PostalCodeFinishKind::Loaded)
                    || (r.kind != Kind::Loaded
                        && matches!(
                            command.outcome,
                            PostalCodeFinishKind::NewPointerDurable
                                | PostalCodeFinishKind::NewPointerWarning
                        )) =>
            {
                &r.next
            }
            Phase::RolledBack(r) if command.outcome == PostalCodeFinishKind::RolledBack => {
                &r.rollback
            }
            Phase::Disabled(r) if command.outcome == PostalCodeFinishKind::Disabled => &r.disabled,
            _ => return Err(Error::InvalidCommand),
        };
        if command.revision != expected.revision || command.generation != expected.generation {
            return Err(Error::RevisionMismatch);
        }
        let pending = edits.pending.take().expect("matched reservation");
        let r = match pending.phase {
            Phase::Published(r) | Phase::RolledBack(r) | Phase::Disabled(r) => r,
            _ => unreachable!(),
        };
        edits.pending = Some(Pending {
            ticket: pending.ticket,
            phase: Phase::Resolved { _retained: r },
        });
        Ok(())
    }
    // As with Imported, an unfinished Published/RolledBack/Disabled phase
    // refuses cancel as Busy on a live Core until finish resolves it. That
    // exit always exists: no other edit can acquire the retained reservation
    // or move its revision before finish, rollback or disable.
    pub fn cancel_postal_code(&self, ticket: &PostalCodeTicket) -> Result<(), Error> {
        let mut edits = self
            .postal_code_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(p) = &edits.pending {
            if p.ticket == *ticket {
                if matches!(
                    p.phase,
                    Phase::Published(_) | Phase::RolledBack(_) | Phase::Disabled(_)
                ) && !self.is_invalid()
                {
                    return Err(Error::Busy);
                }
                edits.pending = None;
            }
        }
        if self.is_invalid() {
            Err(Error::CoreInvalid)
        } else {
            Ok(())
        }
    }
    pub(crate) fn discard_idle_postal_code_preparation(&self) {
        let mut edits = match self.postal_code_edits.try_lock() {
            Ok(e) => e,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(_) => return,
        };
        edits.pending = None;
    }
    pub fn discard_postal_code_preparation(&self) {
        if self.is_invalid() {
            self.postal_code_edits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending = None;
        }
    }
}
pub(crate) fn new_postal_code_edits() -> Mutex<PostalCodeEdits> {
    Mutex::new(Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserDictionaryPriority;
    use crate::test_support::user_dictionary_snapshot;
    use crate::{CoreConfig, SharedVocabulary};
    use serde_json::Value;
    fn fixture_core() -> Core {
        Core::with_vocabulary(
            CoreConfig {
                dictionary_path: None,
            },
            SharedVocabulary::default(),
        )
        .unwrap()
    }
    fn manifest(tsv: &[u8], count: usize) -> Vec<u8> {
        encode_postal_code_manifest(&PostalCodeManifest {
            schema_version: 1,
            source_url: POSTAL_SOURCE_URL.into(),
            source_page_url: POSTAL_SOURCE_PAGE_URL.into(),
            source_last_modified: "unknown".into(),
            retrieved_at: "2026-10-16T00:00:00Z".into(),
            zip_sha256: "0".repeat(64),
            csv_sha256: "1".repeat(64),
            snapshot_sha256: hash(tsv),
            entry_count: count,
        })
        .unwrap()
    }
    fn request(core: &Core, state: StoredFileState, bytes: &[u8]) -> PreparedPostalCodeReply {
        core.begin_postal_code_load(
            PostalCodeLoadCommand {
                expected_revision: core.postal_code_view().management_revision,
                current_state: state,
            },
            bytes,
        )
        .unwrap()
    }
    fn supply(
        core: &Core,
        reply: PreparedPostalCodeReply,
        state: StoredFileState,
        bytes: &[u8],
    ) -> PreparedPostalCodeReply {
        let value: Value = serde_json::from_slice(&reply.plan).unwrap();
        core.supply_postal_code_load(
            PostalCodeSupplyCommand {
                ticket: reply.ticket,
                request_id: value["request"]["request_id"].as_u64().unwrap(),
                file_state: state,
            },
            bytes,
        )
        .unwrap()
    }
    fn loaded(core: &Core, tsv: &[u8], json: &[u8], generation: &str) -> PreparedPostalCodeReply {
        let r = request(
            core,
            StoredFileState::Present,
            format!("{generation}\n").as_bytes(),
        );
        let r = supply(core, r, StoredFileState::Present, json);
        supply(core, r, StoredFileState::Present, tsv)
    }
    fn finish(core: &Core, reply: &PreparedPostalCodeReply, outcome: PostalCodeFinishKind) {
        let v = core.postal_code_view();
        core.finish_postal_code(PostalCodeFinishCommand {
            ticket: reply.ticket.clone(),
            outcome,
            revision: v.management_revision,
            generation: v.generation,
        })
        .unwrap();
    }
    fn resolve(core: &Core, reply: &PreparedPostalCodeReply, kind: PostalCodeFinishKind) {
        core.publish_postal_code(&reply.ticket).unwrap();
        finish(core, reply, kind);
        core.cancel_postal_code(&reply.ticket).unwrap();
    }
    fn zip() -> Vec<u8> {
        let csv=b"\"\",\"\",\"1000001\",\"\",\"\",\"\",\"Tokyo\",\"City\",\"Town\",\"\",\"\",\"\",\"\",\"\",\"\"\n";
        crate::postal_code_zip::tests::fixture(8, &["utf_ken_all.csv"], csv, false)
    }
    fn update(core: &Core, seconds: i64) -> PreparedPostalCodeReply {
        core.prepare_postal_code(
            PostalCodePrepareCommand {
                expected_revision: core.postal_code_view().management_revision,
                source: "japan_post".into(),
                retrieved_unix_seconds: seconds,
                last_modified: None,
            },
            &zip(),
        )
        .unwrap()
    }
    // A load trusts the generation named by the pointer, even when that name is
    // not the manifest's own hash, and returns no bytes to rewrite: reading
    // never turns into a save. A blank TSV line is tolerated on the way.
    #[test]
    fn named_load_keeps_existing_bytes_and_does_not_compare_physical_generation_to_manifest_hash() {
        let core = fixture_core();
        let tsv = b"# muu-postal-code\t1\n\n1000001\tTokyo";
        let m = manifest(tsv, 1);
        let physical = "a".repeat(64);
        assert_ne!(physical, hash(&m));
        let r = loaded(&core, tsv, &m, &physical);
        assert!(r.tsv.is_empty() && r.manifest.is_empty());
        assert_eq!(core.postal_code_view().management_revision, 0);
        resolve(&core, &r, PostalCodeFinishKind::Loaded);
        assert_eq!(core.postal_code_view().generation, Some(physical));
        assert_eq!(core.postal_code_view().layer_status, "available");
    }
    #[test]
    fn missing_current_uses_valid_seed_but_bad_existing_files_are_read_only_without_seed() {
        let core = fixture_core();
        let tsv = b"# muu-postal-code\t1\n1000001\tTown\n";
        let m = manifest(tsv, 1);
        let r = request(&core, StoredFileState::Missing, &[]);
        assert_eq!(
            serde_json::from_slice::<Value>(&r.plan).unwrap()["request"]["kind"],
            "seed_manifest"
        );
        let r = supply(&core, r, StoredFileState::Present, &m);
        let r = supply(&core, r, StoredFileState::Present, tsv);
        assert_eq!(r.tsv, tsv);
        assert_eq!(r.manifest, m);
        assert_eq!(core.postal_code_view().manifest, None);
        resolve(&core, &r, PostalCodeFinishKind::NewPointerDurable);
        for (state, bytes) in [
            (StoredFileState::Present, &b"bad"[..]),
            (StoredFileState::Unreadable, &b""[..]),
        ] {
            let r = request(&core, state, bytes);
            let p: Value = serde_json::from_slice(&r.plan).unwrap();
            assert_eq!(p["state"], "ready");
            assert_eq!(p["ready"]["next_view"]["access"], "read_only");
            assert!(r.tsv.is_empty() && r.manifest.is_empty());
            resolve(&core, &r, PostalCodeFinishKind::Loaded);
        }
        // Explicit whole update is allowed to recover read-only management.
        let r = update(&core, 1);
        resolve(&core, &r, PostalCodeFinishKind::NewPointerDurable);
        assert_eq!(core.postal_code_view().access, "editable");
    }
    #[test]
    fn malformed_and_missing_generation_files_are_empty_whole_layer_without_partial_adoption() {
        for missing in [false, true] {
            let core = fixture_core();
            let r = request(
                &core,
                StoredFileState::Present,
                format!("{}\n", "b".repeat(64)).as_bytes(),
            );
            let r = supply(
                &core,
                r,
                if missing {
                    StoredFileState::Missing
                } else {
                    StoredFileState::Present
                },
                b"not JSON",
            );
            let r = if missing {
                r
            } else {
                supply(
                    &core,
                    r,
                    StoredFileState::Present,
                    b"# muu-postal-code\t1\n1000001\tTown\n",
                )
            };
            resolve(&core, &r, PostalCodeFinishKind::Loaded);
            assert_eq!(core.postal_code_view().access, "read_only");
            assert!(core.vocabulary.postal_snapshot().1.is_empty());
        }
    }
    #[test]
    fn semantic_overflow_keeps_summary_and_empty_layer_then_explicit_update_repairs_it() {
        let core = fixture_core();
        let mut snapshot = PostalCodeSnapshot::default();
        for i in 0..3100 {
            snapshot.entries.push(crate::PostalCodeRecord {
                reading: format!("{i:07}"),
                surface: "a".repeat(4096),
            });
        }
        let tsv = encode_postal_code_tsv(&snapshot).unwrap();
        assert!(tsv.len() < MAX_POSTAL_TSV_BYTES);
        let m = manifest(&tsv, snapshot.entries.len());
        let r = loaded(&core, &tsv, &m, &hash(&m));
        resolve(&core, &r, PostalCodeFinishKind::Loaded);
        assert_eq!(core.postal_code_view().layer_status, "empty_degraded");
        assert_eq!(core.postal_code_view().manifest.unwrap().entry_count, 3100);
        assert!(core.vocabulary.postal_snapshot().1.is_empty());
        let r = update(&core, 1);
        resolve(&core, &r, PostalCodeFinishKind::NewPointerDurable);
        assert_eq!(core.postal_code_view().layer_status, "available");
    }
    #[test]
    fn published_authority_survives_until_finish_gc_and_completion_then_cancel() {
        let core = fixture_core();
        let r = update(&core, 1);
        let p: Value = serde_json::from_slice(&r.plan).unwrap();
        assert_eq!(p["ready"]["published_revision"], 1);
        assert_eq!(p["ready"]["recovery_revision"], 2);
        assert_eq!(core.postal_code_view().management_revision, 0);
        core.publish_postal_code(&r.ticket).unwrap();
        assert_eq!(core.cancel_postal_code(&r.ticket), Err(Error::Busy));
        assert!(matches!(
            core.prepare_postal_code(
                PostalCodePrepareCommand {
                    expected_revision: 1,
                    source: "japan_post".into(),
                    retrieved_unix_seconds: 2,
                    last_modified: None
                },
                &zip()
            ),
            Err(Error::Busy)
        ));
        finish(&core, &r, PostalCodeFinishKind::NewPointerDurable);
        assert_eq!(core.cancel_postal_code(&r.ticket), Ok(()));
        assert_eq!(core.cancel_postal_code(&r.ticket), Ok(()));
    }
    #[test]
    fn rollback_and_postal_only_disable_advance_revision_and_preserve_other_layers() {
        for disable in [false, true] {
            let core = fixture_core();
            let first = update(&core, 1);
            resolve(&core, &first, PostalCodeFinishKind::NewPointerDurable);
            let retained = core.vocabulary.snapshot();
            let next = update(&core, 2);
            core.publish_postal_code(&next.ticket).unwrap();
            core.set_user_fixture(user_dictionary_snapshot([(
                "きょう",
                "共有",
                UserDictionaryPriority::High,
            )]))
            .unwrap();
            let user = core.vocabulary.snapshot().user.clone();
            if disable {
                core.disable_postal_code(&next.ticket).unwrap();
                finish(&core, &next, PostalCodeFinishKind::Disabled);
                assert!(core.vocabulary.postal_snapshot().1.is_empty());
                assert_eq!(core.postal_code_view().reason, Some("runtime_unavailable"));
            } else {
                core.rollback_postal_code(&next.ticket).unwrap();
                finish(&core, &next, PostalCodeFinishKind::RolledBack);
                assert!(Arc::ptr_eq(
                    &retained.postal.overlay,
                    &core.vocabulary.snapshot().postal.overlay
                ));
            }
            assert_eq!(core.postal_code_view().management_revision, 3);
            assert!(Arc::ptr_eq(&user, &core.vocabulary.snapshot().user));
            assert!(!retained.postal.overlay.is_empty());
            core.cancel_postal_code(&next.ticket).unwrap();
        }
    }
    #[test]
    fn metadata_only_update_reuses_overlay_and_releases_all_raw_output_after_prepare() {
        let core = fixture_core();
        let r = update(&core, 1);
        resolve(&core, &r, PostalCodeFinishKind::NewPointerDurable);
        let old = core.vocabulary.snapshot().postal.clone();
        let r = update(&core, 2);
        assert_ne!(
            core.postal_code_view().generation,
            serde_json::from_slice::<Value>(&r.plan).unwrap()["ready"]["generation"]
                .as_str()
                .map(str::to_owned)
        );
        resolve(&core, &r, PostalCodeFinishKind::NewPointerDurable);
        let new = core.vocabulary.snapshot().postal.clone();
        assert!(Arc::ptr_eq(&old.overlay, &new.overlay));
        assert_eq!(old.revision, new.revision);
    }
    // All resolved exits retain the reservation until cancel. A second
    // real update must remain Busy without changing the published authority.
    #[test]
    fn published_update_keeps_the_reservation_through_every_recovery_exit() {
        for action in [1, 2, 3] {
            let core = fixture_core();
            let r = update(&core, 1);
            core.publish_postal_code(&r.ticket).unwrap();
            let published = core.vocabulary.snapshot().postal.clone();
            let next_update = || {
                core.prepare_postal_code(
                    PostalCodePrepareCommand {
                        expected_revision: core.postal_code_view().management_revision,
                        source: "japan_post".into(),
                        retrieved_unix_seconds: 2,
                        last_modified: None,
                    },
                    &zip(),
                )
                .map(|_| ())
            };
            assert_eq!(next_update(), Err(Error::Busy));
            assert_eq!(core.postal_code_view().management_revision, 1);
            assert!(Arc::ptr_eq(&published, &core.vocabulary.snapshot().postal));
            let kind = match action {
                1 => {
                    core.rollback_postal_code(&r.ticket).unwrap();
                    PostalCodeFinishKind::RolledBack
                }
                2 => {
                    core.disable_postal_code(&r.ticket).unwrap();
                    PostalCodeFinishKind::Disabled
                }
                _ => PostalCodeFinishKind::NewPointerDurable,
            };
            assert_eq!(next_update(), Err(Error::Busy));
            finish(&core, &r, kind);
            assert_eq!(next_update(), Err(Error::Busy));
            core.cancel_postal_code(&r.ticket).unwrap();
            let next = update(&core, 2);
            core.cancel_postal_code(&next.ticket).unwrap();
            assert!(!core.is_invalid());
        }
    }
    #[test]
    fn ticket_owner_domain_requests_and_finish_outcome_cannot_be_guessed() {
        let core = fixture_core();
        let other = fixture_core();
        let r = request(&core, StoredFileState::Missing, &[]);
        assert_eq!(
            other.publish_postal_code(&r.ticket),
            Err(Error::InvalidTicket)
        );
        assert!(matches!(
            core.supply_postal_code_load(
                PostalCodeSupplyCommand {
                    ticket: r.ticket.clone(),
                    request_id: 99,
                    file_state: StoredFileState::Missing
                },
                &[]
            ),
            Err(Error::InvalidCommand)
        ));
        core.cancel_postal_code(&r.ticket).unwrap();
        let r = update(&core, 1);
        core.publish_postal_code(&r.ticket).unwrap();
        assert_eq!(
            core.finish_postal_code(PostalCodeFinishCommand {
                ticket: r.ticket.clone(),
                outcome: PostalCodeFinishKind::Loaded,
                revision: 1,
                generation: None
            }),
            Err(Error::InvalidCommand)
        );
        core.invalidate();
        assert_eq!(core.cancel_postal_code(&r.ticket), Err(Error::CoreInvalid));
        assert!(core.postal_code_edits.lock().unwrap().pending.is_none());
    }
    #[test]
    fn private_panics_release_only_unpublished_preparation_and_do_not_invalidate_core() {
        let core = fixture_core();
        let _r = update(&core, 1);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _cleanup = Cleanup(&core);
                let _edits = core.postal_code_edits.lock().unwrap();
                panic!("private fixture");
            }))
            .is_err()
        );
        assert!(!core.is_invalid());
        assert!(
            core.postal_code_edits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .is_none()
        );
        let r = update(&core, 2);
        core.cancel_postal_code(&r.ticket).unwrap();
    }
    #[test]
    fn publication_panics_keep_one_complete_authority_and_invalidate_core() {
        for after in [false, true] {
            let core = fixture_core();
            let r = update(&core, 1);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let edits = core.postal_code_edits.lock().unwrap();
                    let Phase::Ready(ready) = &edits.pending.as_ref().unwrap().phase else {
                        unreachable!()
                    };
                    core.vocabulary
                        .publish_postal_management(
                            0,
                            ready.next.clone(),
                            ready.new_overlay.clone(),
                            || core.publication_guard(),
                            |point| {
                                if point == after {
                                    panic!("public fixture");
                                }
                            },
                        )
                        .unwrap();
                }))
                .is_err()
            );
            assert!(core.is_invalid());
            assert_eq!(
                core.postal_code_view().management_revision,
                u64::from(after)
            );
            assert_eq!(core.vocabulary.snapshot().postal.overlay.is_empty(), !after);
            assert_eq!(core.cancel_postal_code(&r.ticket), Err(Error::CoreInvalid));
        }
    }
}
