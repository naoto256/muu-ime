//! Public-protocol replay for the pinned AJIMEE quality and timing gates.
//! Input, output, dictionary and model paths are supplied by the Python driver.
use muu_session::{
    CommitStreamConfig, Core, CoreConfig, CoreSessionSettings, KeyEvent, LanguageModelState,
    Request, Session, ViewState,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    path::Path,
    sync::Arc,
    time::Instant,
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Deserialize)]
struct Item {
    id: String,
    reading: String,
    context: String,
}
fn command(s: &mut Session, command: &str, arg: Value) -> ViewState {
    s.handle(Request::Command {
        command: command.into(),
        arg,
    })
}
fn key(s: &mut Session, code: u16, text: &str) -> ViewState {
    s.handle(Request::Key {
        key: KeyEvent {
            event_type: "key_down".into(),
            keycode: code,
            modifiers: 0,
            characters_utf8: Some(text.into()),
            characters_ignoring_modifiers: Some(text.into()),
            is_repeat: false,
        },
    })
}
fn text(v: &ViewState) -> String {
    v.preedit.iter().map(|r| r.text.as_str()).collect()
}
fn checked(v: ViewState) -> Result<ViewState, &'static str> {
    if v.error.is_some() || v.poisoned == Some(true) {
        Err("product_view_error")
    } else {
        Ok(v)
    }
}
fn prepare(core: &Arc<Core>, item: &Item) -> Result<Session, &'static str> {
    let mut s = core.create_session().map_err(|_| "session_create")?;
    // A literal commit seeds the *real* product preceding-commit context.
    if !item.context.is_empty() {
        checked(command(&mut s, "set_mode", json!({"mode":"half_alnum"})))?;
        let v = checked(key(&mut s, 0, &item.context))?;
        if v.commit_text.as_deref() != Some(item.context.as_str()) {
            return Err("context_seed_mismatch");
        }
        checked(command(&mut s, "set_mode", json!({"mode":"hiragana"})))?;
    }
    // Public protocol only: preserve a nonempty preedit across Hiragana <->
    // HalfAlnum, append the exact literal span, then remove the sentinel.
    // This avoids reverse romanization and never calls a copied selector.
    checked(key(&mut s, 0, "a"))?;
    checked(command(&mut s, "set_mode", json!({"mode":"half_alnum"})))?;
    checked(key(&mut s, 0, &item.reading))?;
    checked(command(&mut s, "set_mode", json!({"mode":"hiragana"})))?;
    for _ in item.reading.graphemes(true) {
        checked(key(&mut s, 123, ""))?;
    }
    let v = checked(key(&mut s, 51, ""))?;
    if text(&v) != item.reading {
        return Err("preedit_reading_mismatch");
    }
    Ok(s)
}
fn convert(core: &Arc<Core>, item: &Item) -> Result<Value, &'static str> {
    let mut s = prepare(core, item)?;
    let start = Instant::now();
    let view = checked(command(&mut s, "convert", Value::Null))?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let output = text(&view);
    let committed = checked(command(&mut s, "commit", Value::Null))?;
    if committed.commit_text.as_deref() != Some(&output) {
        return Err("commit_output_mismatch");
    }
    let record = core
        .take_commit_stream_event()
        .ok_or("missing_product_record")?;
    let record: Value = serde_json::from_slice(&record).map_err(|_| "invalid_product_record")?;
    Ok(
        json!({"id":item.id,"status":"ok","output":output,"conversion_ms":elapsed,
        "view":view,"commit":committed,"event":record}),
    )
}

#[cfg(target_os = "macos")]
fn memory() -> Result<Value, Box<dyn std::error::Error>> {
    // mach/task_info.h: MACH_TASK_BASIC_INFO (20), 64-bit byte counts.
    #[repr(C)]
    struct BasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: [i32; 2],
        system_time: [i32; 2],
        policy: i32,
        suspend_count: i32,
    }
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    let mut info: BasicInfo = unsafe { std::mem::zeroed() };
    let expected = (std::mem::size_of::<BasicInfo>() / 4) as u32;
    let mut count = expected;
    // SAFETY: BasicInfo matches the SDK layout and count describes its storage.
    let result = unsafe {
        task_info(
            mach_task_self_,
            20,
            (&mut info as *mut BasicInfo).cast(),
            &mut count,
        )
    };
    if result != 0 || count != expected || info.resident_size == 0 {
        return Err("MACH_TASK_BASIC_INFO failed".into());
    }
    Ok(json!({"rss_bytes":info.resident_size,"peak_rss_bytes":info.resident_size_max}))
}

#[cfg(not(target_os = "macos"))]
fn memory() -> Result<Value, Box<dyn std::error::Error>> {
    Err("this measurement gate requires macOS".into())
}

fn emit(value: Value) {
    println!("{value}");
    io::stdout().flush().unwrap();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.len() != 4 {
        return Err("required: DICTIONARY_PATH MODEL_DIR edge|zenz".into());
    }
    let kind = match arguments[3].as_str() {
        "edge" => muu_lm::ModelKind::Edge,
        "zenz" => muu_lm::ModelKind::Zenz,
        _ => return Err("unknown model".into()),
    };
    let start = Instant::now();
    let core = Arc::new(
        Core::new(CoreConfig {
            dictionary_path: Some(Path::new(&arguments[1])),
        })
        .map_err(|_| "public Core initialization failed")?,
    );
    if !core.has_dictionary() {
        return Err("public dictionary did not load".into());
    }
    core.configure_sessions(CoreSessionSettings {
        conversion_lm_dir: Some(Path::new(&arguments[2]).to_path_buf()),
        conversion_lm_model: Some(kind),
        conversion_lm_load_synchronously: true,
        commit_stream_config: Some(CommitStreamConfig {
            schema: "muu.commit-stream.v5".into(),
            process_nonce: "public-conversion-test".into(),
            source_commit: "public-conversion-test".into(),
            build: 0,
        }),
        ..Default::default()
    })
    .map_err(|_| "public session configuration failed")?;
    if core.language_model().state != LanguageModelState::Available {
        return Err("requested public model did not load".into());
    }
    emit(json!({"event":"initialized","model":arguments[3],
        "initialization_ms":start.elapsed().as_secs_f64()*1000.0,"memory":memory()?}));
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        if request.get("control").and_then(Value::as_str) == Some("warmup") {
            let _ = convert(
                &core,
                &Item {
                    id: "warmup".into(),
                    reading: "きょうはいいてんきです".into(),
                    context: String::new(),
                },
            )?;
            emit(json!({"event":"warmup","memory":memory()?}));
        } else {
            let item: Item = serde_json::from_value(request)?;
            let mut row = convert(&core, &item).map_err(|error| format!("{}: {error}", item.id))?;
            row["memory"] = memory()?;
            emit(row);
        }
    }
    Ok(())
}
