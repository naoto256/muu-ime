//! Builds `ViewState` from session state: mode projection of the preedit,
//! segment and candidate views, and the one-time scalar-to-UTF-16 projection
//! of every offset Swift turns into an `NSRange` (rules 3 and 4 of docs/src/internals/architecture.md「2 つに分けている理由」). Swift
//! never recomputes these, so all NFC and grapheme snapping happens here.
use crate::*;
use muu_conversion::Segments as ConversionSegments;
use muu_romaji::Preedit;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub(crate) fn parse_mode(value: &str) -> Option<InputMode> {
    match value {
        "hiragana" => Some(InputMode::Hiragana),
        "full_katakana" => Some(InputMode::FullKatakana),
        "half_katakana" => Some(InputMode::HalfKatakana),
        "full_alnum" => Some(InputMode::FullAlnum),
        "half_alnum" => Some(InputMode::HalfAlnum),
        _ => None,
    }
}

// The mode projection of romaji output, shared by the preedit and commit.
// Hiragana and half alnum are already in their final form (NFC only).
pub(crate) fn project_text(text: &str, mode: InputMode) -> String {
    match mode {
        InputMode::Hiragana | InputMode::HalfAlnum => text.nfc().collect(),
        InputMode::FullKatakana => hiragana_to_katakana(text),
        InputMode::HalfKatakana => katakana_to_halfwidth(&hiragana_to_katakana(text)),
        InputMode::FullAlnum => text.chars().map(to_fullwidth_ascii).collect(),
    }
}

// U+3041..U+3096 map one-to-one by +0x60 (small ゕゖ to ヵヶ included); the
// iteration marks are mapped separately. Everything else passes unchanged.
pub(crate) fn hiragana_to_katakana(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\u{3041}'..='\u{3096}' => char::from_u32(character as u32 + 0x60).unwrap_or(character),
            'ゝ' => 'ヽ',
            'ゞ' => 'ヾ',
            _ => character,
        })
        .collect()
}

// JIS X 0201 half-width forms. Voiced and semi-voiced kana become two
// characters (base plus ﾞ/ﾟ). Kana with no half-width form use the nearest
// one (ヰ→ｲ, ヱ→ｴ, small ヵヶ→ｶｹ, ヮ→ﾜ) or, like ヽヾ, stay full-width.
pub(crate) fn katakana_to_halfwidth(text: &str) -> String {
    let mut result = String::new();
    for character in text.chars() {
        result.push_str(match character {
            '。' => "｡",
            '「' => "｢",
            '」' => "｣",
            '、' => "､",
            '・' => "･",
            'ヲ' => "ｦ",
            'ァ' => "ｧ",
            'ィ' => "ｨ",
            'ゥ' => "ｩ",
            'ェ' => "ｪ",
            'ォ' => "ｫ",
            'ャ' => "ｬ",
            'ュ' => "ｭ",
            'ョ' => "ｮ",
            'ッ' => "ｯ",
            'ー' => "ｰ",
            'ア' => "ｱ",
            'イ' | 'ヰ' => "ｲ",
            'ウ' => "ｳ",
            'エ' | 'ヱ' => "ｴ",
            'オ' => "ｵ",
            'カ' | 'ヵ' => "ｶ",
            'キ' => "ｷ",
            'ク' => "ｸ",
            'ケ' | 'ヶ' => "ｹ",
            'コ' => "ｺ",
            'サ' => "ｻ",
            'シ' => "ｼ",
            'ス' => "ｽ",
            'セ' => "ｾ",
            'ソ' => "ｿ",
            'タ' => "ﾀ",
            'チ' => "ﾁ",
            'ツ' => "ﾂ",
            'テ' => "ﾃ",
            'ト' => "ﾄ",
            'ナ' => "ﾅ",
            'ニ' => "ﾆ",
            'ヌ' => "ﾇ",
            'ネ' => "ﾈ",
            'ノ' => "ﾉ",
            'ハ' => "ﾊ",
            'ヒ' => "ﾋ",
            'フ' => "ﾌ",
            'ヘ' => "ﾍ",
            'ホ' => "ﾎ",
            'マ' => "ﾏ",
            'ミ' => "ﾐ",
            'ム' => "ﾑ",
            'メ' => "ﾒ",
            'モ' => "ﾓ",
            'ヤ' => "ﾔ",
            'ユ' => "ﾕ",
            'ヨ' => "ﾖ",
            'ラ' => "ﾗ",
            'リ' => "ﾘ",
            'ル' => "ﾙ",
            'レ' => "ﾚ",
            'ロ' => "ﾛ",
            'ワ' | 'ヮ' => "ﾜ",
            'ン' => "ﾝ",
            'ガ' => "ｶﾞ",
            'ギ' => "ｷﾞ",
            'グ' => "ｸﾞ",
            'ゲ' => "ｹﾞ",
            'ゴ' => "ｺﾞ",
            'ザ' => "ｻﾞ",
            'ジ' => "ｼﾞ",
            'ズ' => "ｽﾞ",
            'ゼ' => "ｾﾞ",
            'ゾ' => "ｿﾞ",
            'ダ' => "ﾀﾞ",
            'ヂ' => "ﾁﾞ",
            'ヅ' => "ﾂﾞ",
            'デ' => "ﾃﾞ",
            'ド' => "ﾄﾞ",
            'バ' => "ﾊﾞ",
            'ビ' => "ﾋﾞ",
            'ブ' => "ﾌﾞ",
            'ベ' => "ﾍﾞ",
            'ボ' => "ﾎﾞ",
            'パ' => "ﾊﾟ",
            'ピ' => "ﾋﾟ",
            'プ' => "ﾌﾟ",
            'ペ' => "ﾍﾟ",
            'ポ' => "ﾎﾟ",
            'ヴ' => "ｳﾞ",
            'ヽ' => "ヽ",
            'ヾ' => "ヾ",
            _ => {
                result.push(character);
                continue;
            }
        });
    }
    result
}

pub(crate) fn to_fullwidth_ascii(character: char) -> char {
    match character {
        ' ' => '\u{3000}',
        '!'..='~' => char::from_u32(character as u32 + 0xfee0).unwrap_or(character),
        _ => character,
    }
}

pub fn view_from_preedit(preedit: Preedit, consumed: bool) -> ViewState {
    view_from_preedit_mode(preedit, consumed, InputMode::Hiragana)
}

pub(crate) fn view_from_preedit_mode(
    preedit: Preedit,
    consumed: bool,
    mode: InputMode,
) -> ViewState {
    if preedit.display.is_empty() {
        let mut view = empty_view(consumed);
        view.mode = mode;
        return view;
    }
    // The caret is the projected length of the text before the cursor.
    // Projecting the prefix on its own can only compose less than the whole
    // text, so the caret never exceeds `utf16_len`.
    let source_prefix: String = preedit.display.chars().take(preedit.cursor).collect();
    let text = project_text(&preedit.display, mode);
    let caret_utf16 = project_text(&source_prefix, mode).encode_utf16().count();
    let utf16_len = text.encode_utf16().count();
    ViewState {
        preedit: vec![PreeditRun {
            text,
            utf16_start: 0,
            utf16_len,
            is_focused: true,
            is_converted: false,
            underline_style: UnderlineStyle::Single,
        }],
        caret_utf16,
        selection: None,
        segments: None,
        candidates: None,
        commit_text: None,
        mode,
        composition_mode_transition: None,
        degraded: None,
        poisoned: None,
        consumed,
        error: None,
    }
}

pub fn view_from_segments(segments: &ConversionSegments, consumed: bool) -> ViewState {
    if segments.segments().is_empty() {
        return empty_view(consumed);
    }

    let mut utf16_start = 0;
    let mut caret_utf16 = 0;
    let preedit = segments
        .segments()
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            let text: String = segment.focused().surface().nfc().collect();
            let utf16_len = text.encode_utf16().count();
            let is_focused = index == segments.focused_segment();
            if is_focused {
                caret_utf16 = utf16_start + utf16_len;
            }
            let run = PreeditRun {
                text,
                utf16_start,
                utf16_len,
                is_focused,
                is_converted: true,
                underline_style: if is_focused {
                    UnderlineStyle::Thick
                } else {
                    UnderlineStyle::Single
                },
            };
            utf16_start += utf16_len;
            run
        })
        .collect();

    let segment_views = segments
        .segments()
        .iter()
        .map(|segment| {
            let range = segment.reading_range();
            let start = nfc_utf16_offset(segments.reading(), range.start.get());
            let end = nfc_utf16_offset(segments.reading(), range.end.get());
            SegmentView {
                reading_range: TextRange {
                    start,
                    len: end - start,
                },
                candidate_focus: segment.focused_candidate(),
            }
        })
        .collect();
    let focused = segments.focused().expect("non-empty segments have a focus");
    let candidates = CandidateList {
        items: focused
            .candidates()
            .iter()
            .map(|candidate| CandidateItem {
                id: candidate.id().as_str().to_owned(),
                text: candidate.surface().nfc().collect(),
                annotation: None,
            })
            .collect(),
        selected_index: focused.focused_candidate(),
    };

    ViewState {
        preedit,
        caret_utf16,
        selection: None,
        segments: Some(segment_views),
        candidates: Some(candidates),
        commit_text: None,
        mode: InputMode::Hiragana,
        composition_mode_transition: None,
        degraded: None,
        poisoned: None,
        consumed,
        error: None,
    }
}

pub(crate) fn empty_view(consumed: bool) -> ViewState {
    ViewState {
        preedit: Vec::new(),
        caret_utf16: 0,
        selection: None,
        segments: None,
        candidates: None,
        commit_text: None,
        mode: InputMode::Hiragana,
        composition_mode_transition: None,
        degraded: None,
        poisoned: None,
        consumed,
        error: None,
    }
}

// Maps a scalar offset in the reading to a UTF-16 offset in its NFC form.
// The offset first snaps down to a grapheme boundary, so a range never
// splits a user-perceived character; past the end it maps to the full length.
pub(crate) fn nfc_utf16_offset(text: &str, scalar_offset: usize) -> usize {
    let requested_byte = text
        .char_indices()
        .nth(scalar_offset)
        .map_or(text.len(), |(byte, _)| byte);
    let snapped_byte = text
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .take_while(|byte| *byte <= requested_byte)
        .last()
        .unwrap_or(0);
    text[..snapped_byte]
        .chars()
        .nfc()
        .map(char::len_utf16)
        .sum()
}

#[cfg(test)]
mod tests {

    use muu_conversion::{ScalarIndex, ScalarRange, Segment, SegmentState, Segments};

    use crate::test_support::*;
    use crate::view::*;

    use muu_romaji::Preedit;

    use unicode_normalization::UnicodeNormalization;

    #[test]
    fn five_modes_are_projected_by_rust_and_alnum_bypasses_romaji() {
        let mut session =
            new_session(None, None, TestSessionConfig::default()).expect("embedded table");
        session.handle(command(
            "set_mode",
            serde_json::json!({"mode": "full_katakana"}),
        ));
        session.handle(key("k", 0));
        let katakana = session.handle(key("a", 0));
        assert_eq!(katakana.preedit[0].text, "カ");
        assert_eq!(katakana.mode, InputMode::FullKatakana);

        session.handle(command(
            "set_mode",
            serde_json::json!({"mode": "half_katakana"}),
        ));
        for character in "gya".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        assert_eq!(session.current_view(false).preedit[0].text, "ｷﾞｬ");

        session.handle(command(
            "set_mode",
            serde_json::json!({"mode": "full_alnum"}),
        ));
        let full_alnum = session.handle(key("A1", 0));
        assert_eq!(full_alnum.commit_text.as_deref(), Some("Ａ１"));
        assert_eq!(full_alnum.mode, InputMode::FullAlnum);
        let fullwidth_space = session.handle(key(" ", 49));
        assert_eq!(fullwidth_space.commit_text.as_deref(), Some("　"));
        let fullwidth_punctuation = session.handle(key("!", 0));
        assert_eq!(fullwidth_punctuation.commit_text.as_deref(), Some("！"));

        session.handle(command(
            "set_mode",
            serde_json::json!({"mode": "half_alnum"}),
        ));
        let half_alnum = session.handle(key("A1", 0));
        assert_eq!(half_alnum.commit_text.as_deref(), Some("A1"));
        assert_eq!(half_alnum.mode, InputMode::HalfAlnum);
        let ascii_punctuation = session.handle(key("!", 0));
        assert_eq!(ascii_punctuation.commit_text.as_deref(), Some("!"));
    }

    #[test]
    fn unicode_printable_input_restarts_after_converted_selection() {
        let dictionary = conversion_dictionary();
        let mut session =
            session_with_dictionary_path(Some(dictionary.path())).expect("session data");
        for character in "kyou".chars() {
            session.handle(key(&character.to_string(), 0));
        }
        session.handle(key(" ", 49));
        session.select_candidate_index(1);

        let selected_surface = session.segments.as_ref().unwrap().segments()[0]
            .focused()
            .surface()
            .to_owned();
        let restarted = session.handle(key("é", 0));
        assert!(restarted.consumed);
        assert_eq!(
            restarted.commit_text.as_deref(),
            Some(selected_surface.as_str())
        );
        assert_eq!(
            restarted
                .preedit
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            "é"
        );
        assert!(restarted.segments.is_none());
        assert!(restarted.candidates.is_none());

        let continued = session.handle(key("ka", 0));
        assert!(continued.commit_text.is_none());
        assert_eq!(
            continued
                .preedit
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            "éか"
        );
    }

    #[test]
    fn utf16_projection_and_nfc_are_owned_by_rust() {
        let decomposed = "A👨‍👩‍👧‍👦e\u{301}🇯🇵";
        let normalized: String = decomposed.nfc().collect();
        let preedit = Preedit {
            display: decomposed.to_owned(),
            cursor: decomposed.chars().count(),
            reading_hiragana: decomposed.to_owned(),
            hard_boundaries: Vec::new(),
            pending_latin: String::new(),
        };

        let view = view_from_preedit(preedit, true);
        assert_eq!(view.preedit[0].text, normalized);
        assert_eq!(view.preedit[0].utf16_len, normalized.encode_utf16().count());
        assert_eq!(view.caret_utf16, normalized.encode_utf16().count());
    }

    #[test]
    fn utf16_projection_snaps_scalar_offsets_to_grapheme_boundaries() {
        assert_eq!(nfc_utf16_offset("e\u{301}x", 1), 0);
        assert_eq!(nfc_utf16_offset("A👍🏽B", 2), 1);
        assert_eq!(nfc_utf16_offset("A👍🏽B", 3), 5);
    }

    #[test]
    fn converted_segments_project_to_nfc_utf16_view_state() {
        let first_range = ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(3));
        let second_range = ScalarRange::new(ScalarIndex::new(3), ScalarIndex::new(4));
        let first = Segment::new(
            first_range,
            vec![
                conversion_candidate(first_range, "今日", 10, 1),
                conversion_candidate(first_range, "きょう", 20, 2),
            ],
            1,
            SegmentState::Unconfirmed,
        )
        .unwrap();
        let second = Segment::new(
            second_range,
            vec![conversion_candidate(second_range, "学校", 10, 3)],
            0,
            SegmentState::Unconfirmed,
        )
        .unwrap();
        let mut segments = Segments::new("きょうは".into(), vec![first, second], 0).unwrap();
        segments.move_focus(1).unwrap();

        let view = view_from_segments(&segments, true);
        assert_eq!(
            view.preedit
                .iter()
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            ["きょう", "学校"]
        );
        assert_eq!(view.preedit[0].utf16_start, 0);
        assert_eq!(view.preedit[0].utf16_len, 3);
        assert!(!view.preedit[0].is_focused);
        assert_eq!(view.preedit[1].utf16_start, 3);
        assert_eq!(view.preedit[1].utf16_len, 2);
        assert!(view.preedit[1].is_focused);
        assert_eq!(view.caret_utf16, 5);
        assert_eq!(
            view.segments.as_ref().unwrap(),
            &vec![
                SegmentView {
                    reading_range: TextRange { start: 0, len: 3 },
                    candidate_focus: 1,
                },
                SegmentView {
                    reading_range: TextRange { start: 3, len: 1 },
                    candidate_focus: 0,
                },
            ]
        );
        let candidates = view.candidates.as_ref().unwrap();
        assert_eq!(candidates.selected_index, 0);
        assert_eq!(candidates.items[0].text, "学校");
        assert!(!candidates.items[0].id.is_empty());

        let payload = encode_view(&view).unwrap();
        assert_eq!(&payload[..3], &[FORMAT_TAG_JSON, 1, 0]);
        let json: serde_json::Value = serde_json::from_slice(&payload[3..]).unwrap();
        assert_eq!(json["segments"][0]["reading_range"]["start"], 0);
        assert_eq!(json["segments"][1]["reading_range"]["len"], 1);
        assert_eq!(json["candidates"]["selected_index"], 0);
        assert_eq!(json["candidates"]["items"][0]["text"], "学校");
    }
}
