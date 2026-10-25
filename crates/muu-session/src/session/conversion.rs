//! Conversion and operations on converted segments: stage ① to ③ for a new
//! conversion, candidate selection, segment moves and resizes. Every
//! operation here reads the retained `ConversionSnapshot`, never the Core's
//! current stores (docs/src/internals/conversion.md「変換が使うデータを固定する」).
use super::*;
use muu_conversion::{convert_to_segments_resilient, move_segment_focus, resize_focused_segment};
/// Stage ① consumes one retained snapshot (docs/src/internals/conversion.md「変換が使うデータを固定する」). It cannot consult current
/// stores; otherwise a replacement could silently mix conversion authorities.
pub(crate) fn generate_conversion(
    reading: &str,
    captured: &ConversionSnapshot,
    segmenter: &Segmenter,
) -> Result<muu_conversion::ConversionOutcome, ConversionError> {
    convert_to_segments_resilient(
        reading,
        segmenter,
        &captured.costs().expect("conversion dictionary"),
    )
}

impl Session {
    pub(crate) fn convert(&mut self) -> ViewState {
        if self.composer.is_empty() {
            return self.current_view(false);
        }
        self.composer.flush();
        // An explicit convert command can restart conversion (docs/src/internals/conversion.md「変換が使うデータを固定する」). Release
        // the old authority before taking the latest version; retaining it
        // would rerun ranking after discarding selection with stale inputs.
        if self.segments.take().is_some() {
            if let Some(stream) = self.commit_stream.as_mut() {
                stream.cancel_active();
            }
        }
        self.conversion_snapshot = None;
        self.initial_selection_nodes.clear();
        self.explicitly_selected_segment_ranges.clear();

        let conversion_started = Instant::now();
        self.transition_composition_input_mode(CompositionInputMode::Reading);
        let reading = self.composer.preedit().display;
        if self.core.dictionary.is_none() {
            return self.current_view(true);
        }
        // The only place a snapshot is acquired. From here until commit or
        // cancel, every operation on this conversion reads `captured` and
        // never the Core's stores, and no store lock is held while searching
        // or scoring below.
        let core = self.core.clone();
        let captured = core.conversion_snapshot();
        self.conversion_snapshot = Some(captured.clone());
        let result = generate_conversion(&reading, &captured, &self.core.segmenter);
        match result {
            Ok(generated) => {
                let snapshot = captured.costs().expect("conversion dictionary");
                // The LM state is read exactly once per conversion, here. If
                // loading completes a moment later, this conversion still
                // reports and behaves as "loading"; the next one sees the
                // model (docs/src/internals/conversion.md「言語モデルの読み込み」).
                //
                // Stage ② receives stage ① directly (docs/src/internals/conversion.md「言語モデルによる選び直し」). Record its actual
                // input, rather than inferring it from a surface representative
                // whose candidate IDs may differ from the generation (docs/src/internals/storage.md「記録」).
                let language_model = self.core.language_model();
                let selection = selection::select(
                    generated.segments(),
                    generated.path_evidence_complete(),
                    &snapshot,
                    &captured.learning.frequency,
                    &self.preceding_commit_context.snapshot().concat(),
                    &language_model,
                    &self.core.segmenter,
                );
                recording::prepare(
                    self.commit_stream.as_mut(),
                    &generated,
                    &selection,
                    &captured,
                    &language_model,
                    recording::RecordingTimes {
                        conversion_started,
                        result_finished: Instant::now(),
                    },
                );
                #[cfg(test)]
                {
                    self.last_selection_report = Some(selection.report.clone());
                }
                self.initial_selection_nodes = selection
                    .segments
                    .segments()
                    .iter()
                    .map(|s| {
                        let r = s.reading_range();
                        (
                            (r.start.get(), r.end.get()),
                            s.focused().paths()[0]
                                .nodes
                                .iter()
                                .filter_map(|node| match node.token_key {
                                    muu_conversion::TokenKey::Dictionary(id) => {
                                        Some(InitialSelectionNode {
                                            range: muu_conversion::ScalarRange {
                                                start: node.start,
                                                end: node.end,
                                            },
                                            token: id,
                                        })
                                    }
                                    _ => None,
                                })
                                .collect(),
                        )
                    })
                    .collect();
                self.segments = Some(crate::candidate_presentation::present_candidates(
                    selection.segments,
                ));
                self.explicitly_selected_segment_ranges.clear();
                self.current_view(true)
            }
            Err(_) => {
                // No conversion remains to retain these captured inputs.
                self.conversion_snapshot = None;
                self.error_view("conversion_failed", true)
            }
        }
    }
    pub(crate) fn cycle_candidate(&mut self, delta: isize) -> ViewState {
        let Some(segments) = self.segments.as_mut() else {
            return self.commit_then_passthrough();
        };
        let Some((count, current, reading_range)) = segments.focused().map(|focused| {
            let range = focused.reading_range();
            (
                focused.candidates().len(),
                focused.focused_candidate(),
                (range.start.get(), range.end.get()),
            )
        }) else {
            return self.current_view(false);
        };
        // Wraps around in both directions. A focused segment always has at
        // least one candidate (`Segment::new` requires it), so `count` is
        // never zero here.
        let next = if delta < 0 {
            current.checked_sub(1).unwrap_or(count - 1)
        } else {
            (current + 1) % count
        };
        if segments.select_focused_candidate(next).is_err() {
            return self.error_view("candidate_selection_failed", true);
        }
        if next != current {
            self.explicitly_selected_segment_ranges
                .insert(reading_range);
        }
        // Recording asks whether the user touched a candidate (docs/src/internals/storage.md「記録」), including
        // reselecting the first one. Learning still requires an actual change.
        if let Some(stream) = self.commit_stream.as_mut() {
            stream.selected_explicitly();
        }
        self.current_view(true)
    }

    pub(crate) fn select_candidate_index(&mut self, index: usize) -> ViewState {
        let Some(segments) = self.segments.as_mut() else {
            return self.current_view(false);
        };
        let current = segments.focused().map(|segment| {
            let range = segment.reading_range();
            (
                segment.focused_candidate(),
                (range.start.get(), range.end.get()),
            )
        });
        // An index past the list (a number key with fewer candidates) is
        // consumed with no change and no error, unlike an unknown id below.
        if segments.select_focused_candidate(index).is_err() {
            return self.current_view(true);
        }
        if let Some((current, range)) = current.filter(|(current, _)| *current != index) {
            debug_assert_ne!(current, index);
            self.explicitly_selected_segment_ranges.insert(range);
        }
        // Recording asks whether the user touched a candidate (docs/src/internals/storage.md「記録」), including
        // reselecting the first one. Learning still requires an actual change.
        if let Some(stream) = self.commit_stream.as_mut() {
            stream.selected_explicitly();
        }
        self.current_view(true)
    }

    pub(crate) fn select_candidate_id(&mut self, id: &str) -> ViewState {
        let Ok(id) = CandidateId::new(id.to_owned()) else {
            return self.error_view("invalid_candidate_id", true);
        };
        let Some(segments) = self.segments.as_mut() else {
            return self.current_view(false);
        };
        let current = segments.focused().map(|segment| {
            let range = segment.reading_range();
            (
                segment.focused().id().as_str().to_owned(),
                (range.start.get(), range.end.get()),
            )
        });
        if segments.select_focused_candidate_by_id(&id).is_err() {
            return self.error_view("candidate_id_not_found", true);
        }
        if let Some((_, range)) = current.filter(|(current_id, _)| current_id != id.as_str()) {
            self.explicitly_selected_segment_ranges.insert(range);
        }
        // Recording asks whether the user touched a candidate (docs/src/internals/storage.md「記録」), including
        // reselecting the first one. Learning still requires an actual change.
        if let Some(stream) = self.commit_stream.as_mut() {
            stream.selected_explicitly();
        }
        self.current_view(true)
    }

    pub(crate) fn move_segment(&mut self, delta: isize) -> ViewState {
        let Some(current) = self.segments.as_ref().cloned() else {
            return self.commit_then_passthrough();
        };
        let focused = current.focused_segment();
        let target = if delta < 0 {
            focused.checked_sub(1)
        } else {
            focused.checked_add(1)
        };
        let Some(target) = target.filter(|target| *target < current.segments().len()) else {
            return self.current_view(true);
        };
        // Moving focus is navigation, not a new conversion (docs/src/internals/conversion.md「文節の移動と伸縮」). Preserve the
        // candidate catalogs, real selected paths, and manual learning marks.
        // Validation makes rejection unexpected, but navigation failure must
        // remain a recoverable view error, not invalidate this Session (docs/src/internals/architecture.md「失敗したとき」).
        match move_segment_focus(&current, target) {
            Ok(segments) => {
                if let Some(stream) = self.commit_stream.as_mut() {
                    stream.moved();
                }
                self.segments = Some(segments);
                self.current_view(true)
            }
            Err(_) => self.error_view("segment_move_failed", true),
        }
    }

    // Left/Right move the caret inside an unconverted preedit, and move the
    // segment focus once converted.
    pub(crate) fn move_preedit_cursor_or_segment(&mut self, delta: isize) -> ViewState {
        if self.segments.is_none() && !self.composer.is_empty() {
            self.composer.move_cursor(delta);
            self.explicitly_selected_segment_ranges.clear();
            return self.current_view(true);
        }
        self.move_segment(delta)
    }

    pub(crate) fn resize_segment(&mut self, delta: isize) -> ViewState {
        let Some(current) = self.segments.as_ref().cloned() else {
            return self.commit_then_passthrough();
        };
        let focused_segment = current.focused_segment();
        let segment_count = current.segments().len();
        let current_length = current
            .focused()
            .map(|segment| segment.reading_range().len());
        let requested = match (current_length, delta) {
            (Some(length), value) if value < 0 => length.checked_sub(1),
            (Some(length), _) => length.checked_add(1),
            (None, _) => None,
        };
        // Shrinking a one-character segment is a no-op. Growing past the end of
        // the reading is left to `resize_focused_segment`, which reports
        // `invalid_segment_boundary`.
        let Some(requested) = requested.filter(|length| *length > 0) else {
            return self.current_view(true);
        };
        let captured = self
            .conversion_snapshot
            .as_ref()
            .expect("converted snapshot")
            .clone();
        let snapshot = captured.costs().expect("conversion dictionary");
        match resize_focused_segment(&current, requested, &snapshot) {
            Ok(segments) => {
                if let Some(stream) = self.commit_stream.as_mut() {
                    let changed = current
                        .segments()
                        .iter()
                        .map(|s| s.reading_range())
                        .collect::<Vec<_>>()
                        != segments
                            .segments()
                            .iter()
                            .map(|s| s.reading_range())
                            .collect::<Vec<_>>();
                    if changed {
                        stream.resized();
                    }
                }
                // An untouched segment keeps a selected auxiliary candidate,
                // but loses its explicit mark here as any selection does. It
                // is then committed and recorded as not explicitly selected.
                self.segments = Some(crate::candidate_presentation::present_candidates(segments));
                self.explicitly_selected_segment_ranges.clear();
                self.current_view(true)
            }
            Err(error) => {
                let code = match error {
                    ConversionError::InvalidSegmentLength => "invalid_segment_boundary",
                    ConversionError::NoConstrainedPath => "segment_resize_no_constrained_path",
                    _ => "segment_resize_failed",
                };
                self.error_view_with_params(
                    code,
                    // The same bytes the former json! object produced: an
                    // absent current length is still null, and the map
                    // serializes its keys sorted either way.
                    serde_json::Map::from_iter([
                        ("requested_length".to_owned(), requested.into()),
                        (
                            "current_length".to_owned(),
                            current_length.map_or(serde_json::Value::Null, Into::into),
                        ),
                        ("focused_segment".to_owned(), focused_segment.into()),
                        ("segment_count".to_owned(), segment_count.into()),
                    ]),
                    true,
                )
            }
        }
    }
}
