#ifndef MUU_FFI_H
#define MUU_FFI_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#define MUU_RESULT_OK 0

#define MUU_RESULT_NULL_HANDLE 1

/**
 * Arguments the call cannot use: a null pointer with a nonzero length, a
 * missing or aliased output, and on some entries an oversized input (others
 * answer DECODE_ERR for that). Checked before Core state is consulted, so it
 * takes precedence over CREATE_FAILED and CORE_INVALID.
 */
#define MUU_RESULT_BAD_ARG 2

/**
 * Also what every Core entry returns before initialization has completed,
 * and for the life of the process after a failed initialization.
 */
#define MUU_RESULT_CREATE_FAILED 3

#define MUU_RESULT_DECODE_ERR 4

/**
 * Only this handle is unusable: destroy it and create a new one. From
 * `muu_session_create` it means no handle was made.
 */
#define MUU_RESULT_SESSION_INVALID 5

/**
 * This call alone failed; the Core remains usable.
 */
#define MUU_RESULT_INTERNAL 6

/**
 * Terminal for the process: every later Core and session call returns it.
 */
#define MUU_RESULT_CORE_INVALID 7

/**
 * The ticket's management revision is no longer current; nothing was
 * published.
 */
#define MUU_RESULT_REVISION_MISMATCH 8

/**
 * Core holds prepared, or published but unresolved, work for this data.
 */
#define MUU_RESULT_BUSY 9

/**
 * Opaque handle. All session calls are main-thread-affine; never share a handle.
 */
typedef struct MuuSession MuuSession;

/**
 * Bytes crossing the boundary. Inputs are borrowed for the call only. Outputs
 * are Rust-owned and released only by `muu_buffer_free`. Every entry resets
 * its outputs to `{NULL, 0}` before any failure other than a rejected output
 * pointer. A zero `len` is "no bytes" even if `data` is non-null; free it the
 * same way.
 */
typedef struct MuuBuffer {
  uint8_t *data;
  size_t len;
} MuuBuffer;

/**
 * Return ABI epoch 13. The opaque user-file initializer, dictionary
 * transactions and weighted record delivery are the supported host paths.
 * Dictionary prepare/load steps return INTERNAL with a reason for known
 * revision exhaustion (User: user_dictionary_revision_overflow;
 * Imported/Postal: revision_overflow). Panic and output-allocation failure
 * leave every step output empty. ACK functions allocate no response body.
 */
uint32_t muu_abi_version(void);

const char *muu_core_version(void);

/**
 * Read current process facts as raw JSON (schema_version 2, at most 16 KiB):
 * system_dictionary, language_model, commit_stream_enabled and captured
 * language_model_details (request, actual model, fallback and failure reasons).
 * Does not initialize resources, load a model, or drain any queue. Nonzero
 * results leave an empty buffer; INTERNAL is local and does not revoke Core.
 * Free the returned buffer regardless of the result.
 *
 * # Safety
 * A non-null out_status must be writable.
 */
int32_t muu_core_runtime_status(struct MuuBuffer *out_status);

/**
 * Create a session on the main thread after initialization has completed.
 *
 * # Safety
 * Both output pointers, when non-null, must be writable.
 */
int32_t muu_session_create(struct MuuSession **out_session, struct MuuBuffer *out_error);

/**
 * Destroy a session; NULL is a no-op. Must run on the main thread.
 *
 * # Safety
 * A non-null pointer must be live and have been allocated by `muu_session_create`.
 */
void muu_session_destroy(struct MuuSession *session);

/**
 * Handle one protocol envelope on the main thread.
 * If full view encoding fails after a commit, OK may carry a reduced view
 * preserving commit text, next preedit, caret, selection, mode and consumed.
 * Display metadata is omitted; the request, learning and records are not
 * replayed. Without a commit, encoding failure remains INTERNAL with no view.
 * Panic statuses and the envelope format are unchanged.
 *
 * # Safety
 * The session is live, the input readable, and `out` writable.
 */
int32_t muu_session_handle_event(struct MuuSession *session,
                                 const uint8_t *request,
                                 size_t len,
                                 struct MuuBuffer *out);

/**
 * Clear learning and return its new version as raw JSON. Call synchronously on
 * main after deleting the persisted file to exclude intervening session commits.
 *
 * # Safety
 * `out_version` must be writable.
 */
int32_t muu_core_clear_personal_frequency(struct MuuBuffer *out_version);

/**
 * Read learning without changing it. `received_json` is a raw version JSON or
 * `null`. No update returns two `{NULL, 0}` buffers. Neither output may be NULL.
 *
 * # Safety
 * Input bytes are readable, both distinct output pointers are writable.
 */
int32_t muu_core_personal_frequency_snapshot(const uint8_t *received_json,
                                             size_t len,
                                             struct MuuBuffer *out_data,
                                             struct MuuBuffer *out_version);

/**
 * Free a Rust-owned buffer. NULL is a no-op.
 *
 * # Safety
 * Non-null data and its length must be unchanged and not previously freed.
 */
void muu_buffer_free(struct MuuBuffer buffer);

/**
 * Take one opaque v5 event and its delivery weight (raw JSON, schema_version
 * 1 and positive UInt64 drop_weight, at most 1 KiB). Empty queue returns two
 * empty buffers. Nonzero returns both empty; INTERNAL has not consumed the
 * record. Free both buffers on every result. Do not mix old/new consumers.
 *
 * # Safety
 * Non-null outputs must be writable, distinct MuuBuffers.
 */
int32_t muu_core_take_commit_stream_event_with_weight(struct MuuBuffer *out_event,
                                                      struct MuuBuffer *out_weight);

/**
 * Accept loss debt for future records. Request is the same raw JSON weight
 * as take-with-weight (at most 1 KiB, positive UInt64). OK is an acceptance
 * ACK: do not resend. BAD_ARG/INTERNAL have not accepted any debt. CORE_INVALID
 * has an indeterminate publication outcome and must not be blindly retried.
 * No initialization, Session sequence, learning, or queued bytes are changed.
 *
 * # Safety
 * request_json must reference request_len readable bytes.
 */
int32_t muu_core_report_commit_stream_dropped(const uint8_t *request_json, size_t request_len);

/**
 * Initialize with an opaque user-dictionary file instead of a JSON snapshot.
 * Config requires `user_dictionary_file_state`: missing, present, unreadable.
 * A present empty file is distinct from absence. Startup data is applied once.
 *
 * # Safety
 * Inputs are borrowed readable arrays and `out_result` is writable. File
 * bytes are bounded at 8 MiB; callers report an unreadable larger file in config.
 */
int32_t muu_core_initialize_with_user_dictionary_file(const uint8_t *config_json,
                                                      size_t len,
                                                      const uint8_t *learning_data,
                                                      size_t learning_len,
                                                      bool learning_file_present,
                                                      const uint8_t *user_data,
                                                      size_t user_len,
                                                      struct MuuBuffer *out_result);

/**
 * Return the management view, including all decodable rows when the active
 * layer is empty. This operation never rewrites a file.
 *
 * # Safety
 * `out_view` is writable and freed with muu_buffer_free on every result.
 */
int32_t muu_core_user_dictionary_view(struct MuuBuffer *out_view);

/**
 * Prepare an edit without publishing. On success returns canonical file
 * bytes and a plan (ticket, next view and revision). Both outputs are ready
 * before either is transferred. INTERNAL never publishes. A known revision
 * overflow returns its reason in out_plan; panic or output allocation failure
 * leaves both outputs empty.
 *
 * # Safety
 * Input is a borrowed JSON object of at most 64 KiB. Outputs are distinct,
 * writable MuuBuffers; free both on success and failure.
 */
int32_t muu_core_user_dictionary_prepare(const uint8_t *data,
                                         size_t len,
                                         struct MuuBuffer *out_file,
                                         struct MuuBuffer *out_plan);

/**
 * Publish a prepared edit after the host's rename. OK is an irrevocable ACK;
 * INTERNAL and REVISION_MISMATCH mean unpublished. CORE_INVALID is ambiguous:
 * keep the renamed file and stop editing rather than guessing a rollback.
 * No output is allocated after publication.
 *
 * # Safety
 * Input is the borrowed ticket JSON (at most 1 KiB) returned by prepare.
 */
int32_t muu_core_user_dictionary_publish(const uint8_t *data, size_t len);

/**
 * Release private prepared work; repeated cancellation is harmless and
 * cannot undo publication. An invalid Core still releases the ticket.
 *
 * # Safety
 * Input is a borrowed ticket JSON of at most 1 KiB.
 */
int32_t muu_core_user_dictionary_cancel(const uint8_t *data, size_t len);

/**
 * Return Core's display-only imported dictionary management view. No file
 * is interpreted, written or collected by the host as a result of this call.
 *
 * # Safety
 * `out_view` is writable; free it with muu_buffer_free on every result.
 */
int32_t muu_core_imported_dictionary_view(struct MuuBuffer *out_view);

/**
 * Start a private load from current pointer bytes. The step requests only a
 * constrained generation/blob hash or the fixed legacy catalog name. Missing
 * and unreadable files are distinct; a present empty file is not absence.
 *
 * # Safety
 * Borrowed command is at most 64 KiB, pointer at most 128 bytes. Outputs are
 * distinct writable buffers; free both on every result. INTERNAL is unpublished.
 */
int32_t muu_core_imported_dictionary_load_begin(const uint8_t *command,
                                                size_t command_len,
                                                const uint8_t *data,
                                                size_t len,
                                                struct MuuBuffer *out_generation,
                                                struct MuuBuffer *out_plan);

/**
 * Supply a requested startup file, at most one blob (16 MiB) or catalog
 * (32 MiB). The step returns the next constrained request or a ready plan.
 * Merely loading never asks the host to save or collect files.
 *
 * # Safety
 * Inputs are borrowed. Outputs are distinct and writable; free both on every
 * result. A command names a pending request, not an arbitrary host path.
 */
int32_t muu_core_imported_dictionary_load_supply(const uint8_t *command,
                                                 size_t command_len,
                                                 const uint8_t *data,
                                                 size_t len,
                                                 struct MuuBuffer *out_generation,
                                                 struct MuuBuffer *out_plan);

/**
 * Prepare import, replacement, enable, disable or removal without publishing.
 * Steps request a necessary existing blob or stage one canonical blob. Once
 * all stages are acknowledged, returns generation bytes and a final plan.
 *
 * # Safety
 * Command is borrowed JSON <=64 KiB, source <=16 MiB. Outputs must be distinct
 * writable buffers, freed on every result. Known revision overflow is
 * INTERNAL with a reason in out_plan; panic or allocation failure leaves
 * both empty. All failures are unpublished.
 */
int32_t muu_core_imported_dictionary_prepare(const uint8_t *command,
                                             size_t command_len,
                                             const uint8_t *data,
                                             size_t len,
                                             struct MuuBuffer *out_generation,
                                             struct MuuBuffer *out_plan);

/**
 * Continue a private edit with a requested file or acknowledgment that the
 * single staged canonical blob has been saved. Acknowledgment frees its bytes
 * before Core prepares the next blob; whole catalogs are never transferred.
 *
 * # Safety
 * Borrowed command <=64 KiB and file <=32 MiB. Outputs are distinct writable
 * buffers; free both on every result. No publication occurs in this function.
 */
int32_t muu_core_imported_dictionary_prepare_supply(const uint8_t *command,
                                                    size_t command_len,
                                                    const uint8_t *data,
                                                    size_t len,
                                                    struct MuuBuffer *out_generation,
                                                    struct MuuBuffer *out_plan);

/**
 * Copy the one pending canonical blob identified by ticket and SHA. Never
 * fabricates a path; bytes are released by the subsequent written step.
 *
 * # Safety
 * Borrowed command <=64 KiB. `out_blob` is writable and always freed by caller.
 */
int32_t muu_core_imported_dictionary_prepared_blob(const uint8_t *data,
                                                   size_t len,
                                                   struct MuuBuffer *out_blob);

/**
 * Publish after immutable blobs and generation have been saved. No pointer
 * has changed yet. OK is a final publication ACK; INTERNAL / revision mismatch
 * guarantee this call did not publish. CORE_INVALID must never be guessed.
 *
 * # Safety
 * Borrowed ticket JSON <=1 KiB. No output allocation occurs after publication.
 */
int32_t muu_core_imported_dictionary_publish(const uint8_t *data, size_t len);

/**
 * Restore old Imported content after a pointer-save failure. Management
 * revision advances again (r+2); other layers and learning are preserved.
 * INTERNAL / mismatch are unpublished; CORE_INVALID forbids blind recovery.
 *
 * # Safety
 * Borrowed ticket JSON <=1 KiB. This function returns only an i32 ACK.
 */
int32_t muu_core_imported_dictionary_rollback(const uint8_t *data, size_t len);

/**
 * Resolve load, durable/warned new pointer, or rollback. Only a successful
 * durable-new-pointer finish grants the host GC authority for that revision's
 * reference set. Reservation remains through GC and main-thread completion.
 *
 * # Safety
 * Borrowed finish JSON <=64 KiB. No output is allocated.
 */
int32_t muu_core_imported_dictionary_finish(const uint8_t *data, size_t len);

/**
 * Cancel unpublished work or release a resolved reservation. Repeated cancel
 * is harmless; an unresolved publication returns BUSY instead of discarding
 * recovery authority. Invalid Core releases retained private work and returns 7.
 *
 * # Safety
 * Borrowed ticket JSON <=1 KiB.
 */
int32_t muu_core_imported_dictionary_cancel(const uint8_t *data, size_t len);

/**
 * Return canonical exchange-template bytes. The host only selects a
 * destination and writes them; format and content are defined by Rust.
 *
 * # Safety
 * `out_file` is writable and freed with muu_buffer_free on every result.
 */
int32_t muu_core_imported_dictionary_template(struct MuuBuffer *out_file);

/**
 * Return only Core's Postal display summary, never a stored-file definition.
 * # Safety
 * Writable output is always freed with muu_buffer_free, including on failure.
 */
int32_t muu_core_postal_code_view(struct MuuBuffer *out_view);

/**
 * Begin a private load from current (<=128 bytes). Missing and unreadable are
 * distinct. Requests name only a fixed seed file or a constrained generation.
 * # Safety
 * Inputs are borrowed; three outputs must be distinct writable buffers. Free
 * all three on every result. Known revision overflow is INTERNAL with a
 * reason in out_plan; panic or allocation failure leaves all three empty.
 * All failures are unpublished.
 */
int32_t muu_core_postal_code_load_begin(const uint8_t *command,
                                        size_t command_len,
                                        const uint8_t *data,
                                        size_t len,
                                        struct MuuBuffer *out_tsv,
                                        struct MuuBuffer *out_manifest,
                                        struct MuuBuffer *out_plan);

/**
 * Supply exactly the requested file, bounded by 16 MiB TSV / 64 KiB manifest.
 * Existing files are never rewritten merely by reading them.
 * # Safety
 * Borrowed command <=64 KiB. Three distinct writable outputs are freed on
 * every result, including empty buffers. INTERNAL cannot publish.
 */
int32_t muu_core_postal_code_load_supply(const uint8_t *command,
                                         size_t command_len,
                                         const uint8_t *data,
                                         size_t len,
                                         struct MuuBuffer *out_tsv,
                                         struct MuuBuffer *out_manifest,
                                         struct MuuBuffer *out_plan);

/**
 * Prepare a bounded ZIP update, canonical TSV/manifest and the next display
 * before publication. Clock seconds and raw Last-Modified are data, not paths.
 * # Safety
 * Borrowed ZIP <=4 MiB and command <=64 KiB. Three distinct writable outputs
 * must all be freed. Copy/allocation failure leaves all empty plus INTERNAL.
 */
int32_t muu_core_postal_code_prepare(const uint8_t *command,
                                     size_t command_len,
                                     const uint8_t *data,
                                     size_t len,
                                     struct MuuBuffer *out_tsv,
                                     struct MuuBuffer *out_manifest,
                                     struct MuuBuffer *out_plan);

/**
 * Publish the prepared Postal layer BEFORE saving generation/current. OK is
 * the final ACK; INTERNAL and revision mismatch guarantee no publication.
 * # Safety
 * Borrowed ticket <=1 KiB. No output allocation after the public swap.
 */
int32_t muu_core_postal_code_publish(const uint8_t *data, size_t len);

/**
 * Restore the prior Postal authority at r+2; preserve all other live layers.
 * # Safety
 * Borrowed ticket <=1 KiB. CORE_INVALID must never be treated as unpublished.
 */
int32_t muu_core_postal_code_rollback(const uint8_t *data, size_t len);

/**
 * Last-resort ACK publishes an empty Postal layer only (r+2). Other data,
 * learning and retained conversion snapshots are never disabled.
 * # Safety
 * Borrowed ticket <=1 KiB. No fallible response work follows publication.
 */
int32_t muu_core_postal_code_disable(const uint8_t *data, size_t len);

/**
 * Resolve loaded/durable/warning/rollback/disable against the live revision.
 * Only durable pointer plus this ACK grants GC; reservation lasts until cancel.
 * # Safety
 * Borrowed finish JSON <=64 KiB. Returns an output-free ACK.
 */
int32_t muu_core_postal_code_finish(const uint8_t *data, size_t len);

/**
 * Cancel private work or release a resolved reservation. Unresolved Published
 * returns BUSY; terminal Core releases work and returns CORE_INVALID.
 * # Safety
 * Borrowed ticket <=1 KiB; repeat cancel is harmless.
 */
int32_t muu_core_postal_code_cancel(const uint8_t *data, size_t len);

#endif  /* MUU_FFI_H */
