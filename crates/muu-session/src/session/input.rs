//! Key events, commands and composition modes: romaji input, Shift literal
//! input, mode switches, cancel and Backspace. Anything that converts or
//! touches segments is delegated to `conversion`.
use super::*;
// NSEvent.ModifierFlags bit positions, as Swift forwards them unchanged.
pub(crate) const CAPS_LOCK_MODIFIER: u64 = 1 << 16;
pub(crate) const SHIFT_MODIFIER: u64 = 1 << 17;
pub(crate) const CONTROL_MODIFIER: u64 = 1 << 18;
pub(crate) const OPTION_MODIFIER: u64 = 1 << 19;
pub(crate) const COMMAND_MODIFIER: u64 = 1 << 20;
pub(crate) const NUMERIC_PAD_MODIFIER: u64 = 1 << 21;
pub(crate) const HELP_MODIFIER: u64 = 1 << 22;
pub(crate) const FUNCTION_MODIFIER: u64 = 1 << 23;
pub(crate) const SHIFT_TAP_BLOCKING_MODIFIERS: u64 = CAPS_LOCK_MODIFIER
    | CONTROL_MODIFIER
    | OPTION_MODIFIER
    | COMMAND_MODIFIER
    | NUMERIC_PAD_MODIFIER
    | HELP_MODIFIER
    | FUNCTION_MODIFIER;
// macOS virtual key codes of the two Shift keys.
pub(crate) const LEFT_SHIFT_KEYCODE: u16 = 56;
pub(crate) const RIGHT_SHIFT_KEYCODE: u16 = 60;

impl Session {
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ViewState {
        match key.event_type.as_str() {
            "flags_changed" => return self.handle_flags_changed(key),
            "key_down" => self.shift_tap_keycode = None,
            _ => {
                self.shift_tap_keycode = None;
                return self.current_view(false);
            }
        }

        match self.core.keymap.resolve(&key) {
            KeyAction::Command { command, argument } => {
                self.handle_key_command(command, argument, key.characters_utf8.as_deref())
            }
            KeyAction::Input(text) => self.input_text(text),
            KeyAction::LiteralInput(text) => self.begin_literal_input(text),
            KeyAction::PassThrough => {
                self.current_view(!self.composer.is_empty() || self.segments.is_some())
            }
        }
    }

    // A Shift tap toggles reading/literal input. It arms when one Shift goes
    // down with no other modifier, and fires only when that same key comes
    // up with none added. Any key down, command, other key's flags change or
    // second Shift disarms it, so Shift used as a modifier never toggles.
    // Flag changes are never consumed unless the toggle actually happened.
    pub(crate) fn handle_flags_changed(&mut self, key: KeyEvent) -> ViewState {
        let is_shift_key = matches!(key.keycode, LEFT_SHIFT_KEYCODE | RIGHT_SHIFT_KEYCODE);
        let shift_is_down = key.modifiers & SHIFT_MODIFIER != 0;
        let has_blocking_modifier = key.modifiers & SHIFT_TAP_BLOCKING_MODIFIERS != 0;

        if !is_shift_key {
            self.shift_tap_keycode = None;
            return self.current_view(false);
        }

        if shift_is_down {
            self.shift_tap_keycode = if !has_blocking_modifier && self.shift_tap_keycode.is_none() {
                Some(key.keycode)
            } else {
                None
            };
            return self.current_view(false);
        }

        let armed_keycode = self.shift_tap_keycode.take();
        if !has_blocking_modifier
            && armed_keycode == Some(key.keycode)
            && self.toggle_composition_input_mode_from_shift_tap()
        {
            return self.current_view(true);
        }
        self.current_view(false)
    }

    pub(crate) fn handle_key_command(
        &mut self,
        command: KeyCommand,
        argument: Option<usize>,
        fallback_text: Option<&str>,
    ) -> ViewState {
        // In an alnum mode with nothing composed, bound keys are plain text:
        // Space and printable characters commit directly; Enter, Delete and
        // arrows (non-graphic) pass through to the client.
        if command != KeyCommand::CommitAndPassThrough
            && matches!(self.mode, InputMode::FullAlnum | InputMode::HalfAlnum)
            && self.segments.is_none()
            && self.composer.is_empty()
        {
            if let Some(text) = fallback_text.filter(|text| {
                !text.is_empty()
                    && text
                        .chars()
                        .all(|character| character.is_ascii_graphic() || character == ' ')
            }) {
                return self.input_text(text);
            }
            return self.current_view(false);
        }
        match command {
            KeyCommand::Commit => self.commit(),
            KeyCommand::CommitAndPassThrough => self.handle_input_source_switch(),
            KeyCommand::Cancel => self.handle_cancel(),
            KeyCommand::Backspace => self.backspace(),
            KeyCommand::ConvertOrNextCandidate => {
                if self.segments.is_some() {
                    self.cycle_candidate(1)
                } else if let Some(view) = self.input_literal_space() {
                    view
                } else {
                    self.convert()
                }
            }
            KeyCommand::NextCandidate => self.cycle_candidate(1),
            KeyCommand::PreviousCandidate => self.cycle_candidate(-1),
            KeyCommand::MoveSegmentLeft => self.move_preedit_cursor_or_segment(-1),
            KeyCommand::MoveSegmentRight => self.move_preedit_cursor_or_segment(1),
            KeyCommand::ShrinkSegment => self.resize_segment(-1),
            KeyCommand::ExtendSegment => self.resize_segment(1),
            // Number keys: pick a candidate while converted, otherwise type
            // the digit.
            KeyCommand::SelectCandidate => {
                if self.segments.is_some() {
                    self.select_candidate_index(argument.unwrap_or(0))
                } else if let Some(text) = fallback_text {
                    self.input_text(text)
                } else {
                    self.current_view(false)
                }
            }
        }
    }

    // Commands sent by Swift, not resolved from keys. Bad arguments give an
    // error view that is still consumed; an unknown command is ignored.
    pub(crate) fn handle_command(&mut self, command: &str, arg: &serde_json::Value) -> ViewState {
        match command {
            #[cfg(test)]
            "panic_replace_shared_user" => {
                self.core.publish_user_dictionary_panicking(
                    serde_json::from_value(arg["snapshot"].clone()).unwrap(),
                    arg["after"].as_bool().unwrap(),
                );
                unreachable!()
            }
            #[cfg(test)]
            "panic_observe_shared_learning" => {
                let token = self
                    .segments
                    .as_ref()
                    .unwrap()
                    .focused()
                    .unwrap()
                    .focused()
                    .paths()[0]
                    .token_keys()
                    .find_map(|key| match key {
                        TokenKey::Dictionary(id) => Some(*id),
                        _ => None,
                    })
                    .unwrap();
                self.core.observe_selection_panicking(
                    self.conversion_snapshot
                        .as_ref()
                        .unwrap()
                        .learning
                        .generation,
                    &[token],
                    arg["after"].as_bool().unwrap(),
                );
                unreachable!()
            }
            "commit" | "force_commit" => self.commit(),
            "clear_preceding_context" => {
                self.preceding_commit_context.clear();
                self.current_view(true)
            }
            "cancel" => self.cancel(),
            "convert" => self.convert(),
            "select_candidate" => {
                let Some(id) = arg.get("id").and_then(serde_json::Value::as_str) else {
                    return self.error_view("invalid_candidate_id", true);
                };
                self.select_candidate_id(id)
            }
            "move_segment" => match arg.get("direction").and_then(serde_json::Value::as_str) {
                Some("left") => self.move_segment(-1),
                Some("right") => self.move_segment(1),
                _ => self.error_view("invalid_segment_direction", true),
            },
            "resize_segment" => match arg.get("direction").and_then(serde_json::Value::as_str) {
                Some("shrink") => self.resize_segment(-1),
                Some("extend") => self.resize_segment(1),
                _ => self.error_view("invalid_resize_direction", true),
            },
            "set_mode" => match arg
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_mode)
            {
                Some(mode) => self.set_mode(mode),
                None => self.error_view("invalid_input_mode", true),
            },
            _ => self.current_view(false),
        }
    }

    pub(crate) fn handle_cancel(&mut self) -> ViewState {
        if self.restore_reading_from_literal() {
            return self.current_view(true);
        }
        self.cancel()
    }

    pub(crate) fn restore_reading_from_literal(&mut self) -> bool {
        if self.mode != InputMode::Hiragana
            || self.composition_input_mode != CompositionInputMode::Literal
            || self.segments.is_some()
            || self.composer.is_empty()
        {
            return false;
        }
        self.transition_composition_input_mode(CompositionInputMode::Reading)
    }

    pub(crate) fn toggle_composition_input_mode_from_shift_tap(&mut self) -> bool {
        if self.mode != InputMode::Hiragana || self.segments.is_some() {
            return false;
        }
        let next = match self.composition_input_mode {
            CompositionInputMode::Reading => CompositionInputMode::Literal,
            CompositionInputMode::Literal => CompositionInputMode::Reading,
        };
        self.transition_composition_input_mode(next)
    }

    // Returns whether the mode changed. Only a real change is reported to
    // Swift, so it updates the mode indicator once per transition.
    pub(crate) fn transition_composition_input_mode(&mut self, next: CompositionInputMode) -> bool {
        if self.composition_input_mode == next {
            return false;
        }
        self.composition_input_mode = next;
        self.pending_composition_mode_transition = Some(match next {
            CompositionInputMode::Reading => CompositionModeTransition::Reading,
            CompositionInputMode::Literal => CompositionModeTransition::Literal,
        });
        true
    }

    // Drops the whole composition, converted or not, and the conversion's
    // retained inputs; the record draft is cancelled, not completed.
    pub(crate) fn cancel(&mut self) -> ViewState {
        let consumed = !self.composer.is_empty() || self.segments.is_some();
        self.composer.clear();
        self.segments = None;
        self.conversion_snapshot = None;
        self.initial_selection_nodes.clear();
        self.explicitly_selected_segment_ranges.clear();
        self.transition_composition_input_mode(CompositionInputMode::Reading);
        if let Some(commit_stream) = self.commit_stream.as_mut() {
            commit_stream.cancel_active();
        }
        let mut view = empty_view(consumed);
        view.mode = self.mode;
        view
    }

    pub(crate) fn backspace(&mut self) -> ViewState {
        let composer_was_active = !self.composer.is_empty();
        let conversion_was_active = self.segments.take().is_some();
        self.conversion_snapshot = None;
        self.initial_selection_nodes.clear();
        if conversion_was_active {
            if let Some(commit_stream) = self.commit_stream.as_mut() {
                commit_stream.cancel_active();
            }
        }
        self.explicitly_selected_segment_ranges.clear();
        if !conversion_was_active {
            self.composer.backspace();
            if self.composer.is_empty() {
                self.transition_composition_input_mode(CompositionInputMode::Reading);
            }
        }
        // Delete belongs to the active marked-text authority even when the
        // caret is already at its left edge. Passing it through would let the
        // host delete text immediately before the still-active composition.
        // While converted, the first Delete only returns to the full reading;
        // a subsequent Delete performs the ordinary preedit edit.
        self.current_view(composer_was_active || conversion_was_active)
    }

    // Text input, by mode:
    // - alnum: commit the projected text at once (after any converted
    //   segments), or append it literally to a composition still in progress;
    // - literal span: ASCII graphic text extends it, anything else commits
    //   and passes the key through;
    // - reading: while converted, printable text commits the conversion and
    //   starts a new composition with it (non-ASCII included); otherwise only
    //   ASCII graphic text feeds the romaji composer, and anything else
    //   commits and passes through.
    pub(crate) fn input_text(&mut self, text: &str) -> ViewState {
        if matches!(self.mode, InputMode::FullAlnum | InputMode::HalfAlnum) {
            let projected = project_text(text, self.mode);
            if self.segments.is_some() {
                let mut view = self.commit();
                view.commit_text
                    .get_or_insert_with(String::new)
                    .push_str(&projected);
                return view;
            }
            if !self.composer.is_empty() && self.segments.is_none() {
                self.composer.input_literal(&projected);
                return self.current_view(!text.is_empty());
            }
            let mut view = empty_view(true);
            view.mode = self.mode;
            view.commit_text = Some(projected);
            return view;
        }
        if self.composition_input_mode == CompositionInputMode::Literal {
            if text.is_ascii() && text.chars().all(|character| character.is_ascii_graphic()) {
                self.composer.input_literal(text);
                return self.current_view(!text.is_empty());
            }
            return self.commit_then_passthrough();
        }
        if text.is_empty() {
            return self.current_view(false);
        }

        let is_printable = text
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace());
        if self.segments.is_some() && is_printable {
            let commit_text = self.take_selected_segment_text();
            debug_assert!(commit_text.is_some());
            self.composer.clear();
            for character in text.chars() {
                self.composer.input(character);
            }
            let mut view = self.current_view(true);
            view.commit_text = commit_text;
            return view;
        }

        if !text.is_ascii() || !text.chars().all(|character| character.is_ascii_graphic()) {
            return self.commit_then_passthrough();
        }

        self.segments = None;
        self.conversion_snapshot = None;
        self.initial_selection_nodes.clear();
        self.explicitly_selected_segment_ranges.clear();
        let mut accepted = false;
        for character in text.chars() {
            self.composer.input(character);
            accepted = true;
        }
        self.current_view(accepted)
    }

    /// Space extends a literal span instead of starting conversion, so
    /// multi-word Latin text can be typed without leaving the composition.
    /// Returns `None` when Space keeps its conversion meaning; explicit alnum
    /// modes are left alone because Space there converts the pending reading.
    pub(crate) fn input_literal_space(&mut self) -> Option<ViewState> {
        if self.mode != InputMode::Hiragana
            || self.composition_input_mode != CompositionInputMode::Literal
            || self.composer.is_empty()
        {
            return None;
        }
        self.composer.input_literal(" ");
        Some(self.current_view(true))
    }

    pub(crate) fn begin_literal_input(&mut self, text: &str) -> ViewState {
        // A mode/literal transition commits converted segments, not a cancel.
        // Keep their draft until take_selected_segment_text completes it (docs/src/internals/storage.md「記録」).
        if self.mode != InputMode::Hiragana {
            return self.input_text(text);
        }
        let commit_text = self.take_selected_segment_text();
        if commit_text.is_some() {
            self.composer.clear();
        }
        self.transition_composition_input_mode(CompositionInputMode::Literal);
        self.composer.input_literal(text);
        let mut view = self.current_view(!text.is_empty());
        view.commit_text = commit_text;
        view
    }

    // Switching between Hiragana and half alnum keeps an unconverted
    // composition (the half alnum side continues it as a literal span).
    // Every other switch commits first, so text never changes projection
    // after the user has seen it.
    pub(crate) fn set_mode(&mut self, mode: InputMode) -> ViewState {
        if mode == self.mode {
            if mode == InputMode::Hiragana {
                self.transition_composition_input_mode(CompositionInputMode::Reading);
            }
            return self.current_view(true);
        }
        // A mode/literal transition commits converted segments, not a cancel.
        // Keep their draft until take_selected_segment_text completes it (docs/src/internals/storage.md「記録」).
        let preserves_unconverted_composition = self.segments.is_none()
            && !self.composer.is_empty()
            && matches!(
                (self.mode, mode),
                (InputMode::Hiragana, InputMode::HalfAlnum)
                    | (InputMode::HalfAlnum, InputMode::Hiragana)
            );
        if preserves_unconverted_composition {
            if self.mode == InputMode::Hiragana {
                self.composer.flush();
            }
            self.mode = mode;
            self.transition_composition_input_mode(if mode == InputMode::Hiragana {
                CompositionInputMode::Reading
            } else {
                CompositionInputMode::Literal
            });
            return self.current_view(true);
        }
        let mut view = if self.composer.is_empty() && self.segments.is_none() {
            empty_view(true)
        } else {
            self.commit()
        };
        self.mode = mode;
        self.transition_composition_input_mode(CompositionInputMode::Reading);
        view.mode = mode;
        view
    }
}
