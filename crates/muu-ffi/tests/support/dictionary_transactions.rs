// Synthetic dictionary bytes travel through the shipped load/edit ABI.
use super::support::*;
use muu_ffi::*;
use serde_json::{Value, json};
use std::slice;

pub fn upsert_user(surface: &str) {
    let mut out = EMPTY;
    assert_eq!(unsafe { muu_core_user_dictionary_view(&mut out) }, 0);
    let view = take(out, false);
    let command = json!({"operation":"upsert",
        "expected_revision":view["management_revision"],
        "entry_id":view["entries"][0]["entry_id"],
        "reading":"むう","surface":surface,"part_of_speech":"NOUN","priority":"high"});
    let bytes = serde_json::to_vec(&command).unwrap();
    let (mut file, mut plan) = (EMPTY, EMPTY);
    assert_eq!(
        unsafe {
            muu_core_user_dictionary_prepare(bytes.as_ptr(), bytes.len(), &mut file, &mut plan)
        },
        0
    );
    assert!(file.len > 0);
    unsafe {
        muu_buffer_free(file);
    }
    let plan = take(plan, false);
    dictionary_ack(muu_core_user_dictionary_publish, &plan["ticket"]);
    dictionary_ack(muu_core_user_dictionary_cancel, &plan["ticket"]);
}

fn dictionary_ack(function: unsafe extern "C" fn(*const u8, usize) -> i32, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    assert_eq!(unsafe { function(bytes.as_ptr(), bytes.len()) }, 0);
}
fn raw_bytes(buffer: MuuBuffer) -> Vec<u8> {
    let bytes = if buffer.len == 0 {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(buffer.data, buffer.len) }.to_vec()
    };
    unsafe {
        muu_buffer_free(buffer);
    }
    bytes
}
type ImportedStep =
    unsafe extern "C" fn(*const u8, usize, *const u8, usize, *mut MuuBuffer, *mut MuuBuffer) -> i32;
fn imported_step(function: ImportedStep, command: Value, data: &[u8]) -> Value {
    let command = serde_json::to_vec(&command).unwrap();
    let (mut generation, mut plan) = (EMPTY, EMPTY);
    assert_eq!(
        unsafe {
            function(
                command.as_ptr(),
                command.len(),
                data.as_ptr(),
                data.len(),
                &mut generation,
                &mut plan,
            )
        },
        0
    );
    raw_bytes(generation);
    take(plan, false)
}
fn finish_imported(step: &Value, outcome: &str) {
    dictionary_ack(muu_core_imported_dictionary_publish, &step["ticket"]);
    dictionary_ack(
        muu_core_imported_dictionary_finish,
        &json!({
            "ticket":step["ticket"],"outcome":outcome,
            "revision":step["ready"]["published_revision"],
            "generation_sha256":step["ready"]["generation_sha256"],
        }),
    );
    dictionary_ack(muu_core_imported_dictionary_cancel, &step["ticket"]);
}
pub fn load_imported_fixture() {
    let step = imported_step(
        muu_core_imported_dictionary_load_begin,
        json!({"expected_revision":0,"file_state":"missing","now_unix_ms":1}),
        &[],
    );
    assert_eq!(step["request"]["kind"], "legacy_catalog");
    let step = imported_step(
        muu_core_imported_dictionary_load_supply,
        json!({
            "response":"read","ticket":step["ticket"],
            "request_id":step["request"]["request_id"],"file_state":"missing",
        }),
        &[],
    );
    finish_imported(&step, "loaded");
    let source = "#!muu-dictionary\t1\n#id\tstartup.imported\n#name\tFixture\n#version\t1\n#author\tFixture\n#license\tCC0\n#provenance\tSelf-authored\nreading\tsurface\tpos\tpriority\nきょう\tImportedToday\tNOUN\thigh\n".as_bytes();
    let step = imported_step(
        muu_core_imported_dictionary_prepare,
        json!({"action":"import","expected_revision":1,"source_file_name":"fixture.muudict","imported_at_unix_ms":1,"enabled":true}),
        source,
    );
    let request =
        serde_json::to_vec(&json!({"ticket":step["ticket"],"sha256":step["write_blob"]["sha256"]}))
            .unwrap();
    let mut blob = EMPTY;
    assert_eq!(
        unsafe {
            muu_core_imported_dictionary_prepared_blob(request.as_ptr(), request.len(), &mut blob)
        },
        0
    );
    assert!(!raw_bytes(blob).is_empty());
    let step = imported_step(
        muu_core_imported_dictionary_prepare_supply,
        json!({
            "response":"written","ticket":step["ticket"],"sha256":step["write_blob"]["sha256"],
        }),
        &[],
    );
    finish_imported(&step, "new_pointer_durable");
}

pub fn load_postal_fixture() {
    let tsv = b"# muu-postal-code\t1\n1000001\tPostalAddress\n";
    // The self-authored fixture's checksum is independent of the parser.
    let manifest = serde_json::to_vec(&json!({
        "schema_version":1,"source_url":muu_session::POSTAL_SOURCE_URL,
        "source_page_url":muu_session::POSTAL_SOURCE_PAGE_URL,
        "source_last_modified":"unknown","retrieved_at":"2026-01-01T00:00:00Z",
        "zip_sha256":"0".repeat(64),"csv_sha256":"1".repeat(64),
        "snapshot_sha256":"72419db1d4bb586615ad76a1f535f749ec27d11164c9d3719001d934b159b2b8","entry_count":1,
    })).unwrap();
    let pointer = format!("{}\n", "a".repeat(64));
    let command =
        serde_json::to_vec(&json!({"expected_revision":0,"current_state":"present"})).unwrap();
    let (mut t, mut m, mut p) = (EMPTY, EMPTY, EMPTY);
    assert_eq!(
        unsafe {
            muu_core_postal_code_load_begin(
                command.as_ptr(),
                command.len(),
                pointer.as_ptr(),
                pointer.len(),
                &mut t,
                &mut m,
                &mut p,
            )
        },
        0
    );
    raw_bytes(t);
    raw_bytes(m);
    let mut step = take(p, false);
    for bytes in [manifest.as_slice(), tsv.as_slice()] {
        let command = serde_json::to_vec(&json!({"ticket":step["ticket"],"request_id":step["request"]["request_id"],"file_state":"present"})).unwrap();
        let (mut t, mut m, mut p) = (EMPTY, EMPTY, EMPTY);
        assert_eq!(
            unsafe {
                muu_core_postal_code_load_supply(
                    command.as_ptr(),
                    command.len(),
                    bytes.as_ptr(),
                    bytes.len(),
                    &mut t,
                    &mut m,
                    &mut p,
                )
            },
            0
        );
        raw_bytes(t);
        raw_bytes(m);
        step = take(p, false);
    }
    dictionary_ack(muu_core_postal_code_publish, &step["ticket"]);
    dictionary_ack(
        muu_core_postal_code_finish,
        &json!({
            "ticket":step["ticket"],"outcome":"loaded",
            "revision":step["ready"]["published_revision"],
            "generation":step["ready"]["generation"],
        }),
    );
    dictionary_ack(muu_core_postal_code_cancel, &step["ticket"]);
}
