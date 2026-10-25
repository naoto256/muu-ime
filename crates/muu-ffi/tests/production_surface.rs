use muu_ffi::{
    MUU_RESULT_OK, MuuBuffer, MuuSession, muu_buffer_free,
    muu_core_initialize_with_user_dictionary_file, muu_session_create, muu_session_destroy,
    muu_session_handle_event,
};
use std::{ptr, slice};

fn command(name: &str) -> Vec<u8> {
    let mut bytes = vec![muu_session::FORMAT_TAG_JSON];
    bytes.extend_from_slice(&muu_session::SCHEMA_VERSION.to_le_bytes());
    bytes.extend_from_slice(
        serde_json::to_string(&serde_json::json!({
            "kind": "command",
            "command": name,
            "arg": null
        }))
        .expect("command fixture must serialize")
        .as_bytes(),
    );
    bytes
}

fn empty_buffer() -> MuuBuffer {
    MuuBuffer {
        data: ptr::null_mut(),
        len: 0,
    }
}

unsafe fn view_json(buffer: MuuBuffer) -> serde_json::Value {
    // SAFETY: successful FFI calls return a live Rust-owned buffer until freed.
    let bytes = unsafe { slice::from_raw_parts(buffer.data, buffer.len) };
    let value = serde_json::from_slice(&bytes[3..]).expect("view must be valid JSON");
    // SAFETY: ownership of the unchanged buffer is returned exactly once.
    unsafe { muu_buffer_free(buffer) };
    value
}

// panic_test exists only in cfg(test) builds of muu-ffi. Against the normal
// library, used by this integration test, it is an unknown command: the view
// is not poisoned and the Session keeps working.
#[test]
fn production_surface_does_not_expose_panic_injection() {
    let mut engine: *mut MuuSession = ptr::null_mut();
    let mut error = empty_buffer();
    // SAFETY: configuration bytes are readable; absent files use NULL/0.
    assert_eq!(
        unsafe {
            muu_core_initialize_with_user_dictionary_file(
                br#"{"user_dictionary_file_state":"missing"}"#.as_ptr(),
                br#"{"user_dictionary_file_state":"missing"}"#.len(),
                ptr::null(),
                0,
                false,
                ptr::null(),
                0,
                &mut error,
            )
        },
        MUU_RESULT_OK
    );
    unsafe { muu_buffer_free(error) };
    error = empty_buffer();
    let create_code = unsafe { muu_session_create(&mut engine, &mut error) };
    assert_eq!(create_code, MUU_RESULT_OK);
    assert!(!engine.is_null());
    assert!(error.data.is_null());

    let request = command("panic_test");
    let mut out = empty_buffer();
    // SAFETY: engine is live, request bytes are readable, and out is writable.
    let code = unsafe {
        muu_session_handle_event(engine, request.as_ptr(), request.len(), &mut out as *mut _)
    };
    assert_eq!(code, MUU_RESULT_OK);
    // SAFETY: the successful call initialized one owned view buffer.
    let view = unsafe { view_json(out) };
    assert_ne!(view["poisoned"], true);

    let follow_up = command("noop");
    let mut follow_up_out = empty_buffer();
    // SAFETY: engine remains live after the production-shaped command path.
    let follow_up_code = unsafe {
        muu_session_handle_event(
            engine,
            follow_up.as_ptr(),
            follow_up.len(),
            &mut follow_up_out as *mut _,
        )
    };
    assert_eq!(follow_up_code, MUU_RESULT_OK);
    // SAFETY: the successful call initialized one owned view buffer.
    let follow_up_view = unsafe { view_json(follow_up_out) };
    assert_ne!(follow_up_view["poisoned"], true);

    // SAFETY: engine was created above and has not been destroyed yet.
    unsafe { muu_session_destroy(engine) };
}
