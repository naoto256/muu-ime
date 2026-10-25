//! Immutable conversion resources, prepared once and shared by sessions.
//!
//! `Core` is the process-wide half of docs/src/internals/architecture.md「Core と Session」: the
//! dictionary and tables never change, while the vocabulary, the learning
//! store and the record queue are replaced or appended behind short locks.
//! Every shared update is "build, then swap" — a new value is completed
//! privately and published with one assignment — so a failure or a panic at
//! any point leaves either the complete old value or the complete new one.
//!
//! No lock in this module is held across search, inference, overlay
//! expansion or serialization. Vocabulary publication only combines immutable
//! layers; learning additionally checks the generation and builds its next
//! private model under the same lock so a clear cannot interleave (docs/src/internals/conversion.md「学習」).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use muu_conversion::{BoundaryDecision, CostSnapshot, LexiconOverlay, SegmentRule, Segmenter};
use muu_dict::{CoarsePos, Dictionary};
use muu_romaji::RomajiTable;

#[cfg(test)]
use crate::UserDictionarySnapshot;
use crate::keymap::Keymap;
use crate::learning::{LearningSnapshot, LearningStore};
use crate::personal_frequency::PersonalFrequencyModel;
use crate::{PersonalFrequencySnapshot, VocabularySnapshot};
use crate::{SessionError, SharedVocabulary};

/// The immutable resources a `Core` opens. A path that is set but cannot be
/// opened degrades to kana-only instead of failing construction; `None` means
/// no dictionary was requested at all.
#[derive(Default, Clone, Copy)]
pub struct CoreConfig<'a> {
    pub dictionary_path: Option<&'a Path>,
}

/// Process settings selected before publishing the core. Model path and
/// loading mode are fixed resources (docs/src/internals/conversion.md「言語モデルの読み込み」); Session creation never loads it.
#[derive(Clone, Default)]
pub struct CoreSessionSettings {
    pub conversion_lm_dir: Option<PathBuf>,
    /// None preserves the legacy explicit-edge API. The app passes its choice.
    pub conversion_lm_model: Option<muu_lm::ModelKind>,
    /// Only a zenz request may fall back to this edge directory.
    pub conversion_lm_fallback_dir: Option<PathBuf>,
    pub conversion_lm_load_synchronously: bool,
    pub commit_stream_config: Option<crate::CommitStreamConfig>,
}

/// Core's LM lifecycle (docs/src/internals/conversion.md「言語モデルの読み込み」). Failure is final for this Core; conversion
/// must never turn an unavailable state into a synchronous load or retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LanguageModelState {
    Unloaded = 0,
    Loading = 1,
    Available = 2,
    Failed = 3,
}

/// Facts for the host's on-demand status display, separate from a Session's
/// conversion view. Reading them never starts a load, drains a queue, or
/// touches vocabulary/learning. Keep user data and resource paths out of it.
// Serialized as schema_version 2. Swift's CoreRuntimeStatus accepts 1, or 2
// only together with `language_model_details`; a new field is additive, a
// changed meaning needs a new schema version.
#[derive(serde::Serialize)]
pub struct CoreRuntimeStatus {
    schema_version: u32,
    system_dictionary: &'static str,
    language_model: &'static str,
    commit_stream_enabled: bool,
    language_model_details: LanguageModelInfo,
}

/// Captured resource facts, with no paths, error strings, or user text. Both
/// About and v5 use the same result of Core's one load/fallback attempt.
#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct LanguageModelInfo {
    pub requested_model: Option<&'static str>,
    pub actual_model: Option<&'static str>,
    pub route: &'static str,
    pub fallback_reason: Option<&'static str>,
    pub unavailable_reason: Option<&'static str>,
}
impl Default for LanguageModelInfo {
    fn default() -> Self {
        Self {
            requested_model: None,
            actual_model: None,
            route: "disabled",
            fallback_reason: None,
            unavailable_reason: None,
        }
    }
}

/// One nonblocking view of LM availability. The immutable model is present
/// only in Available; no loader lock is exposed to search or inference.
#[derive(Clone)]
pub struct LanguageModelSnapshot {
    pub state: LanguageModelState,
    pub(crate) model: Option<Arc<muu_lm::ConversionLm>>,
    pub info: LanguageModelInfo,
}

// Kept behind its own `Arc`, apart from `Core`, because the detached loader
// thread must hold it. Holding the Core instead would keep the dictionary and
// every store alive until a slow load finished, even after the last Session
// and the registry had let go.
//
// Invariant: `state == Available` implies `model` is set. The loader sets the
// model first and the state second, and nothing ever unsets either. The
// opposite window (model set, state still Loading) is reported as Loading;
// that conversion simply runs without selection.
#[derive(Default)]
struct LanguageModelRuntime {
    state: AtomicU8,
    model: OnceLock<Arc<muu_lm::ConversionLm>>,
    requested: OnceLock<muu_lm::ModelKind>,
    details: OnceLock<LanguageModelInfo>,
}

impl LanguageModelRuntime {
    fn snapshot(&self) -> LanguageModelSnapshot {
        // Release on publication pairs with this acquire. Model publication
        // precedes Available, so conversion never sees a half-loaded model.
        let state = match self.state.load(Ordering::Acquire) {
            0 => LanguageModelState::Unloaded,
            1 => LanguageModelState::Loading,
            2 => LanguageModelState::Available,
            _ => LanguageModelState::Failed,
        };
        let model = (state == LanguageModelState::Available)
            .then(|| self.model.get().cloned())
            .flatten();
        // Never expose preparation's details while state is still Loading.
        // Final details and model are both published before the release store.
        let info = match state {
            LanguageModelState::Unloaded => LanguageModelInfo::default(),
            LanguageModelState::Loading => LanguageModelInfo {
                requested_model: self.requested.get().map(|m| m.as_str()),
                route: "loading",
                ..LanguageModelInfo::default()
            },
            _ => self
                .details
                .get()
                .cloned()
                .unwrap_or_else(|| LanguageModelInfo {
                    requested_model: self
                        .requested
                        .get()
                        .map(|m| m.as_str())
                        .or_else(|| model.as_ref().map(|m| m.kind().as_str())),
                    actual_model: model.as_ref().map(|m| m.kind().as_str()),
                    route: if state == LanguageModelState::Available {
                        "requested"
                    } else {
                        "unavailable"
                    },
                    unavailable_reason: (state == LanguageModelState::Failed)
                        .then_some("load_failed"),
                    ..LanguageModelInfo::default()
                }),
        };
        LanguageModelSnapshot { state, model, info }
    }

    // Runs on the loader. Only a zenz request may fall back, and only to the
    // edge directory; the reasons are fixed codes ("not_found" when files are
    // missing, "load_failed" when they do not open), never paths or messages.
    // The details are stored before the caller publishes the final state.
    fn load_configured(
        &self,
        requested: muu_lm::ModelKind,
        directory: &Path,
        fallback: Option<&Path>,
    ) -> Option<Arc<muu_lm::ConversionLm>> {
        fn open(
            kind: muu_lm::ModelKind,
            directory: Option<&Path>,
        ) -> Result<Arc<muu_lm::ConversionLm>, &'static str> {
            let directory = directory
                .filter(|p| muu_lm::ConversionLm::files_present(kind, p))
                .ok_or("not_found")?;
            muu_lm::ConversionLm::open_for(kind, directory)
                .map(Arc::new)
                .map_err(|_| "load_failed")
        }
        let mut info = LanguageModelInfo {
            requested_model: Some(requested.as_str()),
            route: "requested",
            ..LanguageModelInfo::default()
        };
        let result = match open(requested, Some(directory)) {
            Ok(model) => Ok(model),
            Err(reason) if requested == muu_lm::ModelKind::Zenz => {
                info.route = "fallback";
                info.fallback_reason = Some(reason);
                open(muu_lm::ModelKind::Edge, fallback)
            }
            Err(reason) => Err(reason),
        };
        let model = match result {
            Ok(model) => {
                info.actual_model = Some(model.kind().as_str());
                Some(model)
            }
            Err(reason) => {
                info.route = "unavailable";
                info.unavailable_reason = Some(reason);
                None
            }
        };
        let _ = self.details.set(info);
        model
    }

    // Starts the one load this Core will ever attempt. The exchange from
    // Unloaded admits a single caller; Failed is not Unloaded, so a failed
    // load is never retried (docs/src/internals/conversion.md「言語モデルの読み込み」).
    //
    // Every way out of `run` stores a final state. If the state could stay
    // Loading after the loader was gone, conversions would report "loading"
    // for the rest of the process with nothing left to finish it.
    //
    // With `synchronous`, `run` executes on the caller and no thread is
    // created, so the state is final when this returns. Tests and the
    // equivalence harness depend on that to pin the state.
    fn start_with(
        self: &Arc<Self>,
        synchronous: bool,
        load: impl FnOnce() -> Option<Arc<muu_lm::ConversionLm>> + Send + 'static,
    ) {
        if self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let runtime = self.clone();
        let run = move || {
            // Loading is private preparation, not a shared publication
            // section (docs/src/internals/architecture.md「失敗したとき」). Its panic means Failed, never Core invalid.
            let model = catch_unwind(AssertUnwindSafe(load)).ok().flatten();
            let state = match model {
                Some(model) => {
                    let _ = runtime.model.set(model);
                    LanguageModelState::Available
                }
                None => LanguageModelState::Failed,
            };
            runtime.state.store(state as u8, Ordering::Release);
        };
        if synchronous {
            run();
        } else if std::thread::Builder::new()
            .name("muu-conversion-lm".to_owned())
            .spawn(run)
            .is_err()
        {
            // The closure was dropped unrun, so nobody else will leave
            // Loading on its behalf.
            self.state
                .store(LanguageModelState::Failed as u8, Ordering::Release);
        }
    }
}

/// Rust callers may create multiple cores. The process-wide lifetime policy
/// belongs to the FFI layer, rather than to this resource value.
pub struct Core {
    pub(crate) dictionary: Option<Arc<Dictionary>>,
    pub(crate) romaji_table: Arc<RomajiTable>,
    pub(crate) keymap: Keymap,
    pub(crate) segmenter: Segmenter,
    pub(crate) degraded_kana_only: bool,
    pub(crate) vocabulary: SharedVocabulary,
    learning: LearningStore,
    pub(crate) user_dictionary_edits: Mutex<crate::user_dictionary_edit::UserDictionaryEdits>,
    pub(crate) imported_dictionary_edits:
        Mutex<crate::imported_dictionary_edit::ImportedDictionaryEdits>,
    pub(crate) postal_code_edits: Mutex<crate::postal_code_edit::PostalCodeEdits>,
    // Startup data is applied at most once; see `initialize_data`.
    data_initialized: Mutex<bool>,
    session_settings: OnceLock<CoreSessionSettings>,
    language_model: Arc<LanguageModelRuntime>,
    // Observe the one-read-per-conversion contract without timing the loader.
    #[cfg(test)]
    pub(crate) language_model_reads: std::sync::atomic::AtomicUsize,
    // Source of the per-session nonce written into records; unique within
    // this Core.
    session_sequence: Mutex<u64>,
    // Completed records live here rather than in the Session so that a record
    // survives the destruction of the input client that committed it, and so
    // that Swift drains one queue instead of one per client. Bounded in count
    // and bytes: a host that stops draining costs records, not memory (docs/src/internals/architecture.md「Core と Session」).
    completed_records: Mutex<crate::commit_stream::RecordQueue>,
    // Terminal. Set only by a `PublicationGuard` dropped during unwinding and
    // never cleared: recovery is a process restart (docs/src/internals/architecture.md「失敗したとき」).
    invalid: AtomicBool,
}

/// One conversion's complete immutable inputs. Each component is retained by
/// reference count; no store lock survives capture or cost construction.
///
/// A Session takes one when a conversion starts and keeps it until that
/// conversion is committed or cancelled (docs/src/internals/conversion.md「変換が使うデータを固定する」). Segment
/// moves, resizes and candidate lists must all be computed from the retained
/// value. Reading the store again mid-conversion would combine segments built
/// from an older catalog with segments built from a newer one. Holding it does
/// not block a replacement; the superseded layers are freed when the last
/// conversion that captured them lets go.
#[derive(Clone)]
pub struct ConversionSnapshot {
    pub(crate) dictionary: Option<Arc<Dictionary>>,
    pub(crate) vocabulary: Arc<VocabularySnapshot>,
    pub(crate) learning: Arc<LearningSnapshot>,
}

impl ConversionSnapshot {
    /// `None` only when the Core has no dictionary (kana-only). The learning
    /// adjustments were computed when the learning state was published, so
    /// this takes no lock.
    pub fn costs(&self) -> Option<muu_conversion::CostSnapshot<'_>> {
        self.dictionary.as_deref().map(|dictionary| {
            crate::conversion_snapshot(
                dictionary,
                &self.vocabulary.user.overlay,
                &self.vocabulary.imported.overlay,
                &self.vocabulary.postal.overlay,
                &self.learning.frequency,
            )
        })
    }
}

/// Marks the Core invalid if a panic unwinds while it is alive. It is the
/// boundary between the two failure units of docs/src/internals/architecture.md「失敗したとき」: a
/// panic that passes through a live guard is a Core failure, and any other
/// panic leaves the flag clear, which the FFI layer reads to tell "this
/// session is invalid" from "the core is invalid". The published data is a
/// complete value either way (build, then swap); what is no longer known is
/// whether the caller's update took effect.
///
/// Two rules for callers. Bind it to a name (`let _publication = ...`);
/// `let _ = ...` drops it at once and guards nothing. And never create one in
/// code that can run during unwinding, such as a `Drop` impl: `panicking()`
/// would be true for an unrelated panic and a clean publication would
/// condemn the Core.
pub(crate) struct PublicationGuard<'a>(&'a Core);

impl Drop for PublicationGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.invalidate();
        }
    }
}

impl Core {
    /// Whether startup learning can be checked against its dictionary authority.
    /// A missing dictionary must not turn a recoverable resource failure into
    /// destructive rejection of the host's saved learning (docs/src/internals/storage.md「変換学習の保存」).
    pub fn has_dictionary(&self) -> bool {
        self.dictionary.is_some()
    }

    /// Matches the degraded flag in ViewState: only a requested dictionary
    /// that could not be opened counts as dictionary degradation.
    pub fn degraded_kana_only(&self) -> bool {
        self.degraded_kana_only
    }

    /// Marks a shared publication failure as terminal for every input client.
    pub fn invalidate(&self) {
        self.invalid.store(true, Ordering::Release);
        // Do not wait on preparation or recursively take its private mutex
        // while unwinding publication. The publishing local owns its ticket;
        // other in-flight preparation checks this flag before reserving one.
        self.discard_idle_user_dictionary_preparation();
        self.discard_idle_imported_dictionary_preparation();
        self.discard_idle_postal_code_preparation();
    }

    pub fn is_invalid(&self) -> bool {
        self.invalid.load(Ordering::Acquire)
    }

    /// Enter only after taking a shared publication lock (docs/src/internals/architecture.md「失敗したとき」). Preparation
    /// and serialization cannot corrupt published data, so their panics must
    /// not make every input client permanently unavailable.
    pub(crate) fn publication_guard(&self) -> PublicationGuard<'_> {
        PublicationGuard(self)
    }

    /// Fixes the per-process session settings. Call once, before the Core is
    /// shared; a second call is refused and returns the rejected settings.
    /// Sessions created before this see the defaults (no LM, no recording).
    pub fn configure_sessions(
        &self,
        settings: CoreSessionSettings,
    ) -> Result<(), Box<CoreSessionSettings>> {
        let directory = settings.conversion_lm_dir.clone();
        let synchronous = settings.conversion_lm_load_synchronously;
        let requested = settings
            .conversion_lm_model
            .unwrap_or(muu_lm::ModelKind::Edge);
        let fallback = settings.conversion_lm_fallback_dir.clone();
        self.session_settings.set(settings).map_err(Box::new)?;
        // Without a directory no load is ever started and the state stays
        // Unloaded for the life of this Core: "no model configured" and "not
        // started yet" are the same state to a conversion.
        if let Some(directory) = directory {
            // This Core owns the one load attempt and the resulting Arc (docs/src/internals/conversion.md「言語モデルの読み込み」).
            // A second per-directory cache would obscure failure ownership;
            // only the loader performs IO, never a conversion/state read.
            let _ = self.language_model.requested.set(requested);
            let runtime = self.language_model.clone();
            self.language_model.start_with(synchronous, move || {
                runtime.load_configured(requested, &directory, fallback.as_deref())
            });
        }
        Ok(())
    }

    /// Creation must stay cheap: it opens no file, hashes nothing and parses
    /// no table (docs/src/internals/architecture.md「Core と Session」). The host may keep any number of input clients alive.
    /// The vocabulary and learning are not read here either; a Session takes
    /// them per conversion through `conversion_snapshot`.
    pub fn create_session(self: &Arc<Self>) -> Result<crate::Session, SessionError> {
        let settings = self.session_settings.get().cloned().unwrap_or_default();
        let sequence = {
            let mut sequence = self
                .session_sequence
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *sequence = sequence.checked_add(1).expect("session nonce overflow");
            *sequence
        };
        crate::Session::with_settings(
            self.clone(),
            settings.commit_stream_config,
            format!("rust-session-{sequence:016x}"),
        )
    }

    /// Capture availability without waiting for loader IO (docs/src/internals/conversion.md「言語モデルの読み込み」). A model
    /// that completes during one selection is used by the next conversion.
    pub fn language_model(&self) -> LanguageModelSnapshot {
        #[cfg(test)]
        self.language_model_reads.fetch_add(1, Ordering::Relaxed);
        self.language_model.snapshot()
    }

    pub fn runtime_status(&self) -> CoreRuntimeStatus {
        // `kana_only` means no dictionary is held. This is wider than
        // degraded_kana_only(): a Core that never asked for a dictionary
        // also reports it. That happens in the app if the bundle lacks
        // mozc-oss.muudict, because then no path is passed.
        let snapshot = self.language_model.snapshot();
        CoreRuntimeStatus {
            schema_version: 2,
            system_dictionary: if self.has_dictionary() {
                "available"
            } else {
                "kana_only"
            },
            language_model: match snapshot.state {
                LanguageModelState::Unloaded => "unloaded",
                LanguageModelState::Loading => "loading",
                LanguageModelState::Available => "available",
                LanguageModelState::Failed => "failed",
            },
            commit_stream_enabled: self
                .session_settings
                .get()
                .is_some_and(|settings| settings.commit_stream_config.is_some()),
            language_model_details: snapshot.info,
        }
    }

    /// Removes the oldest completed record. Popping
    /// cannot leave the queue half-updated.
    pub fn take_commit_stream_event(&self) -> Option<Vec<u8>> {
        self.completed_records
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
    }

    /// Prepare both host outputs while the front is still owned by Core. A
    /// preparation error/panic leaves it queued. The bounded copy holds only
    /// the record lock; it never holds vocabulary, learning, or Session locks.
    pub fn take_commit_stream_event_prepared<T, E>(
        &self,
        prepare: impl FnOnce(&[u8], u64) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        let mut records = self
            .completed_records
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(front) = records.front() else {
            return Ok(None);
        };
        // `prepare` runs before the publication guard. Its panic poisons only
        // this mutex, which is recovered above, so Core stays valid.
        let prepared = prepare(&front.bytes, front.drop_weight)?;
        let _publication = self.publication_guard();
        records.pop_front();
        Ok(Some(prepared))
    }

    /// ACK-only publication: after acceptance there is no fallible reply to
    /// prepare. Saturation matches v5 and returning debt never advances seq.
    pub fn report_commit_stream_dropped(&self, weight: u64) {
        let mut records = self
            .completed_records
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _publication = self.publication_guard();
        records.report_dropped(weight);
    }

    pub(crate) fn publish_commit_stream_events(
        &self,
        events: impl IntoIterator<Item = crate::commit_stream::PreparedRecord>,
    ) {
        // Collect before locking: an iterator panic has not touched shared
        // state and cannot be a terminal Core failure (docs/src/internals/architecture.md「失敗したとき」).
        let events = events.into_iter().collect::<Vec<_>>();
        let mut records = self
            .completed_records
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _publication = self.publication_guard();
        for event in events {
            records.push(event);
        }
    }

    /// The current `(generation, revision)`, i.e. what the next conversion
    /// would capture.
    pub fn learning_version(&self) -> crate::LearningVersion {
        self.learning.version()
    }

    /// Persistence query (docs/src/internals/storage.md「変換学習の保存」): returns the current bytes and version when
    /// they are newer than `received`, and unconditionally for `None`. It
    /// changes nothing, so the same argument yields the same answer and a
    /// caller may ask after every keystroke. The Core keeps no "saved" mark;
    /// whether the bytes reached disk is the caller's bookkeeping.
    pub fn personal_frequency_update(
        &self,
        received: Option<crate::LearningVersion>,
    ) -> Result<Option<crate::PersonalFrequencyUpdate>, serde_json::Error> {
        self.learning.update(received)
    }

    /// Empties learning and starts a new generation, returning its version.
    /// Conversions that captured the previous generation still rank with their
    /// captured costs, but their observations are dropped at commit.
    pub fn clear_personal_frequency(&self) -> crate::LearningVersion {
        self.learning.clear_guarded(|| self.publication_guard())
    }

    // Fails only on the embedded romaji table or keymap; a dictionary that
    // does not open degrades instead. The candidate presentation tables are
    // prepared here, once, only when there is something to convert.
    pub fn new(config: CoreConfig<'_>) -> Result<Self, SessionError> {
        let core = Self::with_vocabulary(config, SharedVocabulary::default())?;
        if core.has_dictionary() {
            crate::candidate_presentation::prepare();
        }
        Ok(core)
    }

    pub fn vocabulary(&self) -> &SharedVocabulary {
        &self.vocabulary
    }

    /// Startup data is accepted before Core publication. FFI initialization
    /// validates the opaque file here; rejection leaves usable read-only
    /// management with an empty active layer.
    ///
    /// Applied at most once; later calls return `Ok` without reading their
    /// arguments, because by then sessions may have observed selections that a
    /// reload would overwrite. The learning model is validated before anything
    /// is published, so a rejected call leaves both stores untouched and can
    /// be retried with empty learning. No publication guard is needed: the FFI
    /// layer calls it before the Core is published and reports a panic here as
    /// a failed initialization.
    pub fn initialize_data(
        &self,
        user_state: crate::StoredFileState,
        user_file: &[u8],
        learning: &PersonalFrequencySnapshot,
    ) -> Result<(), SessionError> {
        let mut initialized = self
            .data_initialized
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *initialized {
            return Ok(());
        }
        let model = PersonalFrequencyModel::new(
            learning.clone(),
            self.dictionary.as_ref().map(|d| d.fingerprint()),
        )?;
        self.initialize_user_dictionary_file(user_state, user_file);
        self.learning.initialize(model);
        *initialized = true;
        Ok(())
    }

    /// Captures the inputs for one conversion. Two short locks are taken in
    /// turn, each only to clone an `Arc`. The vocabulary triple is one
    /// published value and so is internally consistent; vocabulary and
    /// learning are independent stores and need no common instant.
    pub fn conversion_snapshot(&self) -> ConversionSnapshot {
        ConversionSnapshot {
            dictionary: self.dictionary.clone(),
            vocabulary: self.vocabulary.snapshot(),
            learning: self.learning.capture(),
        }
    }

    /// Adds one commit's explicitly selected dictionary tokens to the current
    /// learning state. `generation` must be the one the committing conversion
    /// captured; if learning was cleared since, the observation is dropped and
    /// this returns `false` (docs/src/internals/conversion.md「学習」). Overlay and unknown tokens are not passed
    /// here: only system-dictionary token ids are stable across revisions.
    pub fn observe_selection(
        &self,
        generation: u64,
        tokens: &[muu_dict::DictionaryTokenId],
    ) -> bool {
        self.observe_choices(generation, tokens, &[])
    }

    /// Publish one commit's positive and rejected tokens under one generation
    /// check (docs/src/internals/conversion.md「学習」). Splitting the updates would permit a clear or another
    /// Session's commit between them and advance the decay clock twice.
    pub(crate) fn observe_choices(
        &self,
        generation: u64,
        tokens: &[muu_dict::DictionaryTokenId],
        rejected: &[muu_dict::DictionaryTokenId],
    ) -> bool {
        self.dictionary.as_ref().is_some_and(|dictionary| {
            self.learning.observe_guarded(
                generation,
                tokens,
                rejected,
                dictionary.fingerprint(),
                |_| {},
                || self.publication_guard(),
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn publish_user_dictionary_panicking(
        &self,
        snapshot: UserDictionarySnapshot,
        after: bool,
    ) {
        let expected = self.vocabulary.user_management().revision;
        let (management, source, overlay) =
            crate::user_dictionary_file::UserDictionaryManagement::from_snapshot(
                snapshot,
                expected + 1,
            );
        self.vocabulary
            .publish_user_management(
                expected,
                Arc::new(management),
                Arc::new(source),
                overlay,
                || self.publication_guard(),
                |published| {
                    if published == after {
                        panic!("publication fixture");
                    }
                },
            )
            .unwrap();
    }

    #[cfg(test)]
    pub(crate) fn observe_selection_panicking(
        &self,
        generation: u64,
        tokens: &[muu_dict::DictionaryTokenId],
        after: bool,
    ) {
        self.learning.observe_panicking(
            generation,
            tokens,
            self.dictionary.as_ref().unwrap().fingerprint(),
            after,
            || self.publication_guard(),
        );
    }

    /// Rust fixtures can prepare several ordinary cores with separate stores.
    pub fn with_vocabulary(
        config: CoreConfig<'_>,
        vocabulary: SharedVocabulary,
    ) -> Result<Self, SessionError> {
        let dictionary = config
            .dictionary_path
            .and_then(|path| Dictionary::open(path).ok())
            .map(Arc::new);
        Ok(Self {
            session_settings: OnceLock::new(),
            language_model: Arc::new(LanguageModelRuntime::default()),
            #[cfg(test)]
            language_model_reads: std::sync::atomic::AtomicUsize::new(0),
            session_sequence: Mutex::new(0),
            completed_records: Mutex::new(Default::default()),
            invalid: AtomicBool::new(false),
            degraded_kana_only: config.dictionary_path.is_some() && dictionary.is_none(),
            dictionary,
            romaji_table: Arc::new(RomajiTable::default_muu()?),
            keymap: Keymap::default_muu()?,
            segmenter: default_segmenter(),
            vocabulary,
            learning: LearningStore::default(),
            user_dictionary_edits: crate::user_dictionary_edit::new_user_dictionary_edits(),
            imported_dictionary_edits:
                crate::imported_dictionary_edit::new_imported_dictionary_edits(),
            postal_code_edits: crate::postal_code_edit::new_postal_code_edits(),
            data_initialized: Mutex::new(false),
        })
    }
}

// Every boundary not matched by a rule breaks.
pub fn default_segmenter() -> Segmenter {
    // POS ids are format-frozen by dict-src/mozc-pos-map.toml. A content word
    // followed by an auxiliary verb or particle forms one initial bunsetsu.
    Segmenter::new(
        vec![
            SegmentRule {
                previous_right_pos: None,
                next_left_pos: Some(CoarsePos::AuxiliaryVerb.id()),
                previous_surface_suffix: None,
                next_surface_prefix: None,
                decision: BoundaryDecision::Join,
            },
            SegmentRule {
                previous_right_pos: None,
                next_left_pos: Some(CoarsePos::Particle.id()),
                previous_surface_suffix: None,
                next_surface_prefix: None,
                decision: BoundaryDecision::Join,
            },
        ],
        BoundaryDecision::Break,
    )
}

pub(crate) fn conversion_snapshot<'dictionary>(
    dictionary: &'dictionary Dictionary,
    user_dictionary_overlay: &'dictionary LexiconOverlay,
    imported_dictionary_overlay: &'dictionary LexiconOverlay,
    postal_code_overlay: &'dictionary LexiconOverlay,
    frequency: &dyn muu_conversion::CostProvider,
) -> CostSnapshot<'dictionary> {
    CostSnapshot::capture_with_overlay_catalogs(
        dictionary,
        (!user_dictionary_overlay.is_empty()).then_some(user_dictionary_overlay),
        (!imported_dictionary_overlay.is_empty()).then_some(imported_dictionary_overlay),
        (!postal_code_overlay.is_empty()).then_some(postal_code_overlay),
        frequency,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_status_reports_four_states_without_conversion_reads_or_loads() {
        let core = Core::new(CoreConfig::default()).unwrap();
        for (state, name) in [
            (LanguageModelState::Unloaded, "unloaded"),
            (LanguageModelState::Loading, "loading"),
            (LanguageModelState::Available, "available"),
            (LanguageModelState::Failed, "failed"),
        ] {
            // Inject the published lifecycle fact, without loading a model.
            core.language_model
                .state
                .store(state as u8, Ordering::Release);
            let status = serde_json::to_value(core.runtime_status()).unwrap();
            assert_eq!(
                status,
                serde_json::json!({
                    "schema_version": 2, "system_dictionary": "kana_only",
                    "language_model": name, "commit_stream_enabled": false,
                    "language_model_details": core.language_model.snapshot().info
                })
            );
            assert_eq!(
                core.language_model.state.load(Ordering::Acquire),
                state as u8
            );
            assert!(core.language_model.model.get().is_none());
            assert_eq!(core.language_model_reads.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn production_background_start_eventually_publishes_failure() {
        let core = Core::new(CoreConfig {
            dictionary_path: None,
        })
        .unwrap();
        // Keep coverage of start_with's own production thread dispatch; the
        // barrier tests own their threads to join the common publication body.
        core.language_model.start_with(false, || None);
        // This ten-minute watchdog only bounds a hung test. It measures no
        // responsiveness: the barrier tests establish that without a clock.
        let watchdog = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while core.language_model().state == LanguageModelState::Loading {
            assert!(
                std::time::Instant::now() < watchdog,
                "background loader hung"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(core.language_model().state, LanguageModelState::Failed);
        assert!(!core.is_invalid());
    }

    #[test]
    #[ignore = "requires public dictionary, edge (check-assets)"]
    fn background_completion_changes_the_next_conversion_and_reuses_one_model() {
        let (Some(dictionary), Some(directory)) = (
            crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH"),
            crate::real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
        ) else {
            return;
        };
        let core = Arc::new(
            Core::new(CoreConfig {
                dictionary_path: Some(&dictionary),
            })
            .unwrap(),
        );
        assert_eq!(core.language_model().state, LanguageModelState::Unloaded);
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let entered_loader = entered.clone();
        let release_loader = release.clone();
        // Run the same load-and-publish body on a test-owned background
        // thread. Joining it observes terminal-state publication, whereas a
        // notification from inside `load` would arrive before publication.
        let runtime = core.language_model.clone();
        let loader = std::thread::spawn(move || {
            runtime.start_with(true, move || {
                entered_loader.wait();
                release_loader.wait();
                muu_lm::ConversionLm::open(&directory).ok().map(Arc::new)
            });
        });
        entered.wait();
        let mut a = core.create_session().unwrap();
        let mut b = core.create_session().unwrap();
        let convert = |session: &mut crate::Session| {
            for character in "henkanseidogawarui".chars() {
                session.handle(crate::Request::Key {
                    key: crate::KeyEvent {
                        event_type: "key_down".to_owned(),
                        keycode: 0,
                        modifiers: 0,
                        characters_utf8: Some(character.to_string()),
                        characters_ignoring_modifiers: Some(character.to_string()),
                        is_repeat: false,
                    },
                });
            }
            session
                .handle(crate::Request::Command {
                    command: "convert".to_owned(),
                    arg: serde_json::Value::Null,
                })
                .preedit
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>()
        };
        let baseline = convert(&mut a);
        assert_eq!(baseline, "変換制度が悪い");
        assert_eq!(core.language_model().state, LanguageModelState::Loading);
        release.wait();
        loader.join().unwrap();
        assert_eq!(core.language_model().state, LanguageModelState::Available);
        assert_eq!(convert(&mut b), "変換精度が悪い");
        let first = core.language_model().model.unwrap();
        let second = core.language_model().model.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!core.is_invalid());
    }

    #[test]
    fn background_loader_never_blocks_state_reads_and_panic_is_only_model_failure() {
        check_background_loader_failure(None);
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn background_loader_failure_preserves_real_dictionary_conversion() {
        let dictionary = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH")
            .expect("public dictionary required");
        check_background_loader_failure(Some(&dictionary));
    }

    fn check_background_loader_failure(dictionary: Option<&Path>) {
        let core = Arc::new(
            Core::new(CoreConfig {
                dictionary_path: dictionary,
            })
            .unwrap(),
        );
        let mut other = core.create_session().unwrap();
        other.handle(crate::Request::Key {
            key: crate::KeyEvent {
                event_type: "key_down".to_owned(),
                keycode: 0,
                modifiers: 0,
                characters_utf8: Some("ka".to_owned()),
                characters_ignoring_modifiers: Some("ka".to_owned()),
                is_repeat: false,
            },
        });
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let entered_loader = entered.clone();
        let release_loader = release.clone();
        // Own the background thread so completion means the shared runtime
        // has caught the panic and published Failed, not merely left `load`.
        let runtime = core.language_model.clone();
        let loader = std::thread::spawn(move || {
            runtime.start_with(true, move || {
                entered_loader.wait();
                release_loader.wait();
                panic!("private LM loader panic");
            });
        });
        entered.wait();
        // Both calls must return while the loader is still held at the
        // barrier. Only then can this thread release it: a loader-held read
        // lock would deadlock, independent of scheduler speed (docs/src/internals/conversion.md「言語モデルの読み込み」).
        assert_eq!(core.language_model().state, LanguageModelState::Loading);
        let _session = core.create_session().unwrap();
        release.wait();
        loader.join().unwrap();
        assert_eq!(core.language_model().state, LanguageModelState::Failed);
        assert!(!core.is_invalid());
        let baseline = other.handle(crate::Request::Command {
            command: "convert".to_owned(),
            arg: serde_json::Value::Null,
        });
        assert!(!baseline.preedit.is_empty());
        if dictionary.is_some() {
            assert!(baseline.segments.is_some());
        }

        let mut surviving = core.create_session().unwrap();
        let view = surviving.handle(crate::Request::Command {
            command: "set_mode".to_owned(),
            arg: serde_json::json!({"mode":"half_alnum"}),
        });
        assert_eq!(view.mode, crate::InputMode::HalfAlnum);
        core.set_user_fixture(UserDictionarySnapshot::default())
            .unwrap();
        core.language_model
            .start_with(true, || panic!("must not retry a failed load"));
        assert_eq!(core.language_model().state, LanguageModelState::Failed);
    }

    #[test]
    fn shared_mutation_panics_are_terminal_with_a_complete_old_or_new_snapshot() {
        for after in [false, true] {
            let core = Core::new(CoreConfig {
                dictionary_path: None,
            })
            .unwrap();
            let retained = core.conversion_snapshot();
            let before = core.vocabulary.snapshot().user.revision;
            let snapshot = serde_json::from_value(serde_json::json!({"schema_version":1,"entries":[{"reading":"むう","surface":"New","part_of_speech":"NOUN","priority":"high"}]})).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                core.publish_user_dictionary_panicking(snapshot, after);
            }));
            assert!(result.is_err());
            assert!(core.is_invalid());
            assert_eq!(retained.vocabulary.user.revision, before);
            assert_eq!(
                core.vocabulary.snapshot().user.revision,
                before + u64::from(after)
            );
        }
    }
    #[test]
    fn learning_publication_panics_invalidate_core_without_partial_observations() {
        for after in [false, true] {
            let core = Core::new(CoreConfig {
                dictionary_path: None,
            })
            .unwrap();
            let old = core.learning.capture();
            let fingerprint = muu_dict::DictionaryFingerprint::new([0x5a; 32], 100);
            let result = catch_unwind(AssertUnwindSafe(|| {
                core.learning.observe_panicking(
                    old.generation,
                    &[muu_dict::DictionaryTokenId::new(1)],
                    fingerprint,
                    after,
                    || core.publication_guard(),
                );
            }));
            assert!(result.is_err() && core.is_invalid());
            assert_eq!(old.revision, 0);
            let current = core.personal_frequency_update(None).unwrap().unwrap();
            let data: PersonalFrequencySnapshot = serde_json::from_slice(&current.data).unwrap();
            assert_eq!(data.observation_count, u64::from(after));
            assert_eq!(current.version.revision, u64::from(after));
        }
    }

    #[test]
    fn record_append_panics_leave_complete_old_or_new_queue_contents() {
        for after in [false, true] {
            let core = Core::new(CoreConfig {
                dictionary_path: None,
            })
            .unwrap();
            let value = serde_json::json!({"complete":"record"});
            let make = || crate::commit_stream::PreparedRecord::from_json(&value).unwrap();
            let bytes = b"{\"complete\":\"record\",\"dropped_records\":0}".to_vec();
            let result = catch_unwind(AssertUnwindSafe(|| {
                if after {
                    let mut records = core.completed_records.lock().unwrap();
                    let _publication = core.publication_guard();
                    records.push(make());
                    panic!("after shared record publication");
                } else {
                    core.publish_commit_stream_events(std::iter::once(make()).chain(
                        std::iter::from_fn(|| panic!("before shared record publication")),
                    ));
                }
            }));
            assert!(result.is_err());
            assert_eq!(core.is_invalid(), after);
            assert_eq!(core.take_commit_stream_event(), after.then_some(bytes));
            assert!(core.take_commit_stream_event().is_none());
        }
    }
    #[test]
    fn delivery_preparation_failure_keeps_front_and_core_valid() {
        let core = Core::new(CoreConfig {
            dictionary_path: None,
        })
        .unwrap();
        let make = || {
            crate::commit_stream::PreparedRecord::from_json(&serde_json::json!({"r":1})).unwrap()
        };
        core.publish_commit_stream_events([make()]);
        assert_eq!(
            core.take_commit_stream_event_prepared(|_, _| Err::<(), _>("copy")),
            Err("copy")
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = core.take_commit_stream_event_prepared::<(), ()>(|_, _| panic!("copy"));
            }))
            .is_err()
        );
        assert!(!core.is_invalid());
        let (bytes, weight) = core
            .take_commit_stream_event_prepared(|bytes, weight| {
                Ok::<_, ()>((bytes.to_vec(), weight))
            })
            .unwrap()
            .unwrap();
        assert_eq!(weight, 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["dropped_records"],
            0
        );
        core.report_commit_stream_dropped(weight);
        core.publish_commit_stream_events([make()]);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&core.take_commit_stream_event().unwrap())
                .unwrap()["dropped_records"],
            1
        );
    }
    #[test]
    #[ignore = "requires public edge, zenz (check-assets)"]
    fn requested_model_fallback_and_learning_authority_are_captured_together() {
        use muu_dict::DictionaryTokenId;
        use muu_lm::ModelKind::{Edge, Zenz};
        let (Some(edge), Some(zenz)) = (
            crate::real_assets::real_asset_path("MUU_LM_MODEL_DIR"),
            crate::real_assets::real_asset_path("MUU_ZENZ_MODEL_DIR"),
        ) else {
            return;
        };
        let dictionary = crate::test_support::conversion_dictionary();
        let make_core = || {
            Core::new(CoreConfig {
                dictionary_path: Some(dictionary.path()),
            })
            .unwrap()
        };
        let seed = make_core();
        seed.initialize_fixture_data(
            &UserDictionarySnapshot::default(),
            &PersonalFrequencySnapshot::default(),
        )
        .unwrap();
        assert!(seed.observe_choices(
            seed.learning_version().generation,
            &[DictionaryTokenId::new(0)],
            &[DictionaryTokenId::new(1)]
        ));
        let learning = seed.personal_frequency_update(None).unwrap().unwrap().data;
        let snapshot: PersonalFrequencySnapshot = serde_json::from_slice(&learning).unwrap();
        assert_eq!(snapshot.observation_count, 1);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.rejected_entries.len(), 1);
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing");
        let broken = temporary.path().join("broken");
        std::fs::create_dir(&broken).unwrap();
        std::fs::write(broken.join(muu_lm::ZENZ_MODEL_FILE), b"broken GGUF").unwrap();
        std::fs::write(broken.join("model.safetensors"), b"broken weights").unwrap();
        std::fs::write(broken.join("tokenizer.json"), b"{}").unwrap();
        for (request, directory, fallback, actual, reason, unavailable) in [
            (Zenz, &zenz, Some(&edge), Some(Zenz), None, None),
            (Edge, &edge, None, Some(Edge), None, None),
            (
                Zenz,
                &missing,
                Some(&edge),
                Some(Edge),
                Some("not_found"),
                None,
            ),
            (
                Zenz,
                &broken,
                Some(&edge),
                Some(Edge),
                Some("load_failed"),
                None,
            ),
            (
                Zenz,
                &missing,
                Some(&missing),
                None,
                Some("not_found"),
                Some("not_found"),
            ),
            // An explicit edge choice never tries zenz, even if a fallback path was supplied.
            (Edge, &broken, Some(&zenz), None, None, Some("load_failed")),
        ] {
            let core = Arc::new(make_core());
            core.initialize_fixture_data(&UserDictionarySnapshot::default(), &snapshot)
                .unwrap();
            core.configure_sessions(CoreSessionSettings {
                conversion_lm_dir: Some(directory.clone()),
                conversion_lm_model: Some(request),
                conversion_lm_fallback_dir: fallback.cloned(),
                conversion_lm_load_synchronously: true,
                commit_stream_config: Some(crate::test_support::test_commit_stream_config("model")),
            })
            .unwrap_or_else(|_| panic!("one configuration"));
            let lm = core.language_model();
            assert_eq!(lm.model.as_ref().map(|m| m.kind()), actual);
            assert_eq!(lm.info.requested_model, Some(request.as_str()));
            assert_eq!(lm.info.actual_model, actual.map(|m| m.as_str()));
            assert_eq!(lm.info.fallback_reason, reason);
            assert_eq!(lm.info.unavailable_reason, unavailable);
            assert_eq!(
                lm.info.route,
                if actual.is_none() {
                    "unavailable"
                } else if actual != Some(request) {
                    "fallback"
                } else {
                    "requested"
                }
            );
            assert_eq!(
                core.personal_frequency_update(None).unwrap().unwrap().data,
                learning,
                "changing the model must not rebind or clear dictionary learning"
            );
            let status = serde_json::to_value(core.runtime_status()).unwrap();
            assert_eq!(
                status["language_model_details"],
                serde_json::to_value(&lm.info).unwrap()
            );
            let mut session = core.create_session().unwrap();
            for c in "kyou".chars() {
                session.handle(crate::test_support::key(&c.to_string(), 0));
            }
            session.handle(crate::test_support::command(
                "convert",
                serde_json::Value::Null,
            ));
            session.handle(crate::test_support::command(
                "commit",
                serde_json::Value::Null,
            ));
            let event: serde_json::Value =
                serde_json::from_slice(&core.take_commit_stream_event().unwrap()).unwrap();
            let authority = &event["provenance"]["language_model"];
            for (key, value) in serde_json::to_value(&lm.info).unwrap().as_object().unwrap() {
                assert_eq!(&authority[key], value, "{key}");
            }
            assert_eq!(
                event["provenance"]["rules_version"],
                actual.map_or(3, |m| m.rules_version())
            );
            assert_eq!(
                event["initial"]["selection_report"]["rules_version"],
                actual.map_or(3, |m| m.rules_version())
            );
            assert_eq!(event["initial"]["selection_report"]["format_version"], 3);
            assert!(!core.is_invalid());
        }
    }
}
