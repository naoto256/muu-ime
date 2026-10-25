//! Presence of opaque stored bytes, independent of each file's codec.
//!
//! The host reports which of three states it found for the User file and for
//! each Imported and Postal read; Swift's `FileSafety.StoredFileState` spells
//! the same wire names. Only the classification is shared. What a state
//! means stays with each domain: User treats missing as empty and unreadable
//! as read-only, and the Imported and Postal load plans decide per request.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StoredFileState {
    Missing,
    Present,
    Unreadable,
}

#[cfg(test)]
mod tests {
    use super::*;
    // The three enums this replaced used these snake_case names on the host
    // wire; the shared type must keep them exactly, both ways. The negatives
    // catch accepting null, a number, the Rust variant spelling or a fourth
    // state.
    #[test]
    fn wire_names_are_the_existing_three_states() {
        for (state, text) in [
            (StoredFileState::Missing, "missing"),
            (StoredFileState::Present, "present"),
            (StoredFileState::Unreadable, "unreadable"),
        ] {
            let json = format!("\"{text}\"");
            assert_eq!(
                serde_json::from_str::<StoredFileState>(&json).unwrap(),
                state
            );
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
        }
        for text in ["null", "0", "\"Missing\"", "\"unknown\""] {
            assert!(serde_json::from_str::<StoredFileState>(text).is_err());
        }
    }
}
