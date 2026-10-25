//! Key events to actions through `data/default-keymap.toml`. Swift forwards
//! raw events; only this table decides what a key means.
use serde::Deserialize;

use crate::KeyEvent;

const SHIFT_MODIFIER: u64 = 1 << 17;
const CONTROL_MODIFIER: u64 = 1 << 18;
const OPTION_MODIFIER: u64 = 1 << 19;
const COMMAND_MODIFIER: u64 = 1 << 20;
const FUNCTION_MODIFIER: u64 = 1 << 23;
const BINDING_MODIFIERS: u64 =
    SHIFT_MODIFIER | CONTROL_MODIFIER | OPTION_MODIFIER | COMMAND_MODIFIER;
const PASSTHROUGH_MODIFIERS: u64 =
    CONTROL_MODIFIER | OPTION_MODIFIER | COMMAND_MODIFIER | FUNCTION_MODIFIER;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Commit,
    CommitAndPassThrough,
    Cancel,
    Backspace,
    ConvertOrNextCandidate,
    NextCandidate,
    PreviousCandidate,
    MoveSegmentLeft,
    MoveSegmentRight,
    ShrinkSegment,
    ExtendSegment,
    SelectCandidate,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Action<'event> {
    Command {
        command: Command,
        argument: Option<usize>,
    },
    Input(&'event str),
    LiteralInput(&'event str),
    PassThrough,
}

#[derive(Debug)]
pub(crate) struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    pub(crate) fn default_muu() -> Result<Self, KeymapError> {
        Self::parse(include_str!("../data/default-keymap.toml"))
    }

    fn parse(source: &str) -> Result<Self, KeymapError> {
        let source: KeymapSource = toml::from_str(source).map_err(|_| KeymapError::InvalidToml)?;
        if source.schema_version != 1 {
            return Err(KeymapError::UnsupportedSchema(source.schema_version));
        }
        let bindings = source
            .bindings
            .into_iter()
            .map(Binding::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { bindings })
    }

    // Resolution order: the first binding whose keycode and Shift/Control/
    // Option/Command set match exactly; else any Control, Option, Command or
    // Function modifier passes the key through; else the key's text is input.
    // Shift with only ASCII letters starts a literal span; shifted symbols are
    // ordinary input.
    pub(crate) fn resolve<'event>(&self, key: &'event KeyEvent) -> Action<'event> {
        // AppKit sets NSEventModifierFlagFunction on navigation keys as a
        // key-kind marker. The keycode already identifies those keys, so the
        // marker must not take part in binding comparison. Keep it in the
        // passthrough mask below so an unbound F-key never becomes text input.
        let binding_modifiers = key.modifiers & BINDING_MODIFIERS;
        if let Some(binding) = self.bindings.iter().find(|binding| {
            binding.keycode == key.keycode
                && binding.modifiers & BINDING_MODIFIERS == binding_modifiers
        }) {
            return Action::Command {
                command: binding.command,
                argument: binding.argument,
            };
        }
        if key.modifiers & PASSTHROUGH_MODIFIERS != 0 {
            return Action::PassThrough;
        }
        let Some(text) = key
            .characters_utf8
            .as_deref()
            .filter(|text| !text.is_empty())
        else {
            return Action::PassThrough;
        };
        if binding_modifiers == SHIFT_MODIFIER
            && text
                .chars()
                .all(|character| character.is_ascii_alphabetic())
        {
            Action::LiteralInput(text)
        } else {
            Action::Input(text)
        }
    }
}

#[derive(Debug)]
struct Binding {
    keycode: u16,
    modifiers: u64,
    command: Command,
    argument: Option<usize>,
}

// "function" is accepted in the table for readability, but `resolve` masks it
// out of the comparison, so it does not distinguish bindings.
impl TryFrom<BindingSource> for Binding {
    type Error = KeymapError;

    fn try_from(source: BindingSource) -> Result<Self, Self::Error> {
        let mut modifiers = 0;
        for modifier in source.modifiers {
            modifiers |= match modifier.as_str() {
                "shift" => SHIFT_MODIFIER,
                "control" => CONTROL_MODIFIER,
                "option" => OPTION_MODIFIER,
                "command" => COMMAND_MODIFIER,
                "function" => FUNCTION_MODIFIER,
                _ => return Err(KeymapError::UnknownModifier),
            };
        }
        Ok(Self {
            keycode: source.keycode,
            modifiers,
            command: source.command,
            argument: source.argument,
        })
    }
}

#[derive(Deserialize)]
struct KeymapSource {
    schema_version: u16,
    bindings: Vec<BindingSource>,
}

#[derive(Deserialize)]
struct BindingSource {
    keycode: u16,
    #[serde(default)]
    modifiers: Vec<String>,
    command: Command,
    argument: Option<usize>,
}

impl<'de> Deserialize<'de> for Command {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "commit" => Ok(Self::Commit),
            "commit_and_passthrough" => Ok(Self::CommitAndPassThrough),
            "cancel" => Ok(Self::Cancel),
            "backspace" => Ok(Self::Backspace),
            "convert_or_next_candidate" => Ok(Self::ConvertOrNextCandidate),
            "next_candidate" => Ok(Self::NextCandidate),
            "previous_candidate" => Ok(Self::PreviousCandidate),
            "move_segment_left" => Ok(Self::MoveSegmentLeft),
            "move_segment_right" => Ok(Self::MoveSegmentRight),
            "shrink_segment" => Ok(Self::ShrinkSegment),
            "extend_segment" => Ok(Self::ExtendSegment),
            "select_candidate" => Ok(Self::SelectCandidate),
            _ => Err(serde::de::Error::custom("unknown keymap command")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeymapError {
    InvalidToml,
    UnsupportedSchema(u16),
    UnknownModifier,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(keycode: u16, modifiers: u64, text: Option<&str>) -> KeyEvent {
        KeyEvent {
            event_type: "key_down".to_owned(),
            keycode,
            modifiers,
            characters_utf8: text.map(str::to_owned),
            characters_ignoring_modifiers: text.map(str::to_owned),
            is_repeat: false,
        }
    }

    #[test]
    fn default_keymap_distinguishes_segment_move_from_resize() {
        let keymap = Keymap::default_muu().unwrap();
        assert_eq!(
            keymap.resolve(&key(123, FUNCTION_MODIFIER, None)),
            Action::Command {
                command: Command::MoveSegmentLeft,
                argument: None,
            }
        );
        assert_eq!(
            keymap.resolve(&key(124, FUNCTION_MODIFIER, None)),
            Action::Command {
                command: Command::MoveSegmentRight,
                argument: None,
            }
        );
        assert_eq!(
            keymap.resolve(&key(123, SHIFT_MODIFIER | FUNCTION_MODIFIER, None)),
            Action::Command {
                command: Command::ShrinkSegment,
                argument: None,
            }
        );
        assert_eq!(
            keymap.resolve(&key(124, SHIFT_MODIFIER | FUNCTION_MODIFIER, None)),
            Action::Command {
                command: Command::ExtendSegment,
                argument: None,
            }
        );
    }

    #[test]
    fn shift_space_resolves_previous_while_command_space_commits_and_passes_through() {
        let keymap = Keymap::default_muu().unwrap();
        assert_eq!(
            keymap.resolve(&key(49, SHIFT_MODIFIER, Some(" "))),
            Action::Command {
                command: Command::PreviousCandidate,
                argument: None,
            }
        );
        assert_eq!(
            keymap.resolve(&key(49, COMMAND_MODIFIER, Some(" "))),
            Action::Command {
                command: Command::CommitAndPassThrough,
                argument: None,
            }
        );
    }

    #[test]
    fn shifted_letters_start_literal_input_after_explicit_bindings() {
        let keymap = Keymap::default_muu().unwrap();
        assert_eq!(
            keymap.resolve(&key(46, SHIFT_MODIFIER, Some("M"))),
            Action::LiteralInput("M")
        );
        assert_eq!(
            keymap.resolve(&key(44, SHIFT_MODIFIER, Some("?"))),
            Action::Input("?")
        );
    }

    #[test]
    fn unbound_shortcuts_and_function_keys_pass_through_before_text() {
        let keymap = Keymap::default_muu().unwrap();
        for event in [
            key(8, COMMAND_MODIFIER, Some("c")),
            key(0, CONTROL_MODIFIER, Some("a")),
            key(0, OPTION_MODIFIER, Some("å")),
            key(122, FUNCTION_MODIFIER, Some("\u{f704}")),
        ] {
            assert_eq!(keymap.resolve(&event), Action::PassThrough);
        }
    }
}
