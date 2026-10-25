//! Imported-file transactions outlive publication: the pointer is written only
//! after its ACK. A private ticket therefore retains both authorities until
//! rollback or finish, and remains reserved until GC and UI completion.

use crate::StoredFileState;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use muu_conversion::LexiconOverlay;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::imported_dictionary_catalog_file::{
    ImportedDictionaryManifestEntry, LegacyImportedDictionary, MAX_IMPORTED_CATALOG_BYTES,
    decode_imported_dictionary_manifest, decode_imported_dictionary_pointer,
    decode_legacy_imported_catalog, encode_imported_dictionary_manifest,
};
use crate::imported_dictionary_file::{
    MAX_IMPORTED_SOURCE_BYTES, decode_import_source, decode_imported_dictionary_file,
    encode_imported_dictionary_file, is_hash, sha256,
};
use crate::imported_dictionary_management::{
    ImportedDictionaryEditError as Error, ImportedDictionaryManagement,
    ImportedDictionaryPublication, ImportedDictionaryView, ImportedRuntimePart, build_publication,
    summarize_document, summary_identity, summary_layer_error,
};
use crate::{Core, ImportedDictionaryDocument};

pub const MAX_IMPORTED_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_IMPORTED_TICKET_BYTES: usize = 1024;
pub const MAX_IMPORTED_REPLY_BYTES: usize = 32 * 1024 * 1024;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportedDictionaryTicket {
    domain: String,
    owner: u64,
    operation: u64,
    expected_revision: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedDictionaryLoadCommand {
    pub expected_revision: u64,
    pub file_state: StoredFileState,
    pub now_unix_ms: i64,
}

// The three user edits. Only Import carries bytes (the chosen source file);
// for the others the byte argument must be empty.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportedDictionaryEditCommand {
    Import {
        expected_revision: u64,
        source_file_name: String,
        imported_at_unix_ms: i64,
        enabled: bool,
    },
    SetEnabled {
        expected_revision: u64,
        dictionary_id: String,
        enabled: bool,
    },
    Remove {
        expected_revision: u64,
        dictionary_id: String,
    },
}
impl ImportedDictionaryEditCommand {
    fn revision(&self) -> u64 {
        match self {
            Self::Import {
                expected_revision, ..
            }
            | Self::SetEnabled {
                expected_revision, ..
            }
            | Self::Remove {
                expected_revision, ..
            } => *expected_revision,
        }
    }
}

// The host's answer to the last reply's request. `Read` returns the bytes of
// the file it asked for (the request id must be the outstanding one); `Written`
// confirms that the blob it handed over was stored under that SHA-256.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportedDictionarySupplyCommand {
    Read {
        ticket: ImportedDictionaryTicket,
        request_id: u64,
        file_state: StoredFileState,
    },
    Written {
        ticket: ImportedDictionaryTicket,
        sha256: String,
    },
}
impl ImportedDictionarySupplyCommand {
    pub fn ticket(&self) -> &ImportedDictionaryTicket {
        match self {
            Self::Read { ticket, .. } | Self::Written { ticket, .. } => ticket,
        }
    }
}

// How the host's disk step ended: a load needs no write (`Loaded`); an edit
// wrote the new pointer, with or without a durability warning; or the host
// restored the old pointer after `rollback`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportedDictionaryFinishKind {
    Loaded,
    NewPointerDurable,
    NewPointerWarning,
    RolledBack,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedDictionaryFinishCommand {
    pub ticket: ImportedDictionaryTicket,
    pub outcome: ImportedDictionaryFinishKind,
    pub revision: u64,
    pub generation_sha256: Option<String>,
}

/// Both byte outputs are prepared before FFI creates either owned MuuBuffer.
#[derive(Debug)]
pub struct PreparedImportedDictionaryReply {
    pub generation_bytes: Vec<u8>,
    pub plan_json: Vec<u8>,
    pub ticket: ImportedDictionaryTicket,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReadKind {
    Generation,
    Blob,
    LegacyCatalog,
}
#[derive(Clone, Debug, Serialize)]
struct ReadRequest {
    request_id: u64,
    kind: ReadKind,
    sha256: Option<String>,
    purpose: &'static str,
}
enum ReadTask {
    Generation(String),
    Blob {
        entry: Box<ImportedDictionaryManifestEntry>,
        purpose: &'static str,
        write_canonical: bool,
    },
    Legacy {
        hash: Option<String>,
        migration: bool,
    },
}
impl ReadTask {
    fn request(&self, id: u64) -> ReadRequest {
        match self {
            Self::Generation(hash) => ReadRequest {
                request_id: id,
                kind: ReadKind::Generation,
                sha256: Some(hash.clone()),
                purpose: "startup",
            },
            Self::Blob { entry, purpose, .. } => ReadRequest {
                request_id: id,
                kind: ReadKind::Blob,
                sha256: Some(entry.blob_sha256.clone()),
                purpose,
            },
            Self::Legacy { hash, migration } => ReadRequest {
                request_id: id,
                kind: ReadKind::LegacyCatalog,
                sha256: hash.clone(),
                purpose: if *migration { "migration" } else { "startup" },
            },
        }
    }
}

struct Artifact {
    hash: String,
    bytes: Vec<u8>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Load,
    Edit,
}
struct Work {
    mode: Mode,
    old: Arc<ImportedDictionaryPublication>,
    old_overlay: Arc<LexiconOverlay>,
    old_layer_revision: u64,
    candidate: ImportedDictionaryManagement,
    parts: BTreeMap<String, Arc<ImportedRuntimePart>>,
    tasks: VecDeque<ReadTask>,
    request: Option<ReadRequest>,
    request_sequence: u64,
    migration: VecDeque<LegacyImportedDictionary>,
    artifact: Option<Artifact>,
    staged: BTreeSet<String>,
    replaced_id: Option<String>,
    now_unix_ms: i64,
    preview: Option<serde_json::Value>,
}

struct Ready {
    mode: Mode,
    old_overlay: Arc<LexiconOverlay>,
    next: Arc<ImportedDictionaryPublication>,
    next_overlay: Arc<LexiconOverlay>,
    rollback: Arc<ImportedDictionaryPublication>,
    generation: Vec<u8>,
    plan: serde_json::Value,
}
// One operation's life, in order. Working: Core is asking the host for a
// file or for one staged blob to be written. Ready: generation bytes and the
// plan exist, nothing is published. Published: the next state is live and the
// host is writing the pointer. RolledBack: the old content is live again at
// revision r+2. Resolved: finish was acknowledged; both authorities are kept
// until the host has collected files and completed the UI.
//
// Cancel releases any phase except Published on a live Core: once published,
// the host must either finish or roll back, so cancel cannot strand the disk
// pointer and the live state in disagreement.
enum Phase {
    Working(Box<Work>),
    Ready(Ready),
    Published(Ready),
    RolledBack(Ready),
    Resolved { _retained: Ready },
}
struct Pending {
    ticket: ImportedDictionaryTicket,
    phase: Phase,
}
pub(crate) struct ImportedDictionaryEdits {
    owner: u64,
    operation: u64,
    pending: Option<Pending>,
}
impl Default for ImportedDictionaryEdits {
    fn default() -> Self {
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            operation: 0,
            pending: None,
        }
    }
}

/// Declared before the private lock, so it runs after unlock on unwind. Private
/// failure discards only unpublished work; publication panic invalidates Core
/// and allows every phase to release its retained recovery authorities.
struct PrivateCleanup<'a>(&'a Core);
impl Drop for PrivateCleanup<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() || self.0.is_invalid() {
            let mut edits = match self.0.imported_dictionary_edits.try_lock() {
                Ok(edits) => edits,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return,
            };
            let unpublished = edits
                .pending
                .as_ref()
                .is_some_and(|p| matches!(p.phase, Phase::Working(_) | Phase::Ready(_)));
            if unpublished || self.0.is_invalid() {
                edits.pending = None;
            }
        }
    }
}

fn current_work(core: &Core, revision: u64, mode: Mode, now: i64) -> Result<Work, Error> {
    if core.is_invalid() {
        return Err(Error::CoreInvalid);
    }
    let (old, layer) = core.vocabulary.imported_authorities();
    if old.management.revision != revision {
        return Err(Error::RevisionMismatch);
    }
    let mut candidate = old.management.clone();
    candidate.revision = revision.checked_add(1).ok_or(Error::RevisionOverflow)?;
    // A rollback publishes at r+2. Check that now, while nothing is staged,
    // so the overflow cannot first appear after the host has published.
    revision.checked_add(2).ok_or(Error::RevisionOverflow)?;
    Ok(Work {
        mode,
        old_overlay: layer.overlay.clone(),
        old_layer_revision: layer.revision,
        parts: old.parts.clone(),
        old,
        candidate,
        tasks: VecDeque::new(),
        request: None,
        request_sequence: 0,
        migration: VecDeque::new(),
        artifact: None,
        staged: BTreeSet::new(),
        replaced_id: None,
        now_unix_ms: now,
        preview: None,
    })
}

impl Work {
    fn readonly(&mut self, reason: &'static str) {
        self.candidate.initialized = true;
        self.candidate.dictionaries = None;
        self.candidate.file_error = Some(reason);
        self.candidate.layer_error = None;
        self.parts.clear();
        self.tasks.clear();
        self.migration.clear();
        self.artifact = None;
        self.request = None;
    }

    fn accept_document(
        &mut self,
        document: ImportedDictionaryDocument,
        entry: ImportedDictionaryManifestEntry,
        write: bool,
    ) -> Result<(), Error> {
        let (mut summary, part, bytes) = summarize_document(document, entry)?;
        let id = summary.manifest.metadata.id.clone();
        if write {
            summary.manifest.blob_sha256 = summary.canonical_blob_sha256.clone();
            if !self.staged.contains(&summary.canonical_blob_sha256) {
                self.artifact = Some(Artifact {
                    hash: summary.canonical_blob_sha256.clone(),
                    bytes,
                });
            }
        }
        let dictionaries = self.candidate.dictionaries.get_or_insert_default();
        let prior = dictionaries
            .iter()
            .position(|d| d.manifest.metadata.id == id)
            .map(|index| dictionaries.remove(index));
        let reuse = prior
            .as_ref()
            .is_some_and(|d| d.source_identity == summary.source_identity);
        if summary.manifest.enabled {
            if !reuse || !self.parts.contains_key(&id) {
                self.parts.insert(id.clone(), part);
            }
        } else {
            self.parts.remove(&id);
        }
        dictionaries.push(summary);
        dictionaries.sort_by_key(|d| d.manifest.order);
        // Stop accumulating enabled source rows as soon as coarse bounds fail.
        // Every remaining blob is still validated and summarized individually.
        if summary_layer_error(dictionaries).is_some() {
            self.parts.clear();
        }
        Ok(())
    }

    fn accept_read(
        &mut self,
        request_id: u64,
        state: StoredFileState,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let request = self.request.take().ok_or(Error::InvalidCommand)?;
        if request.request_id != request_id {
            return Err(Error::InvalidCommand);
        }
        let task = self.tasks.pop_front().ok_or(Error::InvalidCommand)?;
        let limit = if matches!(task, ReadTask::Blob { .. }) {
            MAX_IMPORTED_SOURCE_BYTES
        } else {
            MAX_IMPORTED_CATALOG_BYTES
        };
        if bytes.len() > limit {
            return Err(Error::File(crate::ImportedDictionaryFileError::new(
                "source_too_large",
            )));
        }
        if state != StoredFileState::Present {
            if self.mode == Mode::Load
                && matches!(
                    task,
                    ReadTask::Legacy {
                        hash: None,
                        migration: false
                    }
                )
                && state == StoredFileState::Missing
            {
                return Ok(());
            }
            return Err(Error::File(crate::ImportedDictionaryFileError::new(
                if state == StoredFileState::Missing {
                    "missing_imported_dictionary_file"
                } else {
                    "unreadable_imported_dictionary_file"
                },
            )));
        }
        if request
            .sha256
            .as_ref()
            .is_some_and(|hash| *hash != sha256(bytes))
        {
            return Err(Error::File(crate::ImportedDictionaryFileError::new(
                "imported_dictionary_hash_mismatch",
            )));
        }
        match task {
            ReadTask::Generation(hash) => {
                let manifest = decode_imported_dictionary_manifest(bytes)?;
                self.candidate.generation = Some(hash);
                self.candidate.legacy_sha256 = None;
                for entry in manifest.dictionaries {
                    self.tasks.push_back(ReadTask::Blob {
                        entry: Box::new(entry),
                        purpose: "startup",
                        write_canonical: false,
                    });
                }
            }
            ReadTask::Blob {
                entry,
                write_canonical,
                ..
            } => {
                let document = decode_imported_dictionary_file(bytes)?;
                self.accept_document(document, *entry, write_canonical)?;
            }
            ReadTask::Legacy { migration, .. } => {
                let legacy = decode_legacy_imported_catalog(bytes, self.now_unix_ms)?;
                if migration {
                    // The legacy catalog is one file holding every dictionary,
                    // so all of its decoded documents stay here until each one
                    // has been written as a blob. Unlike the blob paths, this
                    // holds more than one dictionary's rows at a time.
                    self.migration = legacy.into();
                    self.candidate.legacy_sha256 = None;
                } else {
                    self.candidate.legacy_sha256 = Some(sha256(bytes));
                    for dictionary in legacy {
                        let canonical = encode_imported_dictionary_file(&dictionary.document)?;
                        let document = decode_imported_dictionary_file(&canonical)?;
                        let entry = ImportedDictionaryManifestEntry {
                            metadata: document.metadata.clone(),
                            enabled: dictionary.enabled,
                            imported_at_unix_ms: dictionary.imported_at_unix_ms,
                            source_sha256: dictionary.source_sha256,
                            blob_sha256: sha256(&canonical),
                            order: dictionary.order,
                        };
                        self.accept_document(document, entry, false)?;
                    }
                }
            }
        }
        Ok(())
    }

    // Which stored blobs an edit must read back before it can build the new
    // layer: a legacy catalog that still holds other dictionaries (it is
    // migrated whole), a blob whose stored bytes are not canonical (rewritten),
    // and an enabled dictionary whose rows are not in memory. The dictionary
    // being replaced by an import is skipped; its new rows are already here.
    fn queue_edit_reads(&mut self) {
        let dictionaries = self.candidate.dictionaries.as_deref().unwrap_or_default();
        if let Some(hash) = &self.candidate.legacy_sha256 {
            if dictionaries
                .iter()
                .any(|d| self.replaced_id.as_ref() != Some(&d.manifest.metadata.id))
            {
                self.tasks.push_back(ReadTask::Legacy {
                    hash: Some(hash.clone()),
                    migration: true,
                });
            } else {
                self.candidate.legacy_sha256 = None;
            }
            return;
        }
        let recover = self.old.management.layer_error.is_some();
        let can_build = summary_layer_error(dictionaries).is_none();
        for dictionary in dictionaries {
            let id = &dictionary.manifest.metadata.id;
            if self.replaced_id.as_ref() == Some(id) {
                continue;
            }
            let dirty = dictionary.manifest.blob_sha256 != dictionary.canonical_blob_sha256;
            let missing = can_build && dictionary.manifest.enabled && !self.parts.contains_key(id);
            if dirty || missing {
                self.tasks.push_back(ReadTask::Blob {
                    entry: Box::new(dictionary.manifest.clone()),
                    purpose: if dirty {
                        "canonicalization"
                    } else if recover {
                        "degraded_recovery"
                    } else {
                        "enable"
                    },
                    write_canonical: dirty,
                });
            }
        }
    }

    // At most one thing is outstanding with the host: either a blob to write
    // (`artifact`) or one file to read (`request`). Migration documents are
    // turned into blobs one at a time for that reason.
    fn next_request(&mut self) -> Result<(), Error> {
        while self.artifact.is_none() {
            let Some(dictionary) = self.migration.pop_front() else {
                break;
            };
            if self.replaced_id.as_ref() == Some(&dictionary.document.metadata.id) {
                continue;
            }
            let Some(entry) = self
                .candidate
                .dictionaries
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|d| d.manifest.metadata.id == dictionary.document.metadata.id)
                .map(|d| d.manifest.clone())
            else {
                continue;
            };
            let canonical = encode_imported_dictionary_file(&dictionary.document)?;
            self.accept_document(decode_imported_dictionary_file(&canonical)?, entry, true)?;
        }
        if self.artifact.is_none() && self.request.is_none() {
            if let Some(task) = self.tasks.front() {
                self.request_sequence = self
                    .request_sequence
                    .checked_add(1)
                    .ok_or(Error::RevisionOverflow)?;
                self.request = Some(task.request(self.request_sequence));
            }
        }
        Ok(())
    }
}

// Builds everything the publish needs while nothing is live yet: the next
// publication and overlay, the rollback state at r+2, the generation bytes
// and the plan the host follows (pointer bytes, blobs to keep and to collect,
// both views). An edit that would make a valid layer invalid is refused here.
fn make_ready(work: Work) -> Result<Ready, Error> {
    let unchanged = work.old.management.file_error.is_none()
        && work.old.management.layer_error.is_none()
        && work.candidate.file_error.is_none()
        && summary_identity(work.candidate.dictionaries.as_deref().unwrap_or_default())
            == work.old.source_identity;
    let (next, next_overlay) = if unchanged {
        // Like the legacy snapshot equality check, metadata-only/no-op edits
        // do not rebuild an identical runtime layer. The identity includes
        // ordered IDs, enabled flags and exact normalized rows, never metadata.
        (
            Arc::new(ImportedDictionaryPublication {
                management: work.candidate,
                parts: work.parts,
                source_identity: work.old.source_identity.clone(),
            }),
            work.old_overlay.clone(),
        )
    } else {
        build_publication(work.candidate, work.parts).map_err(Error::Layer)?
    };
    if work.mode == Mode::Edit && work.old.management.layer_error.is_none() {
        if let Some(reason) = next.management.layer_error {
            return Err(Error::Layer(reason));
        }
    }
    if work.old.source_identity != next.source_identity {
        work.old_layer_revision
            .checked_add(2)
            .ok_or(Error::RevisionOverflow)?;
    }
    let mut next = Arc::unwrap_or_clone(next);
    let generation = if work.mode == Mode::Edit {
        let bytes = encode_imported_dictionary_manifest(&next.management.manifest())?;
        next.management.generation = Some(sha256(&bytes));
        next.management.legacy_sha256 = None;
        bytes
    } else {
        Vec::new()
    };
    let next = Arc::new(next);
    // The old content is republished under a revision of its own, r+2, not
    // r. A view or ticket taken from either the old (r) or the published (r+1)
    // state then fails the revision check instead of matching restored content.
    let mut rollback = work.old.as_ref().clone();
    rollback.management.revision = work
        .old
        .management
        .revision
        .checked_add(2)
        .ok_or(Error::RevisionOverflow)?;
    let referenced: Vec<_> = next
        .management
        .dictionaries
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|d| d.manifest.blob_sha256.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let previous: Vec<_> = work
        .old
        .management
        .dictionaries
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|d| d.manifest.blob_sha256.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let pointer = next
        .management
        .generation
        .as_ref()
        .map(|hash| base64::encode(format!("{hash}\n")));
    let plan = json!({"expected_revision":work.old.management.revision,"published_revision":next.management.revision,
        "rollback_revision":rollback.management.revision,"generation_sha256":next.management.generation,
        "previous_generation_sha256":work.old.management.generation,
        "current_bytes_base64":pointer,"referenced_blobs":referenced,"previous_referenced_blobs":previous,
        "next_view":next.management.view(),"rollback_view":rollback.management.view(),"preview":work.preview});
    Ok(Ready {
        mode: work.mode,
        old_overlay: work.old_overlay,
        next,
        next_overlay,
        rollback: Arc::new(rollback),
        generation,
        plan,
    })
}

fn reply(
    ticket: &ImportedDictionaryTicket,
    phase: &Phase,
) -> Result<PreparedImportedDictionaryReply, Error> {
    let (generation, value) = match phase {
        Phase::Working(work) => {
            let artifact = work
                .artifact
                .as_ref()
                .map(|a| json!({"sha256":a.hash,"byte_length":a.bytes.len()}));
            (
                Vec::new(),
                json!({"schema_version":1,"state":"needs_blob","ticket":ticket,
                "request":work.request,"write_blob":artifact,"preview":work.preview,"ready":null}),
            )
        }
        Phase::Ready(ready) => (
            ready.generation.clone(),
            json!({"schema_version":1,"state":"ready","ticket":ticket,
            "request":null,"write_blob":null,"preview":null,"ready":ready.plan}),
        ),
        _ => return Err(Error::InvalidCommand),
    };
    let plan_json = serde_json::to_vec(&value).map_err(|_| Error::InvalidCommand)?;
    if plan_json.len() > MAX_IMPORTED_REPLY_BYTES {
        return Err(Error::InvalidCommand);
    }
    Ok(PreparedImportedDictionaryReply {
        generation_bytes: generation,
        plan_json,
        ticket: ticket.clone(),
    })
}

fn advance(mut work: Work) -> Result<Phase, Error> {
    work.next_request()?;
    if work.request.is_none() && work.artifact.is_none() {
        Ok(Phase::Ready(make_ready(work)?))
    } else {
        Ok(Phase::Working(Box::new(work)))
    }
}

impl ImportedDictionaryEdits {
    fn reserve(&mut self, revision: u64) -> Result<ImportedDictionaryTicket, Error> {
        if self.pending.is_some() {
            return Err(Error::Busy);
        }
        self.operation = self
            .operation
            .checked_add(1)
            .ok_or(Error::RevisionOverflow)?;
        Ok(ImportedDictionaryTicket {
            domain: "imported_dictionary".into(),
            owner: self.owner,
            operation: self.operation,
            expected_revision: revision,
        })
    }
    fn matching(&mut self, ticket: &ImportedDictionaryTicket) -> Result<&mut Pending, Error> {
        self.pending
            .as_mut()
            .filter(|p| p.ticket == *ticket)
            .ok_or(Error::InvalidTicket)
    }
}

impl Core {
    pub fn imported_dictionary_view(&self) -> ImportedDictionaryView {
        self.vocabulary.imported_management().management.view()
    }

    pub fn begin_imported_dictionary_load(
        &self,
        command: ImportedDictionaryLoadCommand,
        bytes: &[u8],
    ) -> Result<PreparedImportedDictionaryReply, Error> {
        let _cleanup = PrivateCleanup(self);
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ticket = edits.reserve(command.expected_revision)?;
        let mut work = current_work(
            self,
            command.expected_revision,
            Mode::Load,
            command.now_unix_ms,
        )?;
        if work.old.management.initialized {
            return Err(Error::InvalidCommand);
        }
        work.candidate.initialized = true;
        work.candidate.dictionaries = Some(Vec::new());
        work.candidate.file_error = None;
        work.candidate.layer_error = None;
        work.candidate.generation = None;
        work.parts.clear();
        match command.file_state {
            StoredFileState::Missing => {
                if !bytes.is_empty() {
                    return Err(Error::InvalidCommand);
                }
                work.tasks.push_back(ReadTask::Legacy {
                    hash: None,
                    migration: false,
                });
            }
            StoredFileState::Unreadable => work.readonly("unreadable_imported_dictionary_pointer"),
            StoredFileState::Present => match decode_imported_dictionary_pointer(bytes) {
                Ok(hash) => work.tasks.push_back(ReadTask::Generation(hash)),
                Err(error) => work.readonly(error.code),
            },
        }
        let phase = advance(work)?;
        let result = reply(&ticket, &phase)?;
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        edits.pending = Some(Pending { ticket, phase });
        Ok(result)
    }

    pub fn prepare_imported_dictionary(
        &self,
        command: ImportedDictionaryEditCommand,
        bytes: &[u8],
    ) -> Result<PreparedImportedDictionaryReply, Error> {
        let _cleanup = PrivateCleanup(self);
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ticket = edits.reserve(command.revision())?;
        let mut work = current_work(self, command.revision(), Mode::Edit, 0)?;
        if !work.old.management.initialized {
            return Err(Error::Busy);
        }
        if work.old.management.file_error.is_some() {
            return Err(Error::ReadOnly);
        }
        if work.old.management.layer_error.is_some()
            && !matches!(
                command,
                ImportedDictionaryEditCommand::Remove { .. }
                    | ImportedDictionaryEditCommand::SetEnabled { enabled: false, .. }
            )
        {
            return Err(Error::DegradedRepairOnly);
        }
        match command {
            ImportedDictionaryEditCommand::Import {
                source_file_name,
                imported_at_unix_ms,
                enabled,
                ..
            } => {
                if source_file_name.is_empty() || source_file_name.contains(['/', '\0']) {
                    return Err(Error::InvalidCommand);
                }
                let document = decode_import_source(bytes, &source_file_name)?;
                let dictionaries = work.candidate.dictionaries.as_deref().unwrap_or_default();
                let previous = dictionaries
                    .iter()
                    .find(|d| d.manifest.metadata.id == document.metadata.id);
                if previous.is_none() && dictionaries.len() >= 32 {
                    return Err(Error::Layer("too_many_dictionaries"));
                }
                let order = if let Some(previous) = previous {
                    previous.manifest.order
                } else {
                    dictionaries
                        .iter()
                        .map(|d| d.manifest.order)
                        .max()
                        .unwrap_or(-1)
                        .checked_add(1)
                        .ok_or(Error::RevisionOverflow)?
                };
                work.preview = Some(
                    json!({"metadata":document.metadata,"entry_count":document.entries.len(),"replaces_existing":previous.is_some()}),
                );
                work.replaced_id = Some(document.metadata.id.clone());
                let entry = ImportedDictionaryManifestEntry {
                    metadata: document.metadata.clone(),
                    enabled,
                    imported_at_unix_ms,
                    source_sha256: sha256(bytes),
                    blob_sha256: String::new(),
                    order,
                };
                work.accept_document(document, entry, true)?;
            }
            ImportedDictionaryEditCommand::SetEnabled {
                dictionary_id,
                enabled,
                ..
            } => {
                if !bytes.is_empty() {
                    return Err(Error::InvalidCommand);
                }
                // `dictionaries` is None only with a file error, refused above.
                let dictionary = work
                    .candidate
                    .dictionaries
                    .as_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|d| d.manifest.metadata.id == dictionary_id)
                    .ok_or(Error::MissingDictionary)?;
                dictionary.manifest.enabled = enabled;
                if !enabled {
                    work.parts.remove(&dictionary_id);
                }
            }
            ImportedDictionaryEditCommand::Remove { dictionary_id, .. } => {
                if !bytes.is_empty() {
                    return Err(Error::InvalidCommand);
                }
                let dictionaries = work.candidate.dictionaries.as_mut().unwrap();
                let index = dictionaries
                    .iter()
                    .position(|d| d.manifest.metadata.id == dictionary_id)
                    .ok_or(Error::MissingDictionary)?;
                dictionaries.remove(index);
                work.parts.remove(&dictionary_id);
            }
        }
        work.queue_edit_reads();
        let phase = advance(work)?;
        let result = reply(&ticket, &phase)?;
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        edits.pending = Some(Pending { ticket, phase });
        Ok(result)
    }

    fn supply_imported_dictionary(
        &self,
        command: ImportedDictionarySupplyCommand,
        bytes: &[u8],
        loading: bool,
    ) -> Result<PreparedImportedDictionaryReply, Error> {
        let _cleanup = PrivateCleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ticket = command.ticket().clone();
        edits.matching(&ticket)?;
        // From here every error return drops the taken work: the operation
        // ends unpublished and the ticket is released. Blobs the host already
        // wrote stay on disk as orphans until a later successful operation
        // collects them. During a load, a file error instead becomes a
        // read-only result and the load continues to its ready state.
        let pending = edits.pending.take().unwrap();
        let Phase::Working(mut work) = pending.phase else {
            edits.pending = Some(pending);
            return Err(Error::InvalidCommand);
        };
        if (work.mode == Mode::Load) != loading {
            return Err(Error::InvalidCommand);
        }
        match command {
            ImportedDictionarySupplyCommand::Read {
                request_id,
                file_state,
                ..
            } => {
                if work.artifact.is_some() {
                    return Err(Error::InvalidCommand);
                }
                if let Err(error) = work.accept_read(request_id, file_state, bytes) {
                    if loading && matches!(error, Error::File(_)) {
                        work.readonly(error.code());
                    } else {
                        return Err(error);
                    }
                }
            }
            ImportedDictionarySupplyCommand::Written { sha256, .. } => {
                if loading || !bytes.is_empty() {
                    return Err(Error::InvalidCommand);
                }
                let artifact = work.artifact.take().ok_or(Error::InvalidCommand)?;
                if artifact.hash != sha256 {
                    return Err(Error::InvalidCommand);
                }
                work.staged.insert(sha256);
            }
        }
        let phase = advance(*work)?;
        let result = reply(&ticket, &phase)?;
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        edits.pending = Some(Pending { ticket, phase });
        Ok(result)
    }

    pub fn supply_imported_dictionary_load(
        &self,
        command: ImportedDictionarySupplyCommand,
        bytes: &[u8],
    ) -> Result<PreparedImportedDictionaryReply, Error> {
        self.supply_imported_dictionary(command, bytes, true)
    }
    pub fn supply_imported_dictionary_prepare(
        &self,
        command: ImportedDictionarySupplyCommand,
        bytes: &[u8],
    ) -> Result<PreparedImportedDictionaryReply, Error> {
        self.supply_imported_dictionary(command, bytes, false)
    }

    pub fn imported_dictionary_prepared_blob(
        &self,
        ticket: &ImportedDictionaryTicket,
        hash: &str,
    ) -> Result<Vec<u8>, Error> {
        let _cleanup = PrivateCleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        if !is_hash(hash) {
            return Err(Error::InvalidCommand);
        }
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = edits.matching(ticket)?;
        let Phase::Working(work) = &pending.phase else {
            return Err(Error::InvalidCommand);
        };
        let artifact = work
            .artifact
            .as_ref()
            .filter(|a| a.hash == hash)
            .ok_or(Error::InvalidCommand)?;
        Ok(artifact.bytes.clone())
    }

    pub fn publish_imported_dictionary(
        &self,
        ticket: &ImportedDictionaryTicket,
    ) -> Result<(), Error> {
        let _cleanup = PrivateCleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = edits.matching(ticket)?;
        let Phase::Ready(ready) = &pending.phase else {
            return Err(Error::InvalidCommand);
        };
        self.vocabulary.publish_imported_management(
            ticket.expected_revision,
            ready.next.clone(),
            ready.next_overlay.clone(),
            || self.publication_guard(),
            |_| {},
        )?;
        // No fallible work after the swap. Moving an owned phase needs no
        // serialization or allocation, so OK cannot be mistaken for a retry.
        let pending = edits.pending.take().unwrap();
        let Phase::Ready(ready) = pending.phase else {
            unreachable!()
        };
        edits.pending = Some(Pending {
            ticket: pending.ticket,
            phase: Phase::Published(ready),
        });
        Ok(())
    }

    pub fn rollback_imported_dictionary(
        &self,
        ticket: &ImportedDictionaryTicket,
    ) -> Result<(), Error> {
        let _cleanup = PrivateCleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = edits.matching(ticket)?;
        let Phase::Published(ready) = &pending.phase else {
            return Err(Error::InvalidCommand);
        };
        if ready.mode != Mode::Edit {
            return Err(Error::InvalidCommand);
        }
        self.vocabulary.publish_imported_management(
            ready.next.management.revision,
            ready.rollback.clone(),
            ready.old_overlay.clone(),
            || self.publication_guard(),
            |_| {},
        )?;
        let pending = edits.pending.take().unwrap();
        let Phase::Published(ready) = pending.phase else {
            unreachable!()
        };
        edits.pending = Some(Pending {
            ticket: pending.ticket,
            phase: Phase::RolledBack(ready),
        });
        Ok(())
    }

    pub fn finish_imported_dictionary(
        &self,
        command: ImportedDictionaryFinishCommand,
    ) -> Result<(), Error> {
        let _cleanup = PrivateCleanup(self);
        if self.is_invalid() {
            return Err(Error::CoreInvalid);
        }
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = edits.matching(&command.ticket)?;
        let (ready, expected) = match &pending.phase {
            Phase::Published(ready)
                if ready.mode == Mode::Load
                    && command.outcome == ImportedDictionaryFinishKind::Loaded =>
            {
                (ready, &ready.next)
            }
            Phase::Published(ready)
                if ready.mode == Mode::Edit
                    && matches!(
                        command.outcome,
                        ImportedDictionaryFinishKind::NewPointerDurable
                            | ImportedDictionaryFinishKind::NewPointerWarning
                    ) =>
            {
                (ready, &ready.next)
            }
            Phase::RolledBack(ready)
                if command.outcome == ImportedDictionaryFinishKind::RolledBack =>
            {
                (ready, &ready.rollback)
            }
            _ => return Err(Error::InvalidCommand),
        };
        if command.revision != expected.management.revision
            || command.generation_sha256 != expected.management.generation
        {
            return Err(Error::RevisionMismatch);
        }
        let _ = ready;
        let pending = edits.pending.take().unwrap();
        let ready = match pending.phase {
            Phase::Published(ready) | Phase::RolledBack(ready) => ready,
            _ => unreachable!(),
        };
        edits.pending = Some(Pending {
            ticket: pending.ticket,
            // Keep both immutable authorities through the host's GC and UI
            // completion. Only cancel releases the operation reservation.
            phase: Phase::Resolved { _retained: ready },
        });
        Ok(())
    }

    pub fn cancel_imported_dictionary(
        &self,
        ticket: &ImportedDictionaryTicket,
    ) -> Result<(), Error> {
        let mut edits = self
            .imported_dictionary_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = &edits.pending {
            if pending.ticket == *ticket {
                // Published authority must survive until finish or rollback.
                // Nothing else can move the management revision meanwhile:
                // every edit goes through this one reserved slot.
                if matches!(pending.phase, Phase::Published(_)) && !self.is_invalid() {
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

    pub(crate) fn discard_idle_imported_dictionary_preparation(&self) {
        let mut edits = match self.imported_dictionary_edits.try_lock() {
            Ok(edits) => edits,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        edits.pending = None;
    }

    /// Terminal cleanup cannot require a decodable ticket. It is not a
    /// recovery operation and must never discard a live Core's publication.
    pub fn discard_imported_dictionary_preparation(&self) {
        if self.is_invalid() {
            self.imported_dictionary_edits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending = None;
        }
    }
}

pub(crate) fn new_imported_dictionary_edits() -> Mutex<ImportedDictionaryEdits> {
    Mutex::new(Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CoreConfig, ImportedDictionaryManifest, ImportedDictionaryRecord,
        UserDictionaryPartOfSpeech, UserDictionaryPriority,
    };

    #[derive(Default)]
    struct Files {
        bytes: BTreeMap<String, Vec<u8>>,
        legacy: Option<Vec<u8>>,
        reads: Vec<(String, String)>,
        writes: Vec<String>,
    }
    fn core() -> Core {
        Core::new(CoreConfig {
            dictionary_path: None,
        })
        .unwrap()
    }
    fn document(id: &str) -> ImportedDictionaryDocument {
        let mut document =
            decode_imported_dictionary_file(&crate::imported_dictionary_template()).unwrap();
        document.metadata.id = id.into();
        document.entries = vec![ImportedDictionaryRecord {
            reading: "むう".into(),
            surface: "Muu".into(),
            part_of_speech: UserDictionaryPartOfSpeech::noun(),
            priority: UserDictionaryPriority::High,
        }];
        document
    }
    fn drive(
        core: &Core,
        mut reply: PreparedImportedDictionaryReply,
        files: &mut Files,
        load: bool,
    ) -> PreparedImportedDictionaryReply {
        for _ in 0..100 {
            let step: serde_json::Value = serde_json::from_slice(&reply.plan_json).unwrap();
            if step["state"] == "ready" {
                return reply;
            }
            let (command, bytes) = if let Some(hash) = step["write_blob"]["sha256"].as_str() {
                assert!(!load);
                let bytes = core
                    .imported_dictionary_prepared_blob(&reply.ticket, hash)
                    .unwrap();
                assert_eq!(sha256(&bytes), hash);
                files.bytes.insert(hash.into(), bytes);
                files.writes.push(hash.into());
                (
                    ImportedDictionarySupplyCommand::Written {
                        ticket: reply.ticket.clone(),
                        sha256: hash.into(),
                    },
                    Vec::new(),
                )
            } else {
                let request = &step["request"];
                let kind = request["kind"].as_str().unwrap();
                files
                    .reads
                    .push((kind.into(), request["purpose"].as_str().unwrap().into()));
                let bytes = if kind == "legacy_catalog" {
                    files.legacy.clone()
                } else {
                    files
                        .bytes
                        .get(request["sha256"].as_str().unwrap())
                        .cloned()
                };
                (
                    ImportedDictionarySupplyCommand::Read {
                        ticket: reply.ticket.clone(),
                        request_id: request["request_id"].as_u64().unwrap(),
                        file_state: if bytes.is_some() {
                            StoredFileState::Present
                        } else {
                            StoredFileState::Missing
                        },
                    },
                    bytes.unwrap_or_default(),
                )
            };
            reply = if load {
                core.supply_imported_dictionary_load(command, &bytes)
            } else {
                core.supply_imported_dictionary_prepare(command, &bytes)
            }
            .unwrap();
        }
        panic!("finite fixture protocol did not complete");
    }
    fn resolve(core: &Core, reply: PreparedImportedDictionaryReply, files: &mut Files, load: bool) {
        let step: serde_json::Value = serde_json::from_slice(&reply.plan_json).unwrap();
        let plan = &step["ready"];
        let hash = plan["generation_sha256"].as_str().map(str::to_owned);
        if !load {
            files
                .bytes
                .insert(hash.clone().unwrap(), reply.generation_bytes);
        }
        core.publish_imported_dictionary(&reply.ticket).unwrap();
        core.finish_imported_dictionary(ImportedDictionaryFinishCommand {
            ticket: reply.ticket.clone(),
            outcome: if load {
                ImportedDictionaryFinishKind::Loaded
            } else {
                ImportedDictionaryFinishKind::NewPointerDurable
            },
            revision: plan["published_revision"].as_u64().unwrap(),
            generation_sha256: hash,
        })
        .unwrap();
        core.cancel_imported_dictionary(&reply.ticket).unwrap();
    }
    fn empty(core: &Core, files: &mut Files) {
        let reply = core
            .begin_imported_dictionary_load(
                ImportedDictionaryLoadCommand {
                    expected_revision: 0,
                    file_state: StoredFileState::Missing,
                    now_unix_ms: 123,
                },
                &[],
            )
            .unwrap();
        let reply = drive(core, reply, files, true);
        resolve(core, reply, files, true);
        files.reads.clear();
    }
    fn import(
        core: &Core,
        document: &ImportedDictionaryDocument,
    ) -> PreparedImportedDictionaryReply {
        core.prepare_imported_dictionary(
            ImportedDictionaryEditCommand::Import {
                expected_revision: core.imported_dictionary_view().management_revision,
                source_file_name: "fixture.muudict".into(),
                imported_at_unix_ms: 456,
                enabled: true,
            },
            &encode_imported_dictionary_file(document).unwrap(),
        )
        .unwrap()
    }
    fn enabled(core: &Core, id: &str, enabled: bool) -> PreparedImportedDictionaryReply {
        core.prepare_imported_dictionary(
            ImportedDictionaryEditCommand::SetEnabled {
                expected_revision: core.imported_dictionary_view().management_revision,
                dictionary_id: id.into(),
                enabled,
            },
            &[],
        )
        .unwrap()
    }
    fn stored(
        core: &Core,
        docs: &[ImportedDictionaryDocument],
        raw: Option<Vec<u8>>,
        files: &mut Files,
    ) {
        let dictionaries = docs
            .iter()
            .enumerate()
            .map(|(index, doc)| {
                let bytes = raw
                    .clone()
                    .unwrap_or_else(|| encode_imported_dictionary_file(doc).unwrap());
                let hash = sha256(&bytes);
                files.bytes.insert(hash.clone(), bytes);
                ImportedDictionaryManifestEntry {
                    metadata: doc.metadata.clone(),
                    enabled: true,
                    imported_at_unix_ms: 1,
                    source_sha256: hash.clone(),
                    blob_sha256: hash,
                    order: index as i64,
                }
            })
            .collect();
        let bytes = encode_imported_dictionary_manifest(&ImportedDictionaryManifest {
            schema_version: 2,
            dictionaries,
        })
        .unwrap();
        let hash = sha256(&bytes);
        files.bytes.insert(hash.clone(), bytes);
        let reply = core
            .begin_imported_dictionary_load(
                ImportedDictionaryLoadCommand {
                    expected_revision: 0,
                    file_state: StoredFileState::Present,
                    now_unix_ms: 123,
                },
                format!("{hash}\n").as_bytes(),
            )
            .unwrap();
        let reply = drive(core, reply, files, true);
        resolve(core, reply, files, true);
    }

    #[test]
    fn private_steps_publish_once_and_reserve_until_gc_and_ui_completion() {
        let core = core();
        let mut files = Files::default();
        empty(&core, &mut files);
        let reply = import(&core, &document("fixture.a"));
        assert!(
            core.imported_dictionary_view()
                .dictionaries
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            core.prepare_imported_dictionary(
                ImportedDictionaryEditCommand::Remove {
                    expected_revision: 1,
                    dictionary_id: "fixture.a".into()
                },
                &[]
            ),
            Err(Error::Busy)
        ));
        let reply = drive(&core, reply, &mut files, false);
        assert_eq!(files.writes.len(), 1);
        core.publish_imported_dictionary(&reply.ticket).unwrap();
        assert_eq!(
            core.imported_dictionary_view().dictionaries.unwrap().len(),
            1
        );
        assert!(matches!(
            core.cancel_imported_dictionary(&reply.ticket),
            Err(Error::Busy)
        ));
        let generation = sha256(&reply.generation_bytes);
        core.finish_imported_dictionary(ImportedDictionaryFinishCommand {
            ticket: reply.ticket.clone(),
            outcome: ImportedDictionaryFinishKind::NewPointerDurable,
            revision: 2,
            generation_sha256: Some(generation),
        })
        .unwrap();
        assert!(matches!(
            core.prepare_imported_dictionary(
                ImportedDictionaryEditCommand::Remove {
                    expected_revision: 2,
                    dictionary_id: "fixture.a".into()
                },
                &[]
            ),
            Err(Error::Busy)
        ));
        core.cancel_imported_dictionary(&reply.ticket).unwrap();
        core.cancel_imported_dictionary(&reply.ticket).unwrap();
        assert!(matches!(
            core.publish_imported_dictionary(&reply.ticket),
            Err(Error::InvalidTicket)
        ));
    }

    #[test]
    fn disabling_reads_nothing_and_enabling_reads_only_the_changed_dictionary() {
        let core = core();
        let mut files = Files::default();
        stored(&core, &[document("a"), document("b")], None, &mut files);
        assert!(files.writes.is_empty()); // Loading never stages or rewrites a file.
        let original = core.vocabulary.imported_management();
        files.reads.clear();
        let reply = drive(&core, enabled(&core, "a", false), &mut files, false);
        resolve(&core, reply, &mut files, false);
        assert!(files.reads.is_empty());
        assert!(files.writes.is_empty());
        let disabled = core.vocabulary.imported_management();
        assert!(!disabled.parts.contains_key("a"));
        assert!(Arc::ptr_eq(&original.parts["b"], &disabled.parts["b"]));
        let reply = drive(&core, enabled(&core, "a", true), &mut files, false);
        resolve(&core, reply, &mut files, false);
        assert_eq!(files.reads, vec![("blob".into(), "enable".into())]);
        assert!(Arc::ptr_eq(
            &disabled.parts["b"],
            &core.vocabulary.imported_management().parts["b"]
        ));
    }

    #[test]
    fn rollback_restores_imported_content_with_a_new_revision_and_preserves_other_layers() {
        let core = core();
        let mut files = Files::default();
        empty(&core, &mut files);
        let old = core.vocabulary.snapshot().imported.overlay.clone();
        let reply = drive(&core, import(&core, &document("a")), &mut files, false);
        core.publish_imported_dictionary(&reply.ticket).unwrap();
        core.set_user_fixture(crate::UserDictionarySnapshot {
            schema_version: 1,
            entries: vec![],
        })
        .unwrap();
        let user = core.vocabulary.snapshot().user.clone();
        core.rollback_imported_dictionary(&reply.ticket).unwrap();
        assert_eq!(core.imported_dictionary_view().management_revision, 3);
        assert!(
            core.imported_dictionary_view()
                .dictionaries
                .unwrap()
                .is_empty()
        );
        assert!(Arc::ptr_eq(
            &old,
            &core.vocabulary.snapshot().imported.overlay
        ));
        assert!(Arc::ptr_eq(&user, &core.vocabulary.snapshot().user));
        core.finish_imported_dictionary(ImportedDictionaryFinishCommand {
            ticket: reply.ticket.clone(),
            outcome: ImportedDictionaryFinishKind::RolledBack,
            revision: 3,
            generation_sha256: None,
        })
        .unwrap();
        core.cancel_imported_dictionary(&reply.ticket).unwrap();
    }

    // After publish, a new edit is Busy and leaves the revision and the
    // published overlay untouched. Finish still succeeds, new edits stay Busy
    // until cancel, and then one can reserve: the slot is never stuck.
    #[test]
    fn published_edit_rejects_new_edits_until_finish_and_cancel() {
        let core = core();
        let mut files = Files::default();
        empty(&core, &mut files);
        let reply = drive(&core, import(&core, &document("a")), &mut files, false);
        core.publish_imported_dictionary(&reply.ticket).unwrap();
        let published = core.vocabulary.snapshot().imported.overlay.clone();
        let next_edit = || self::ImportedDictionaryEditCommand::SetEnabled {
            expected_revision: core.imported_dictionary_view().management_revision,
            dictionary_id: "a".into(),
            enabled: false,
        };
        assert!(matches!(
            core.prepare_imported_dictionary(next_edit(), &[])
                .map(|_| ()),
            Err(Error::Busy)
        ));
        assert_eq!(core.imported_dictionary_view().management_revision, 2);
        assert!(Arc::ptr_eq(
            &published,
            &core.vocabulary.snapshot().imported.overlay
        ));
        core.finish_imported_dictionary(ImportedDictionaryFinishCommand {
            ticket: reply.ticket.clone(),
            outcome: ImportedDictionaryFinishKind::NewPointerDurable,
            revision: 2,
            generation_sha256: Some(sha256(&reply.generation_bytes)),
        })
        .unwrap();
        assert!(matches!(
            core.prepare_imported_dictionary(next_edit(), &[])
                .map(|_| ()),
            Err(Error::Busy)
        ));
        core.cancel_imported_dictionary(&reply.ticket).unwrap();
        let next = enabled(&core, "a", false);
        core.cancel_imported_dictionary(&next.ticket).unwrap();
        assert!(!core.is_invalid());
    }

    // The same through the rollback exit: rollback succeeds, a new edit is
    // still Busy until the rolled-back finish and cancel, and a new import can
    // then reserve.
    #[test]
    fn published_edit_rejects_new_edits_until_rollback_and_cancel() {
        let core = core();
        let mut files = Files::default();
        empty(&core, &mut files);
        let reply = drive(&core, import(&core, &document("a")), &mut files, false);
        core.publish_imported_dictionary(&reply.ticket).unwrap();
        let next_edit = || self::ImportedDictionaryEditCommand::SetEnabled {
            expected_revision: core.imported_dictionary_view().management_revision,
            dictionary_id: "a".into(),
            enabled: false,
        };
        assert!(matches!(
            core.prepare_imported_dictionary(next_edit(), &[])
                .map(|_| ()),
            Err(Error::Busy)
        ));
        core.rollback_imported_dictionary(&reply.ticket).unwrap();
        assert_eq!(core.imported_dictionary_view().management_revision, 3);
        assert!(matches!(
            core.prepare_imported_dictionary(next_edit(), &[])
                .map(|_| ()),
            Err(Error::Busy)
        ));
        core.finish_imported_dictionary(ImportedDictionaryFinishCommand {
            ticket: reply.ticket.clone(),
            outcome: ImportedDictionaryFinishKind::RolledBack,
            revision: 3,
            generation_sha256: None,
        })
        .unwrap();
        core.cancel_imported_dictionary(&reply.ticket).unwrap();
        let next = drive(&core, import(&core, &document("b")), &mut files, false);
        core.cancel_imported_dictionary(&next.ticket).unwrap();
        assert!(!core.is_invalid());
    }

    #[test]
    fn user_update_after_imported_publication_survives_finish_and_rollback() {
        for rollback in [false, true] {
            let core = core();
            let mut files = Files::default();
            empty(&core, &mut files);
            let old_imported = core.vocabulary.snapshot().imported.overlay.clone();
            let reply = drive(&core, import(&core, &document("a")), &mut files, false);
            core.publish_imported_dictionary(&reply.ticket).unwrap();
            let old_user = core.vocabulary.snapshot().user.clone();
            core.set_user_fixture(crate::test_support::user_dictionary_snapshot([(
                "むう",
                "保持",
                UserDictionaryPriority::High,
            )]))
            .unwrap();
            let user = core.vocabulary.snapshot().user.clone();
            assert!(!Arc::ptr_eq(&old_user, &user));
            // A publication in another layer must not invalidate Imported's
            // revision or let its finish/rollback overwrite the new User layer.
            assert_eq!(core.imported_dictionary_view().management_revision, 2);
            if rollback {
                core.rollback_imported_dictionary(&reply.ticket).unwrap();
                assert!(Arc::ptr_eq(
                    &old_imported,
                    &core.vocabulary.snapshot().imported.overlay
                ));
            }
            core.finish_imported_dictionary(ImportedDictionaryFinishCommand {
                ticket: reply.ticket.clone(),
                outcome: if rollback {
                    ImportedDictionaryFinishKind::RolledBack
                } else {
                    ImportedDictionaryFinishKind::NewPointerDurable
                },
                revision: if rollback { 3 } else { 2 },
                generation_sha256: if rollback {
                    None
                } else {
                    Some(sha256(&reply.generation_bytes))
                },
            })
            .unwrap();
            core.cancel_imported_dictionary(&reply.ticket).unwrap();
            assert!(Arc::ptr_eq(&user, &core.vocabulary.snapshot().user));
            assert!(!core.is_invalid());
        }
    }

    #[test]
    fn uppercase_and_legacy_ha_remain_manageable_but_keep_the_old_empty_runtime() {
        for mut bad in [document("Upper"), document("ha")] {
            if bad.metadata.id == "ha" {
                bad.entries[0].reading = "いう".into();
                bad.entries[0].surface = "言ふ".into();
                bad.entries[0].part_of_speech =
                    UserDictionaryPartOfSpeech::from_id("HA_GROUP1_VERB").unwrap();
            }
            let core = core();
            let mut files = Files::default();
            stored(&core, &[document("good"), bad], None, &mut files);
            let view = core.imported_dictionary_view();
            assert_eq!(view.layer_status, "empty_degraded");
            assert_eq!(view.dictionaries.unwrap().len(), 2);
            assert!(core.vocabulary.imported_management().parts.is_empty());
            assert!(matches!(
                core.prepare_imported_dictionary(
                    ImportedDictionaryEditCommand::SetEnabled {
                        expected_revision: 1,
                        dictionary_id: "good".into(),
                        enabled: true
                    },
                    &[]
                ),
                Err(Error::DegradedRepairOnly)
            ));
            files.reads.clear();
            let id = core.imported_dictionary_view().dictionaries.unwrap()[1]
                .manifest
                .metadata
                .id
                .clone();
            let reply = core
                .prepare_imported_dictionary(
                    ImportedDictionaryEditCommand::Remove {
                        expected_revision: 1,
                        dictionary_id: id,
                    },
                    &[],
                )
                .unwrap();
            let reply = drive(&core, reply, &mut files, false);
            resolve(&core, reply, &mut files, false);
            assert_eq!(
                files.reads,
                vec![("blob".into(), "degraded_recovery".into())]
            );
            assert_eq!(core.imported_dictionary_view().layer_status, "active");
        }
    }

    #[test]
    fn noncanonical_blob_is_read_only_until_the_first_explicit_edit() {
        let core = core();
        let mut files = Files::default();
        let mut doc = document("a");
        doc.entries[0].reading = "か".into();
        let canonical = encode_imported_dictionary_file(&doc).unwrap();
        let raw = String::from_utf8(canonical.clone())
            .unwrap()
            .replace("か\t", "カ\t")
            .into_bytes();
        assert_ne!(raw, canonical);
        stored(&core, &[doc], Some(raw), &mut files);
        assert!(files.writes.is_empty());
        files.reads.clear();
        let reply = drive(&core, enabled(&core, "a", false), &mut files, false);
        resolve(&core, reply, &mut files, false);
        assert_eq!(
            files.reads,
            vec![("blob".into(), "canonicalization".into())]
        );
        assert_eq!(files.writes, vec![sha256(&canonical)]);
        files.reads.clear();
        files.writes.clear();
        let reply = drive(&core, enabled(&core, "a", false), &mut files, false);
        resolve(&core, reply, &mut files, false);
        assert!(files.reads.is_empty());
        assert!(files.writes.is_empty());
    }

    #[test]
    fn legacy_catalog_migrates_only_on_edit_and_streams_one_canonical_blob_at_a_time() {
        let core = core();
        let mut files = Files {legacy: Some(serde_json::to_vec(&json!({"schema_version":1,"dictionaries":[
            {"metadata":document("a").metadata,"enabled":true,"entries":[{"reading":"むう","surface":"Muu","partOfSpeech":"NOUN","priority":"high"}]},
            {"metadata":document("b").metadata,"enabled":true,"entries":[{"reading":"むう","surface":"Muu","partOfSpeech":"NOUN","priority":"high"}]}
        ]})).unwrap()), ..Default::default()};
        let reply = core
            .begin_imported_dictionary_load(
                ImportedDictionaryLoadCommand {
                    expected_revision: 0,
                    file_state: StoredFileState::Missing,
                    now_unix_ms: 123,
                },
                &[],
            )
            .unwrap();
        let reply = drive(&core, reply, &mut files, true);
        resolve(&core, reply, &mut files, true);
        assert!(files.writes.is_empty());
        files.reads.clear();
        let reply = drive(&core, enabled(&core, "a", false), &mut files, false);
        assert_eq!(
            files.reads,
            vec![("legacy_catalog".into(), "migration".into())]
        );
        assert_eq!(files.writes.len(), 2);
        resolve(&core, reply, &mut files, false);
        files.reads.clear();
        files.writes.clear();
        let reply = drive(&core, enabled(&core, "a", true), &mut files, false);
        resolve(&core, reply, &mut files, false);
        assert_eq!(files.reads, vec![("blob".into(), "enable".into())]);
        assert!(files.writes.is_empty());
    }

    #[test]
    fn malformed_files_become_read_only_while_protocol_errors_do_not_publish() {
        for state in [StoredFileState::Present, StoredFileState::Unreadable] {
            let core = core();
            let reply = core
                .begin_imported_dictionary_load(
                    ImportedDictionaryLoadCommand {
                        expected_revision: 0,
                        file_state: state,
                        now_unix_ms: 0,
                    },
                    &[],
                )
                .unwrap();
            let mut files = Files::default();
            resolve(&core, reply, &mut files, true);
            assert_eq!(core.imported_dictionary_view().access, "read_only");
            assert!(files.writes.is_empty());
        }
        let core = core();
        let reply = core
            .begin_imported_dictionary_load(
                ImportedDictionaryLoadCommand {
                    expected_revision: 0,
                    file_state: StoredFileState::Missing,
                    now_unix_ms: 0,
                },
                &[],
            )
            .unwrap();
        assert!(matches!(
            core.supply_imported_dictionary_load(
                ImportedDictionarySupplyCommand::Read {
                    ticket: reply.ticket,
                    request_id: 999,
                    file_state: StoredFileState::Missing
                },
                &[]
            ),
            Err(Error::InvalidCommand)
        ));
        assert_eq!(core.imported_dictionary_view().management_revision, 0);
    }

    #[test]
    fn publication_panic_leaves_a_whole_state_and_invalidates_core_only_in_the_public_interval() {
        for after in [false, true] {
            let core = core();
            let mut files = Files::default();
            empty(&core, &mut files);
            let reply = drive(&core, import(&core, &document("a")), &mut files, false);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut edits = core.imported_dictionary_edits.lock().unwrap();
                let Phase::Ready(ready) = &edits.matching(&reply.ticket).unwrap().phase else {
                    panic!("ready fixture")
                };
                core.vocabulary.publish_imported_management(
                    1,
                    ready.next.clone(),
                    ready.next_overlay.clone(),
                    || core.publication_guard(),
                    |point| {
                        if point == after {
                            panic!("publication fixture")
                        }
                    },
                )
            }));
            assert!(result.is_err());
            assert!(core.is_invalid());
            assert_eq!(
                core.imported_dictionary_view().dictionaries.unwrap().len(),
                usize::from(after)
            );
            assert!(matches!(
                core.cancel_imported_dictionary(&reply.ticket),
                Err(Error::CoreInvalid)
            ));
        }
    }

    #[test]
    fn private_panic_releases_a_poisoned_reservation_without_invalidating_core() {
        let core = core();
        let mut files = Files::default();
        empty(&core, &mut files);
        let _prepared = import(&core, &document("a"));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cleanup = PrivateCleanup(&core);
            let _edits = core.imported_dictionary_edits.lock().unwrap();
            panic!("private fixture");
        }));
        assert!(result.is_err());
        assert!(!core.is_invalid());
        assert!(
            core.imported_dictionary_edits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .is_none()
        );
        let prepared = import(&core, &document("b"));
        core.cancel_imported_dictionary(&prepared.ticket).unwrap();
    }

    #[test]
    fn partial_degraded_repair_keeps_every_summary_and_an_empty_whole_layer() {
        let core = core();
        let mut files = Files::default();
        stored(
            &core,
            &[document("UpperA"), document("UpperB"), document("good")],
            None,
            &mut files,
        );
        files.reads.clear();
        let reply = core
            .prepare_imported_dictionary(
                ImportedDictionaryEditCommand::Remove {
                    expected_revision: 1,
                    dictionary_id: "UpperA".into(),
                },
                &[],
            )
            .unwrap();
        let reply = drive(&core, reply, &mut files, false);
        let manifest = decode_imported_dictionary_manifest(&reply.generation_bytes).unwrap();
        assert_eq!(manifest.dictionaries.len(), 2);
        resolve(&core, reply, &mut files, false);
        assert!(files.reads.is_empty());
        assert_eq!(
            core.imported_dictionary_view().layer_status,
            "empty_degraded"
        );
        assert_eq!(
            core.imported_dictionary_view().dictionaries.unwrap().len(),
            2
        );
        assert!(core.vocabulary.imported_management().parts.is_empty());
        let reply = core
            .prepare_imported_dictionary(
                ImportedDictionaryEditCommand::Remove {
                    expected_revision: 2,
                    dictionary_id: "UpperB".into(),
                },
                &[],
            )
            .unwrap();
        let reply = drive(&core, reply, &mut files, false);
        resolve(&core, reply, &mut files, false);
        assert_eq!(
            files.reads,
            vec![("blob".into(), "degraded_recovery".into())]
        );
        assert_eq!(core.imported_dictionary_view().layer_status, "active");
    }

    #[test]
    fn catalog_and_migration_match_legacy_foundation_bytes_and_declared_int64_id_changes() {
        // Captured from the legacy Swift store in isolated fixture directories.
        // Six deliberate acceptance changes: three exact-Int64 boundary cases
        // and leading -/./_ IDs. All other accepted canonical bytes are Swift's.
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../tests/data/imported-dictionary-catalog-compat.json"
        ))
        .unwrap();
        assert_eq!(cases.len(), 51);
        for case in cases {
            let core = core();
            let mut files = Files::default();
            let data = base64::decode(case["data_base64"].as_str().unwrap()).unwrap();
            for (hash, bytes) in case["blobs"].as_object().unwrap() {
                files.bytes.insert(
                    hash.clone(),
                    base64::decode(bytes.as_str().unwrap()).unwrap(),
                );
            }
            let (state, pointer) = if case["kind"] == "legacy" {
                files.legacy = Some(data);
                (StoredFileState::Missing, Vec::new())
            } else {
                let hash = case["generation_sha256"].as_str().unwrap();
                files.bytes.insert(hash.into(), data);
                (StoredFileState::Present, format!("{hash}\n").into_bytes())
            };
            let reply = core
                .begin_imported_dictionary_load(
                    ImportedDictionaryLoadCommand {
                        expected_revision: 0,
                        file_state: state,
                        now_unix_ms: case["now_unix_ms"].as_i64().unwrap_or(123),
                    },
                    &pointer,
                )
                .unwrap();
            let reply = drive(&core, reply, &mut files, true);
            resolve(&core, reply, &mut files, true);
            let view = core.imported_dictionary_view();
            assert_eq!(
                view.access == "editable",
                case["expected"]["accepted"].as_bool().unwrap(),
                "{}",
                case["id"]
            );
            assert!(
                files.writes.is_empty(),
                "reading never migrates {}",
                case["id"]
            );
            if let Some(dictionaries) = view.dictionaries {
                assert_eq!(
                    dictionaries.len(),
                    case["expected"]["dictionary_count"].as_u64().unwrap() as usize,
                    "{}",
                    case["id"]
                );
                assert_eq!(
                    dictionaries.iter().map(|d| d.entry_count).sum::<usize>(),
                    case["expected"]["entry_count"].as_u64().unwrap() as usize,
                    "{}",
                    case["id"]
                );
                if let Some(first) = dictionaries.first() {
                    let reply = drive(
                        &core,
                        enabled(&core, &first.manifest.metadata.id, false),
                        &mut files,
                        false,
                    );
                    assert_eq!(
                        base64::encode(&reply.generation_bytes),
                        case["expected"]["canonical_base64"].as_str().unwrap(),
                        "{}",
                        case["id"]
                    );
                    core.cancel_imported_dictionary(&reply.ticket).unwrap();
                }
            }
        }
    }
}
