//! The user dictionary byte protocol and disk/publication handshake (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
//! Only these new prepare/publish operations promise that INTERNAL means
//! unpublished. Existing Core operations keep their established semantics.

use super::*;

/// Initialize with an opaque user-dictionary file instead of a JSON snapshot.
/// Config requires `user_dictionary_file_state`: missing, present, unreadable.
/// A present empty file is distinct from absence. Startup data is applied once.
///
/// # Safety
/// Inputs are borrowed readable arrays and `out_result` is writable. File
/// bytes are bounded at 8 MiB; callers report an unreadable larger file in config.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_initialize_with_user_dictionary_file(
    config_json: *const u8,
    len: usize,
    learning_data: *const u8,
    learning_len: usize,
    learning_file_present: bool,
    user_data: *const u8,
    user_len: usize,
    out_result: *mut MuuBuffer,
) -> i32 {
    if out_result.is_null() {
        return MUU_RESULT_BAD_ARG;
    }
    unsafe { out_result.write(MuuBuffer::EMPTY) };
    // An oversized file is refused here, before the registry, so unlike an
    // oversized config it is not retained and a corrected call may follow.
    if (config_json.is_null() && len > 0)
        || (learning_data.is_null() && learning_len > 0)
        || (user_data.is_null() && user_len > 0)
        || user_len > muu_session::MAX_USER_DICTIONARY_SNAPSHOT_UTF8_BYTES
    {
        return MUU_RESULT_BAD_ARG;
    }
    let mut initialization_status = MUU_RESULT_OK;
    let result = catch_unwind(AssertUnwindSafe(|| {
        let config = if len > MAX_PROTOCOL_ENVELOPE_BYTES {
            Err("config_too_large")
        } else {
            Ok(unsafe { borrowed(config_json, len) })
        };
        let learning = if learning_len > MAX_LEARNING_BYTES {
            &[0u8][..]
        } else {
            unsafe { borrowed(learning_data, learning_len) }
        };
        let file = unsafe { borrowed(user_data, user_len) };
        with_registry(|registry| registry.initialize(config, learning, learning_file_present, file))
            .map(|(status, bytes)| {
                initialization_status = status;
                bytes
            })
    }));
    let status = finish_buffer_result(result, out_result);
    if status == MUU_RESULT_OK {
        initialization_status
    } else {
        status
    }
}

/// Return the management view, including all decodable rows when the active
/// layer is empty. This operation never rewrites a file.
///
/// # Safety
/// `out_view` is writable and freed with muu_buffer_free on every result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_user_dictionary_view(out_view: *mut MuuBuffer) -> i32 {
    unsafe {
        core_buffer_operation(out_view, |core| {
            let bytes = serde_json::to_vec(&core.user_dictionary_view())
                .map_err(|_| (MUU_RESULT_INTERNAL, "user_dictionary_view_failed"))?;
            if bytes.len() > MAX_USER_DICTIONARY_VIEW_BYTES {
                return Err((MUU_RESULT_INTERNAL, "user_dictionary_output_too_large"));
            }
            Ok(bytes)
        })
    }
}

// An oversized reply is INTERNAL: the edit was valid and nothing was
// published. Every other rejection of the edit itself is DECODE_ERR.
// RevisionOverflow is the one INTERNAL failure with a body, keeping User's
// existing `user_dictionary_revision_overflow` code (its status moved from 4
// to INTERNAL in ABI 13).
pub(super) fn edit_error(error: UserDictionaryEditError) -> PrivateFailure {
    let failure = edit_failure(error.class(), error.code());
    if matches!(error, UserDictionaryEditError::RevisionOverflow) {
        PrivateFailure::known(failure.status, failure.code)
    } else {
        failure
    }
}

/// Prepare an edit without publishing. On success returns canonical file
/// bytes and a plan (ticket, next view and revision). Both outputs are ready
/// before either is transferred. INTERNAL never publishes. A known revision
/// overflow returns its reason in out_plan; panic or output allocation failure
/// leaves both outputs empty.
///
/// # Safety
/// Input is a borrowed JSON object of at most 64 KiB. Outputs are distinct,
/// writable MuuBuffers; free both on success and failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_user_dictionary_prepare(
    data: *const u8,
    len: usize,
    out_file: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        clear_outputs([out_file, out_plan]);
    }
    if !distinct_outputs([out_file, out_plan]) || (data.is_null() && len > 0) {
        return MUU_RESULT_BAD_ARG;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| -> Result<_, PrivateFailure> {
            let core = &registry.ready()?.core;
            let command: UserDictionaryEditCommand = unsafe {
                decode_input(
                    data,
                    len,
                    MAX_USER_DICTIONARY_EDIT_BYTES,
                    "user_dictionary_edit_too_large",
                    "user_dictionary_invalid_edit",
                )
            }?;
            let prepared = core.prepare_user_dictionary(command).map_err(edit_error)?;
            let cancel_core = core.clone();
            let reservation = PreparedOutputGuard::new(prepared.ticket, move |ticket| {
                cancel_core.cancel_user_dictionary(&ticket)
            });
            Ok(stage_outputs(
                [prepared.file, prepared.plan],
                reservation,
                |index| {
                    #[cfg(test)]
                    if index == 1 {
                        initialization_test_point("user_prepare_second_buffer");
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
                staged.transfer([out_file, out_plan]);
            }
            MUU_RESULT_OK
        }
        other => {
            let failure = match other {
                Ok(Err(error)) => error,
                Err(_) => PrivateFailure::unwind(core_panic_result()),
                _ => unreachable!(),
            };
            unsafe { write_private_failure(out_plan, failure, || error_bytes(failure.code)) }
        }
    }
}

unsafe fn ticket(data: *const u8, len: usize) -> Result<UserDictionaryTicket, i32> {
    unsafe {
        decode_input(
            data,
            len,
            MAX_USER_DICTIONARY_TICKET_BYTES,
            "user_dictionary_ticket_too_large",
            "user_dictionary_invalid_ticket",
        )
    }
    .map_err(|e| e.0)
}

/// Publish a prepared edit after the host's rename. OK is an irrevocable ACK;
/// INTERNAL and REVISION_MISMATCH mean unpublished. CORE_INVALID is ambiguous:
/// keep the renamed file and stop editing rather than guessing a rollback.
/// No output is allocated after publication.
///
/// # Safety
/// Input is the borrowed ticket JSON (at most 1 KiB) returned by prepare.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_user_dictionary_publish(data: *const u8, len: usize) -> i32 {
    unsafe {
        acknowledge_ready(data, len, |core| {
            // Terminal Core state precedes semantic ticket errors.
            let ticket = ticket(data, len)?;
            #[cfg(test)]
            initialization_test_point("user_publish_before_lock");
            core.publish_user_dictionary(&ticket)
                .map_err(|e| edit_error(e).status)
        })
    }
}

/// Release private prepared work; repeated cancellation is harmless and
/// cannot undo publication. An invalid Core still releases the ticket.
///
/// # Safety
/// Input is a borrowed ticket JSON of at most 1 KiB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_user_dictionary_cancel(data: *const u8, len: usize) -> i32 {
    unsafe {
        acknowledge(data, len, |registry| {
            let initialized = registry.initialized.get().ok_or(MUU_RESULT_CREATE_FAILED)?;
            let initialized = initialized
                .result
                .as_ref()
                .map_err(|_| MUU_RESULT_CREATE_FAILED)?;
            if initialized.core.is_invalid() {
                initialized.core.discard_user_dictionary_preparation();
                return Err(MUU_RESULT_CORE_INVALID);
            }
            let ticket = ticket(data, len)?;
            initialized.core.cancel_user_dictionary(&ticket);
            if initialized.core.is_invalid() {
                Err(MUU_RESULT_CORE_INVALID)
            } else {
                Ok(())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn take(buffer: MuuBuffer) -> Value {
        assert!(!buffer.data.is_null());
        let value =
            serde_json::from_slice(unsafe { slice::from_raw_parts(buffer.data, buffer.len) })
                .unwrap();
        unsafe {
            muu_buffer_free(buffer);
        }
        value
    }
    fn initialize(config: Value, file: &[u8]) -> (i32, Value) {
        let bytes = serde_json::to_vec(&config).unwrap();
        let mut out = MuuBuffer::EMPTY;
        let status = unsafe {
            muu_core_initialize_with_user_dictionary_file(
                bytes.as_ptr(),
                bytes.len(),
                ptr::null(),
                0,
                false,
                file.as_ptr(),
                file.len(),
                &mut out,
            )
        };
        (status, take(out))
    }
    fn view() -> Value {
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(unsafe { muu_core_user_dictionary_view(&mut out) }, 0);
        take(out)
    }
    fn command(revision: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({"operation":"upsert", "expected_revision":revision,
        "reading":"ムウ", "surface":"Muu", "part_of_speech":"NOUN", "priority":"high"}))
        .unwrap()
    }
    fn prepare(command: &[u8]) -> (i32, MuuBuffer, MuuBuffer) {
        let (mut file, mut plan) = (MuuBuffer::EMPTY, MuuBuffer::EMPTY);
        let status = unsafe {
            muu_core_user_dictionary_prepare(command.as_ptr(), command.len(), &mut file, &mut plan)
        };
        (status, file, plan)
    }
    #[test]
    fn byte_startup_distinguishes_missing_empty_and_unreadable_without_rewriting() {
        for (state, bytes, editable) in [
            ("missing", &b""[..], true),
            ("present", &b""[..], false),
            ("unreadable", &b""[..], false),
            ("present", &b"# muu-user-dictionary\t1\n"[..], true),
        ] {
            std::thread::spawn(move || {
                assert_eq!(
                    initialize(json!({"user_dictionary_file_state":state}), bytes).0,
                    0
                );
                let result = view();
                assert_eq!(
                    result["access"],
                    if editable { "editable" } else { "read_only" }
                );
                assert_eq!(result["management_revision"], 0);
                assert_eq!(result["entries"].is_array(), editable);
            })
            .join()
            .unwrap();
        }
    }
    #[test]
    fn byte_initialization_rejects_snapshot_fields_and_retains_failure() {
        let config = json!({"user_dictionary_file_state":"missing","user_dictionary":{"schema_version":1,"entries":[]}});
        let first = initialize(config, &[]);
        assert_eq!(first.0, 3);
        assert_eq!(first.1["code"], "invalid_config");
        assert_eq!(
            initialize(json!({"user_dictionary_file_state":"missing"}), &[]),
            first
        );
    }
    #[test]
    fn prepare_transfers_two_outputs_and_publish_returns_only_ack() {
        initialize(json!({"user_dictionary_file_state":"missing"}), &[]);
        let (status, file, plan) = prepare(&command(0));
        assert_eq!(status, 0);
        let saved = unsafe { slice::from_raw_parts(file.data, file.len) }.to_vec();
        unsafe {
            muu_buffer_free(file);
        }
        let plan = take(plan);
        assert_eq!(plan["view"]["management_revision"], 1);
        assert_eq!(view()["entry_count"], 0);
        let (busy, file, error) = prepare(&command(0));
        assert_eq!(busy, 9);
        assert!(file.data.is_null());
        assert_eq!(take(error)["code"], "user_dictionary_busy");
        let ticket = serde_json::to_vec(&plan["ticket"]).unwrap();
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ticket.as_ptr(), ticket.len()) },
            0
        );
        assert_eq!(view(), plan["view"]);
        assert_eq!(
            muu_session::decode_user_dictionary_file(&saved)
                .unwrap()
                .entries[0]
                .reading,
            "むう"
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ticket.as_ptr(), ticket.len()) },
            4
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_cancel(ticket.as_ptr(), ticket.len()) },
            0
        );
    }
    #[test]
    fn either_output_copy_panic_cancels_reservation_and_zeros_both_outputs() {
        initialize(json!({"user_dictionary_file_state":"missing"}), &[]);
        for point in ["buffer_copy", "user_prepare_second_buffer"] {
            INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
            let (status, file, plan) = prepare(&command(0));
            assert_eq!(status, 6);
            assert!(file.data.is_null());
            assert_eq!(file.len, 0);
            assert!(plan.data.is_null());
            assert_eq!(plan.len, 0);
            assert_eq!(view()["management_revision"], 0);
            let (status, file, plan) = prepare(&command(0));
            assert_eq!(status, 0);
            unsafe {
                muu_buffer_free(file);
            }
            let plan = take(plan);
            let ticket = serde_json::to_vec(&plan["ticket"]).unwrap();
            assert_eq!(
                unsafe { muu_core_user_dictionary_cancel(ticket.as_ptr(), ticket.len()) },
                0
            );
        }
    }
    #[test]
    fn stale_management_revision_is_eight_and_never_publishes() {
        initialize(json!({"user_dictionary_file_state":"missing"}), &[]);
        let (status, file, plan) = prepare(&command(0));
        assert_eq!(status, MUU_RESULT_OK);
        unsafe {
            muu_buffer_free(file);
        }
        let ticket = serde_json::to_vec(&take(plan)["ticket"]).unwrap();
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ticket.as_ptr(), ticket.len()) },
            MUU_RESULT_OK
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_cancel(ticket.as_ptr(), ticket.len()) },
            MUU_RESULT_OK
        );
        let accepted = view();
        let (status, file, plan) = prepare(&command(0));
        assert_eq!(status, MUU_RESULT_REVISION_MISMATCH);
        assert!(file.data.is_null());
        assert_eq!(file.len, 0);
        assert_eq!(take(plan)["code"], "user_dictionary_revision_mismatch");
        assert_eq!(view(), accepted);
    }
    #[test]
    fn invalid_buffers_limits_and_core_invalidation_do_not_leak_a_ticket() {
        initialize(json!({"user_dictionary_file_state":"missing"}), &[]);
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe { muu_core_user_dictionary_prepare(ptr::null(), 1, &mut out, &mut out) },
            2
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_prepare(ptr::null(), 0, ptr::null_mut(), &mut out) },
            2
        );
        let data = vec![b' '; MAX_USER_DICTIONARY_EDIT_BYTES + 1];
        let (status, file, plan) = prepare(&data);
        assert_eq!(status, 4);
        assert!(file.data.is_null());
        unsafe {
            muu_buffer_free(plan);
        }
        let (status, file, plan) = prepare(&command(0));
        assert_eq!(status, 0);
        unsafe {
            muu_buffer_free(file);
        }
        let ticket = serde_json::to_vec(&take(plan)["ticket"]).unwrap();
        with_registry(|r| r.ready().unwrap().core.invalidate());
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ticket.as_ptr(), ticket.len()) },
            7
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_cancel(ticket.as_ptr(), ticket.len()) },
            7
        );
    }
    #[test]
    fn byte_startup_is_applied_once_and_retains_one_registry() {
        let initial = initialize(
            json!({"user_dictionary_file_state":"present"}),
            b"# muu-user-dictionary\t1\n",
        );
        assert_eq!(initial.0, 0);
        let (status, file, plan) = prepare(&command(0));
        assert_eq!(status, 0);
        unsafe {
            muu_buffer_free(file);
        }
        let ticket = serde_json::to_vec(&take(plan)["ticket"]).unwrap();
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ticket.as_ptr(), ticket.len()) },
            0
        );
        assert_eq!(view()["entry_count"], 1);
        assert_eq!(
            initialize(
                json!({"user_dictionary_file_state":"unreadable"}),
                b"not a file"
            ),
            initial
        );
        let config = br#"{"user_dictionary_file_state":"missing"}"#;
        let mut out = MuuBuffer::EMPTY;
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
                    &mut out,
                )
            },
            0
        );
        assert_eq!(take(out), initial.1);
        assert_eq!(view()["entry_count"], 1);
        let mismatch = initialize(
            json!({"user_dictionary_file_state":"missing","dictionary_path":"different"}),
            &[],
        );
        assert_eq!(mismatch.0, 3);
        assert_eq!(mismatch.1["code"], "core_configuration_mismatch");
    }
    #[test]
    fn private_publish_panic_is_six_unpublished_and_cancellable() {
        initialize(json!({"user_dictionary_file_state":"missing"}), &[]);
        let (status, file, plan) = prepare(&command(0));
        assert_eq!(status, 0);
        unsafe {
            muu_buffer_free(file);
        }
        let ticket = serde_json::to_vec(&take(plan)["ticket"]).unwrap();
        INITIALIZATION_PANIC.with(|p| p.set(Some("user_publish_before_lock")));
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ticket.as_ptr(), ticket.len()) },
            6
        );
        assert_eq!(view()["management_revision"], 0);
        assert_eq!(view()["entry_count"], 0);
        assert_eq!(
            unsafe { muu_core_user_dictionary_cancel(ticket.as_ptr(), ticket.len()) },
            0
        );
    }
    #[test]
    fn terminal_status_precedes_ticket_validation_but_not_bad_pointers() {
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ptr::null(), 0) },
            3
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_cancel(ptr::null(), 0) },
            3
        );
        initialize(json!({"user_dictionary_file_state":"missing"}), &[]);
        with_registry(|r| r.ready().unwrap().core.invalidate());
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ptr::null(), 0) },
            7
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_cancel(ptr::null(), 0) },
            7
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_publish(ptr::null(), 1) },
            2
        );
        assert_eq!(
            unsafe { muu_core_user_dictionary_cancel(ptr::null(), 1) },
            2
        );
    }
}
