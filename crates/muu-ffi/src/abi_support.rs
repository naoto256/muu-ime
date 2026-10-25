//! Private dictionary replies distinguish a known rejection from a failure
//! whose response cannot be trusted. The numeric status alone is not enough
//! in ABI 13: revision exhaustion and an allocation panic are both INTERNAL.
use super::*;

// The failure-body contract of the dictionary prepare and step functions:
// - Known: the outcome is a rejection Rust understands, so a reason body
//   (`{"code", ...}`) is written to the reason output. A known
//   RevisionOverflow is INTERNAL but still Known, so the host can tell an
//   exhausted revision from a crash.
// - Empty: a panic, or a failure while allocating the outputs or the reason
//   itself. Nothing in the response can be trusted, so every output stays
//   empty and only the status is returned.
#[derive(Clone, Copy)]
pub(super) enum FailureBody {
    Known,
    Empty,
}

#[derive(Clone, Copy)]
pub(super) struct PrivateFailure {
    pub status: i32,
    pub code: &'static str,
    body: FailureBody,
}
impl PrivateFailure {
    pub fn known(status: i32, code: &'static str) -> Self {
        Self {
            status,
            code,
            body: FailureBody::Known,
        }
    }
    pub fn unwind((status, code): (i32, &'static str)) -> Self {
        Self {
            status,
            code,
            body: FailureBody::Empty,
        }
    }
}
// The default for a plain (status, code) pair: INTERNAL means Empty, since
// almost every INTERNAL is a panic or an output too large to build. A known
// INTERNAL with a reason has to be built with `known` on purpose.
impl From<(i32, &'static str)> for PrivateFailure {
    fn from((status, code): (i32, &'static str)) -> Self {
        Self {
            status,
            code,
            body: if status == MUU_RESULT_INTERNAL {
                FailureBody::Empty
            } else {
                FailureBody::Known
            },
        }
    }
}

// The caller has already zeroed every output and cancelled any reservation
// it acquired before output preparation failed. Nothing is transferred until
// the complete reason buffer has been allocated.
// If even the reason buffer cannot be allocated, CORE_INVALID is still
// returned as 7, so an invalid Core never looks retryable; any other status
// becomes INTERNAL with no body.
pub(super) unsafe fn write_private_failure(
    out: *mut MuuBuffer,
    failure: PrivateFailure,
    bytes: impl FnOnce() -> Vec<u8>,
) -> i32 {
    if matches!(failure.body, FailureBody::Empty) {
        return failure.status;
    }
    match catch_unwind(AssertUnwindSafe(|| prepare_buffer(bytes()))) {
        Ok(bytes) => {
            unsafe {
                out.write(buffer_from_box(bytes));
            }
            failure.status
        }
        Err(_) if failure.status == MUU_RESULT_CORE_INVALID => MUU_RESULT_CORE_INVALID,
        Err(_) => MUU_RESULT_INTERNAL,
    }
}

// Callers reject null/nonzero inputs before borrowing. No slice is formed
// for NULL/0, including a zero-length request from C.
pub(super) unsafe fn borrowed<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(data, len) }
    }
}

pub(super) fn bounded_decode<T: DeserializeOwned>(
    bytes: &[u8],
    limit: usize,
    too_large: &'static str,
    invalid: &'static str,
) -> Result<T, (i32, &'static str)> {
    if bytes.len() > limit {
        return Err((MUU_RESULT_DECODE_ERR, too_large));
    }
    serde_json::from_slice(bytes).map_err(|_| (MUU_RESULT_DECODE_ERR, invalid))
}

// The transport bound is checked before constructing even a borrowed slice.
// Call inside the ready/cancel operation so terminal-state precedence remains
// the same as the domain's ACK contract.
pub(super) unsafe fn decode_input<T: DeserializeOwned>(
    data: *const u8,
    len: usize,
    limit: usize,
    too_large: &'static str,
    invalid: &'static str,
) -> Result<T, (i32, &'static str)> {
    if len > limit {
        return Err((MUU_RESULT_DECODE_ERR, too_large));
    }
    bounded_decode(unsafe { borrowed(data, len) }, limit, too_large, invalid)
}

// The only place a session-level EditErrorClass becomes a MUU_RESULT_*
// number. Each domain error keeps its own code and decides whether its
// failure is Known (see each module's `edit_error`).
pub(super) fn edit_failure(
    class: muu_session::EditErrorClass,
    code: &'static str,
) -> PrivateFailure {
    use muu_session::EditErrorClass;
    let status = match class {
        EditErrorClass::Busy => MUU_RESULT_BUSY,
        EditErrorClass::RevisionMismatch => MUU_RESULT_REVISION_MISMATCH,
        EditErrorClass::CoreInvalid => MUU_RESULT_CORE_INVALID,
        EditErrorClass::Internal => MUU_RESULT_INTERNAL,
        EditErrorClass::Rejected => MUU_RESULT_DECODE_ERR,
    };
    (status, code).into()
}

// The shared ACK path: pointer check, one panic boundary, the registry, and
// an i32 result with no output. A panic is reported like any Core panic.
pub(super) unsafe fn acknowledge(
    data: *const u8,
    len: usize,
    operation: impl FnOnce(&CoreRegistry) -> Result<(), i32>,
) -> i32 {
    if data.is_null() && len > 0 {
        return MUU_RESULT_BAD_ARG;
    }
    match catch_unwind(AssertUnwindSafe(|| with_registry(operation))) {
        Ok(Ok(())) => MUU_RESULT_OK,
        Ok(Err(status)) => status,
        Err(_) => core_panic_result().0,
    }
}

pub(super) unsafe fn acknowledge_ready(
    data: *const u8,
    len: usize,
    operation: impl FnOnce(&Core) -> Result<(), i32>,
) -> i32 {
    unsafe {
        acknowledge(data, len, |registry| {
            let core = &registry.ready().map_err(|e| e.0)?.core;
            operation(core)
        })
    }
}

pub(super) unsafe fn clear_outputs<const N: usize>(outputs: [*mut MuuBuffer; N]) {
    for output in outputs {
        if !output.is_null() {
            unsafe {
                output.write(MuuBuffer::EMPTY);
            }
        }
    }
}
pub(super) fn distinct_outputs<const N: usize>(outputs: [*mut MuuBuffer; N]) -> bool {
    outputs
        .iter()
        .enumerate()
        .all(|(index, output)| !output.is_null() && !outputs[..index].contains(output))
}

// Own only the ticket created by a successful private step. A prior BUSY or
// malformed request must never cancel some other operation's reservation.
pub(super) struct PreparedOutputGuard<T, C: FnOnce(T)> {
    ticket: Option<T>,
    cancel: Option<C>,
}
impl<T, C: FnOnce(T)> PreparedOutputGuard<T, C> {
    pub fn new(ticket: T, cancel: C) -> Self {
        Self {
            ticket: Some(ticket),
            cancel: Some(cancel),
        }
    }
    fn disarm(&mut self) {
        self.ticket = None;
        self.cancel = None;
    }
}
impl<T, C: FnOnce(T)> Drop for PreparedOutputGuard<T, C> {
    fn drop(&mut self) {
        if let (Some(ticket), Some(cancel)) = (self.ticket.take(), self.cancel.take()) {
            cancel(ticket);
        }
    }
}

pub(super) struct StagedOutputs<T, C: FnOnce(T), const N: usize> {
    buffers: [Box<[u8]>; N],
    reservation: PreparedOutputGuard<T, C>,
}
// Keep the guard until the caller has left its unwind boundary. No fallible
// allocation, serialization or Core operation follows the ownership move.
impl<T, C: FnOnce(T), const N: usize> StagedOutputs<T, C, N> {
    pub unsafe fn transfer(self, outputs: [*mut MuuBuffer; N]) {
        let Self {
            buffers,
            mut reservation,
        } = self;
        reservation.disarm();
        for (output, bytes) in outputs.into_iter().zip(buffers) {
            unsafe {
                output.write(buffer_from_box(bytes));
            }
        }
    }
}
pub(super) fn stage_outputs<T, C: FnOnce(T), const N: usize>(
    bytes: [Vec<u8>; N],
    reservation: PreparedOutputGuard<T, C>,
    mut before_copy: impl FnMut(usize),
) -> StagedOutputs<T, C, N> {
    let mut index = 0;
    let buffers = bytes.map(|bytes| {
        before_copy(index);
        index += 1;
        prepare_buffer(bytes)
    });
    StagedOutputs {
        buffers,
        reservation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muu_session::{ImportedDictionaryEditError, PostalCodeEditError};

    // For two and three outputs, a failure while copying any one of them
    // cancels the reservation exactly once; a complete transfer cancels
    // nothing and hands over every buffer.
    #[test]
    fn staged_outputs_own_cancellation_until_the_complete_transfer() {
        use std::cell::Cell;
        fn check<const N: usize>() {
            for failed_copy in 0..N {
                let cancelled = Cell::new(0);
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let guard = PreparedOutputGuard::new(17, |ticket| {
                        assert_eq!(ticket, 17);
                        cancelled.set(cancelled.get() + 1);
                    });
                    stage_outputs(
                        std::array::from_fn::<_, N, _>(|_| vec![1]),
                        guard,
                        |index| {
                            if index == failed_copy {
                                panic!("private copy");
                            }
                        },
                    )
                }));
                assert!(result.is_err());
                assert_eq!(cancelled.get(), 1);
            }
            let cancelled = Cell::new(0);
            let staged = stage_outputs(
                std::array::from_fn::<_, N, _>(|_| vec![1]),
                PreparedOutputGuard::new(17, |_| cancelled.set(cancelled.get() + 1)),
                |_| {},
            );
            let mut outputs = std::array::from_fn::<_, N, _>(|_| MuuBuffer::EMPTY);
            let pointers = std::array::from_fn(|index| &mut outputs[index] as *mut MuuBuffer);
            unsafe {
                staged.transfer(pointers);
            }
            assert_eq!(cancelled.get(), 0);
            for output in outputs {
                assert_eq!(
                    unsafe { slice::from_raw_parts(output.data, output.len) },
                    &[1]
                );
                unsafe {
                    muu_buffer_free(output);
                }
            }
        }
        check::<2>();
        check::<3>();
    }

    // The body contract for all three dictionaries: a known overflow is
    // INTERNAL with its code in the body; a panic (INTERNAL or CORE_INVALID)
    // never builds a body; and a failure to allocate the reason leaves the
    // output empty, keeping CORE_INVALID as 7.
    #[test]
    fn known_revision_overflow_has_a_reason_but_unwind_and_copy_failure_do_not() {
        let failures = [
            (
                user_dictionary_abi::edit_error(UserDictionaryEditError::RevisionOverflow),
                "user_dictionary_revision_overflow",
            ),
            (
                imported_dictionary_abi::edit_error(ImportedDictionaryEditError::RevisionOverflow),
                "revision_overflow",
            ),
            (
                postal_code_abi::edit_error(PostalCodeEditError::RevisionOverflow),
                "revision_overflow",
            ),
        ];
        for (failure, code) in failures {
            assert_eq!(failure.status, MUU_RESULT_INTERNAL);
            assert_eq!(failure.code, code);
            let mut out = MuuBuffer::EMPTY;
            assert_eq!(
                unsafe { write_private_failure(&mut out, failure, || error_bytes(failure.code)) },
                MUU_RESULT_INTERNAL
            );
            let value: serde_json::Value =
                serde_json::from_slice(unsafe { slice::from_raw_parts(out.data, out.len) })
                    .unwrap();
            assert_eq!(value["code"], code);
            unsafe {
                muu_buffer_free(out);
            }

            for status in [MUU_RESULT_INTERNAL, MUU_RESULT_CORE_INVALID] {
                out = MuuBuffer::EMPTY;
                assert_eq!(
                    unsafe {
                        write_private_failure(
                            &mut out,
                            PrivateFailure::unwind((status, "internal_panic")),
                            || panic!("untrusted body must not be constructed"),
                        )
                    },
                    status
                );
                assert!(out.data.is_null());
                assert_eq!(out.len, 0);
            }
            out = MuuBuffer::EMPTY;
            INITIALIZATION_PANIC.with(|p| p.set(Some("buffer_copy")));
            assert_eq!(
                unsafe { write_private_failure(&mut out, failure, || error_bytes(code)) },
                MUU_RESULT_INTERNAL
            );
            assert!(out.data.is_null());
            assert_eq!(out.len, 0);
        }
        let mut out = MuuBuffer::EMPTY;
        INITIALIZATION_PANIC.with(|p| p.set(Some("buffer_copy")));
        assert_eq!(
            unsafe {
                write_private_failure(
                    &mut out,
                    PrivateFailure::known(MUU_RESULT_CORE_INVALID, "core_invalid"),
                    || error_bytes("core_invalid"),
                )
            },
            MUU_RESULT_CORE_INVALID
        );
        assert!(out.data.is_null());
        assert_eq!(out.len, 0);
    }
}
