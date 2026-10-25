//! The wire format shared with Swift: request and view types, the envelope
//! (`[format tag][schema version][JSON]`) and its size limits
//! (docs/src/internals/architecture.md「C ABI」). The shapes are frozen against
//! `ProtocolModels.swift`; changing a field here changes the C ABI contract.
//! Nothing in this module decides conversion content.
use crate::view::empty_view;
use serde::{Deserialize, Serialize};
// Envelope prefix: one format tag byte, then the schema version as u16
// little-endian, then UTF-8 JSON. Both directions use the same prefix, and
// either side rejects an unknown tag or version instead of guessing.
pub const FORMAT_TAG_JSON: u8 = 1;
pub const SCHEMA_VERSION: u16 = 1;
pub const ENVELOPE_PREFIX_LEN: usize = 3;
// Checked before any JSON decoding. The ordinary limit also covers the raw
// initialization config; the imported-dictionary catalog gets the larger one
// because a full catalog is a single envelope.
pub const MAX_PROTOCOL_ENVELOPE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMPORTED_DICTIONARY_ENVELOPE_BYTES: usize = 64 * 1024 * 1024;
// Unknown fields are accepted in requests and views, so either side can add
// a field without breaking the other. Unknown command names are not errors
// either: `handle_command` answers them with an unconsumed current view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Key {
        key: KeyEvent,
    },
    Command {
        command: String,
        #[serde(default)]
        arg: serde_json::Value,
    },
}

// One NSEvent as Swift saw it. `keycode` is the macOS virtual key code and
// `modifiers` the raw NSEvent.ModifierFlags bits; `event_type` is
// "key_down" or "flags_changed", and any other type is ignored (key-up only
// disarms a pending Shift tap). Rust resolves the meaning through the keymap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub event_type: String,
    pub keycode: u16,
    pub modifiers: u64,
    pub characters_utf8: Option<String>,
    pub characters_ignoring_modifiers: Option<String>,
    pub is_repeat: bool,
}

// The whole visible state after one request; Swift replaces, never merges.
// Every offset is UTF-16 over NFC text, computed once in `view.rs`. Absent
// optional fields mean "none". `consumed` tells Swift whether to swallow the
// key; false returns it to the client, after `commit_text` if both are set.
// `degraded` is set only by `Session::current_view`, and
// Swift does not decode it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewState {
    pub preedit: Vec<PreeditRun>,
    pub caret_utf16: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<TextRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segments: Option<Vec<SegmentView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<CandidateList>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_text: Option<String>,
    pub mode: InputMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composition_mode_transition: Option<CompositionModeTransition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded: Option<DegradedMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poisoned: Option<bool>,
    pub consumed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ViewError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositionModeTransition {
    Reading,
    Literal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreeditRun {
    pub text: String,
    pub utf16_start: usize,
    pub utf16_len: usize,
    pub is_focused: bool,
    pub is_converted: bool,
    pub underline_style: UnderlineStyle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    pub start: usize,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentView {
    pub reading_range: TextRange,
    pub candidate_focus: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateList {
    pub items: Vec<CandidateItem>,
    pub selected_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateItem {
    pub id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    Hiragana,
    FullKatakana,
    HalfKatakana,
    FullAlnum,
    HalfAlnum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnderlineStyle {
    Single,
    Thick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedMode {
    KanaOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewError {
    pub code: String,
    // Always a JSON object, fixed by the type rather than by each producer.
    // Swift's full ViewState decoder also requires an object. Its bounded
    // OK-response recovery can retain separately validated commit text;
    // keeping this type strict avoids that failure path for normal views.
    pub params: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    TooShort,
    TooLarge,
    UnsupportedFormat,
    UnsupportedSchema,
    InvalidJson,
}

// The size check comes first, so an oversized input is refused before any
// prefix or JSON work, whatever its contents.
pub fn decode_request(bytes: &[u8]) -> Result<Request, ProtocolError> {
    if bytes.len() > MAX_PROTOCOL_ENVELOPE_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    if bytes.len() < ENVELOPE_PREFIX_LEN {
        return Err(ProtocolError::TooShort);
    }
    if bytes[0] != FORMAT_TAG_JSON {
        return Err(ProtocolError::UnsupportedFormat);
    }
    let version = u16::from_le_bytes([bytes[1], bytes[2]]);
    if version != SCHEMA_VERSION {
        return Err(ProtocolError::UnsupportedSchema);
    }
    serde_json::from_slice(&bytes[ENVELOPE_PREFIX_LEN..]).map_err(|_| ProtocolError::InvalidJson)
}

pub fn encode_view(view: &ViewState) -> Result<Vec<u8>, serde_json::Error> {
    let json = serde_json::to_vec(view)?;
    let mut payload = Vec::with_capacity(ENVELOPE_PREFIX_LEN + json.len());
    payload.push(FORMAT_TAG_JSON);
    payload.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    payload.extend_from_slice(&json);
    Ok(payload)
}

// What a Session that panicked answers from then on. It is consumed so the
// key is not delivered to the client on top of a lost composition; Swift
// clears the marked text and replaces the Session (docs/src/internals/architecture.md「失敗したとき」).
pub fn poisoned_view() -> ViewState {
    ViewState {
        poisoned: Some(true),
        consumed: true,
        error: Some(ViewError {
            code: "engine_panic".to_owned(),
            params: serde_json::Map::new(),
        }),
        ..empty_view(true)
    }
}

#[cfg(test)]
mod tests {

    use crate::test_support::*;

    use crate::*;

    #[test]
    fn request_round_trip_prefix_is_validated() {
        let mut bytes = vec![FORMAT_TAG_JSON];
        bytes.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        bytes.extend_from_slice(br#"{"kind":"command","command":"force_commit","arg":null}"#);

        let request = decode_request(&bytes).expect("valid request");
        assert!(matches!(request, Request::Command { command, .. } if command == "force_commit"));
    }

    #[test]
    fn request_decoder_rejects_oversized_envelopes_before_json_decode() {
        let bytes = vec![b' '; MAX_PROTOCOL_ENVELOPE_BYTES + 1];
        assert_eq!(decode_request(&bytes), Err(ProtocolError::TooLarge));
    }

    // The object-only contract on the Rust side: an existing object round-trips
    // to the same bytes, every other JSON kind is refused, and the panic view
    // still sends an empty object.
    #[test]
    fn view_error_params_require_an_object_and_preserve_wire_bytes() {
        let bytes =
            br#"{"code":"fixture_error","params":{"current_length":2,"requested_length":3}}"#;
        let error: ViewError = serde_json::from_slice(bytes).unwrap();
        assert_eq!(serde_json::to_vec(&error).unwrap(), bytes);
        for params in ["null", "[]", "1", "\"text\""] {
            let wire = format!(r#"{{"code":"fixture_error","params":{params}}}"#);
            assert!(
                serde_json::from_str::<ViewError>(&wire).is_err(),
                "non-object params: {params}"
            );
        }
        assert_eq!(
            serde_json::to_string(&poisoned_view().error.unwrap()).unwrap(),
            r#"{"code":"engine_panic","params":{}}"#
        );
    }

    #[test]
    fn schema_v1_shape_is_frozen() {
        let mut session =
            new_session(None, None, TestSessionConfig::default()).expect("embedded table");
        let view = session.handle(key("a", 0));
        let payload = encode_view(&view).expect("serializable view");

        assert_eq!(&payload[..3], &[FORMAT_TAG_JSON, 1, 0]);
        let json: serde_json::Value = serde_json::from_slice(&payload[3..]).unwrap();
        insta::assert_json_snapshot!(json, @r###"
        {
          "caret_utf16": 1,
          "consumed": true,
          "mode": "hiragana",
          "preedit": [
            {
              "is_converted": false,
              "is_focused": true,
              "text": "あ",
              "underline_style": "single",
              "utf16_len": 1,
              "utf16_start": 0
            }
          ]
        }
        "###);
    }
}
