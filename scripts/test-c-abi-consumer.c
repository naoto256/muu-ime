// Exercise the shipped header and Release staticlib from an independent C
// caller. No Swift/Rust test wrapper, files, model or IMK server is involved.
// The calls follow RustEngine.swift; each comment names the Swift behavior
// that the observation stands for. Output is one JSON line per observation,
// read by test-c-abi-consumer.py.
#include "muu_ffi.h"
#include <malloc/malloc.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// The checker owns the repetition count. A standalone compile must choose it
// explicitly too, rather than silently weakening the allocation observation.
#ifndef MUU_LEAK_ITERATIONS
#error "MUU_LEAK_ITERATIONS must be provided by the checker"
#endif
_Static_assert(MUU_LEAK_ITERATIONS > 0, "positive leak iteration count");

// Unlike assert, CHECK and every ABI call remain active with NDEBUG. The
// checker always compiles with -DNDEBUG, so a check that silently relied on
// assert would vanish there. Every ABI call is made outside the condition.
#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "CHECK failed at line %d: %s\n", __LINE__, #condition); \
        exit(1); \
    } \
} while (0)

// Swift imports MuuBuffer from this header and builds it as
// MuuBuffer(data:len:): a pointer followed by a length, two words in all.
_Static_assert(sizeof(struct MuuBuffer) == 2 * sizeof(size_t), "MuuBuffer size");
_Static_assert(offsetof(struct MuuBuffer, len) == sizeof(void *), "MuuBuffer layout");

static bool print_observations = true;

// Swift's copy() rejects a NULL or empty result, so every emitted buffer
// must be non-empty past any envelope header. buffer_len is the whole
// allocation, header included; the checker picks the free mutant's target
// from it. With observations off, the CHECK still runs.
static void emit(const char *name, struct MuuBuffer buffer, size_t prefix) {
    CHECK(buffer.data != NULL && buffer.len > prefix);
    if (!print_observations) return;
    printf("{\"name\":\"%s\",\"value\":", name);
    const size_t written = fwrite(buffer.data + prefix, 1, buffer.len - prefix, stdout);
    CHECK(written == buffer.len - prefix);
    const int ended = printf(",\"buffer_len\":%zu}\n", buffer.len);
    CHECK(ended >= 0);
}

// MuuEnvelope.encode: JSON tag 1 and little-endian schema version 1. The
// reply must carry the same three bytes, because MuuEnvelope.decodeView
// rejects any other tag or version. As in Swift, the view is freed only
// after it has been read.
static void send_request(MuuSession *session, const char *name, const char *json) {
    const size_t size = strlen(json) + 3;
    uint8_t *request = malloc(size);
    CHECK(request != NULL);
    request[0] = 1;
    request[1] = 1;
    request[2] = 0;
    memcpy(request + 3, json, size - 3);
    struct MuuBuffer view = {0};
    const int32_t result = muu_session_handle_event(session, request, size, &view);
    free(request);
    CHECK(result == MUU_RESULT_OK);
    CHECK(view.data != NULL && view.len > 3);
    CHECK(view.data[0] == 1 && view.data[1] == 1 && view.data[2] == 0);
    emit(name, view, 3);
    muu_buffer_free(view);
}

static void exercise(const char *config, const uint8_t *user_data, size_t user_len) {
    struct MuuBuffer initialized = {0}, status = {0};
    // NULL, 0, false is "no learning file", what Swift passes for nil.
    // This is RustCore.initialize's entry. Repeated calls copy the report but
    // never reapply startup data; they still return independently owned bytes.
    const int32_t initialized_result = muu_core_initialize_with_user_dictionary_file(
        (const uint8_t *)config, strlen(config), NULL, 0, false,
        user_data, user_len, &initialized);
    CHECK(initialized_result == MUU_RESULT_OK);
    const int32_t status_result = muu_core_runtime_status(&status);
    CHECK(status_result == MUU_RESULT_OK);
    // Status is what CoreRuntimeStatus decodes. Swift copies each result and
    // frees it in its own defer, so the two must be separate allocations that
    // stay readable after the next call.
    CHECK(initialized.data != status.data);
    emit("initialized", initialized, 0);
    emit("status", status, 0);
    muu_buffer_free(initialized);
    muu_buffer_free(status);

    // Observe that the supplied file reached management, rather than merely
    // accepting the extra C arguments without applying them. This is the
    // view UserDictionaryStore reads through RustCore.userDictionaryBridge.
    struct MuuBuffer dictionary = {0};
    const int32_t dictionary_result = muu_core_user_dictionary_view(&dictionary);
    CHECK(dictionary_result == MUU_RESULT_OK);
    emit("user_dictionary", dictionary, 0);
    muu_buffer_free(dictionary);

    // Success hands back a live session and leaves the error output empty.
    // The three requests type "kyou", convert it and commit it.
    MuuSession *session = NULL;
    struct MuuBuffer error = {0};
    const int32_t session_result = muu_session_create(&session, &error);
    CHECK(session_result == MUU_RESULT_OK && session != NULL);
    CHECK(error.data == NULL && error.len == 0);
    muu_buffer_free(error);
    send_request(session, "key", "{\"kind\":\"key\",\"key\":{\"event_type\":\"key_down\",\"keycode\":0,"
                 "\"characters_utf8\":\"kyou\",\"characters_ignoring_modifiers\":\"kyou\",\"modifiers\":0,\"is_repeat\":false}}");
    send_request(session, "convert", "{\"kind\":\"command\",\"command\":\"convert\"}");
    send_request(session, "commit", "{\"kind\":\"command\",\"command\":\"commit\"}");

    // Keep both snapshot outputs alive across the call and read them before
    // freeing in reverse order, as with Swift's two independent buffers.
    // RustCore.personalFrequency sends a nil version as JSON `null`.
    struct MuuBuffer learning = {0}, version = {0};
    const int32_t learning_result = muu_core_personal_frequency_snapshot(
        (const uint8_t *)"null", 4, &learning, &version);
    CHECK(learning_result == MUU_RESULT_OK);
    CHECK(learning.data != version.data);
    emit("learning", learning, 0);
    emit("learning_version", version, 0);
    muu_buffer_free(version);
    muu_buffer_free(learning);
    // RustEngine's deinit destroys its handle; the header makes NULL a no-op.
    muu_session_destroy(session);
    muu_session_destroy(NULL);
}

static size_t live_bytes(void) {
    malloc_statistics_t statistics = {0};
    // NULL sums all malloc zones. size_in_use excludes allocator free lists;
    // RSS would conflate freed buffers with memory retained by the allocator.
    malloc_zone_statistics(NULL, &statistics);
    return statistics.size_in_use;
}

int main(int argc, char **argv) {
    CHECK(argc == 2);
    const bool present = strcmp(argv[1], "minimal") == 0;
    CHECK(present || strcmp(argv[1], "missing") == 0);
    // AppDelegate sets up Core only when the ABI equals
    // MuuConstants.expectedABIVersion (13); otherwise keys pass through and
    // RustCore.initialize fails with abi_mismatch. AppDelegate logs the core
    // version with String(cString:), so it must be a non-NULL C string.
    const uint32_t abi_version = muu_abi_version();
    CHECK(abi_version == 13);
    const char *core_version = muu_core_version();
    CHECK(core_version != NULL && strlen(core_version) > 0);
    // Before initialization, status fails and leaves the buffer empty.
    // RustCore.runtimeStatus frees the output whatever the result, so that
    // free must be a no-op. The checker's broken-header negative fails here.
    struct MuuBuffer absent = {0};
    const int32_t before_initialization = muu_core_runtime_status(&absent);
    CHECK(before_initialization == MUU_RESULT_CREATE_FAILED);
    CHECK(absent.data == NULL && absent.len == 0);
    muu_buffer_free(absent);

    // Self-written one-entry TSV. CRC32 covers the first four columns.
    const char file[] = "# muu-user-dictionary\t1\nあび\t阿備\tNOUN\tnormal\t490844a9\n";
    const char *config = present ? "{\"user_dictionary_file_state\":\"present\"}"
                                 : "{\"user_dictionary_file_state\":\"missing\"}";
    const uint8_t *user_data = present ? (const uint8_t *)file : NULL;
    const size_t user_len = present ? strlen(file) : 0;
    exercise(config, user_data, user_len);

    // Warm lazy caches before measuring the identical eight-buffer sequence.
    // Two samples of MUU_LEAK_ITERATIONS (5,000 today) also expose leaking
    // only the smallest output (the 29-byte version), rather than only a
    // no-op of every buffer free. Each leaked copy raised size_in_use by
    // 32 B: at 5,000, +160,000 B after the first sample and +320,000 B after
    // the second, against 128 KiB of headroom.
    // At 1,000 per sample it stayed under (+32,000 / +64,000 B).
    print_observations = false;
    for (size_t i = 0; i < 32; ++i) exercise(config, user_data, user_len);
    const size_t baseline = live_bytes();
    for (size_t i = 0; i < MUU_LEAK_ITERATIONS; ++i) exercise(config, user_data, user_len);
    const size_t first = live_bytes();
    for (size_t i = 0; i < MUU_LEAK_ITERATIONS; ++i) exercise(config, user_data, user_len);
    const size_t second = live_bytes();
    printf("{\"name\":\"malloc_usage\",\"value\":{\"iterations_per_sample\":%zu,"
           "\"baseline\":%zu,\"first\":%zu,\"second\":%zu}}\n",
           (size_t)MUU_LEAK_ITERATIONS, baseline, first, second);
    return 0;
}
