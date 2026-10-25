//! Stable C ABI for process resources and main-thread-affine sessions.
//!
//! Threading (docs/src/internals/architecture.md「C ABI」): session functions take a raw
//! handle with no lock and must only be called from the host's main thread.
//! Core functions go through the process registry and are safe from any
//! thread; dictionary load and edit operations may run off main. The persistence
//! rules of docs/src/internals/storage.md「変換学習の保存」 additionally require the learning query and clear to run on
//! main, which this layer cannot enforce.
//!
//! Failure units (docs/src/internals/architecture.md「失敗したとき」): `MUU_RESULT_SESSION_INVALID` condemns one handle and
//! the host recovers by destroying and recreating it.
//! `MUU_RESULT_CORE_INVALID` is terminal for the process; every later call
//! returns it and recreating sessions cannot help. It is returned if and only
//! if the Core's invalid flag is set, which happens only when a panic unwinds
//! out of a locked publication section. A panic in a core call outside such a
//! section (decoding, preparing a layer, reading, serializing, building the
//! output buffer) fails that one call with `MUU_RESULT_INTERNAL` and leaves
//! the Core usable.

mod abi_support;
use abi_support::{
    PreparedOutputGuard, PrivateFailure, acknowledge, acknowledge_ready, borrowed, bounded_decode,
    clear_outputs, decode_input, distinct_outputs, edit_failure, stage_outputs,
    write_private_failure,
};

mod commit_stream_abi;
pub use commit_stream_abi::*;
mod user_dictionary_abi;
pub use user_dictionary_abi::*;
mod imported_dictionary_abi;
mod postal_code_abi;
pub use imported_dictionary_abi::*;
pub use postal_code_abi::*;

use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::sync::{Arc, OnceLock};

#[cfg(test)]
use muu_session::UserDictionarySnapshot;
use muu_session::{
    CommitStreamConfig, Core, CoreConfig, CoreSessionSettings, LearningVersion,
    MAX_PROTOCOL_ENVELOPE_BYTES, MAX_USER_DICTIONARY_EDIT_BYTES, MAX_USER_DICTIONARY_TICKET_BYTES,
    MAX_USER_DICTIONARY_VIEW_BYTES, PersonalFrequencySnapshot, Session, StoredFileState,
    UserDictionaryEditCommand, UserDictionaryEditError, UserDictionaryTicket, decode_request,
    encode_view,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

// Result codes. `just ffi-abi-contract-check` pins every value. Meanings
// specific to one function, notably whether INTERNAL guarantees "not
// published", are documented on that function.
pub const MUU_RESULT_OK: i32 = 0;
pub const MUU_RESULT_NULL_HANDLE: i32 = 1;
/// Arguments the call cannot use: a null pointer with a nonzero length, a
/// missing or aliased output, and on some entries an oversized input (others
/// answer DECODE_ERR for that). Checked before Core state is consulted, so it
/// takes precedence over CREATE_FAILED and CORE_INVALID.
pub const MUU_RESULT_BAD_ARG: i32 = 2;
/// Also what every Core entry returns before initialization has completed,
/// and for the life of the process after a failed initialization.
pub const MUU_RESULT_CREATE_FAILED: i32 = 3;
pub const MUU_RESULT_DECODE_ERR: i32 = 4;
/// Only this handle is unusable: destroy it and create a new one. From
/// `muu_session_create` it means no handle was made.
pub const MUU_RESULT_SESSION_INVALID: i32 = 5;
/// This call alone failed; the Core remains usable.
pub const MUU_RESULT_INTERNAL: i32 = 6;
/// Terminal for the process: every later Core and session call returns it.
pub const MUU_RESULT_CORE_INVALID: i32 = 7;
/// The ticket's management revision is no longer current; nothing was
/// published.
pub const MUU_RESULT_REVISION_MISMATCH: i32 = 8;
/// Core holds prepared, or published but unresolved, work for this data.
pub const MUU_RESULT_BUSY: i32 = 9;
// Swift refuses to initialize on a mismatch and passes every key through.
const ABI_VERSION: u32 = 13;
// Same bound as the stored file (docs/src/internals/storage.md「保存する場所」); larger startup learning is invalid
// without being read.
const MAX_LEARNING_BYTES: usize = 512 * 1024;
static CORE_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
// The app build passes HEAD separately from its Info.plist. Keep this marker
// reachable through a linked ABI function so archive/dead stripping cannot
// remove it. This identifies the commit, not uncommitted source edits; product
// installation requires a clean committed tree (docs/src/internals/build-and-test.md「ビルド」).
static RUST_SOURCE_ID: &[u8] = concat!(
    "MUU_RUST_SOURCE_COMMIT=",
    env!("MUU_RUST_SOURCE_COMMIT"),
    "\0"
)
.as_bytes();

// Absent in the configuration is the legacy entry: no model kind reaches
// Core, and the directory is used as edge only if both model files exist
// (`build_core`). Present, the directory is passed even when its
// files are missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConversionModelChoice {
    Edge,
    Zenz,
}
impl ConversionModelChoice {
    fn kind(self) -> muu_session::ConversionModelKind {
        match self {
            Self::Edge => muu_session::ConversionModelKind::Edge,
            Self::Zenz => muu_session::ConversionModelKind::Zenz,
        }
    }
}

/// The part of the configuration that identifies the Core. A repeated
/// initialization must match it exactly or is refused (docs/src/internals/architecture.md「失敗したとき」). Only values the
/// host keeps fixed for the life of the process belong here. Initial data
/// (startup learning, the startup user dictionary) is deliberately absent:
/// it is applied by the first call only and ignored by later ones, so
/// comparing it would reject a harmless repeat. Paths are compared as given,
/// before any presence check, and recording only as enabled or not.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CoreResourceConfig {
    dictionary_path: Option<String>,
    conversion_lm_dir: Option<String>,
    conversion_lm_model: Option<ConversionModelChoice>,
    conversion_lm_fallback_dir: Option<String>,
    conversion_lm_load_synchronously: bool,
    recording_enabled: bool,
}

// ABI 13 accepts only the opaque user-file startup protocol. Unknown keys
// are rejected and that first failure is retained by the registry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializationConfig {
    #[serde(default)]
    dictionary_path: Option<String>,
    #[serde(default)]
    conversion_lm_dir: Option<String>,
    #[serde(default)]
    conversion_lm_model: Option<ConversionModelChoice>,
    #[serde(default)]
    conversion_lm_fallback_dir: Option<String>,
    #[serde(default)]
    conversion_lm_load_synchronously: bool,
    #[serde(default)]
    commit_stream: Option<CommitStreamConfig>,
    user_dictionary_file_state: StoredFileState,
    #[cfg(test)]
    #[serde(default)]
    force_initialize_failure: bool,
}

fn decode_initialization_config(
    bytes: Result<&[u8], &'static str>,
) -> Result<InitializationConfig, &'static str> {
    serde_json::from_slice(bytes?).map_err(|_| "invalid_config")
}

impl Default for InitializationConfig {
    fn default() -> Self {
        serde_json::from_str(r#"{"user_dictionary_file_state":"missing"}"#)
            .expect("default byte initialization configuration is valid")
    }
}

impl InitializationConfig {
    fn resources(&self) -> CoreResourceConfig {
        CoreResourceConfig {
            dictionary_path: self.dictionary_path.clone(),
            conversion_lm_dir: self.conversion_lm_dir.clone(),
            conversion_lm_model: self.conversion_lm_model,
            conversion_lm_fallback_dir: self.conversion_lm_fallback_dir.clone(),
            conversion_lm_load_synchronously: self.conversion_lm_load_synchronously,
            recording_enabled: self.commit_stream.is_some(),
        }
    }
}

struct InitializedCore {
    core: Arc<Core>,
    report: Vec<u8>,
}
// The outcome of the one initialization attempt. A failure is stored just
// like a success: the registry never retries, so every later call reports the
// same logical result (docs/src/internals/architecture.md「失敗したとき」), even if copying a report fails for one call.
struct CoreInitialization {
    resources: Option<CoreResourceConfig>,
    result: Result<InitializedCore, &'static str>,
    failure_report: Vec<u8>,
}
#[derive(Default)]
struct CoreRegistry {
    initialized: OnceLock<CoreInitialization>,
}

#[cfg(not(test))]
static PROCESS_CORE: CoreRegistry = CoreRegistry {
    initialized: OnceLock::new(),
};
// Unit tests each need a fresh, uninitialized registry, and the test harness
// runs them on separate threads. The real process-wide static is exercised by
// the integration tests under `tests/`, each of which is its own process.
#[cfg(test)]
thread_local! {
    static TEST_CORE: CoreRegistry = CoreRegistry::default();
}
fn with_registry<T>(operation: impl FnOnce(&CoreRegistry) -> T) -> T {
    #[cfg(test)]
    {
        TEST_CORE.with(operation)
    }
    #[cfg(not(test))]
    {
        operation(&PROCESS_CORE)
    }
}

type InitializationOutput = Result<(i32, Box<[u8]>), (i32, &'static str)>;

impl CoreRegistry {
    fn initialize(
        &self,
        config_json: Result<&[u8], &'static str>,
        learning: &[u8],
        present: bool,
        user_file: &[u8],
    ) -> InitializationOutput {
        let decode = || decode_initialization_config(config_json);
        let mut first_report = None;
        // OnceLock serializes the entire first attempt, including failure
        // report preparation (docs/src/internals/architecture.md「失敗したとき」). Otherwise a first output panic would
        // escape the retained initialization boundary.
        let initialized = self.initialized.get_or_init(|| {
            catch_unwind(AssertUnwindSafe(|| {
                let outcome = (|| {
                    #[cfg(test)]
                    initialization_test_point("decode");
                    let config = decode()?;
                    let resources = config.resources();
                    #[cfg(test)]
                    initialization_test_point("build");
                    let initialized = build_core(config, learning, present, user_file)?;
                    Ok((resources, initialized))
                })();
                let initialized = match outcome {
                    Ok((resources, initialized)) => CoreInitialization {
                        resources: Some(resources),
                        result: Ok(initialized),
                        failure_report: Vec::new(),
                    },
                    Err(code) => CoreInitialization {
                        resources: None,
                        result: Err(code),
                        failure_report: error_bytes(code),
                    },
                };
                #[cfg(test)]
                initialization_test_point("first_report");
                let report = match &initialized.result {
                    Ok(accepted) => &accepted.report,
                    Err(_) => &initialized.failure_report,
                };
                first_report = Some(prepare_buffer(report.clone()));
                initialized
            }))
            .unwrap_or_else(|_| {
                // The panic fallback is fixed bytes: do not repeat the failed
                // serializer/copy operation while retaining its outcome.
                let report = br#"{"code":"core_initialization_panic","params":{}}"#.to_vec();
                first_report = Some(report.clone().into_boxed_slice());
                CoreInitialization {
                    resources: None,
                    result: Err("core_initialization_panic"),
                    failure_report: report,
                }
            })
        });
        // A repeat after the Core was invalidated reports that, not the
        // cached success.
        let status = match &initialized.result {
            Ok(accepted) => {
                if accepted.core.is_invalid() {
                    return Err((MUU_RESULT_CORE_INVALID, "core_invalid"));
                }
                MUU_RESULT_OK
            }
            Err(_) => MUU_RESULT_CREATE_FAILED,
        };
        if let Some(report) = first_report {
            return Ok((status, report));
        }
        // Repeats compare only immutable resources; startup data is never
        // reapplied. Copy failure must not change either cached outcome.
        let report = match &initialized.result {
            Ok(accepted) => {
                let resources = decode()
                    .map_err(|code| (MUU_RESULT_CREATE_FAILED, code))?
                    .resources();
                if initialized.resources.as_ref() != Some(&resources) {
                    return Err((MUU_RESULT_CREATE_FAILED, "core_configuration_mismatch"));
                }
                &accepted.report
            }
            Err(_) => &initialized.failure_report,
        };
        #[cfg(test)]
        initialization_test_point("repeat_report");
        Ok((status, prepare_buffer(report.clone())))
    }

    /// Calls before publication fail immediately, even while another thread
    /// is still constructing the Core.
    ///
    /// `OnceLock::get` does not wait for an initializer in progress, so the
    /// host's main thread is never parked behind the dictionary open. The cost
    /// is that ordering becomes the host's responsibility: a session or a
    /// dictionary operation that arrives early is refused, not deferred (docs/src/internals/architecture.md「C ABI」).
    fn ready(&self) -> Result<&InitializedCore, (i32, &'static str)> {
        let initialized = self
            .initialized
            .get()
            .ok_or((MUU_RESULT_CREATE_FAILED, "core_not_initialized"))?;
        let initialized = initialized
            .result
            .as_ref()
            .map_err(|code| (MUU_RESULT_CREATE_FAILED, *code))?;
        if initialized.core.is_invalid() {
            initialized.core.discard_user_dictionary_preparation();
            return Err((MUU_RESULT_CORE_INVALID, "core_invalid"));
        }
        Ok(initialized)
    }
}

#[cfg(test)]
thread_local! {
    static INITIALIZATION_PANIC: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn initialization_test_point(point: &'static str) {
    INITIALIZATION_PANIC.with(|injection| {
        if injection.get() == Some(point) {
            injection.set(None);
            panic!("initialization test: {point}");
        }
    });
}

fn build_core(
    config: InitializationConfig,
    learning: &[u8],
    present: bool,
    user_file: &[u8],
) -> Result<InitializedCore, &'static str> {
    #[cfg(test)]
    if config.force_initialize_failure {
        return Err("forced_initialize_failure");
    }
    let dictionary_path = config.dictionary_path.as_ref().map(PathBuf::from);
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: dictionary_path.as_deref(),
        })
        .map_err(|_| "core_data_invalid")?,
    );
    // Without the system dictionary, saved learning cannot be checked against
    // its authority (docs/src/internals/storage.md「変換学習の保存」). Do not even parse it: reporting it as invalid would
    // make the host quarantine valid data and replace it with an empty file.
    let learning_unverifiable = present && !core.has_dictionary();
    let mut learning_invalid = false;
    let mut learning_snapshot = if present && !learning_unverifiable {
        if learning.len() > MAX_LEARNING_BYTES {
            learning_invalid = true;
            PersonalFrequencySnapshot::default()
        } else {
            serde_json::from_slice(learning).unwrap_or_else(|_| {
                learning_invalid = true;
                PersonalFrequencySnapshot::default()
            })
        }
    } else {
        PersonalFrequencySnapshot::default()
    };
    // Learning rejection publishes neither startup store, so a retry with
    // empty learning is safe. User-file rejection remains a usable read-only
    // management state, exactly as in the byte entry before ABI 13.
    loop {
        match core.initialize_data(
            config.user_dictionary_file_state,
            user_file,
            &learning_snapshot,
        ) {
            Ok(()) => break,
            Err(muu_session::SessionError::PersonalFrequency(_)) => {
                learning_invalid = true;
                learning_snapshot = PersonalFrequencySnapshot::default();
            }
            Err(_) => return Err("core_data_invalid"),
        }
    }
    let user_invalid = core.user_dictionary_view().reason.is_some();
    // An omitted model keeps the explicit-edge presence check. Callers pass
    // the requested model even when missing: Core owns the one background
    // attempt, fallback and its not_found / load_failed facts.
    let explicit_model = config.conversion_lm_model.map(ConversionModelChoice::kind);
    let lm_dir = config.conversion_lm_dir.map(PathBuf::from).filter(|path| {
        explicit_model.is_some()
            || (path.join("model.safetensors").is_file() && path.join("tokenizer.json").is_file())
    });
    let settings = CoreSessionSettings {
        conversion_lm_dir: lm_dir,
        conversion_lm_model: explicit_model,
        conversion_lm_fallback_dir: config.conversion_lm_fallback_dir.map(PathBuf::from),
        conversion_lm_load_synchronously: config.conversion_lm_load_synchronously,
        commit_stream_config: config.commit_stream,
    };
    // Only "no directory survived the filter above". With an explicit model
    // this is false even when the files are missing; that fact surfaces
    // later as `not_found` in `muu_core_runtime_status`.
    let language_model_unavailable = settings.conversion_lm_dir.is_none();
    core.configure_sessions(settings)
        .map_err(|_| "core_data_invalid")?;
    // Validate the configured recording template before publication. No file
    // reads occur in session creation; the temporary session produces no event.
    // LM loading was started by configure_sessions, synchronously only if requested.
    // This session takes nonce 1, so the host's first session is nonce 2.
    core.create_session().map_err(|_| "core_data_invalid")?;
    let dictionary_kana_only = core.degraded_kana_only();
    let report = serde_json::to_vec(&serde_json::json!({
        "status": if dictionary_kana_only || learning_invalid || learning_unverifiable || user_invalid || language_model_unavailable { "degraded" } else { "success" },
        "degraded": {
            "dictionary_kana_only": dictionary_kana_only,
            "personal_frequency_invalid": learning_invalid,
            "personal_frequency_unverifiable": learning_unverifiable,
            "user_dictionary_invalid": user_invalid,
            "language_model_unavailable": language_model_unavailable,
        },
        // The version of the state that was accepted (or of the empty state
        // that replaced a rejected file). The host derives its persistence
        // baseline from this (docs/src/internals/storage.md「変換学習の保存」).
        "personal_frequency_version": core.learning_version(),
    })).map_err(|_| "core_data_invalid")?;
    Ok(InitializedCore { core, report })
}

/// Bytes crossing the boundary. Inputs are borrowed for the call only. Outputs
/// are Rust-owned and released only by `muu_buffer_free`. Every entry resets
/// its outputs to `{NULL, 0}` before any failure other than a rejected output
/// pointer. A zero `len` is "no bytes" even if `data` is non-null; free it the
/// same way.
#[repr(C)]
pub struct MuuBuffer {
    pub data: *mut u8,
    pub len: usize,
}
impl MuuBuffer {
    const EMPTY: Self = Self {
        data: ptr::null_mut(),
        len: 0,
    };
}

/// Opaque handle. All session calls are main-thread-affine; never share a handle.
pub struct MuuSession {
    session: Session,
    // Kept beside the session so the Core's validity can be read without
    // touching a `Session` that a panic may have left half-updated.
    core: Arc<Core>,
    // Set when a panic unwound out of `session`. Its fields may be
    // inconsistent, so it is never entered again; the host must destroy the
    // handle and create a new one.
    poisoned: bool,
    _not_send_or_sync: std::marker::PhantomData<*mut c_void>,
}

/// Return ABI epoch 13. The opaque user-file initializer, dictionary
/// transactions and weighted record delivery are the supported host paths.
/// Dictionary prepare/load steps return INTERNAL with a reason for known
/// revision exhaustion (User: user_dictionary_revision_overflow;
/// Imported/Postal: revision_overflow). Panic and output-allocation failure
/// leave every step output empty. ACK functions allocate no response body.
#[unsafe(no_mangle)]
pub extern "C" fn muu_abi_version() -> u32 {
    ABI_VERSION
}
#[unsafe(no_mangle)]
pub extern "C" fn muu_core_version() -> *const c_char {
    std::hint::black_box(RUST_SOURCE_ID);
    CORE_VERSION.as_ptr().cast()
}

/// Read current process facts as raw JSON (schema_version 2, at most 16 KiB):
/// system_dictionary, language_model, commit_stream_enabled and captured
/// language_model_details (request, actual model, fallback and failure reasons).
/// Does not initialize resources, load a model, or drain any queue. Nonzero
/// results leave an empty buffer; INTERNAL is local and does not revoke Core.
/// Free the returned buffer regardless of the result.
///
/// # Safety
/// A non-null out_status must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_runtime_status(out_status: *mut MuuBuffer) -> i32 {
    if out_status.is_null() {
        return MUU_RESULT_BAD_ARG;
    }
    unsafe { out_status.write(MuuBuffer::EMPTY) };
    // Unlike publishing APIs, this read has no shared mutation boundary.
    // All fallible preparation is inside the catch; the final ownership
    // transfer is allocation-free and emits no error payload on failure.
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| {
            // ready() on an invalid Core also discards an idle user
            // dictionary preparation, as on every other entry point. That
            // is the only state this read can change.
            let core = &registry.ready()?.core;
            #[cfg(test)]
            initialization_test_point("runtime_status");
            let bytes = serde_json::to_vec(&core.runtime_status())
                .map_err(|_| (MUU_RESULT_INTERNAL, "runtime_status_failed"))?;
            if bytes.len() > 16 * 1024 {
                return Err((MUU_RESULT_INTERNAL, "runtime_status_too_large"));
            }
            Ok(prepare_buffer(bytes))
        })
    }));
    match result {
        Ok(Ok(bytes)) => {
            unsafe { out_status.write(buffer_from_box(bytes)) };
            MUU_RESULT_OK
        }
        Ok(Err((status, _))) => status,
        Err(_) => core_panic_result().0,
    }
}

/// Create a session on the main thread after initialization has completed.
///
/// # Safety
/// Both output pointers, when non-null, must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_session_create(
    out_session: *mut *mut MuuSession,
    out_error: *mut MuuBuffer,
) -> i32 {
    if out_session.is_null() {
        return MUU_RESULT_NULL_HANDLE;
    }
    unsafe { out_session.write(ptr::null_mut()) };
    if !out_error.is_null() {
        unsafe { out_error.write(MuuBuffer::EMPTY) };
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| {
            let core = registry.ready()?.core.clone();
            let session = core
                .create_session()
                .map_err(|_| (MUU_RESULT_CREATE_FAILED, "session_data_invalid"))?;
            Ok(Box::into_raw(Box::new(MuuSession {
                session,
                core,
                poisoned: false,
                _not_send_or_sync: std::marker::PhantomData,
            })))
        })
    }));
    match result {
        Ok(Ok(session)) => {
            unsafe { out_session.write(session) };
            MUU_RESULT_OK
        }
        Ok(Err((code, error))) => {
            write_error(out_error, error);
            code
        }
        // No handle exists to condemn; 5 here only means "no session", and
        // the host keeps passing keys through. Session creation enters no
        // publication section, so this cannot be a terminal Core failure.
        Err(_) => {
            write_error(out_error, "session_panic");
            MUU_RESULT_SESSION_INVALID
        }
    }
}

/// Destroy a session; NULL is a no-op. Must run on the main thread.
///
/// # Safety
/// A non-null pointer must be live and have been allocated by `muu_session_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_session_destroy(session: *mut MuuSession) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !session.is_null() {
            drop(unsafe { Box::from_raw(session) });
        }
    }));
}

/// Handle one protocol envelope on the main thread.
/// If full view encoding fails after a commit, OK may carry a reduced view
/// preserving commit text, next preedit, caret, selection, mode and consumed.
/// Display metadata is omitted; the request, learning and records are not
/// replayed. Without a commit, encoding failure remains INTERNAL with no view.
/// Panic statuses and the envelope format are unchanged.
///
/// # Safety
/// The session is live, the input readable, and `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_session_handle_event(
    session: *mut MuuSession,
    request: *const u8,
    len: usize,
    out: *mut MuuBuffer,
) -> i32 {
    if out.is_null() {
        return MUU_RESULT_BAD_ARG;
    }
    unsafe { out.write(MuuBuffer::EMPTY) };
    if session.is_null() {
        return MUU_RESULT_NULL_HANDLE;
    }
    if request.is_null() && len > 0 {
        return MUU_RESULT_BAD_ARG;
    }
    let handle = unsafe { &mut *session };
    // The Core check comes first: once the Core is invalid every handle must
    // report that, including one that was poisoned earlier, so the host stops
    // recreating sessions.
    if handle.core.is_invalid() {
        return MUU_RESULT_CORE_INVALID;
    }
    if handle.poisoned {
        return write_view(out, muu_session::poisoned_view())
            .map(|_| MUU_RESULT_SESSION_INVALID)
            .unwrap_or(MUU_RESULT_INTERNAL);
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let bytes = if len == 0 {
            &[][..]
        } else {
            unsafe { slice::from_raw_parts(request, len) }
        };
        // A request that does not decode never reaches the session, so its
        // state is unchanged and there is no view.
        let request = match decode_request(bytes) {
            Ok(request) => request,
            Err(_) => return MUU_RESULT_DECODE_ERR,
        };
        #[cfg(test)]
        if matches!(&request, muu_session::Request::Command { command, .. } if command == "panic_test")
        {
            panic!("session test panic");
        }
        // Keep the applied view alive through encoding. A committed string
        // cannot be reconstructed by replaying the request: that would also
        // repeat learning and record delivery.
        write_view(out, handle.session.handle(request))
            .map(|_| MUU_RESULT_OK)
            .unwrap_or(MUU_RESULT_INTERNAL)
    }));
    match result {
        Ok(code) => code,
        Err(_) => {
            // The session is unusable either way. Which failure unit this was
            // is read from the Core: a panic that unwound out of a locked
            // publication section set the invalid flag on its way out (the
            // Core's publication guard); any other panic left the published
            // values untouched, and only this handle is condemned.
            handle.poisoned = true;
            if handle.core.is_invalid() {
                MUU_RESULT_CORE_INVALID
            } else {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let _ = write_view(out, muu_session::poisoned_view());
                }));
                MUU_RESULT_SESSION_INVALID
            }
        }
    }
}

/// Clear learning and return its new version as raw JSON. Call synchronously on
/// main after deleting the persisted file to exclude intervening session commits.
///
/// # Safety
/// `out_version` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_clear_personal_frequency(out_version: *mut MuuBuffer) -> i32 {
    unsafe {
        core_buffer_operation(out_version, |core| {
            serde_json::to_vec(&core.clear_personal_frequency())
                .map_err(|_| (MUU_RESULT_INTERNAL, "encode_error"))
        })
    }
}
/// Read learning without changing it. `received_json` is a raw version JSON or
/// `null`. No update returns two `{NULL, 0}` buffers. Neither output may be NULL.
///
/// # Safety
/// Input bytes are readable, both distinct output pointers are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_personal_frequency_snapshot(
    received_json: *const u8,
    len: usize,
    out_data: *mut MuuBuffer,
    out_version: *mut MuuBuffer,
) -> i32 {
    if out_data.is_null() || out_version.is_null() || out_data == out_version {
        return MUU_RESULT_BAD_ARG;
    }
    unsafe {
        out_data.write(MuuBuffer::EMPTY);
        out_version.write(MuuBuffer::EMPTY);
    }
    if (received_json.is_null() && len > 0) || len > MAX_PROTOCOL_ENVELOPE_BYTES {
        return MUU_RESULT_BAD_ARG;
    }
    // Unlike the other entries, the version is decoded before readiness: a
    // malformed version gets DECODE_ERR even from an invalid Core. The query
    // has no publication section, so a panic is INTERNAL without asking Core.
    let result = catch_unwind(AssertUnwindSafe(|| {
        let received = if len == 0 {
            None
        } else {
            serde_json::from_slice::<Option<LearningVersion>>(unsafe {
                slice::from_raw_parts(received_json, len)
            })
            .map_err(|_| (MUU_RESULT_DECODE_ERR, "invalid_learning_version"))?
        };
        with_registry(|registry| {
            let update = registry
                .ready()?
                .core
                .personal_frequency_update(received)
                .map_err(|_| (MUU_RESULT_INTERNAL, "encode_error"))?;
            update
                .map(|update| {
                    serde_json::to_vec(&update.version)
                        .map(|version| (prepare_buffer(update.data), prepare_buffer(version)))
                        .map_err(|_| (MUU_RESULT_INTERNAL, "encode_error"))
                })
                .transpose()
        })
    }));
    // Output ownership conversion is read-only work too. Keep both buffers
    // inside the ABI unwind boundary; failure must not invalidate Core (docs/src/internals/architecture.md「失敗したとき」).
    catch_unwind(AssertUnwindSafe(|| match result {
        Ok(Ok(Some((data, version)))) => {
            unsafe {
                out_data.write(buffer_from_box(data));
                out_version.write(buffer_from_box(version));
            }
            MUU_RESULT_OK
        }
        Ok(Ok(None)) => MUU_RESULT_OK,
        Ok(Err((code, _))) => code,
        Err(_) => MUU_RESULT_INTERNAL,
    }))
    .unwrap_or(MUU_RESULT_INTERNAL)
}
// One output for both outcomes: the result on OK (empty bytes stay
// `{NULL, 0}`), or `{code, params}` JSON on failure. The host must read the
// status before interpreting the buffer.
unsafe fn core_buffer_operation(
    out: *mut MuuBuffer,
    operation: impl FnOnce(&Core) -> Result<Vec<u8>, (i32, &'static str)>,
) -> i32 {
    if out.is_null() {
        return MUU_RESULT_BAD_ARG;
    }
    unsafe { out.write(MuuBuffer::EMPTY) };
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| operation(&registry.ready()?.core).map(prepare_buffer))
    }));
    finish_buffer_result(result, out)
}
type BufferOperationResult = Result<Box<[u8]>, (i32, &'static str)>;
type CaughtBufferOperation = Result<BufferOperationResult, Box<dyn std::any::Any + Send>>;

// Only a shared publication guard can condemn Core (docs/src/internals/architecture.md「失敗したとき」). Readers,
// preparation and output copies report INTERNAL without setting that flag;
// returning 7 for them would make the host and Rust disagree permanently.
fn core_panic_result() -> (i32, &'static str) {
    with_registry(|registry| {
        match registry.ready() {
            Err((MUU_RESULT_CORE_INVALID, _)) => (MUU_RESULT_CORE_INVALID, "core_invalid"),
            // A retained initialization failure is a logical outcome, not
            // the status of this output operation. A copy panic is INTERNAL
            // even then, so the next call can still return the original report.
            _ => (MUU_RESULT_INTERNAL, "internal_panic"),
        }
    })
}

fn finish_buffer_result(result: CaughtBufferOperation, out: *mut MuuBuffer) -> i32 {
    // Buffer ownership conversion is output work too; it must not unwind
    // across C or turn a completed publication into a terminal failure.
    match catch_unwind(AssertUnwindSafe(|| match result {
        Ok(Ok(bytes)) => {
            if !bytes.is_empty() {
                unsafe { out.write(buffer_from_box(bytes)) };
            }
            MUU_RESULT_OK
        }
        Ok(Err((code, error))) => {
            write_error(out, error);
            code
        }
        Err(_) => {
            let (status, code) = core_panic_result();
            write_error(out, code);
            status
        }
    })) {
        Ok(status) => status,
        Err(_) => {
            let (status, code) = core_panic_result();
            write_error(out, code);
            status
        }
    }
}

/// Free a Rust-owned buffer. NULL is a no-op.
///
/// # Safety
/// Non-null data and its length must be unchanged and not previously freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_buffer_free(buffer: MuuBuffer) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !buffer.data.is_null() {
            drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(buffer.data, buffer.len)) });
        }
    }));
}

// Allocate and size the owned output while still inside the operation's
// unwind boundary. Initial initialization does this before publishing Core;
// repeats do it after publication and cannot change the cached outcome (docs/src/internals/architecture.md「失敗したとき」).
fn prepare_buffer(bytes: Vec<u8>) -> Box<[u8]> {
    #[cfg(test)]
    initialization_test_point("buffer_copy");
    bytes.into_boxed_slice()
}

fn buffer_from_box(mut boxed: Box<[u8]>) -> MuuBuffer {
    let buffer = MuuBuffer {
        data: boxed.as_mut_ptr(),
        len: boxed.len(),
    };
    std::mem::forget(boxed);
    buffer
}

fn into_buffer(bytes: Vec<u8>) -> MuuBuffer {
    buffer_from_box(prepare_buffer(bytes))
}

#[cfg(test)]
thread_local! {
    // Fail the full encoding only. The reduced view uses the real encoder.
    static FAIL_FULL_VIEW_ENCODING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// The reduced view keeps everything needed to commit and to show what comes
// next: commit text, preedit, caret, selection, mode and consumed. It drops
// display metadata only. It must not become "commit plus empty preedit": a
// key typed during conversion can commit the previous text and start a new
// preedit in the same reply, and a view claiming no composition would make
// the host skip committing that character on a click or focus change.
fn write_view(out: *mut MuuBuffer, view: muu_session::ViewState) -> Result<(), ()> {
    let full = encode_full_view(&view);
    let bytes = match full {
        Ok(bytes) => bytes,
        Err(()) if view.commit_text.is_some() => {
            let reduced = muu_session::ViewState {
                segments: None,
                candidates: None,
                composition_mode_transition: None,
                degraded: None,
                poisoned: None,
                error: None,
                ..view
            };
            encode_view(&reduced).map_err(|_| ())?
        }
        Err(()) => return Err(()),
    };
    // SAFETY: all callers validate out before reaching this helper.
    unsafe { out.write(into_buffer(bytes)) };
    Ok(())
}

fn encode_full_view(view: &muu_session::ViewState) -> Result<Vec<u8>, ()> {
    #[cfg(test)]
    if FAIL_FULL_VIEW_ENCODING.with(|failure| failure.replace(false)) {
        return Err(());
    }
    encode_view(view).map_err(|_| ())
}

fn write_error(out_error: *mut MuuBuffer, code: &'static str) {
    if out_error.is_null() {
        return;
    }
    let bytes = error_bytes(code);
    // SAFETY: create validated the caller-provided output pointer when non-null.
    unsafe { out_error.write(into_buffer(bytes)) };
}

fn error_bytes(code: &'static str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "code": code, "params": {} }))
        .expect("static error payload serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn initialize(mut config: Value, data: Option<&[u8]>) -> (i32, Value) {
        let snapshot = config
            .as_object_mut()
            .and_then(|object| object.remove("user_dictionary"));
        let user_file = snapshot.map(|value| {
            serde_json::from_value::<UserDictionarySnapshot>(value)
                .ok()
                .and_then(|snapshot| muu_session::encode_user_dictionary_file(&snapshot).ok())
                .unwrap_or_else(|| b"invalid synthetic user file".to_vec())
        });
        config["user_dictionary_file_state"] = json!(if user_file.is_some() {
            "present"
        } else {
            "missing"
        });
        let user_file = user_file.unwrap_or_default();
        let config = serde_json::to_vec(&config).unwrap();
        let mut out = MuuBuffer::EMPTY;
        let data = data.unwrap_or_default();
        let status = unsafe {
            muu_core_initialize_with_user_dictionary_file(
                config.as_ptr(),
                config.len(),
                data.as_ptr(),
                data.len(),
                !data.is_empty(),
                user_file.as_ptr(),
                user_file.len(),
                &mut out,
            )
        };
        (status, take(out, false))
    }
    fn initialize_present(data: &[u8]) -> (i32, Value) {
        let dictionary = test_dictionary();
        let config = serde_json::to_vec(
            &json!({"dictionary_path":dictionary.path(),"user_dictionary_file_state":"missing"}),
        )
        .unwrap();
        let mut out = MuuBuffer::EMPTY;
        let status = unsafe {
            muu_core_initialize_with_user_dictionary_file(
                config.as_ptr(),
                config.len(),
                data.as_ptr(),
                data.len(),
                true,
                ptr::null(),
                0,
                &mut out,
            )
        };
        (status, take(out, false))
    }
    fn take(buffer: MuuBuffer, envelope: bool) -> Value {
        assert!(!buffer.data.is_null());
        let bytes = unsafe { slice::from_raw_parts(buffer.data, buffer.len) };
        let value = serde_json::from_slice(if envelope { &bytes[3..] } else { bytes }).unwrap();
        unsafe { muu_buffer_free(buffer) };
        value
    }
    unsafe fn take_event(out: *mut MuuBuffer) -> i32 {
        let mut metadata = MuuBuffer::EMPTY;
        let status = unsafe { muu_core_take_commit_stream_event_with_weight(out, &mut metadata) };
        if metadata.len != 0 {
            let weight = take(metadata, false);
            assert_eq!(weight["schema_version"], 1);
            assert!(weight["drop_weight"].as_u64().unwrap() > 0);
        } else {
            assert!(metadata.data.is_null());
        }
        status
    }
    fn create() -> *mut MuuSession {
        let mut session = ptr::null_mut();
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_session_create(&mut session, &mut out) },
            MUU_RESULT_OK
        );
        assert!(out.data.is_null());
        session
    }
    fn request(session: *mut MuuSession, value: Value) -> (i32, Option<Value>) {
        let mut bytes = vec![1, 1, 0];
        bytes.extend(serde_json::to_vec(&value).unwrap());
        let mut out = MuuBuffer::EMPTY;
        let status =
            unsafe { muu_session_handle_event(session, bytes.as_ptr(), bytes.len(), &mut out) };
        (status, (!out.data.is_null()).then(|| take(out, true)))
    }
    fn key(session: *mut MuuSession, text: &str) -> Value {
        request(session, json!({"kind":"key", "key":{"event_type":"key_down", "keycode":0,"characters_utf8":text,"characters_ignoring_modifiers":text,"modifiers":0,"is_repeat":false}})).1.unwrap()
    }
    fn snapshot(received: Option<Value>) -> (Value, Value) {
        let received = serde_json::to_vec(&received).unwrap();
        let mut data = MuuBuffer::EMPTY;
        let mut version = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_personal_frequency_snapshot(
                    received.as_ptr(),
                    received.len(),
                    &mut data,
                    &mut version,
                )
            },
            0
        );
        (take(data, false), take(version, false))
    }
    fn user(surface: &str) -> Value {
        json!({"schema_version":1,"entries":[{"reading":"むう","surface":surface,"part_of_speech":"NOUN","priority":"high"}]})
    }

    #[test]
    fn model_choice_and_fallback_are_validated_fixed_core_resources() {
        let a = decode_initialization_config(Ok(br#"{"user_dictionary_file_state":"missing","conversion_lm_model":"zenz","conversion_lm_dir":"zenz","conversion_lm_fallback_dir":"edge"}"#)).unwrap();
        let b = decode_initialization_config(Ok(br#"{"user_dictionary_file_state":"present","conversion_lm_model":"zenz","conversion_lm_dir":"zenz","conversion_lm_fallback_dir":"edge"}"#)).unwrap();
        assert_eq!(a.resources(), b.resources());
        let edge = decode_initialization_config(Ok(br#"{"user_dictionary_file_state":"missing","conversion_lm_model":"edge","conversion_lm_dir":"zenz","conversion_lm_fallback_dir":"edge"}"#)).unwrap();
        assert_ne!(a.resources(), edge.resources());
        let other = decode_initialization_config(Ok(br#"{"user_dictionary_file_state":"missing","conversion_lm_model":"zenz","conversion_lm_dir":"zenz","conversion_lm_fallback_dir":"other"}"#)).unwrap();
        assert_ne!(a.resources(), other.resources());
        assert!(
            decode_initialization_config(Ok(
                br#"{"user_dictionary_file_state":"missing","conversion_lm_model":"unknown"}"#
            ))
            .is_err()
        );
        assert!(
            decode_initialization_config(Ok(
                br#"{"user_dictionary_file_state":"missing","conversion_lm_model":"unknown"}"#
            ))
            .is_err()
        );
        assert!(
            InitializationConfig::default()
                .conversion_lm_model
                .is_none(),
            "legacy callers stay edge"
        );
    }

    #[test]
    fn all_first_initialization_panics_are_retained_as_create_failed() {
        for point in ["decode", "build", "first_report", "buffer_copy"] {
            std::thread::spawn(move || {
                INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
                let first = initialize(json!({}), None);
                assert_eq!(first.0, MUU_RESULT_CREATE_FAILED);
                assert_eq!(first.1["code"], "core_initialization_panic");
                assert_eq!(initialize(json!({}), None), first);
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn runtime_status_before_initialization_is_empty_and_does_not_initialize() {
        let mut out = MuuBuffer {
            data: ptr::dangling_mut(),
            len: 99,
        };
        assert_eq!(
            unsafe { muu_core_runtime_status(&mut out) },
            MUU_RESULT_CREATE_FAILED
        );
        assert!(out.data.is_null() && out.len == 0);
        assert!(with_registry(|registry| registry
            .initialized
            .get()
            .is_none()));
        assert_eq!(
            unsafe { muu_core_runtime_status(ptr::null_mut()) },
            MUU_RESULT_BAD_ARG
        );
        assert_eq!(initialize(json!({}), None).0, MUU_RESULT_OK);
    }

    #[test]
    fn runtime_status_retained_failure_and_invalid_core_leave_empty_outputs() {
        std::thread::spawn(|| {
            assert_eq!(
                initialize(json!({"unknown":true}), None).0,
                MUU_RESULT_CREATE_FAILED
            );
            let mut out = MuuBuffer::EMPTY;
            assert_eq!(
                unsafe { muu_core_runtime_status(&mut out) },
                MUU_RESULT_CREATE_FAILED
            );
            assert!(out.data.is_null() && out.len == 0);
            assert_eq!(initialize(json!({}), None).0, MUU_RESULT_CREATE_FAILED);
        })
        .join()
        .unwrap();
        initialize(json!({}), None);
        with_registry(|registry| registry.ready().unwrap().core.invalidate());
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_core_runtime_status(&mut out) },
            MUU_RESULT_CORE_INVALID
        );
        assert!(out.data.is_null() && out.len == 0);
    }

    #[test]
    fn runtime_status_failures_are_local_and_do_not_consume_records_or_learning() {
        let dictionary = test_dictionary();
        let config = json!({"dictionary_path":dictionary.path(),"commit_stream":{
            "schema":"muu.commit-stream.v5","process_nonce":"p","source_commit":"c","build":1
        }});
        let report = initialize(config.clone(), None);
        assert_eq!(report.0, MUU_RESULT_OK);
        let session = create();
        key(session, "kyou");
        assert_eq!(
            request(session, json!({"kind":"command","command":"convert"})).0,
            MUU_RESULT_OK
        );
        assert_eq!(
            request(session, json!({"kind":"command","command":"commit"})).0,
            MUU_RESULT_OK
        );
        let learning = snapshot(None);
        let vocabulary =
            with_registry(|registry| registry.ready().unwrap().core.vocabulary().snapshot());
        for point in ["runtime_status", "buffer_copy"] {
            INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
            let mut out = MuuBuffer::EMPTY;
            assert_eq!(
                unsafe { muu_core_runtime_status(&mut out) },
                MUU_RESULT_INTERNAL
            );
            assert!(out.data.is_null() && out.len == 0);
            assert!(!with_registry(|registry| registry
                .ready()
                .unwrap()
                .core
                .is_invalid()));
            assert_eq!(key(session, "a")["consumed"], true);
            assert_eq!(
                request(session, json!({"kind":"command","command":"cancel"})).0,
                MUU_RESULT_OK
            );
        }
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(unsafe { muu_core_runtime_status(&mut out) }, MUU_RESULT_OK);
        assert!(out.len <= 16 * 1024);
        assert_eq!(
            take(out, false),
            json!({"schema_version":2,"system_dictionary":"available",
            "language_model":"unloaded","commit_stream_enabled":true,
            "language_model_details":{"requested_model":null,"actual_model":null,"route":"disabled",
                "fallback_reason":null,"unavailable_reason":null}})
        );
        assert_eq!(snapshot(None), learning);
        assert!(with_registry(|registry| Arc::ptr_eq(
            &vocabulary,
            &registry.ready().unwrap().core.vocabulary().snapshot()
        )));
        assert_eq!(initialize(config, None), report);
        out = MuuBuffer::EMPTY;
        assert_eq!(unsafe { take_event(&mut out) }, MUU_RESULT_OK);
        assert_eq!(take(out, false)["schema"], "muu.commit-stream.v5");
        out = MuuBuffer::EMPTY;
        assert_eq!(unsafe { take_event(&mut out) }, MUU_RESULT_OK);
        assert!(out.data.is_null() && out.len == 0);
        unsafe { muu_session_destroy(session) };
    }

    #[test]
    fn invalid_initialization_configuration_is_retained() {
        let failed = initialize(json!({"unknown":true}), None);
        assert_eq!(failed.0, MUU_RESULT_CREATE_FAILED);
        assert_eq!(initialize(json!({}), None), failed);
    }

    #[test]
    fn repeated_report_copy_panic_does_not_revoke_initialization_success() {
        let accepted = initialize(json!({}), None);
        for point in ["repeat_report", "buffer_copy"] {
            INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
            let failed = initialize(json!({}), None);
            assert_eq!(failed.0, MUU_RESULT_INTERNAL);
            assert!(!with_registry(|r| r.ready().unwrap().core.is_invalid()));
            assert_eq!(initialize(json!({}), None), accepted);
        }
        unsafe { muu_session_destroy(create()) };
    }

    #[test]
    fn repeated_report_copy_panic_does_not_change_initialization_failure() {
        let rejected = initialize(json!({"unknown":true}), None);
        assert_eq!(
            rejected,
            (
                MUU_RESULT_CREATE_FAILED,
                json!({"code":"invalid_config","params":{}})
            )
        );
        for point in ["repeat_report", "buffer_copy"] {
            INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
            assert_eq!(
                initialize(json!({}), None),
                (
                    MUU_RESULT_INTERNAL,
                    json!({"code":"internal_panic","params":{}})
                )
            );
            assert_eq!(initialize(json!({}), None), rejected);
        }
    }

    #[test]
    fn first_failure_report_preparation_panic_is_retained() {
        for point in ["first_report", "buffer_copy"] {
            std::thread::spawn(move || {
                INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
                let failed = initialize(json!({"unknown":true}), None);
                assert_eq!(
                    failed,
                    (
                        MUU_RESULT_CREATE_FAILED,
                        json!({"code":"core_initialization_panic","params":{}})
                    )
                );
                assert_eq!(initialize(json!({}), None), failed);
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn output_buffer_panic_after_success_is_internal_and_core_remains_usable() {
        initialize(json!({}), None);
        INITIALIZATION_PANIC.with(|p| p.set(Some("buffer_copy")));
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_core_clear_personal_frequency(&mut out) },
            MUU_RESULT_INTERNAL
        );
        assert_eq!(take(out, false)["code"], "internal_panic");
        assert!(!with_registry(|r| r.ready().unwrap().core.is_invalid()));
        assert_eq!(snapshot(None).1["generation"], 1);
        unsafe { muu_session_destroy(create()) };
    }

    #[test]
    fn learning_output_copy_panic_is_internal_and_query_remains_non_consuming() {
        initialize(json!({}), None);
        let expected = snapshot(None);
        INITIALIZATION_PANIC.with(|p| p.set(Some("buffer_copy")));
        let mut data = MuuBuffer::EMPTY;
        let mut version = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_personal_frequency_snapshot(ptr::null(), 0, &mut data, &mut version)
            },
            MUU_RESULT_INTERNAL
        );
        assert!(data.data.is_null() && version.data.is_null());
        assert_eq!(snapshot(None), expected);
        assert!(!with_registry(|r| r.ready().unwrap().core.is_invalid()));
    }

    #[test]
    fn read_and_preparation_panics_return_internal_and_allow_the_next_operation() {
        initialize(json!({}), None);
        let old = with_registry(|r| r.ready().unwrap().core.vocabulary().snapshot());
        let command = r#"{"operation":"upsert","expected_revision":0,"reading":"むう","surface":"New","part_of_speech":"NOUN","priority":"high"}"#.as_bytes();
        let (mut file, mut plan) = (MuuBuffer::EMPTY, MuuBuffer::EMPTY);
        INITIALIZATION_PANIC.with(|p| p.set(Some("user_prepare_second_buffer")));
        assert_eq!(
            unsafe {
                muu_core_user_dictionary_prepare(
                    command.as_ptr(),
                    command.len(),
                    &mut file,
                    &mut plan,
                )
            },
            MUU_RESULT_INTERNAL
        );
        assert!(file.data.is_null() && plan.data.is_null());
        assert_eq!((file.len, plan.len), (0, 0));
        assert!(with_registry(|r| Arc::ptr_eq(
            &old,
            &r.ready().unwrap().core.vocabulary().snapshot()
        )));
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { core_buffer_operation(&mut out, |_| panic!("read or serialize")) },
            MUU_RESULT_INTERNAL
        );
        assert_eq!(take(out, false)["code"], "internal_panic");
        assert!(!with_registry(|r| r.ready().unwrap().core.is_invalid()));
        unsafe {
            muu_session_destroy(create());
        }
    }

    #[test]
    fn versions_are_stable_and_static() {
        assert_eq!(muu_abi_version(), 13);
        assert_eq!(muu_core_version(), muu_core_version());
        assert!(!muu_core_version().is_null());
    }
    #[test]
    fn initialization_reports_degradation_without_loading_a_model() {
        let (status, report) = initialize(
            json!({"dictionary_path":"missing.muudict","conversion_lm_dir":"missing-model"}),
            None,
        );
        assert_eq!(status, 0);
        assert_eq!(report["status"], "degraded");
        assert_eq!(report["degraded"]["dictionary_kana_only"], true);
        assert_eq!(report["degraded"]["language_model_unavailable"], true);
        assert_eq!(report["degraded"]["personal_frequency_invalid"], false);
    }
    #[test]
    fn repeated_initialization_returns_identical_cached_result_and_ignores_initial_data() {
        let config = json!({"user_dictionary":user("First")});
        let first = initialize(config, None);
        let second = initialize(json!({"user_dictionary":user("Ignored")}), Some(b"bad"));
        assert_eq!(first, second);
        with_registry(|r| {
            assert_eq!(
                r.ready()
                    .unwrap()
                    .core
                    .vocabulary()
                    .snapshot()
                    .user
                    .revision,
                1
            )
        });
    }
    #[test]
    fn a_different_dictionary_is_rejected_after_initialization() {
        assert_eq!(initialize(json!({}), None).0, 0);
        let (status, error) = initialize(json!({"dictionary_path":"different"}), None);
        assert_eq!(status, MUU_RESULT_CREATE_FAILED);
        assert_eq!(error["code"], "core_configuration_mismatch");
    }
    #[test]
    fn lm_directory_is_fixed_at_initialization() {
        assert_eq!(initialize(json!({}), None).0, 0);
        assert_eq!(
            initialize(json!({"conversion_lm_dir":"different"}), None).0,
            MUU_RESULT_CREATE_FAILED
        );
    }
    #[test]
    fn lm_loading_mode_is_an_immutable_initialization_resource() {
        let accepted = initialize(json!({"conversion_lm_load_synchronously":true}), None);
        assert_eq!(accepted.0, MUU_RESULT_OK);
        assert_eq!(
            initialize(json!({"conversion_lm_load_synchronously":true}), None),
            accepted
        );
        let rejected = initialize(json!({"conversion_lm_load_synchronously":false}), None);
        assert_eq!(rejected.0, MUU_RESULT_CREATE_FAILED);
        assert_eq!(rejected.1["code"], "core_configuration_mismatch");
    }

    #[test]
    fn recording_enablement_is_fixed_at_initialization() {
        assert_eq!(initialize(json!({}), None).0, 0);
        // A valid configuration must reach resource comparison (docs/src/internals/architecture.md「失敗したとき」), rather
        // than pass this test merely because decoding rejected retired fields.
        let record = json!({"schema":"muu.commit-stream.v5","process_nonce":"p","source_commit":"c","build":1});
        let (status, error) = initialize(json!({"commit_stream":record}), None);
        assert_eq!(status, MUU_RESULT_CREATE_FAILED);
        assert_eq!(error["code"], "core_configuration_mismatch");
    }
    #[test]
    fn initialization_failure_is_retained() {
        let first = initialize(json!({"force_initialize_failure":true}), None);
        assert_eq!(first.0, MUU_RESULT_CREATE_FAILED);
        assert_eq!(initialize(json!({}), None), first);
        INITIALIZATION_PANIC.with(|p| p.set(Some("buffer_copy")));
        assert_eq!(initialize(json!({}), None).0, MUU_RESULT_INTERNAL);
        assert_eq!(initialize(json!({}), None), first);
    }
    #[test]
    fn malformed_learning_degrades_to_empty_instead_of_retrying_initialization() {
        let (status, report) = initialize_present(b"bad");
        assert_eq!(status, 0);
        assert_eq!(report["degraded"]["personal_frequency_invalid"], true);
        assert_eq!(snapshot(None).0["observation_count"], 0);
        unsafe { muu_session_destroy(create()) };
    }
    #[test]
    fn present_empty_learning_file_is_invalid_but_absence_is_accepted() {
        assert_eq!(
            initialize_present(b"").1["degraded"]["personal_frequency_invalid"],
            true
        );
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn startup_learning_requires_matching_dictionary_fingerprint() {
        let Some(path) = std::env::var_os("MUU_REAL_DICT_PATH") else {
            assert!(
                std::env::var_os("MUU_REQUIRE_REAL_ASSETS").is_none(),
                "MUU_REAL_DICT_PATH required"
            );
            return;
        };
        let config = serde_json::to_vec(
            &json!({"dictionary_path":PathBuf::from(path),"user_dictionary_file_state":"missing"}),
        )
        .unwrap();
        let current_sha = "4f5b9a05d483eb13a5685e2cab5b78dacf9e335248b015a3e103918b0a1724a0";
        for (source_sha, token_count, rejected) in [
            (current_sha.to_owned(), 2_038_107, false),
            ("0".repeat(64), 2_038_107, true),
            (current_sha.to_owned(), 2_038_108, true),
        ] {
            let history = json!({"schema_version":2,
                "dictionary_payload_sha256":source_sha,
                "dictionary_token_count":token_count,"observation_count":63,
                "entries":[{"dictionary_token_id":0,"count":255}],
                "rejected_entries":[{"dictionary_token_id":2038106,"count":3}]});
            let bytes = serde_json::to_vec(&history).unwrap();
            let registry = CoreRegistry::default();
            let (code, output) = registry.initialize(Ok(&config), &bytes, true, &[]).unwrap();
            let report: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(code, MUU_RESULT_OK, "{report}");
            assert_eq!(report["degraded"]["personal_frequency_invalid"], rejected);
            assert_eq!(report["degraded"]["personal_frequency_unverifiable"], false);
            assert_eq!(
                report["personal_frequency_version"],
                json!({"generation":0,"revision":0})
            );
            let core = &registry.ready().unwrap().core;
            let update = core.personal_frequency_update(None).unwrap().unwrap();
            let snapshot: Value = serde_json::from_slice(&update.data).unwrap();
            if rejected {
                assert_eq!(snapshot["observation_count"], 0);
                assert_eq!(snapshot["entries"], json!([]));
                assert_eq!(snapshot["rejected_entries"], json!([]));
            } else {
                assert_eq!(snapshot, history);
                assert!(
                    core.personal_frequency_update(Some(update.version))
                        .unwrap()
                        .is_none()
                );
            }
        }
    }
    #[test]
    fn invalid_startup_user_file_degrades_without_poisoning_or_reapplying_startup() {
        for user in [
            json!({"schema_version":99,"entries":[]}),
            json!({"broken":true}),
        ] {
            std::thread::spawn(move || {
                let (code, report) = initialize(json!({"user_dictionary":user}), None);
                assert_eq!(code, MUU_RESULT_OK);
                assert_eq!(report["degraded"]["user_dictionary_invalid"], true);
                assert_eq!(
                    with_registry(|r| r
                        .ready()
                        .unwrap()
                        .core
                        .vocabulary()
                        .snapshot()
                        .user
                        .revision),
                    0
                );
                let mut out = MuuBuffer::EMPTY;
                assert_eq!(
                    unsafe { muu_core_user_dictionary_view(&mut out) },
                    MUU_RESULT_OK
                );
                let view = take(out, false);
                assert_eq!(view["access"], "read_only");
                unsafe {
                    muu_session_destroy(create());
                }
                // Replacing the startup file takes effect next process.
                assert_eq!(
                    initialize(json!({"user_dictionary":self::user("Repaired")}), None).1,
                    report
                );
                let mut out = MuuBuffer::EMPTY;
                assert_eq!(
                    unsafe { muu_core_user_dictionary_view(&mut out) },
                    MUU_RESULT_OK
                );
                assert_eq!(take(out, false), view);
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn dictionary_absence_preserves_learning_without_attempting_to_validate_it() {
        let dictionary = test_dictionary();
        let fingerprint = muu_dict::Dictionary::open(dictionary.path())
            .unwrap()
            .fingerprint();
        let sha = fingerprint
            .payload_sha256()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let valid = serde_json::to_vec(&json!({"schema_version":1,"dictionary_payload_sha256":sha,
            "dictionary_token_count":fingerprint.token_count(),"observation_count":1,
            "entries":[{"dictionary_token_id":0,"count":1}]}))
        .unwrap();
        for path in [None, Some("missing.muudict")] {
            for learning in [valid.clone(), b"not even JSON".to_vec()] {
                std::thread::spawn(move || {
                    let (code, report) =
                        initialize(json!({"dictionary_path":path}), Some(&learning));
                    assert_eq!(code, MUU_RESULT_OK);
                    assert_eq!(report["degraded"]["personal_frequency_invalid"], false);
                    assert_eq!(report["degraded"]["personal_frequency_unverifiable"], true);
                    assert_eq!(report["degraded"]["dictionary_kana_only"], path.is_some());
                    assert_eq!(snapshot(None).0["observation_count"], 0);
                    unsafe { muu_session_destroy(create()) };
                })
                .join()
                .unwrap();
            }
        }
        let (_, report) = initialize(json!({}), None);
        assert_eq!(report["degraded"]["personal_frequency_unverifiable"], false);
        assert_eq!(report["degraded"]["dictionary_kana_only"], false);
    }
    #[test]
    fn initialization_rejects_oversized_config_before_reading() {
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_initialize_with_user_dictionary_file(
                    b"x".as_ptr(),
                    MAX_PROTOCOL_ENVELOPE_BYTES + 1,
                    ptr::null(),
                    0,
                    false,
                    ptr::null(),
                    0,
                    &mut out,
                )
            },
            MUU_RESULT_CREATE_FAILED
        );
        assert_eq!(take(out, false)["code"], "config_too_large");
    }
    #[test]
    fn initialization_rejects_unknown_configuration_fields() {
        assert_eq!(
            initialize(json!({"personal_frequency":{}}), None).1["code"],
            "invalid_config"
        );
    }
    #[test]
    fn session_creation_before_initialization_fails_and_zeros_outputs() {
        let mut session = ptr::dangling_mut();
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_session_create(&mut session, &mut out) },
            MUU_RESULT_CREATE_FAILED
        );
        assert!(session.is_null());
        assert_eq!(take(out, false)["code"], "core_not_initialized");
    }
    #[test]
    fn store_views_before_initialization_fail_without_creating_core() {
        for view in [
            muu_core_user_dictionary_view,
            muu_core_imported_dictionary_view,
            muu_core_postal_code_view,
        ] {
            let mut out = MuuBuffer::EMPTY;
            assert_eq!(unsafe { view(&mut out) }, MUU_RESULT_CREATE_FAILED);
            assert_eq!(take(out, false)["code"], "core_not_initialized");
        }
    }
    #[test]
    fn learning_query_is_non_consuming_and_equal_version_returns_two_empty_buffers() {
        initialize(json!({}), None);
        let first = snapshot(None);
        assert_eq!(snapshot(None), first);
        let received = serde_json::to_vec(&first.1).unwrap();
        let mut data = MuuBuffer::EMPTY;
        let mut version = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_personal_frequency_snapshot(
                    received.as_ptr(),
                    received.len(),
                    &mut data,
                    &mut version,
                )
            },
            0
        );
        assert!(data.data.is_null() && version.data.is_null());
        assert_eq!((data.len, version.len), (0, 0));
    }
    #[test]
    fn clear_returns_new_generation_and_is_visible_without_sessions() {
        initialize(json!({}), None);
        let old = snapshot(None).1;
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(unsafe { muu_core_clear_personal_frequency(&mut out) }, 0);
        let new = take(out, false);
        assert_ne!(old["generation"], new["generation"]);
        assert_eq!(snapshot(Some(old)).1, new);
    }
    // Only the full encoding fails, after a key that both commits and starts
    // the next preedit. The reply is OK with the reduced view, the commit and
    // next character intact, and learning and the record counted once. The
    // same failure without a commit stays INTERNAL with no view.
    #[test]
    fn view_encoding_failure_preserves_commit_next_preedit_and_single_core_effects() {
        let dictionary = test_dictionary();
        initialize(
            json!({"dictionary_path":dictionary.path(),"commit_stream":{
                "schema":"muu.commit-stream.v5","process_nonce":"p","source_commit":"c","build":1
            }}),
            None,
        );
        let session = create();
        key(session, "kyou");
        let (_, converted) = request(session, json!({"kind":"command","command":"convert"}));
        let expected = converted.unwrap()["preedit"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run["text"].as_str().unwrap())
            .collect::<String>();
        FAIL_FULL_VIEW_ENCODING.with(|failure| failure.set(true));
        let (status, output) = request(
            session,
            json!({"kind":"key","key":{
                "event_type":"key_down","keycode":0,"characters_utf8":"a",
                "characters_ignoring_modifiers":"a","modifiers":0,"is_repeat":false
            }}),
        );
        assert_eq!(
            status, MUU_RESULT_OK,
            "encoding must not lose a committed string"
        );
        let view = output.unwrap();
        assert_eq!(view["commit_text"], expected);
        assert_eq!(view["preedit"][0]["text"], "あ");
        assert_eq!(view["caret_utf16"], 1);
        assert_eq!(view["consumed"], true);
        for field in ["segments", "candidates", "error"] {
            assert!(view.get(field).is_none());
        }
        let learning = snapshot(None);
        let mut event = MuuBuffer::EMPTY;
        let mut metadata = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut metadata) },
            MUU_RESULT_OK
        );
        assert_eq!(
            take(metadata, false)["drop_weight"],
            1,
            "one Session commit, not a replay"
        );
        assert_eq!(take(event, false)["schema"], "muu.commit-stream.v5");
        event = MuuBuffer::EMPTY;
        assert_eq!(unsafe { take_event(&mut event) }, MUU_RESULT_OK);
        assert!(event.data.is_null() && event.len == 0);
        assert_eq!(
            snapshot(None),
            learning,
            "encoding does not replay learning"
        );
        // An event without a commit still has the old local INTERNAL result.
        FAIL_FULL_VIEW_ENCODING.with(|failure| failure.set(true));
        let (status, output) = request(session, json!({"kind":"command","command":"snapshot"}));
        assert_eq!(status, MUU_RESULT_INTERNAL);
        assert!(output.is_none());
        assert_eq!(snapshot(None), learning);
        unsafe { muu_session_destroy(session) };
    }

    #[test]
    fn dynamic_view_crosses_the_abi_and_buffer_is_freed_by_rust() {
        initialize(json!({}), None);
        let session = create();
        assert_eq!(key(session, "k")["preedit"][0]["text"], "k");
        assert_eq!(key(session, "a")["preedit"][0]["text"], "か");
        unsafe { muu_session_destroy(session) };
    }
    #[test]
    fn session_panic_is_recoverable_and_does_not_invalidate_the_core() {
        initialize(json!({}), None);
        let a = create();
        let b = create();
        let (status, view) = request(a, json!({"kind":"command","command":"panic_test"}));
        assert_eq!(status, MUU_RESULT_SESSION_INVALID);
        assert_eq!(view.unwrap()["poisoned"], true);
        assert_eq!(
            request(a, json!({"kind":"command","command":"cancel"})).0,
            MUU_RESULT_SESSION_INVALID
        );
        assert_eq!(key(b, "a")["preedit"][0]["text"], "あ");
        unsafe {
            muu_session_destroy(a);
            muu_session_destroy(b);
            muu_session_destroy(create());
        }
    }
    #[test]
    fn unicode_projection_survives_the_ffi_buffer_contract() {
        let source = "A👨‍👩‍👧‍👦e\u{301}🇯🇵";
        let preedit = muu_romaji::Preedit {
            display: source.to_owned(),
            cursor: source.chars().count(),
            reading_hiragana: source.to_owned(),
            hard_boundaries: Vec::new(),
            pending_latin: String::new(),
        };
        let mut out = MuuBuffer::EMPTY;
        write_view(&mut out, muu_session::view_from_preedit(preedit, true)).unwrap();
        let view = take(out, true);
        let text = view["preedit"][0]["text"].as_str().unwrap();
        assert_eq!(text, "A👨‍👩‍👧‍👦é🇯🇵");
        assert_eq!(
            view["preedit"][0]["utf16_len"].as_u64().unwrap() as usize,
            text.encode_utf16().count()
        );
        assert_eq!(
            view["caret_utf16"].as_u64().unwrap() as usize,
            text.encode_utf16().count()
        );
    }
    #[test]
    fn invalid_arguments_are_rejected_after_zeroing_outputs() {
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_session_handle_event(ptr::null_mut(), ptr::null(), 0, &mut out) },
            MUU_RESULT_NULL_HANDLE
        );
        assert!(out.data.is_null());
        assert_eq!(
            unsafe {
                muu_core_initialize_with_user_dictionary_file(
                    ptr::null(),
                    1,
                    ptr::null(),
                    0,
                    false,
                    ptr::null(),
                    0,
                    &mut out,
                )
            },
            MUU_RESULT_BAD_ARG
        );
        assert_eq!(
            unsafe { muu_core_personal_frequency_snapshot(ptr::null(), 0, &mut out, &mut out) },
            MUU_RESULT_BAD_ARG
        );
        unsafe {
            muu_session_destroy(ptr::null_mut());
            muu_buffer_free(out);
        }
    }

    #[test]
    fn empty_weighted_commit_stream_take_returns_two_empty_buffers() {
        initialize(json!({}), None);
        let (mut out, mut weight) = (MuuBuffer::EMPTY, MuuBuffer::EMPTY);
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut out, &mut weight) },
            MUU_RESULT_OK
        );
        assert!(out.data.is_null() && weight.data.is_null());
        assert_eq!((out.len, weight.len), (0, 0));
    }
    #[test]
    fn invalid_core_is_terminal_for_all_sessions_and_core_calls() {
        initialize(json!({}), None);
        let a = create();
        let b = create();
        key(a, "k");
        let mut out = MuuBuffer::EMPTY;
        // Core tests cover the real publication panic guard. Inject its
        // terminal outcome here to exercise every FFI consumer.
        assert_eq!(
            unsafe {
                core_buffer_operation(&mut out, |core| {
                    core.invalidate();
                    panic!("terminal Core fixture");
                })
            },
            MUU_RESULT_CORE_INVALID
        );
        assert_eq!(take(out, false)["code"], "core_invalid");
        for session in [a, b] {
            assert_eq!(
                request(session, json!({"kind":"command","command":"cancel"})).0,
                MUU_RESULT_CORE_INVALID
            );
        }
        assert_eq!(initialize(json!({}), None).0, MUU_RESULT_CORE_INVALID);
        let mut session = ptr::null_mut();
        out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_session_create(&mut session, &mut out) },
            MUU_RESULT_CORE_INVALID
        );
        assert!(session.is_null());
        assert_eq!(take(out, false)["code"], "core_invalid");
        unsafe {
            muu_session_destroy(a);
            muu_session_destroy(b);
        }
    }
    fn test_dictionary() -> tempfile::NamedTempFile {
        use muu_dict::{
            CoarsePos, DictionaryBuilder, LeftContextId, LexiconEntry, RightContextId,
            UnknownDefinition,
        };
        let mut builder =
            DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
        builder.provenance(b"name = \"startup-validation-test\"\n");
        builder.add_entry(LexiconEntry {
            reading: "きょう".into(),
            surface: "今日".into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: CoarsePos::Noun.id(),
            right_pos_id: CoarsePos::Noun.id(),
            word_cost: 0,
        });
        builder.add_unknown(UnknownDefinition {
            word_cost: 10000,
            ..UnknownDefinition::default()
        });
        let dictionary = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(dictionary.path(), builder.build().unwrap()).unwrap();
        dictionary
    }

    fn malformed_learning_case(mutate: impl FnOnce(&mut Value)) {
        use muu_dict::Dictionary;
        let dictionary = test_dictionary();
        let fingerprint = Dictionary::open(dictionary.path()).unwrap().fingerprint();
        let sha = fingerprint
            .payload_sha256()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut learning = json!({"schema_version":1,"dictionary_payload_sha256":sha,"dictionary_token_count":fingerprint.token_count(),"observation_count":1,"entries":[{"dictionary_token_id":0,"count":1}]});
        let control = Core::new(CoreConfig {
            dictionary_path: Some(dictionary.path()),
        })
        .unwrap();
        control
            .initialize_data(
                StoredFileState::Missing,
                &[],
                &serde_json::from_value(learning.clone()).unwrap(),
            )
            .unwrap();
        mutate(&mut learning);
        let bytes = serde_json::to_vec(&learning).unwrap();
        let (code, result) = initialize(
            json!({"dictionary_path":dictionary.path(),"user_dictionary_file_state":"missing"}),
            Some(&bytes),
        );
        assert_eq!(code, MUU_RESULT_OK);
        assert_eq!(result["degraded"]["personal_frequency_invalid"], true);
        let data = snapshot(None).0;
        assert_eq!(data["observation_count"], 0);
        assert_eq!(data["entries"], json!([]));
        unsafe { muu_session_destroy(create()) };
    }
    #[test]
    fn startup_learning_requires_each_v1_root_key_including_nullable_authority() {
        for field in [
            "schema_version",
            "dictionary_payload_sha256",
            "dictionary_token_count",
            "observation_count",
            "entries",
        ] {
            // Each initial file must reach a fresh test-only registry.
            std::thread::spawn(move || {
                malformed_learning_case(|learning| {
                    // Empty learning with explicit null authority is valid;
                    // missing nullable keys must fail decoding, not merely
                    // fail authority validation for a populated snapshot.
                    learning["dictionary_payload_sha256"] = Value::Null;
                    learning["dictionary_token_count"] = Value::Null;
                    learning["observation_count"] = json!(0);
                    learning["entries"] = json!([]);
                    learning.as_object_mut().unwrap().remove(field).unwrap();
                });
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn startup_learning_requires_both_v1_entry_keys() {
        for field in ["dictionary_token_id", "count"] {
            std::thread::spawn(move || {
                malformed_learning_case(|learning| {
                    learning["entries"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove(field)
                        .unwrap();
                });
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn startup_learning_rejects_both_missing_authority_keys() {
        let missing = json!({"schema_version":1,"observation_count":0,"entries":[]});
        let (code, report) = initialize_present(&serde_json::to_vec(&missing).unwrap());
        assert_eq!(code, MUU_RESULT_OK);
        assert_eq!(report["degraded"]["personal_frequency_invalid"], true);
        assert_eq!(snapshot(None).0["entries"], json!([]));
    }

    #[test]
    fn startup_learning_accepts_explicit_null_authority_keys() {
        let learning = json!({"schema_version":1,"dictionary_payload_sha256":null,
            "dictionary_token_count":null,"observation_count":0,"entries":[]});
        let (code, report) = initialize_present(&serde_json::to_vec(&learning).unwrap());
        assert_eq!(code, MUU_RESULT_OK);
        assert_eq!(report["degraded"]["personal_frequency_invalid"], false);
        let mut expected = learning;
        expected["schema_version"] = json!(2);
        expected["rejected_entries"] = json!([]);
        assert_eq!(snapshot(None).0, expected);
    }

    #[test]
    fn startup_v2_learning_accepts_empty_rejections() {
        let learning = json!({"schema_version":2,"dictionary_payload_sha256":null,
            "dictionary_token_count":null,"observation_count":0,"entries":[],"rejected_entries":[]});
        let (code, report) = initialize_present(&serde_json::to_vec(&learning).unwrap());
        assert_eq!(code, MUU_RESULT_OK);
        assert_eq!(report["degraded"]["personal_frequency_invalid"], false);
        assert_eq!(snapshot(None).0, learning);
    }
    #[test]
    fn startup_v2_learning_requires_rejected_entries() {
        malformed_learning_case(|data| data["schema_version"] = json!(2));
    }
    #[test]
    fn startup_v2_learning_rejects_invalid_rejection_counts() {
        malformed_learning_case(|data| {
            data["schema_version"] = json!(2);
            data["rejected_entries"] = json!([{"dictionary_token_id":0,"count":0}]);
        });
    }
    #[test]
    fn startup_learning_unknown_root_fields_are_rejected_by_rust() {
        malformed_learning_case(|data| data["future_authority"] = json!(true));
    }
    #[test]
    fn startup_learning_unknown_entry_fields_are_rejected_by_rust() {
        malformed_learning_case(|data| data["entries"][0]["future_authority"] = json!(true));
    }
    #[test]
    fn startup_learning_schema_drift_is_rejected_by_rust() {
        malformed_learning_case(|data| data["schema_version"] = json!(3));
    }
    #[test]
    fn startup_learning_count_overflow_is_rejected_by_rust() {
        malformed_learning_case(|data| data["entries"][0]["count"] = json!(256));
    }
    #[test]
    fn ready_calls_do_not_wait_for_an_in_progress_initialization() {
        let registry = Arc::new(CoreRegistry::default());
        let started = std::sync::Barrier::new(2);
        let release = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let registry_ref = &registry;
            let started_ref = &started;
            let release_ref = &release;
            let initializing = scope.spawn(move || {
                registry_ref.initialized.get_or_init(|| {
                    started_ref.wait();
                    release_ref.wait();
                    let config = InitializationConfig::default();
                    CoreInitialization {
                        resources: Some(config.resources()),
                        result: build_core(config, &[], false, &[]),
                        failure_report: Vec::new(),
                    }
                });
            });
            started.wait();
            assert_eq!(
                registry.ready().err().unwrap(),
                (MUU_RESULT_CREATE_FAILED, "core_not_initialized")
            );
            release.wait();
            initializing.join().unwrap();
            assert!(registry.ready().is_ok());
        });
    }

    #[test]
    fn model_presence_is_reported_without_attempting_to_load_model_contents() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("model.safetensors"), b"not a model").unwrap();
        std::fs::write(root.path().join("tokenizer.json"), b"not a tokenizer").unwrap();
        let (code, report) = initialize(json!({"conversion_lm_dir":root.path()}), None);
        assert_eq!(code, MUU_RESULT_OK);
        assert_eq!(report["degraded"]["language_model_unavailable"], false);
    }
    #[test]
    fn weighted_delivery_is_atomic_and_returns_debt_only_to_future_records() {
        let dictionary = test_dictionary();
        assert_eq!(initialize(json!({"dictionary_path":dictionary.path(),"commit_stream":{
            "schema":"muu.commit-stream.v5","process_nonce":"p","source_commit":"c","build":1
        }}), None).0, MUU_RESULT_OK);
        let session = create();
        let commit = || {
            key(session, "kyou");
            assert_eq!(
                request(session, json!({"kind":"command","command":"convert"})).0,
                0
            );
            assert_eq!(
                request(session, json!({"kind":"command","command":"commit"})).0,
                0
            );
        };
        let report = |value: serde_json::Value| {
            let bytes = serde_json::to_vec(&value).unwrap();
            unsafe { muu_core_report_commit_stream_dropped(bytes.as_ptr(), bytes.len()) }
        };
        commit();
        let learning = snapshot(None);
        for point in ["buffer_copy", "delivery_weight"] {
            INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
            let mut event = MuuBuffer::EMPTY;
            let mut weight = MuuBuffer::EMPTY;
            assert_eq!(
                unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
                MUU_RESULT_INTERNAL
            );
            assert!(
                event.data.is_null() && event.len == 0 && weight.data.is_null() && weight.len == 0
            );
        }
        let mut event = MuuBuffer::EMPTY;
        let mut weight = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
            0
        );
        assert_eq!(take(event, false)["dropped_records"], 0);
        assert_eq!(
            take(weight, false),
            json!({"schema_version":1,"drop_weight":1})
        );
        event = MuuBuffer::EMPTY;
        weight = MuuBuffer::EMPTY;
        commit(); // Earlier queued record must not acquire returned debt.
        INITIALIZATION_PANIC.with(|p| p.set(Some("delivery_report")));
        assert_eq!(
            report(json!({"schema_version":1,"drop_weight":99})),
            MUU_RESULT_INTERNAL
        );
        for bad in [
            json!({"schema_version":2,"drop_weight":1}),
            json!({"schema_version":1,"drop_weight":0}),
            json!({"schema_version":1,"drop_weight":-1}),
            json!({"schema_version":1,"drop_weight":1.5}),
            json!({"schema_version":1,"drop_weight":1,"extra":0}),
        ] {
            assert_eq!(report(bad), MUU_RESULT_BAD_ARG);
        }
        assert_eq!(report(json!({"schema_version":1,"drop_weight":4})), 0);
        commit();
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
            0
        );
        assert_eq!(take(event, false)["dropped_records"], 0);
        assert_eq!(take(weight, false)["drop_weight"], 1);
        event = MuuBuffer::EMPTY;
        weight = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
            0
        );
        assert_eq!(take(event, false)["dropped_records"], 4);
        assert_eq!(take(weight, false)["drop_weight"], 5);
        event = MuuBuffer::EMPTY;
        weight = MuuBuffer::EMPTY;
        for n in [(1u64 << 53) + 1, u64::MAX] {
            assert_eq!(report(json!({"schema_version":1,"drop_weight":n})), 0);
            commit();
            assert_eq!(
                unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
                0
            );
            assert_eq!(take(event, false)["dropped_records"], n);
            assert_eq!(
                take(weight, false)["drop_weight"].as_u64(),
                Some(n.saturating_add(1))
            );
            event = MuuBuffer::EMPTY;
            weight = MuuBuffer::EMPTY;
        }
        assert_eq!(snapshot(None), learning);
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
            0
        );
        assert!(event.data.is_null() && event.len == 0 && weight.data.is_null() && weight.len == 0);
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut event) },
            MUU_RESULT_BAD_ARG
        );
        unsafe { muu_session_destroy(session) };
    }

    #[test]
    fn weighted_delivery_uninitialized_and_invalid_do_not_initialize_or_return_buffers() {
        let mut event = MuuBuffer::EMPTY;
        let mut weight = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
            MUU_RESULT_CREATE_FAILED
        );
        let bytes = br#"{"schema_version":1,"drop_weight":1}"#;
        assert_eq!(
            unsafe { muu_core_report_commit_stream_dropped(bytes.as_ptr(), bytes.len()) },
            MUU_RESULT_CREATE_FAILED
        );
        assert_eq!(
            unsafe { muu_core_report_commit_stream_dropped(bytes.as_ptr(), 1025) },
            MUU_RESULT_BAD_ARG
        );
        assert_eq!(initialize(json!({}), None).0, 0);
        with_registry(|registry| registry.ready().unwrap().core.invalidate());
        assert_eq!(
            unsafe { muu_core_take_commit_stream_event_with_weight(&mut event, &mut weight) },
            MUU_RESULT_CORE_INVALID
        );
        assert_eq!(
            unsafe { muu_core_report_commit_stream_dropped(bytes.as_ptr(), bytes.len()) },
            MUU_RESULT_CORE_INVALID
        );
        assert!(event.data.is_null() && event.len == 0 && weight.data.is_null() && weight.len == 0);
    }
}
