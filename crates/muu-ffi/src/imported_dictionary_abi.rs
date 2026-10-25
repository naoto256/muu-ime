//! The imported dictionary named-file protocol. Every step is private; only
//! output-free ACK operations publish. This keeps host IO and recovery outside
//! Core locks.

use super::*;
use muu_session::{
    ImportedDictionaryEditCommand, ImportedDictionaryEditError as EditError,
    ImportedDictionaryFinishCommand, ImportedDictionaryLoadCommand,
    ImportedDictionarySupplyCommand, ImportedDictionaryTicket, MAX_IMPORTED_CATALOG_BYTES,
    MAX_IMPORTED_COMMAND_BYTES, MAX_IMPORTED_REPLY_BYTES, MAX_IMPORTED_SOURCE_BYTES,
    MAX_IMPORTED_TICKET_BYTES, PreparedImportedDictionaryReply,
};

// A revision overflow is INTERNAL: nothing was published and the edit was
// not at fault. Every other rejection of the edit is DECODE_ERR.
// RevisionOverflow is the one INTERNAL failure with a body. Its code is
// `revision_overflow`. In ABI 12 the step dropped the body and the host fell
// back to a generic code; reporting the real code is the approved ABI 13 code
// exception for Imported and Postal. Swift keeps the earlier presentation and
// recovery for it.
pub(super) fn edit_error(error: EditError) -> PrivateFailure {
    let failure = edit_failure(error.class(), error.code());
    if matches!(error, EditError::RevisionOverflow) {
        PrivateFailure::known(failure.status, failure.code)
    } else {
        failure
    }
}
fn decode<T: DeserializeOwned>(bytes: &[u8], limit: usize) -> Result<T, (i32, &'static str)> {
    bounded_decode(
        bytes,
        limit,
        "imported_dictionary_command_too_large",
        "invalid_imported_dictionary_command",
    )
}

/// Return Core's display-only imported dictionary management view. No file
/// is interpreted, written or collected by the host as a result of this call.
///
/// # Safety
/// `out_view` is writable; free it with muu_buffer_free on every result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_view(out_view: *mut MuuBuffer) -> i32 {
    unsafe {
        core_buffer_operation(out_view, |core| {
            let bytes = serde_json::to_vec(&core.imported_dictionary_view())
                .map_err(|_| (MUU_RESULT_INTERNAL, "imported_dictionary_view_failed"))?;
            if bytes.len() > MAX_IMPORTED_REPLY_BYTES {
                return Err((MUU_RESULT_INTERNAL, "imported_dictionary_output_too_large"));
            }
            Ok(bytes)
        })
    }
}

/// Start a private load from current pointer bytes. The step requests only a
/// constrained generation/blob hash or the fixed legacy catalog name. Missing
/// and unreadable files are distinct; a present empty file is not absence.
///
/// # Safety
/// Borrowed command is at most 64 KiB, pointer at most 128 bytes. Outputs are
/// distinct writable buffers; free both on every result. INTERNAL is unpublished.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_load_begin(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    out_generation: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        step(
            command,
            command_len,
            data,
            len,
            128,
            out_generation,
            out_plan,
            |core, command, bytes| {
                core.begin_imported_dictionary_load(
                    decode::<ImportedDictionaryLoadCommand>(command, MAX_IMPORTED_COMMAND_BYTES)?,
                    bytes,
                )
                .map_err(step_error)
            },
        )
    }
}

/// Supply a requested startup file, at most one blob (16 MiB) or catalog
/// (32 MiB). The step returns the next constrained request or a ready plan.
/// Merely loading never asks the host to save or collect files.
///
/// # Safety
/// Inputs are borrowed. Outputs are distinct and writable; free both on every
/// result. A command names a pending request, not an arbitrary host path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_load_supply(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    out_generation: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        step(
            command,
            command_len,
            data,
            len,
            MAX_IMPORTED_CATALOG_BYTES,
            out_generation,
            out_plan,
            |core, command, bytes| {
                core.supply_imported_dictionary_load(
                    decode::<ImportedDictionarySupplyCommand>(command, MAX_IMPORTED_COMMAND_BYTES)?,
                    bytes,
                )
                .map_err(step_error)
            },
        )
    }
}

/// Prepare import, replacement, enable, disable or removal without publishing.
/// Steps request a necessary existing blob or stage one canonical blob. Once
/// all stages are acknowledged, returns generation bytes and a final plan.
///
/// # Safety
/// Command is borrowed JSON <=64 KiB, source <=16 MiB. Outputs must be distinct
/// writable buffers, freed on every result. Known revision overflow is
/// INTERNAL with a reason in out_plan; panic or allocation failure leaves
/// both empty. All failures are unpublished.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_prepare(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    out_generation: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        step(
            command,
            command_len,
            data,
            len,
            MAX_IMPORTED_SOURCE_BYTES,
            out_generation,
            out_plan,
            |core, command, bytes| {
                core.prepare_imported_dictionary(
                    decode::<ImportedDictionaryEditCommand>(command, MAX_IMPORTED_COMMAND_BYTES)?,
                    bytes,
                )
                .map_err(step_error)
            },
        )
    }
}

/// Continue a private edit with a requested file or acknowledgment that the
/// single staged canonical blob has been saved. Acknowledgment frees its bytes
/// before Core prepares the next blob; whole catalogs are never transferred.
///
/// # Safety
/// Borrowed command <=64 KiB and file <=32 MiB. Outputs are distinct writable
/// buffers; free both on every result. No publication occurs in this function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_prepare_supply(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    out_generation: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        step(
            command,
            command_len,
            data,
            len,
            MAX_IMPORTED_CATALOG_BYTES,
            out_generation,
            out_plan,
            |core, command, bytes| {
                core.supply_imported_dictionary_prepare(
                    decode::<ImportedDictionarySupplyCommand>(command, MAX_IMPORTED_COMMAND_BYTES)?,
                    bytes,
                )
                .map_err(step_error)
            },
        )
    }
}

struct StepError {
    failure: PrivateFailure,
    params: serde_json::Value,
}
impl From<(i32, &'static str)> for StepError {
    fn from((status, code): (i32, &'static str)) -> Self {
        Self {
            failure: (status, code).into(),
            params: serde_json::Value::Null,
        }
    }
}
fn step_error(error: EditError) -> StepError {
    let params = match &error {
        EditError::File(error) => serde_json::json!({"line":error.line,"field":error.field}),
        _ => serde_json::Value::Null,
    };
    StepError {
        failure: edit_error(error),
        params,
    }
}
type StepResult = Result<PreparedImportedDictionaryReply, StepError>;
#[allow(clippy::too_many_arguments)] // One helper preserves the two-output ownership contract.
unsafe fn step(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    file_limit: usize,
    out_generation: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
    operation: impl FnOnce(&Core, &[u8], &[u8]) -> StepResult,
) -> i32 {
    unsafe {
        clear_outputs([out_generation, out_plan]);
    }
    if !distinct_outputs([out_generation, out_plan])
        || (command.is_null() && command_len > 0)
        || (data.is_null() && len > 0)
        || len > file_limit
    {
        return MUU_RESULT_BAD_ARG;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| {
            let core = &registry.ready().map_err(StepError::from)?.core;
            if command_len > MAX_IMPORTED_COMMAND_BYTES {
                return Err((
                    MUU_RESULT_DECODE_ERR,
                    "imported_dictionary_command_too_large",
                )
                    .into());
            }
            let prepared = operation(core, unsafe { borrowed(command, command_len) }, unsafe {
                borrowed(data, len)
            })?;
            let cancel_core = core.clone();
            let reservation = PreparedOutputGuard::new(prepared.ticket, move |ticket| {
                let _ = cancel_core.cancel_imported_dictionary(&ticket);
            });
            Ok(stage_outputs(
                [prepared.generation_bytes, prepared.plan_json],
                reservation,
                |index| {
                    #[cfg(test)]
                    if index == 1 {
                        initialization_test_point("imported_prepare_second_buffer");
                    }
                    #[cfg(not(test))]
                    let _ = index;
                },
            ))
        })
    }));
    match result {
        Ok(Ok(staged)) => {
            unsafe {
                staged.transfer([out_generation, out_plan]);
            }
            MUU_RESULT_OK
        }
        other => {
            let error: StepError = match other {
                Ok(Err(error)) => error,
                Err(_) => StepError {
                    failure: PrivateFailure::unwind(core_panic_result()),
                    params: serde_json::Value::Null,
                },
                _ => unreachable!(),
            };
            unsafe {
                write_private_failure(out_plan, error.failure, || {
                    serde_json::to_vec(&serde_json::json!({
                        "code":error.failure.code,"params":error.params
                    }))
                    .expect("bounded error JSON")
                })
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobCommand {
    ticket: ImportedDictionaryTicket,
    sha256: String,
}

/// Copy the one pending canonical blob identified by ticket and SHA. Never
/// fabricates a path; bytes are released by the subsequent written step.
///
/// # Safety
/// Borrowed command <=64 KiB. `out_blob` is writable and always freed by caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_prepared_blob(
    data: *const u8,
    len: usize,
    out_blob: *mut MuuBuffer,
) -> i32 {
    if data.is_null() && len > 0 {
        if !out_blob.is_null() {
            unsafe { out_blob.write(MuuBuffer::EMPTY) }
        }
        return MUU_RESULT_BAD_ARG;
    }
    unsafe {
        core_buffer_operation(out_blob, |core| {
            let command: BlobCommand = decode_input(
                data,
                len,
                MAX_IMPORTED_COMMAND_BYTES,
                "imported_dictionary_command_too_large",
                "invalid_imported_dictionary_command",
            )?;
            core.imported_dictionary_prepared_blob(&command.ticket, &command.sha256)
                .map_err(|error| {
                    let failure = edit_error(error);
                    (failure.status, failure.code)
                })
        })
    }
}

// Shared body of the output-free ACKs. Readiness precedes decoding so a bad
// ticket can never turn an ambiguous CORE_INVALID into a definite rejection.
unsafe fn ack<T: DeserializeOwned>(
    data: *const u8,
    len: usize,
    limit: usize,
    operation: impl FnOnce(&Core, T) -> Result<(), EditError>,
) -> i32 {
    unsafe {
        acknowledge_ready(data, len, |core| {
            let command = decode_input(
                data,
                len,
                limit,
                "imported_dictionary_command_too_large",
                "invalid_imported_dictionary_command",
            )
            .map_err(|e| e.0)?;
            operation(core, command).map_err(|e| edit_error(e).status)
        })
    }
}

/// Publish after immutable blobs and generation have been saved. No pointer
/// has changed yet. OK is a final publication ACK; INTERNAL / revision mismatch
/// guarantee this call did not publish. CORE_INVALID must never be guessed.
///
/// # Safety
/// Borrowed ticket JSON <=1 KiB. No output allocation occurs after publication.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_publish(data: *const u8, len: usize) -> i32 {
    unsafe {
        ack(data, len, MAX_IMPORTED_TICKET_BYTES, |core, ticket| {
            core.publish_imported_dictionary(&ticket)
        })
    }
}

/// Restore old Imported content after a pointer-save failure. Management
/// revision advances again (r+2); other layers and learning are preserved.
/// INTERNAL / mismatch are unpublished; CORE_INVALID forbids blind recovery.
///
/// # Safety
/// Borrowed ticket JSON <=1 KiB. This function returns only an i32 ACK.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_rollback(data: *const u8, len: usize) -> i32 {
    unsafe {
        ack(data, len, MAX_IMPORTED_TICKET_BYTES, |core, ticket| {
            core.rollback_imported_dictionary(&ticket)
        })
    }
}

/// Resolve load, durable/warned new pointer, or rollback. Only a successful
/// durable-new-pointer finish grants the host GC authority for that revision's
/// reference set. Reservation remains through GC and main-thread completion.
///
/// # Safety
/// Borrowed finish JSON <=64 KiB. No output is allocated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_finish(data: *const u8, len: usize) -> i32 {
    unsafe {
        ack(
            data,
            len,
            MAX_IMPORTED_COMMAND_BYTES,
            |core, command: ImportedDictionaryFinishCommand| {
                core.finish_imported_dictionary(command)
            },
        )
    }
}

/// Cancel unpublished work or release a resolved reservation. Repeated cancel
/// is harmless; an unresolved publication returns BUSY instead of discarding
/// recovery authority. Invalid Core releases retained private work and returns 7.
///
/// # Safety
/// Borrowed ticket JSON <=1 KiB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_cancel(data: *const u8, len: usize) -> i32 {
    unsafe {
        acknowledge(data, len, |registry| {
            let initialized = registry
                .initialized
                .get()
                .ok_or(MUU_RESULT_CREATE_FAILED)?
                .result
                .as_ref()
                .map_err(|_| MUU_RESULT_CREATE_FAILED)?;
            if initialized.core.is_invalid() {
                initialized.core.discard_imported_dictionary_preparation();
                return Err(MUU_RESULT_CORE_INVALID);
            }
            let ticket = decode_input(
                data,
                len,
                MAX_IMPORTED_TICKET_BYTES,
                "imported_dictionary_command_too_large",
                "invalid_imported_dictionary_command",
            )
            .map_err(|e| e.0)?;
            initialized
                .core
                .cancel_imported_dictionary(&ticket)
                .map_err(|e| edit_error(e).status)
        })
    }
}

/// Return canonical exchange-template bytes. The host only selects a
/// destination and writes them; format and content are defined by Rust.
///
/// # Safety
/// `out_file` is writable and freed with muu_buffer_free on every result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_imported_dictionary_template(out_file: *mut MuuBuffer) -> i32 {
    unsafe {
        core_buffer_operation(
            out_file,
            |_| Ok(muu_session::imported_dictionary_template()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    type StepFn = unsafe extern "C" fn(
        *const u8,
        usize,
        *const u8,
        usize,
        *mut MuuBuffer,
        *mut MuuBuffer,
    ) -> i32;
    fn bytes(buffer: MuuBuffer) -> Vec<u8> {
        let result = if buffer.len == 0 {
            vec![]
        } else {
            unsafe { slice::from_raw_parts(buffer.data, buffer.len) }.to_vec()
        };
        unsafe { muu_buffer_free(buffer) };
        result
    }
    fn call(function: StepFn, command: Value, data: &[u8]) -> (i32, Vec<u8>, Vec<u8>) {
        let command = serde_json::to_vec(&command).unwrap();
        let (mut generation, mut plan) = (MuuBuffer::EMPTY, MuuBuffer::EMPTY);
        let status = unsafe {
            function(
                command.as_ptr(),
                command.len(),
                data.as_ptr(),
                data.len(),
                &mut generation,
                &mut plan,
            )
        };
        (status, bytes(generation), bytes(plan))
    }
    fn ack_call(function: unsafe extern "C" fn(*const u8, usize) -> i32, value: &Value) -> i32 {
        let bytes = serde_json::to_vec(value).unwrap();
        unsafe { function(bytes.as_ptr(), bytes.len()) }
    }
    fn initialize() {
        let config = br#"{"user_dictionary_file_state":"missing"}"#;
        let mut result = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_initialize_with_user_dictionary_file(
                    config.as_ptr(),
                    config.len(),
                    ptr::null(),
                    0,
                    false,
                    ptr::null(),
                    0,
                    &mut result,
                )
            },
            0
        );
        bytes(result);
    }
    fn empty() {
        initialize();
        let (status, generation, plan) = call(
            muu_core_imported_dictionary_load_begin,
            json!({"expected_revision":0,"file_state":"missing","now_unix_ms":123}),
            &[],
        );
        assert_eq!(status, 0);
        assert!(generation.is_empty());
        let step: Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(step["request"]["kind"], "legacy_catalog");
        let (status, _, plan) = call(
            muu_core_imported_dictionary_load_supply,
            json!({"response":"read","ticket":step["ticket"],"request_id":step["request"]["request_id"],"file_state":"missing"}),
            &[],
        );
        assert_eq!(status, 0);
        let step: Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(step["state"], "ready");
        assert_eq!(
            ack_call(muu_core_imported_dictionary_publish, &step["ticket"]),
            0
        );
        assert_eq!(
            ack_call(
                muu_core_imported_dictionary_finish,
                &json!({"ticket":step["ticket"],"outcome":"loaded","revision":1,"generation_sha256":null})
            ),
            0
        );
        assert_eq!(
            ack_call(muu_core_imported_dictionary_cancel, &step["ticket"]),
            0
        );
    }
    fn import_command() -> Value {
        json!({"action":"import","expected_revision":1,"source_file_name":"fixture.muudict","imported_at_unix_ms":456,"enabled":true})
    }
    fn published_import() -> Value {
        empty();
        let (status, _, plan) = call(
            muu_core_imported_dictionary_prepare,
            import_command(),
            &muu_session::imported_dictionary_template(),
        );
        assert_eq!(status, 0);
        let step: Value = serde_json::from_slice(&plan).unwrap();
        let (status, _, plan) = call(
            muu_core_imported_dictionary_prepare_supply,
            json!({"response":"written","ticket":step["ticket"],"sha256":step["write_blob"]["sha256"]}),
            &[],
        );
        assert_eq!(status, 0);
        let step: Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(
            ack_call(muu_core_imported_dictionary_publish, &step["ticket"]),
            0
        );
        step
    }
    // A new edit while the slot is reserved answers BUSY in both the status
    // and the reason, with no generation output.
    fn reject_new_edit_while_reserved() {
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(unsafe { muu_core_imported_dictionary_view(&mut out) }, 0);
        let view: Value = serde_json::from_slice(&bytes(out)).unwrap();
        let (status, generation, plan) = call(
            muu_core_imported_dictionary_prepare,
            json!({"action":"remove","expected_revision":view["management_revision"],
                   "dictionary_id":"fixture.a"}),
            &[],
        );
        assert_eq!(status, MUU_RESULT_BUSY);
        assert!(generation.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&plan).unwrap()["code"],
            "busy"
        );
    }
    // Through the C ABI: after publish the next edit answers BUSY (9), finish
    // then succeeds with 0, the next edit is still refused, and cancel
    // releases the slot.
    #[test]
    fn new_edit_after_publish_is_busy_and_finish_remains_valid() {
        let step = published_import();
        reject_new_edit_while_reserved();
        assert_eq!(
            ack_call(
                muu_core_imported_dictionary_finish,
                &json!({"ticket":step["ticket"],"outcome":"new_pointer_durable","revision":2,"generation_sha256":step["ready"]["generation_sha256"]})
            ),
            0
        );
        reject_new_edit_while_reserved();
        assert_eq!(
            ack_call(muu_core_imported_dictionary_cancel, &step["ticket"]),
            0
        );
    }
    // The rollback exit through the C ABI: rollback and its finish both succeed
    // while a new edit keeps answering BUSY, and cancel releases the slot.
    #[test]
    fn new_edit_after_publish_is_busy_and_rollback_remains_valid() {
        let step = published_import();
        reject_new_edit_while_reserved();
        assert_eq!(
            ack_call(muu_core_imported_dictionary_rollback, &step["ticket"]),
            0
        );
        reject_new_edit_while_reserved();
        assert_eq!(
            ack_call(
                muu_core_imported_dictionary_finish,
                &json!({"ticket":step["ticket"],"outcome":"rolled_back","revision":3,"generation_sha256":null})
            ),
            0
        );
        assert_eq!(
            ack_call(muu_core_imported_dictionary_cancel, &step["ticket"]),
            0
        );
    }
    #[test]
    fn byte_load_and_edit_step_staging_publish_and_finish_cross_the_abi() {
        empty();
        let source = muu_session::imported_dictionary_template();
        let (status, generation, plan) = call(
            muu_core_imported_dictionary_prepare,
            import_command(),
            &source,
        );
        assert_eq!(status, 0);
        assert!(generation.is_empty());
        let step: Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(step["state"], "needs_blob");
        let ticket = &step["ticket"];
        let command =
            serde_json::to_vec(&json!({"ticket":ticket,"sha256":step["write_blob"]["sha256"]}))
                .unwrap();
        let mut blob = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_imported_dictionary_prepared_blob(
                    command.as_ptr(),
                    command.len(),
                    &mut blob,
                )
            },
            0
        );
        assert_eq!(bytes(blob), source);
        let (status, generation, plan) = call(
            muu_core_imported_dictionary_prepare_supply,
            json!({"response":"written","ticket":ticket,"sha256":step["write_blob"]["sha256"]}),
            &[],
        );
        assert_eq!(status, 0);
        assert!(!generation.is_empty());
        let step: Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(step["state"], "ready");
        assert_eq!(ack_call(muu_core_imported_dictionary_publish, ticket), 0);
        assert_eq!(ack_call(muu_core_imported_dictionary_cancel, ticket), 9);
        assert_eq!(
            ack_call(
                muu_core_imported_dictionary_finish,
                &json!({"ticket":ticket,"outcome":"new_pointer_durable","revision":2,"generation_sha256":step["ready"]["generation_sha256"]})
            ),
            0
        );
        assert_eq!(ack_call(muu_core_imported_dictionary_cancel, ticket), 0);
    }
    #[test]
    fn either_step_output_copy_failure_is_unpublished_and_releases_reservation() {
        empty();
        let source = muu_session::imported_dictionary_template();
        for point in ["buffer_copy", "imported_prepare_second_buffer"] {
            INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
            let (status, generation, plan) = call(
                muu_core_imported_dictionary_prepare,
                import_command(),
                &source,
            );
            assert_eq!(status, 6);
            assert!(generation.is_empty());
            assert!(plan.is_empty());
            let (status, _, plan) = call(
                muu_core_imported_dictionary_prepare,
                import_command(),
                &source,
            );
            assert_eq!(status, 0);
            let step: Value = serde_json::from_slice(&plan).unwrap();
            assert_eq!(
                ack_call(muu_core_imported_dictionary_cancel, &step["ticket"]),
                0
            );
        }
    }
    #[test]
    fn stale_revision_busy_and_terminal_status_cannot_be_confused_with_publication() {
        empty();
        let source = muu_session::imported_dictionary_template();
        let mut stale = import_command();
        stale["expected_revision"] = json!(0);
        assert_eq!(
            call(muu_core_imported_dictionary_prepare, stale, &source).0,
            8
        );
        let (_, _, plan) = call(
            muu_core_imported_dictionary_prepare,
            import_command(),
            &source,
        );
        let step: Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(
            call(
                muu_core_imported_dictionary_prepare,
                import_command(),
                &source
            )
            .0,
            9
        );
        with_registry(|registry| registry.ready().unwrap().core.invalidate());
        assert_eq!(
            ack_call(
                muu_core_imported_dictionary_publish,
                &json!({"bad":"ticket"})
            ),
            7
        );
        assert_eq!(
            ack_call(muu_core_imported_dictionary_cancel, &step["ticket"]),
            7
        );
    }
    #[test]
    fn output_aliasing_null_inputs_and_limits_are_rejected_before_dereference() {
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_imported_dictionary_prepare(
                    ptr::null(),
                    0,
                    ptr::null(),
                    0,
                    &mut out,
                    &mut out,
                )
            },
            2
        );
        assert_eq!(
            unsafe {
                muu_core_imported_dictionary_prepare(
                    ptr::null(),
                    1,
                    ptr::null(),
                    0,
                    &mut out,
                    ptr::null_mut(),
                )
            },
            2
        );
        assert_eq!(
            unsafe { muu_core_imported_dictionary_prepared_blob(ptr::null(), 1, &mut out) },
            2
        );
        assert!(out.data.is_null());
        assert_eq!(out.len, 0);
    }

    #[test]
    fn file_validation_line_facts_reach_the_host_without_host_revalidation() {
        empty();
        let source = String::from_utf8(muu_session::imported_dictionary_template())
            .unwrap()
            .replace("NOUN", "UNSUPPORTED");
        let (status, generation, plan) = call(
            muu_core_imported_dictionary_prepare,
            import_command(),
            source.as_bytes(),
        );
        assert_eq!(status, 4);
        assert!(generation.is_empty());
        let error: Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(error["code"], "unsupported_part_of_speech");
        assert!(
            error["params"]["line"]
                .as_u64()
                .is_some_and(|line| line > 0)
        );
        assert_eq!(
            call(
                muu_core_imported_dictionary_prepare,
                import_command(),
                &muu_session::imported_dictionary_template()
            )
            .0,
            0
        );
    }
    #[test]
    fn step_keeps_known_overflow_reason_and_empties_unwind_outputs() {
        initialize();
        let mut generation = MuuBuffer::EMPTY;
        let mut plan = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                step(
                    ptr::null(),
                    0,
                    ptr::null(),
                    0,
                    0,
                    &mut generation,
                    &mut plan,
                    |_, _, _| Err(step_error(EditError::RevisionOverflow)),
                )
            },
            MUU_RESULT_INTERNAL
        );
        assert!(generation.data.is_null() && generation.len == 0);
        let reason: Value = serde_json::from_slice(&bytes(plan)).unwrap();
        assert_eq!(reason["code"], "revision_overflow");
        let mut plan = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                step(
                    ptr::null(),
                    0,
                    ptr::null(),
                    0,
                    0,
                    &mut generation,
                    &mut plan,
                    |_, _, _| panic!("private preparation"),
                )
            },
            MUU_RESULT_INTERNAL
        );
        assert!(generation.data.is_null() && generation.len == 0);
        assert!(plan.data.is_null() && plan.len == 0);
        assert!(with_registry(|r| r.ready().is_ok()));
    }
}
