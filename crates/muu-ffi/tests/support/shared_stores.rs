use muu_ffi::*;
use serde_json::{Value, json};
use std::{ptr, slice};
pub const EMPTY: MuuBuffer = MuuBuffer {
    data: ptr::null_mut(),
    len: 0,
};
pub struct Engine(pub *mut MuuSession);
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { muu_session_destroy(self.0) };
    }
}
pub fn take(buffer: MuuBuffer, envelope: bool) -> Value {
    assert!(!buffer.data.is_null());
    // SAFETY: FFI returned this live buffer; copy before freeing it once.
    let bytes = unsafe { slice::from_raw_parts(buffer.data, buffer.len) };
    let result = serde_json::from_slice(if envelope { &bytes[3..] } else { bytes }).unwrap();
    unsafe { muu_buffer_free(buffer) };
    result
}
pub fn initialize(mut config: Value, learning: Option<&[u8]>) -> (i32, Value) {
    let user = config.as_object_mut().unwrap().remove("user_dictionary");
    let user_file = user.map(|value| {
        muu_session::encode_user_dictionary_file(&serde_json::from_value(value).unwrap()).unwrap()
    });
    config["user_dictionary_file_state"] = json!(if user_file.is_some() {
        "present"
    } else {
        "missing"
    });
    let user_file = user_file.unwrap_or_default();
    let bytes = serde_json::to_vec(&config).unwrap();
    let present = learning.is_some();
    let learning = learning.unwrap_or_default();
    let mut out = EMPTY;
    let status = unsafe {
        muu_core_initialize_with_user_dictionary_file(
            bytes.as_ptr(),
            bytes.len(),
            learning.as_ptr(),
            learning.len(),
            present,
            user_file.as_ptr(),
            user_file.len(),
            &mut out,
        )
    };
    (status, take(out, false))
}
pub fn create() -> Result<Engine, Value> {
    let mut session = ptr::null_mut();
    let mut error = EMPTY;
    let status = unsafe { muu_session_create(&mut session, &mut error) };
    if status == MUU_RESULT_OK {
        assert!(error.data.is_null());
        Ok(Engine(session))
    } else {
        assert!(session.is_null());
        Err(take(error, false))
    }
}
pub fn learning() -> (Value, Value) {
    let mut data = EMPTY;
    let mut version = EMPTY;
    assert_eq!(
        unsafe { muu_core_personal_frequency_snapshot(ptr::null(), 0, &mut data, &mut version) },
        0
    );
    (take(data, false), take(version, false))
}
pub fn event(engine: &Engine, value: Value) -> Value {
    let mut bytes = vec![1, 1, 0];
    bytes.extend_from_slice(value.to_string().as_bytes());
    let mut out = EMPTY;
    assert_eq!(
        unsafe { muu_session_handle_event(engine.0, bytes.as_ptr(), bytes.len(), &mut out) },
        MUU_RESULT_OK
    );
    take(out, true)
}
pub fn convert(engine: &Engine, text: &str) -> Value {
    for character in text
        .chars()
        .map(|c| c.to_string())
        .chain(std::iter::once(" ".into()))
    {
        let view = event(
            engine,
            json!({"kind":"key", "key": {
                "event_type":"key_down", "keycode":if character == " " {49} else {0},
                "modifiers":0, "characters_utf8":character,
                "characters_ignoring_modifiers":character,"is_repeat":false
            }}),
        );
        if character == " " {
            return view;
        }
    }
    unreachable!()
}
pub fn surface(view: &Value) -> String {
    view["preedit"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["text"].as_str().unwrap())
        .collect()
}
pub fn cancel(engine: &Engine) {
    event(
        engine,
        json!({"kind":"command","command":"cancel","arg":null}),
    );
}
