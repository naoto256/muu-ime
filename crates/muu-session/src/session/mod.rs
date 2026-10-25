//! One input client's state and the request dispatcher (docs/src/internals/architecture.md「Core と Session」). This file holds
//! the `Session` type, its creation and the single commit path; `input`
//! handles keys, commands and composition modes, and `conversion` acquires the
//! conversion snapshot and runs every segment and candidate operation on it.
//! Process-wide data is reached only through `core`.
use crate::commit_stream::CommitStreamState;
use crate::keymap::{Action as KeyAction, Command as KeyCommand, KeymapError};
use crate::preceding_context::PrecedingCommitContext;
use crate::view::*;
use crate::*;
use muu_conversion::{
    CandidateId, ConversionError, Segmenter, Segments as ConversionSegments, TokenKey,
};
use muu_romaji::{RomajiComposer, TableError};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;
use unicode_normalization::UnicodeNormalization;
mod conversion;
mod input;
#[cfg(test)]
pub(crate) use conversion::generate_conversion;
#[cfg(test)]
pub(crate) use input::{
    COMMAND_MODIFIER, FUNCTION_MODIFIER, LEFT_SHIFT_KEYCODE, NUMERIC_PAD_MODIFIER,
    RIGHT_SHIFT_KEYCODE, SHIFT_MODIFIER,
};
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    RomajiTable(TableError),
    Keymap(KeymapError),
    UserDictionary(UserDictionaryError),
    ImportedDictionary(ImportedDictionaryError),
    PostalCode(PostalCodeError),
    PersonalFrequency(PersonalFrequencyError),
    InvalidCommitStreamSchema,
}

impl From<TableError> for SessionError {
    fn from(value: TableError) -> Self {
        Self::RomajiTable(value)
    }
}

impl From<KeymapError> for SessionError {
    fn from(value: KeymapError) -> Self {
        Self::Keymap(value)
    }
}

impl From<UserDictionaryError> for SessionError {
    fn from(value: UserDictionaryError) -> Self {
        Self::UserDictionary(value)
    }
}

impl From<ImportedDictionaryError> for SessionError {
    fn from(value: ImportedDictionaryError) -> Self {
        Self::ImportedDictionary(value)
    }
}

impl From<PostalCodeError> for SessionError {
    fn from(value: PostalCodeError) -> Self {
        Self::PostalCode(value)
    }
}

impl From<PersonalFrequencyError> for SessionError {
    fn from(value: PersonalFrequencyError) -> Self {
        Self::PersonalFrequency(value)
    }
}

/// The in-progress state of one input client (docs/src/internals/architecture.md「Core と Session」).
/// Everything process-wide is reached through `core`; a Session owns no
/// dictionary, vocabulary or learning of its own. It has no internal locking
/// and must be driven from a single thread.
pub struct Session {
    pub(super) composer: RomajiComposer,
    // Inspect real key dispatch in tests without retaining reports in the
    // production Session or expanding its ABI-visible state.
    #[cfg(test)]
    pub(super) last_selection_report: Option<SelectionReport>,
    pub(super) core: Arc<Core>,
    pub(super) segments: Option<ConversionSegments>,
    // Stage-② actual dictionary nodes, independent of recording. Both the
    // segment and node reading ranges must match at commit: a merged word is
    // not evidence that the user rejected a particle (docs/src/internals/conversion.md「学習」).
    // Like `conversion_snapshot`, this belongs to one conversion: every path
    // that starts, cancels or commits a conversion clears it, or a later
    // commit would reject tokens from an unrelated earlier conversion.
    pub(super) initial_selection_nodes:
        std::collections::BTreeMap<(usize, usize), Vec<InitialSelectionNode>>,
    // The inputs of the conversion in progress (docs/src/internals/conversion.md「変換が使うデータを固定する」). Invariant: this is
    // `Some` whenever `segments` is `Some`. Every path that drops `segments`
    // must drop this too, or the session would pin a superseded vocabulary
    // and rank its next conversion with stale data.
    pub(super) conversion_snapshot: Option<ConversionSnapshot>,
    pub(super) mode: InputMode,
    pub(super) composition_input_mode: CompositionInputMode,
    pub(super) preceding_commit_context: PrecedingCommitContext,
    pub(super) explicitly_selected_segment_ranges: BTreeSet<(usize, usize)>,
    pub(super) commit_stream: Option<CommitStreamState>,
    pub(super) shift_tap_keycode: Option<u16>,
    pub(super) pending_composition_mode_transition: Option<CompositionModeTransition>,
}

// Keep only the authority needed for learning, without retaining surfaces or
// full path copies. Node ranges cannot be inferred from a token set after a
// user has merged or split words.
#[derive(Clone, Debug)]
pub(super) struct InitialSelectionNode {
    range: muu_conversion::ScalarRange,
    token: muu_dict::DictionaryTokenId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompositionInputMode {
    Reading,
    Literal,
}

impl Session {
    pub fn new(core: Arc<Core>) -> Result<Self, SessionError> {
        core.create_session()
    }

    pub(crate) fn with_settings(
        core: Arc<Core>,
        commit_stream_config: Option<CommitStreamConfig>,
        session_nonce: String,
    ) -> Result<Self, SessionError> {
        if let Some(config) = &commit_stream_config {
            config
                .validate()
                .map_err(|_| SessionError::InvalidCommitStreamSchema)?;
        }
        // Recording captures converted Segments (docs/src/internals/storage.md「記録」); a dictionary-free
        // kana commit has no conversion facts to record.
        let commit_stream =
            commit_stream_config.map(|config| CommitStreamState::new(config, session_nonce));
        Ok(Self {
            composer: RomajiComposer::with_table(core.romaji_table.clone()),
            #[cfg(test)]
            last_selection_report: None,
            core,
            conversion_snapshot: None,
            segments: None,
            initial_selection_nodes: Default::default(),
            mode: InputMode::Hiragana,
            composition_input_mode: CompositionInputMode::Reading,
            preceding_commit_context: PrecedingCommitContext::default(),
            explicitly_selected_segment_ranges: BTreeSet::new(),
            commit_stream,
            shift_tap_keycode: None,
            pending_composition_mode_transition: None,
        })
    }

    // The single entry for one request. The composition-mode transition is
    // reported only for the request that caused it. Every commit is fed to
    // the preceding context here, whichever path produced it, including a
    // commit whose key is then passed through.
    pub fn handle(&mut self, request: Request) -> ViewState {
        self.pending_composition_mode_transition = None;
        let mut view = match request {
            Request::Key { key } => self.handle_key(key),
            Request::Command { command, arg } => {
                self.shift_tap_keycode = None;
                self.handle_command(&command, &arg)
            }
        };
        view.composition_mode_transition = self.pending_composition_mode_transition.take();
        if let Some(committed) = view.commit_text.as_deref() {
            self.preceding_commit_context.observe(committed);
        }
        view
    }

    pub fn current_view(&self, consumed: bool) -> ViewState {
        let mut view = if let Some(segments) = &self.segments {
            view_from_segments(segments, consumed)
        } else {
            view_from_preedit_mode(self.composer.preedit(), consumed, self.mode)
        };
        view.mode = self.mode;
        if self.core.degraded_kana_only {
            view.degraded = Some(DegradedMode::KanaOnly);
        }
        view
    }

    #[cfg(test)]
    fn take_commit_stream_event(&mut self) -> Option<Vec<u8>> {
        self.core.take_commit_stream_event()
    }

    // Commits the converted segments if any, else the unconverted preedit in
    // the current mode's projection (pending romaji flushed first). An empty
    // composition commits nothing and is not consumed. The returned view is
    // built from `empty_view`, so it carries no `degraded` flag.
    fn commit(&mut self) -> ViewState {
        let commit_text = if let Some(commit_text) = self.take_selected_segment_text() {
            commit_text
        } else {
            if self.composer.is_empty() {
                self.transition_composition_input_mode(CompositionInputMode::Reading);
                return self.current_view(false);
            }
            self.composer.flush();
            project_text(&self.composer.preedit().display, self.mode)
                .nfc()
                .collect()
        };
        self.composer.clear();
        self.transition_composition_input_mode(CompositionInputMode::Reading);
        let mut view = empty_view(true);
        view.mode = self.mode;
        view.commit_text = Some(commit_text);
        view
    }

    // The single commit path for a converted composition. Taking the snapshot
    // first ends the conversion's hold on its inputs whether or not there are
    // segments to commit.
    //
    // Local record capture/serialization failure becomes a dropped record;
    // commit and learning continue. Other Session preparation panics (such as
    // collecting learning token ids) invalidate this Session. Both shared
    // updates publish under the Core's guard, so a publication panic instead
    // invalidates the Core (docs/src/internals/architecture.md「失敗したとき」, docs/src/internals/storage.md「記録」).
    fn take_selected_segment_text(&mut self) -> Option<String> {
        let captured = self.conversion_snapshot.take();
        let initial_nodes = std::mem::take(&mut self.initial_selection_nodes);
        self.segments.take().map(|segments| {
            let candidate_selected_explicitly = !self.explicitly_selected_segment_ranges.is_empty();
            if let Some(commit_stream) = self.commit_stream.as_mut() {
                self.core.publish_commit_stream_events(recording::complete(
                    commit_stream,
                    &segments,
                    &self.explicitly_selected_segment_ranges,
                ));
            }
            if candidate_selected_explicitly {
                if let Some(captured) = captured.as_ref() {
                    let mut token_ids = BTreeSet::new();
                    let mut rejected_ids = BTreeSet::new();
                    for segment in segments.segments() {
                        let range = segment.reading_range();
                        let key = (range.start.get(), range.end.get());
                        if !self.explicitly_selected_segment_ranges.contains(&key) {
                            continue;
                        }
                        // Auxiliary selection is explicit in the UI/record,
                        // but cannot reward or reject dictionary tokens. Its
                        // retained resize context is not a committed path.
                        if segment.focused().auxiliary().is_some() {
                            continue;
                        }
                        let selected = segment.focused().paths()[0]
                            .token_keys()
                            .filter_map(|key| match key {
                                TokenKey::Dictionary(id) => Some(*id),
                                _ => None,
                            })
                            .collect::<BTreeSet<_>>();
                        // Compare actual paths[0], not surface representatives
                        // or the scored latent path (docs/src/internals/conversion.md「学習」). Only an exact node
                        // span replaced by another token proves a rejection.
                        // A merged word such as 今日+は → 教派 must not penalize
                        // the old particle in every other conversion. Unchanged
                        // tokens and resized segment ranges contribute none.
                        if let Some(initial) = initial_nodes.get(&key) {
                            rejected_ids.extend(initial.iter().filter_map(|node| {
                                segment.focused().paths()[0]
                                    .nodes
                                    .iter()
                                    .any(|final_node| {
                                        final_node.start == node.range.start
                                            && final_node.end == node.range.end
                                            && final_node.token_key
                                                != TokenKey::Dictionary(node.token)
                                    })
                                    .then_some(node.token)
                            }));
                        }
                        token_ids.extend(selected);
                    }
                    // Present the generation this conversion captured, not
                    // the current one: if learning was cleared while the
                    // conversion was open, the store must be able to tell and
                    // drop the observation.
                    self.core.observe_choices(
                        captured.learning.generation,
                        &token_ids.into_iter().collect::<Vec<_>>(),
                        &rejected_ids.into_iter().collect::<Vec<_>>(),
                    );
                }
            }
            self.explicitly_selected_segment_ranges.clear();
            segments
                .segments()
                .iter()
                .map(|segment| segment.focused().surface())
                .collect::<String>()
                .nfc()
                .collect()
        })
    }

    // For a key Muu does not handle while composing: commit what is there,
    // then return the key to the client so it applies after the commit.
    fn commit_then_passthrough(&mut self) -> ViewState {
        if self.composer.is_empty() && self.segments.is_none() {
            return self.current_view(false);
        }
        let mut view = self.commit();
        view.consumed = false;
        view
    }

    fn handle_input_source_switch(&mut self) -> ViewState {
        if self.restore_reading_from_literal() {
            return self.current_view(true);
        }
        self.commit_then_passthrough()
    }

    fn error_view(&self, code: &str, consumed: bool) -> ViewState {
        self.error_view_with_params(code, serde_json::Map::new(), consumed)
    }

    fn error_view_with_params(
        &self,
        code: &str,
        params: serde_json::Map<String, serde_json::Value>,
        consumed: bool,
    ) -> ViewState {
        let mut view = self.current_view(consumed);
        view.error = Some(ViewError {
            code: code.to_owned(),
            params,
        });
        view
    }
}

#[cfg(test)]
mod tests;
