//! Opaque delivery with an out-of-band loss weight. Only these new functions
//! promise INTERNAL before consume/accept; existing ABI failure units stay put.
use super::*;

const MAX_WEIGHT_BYTES: usize = 1024;
// The weight is the record itself plus the `dropped_records` it carries
// (docs/src/internals/storage.md「記録」). The host hands it back only when it finally discards the record,
// and the same JSON shape is accepted back by the report entry.
#[derive(serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryWeight {
    schema_version: u32,
    drop_weight: u64,
}

/// Take one opaque v5 event and its delivery weight (raw JSON, schema_version
/// 1 and positive UInt64 drop_weight, at most 1 KiB). Empty queue returns two
/// empty buffers. Nonzero returns both empty; INTERNAL has not consumed the
/// record. Free both buffers on every result. Do not mix old/new consumers.
///
/// # Safety
/// Non-null outputs must be writable, distinct MuuBuffers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_take_commit_stream_event_with_weight(
    out_event: *mut MuuBuffer,
    out_weight: *mut MuuBuffer,
) -> i32 {
    if !out_event.is_null() {
        unsafe { out_event.write(MuuBuffer::EMPTY) };
    }
    if !out_weight.is_null() {
        unsafe { out_weight.write(MuuBuffer::EMPTY) };
    }
    if out_event.is_null() || out_weight.is_null() || out_event == out_weight {
        return MUU_RESULT_BAD_ARG;
    }
    // Both outputs are built while the record is still at the queue front;
    // Core pops it only after this closure returns Ok, inside its
    // publication guard. A failed or panicking copy leaves it queued.
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| {
            registry
                .ready()?
                .core
                .take_commit_stream_event_prepared(|bytes, drop_weight| {
                    let event = prepare_buffer(bytes.to_vec());
                    #[cfg(test)]
                    initialization_test_point("delivery_weight");
                    let metadata = serde_json::to_vec(&DeliveryWeight {
                        schema_version: 1,
                        drop_weight,
                    })
                    .map_err(|_| (MUU_RESULT_INTERNAL, "delivery_weight_failed"))?;
                    if metadata.len() > MAX_WEIGHT_BYTES {
                        return Err((MUU_RESULT_INTERNAL, "delivery_weight_too_large"));
                    }
                    Ok((event, prepare_buffer(metadata)))
                })
        })
    }));
    match result {
        Ok(Ok(Some((event, weight)))) => {
            // Both allocations are ready and Core has consumed exactly once.
            // Ownership transfer cannot fail and does not prepare more output.
            unsafe {
                out_event.write(buffer_from_box(event));
                out_weight.write(buffer_from_box(weight));
            }
            MUU_RESULT_OK
        }
        Ok(Ok(None)) => MUU_RESULT_OK,
        Ok(Err((status, _))) => status,
        Err(_) => core_panic_result().0,
    }
}

/// Accept loss debt for future records. Request is the same raw JSON weight
/// as take-with-weight (at most 1 KiB, positive UInt64). OK is an acceptance
/// ACK: do not resend. BAD_ARG/INTERNAL have not accepted any debt. CORE_INVALID
/// has an indeterminate publication outcome and must not be blindly retried.
/// No initialization, Session sequence, learning, or queued bytes are changed.
///
/// # Safety
/// request_json must reference request_len readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn muu_core_report_commit_stream_dropped(
    request_json: *const u8,
    request_len: usize,
) -> i32 {
    if request_json.is_null() || request_len == 0 || request_len > MAX_WEIGHT_BYTES {
        return MUU_RESULT_BAD_ARG;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let request: DeliveryWeight =
            serde_json::from_slice(unsafe { slice::from_raw_parts(request_json, request_len) })
                .map_err(|_| (MUU_RESULT_BAD_ARG, "invalid_delivery_weight"))?;
        if request.schema_version != 1 || request.drop_weight == 0 {
            return Err((MUU_RESULT_BAD_ARG, "invalid_delivery_weight"));
        }
        // Decoding comes before ready(), so a malformed request is BAD_ARG
        // even on an uninitialized Core. Either way nothing was accepted.
        #[cfg(test)]
        initialization_test_point("delivery_report");
        // The debt saturates and is written into the next record that is
        // queued; records already queued or taken are never rewritten.
        with_registry(|registry| {
            registry
                .ready()?
                .core
                .report_commit_stream_dropped(request.drop_weight);
            // No allocation, logging, or reply generation after publication.
            Ok(())
        })
    }));
    match result {
        Ok(Ok(())) => MUU_RESULT_OK,
        Ok(Err((status, _))) => status,
        Err(_) => core_panic_result().0,
    }
}
