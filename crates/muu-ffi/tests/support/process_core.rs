use std::{path::Path, ptr, slice};

use muu_dict::{
    CoarsePos, Dictionary, DictionaryBuilder, LeftContextId, LexiconEntry, RightContextId,
    UnknownDefinition,
};
use muu_ffi::*;
use serde_json::{Value, json};
use tempfile::NamedTempFile;

pub fn dictionary(surface: &str) -> NamedTempFile {
    let mut builder =
        DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.provenance(b"name = \"ffi-process-core-test\"\n");
    builder.add_entry(LexiconEntry {
        reading: "きょう".into(),
        surface: surface.into(),
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Noun.id(),
        right_pos_id: CoarsePos::Noun.id(),
        word_cost: 0,
    });
    builder.add_unknown(UnknownDefinition {
        word_cost: 10_000,
        ..UnknownDefinition::default()
    });
    let file = NamedTempFile::new().unwrap();
    std::fs::write(file.path(), builder.build().unwrap()).unwrap();
    Dictionary::open(file.path()).expect("valid dictionary fixture");
    file
}

fn empty_buffer() -> MuuBuffer {
    MuuBuffer {
        data: ptr::null_mut(),
        len: 0,
    }
}

fn take_json(buffer: MuuBuffer, envelope: bool) -> Value {
    assert!(!buffer.data.is_null());
    // SAFETY: the caller passes one live FFI-owned output buffer. It is freed
    // once below, after the JSON has been copied into an owned value.
    let bytes = unsafe { slice::from_raw_parts(buffer.data, buffer.len) };
    let json = serde_json::from_slice(if envelope { &bytes[3..] } else { bytes }).unwrap();
    unsafe { muu_buffer_free(buffer) };
    json
}

struct Engine(*mut MuuSession);

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns its engine on the creating thread.
        unsafe { muu_session_destroy(self.0) };
    }
}

/// Initialize resources through the shipped registry; no session is created here.
pub fn initialize(path: &Path) -> (i32, Value) {
    let config = json!({"dictionary_path":path,"user_dictionary_file_state":"missing"}).to_string();
    let mut out = empty_buffer();
    let status = unsafe {
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
    };
    (status, take_json(out, false))
}
/// Session handles stay on the test's main thread.
pub fn create_and_convert(path: &Path) -> (i32, Value) {
    let (code, error) = initialize(path);
    if code != MUU_RESULT_OK {
        return (code, error);
    }
    let mut engine = ptr::null_mut();
    let mut error = empty_buffer();
    let code = unsafe { muu_session_create(&mut engine, &mut error) };
    assert_eq!(code, MUU_RESULT_OK);
    assert!(!engine.is_null());
    assert!(error.data.is_null());
    let engine = Engine(engine);
    let mut view = Value::Null;
    for (characters, keycode) in [("k", 0), ("y", 0), ("o", 0), ("u", 0), (" ", 49)] {
        let mut request = vec![muu_session::FORMAT_TAG_JSON];
        request.extend_from_slice(&muu_session::SCHEMA_VERSION.to_le_bytes());
        request.extend_from_slice(
            json!({"kind":"key", "key":{
                "event_type":"key_down", "keycode":keycode, "modifiers":0,
                "characters_utf8":characters, "characters_ignoring_modifiers":characters,
                "is_repeat":false
            }})
            .to_string()
            .as_bytes(),
        );
        let mut output = empty_buffer();
        // SAFETY: the guard owns a live engine and request/output storage is valid.
        let code = unsafe {
            muu_session_handle_event(engine.0, request.as_ptr(), request.len(), &mut output)
        };
        assert_eq!(code, MUU_RESULT_OK);
        view = take_json(output, true);
    }
    (code, view)
}

pub fn assert_surface(view: &Value, expected: &str) {
    assert!(view["degraded"].is_null());
    let surface = view["preedit"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["text"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(surface, expected);
}
