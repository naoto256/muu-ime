//! Postal's opaque named-file protocol. Only output-free ACKs mutate live data;
//! private output-copy failures cannot turn into ambiguous publications.
use super::*;
use muu_session::{
    MAX_POSTAL_COMMAND_BYTES, MAX_POSTAL_MANIFEST_BYTES, MAX_POSTAL_TICKET_BYTES,
    MAX_POSTAL_TSV_BYTES, MAX_POSTAL_VIEW_BYTES, MAX_POSTAL_ZIP_BYTES,
    PostalCodeEditError as EditError, PostalCodeFinishCommand, PostalCodeLoadCommand,
    PostalCodePrepareCommand, PostalCodeSupplyCommand, PostalCodeTicket, PreparedPostalCodeReply,
};
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
        "postal_code_command_too_large",
        "invalid_postal_code_command",
    )
}
/// Return only Core's Postal display summary, never a stored-file definition.
/// # Safety
/// Writable output is always freed with muu_buffer_free, including on failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_view(out_view: *mut MuuBuffer) -> i32 {
    unsafe {
        core_buffer_operation(out_view, |core| {
            let bytes = serde_json::to_vec(&core.postal_code_view())
                .map_err(|_| (MUU_RESULT_INTERNAL, "postal_code_view_failed"))?;
            if bytes.len() > MAX_POSTAL_VIEW_BYTES {
                return Err((MUU_RESULT_INTERNAL, "postal_code_output_too_large"));
            }
            Ok(bytes)
        })
    }
}
/// Begin a private load from current (<=128 bytes). Missing and unreadable are
/// distinct. Requests name only a fixed seed file or a constrained generation.
/// # Safety
/// Inputs are borrowed; three outputs must be distinct writable buffers. Free
/// all three on every result. Known revision overflow is INTERNAL with a
/// reason in out_plan; panic or allocation failure leaves all three empty.
/// All failures are unpublished.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_load_begin(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    out_tsv: *mut MuuBuffer,
    out_manifest: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        step(
            command,
            command_len,
            data,
            len,
            128,
            out_tsv,
            out_manifest,
            out_plan,
            |core, c, b| {
                core.begin_postal_code_load(
                    decode::<PostalCodeLoadCommand>(c, MAX_POSTAL_COMMAND_BYTES)?,
                    b,
                )
                .map_err(edit_error)
            },
        )
    }
}
/// Supply exactly the requested file, bounded by 16 MiB TSV / 64 KiB manifest.
/// Existing files are never rewritten merely by reading them.
/// # Safety
/// Borrowed command <=64 KiB. Three distinct writable outputs are freed on
/// every result, including empty buffers. INTERNAL cannot publish.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_load_supply(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    out_tsv: *mut MuuBuffer,
    out_manifest: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        step(
            command,
            command_len,
            data,
            len,
            MAX_POSTAL_TSV_BYTES,
            out_tsv,
            out_manifest,
            out_plan,
            |core, c, b| {
                core.supply_postal_code_load(
                    decode::<PostalCodeSupplyCommand>(c, MAX_POSTAL_COMMAND_BYTES)?,
                    b,
                )
                .map_err(edit_error)
            },
        )
    }
}
/// Prepare a bounded ZIP update, canonical TSV/manifest and the next display
/// before publication. Clock seconds and raw Last-Modified are data, not paths.
/// # Safety
/// Borrowed ZIP <=4 MiB and command <=64 KiB. Three distinct writable outputs
/// must all be freed. Copy/allocation failure leaves all empty plus INTERNAL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_prepare(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    out_tsv: *mut MuuBuffer,
    out_manifest: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
) -> i32 {
    unsafe {
        step(
            command,
            command_len,
            data,
            len,
            MAX_POSTAL_ZIP_BYTES,
            out_tsv,
            out_manifest,
            out_plan,
            |core, c, b| {
                core.prepare_postal_code(
                    decode::<PostalCodePrepareCommand>(c, MAX_POSTAL_COMMAND_BYTES)?,
                    b,
                )
                .map_err(edit_error)
            },
        )
    }
}
#[allow(clippy::too_many_arguments)] // One boundary implements all-or-empty ownership of 3 buffers.
unsafe fn step(
    command: *const u8,
    command_len: usize,
    data: *const u8,
    len: usize,
    file_limit: usize,
    out_tsv: *mut MuuBuffer,
    out_manifest: *mut MuuBuffer,
    out_plan: *mut MuuBuffer,
    operation: impl FnOnce(&Core, &[u8], &[u8]) -> Result<PreparedPostalCodeReply, PrivateFailure>,
) -> i32 {
    unsafe {
        clear_outputs([out_tsv, out_manifest, out_plan]);
    }
    if !distinct_outputs([out_tsv, out_manifest, out_plan])
        || (command.is_null() && command_len > 0)
        || (data.is_null() && len > 0)
        || len > file_limit
    {
        return MUU_RESULT_BAD_ARG;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| -> Result<_, PrivateFailure> {
            let core = &registry.ready()?.core;
            if command_len > MAX_POSTAL_COMMAND_BYTES {
                return Err((MUU_RESULT_DECODE_ERR, "postal_code_command_too_large").into());
            }
            let p = operation(core, unsafe { borrowed(command, command_len) }, unsafe {
                borrowed(data, len)
            })?;
            let cancel_core = core.clone();
            let reservation = PreparedOutputGuard::new(p.ticket, move |ticket| {
                let _ = cancel_core.cancel_postal_code(&ticket);
            });
            // The same bounds a later load accepts, so the host is never
            // handed a file that the next startup would refuse for its size.
            if p.tsv.len() > MAX_POSTAL_TSV_BYTES
                || p.manifest.len() > MAX_POSTAL_MANIFEST_BYTES
                || p.plan.len() > MAX_POSTAL_VIEW_BYTES
            {
                return Err((MUU_RESULT_INTERNAL, "postal_code_output_too_large").into());
            }
            Ok(stage_outputs(
                [p.tsv, p.manifest, p.plan],
                reservation,
                |index| {
                    #[cfg(test)]
                    match index {
                        1 => initialization_test_point("postal_prepare_second_buffer"),
                        2 => initialization_test_point("postal_prepare_third_buffer"),
                        _ => (),
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
                staged.transfer([out_tsv, out_manifest, out_plan]);
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
// Readiness precedes decoding, as in the other ACK entries: a bad ticket must
// not turn an ambiguous CORE_INVALID into a definite rejection.
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
                "postal_code_command_too_large",
                "invalid_postal_code_command",
            )
            .map_err(|e| e.0)?;
            operation(core, command).map_err(|e| edit_error(e).status)
        })
    }
}
/// Publish the prepared Postal layer BEFORE saving generation/current. OK is
/// the final ACK; INTERNAL and revision mismatch guarantee no publication.
/// # Safety
/// Borrowed ticket <=1 KiB. No output allocation after the public swap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_publish(data: *const u8, len: usize) -> i32 {
    unsafe {
        ack(data, len, MAX_POSTAL_TICKET_BYTES, |c, t| {
            c.publish_postal_code(&t)
        })
    }
}
/// Restore the prior Postal authority at r+2; preserve all other live layers.
/// # Safety
/// Borrowed ticket <=1 KiB. CORE_INVALID must never be treated as unpublished.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_rollback(data: *const u8, len: usize) -> i32 {
    unsafe {
        ack(data, len, MAX_POSTAL_TICKET_BYTES, |c, t| {
            c.rollback_postal_code(&t)
        })
    }
}
/// Last-resort ACK publishes an empty Postal layer only (r+2). Other data,
/// learning and retained conversion snapshots are never disabled.
/// # Safety
/// Borrowed ticket <=1 KiB. No fallible response work follows publication.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_disable(data: *const u8, len: usize) -> i32 {
    unsafe {
        ack(data, len, MAX_POSTAL_TICKET_BYTES, |c, t| {
            c.disable_postal_code(&t)
        })
    }
}
/// Resolve loaded/durable/warning/rollback/disable against the live revision.
/// Only durable pointer plus this ACK grants GC; reservation lasts until cancel.
/// # Safety
/// Borrowed finish JSON <=64 KiB. Returns an output-free ACK.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_finish(data: *const u8, len: usize) -> i32 {
    unsafe {
        ack(
            data,
            len,
            MAX_POSTAL_COMMAND_BYTES,
            |c, t: PostalCodeFinishCommand| c.finish_postal_code(t),
        )
    }
}
/// Cancel private work or release a resolved reservation. Unresolved Published
/// returns BUSY; terminal Core releases work and returns CORE_INVALID.
/// # Safety
/// Borrowed ticket <=1 KiB; repeat cancel is harmless.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_postal_code_cancel(data: *const u8, len: usize) -> i32 {
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
                initialized.core.discard_postal_code_preparation();
                return Err(MUU_RESULT_CORE_INVALID);
            }
            let ticket: PostalCodeTicket = decode_input(
                data,
                len,
                MAX_POSTAL_TICKET_BYTES,
                "postal_code_command_too_large",
                "invalid_postal_code_command",
            )
            .map_err(|e| e.0)?;
            initialized
                .core
                .cancel_postal_code(&ticket)
                .map_err(|e| edit_error(e).status)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    type Step = unsafe extern "C" fn(
        *const u8,
        usize,
        *const u8,
        usize,
        *mut MuuBuffer,
        *mut MuuBuffer,
        *mut MuuBuffer,
    ) -> i32;
    fn bytes(b: MuuBuffer) -> Vec<u8> {
        let result = if b.len == 0 {
            vec![]
        } else {
            unsafe { slice::from_raw_parts(b.data, b.len) }.to_vec()
        };
        unsafe { muu_buffer_free(b) };
        result
    }
    fn initialize() {
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
        bytes(out);
    }
    fn call(f: Step, command: Value, input: &[u8]) -> (i32, Vec<u8>, Vec<u8>, Vec<u8>) {
        let command = serde_json::to_vec(&command).unwrap();
        let (mut t, mut m, mut p) = (MuuBuffer::EMPTY, MuuBuffer::EMPTY, MuuBuffer::EMPTY);
        let status = unsafe {
            f(
                command.as_ptr(),
                command.len(),
                input.as_ptr(),
                input.len(),
                &mut t,
                &mut m,
                &mut p,
            )
        };
        (status, bytes(t), bytes(m), bytes(p))
    }
    fn ack(f: unsafe extern "C" fn(*const u8, usize) -> i32, v: &Value) -> i32 {
        let b = serde_json::to_vec(v).unwrap();
        unsafe { f(b.as_ptr(), b.len()) }
    }
    fn prepared() -> (Vec<u8>, Vec<u8>, Value) {
        let (status, t, m, p) = call(
            muu_core_postal_code_prepare,
            json!({"expected_revision":0,"source":"japan_post","retrieved_unix_seconds":1,"last_modified":null}),
            include_bytes!("../tests/data/postal-update.zip"),
        );
        assert_eq!(status, 0);
        assert!(t.starts_with(b"# muu-postal-code\t1\n"));
        assert!(m.contains(&b'{'));
        (t, m, serde_json::from_slice(&p).unwrap())
    }
    #[test]
    fn postal_three_buffers_publish_finish_and_reservation_are_owned_independently() {
        initialize();
        let (t, m, p) = prepared();
        assert!(!t.is_empty() && !m.is_empty());
        assert_eq!(ack(muu_core_postal_code_publish, &p["ticket"]), 0);
        assert_eq!(ack(muu_core_postal_code_cancel, &p["ticket"]), 9);
        assert_eq!(
            ack(
                muu_core_postal_code_finish,
                &json!({"ticket":p["ticket"],"outcome":"new_pointer_durable","revision":1,"generation":p["ready"]["generation"]})
            ),
            0
        );
        assert_eq!(ack(muu_core_postal_code_cancel, &p["ticket"]), 0);
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(unsafe { muu_core_postal_code_view(&mut out) }, 0);
        let v: Value = serde_json::from_slice(&bytes(out)).unwrap();
        assert_eq!(v["manifest"]["entry_count"], 1);
    }
    #[test]
    fn each_postal_output_copy_failure_is_all_empty_and_only_cancels_its_private_ticket() {
        for point in [
            "buffer_copy",
            "postal_prepare_second_buffer",
            "postal_prepare_third_buffer",
        ] {
            std::thread::spawn(move || {
                initialize();
                INITIALIZATION_PANIC.with(|p| p.set(Some(point)));
                let (s, t, m, p) = call(
                    muu_core_postal_code_prepare,
                    json!({"expected_revision":0,"source":"japan_post","retrieved_unix_seconds":1}),
                    include_bytes!("../tests/data/postal-update.zip"),
                );
                assert_eq!(s, 6);
                assert!(t.is_empty() && m.is_empty() && p.is_empty());
                assert!(!with_registry(|r| r.ready().unwrap().core.is_invalid()));
                let (_, _, p) = prepared();
                assert_eq!(ack(muu_core_postal_code_cancel, &p["ticket"]), 0);
            })
            .join()
            .unwrap();
        }
    }
    #[test]
    fn failed_second_preparation_does_not_cancel_an_existing_published_ticket() {
        initialize();
        let (_, _, p) = prepared();
        assert_eq!(ack(muu_core_postal_code_publish, &p["ticket"]), 0);
        let (s, t, m, _) = call(
            muu_core_postal_code_prepare,
            json!({"expected_revision":1,"source":"japan_post","retrieved_unix_seconds":2}),
            include_bytes!("../tests/data/postal-update.zip"),
        );
        assert_eq!(s, 9);
        assert!(t.is_empty() && m.is_empty());
        assert_eq!(ack(muu_core_postal_code_rollback, &p["ticket"]), 0);
    }
    #[test]
    fn postal_named_load_requests_and_bad_arguments_respect_all_three_outputs() {
        initialize();
        let (s, t, m, p) = call(
            muu_core_postal_code_load_begin,
            json!({"expected_revision":0,"current_state":"missing"}),
            &[],
        );
        assert_eq!(s, 0);
        assert!(t.is_empty() && m.is_empty());
        let p: Value = serde_json::from_slice(&p).unwrap();
        assert_eq!(p["request"]["kind"], "seed_manifest");
        let (s, _, _, out) = call(
            muu_core_postal_code_load_supply,
            json!({"ticket":p["ticket"],"request_id":1,"file_state":"missing"}),
            &[],
        );
        assert_eq!(s, 0);
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["ready"]["next_view"]["access"], "read_only");
        assert_eq!(ack(muu_core_postal_code_cancel, &p["ticket"]), 0);
        let mut out = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                muu_core_postal_code_load_begin(
                    ptr::null(),
                    1,
                    ptr::null(),
                    0,
                    &mut out,
                    &mut out,
                    &mut out,
                )
            },
            2
        );
        assert!(out.data.is_null() && out.len == 0);
        let (s, t, m, p) = call(
            muu_core_postal_code_prepare,
            json!({"expected_revision":0,"source":"not_an_allowed_path","retrieved_unix_seconds":1}),
            &[],
        );
        assert_eq!(s, 4);
        assert!(t.is_empty() && m.is_empty());
        let decode_error = serde_json::from_slice::<Value>(&p).ok();
        with_registry(|registry| registry.ready().unwrap().core.invalidate());
        let (s, t, m, p) = call(
            muu_core_postal_code_prepare,
            json!({"expected_revision":0,"source":"utf_all", "retrieved_unix_seconds":1}),
            &[],
        );
        assert_eq!(s, MUU_RESULT_CORE_INVALID);
        assert!(t.is_empty() && m.is_empty());
        let invalid_core_error = serde_json::from_slice::<Value>(&p).ok();
        assert_eq!(
            [decode_error, invalid_core_error],
            [
                Some(json!({"code":"invalid_postal_code_command","params":{}})),
                Some(json!({"code":"core_invalid","params":{}})),
            ]
        );
    }
    // For each exit (finish, rollback then finish, disable then finish), a new
    // update is BUSY from publish until cancel, then accepted. Each
    // action runs on its own thread because the test Core is thread-local,
    // so every round starts from a fresh, uninitialized Core.
    #[test]
    fn postal_updates_are_busy_until_published_authority_is_released() {
        for action in [0, 1, 2] {
            std::thread::spawn(move || {
                initialize();
                let (_, _, p) = prepared();
                assert_eq!(ack(muu_core_postal_code_publish, &p["ticket"]), 0);
                let update = |expected_revision| call(muu_core_postal_code_prepare,
                    json!({"expected_revision":expected_revision,"source":"japan_post",
                           "retrieved_unix_seconds":2,"last_modified":null}),
                    include_bytes!("../tests/data/postal-update.zip"));
                let (status, tsv, manifest, plan) = update(1);
                assert_eq!(status, MUU_RESULT_BUSY);
                assert!(tsv.is_empty() && manifest.is_empty());
                assert_eq!(serde_json::from_slice::<Value>(&plan).unwrap()["code"], "busy");
                let (outcome, revision, generation) = match action {
                    0 => ("new_pointer_durable", 1, p["ready"]["generation"].clone()),
                    1 => { assert_eq!(ack(muu_core_postal_code_rollback, &p["ticket"]), 0); ("rolled_back", 2, Value::Null) }
                    _ => { assert_eq!(ack(muu_core_postal_code_disable, &p["ticket"]), 0); ("disabled", 2, Value::Null) }
                };
                assert_eq!(ack(muu_core_postal_code_finish,
                    &json!({"ticket":p["ticket"],"outcome":outcome,"revision":revision,"generation":generation})), 0);
                let (status, tsv, manifest, plan) = update(revision);
                assert_eq!(status, MUU_RESULT_BUSY);
                assert!(tsv.is_empty() && manifest.is_empty());
                assert_eq!(serde_json::from_slice::<Value>(&plan).unwrap()["code"], "busy");
                assert_eq!(ack(muu_core_postal_code_cancel, &p["ticket"]), 0);
                let (status, _, _, plan) = update(revision);
                assert_eq!(status, MUU_RESULT_OK);
                let next: Value = serde_json::from_slice(&plan).unwrap();
                assert_eq!(ack(muu_core_postal_code_cancel, &next["ticket"]), 0);
            }).join().unwrap();
        }
    }
    #[test]
    fn step_keeps_known_overflow_reason_and_empties_unwind_outputs() {
        initialize();
        let mut tsv = MuuBuffer::EMPTY;
        let mut manifest = MuuBuffer::EMPTY;
        let mut plan = MuuBuffer::EMPTY;
        assert_eq!(
            unsafe {
                step(
                    ptr::null(),
                    0,
                    ptr::null(),
                    0,
                    0,
                    &mut tsv,
                    &mut manifest,
                    &mut plan,
                    |_, _, _| Err(edit_error(EditError::RevisionOverflow)),
                )
            },
            MUU_RESULT_INTERNAL
        );
        assert!(tsv.data.is_null() && tsv.len == 0);
        assert!(manifest.data.is_null() && manifest.len == 0);
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
                    &mut tsv,
                    &mut manifest,
                    &mut plan,
                    |_, _, _| panic!("private preparation"),
                )
            },
            MUU_RESULT_INTERNAL
        );
        assert!(tsv.data.is_null() && tsv.len == 0);
        assert!(manifest.data.is_null() && manifest.len == 0);
        assert!(plan.data.is_null() && plan.len == 0);
        assert!(with_registry(|r| r.ready().is_ok()));
    }
}
