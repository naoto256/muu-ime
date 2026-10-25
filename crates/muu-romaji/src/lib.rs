//! Pure Rust romaji-to-hiragana composition.
//!
//! The human-readable table is the source of truth. The trie built from it is
//! an in-memory, reproducible cache and contains no platform-specific code.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use unicode_normalization::{UnicodeNormalization, is_nfc};
use unicode_segmentation::UnicodeSegmentation;

const MUU_ORIGINAL_TABLE: &str = include_str!("../data/muu-original.toml");

// The composition as Session sees it, all NFC. `cursor` and
// `hard_boundaries` count scalars of `display`; unresolved romaji appears in
// `display` at the caret and is repeated in `pending_latin`. Display and
// reading are built from the same units and are equal today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preedit {
    pub display: String,
    pub cursor: usize,
    pub reading_hiragana: String,
    pub hard_boundaries: Vec<usize>,
    pub pending_latin: String,
}

// One resolved piece of the preedit: a table output, ん, or a literal span.
// Literal spans and outputs from `boundary_inputs` mark a hard boundary, which
// conversion must not join across.
#[derive(Debug, Clone)]
struct CompositionUnit {
    display: String,
    reading: String,
    creates_hard_boundary: bool,
    kind: CompositionUnitKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompositionUnitKind {
    Reading,
    Literal,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceTable {
    schema_version: u16,
    name: String,
    #[serde(default)]
    boundary_inputs: BTreeSet<String>,
    nasal: NasalDecisionTable,
    entry: Vec<SourceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceEntry {
    input: String,
    output: String,
    next: String,
}

// How a pending "n" is settled: before a `continue_with` letter it may still
// become a syllable (na, nya…); `explicit_with` letters (n, ') end it as ん
// through the table; before a `commit_before` letter it becomes ん at once.
#[derive(Debug, Clone, Deserialize)]
struct NasalDecisionTable {
    continue_with: BTreeSet<char>,
    explicit_with: BTreeSet<char>,
    commit_before: BTreeSet<char>,
}

#[derive(Debug, Clone)]
struct TableEntry {
    output: String,
    next: String,
    creates_hard_boundary: bool,
}

#[derive(Debug, Default)]
struct TrieNode {
    children: BTreeMap<char, usize>,
    entry: Option<usize>,
}

/// The parsed romaji table. Immutable once built, so one instance is shared
/// by every composer in the process; creating a composer must not parse the
/// embedded TOML again.
#[derive(Debug)]
pub struct RomajiTable {
    nodes: Vec<TrieNode>,
    entries: Vec<TableEntry>,
    nasal: NasalDecisionTable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    InvalidToml(String),
    UnsupportedSchema(u16),
    InvalidName(String),
    EmptyInput,
    NonAsciiInput(String),
    NonAsciiNext(String),
    NonNfcOutput(String),
    NonTerminatingNext { input: String, next: String },
    DuplicateInput(String),
    InvalidNasalRules,
}

impl fmt::Display for TableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for TableError {}

#[derive(Debug)]
pub struct RomajiComposer {
    table: Arc<RomajiTable>,
    units: Vec<CompositionUnit>,
    pending: String,
    // Byte offset in the concatenated, resolved unit display. Pending romaji
    // is rendered at this boundary and the visible caret follows it.
    cursor_byte: usize,
}

impl RomajiComposer {
    pub fn new() -> Result<Self, TableError> {
        Ok(Self::with_table(Arc::new(RomajiTable::default_muu()?)))
    }

    /// Starts an independent composition using an already parsed table.
    pub fn with_table(table: Arc<RomajiTable>) -> Self {
        Self {
            table,
            units: Vec::new(),
            pending: String::new(),
            cursor_byte: 0,
        }
    }

    /// The immutable table used by this composer.
    pub fn table(&self) -> &Arc<RomajiTable> {
        &self.table
    }

    // ASCII capitals are folded so the table only holds lower case; a
    // case-preserving span goes through `input_literal` instead.
    pub fn input(&mut self, value: char) {
        let normalized = if value.is_ascii_uppercase() {
            value.to_ascii_lowercase()
        } else {
            value
        };
        self.pending.push(normalized);
        self.resolve_pending(false);
    }

    /// Appends an exact, case-preserving span without running the romaji table.
    ///
    /// An input-mode switch is an explicit boundary, so any pending romaji is
    /// resolved before the literal span begins.
    pub fn input_literal(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        self.flush();
        let insertion_index = self.split_units_at_byte(self.cursor_byte);
        if let Some(unit) = insertion_index
            .checked_sub(1)
            .and_then(|index| self.units.get_mut(index))
            .filter(|unit| unit.kind == CompositionUnitKind::Literal)
        {
            unit.display.push_str(value);
            unit.reading.push_str(value);
            self.restore_cursor_after_insertion(self.cursor_byte + value.len());
            return;
        }
        self.insert_unit(CompositionUnit {
            display: value.to_owned(),
            reading: value.to_owned(),
            creates_hard_boundary: true,
            kind: CompositionUnitKind::Literal,
        });
    }

    pub fn backspace(&mut self) -> bool {
        if pop_last_grapheme(&mut self.pending) {
            return true;
        }

        let display = self.resolved_display();
        let Some((grapheme_start, _)) = display[..self.cursor_byte]
            .grapheme_indices(true)
            .next_back()
        else {
            return false;
        };
        let first = self.split_units_at_byte(grapheme_start);
        let after = self.split_units_at_byte(self.cursor_byte);
        self.units.drain(first..after);
        self.cursor_byte = grapheme_start;
        true
    }

    /// Moves the preedit caret by one visible grapheme without committing.
    ///
    /// Starting an edit stabilizes pending romaji at the current boundary so
    /// subsequent insertion never depends on text to the right of the caret.
    pub fn move_cursor(&mut self, delta: isize) -> bool {
        self.flush();
        let display = self.resolved_display();
        let target = if delta < 0 {
            display[..self.cursor_byte]
                .grapheme_indices(true)
                .next_back()
                .map(|(start, _)| start)
        } else if delta > 0 {
            display[self.cursor_byte..]
                .graphemes(true)
                .next()
                .map(|grapheme| self.cursor_byte + grapheme.len())
        } else {
            Some(self.cursor_byte)
        };
        let Some(target) = target else {
            return false;
        };
        let moved = target != self.cursor_byte;
        // Keep pending at a concrete unit boundary. The target is already a
        // global grapheme boundary, so splitting here cannot divide a visible
        // character or combining sequence.
        self.split_units_at_byte(target);
        self.cursor_byte = target;
        moved
    }

    pub fn flush(&mut self) {
        self.resolve_pending(true);
    }

    pub fn clear(&mut self) {
        self.units.clear();
        self.pending.clear();
        self.cursor_byte = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty() && self.pending.is_empty()
    }

    pub fn preedit(&self) -> Preedit {
        let mut raw_display = String::new();
        let mut raw_reading = String::new();
        let mut hard_boundary_bytes = Vec::new();
        let mut stable_byte = 0;

        for unit in &self.units {
            if stable_byte == self.cursor_byte {
                raw_display.push_str(&self.pending);
                raw_reading.push_str(&self.pending);
            }
            raw_display.push_str(&unit.display);
            raw_reading.push_str(&unit.reading);
            stable_byte += unit.display.len();
            if unit.creates_hard_boundary {
                hard_boundary_bytes.push(raw_display.len());
            }
        }
        if stable_byte == self.cursor_byte {
            raw_display.push_str(&self.pending);
            raw_reading.push_str(&self.pending);
        }

        let caret_source_byte = self.cursor_byte + self.pending.len();
        let cursor = raw_display[..caret_source_byte]
            .nfc()
            .collect::<String>()
            .chars()
            .count();
        let hard_boundaries = hard_boundary_bytes
            .into_iter()
            .map(|byte| {
                raw_display[..byte]
                    .nfc()
                    .collect::<String>()
                    .chars()
                    .count()
            })
            .collect();
        let display: String = raw_display.nfc().collect();
        let reading_hiragana: String = raw_reading.nfc().collect();
        Preedit {
            cursor,
            display,
            reading_hiragana,
            hard_boundaries,
            pending_latin: self.pending.clone(),
        }
    }

    // Turns as much pending romaji into units as the table allows. Without a
    // boundary it waits whenever more input could still change the result (a
    // lone n, or a prefix of a longer entry). At a boundary (flush, caret move,
    // mode switch) it settles everything: exact entries are taken even if
    // longer ones exist, and anything unmatched becomes literal text one
    // character at a time. Each step consumes input, because `next` is always
    // shorter than its entry's input, so the loop ends.
    fn resolve_pending(&mut self, at_boundary: bool) {
        loop {
            if self.pending.is_empty() {
                return;
            }
            if self.pending == "n" {
                if at_boundary {
                    self.emit_nasal();
                }
                return;
            }
            if self.should_commit_leading_nasal() {
                self.emit_nasal();
                continue;
            }

            let matched = self.table.match_pending(&self.pending);
            if matched.is_complete_prefix {
                if let Some(entry_index) = matched.exact {
                    if !matched.has_extensions || at_boundary {
                        self.emit_entry(entry_index, self.pending.len());
                        continue;
                    }
                }
                if !at_boundary {
                    return;
                }
            } else if let Some((entry_index, byte_len)) = matched.longest {
                self.emit_entry(entry_index, byte_len);
                continue;
            }

            if !at_boundary && matched.is_complete_prefix {
                return;
            }
            self.emit_leading_literal();
        }
    }

    fn should_commit_leading_nasal(&self) -> bool {
        let mut chars = self.pending.chars();
        if chars.next() != Some('n') {
            return false;
        }
        let Some(next) = chars.next() else {
            return false;
        };
        if self.table.nasal.continue_with.contains(&next)
            || self.table.nasal.explicit_with.contains(&next)
        {
            return false;
        }
        self.table.nasal.commit_before.contains(&next)
    }

    fn emit_nasal(&mut self) {
        self.pending.remove(0);
        self.insert_unit(CompositionUnit {
            display: "ん".to_owned(),
            reading: "ん".to_owned(),
            creates_hard_boundary: false,
            kind: CompositionUnitKind::Reading,
        });
    }

    fn emit_entry(&mut self, entry_index: usize, consumed_bytes: usize) {
        let entry = &self.table.entries[entry_index];
        let trailing = self.pending[consumed_bytes..].to_owned();
        let next_pending = format!("{}{}", entry.next, trailing);
        self.insert_unit(CompositionUnit {
            display: entry.output.clone(),
            reading: entry.output.clone(),
            creates_hard_boundary: entry.creates_hard_boundary,
            kind: CompositionUnitKind::Reading,
        });
        self.pending = next_pending;
    }

    fn emit_leading_literal(&mut self) {
        let Some(first) = self.pending.chars().next() else {
            return;
        };
        let byte_len = first.len_utf8();
        self.pending.drain(..byte_len);
        let text = first.to_string();
        self.insert_unit(CompositionUnit {
            display: text.clone(),
            reading: text,
            creates_hard_boundary: true,
            kind: CompositionUnitKind::Literal,
        });
    }

    fn insert_unit(&mut self, unit: CompositionUnit) {
        let display_len = unit.display.len();
        let insertion_index = self.split_units_at_byte(self.cursor_byte);
        self.units.insert(insertion_index, unit);
        self.restore_cursor_after_insertion(self.cursor_byte + display_len);
    }

    fn restore_cursor_after_insertion(&mut self, candidate: usize) {
        let display = self.resolved_display();
        let target = display
            .grapheme_indices(true)
            .map(|(start, _)| start)
            .chain(std::iter::once(display.len()))
            .find(|boundary| *boundary >= candidate)
            .expect("display end is always a grapheme boundary");
        // Inserting a scalar can merge with the right-hand text and erase the
        // old boundary (RI pairs, combining marks, or ZWJ sequences). Keep the
        // caret after the resulting visible grapheme instead of inside it.
        self.split_units_at_byte(target);
        self.cursor_byte = target;
    }

    // Returns the unit index at which `offset` starts, splitting a unit when
    // the offset falls inside it. The hard boundary, if any, stays with the
    // right-hand part, which ends where the original unit ended.
    fn split_units_at_byte(&mut self, offset: usize) -> usize {
        debug_assert!(offset <= self.resolved_display().len());
        let mut unit_start = 0;
        for index in 0..self.units.len() {
            let unit_end = unit_start + self.units[index].display.len();
            if offset == unit_start {
                return index;
            }
            if offset == unit_end {
                return index + 1;
            }
            if offset < unit_end {
                let local_offset = offset - unit_start;
                debug_assert!(self.units[index].display.is_char_boundary(local_offset));
                debug_assert_eq!(self.units[index].display, self.units[index].reading);
                let trailing_display = self.units[index].display.split_off(local_offset);
                let trailing_reading = self.units[index].reading.split_off(local_offset);
                let creates_hard_boundary = self.units[index].creates_hard_boundary;
                let kind = self.units[index].kind;
                self.units[index].creates_hard_boundary = false;
                self.units.insert(
                    index + 1,
                    CompositionUnit {
                        display: trailing_display,
                        reading: trailing_reading,
                        creates_hard_boundary,
                        kind,
                    },
                );
                return index + 1;
            }
            unit_start = unit_end;
        }
        self.units.len()
    }

    fn resolved_display(&self) -> String {
        self.units
            .iter()
            .map(|unit| unit.display.as_str())
            .collect()
    }
}

fn pop_last_grapheme(text: &mut String) -> bool {
    let Some((start, _)) = text.grapheme_indices(true).next_back() else {
        return false;
    };
    text.truncate(start);
    true
}

impl RomajiTable {
    /// Parses and validates the embedded Muu table.
    pub fn default_muu() -> Result<Self, TableError> {
        Self::from_source(MUU_ORIGINAL_TABLE)
    }

    fn from_source(source: &str) -> Result<Self, TableError> {
        let parsed: SourceTable =
            toml::from_str(source).map_err(|error| TableError::InvalidToml(error.to_string()))?;
        if parsed.schema_version != 1 {
            return Err(TableError::UnsupportedSchema(parsed.schema_version));
        }
        if parsed.name != "muu-original" {
            return Err(TableError::InvalidName(parsed.name));
        }
        if parsed.nasal.continue_with.is_empty()
            || parsed.nasal.explicit_with.is_empty()
            || parsed.nasal.commit_before.is_empty()
            || !parsed.nasal.explicit_with.contains(&'n')
            || !parsed.nasal.explicit_with.contains(&'\'')
        {
            return Err(TableError::InvalidNasalRules);
        }

        let mut table = Self {
            nodes: vec![TrieNode::default()],
            entries: Vec::with_capacity(parsed.entry.len()),
            nasal: parsed.nasal,
        };
        for source_entry in parsed.entry {
            table.insert(source_entry, &parsed.boundary_inputs)?;
        }
        Ok(table)
    }

    // Rules every entry must meet: non-empty ASCII input, ASCII `next`, NFC
    // output, a `next` strictly shorter than the input (so composition always
    // terminates), and no duplicate input.
    fn insert(
        &mut self,
        source: SourceEntry,
        boundary_inputs: &BTreeSet<String>,
    ) -> Result<(), TableError> {
        if source.input.is_empty() {
            return Err(TableError::EmptyInput);
        }
        if !source.input.is_ascii() {
            return Err(TableError::NonAsciiInput(source.input));
        }
        if !source.next.is_ascii() {
            return Err(TableError::NonAsciiNext(source.next));
        }
        if !is_nfc(&source.output) {
            return Err(TableError::NonNfcOutput(source.output));
        }
        if !source.next.is_empty() && source.next.len() >= source.input.len() {
            return Err(TableError::NonTerminatingNext {
                input: source.input,
                next: source.next,
            });
        }

        let entry_index = self.entries.len();
        let mut node_index = 0;
        for character in source.input.chars() {
            let child = if let Some(child) = self.nodes[node_index].children.get(&character) {
                *child
            } else {
                let child = self.nodes.len();
                self.nodes.push(TrieNode::default());
                self.nodes[node_index].children.insert(character, child);
                child
            };
            node_index = child;
        }
        if self.nodes[node_index].entry.is_some() {
            return Err(TableError::DuplicateInput(source.input));
        }
        self.nodes[node_index].entry = Some(entry_index);
        self.entries.push(TableEntry {
            creates_hard_boundary: boundary_inputs.contains(&source.input),
            output: source.output,
            next: source.next,
        });
        Ok(())
    }

    // `exact`: an entry for all of `pending`. `longest`: the longest entry that
    // is a prefix of it. `is_complete_prefix`: `pending` is a path in the trie.
    // `has_extensions`: some longer entry continues it.
    fn match_pending(&self, pending: &str) -> PendingMatch {
        let mut node_index = 0;
        let mut longest = None;
        let mut consumed_bytes = 0;

        for character in pending.chars() {
            let Some(child) = self.nodes[node_index].children.get(&character) else {
                return PendingMatch {
                    exact: None,
                    longest,
                    is_complete_prefix: false,
                    has_extensions: false,
                };
            };
            node_index = *child;
            consumed_bytes += character.len_utf8();
            if let Some(entry) = self.nodes[node_index].entry {
                longest = Some((entry, consumed_bytes));
            }
        }

        PendingMatch {
            exact: self.nodes[node_index].entry,
            longest,
            is_complete_prefix: true,
            has_extensions: !self.nodes[node_index].children.is_empty(),
        }
    }
}

#[derive(Debug)]
struct PendingMatch {
    exact: Option<usize>,
    longest: Option<(usize, usize)>,
    is_complete_prefix: bool,
    has_extensions: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMON_COMPATIBILITY_TABLE: &str = include_str!("../data/common-compatibility.tsv");
    const MOZC_COMMON_SOUND_PARTITION: &str =
        include_str!("../data/mozc-common-sound-partition.tsv");
    const REVIEWED_COMPATIBILITY_ABSENCES: &str =
        include_str!("../data/common-compatibility-reviewed-absences.tsv");

    #[derive(Debug)]
    struct MozcPartitionRow {
        source_line: usize,
        input: &'static str,
        output: &'static str,
        next: &'static str,
        disposition: &'static str,
        reason: &'static str,
    }

    fn common_compatibility_entries() -> Vec<(&'static str, &'static str, &'static str)> {
        COMMON_COMPATIBILITY_TABLE
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split('\t');
                let input = fields.next().expect("compatibility input");
                let output = fields.next().expect("compatibility output");
                let next = fields.next().unwrap_or("");
                assert!(
                    fields.next().is_none(),
                    "unexpected compatibility field: {line}"
                );
                (input, output, next)
            })
            .collect()
    }

    fn mozc_common_sound_partition() -> Vec<MozcPartitionRow> {
        MOZC_COMMON_SOUND_PARTITION
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split('\t');
                let source_line = fields
                    .next()
                    .expect("partition source line")
                    .parse()
                    .expect("numeric partition source line");
                let input = fields.next().expect("partition input");
                let output = fields.next().expect("partition output");
                let next = match fields.next().expect("partition next") {
                    "-" => "",
                    value => value,
                };
                let disposition = fields.next().expect("partition disposition");
                let reason = fields.next().expect("partition reason");
                assert!(
                    fields.next().is_none(),
                    "unexpected partition field: {line}"
                );
                MozcPartitionRow {
                    source_line,
                    input,
                    output,
                    next,
                    disposition,
                    reason,
                }
            })
            .collect()
    }

    fn reviewed_compatibility_absences() -> Vec<(&'static str, &'static str)> {
        REVIEWED_COMPATIBILITY_ABSENCES
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split('\t');
                let input = fields.next().expect("reviewed absence input");
                assert_eq!(fields.next(), Some("excluded"), "{input} decision");
                let reason = fields.next().expect("reviewed absence reason");
                assert!(
                    fields.next().is_none(),
                    "unexpected reviewed absence field: {line}"
                );
                (input, reason)
            })
            .collect()
    }

    fn compose(input: &str) -> RomajiComposer {
        let mut composer = RomajiComposer::new().expect("embedded table is valid");
        for character in input.chars() {
            composer.input(character);
        }
        composer
    }

    #[test]
    fn shared_table_keeps_composition_state_independent() {
        let table = Arc::new(RomajiTable::default_muu().expect("embedded table"));
        let mut first = RomajiComposer::with_table(table.clone());
        let mut second = RomajiComposer::with_table(table.clone());
        first.input('a');
        second.input('i');
        assert!(Arc::ptr_eq(first.table(), second.table()));
        assert!(Arc::ptr_eq(first.table(), &table));
        assert_eq!(first.preedit().display, "あ");
        assert_eq!(second.preedit().display, "い");
    }

    #[test]
    fn composition_acceptance_sequences_are_golden() {
        let mut final_n = compose("n");
        final_n.flush();

        let snapshots = [
            ("konnnichiha", compose("konnnichiha").preedit()),
            ("kya", compose("kya").preedit()),
            ("kka", compose("kka").preedit()),
            ("nk", compose("nk").preedit()),
            ("final_n", final_n.preedit()),
        ];

        insta::assert_yaml_snapshot!(snapshots, @r###"
        - - konnnichiha
          - display: こんにちは
            cursor: 5
            reading_hiragana: こんにちは
            hard_boundaries: []
            pending_latin: ""
        - - kya
          - display: きゃ
            cursor: 2
            reading_hiragana: きゃ
            hard_boundaries: []
            pending_latin: ""
        - - kka
          - display: っか
            cursor: 2
            reading_hiragana: っか
            hard_boundaries: []
            pending_latin: ""
        - - nk
          - display: んk
            cursor: 2
            reading_hiragana: んk
            hard_boundaries: []
            pending_latin: k
        - - final_n
          - display: ん
            cursor: 1
            reading_hiragana: ん
            hard_boundaries: []
            pending_latin: ""
        "###);
    }

    #[test]
    fn foreign_f_sounds_preserve_prefix_and_boundary_contracts() {
        for (input, expected) in [
            ("fa", "ふぁ"),
            ("fi", "ふぃ"),
            ("fe", "ふぇ"),
            ("fo", "ふぉ"),
            ("fyu", "ふゅ"),
        ] {
            let preedit = compose(input).preedit();
            assert_eq!(preedit.display, expected, "{input}");
            assert_eq!(preedit.reading_hiragana, expected, "{input}");
            assert!(preedit.pending_latin.is_empty(), "{input}");
        }

        let pending = compose("f").preedit();
        assert_eq!(pending.display, "f");
        assert_eq!(pending.pending_latin, "f");

        let doubled_prefix = compose("ff").preedit();
        assert_eq!(doubled_prefix.display, "っf");
        assert_eq!(doubled_prefix.reading_hiragana, "っf");
        assert_eq!(doubled_prefix.pending_latin, "f");

        let resumed_prefix = compose("ffa").preedit();
        assert_eq!(resumed_prefix.display, "っふぁ");
        assert_eq!(resumed_prefix.reading_hiragana, "っふぁ");
        assert!(resumed_prefix.pending_latin.is_empty());

        let mut invalid_followup = compose("fk");
        assert_eq!(invalid_followup.preedit().display, "fk");
        assert_eq!(invalid_followup.preedit().pending_latin, "k");
        invalid_followup.flush();
        assert_eq!(invalid_followup.preedit().display, "fk");
        assert!(invalid_followup.preedit().pending_latin.is_empty());

        assert_eq!(compose("fu").preedit().display, "ふ");
        assert_eq!(compose("vu").preedit().display, "ゔ");
    }

    #[test]
    fn explicit_literal_spans_preserve_case_and_composition_boundaries() {
        let mut composer = RomajiComposer::new().expect("embedded table");
        for character in "koreha".chars() {
            composer.input(character);
        }
        composer.input_literal("Muu");
        for character in "dashine".chars() {
            composer.input(character);
        }

        let preedit = composer.preedit();
        assert_eq!(preedit.display, "これはMuuだしね");
        assert_eq!(preedit.reading_hiragana, "これはMuuだしね");
        assert_eq!(preedit.hard_boundaries, vec![6]);
        assert!(preedit.pending_latin.is_empty());

        let mut pending = RomajiComposer::new().expect("embedded table");
        for character in "hon".chars() {
            pending.input(character);
        }
        pending.input_literal("Muu");
        assert_eq!(pending.preedit().display, "ほんMuu");
        assert!(pending.backspace());
        assert_eq!(pending.preedit().display, "ほんMu");
    }

    #[test]
    fn pinned_mozc_common_sound_rows_form_an_exact_partition() {
        const UPSTREAM_COMMIT: &str = "3f235b4eb6fcff7d14ef5f0fb8ee56de7ee4c732";
        const UPSTREAM_SHA256: &str =
            "fcb900e311d32ced05499c86e236d1084fb0c0937398deb6311fc6c7287a7821";

        assert!(MOZC_COMMON_SOUND_PARTITION.contains(UPSTREAM_COMMIT));
        assert!(MOZC_COMMON_SOUND_PARTITION.contains(UPSTREAM_SHA256));
        assert!(
            MOZC_COMMON_SOUND_PARTITION
                .contains("license_text=THIRD_PARTY_LICENSES/dictionaries/mozc/LICENSE.txt")
        );

        let partition = mozc_common_sound_partition();
        let expected_lines = (16..=165)
            .chain(std::iter::once(190))
            .chain(215..=239)
            .chain(245..=254)
            .chain(315..=322)
            .collect::<BTreeSet<_>>();
        let actual_lines = partition
            .iter()
            .map(|row| row.source_line)
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_lines, expected_lines, "unclassified upstream row");
        assert_eq!(partition.len(), 194, "partition row count");
        assert!(
            partition
                .windows(2)
                .all(|rows| rows[0].source_line < rows[1].source_line),
            "upstream row order must be stable"
        );

        let source: SourceTable = toml::from_str(MUU_ORIGINAL_TABLE).expect("embedded source");
        let compatibility = common_compatibility_entries()
            .into_iter()
            .map(|(input, output, next)| (input, (output, next)))
            .collect::<BTreeMap<_, _>>();
        let mut seen_inputs = BTreeSet::new();
        let mut counts = BTreeMap::new();

        for row in &partition {
            assert!(seen_inputs.insert(row.input), "duplicate partition input");
            *counts.entry(row.disposition).or_insert(0usize) += 1;
            let current = source
                .entry
                .iter()
                .filter(|entry| entry.input == row.input)
                .collect::<Vec<_>>();

            match row.disposition {
                "existing" => {
                    assert_eq!(row.reason, "base_exact");
                    assert_eq!(current.len(), 1, "{} current count", row.input);
                    assert_eq!(current[0].output, row.output, "{} output", row.input);
                    assert_eq!(current[0].next, row.next, "{} next", row.input);
                    assert!(!compatibility.contains_key(row.input));
                }
                "adopted" => {
                    assert_eq!(row.reason, "common_sound_added");
                    assert_eq!(current.len(), 1, "{} current count", row.input);
                    assert_eq!(current[0].output, row.output, "{} output", row.input);
                    assert_eq!(current[0].next, row.next, "{} next", row.input);
                    assert_eq!(
                        compatibility.get(row.input),
                        Some(&(row.output, row.next)),
                        "{} adoption oracle",
                        row.input
                    );
                }
                "excluded" => {
                    let expected_reason = match row.input {
                        "ll" | "xx" => "small_kana_gemination_alias",
                        "www" => "recursive_vendor_shortcut",
                        "t'i" | "t'yu" | "d'i" | "d'yu" | "t'u" | "d'u" => {
                            "apostrophe_vendor_alias"
                        }
                        input => panic!("unclassified exclusion: {input}"),
                    };
                    assert_eq!(row.reason, expected_reason, "{} reason", row.input);
                    assert!(current.is_empty(), "{} must stay excluded", row.input);
                    assert!(!compatibility.contains_key(row.input));
                }
                disposition => panic!("unclassified disposition: {disposition}"),
            }
        }

        assert_eq!(counts.get("existing"), Some(&73));
        assert_eq!(counts.get("adopted"), Some(&112));
        assert_eq!(counts.get("excluded"), Some(&9));
        assert_eq!(counts.len(), 3, "unclassified disposition");
        assert_eq!(compatibility.len(), 112, "every adoption is classified");
    }

    #[test]
    fn reviewed_q_f_wh_sh_ch_j_matrix_has_no_silent_omissions() {
        let partition_inputs = mozc_common_sound_partition()
            .into_iter()
            .map(|row| row.input)
            .collect::<BTreeSet<_>>();
        let absences = reviewed_compatibility_absences();
        let absence_inputs = absences
            .iter()
            .map(|(input, _)| *input)
            .collect::<BTreeSet<_>>();
        assert_eq!(absence_inputs.len(), absences.len(), "duplicate absence");
        assert!(partition_inputs.is_disjoint(&absence_inputs));

        let suffixes = ["a", "i", "u", "e", "o", "ya", "yi", "yu", "ye", "yo"];
        let mut reviewed = BTreeSet::new();
        for stem in ["q", "f", "wh", "sh", "ch", "j"] {
            for suffix in suffixes {
                let input = format!("{stem}{suffix}");
                let present = partition_inputs.contains(input.as_str());
                let absent = absence_inputs.contains(input.as_str());
                assert_ne!(present, absent, "{input} must be present xor excluded");
                reviewed.insert(input);
            }
        }
        assert_eq!(reviewed.len(), 60);
        assert_eq!(absence_inputs.len(), 22);
        for required in ["qya", "qyu", "qyo", "fyi", "fye"] {
            assert!(absence_inputs.contains(required), "{required} decision");
        }
        for (input, reason) in absences {
            assert_eq!(reason, "absent_from_pinned_mozc_and_repo_contract");
            assert!(reviewed.contains(input), "unscoped absence: {input}");
            assert!(
                !MUU_ORIGINAL_TABLE.contains(&format!("input = \"{input}\"")),
                "reviewed absence must not become behavior silently: {input}"
            );
        }
    }

    #[test]
    fn compatibility_prefix_graph_and_table_order_are_conflict_free() {
        let source: SourceTable = toml::from_str(MUU_ORIGINAL_TABLE).expect("embedded source");
        let table = RomajiTable::from_source(MUU_ORIGINAL_TABLE).expect("embedded table");
        let supported = mozc_common_sound_partition()
            .into_iter()
            .filter(|row| row.disposition != "excluded")
            .collect::<Vec<_>>();
        let mut prefixes = BTreeSet::new();

        for row in &supported {
            let exact = table.match_pending(row.input);
            assert!(exact.exact.is_some(), "{} exact rule", row.input);
            assert_eq!(
                compose(row.input).preedit().display,
                format!("{}{}", row.output, row.next),
                "{} exact composition",
                row.input
            );
            for (byte, _) in row.input.char_indices().skip(1) {
                let prefix = &row.input[..byte];
                prefixes.insert(prefix.to_owned());
                let matched = table.match_pending(prefix);
                assert!(matched.is_complete_prefix, "{prefix} trie path");
                assert!(matched.has_extensions, "{prefix} trie extension");
                let preedit = compose(prefix).preedit();
                assert_eq!(preedit.pending_latin, prefix, "{prefix} pending");
            }
        }
        for required in ["j", "jy", "q", "f", "fy", "wh", "sh", "ch"] {
            assert!(
                prefixes.contains(required),
                "missing prefix edge: {required}"
            );
        }

        let positions = source
            .entry
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.input.as_str(), index))
            .collect::<BTreeMap<_, _>>();
        let adopted_positions = common_compatibility_entries()
            .into_iter()
            .map(|(input, _, _)| positions[input])
            .collect::<Vec<_>>();
        assert!(
            adopted_positions.windows(2).all(|pair| pair[0] < pair[1]),
            "compatibility TOML order drift"
        );

        let duplicate = format!(
            "{MUU_ORIGINAL_TABLE}\n[[entry]]\ninput = \"je\"\noutput = \"じぇ\"\nnext = \"\"\n"
        );
        assert!(matches!(
            RomajiTable::from_source(&duplicate),
            Err(TableError::DuplicateInput(input)) if input == "je"
        ));
        let conflict = format!(
            "{MUU_ORIGINAL_TABLE}\n[[entry]]\ninput = \"je\"\noutput = \"じえ\"\nnext = \"\"\n"
        );
        assert!(matches!(
            RomajiTable::from_source(&conflict),
            Err(TableError::DuplicateInput(input)) if input == "je"
        ));
    }

    #[test]
    fn common_extended_sound_manifest_is_exact_and_fully_consumed() {
        let source: SourceTable = toml::from_str(MUU_ORIGINAL_TABLE).expect("embedded source");
        let compatibility = common_compatibility_entries();
        let unique_inputs = compatibility
            .iter()
            .map(|(input, _, _)| *input)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            unique_inputs.len(),
            compatibility.len(),
            "oracle duplicates"
        );

        for (input, expected_output, expected_next) in compatibility {
            let matches = source
                .entry
                .iter()
                .filter(|entry| entry.input == input)
                .collect::<Vec<_>>();
            assert_eq!(matches.len(), 1, "{input} must occur exactly once");
            assert_eq!(matches[0].output, expected_output, "{input} output");
            assert_eq!(matches[0].next, expected_next, "{input} next");

            let preedit = compose(input).preedit();
            let expected_display = format!("{expected_output}{expected_next}");
            assert_eq!(preedit.display, expected_display, "{input} display");
            assert_eq!(
                preedit.reading_hiragana, expected_display,
                "{input} reading"
            );
            assert_eq!(preedit.pending_latin, expected_next, "{input} pending");
        }

        assert_eq!(compose("je").preedit().display, "じぇ");
        assert_ne!(compose("je").preedit().display, "jえ");
    }

    #[test]
    fn extended_sound_prefixes_use_longest_match_and_safe_fallback() {
        for (input, pending) in [
            ("j", "j"),
            ("vy", "vy"),
            ("ts", "ts"),
            ("dh", "dh"),
            ("kw", "kw"),
            ("wh", "wh"),
        ] {
            let preedit = compose(input).preedit();
            assert_eq!(preedit.display, input, "{input} display");
            assert_eq!(preedit.pending_latin, pending, "{input} pending");
        }

        for (input, expected) in [
            ("je", "じぇ"),
            ("jye", "じぇ"),
            ("she", "しぇ"),
            ("che", "ちぇ"),
            ("qqa", "っくぁ"),
            ("tcha", "っちゃ"),
        ] {
            let preedit = compose(input).preedit();
            assert_eq!(preedit.display, expected, "{input}");
            assert!(preedit.pending_latin.is_empty(), "{input}");
        }

        let mut invalid = compose("jq");
        assert_eq!(invalid.preedit().display, "jq");
        assert_eq!(invalid.preedit().pending_latin, "q");
        invalid.flush();
        assert_eq!(invalid.preedit().display, "jq");
        assert!(invalid.preedit().pending_latin.is_empty());
    }

    #[test]
    fn extended_sound_backspace_uses_visible_graphemes() {
        for (input, initial, after_backspace) in [
            ("je", "じぇ", "じ"),
            ("va", "ゔぁ", "ゔ"),
            ("tsa", "つぁ", "つ"),
            ("thi", "てぃ", "て"),
            ("kwa", "くぁ", "く"),
        ] {
            let mut composer = compose(input);
            assert_eq!(composer.preedit().display, initial, "{input}");
            assert!(composer.backspace(), "{input}");
            assert_eq!(composer.preedit().display, after_backspace, "{input}");
        }
    }

    #[test]
    fn backspace_deletes_the_last_visible_grapheme() {
        let mut single = compose("ka");
        assert_eq!(single.preedit().display, "か");
        assert!(single.backspace());
        assert!(single.is_empty());

        let mut composer = compose("kka");
        assert_eq!(composer.preedit().display, "っか");

        assert!(composer.backspace());
        assert_eq!(composer.preedit().display, "っ");

        assert!(composer.backspace());
        assert!(composer.is_empty());
        assert!(!composer.backspace());

        let mut pending = compose("kk");
        assert_eq!(pending.preedit().display, "っk");
        assert!(pending.backspace());
        assert_eq!(pending.preedit().display, "っ");

        let mut foreign_sound = compose("fa");
        assert_eq!(foreign_sound.preedit().display, "ふぁ");
        assert!(foreign_sound.backspace());
        assert_eq!(foreign_sound.preedit().display, "ふ");
    }

    #[test]
    fn grapheme_deletion_never_splits_a_combining_sequence() {
        let mut text = "きか\u{3099}".to_owned();
        assert!(pop_last_grapheme(&mut text));
        assert_eq!(text, "き");
        assert!(pop_last_grapheme(&mut text));
        assert!(text.is_empty());
    }

    #[test]
    fn backspace_deletes_graphemes_that_span_multiple_units() {
        for displayed_grapheme in ["か\u{3099}", "👩🏽‍💻", "🇯🇵"] {
            let mut composer = compose(displayed_grapheme);
            assert_eq!(
                composer.preedit().display,
                displayed_grapheme.nfc().collect::<String>()
            );
            assert!(composer.backspace());
            assert!(composer.is_empty());
        }
    }

    #[test]
    fn contracted_and_small_kana_delete_by_visible_grapheme() {
        let mut contracted = compose("kya");
        assert!(contracted.backspace());
        assert_eq!(contracted.preedit().display, "き");
        assert!(contracted.backspace());
        assert!(contracted.is_empty());

        let mut small = compose("xya");
        assert!(small.backspace());
        assert!(small.is_empty());
    }

    #[test]
    fn preedit_cursor_inserts_and_deletes_at_visible_grapheme_boundaries() {
        let mut composer = compose("kakiku");
        assert_eq!(composer.preedit().display, "かきく");
        assert_eq!(composer.preedit().cursor, 3);

        assert!(composer.move_cursor(-1));
        assert!(composer.move_cursor(-1));
        assert_eq!(composer.preedit().cursor, 1);

        composer.input('s');
        assert_eq!(composer.preedit().display, "かsきく");
        assert_eq!(composer.preedit().cursor, 2);
        composer.input('a');
        assert_eq!(composer.preedit().display, "かさきく");
        assert_eq!(composer.preedit().cursor, 2);

        assert!(composer.backspace());
        assert_eq!(composer.preedit().display, "かきく");
        assert_eq!(composer.preedit().cursor, 1);
        assert!(composer.move_cursor(-1));
        assert!(!composer.move_cursor(-1));
        assert_eq!(composer.preedit().cursor, 0);
        assert!(composer.move_cursor(1));
        assert_eq!(composer.preedit().cursor, 1);
    }

    #[test]
    fn cursor_movement_stabilizes_pending_romaji_before_editing() {
        let mut composer = compose("k");
        assert_eq!(composer.preedit().pending_latin, "k");

        assert!(composer.move_cursor(-1));
        let stabilized = composer.preedit();
        assert_eq!(stabilized.display, "k");
        assert_eq!(stabilized.pending_latin, "");
        assert_eq!(stabilized.cursor, 0);

        composer.input('a');
        let inserted = composer.preedit();
        assert_eq!(inserted.display, "あk");
        assert_eq!(inserted.reading_hiragana, "あk");
        assert_eq!(inserted.cursor, 1);
    }

    #[test]
    fn literal_spans_can_be_edited_at_the_visible_caret() {
        let mut composer = RomajiComposer::new().expect("embedded table");
        composer.input_literal("Muu");
        assert_eq!(composer.preedit().display, "Muu");
        assert_eq!(composer.preedit().cursor, 3);

        assert!(composer.move_cursor(-1));
        assert!(composer.move_cursor(-1));
        composer.input_literal("X");
        assert_eq!(composer.preedit().display, "MXuu");
        assert_eq!(composer.preedit().cursor, 2);

        assert!(composer.backspace());
        assert_eq!(composer.preedit().display, "Muu");
        assert_eq!(composer.preedit().cursor, 1);
    }

    #[test]
    fn insertion_reestablishes_a_legal_boundary_after_reclustering() {
        for (right, inserted, expected, cursor) in [
            ("🇯🇵", "🇺", "🇺🇯🇵", 2),
            ("\u{301}", "e", "é", 1),
            ("\u{200d}💻", "👩", "👩\u{200d}💻", 3),
        ] {
            let mut composer = RomajiComposer::new().expect("embedded table");
            composer.input_literal(right);
            while composer.move_cursor(-1) {}
            composer.input_literal(inserted);

            let preedit = composer.preedit();
            assert_eq!(preedit.display, expected);
            assert_eq!(preedit.cursor, cursor);
            assert!(composer.move_cursor(-1));
            assert_eq!(composer.preedit().cursor, 0);
            assert!(!composer.move_cursor(-1));
        }

        // Exercise the adjacent-literal merge path as well as insert_unit:
        // the newly appended RI pairs with the formerly separate right RI.
        let mut merged = RomajiComposer::new().expect("embedded table");
        merged.input_literal("X");
        merged.input('🇯');
        assert!(merged.move_cursor(-1));
        merged.input_literal("🇺");
        assert_eq!(merged.preedit().display, "X🇺🇯");
        assert_eq!(merged.preedit().cursor, 3);
        assert!(merged.move_cursor(-1));
        assert_eq!(merged.preedit().cursor, 1);
    }

    #[test]
    fn pending_romaji_is_rendered_inside_a_multi_grapheme_unit() {
        let mut composer = compose("kya");
        assert_eq!(composer.preedit().display, "きゃ");
        assert!(composer.move_cursor(-1));

        composer.input('s');
        assert_eq!(composer.preedit().display, "きsゃ");
        assert_eq!(composer.preedit().cursor, 2);
        composer.input('a');
        assert_eq!(composer.preedit().display, "きさゃ");
        assert_eq!(composer.preedit().cursor, 2);
    }

    #[test]
    fn cursor_and_delete_never_split_cross_unit_graphemes() {
        for displayed_grapheme in ["か\u{3099}", "👩🏽‍💻", "🇯🇵"] {
            let mut composer = compose(displayed_grapheme);
            let end = composer.preedit().display.chars().count();
            assert_eq!(composer.preedit().cursor, end);
            assert!(composer.move_cursor(-1));
            assert_eq!(composer.preedit().cursor, 0);
            assert!(!composer.move_cursor(-1));
            assert!(composer.move_cursor(1));
            assert_eq!(composer.preedit().cursor, end);
            assert!(composer.backspace());
            assert!(composer.is_empty());
        }
    }

    #[test]
    fn punctuation_comes_from_the_table_and_creates_a_boundary() {
        for (input, expected) in [
            ("a!", "あ！"),
            ("a\"", "あ＂"),
            ("a#", "あ＃"),
            ("a$", "あ＄"),
            ("a%", "あ％"),
            ("a&", "あ＆"),
            ("a'", "あ＇"),
            ("a(", "あ（"),
            ("a)", "あ）"),
            ("a*", "あ＊"),
            ("a+", "あ＋"),
            ("a,", "あ、"),
            ("a-", "あー"),
            ("a.", "あ。"),
            ("a/", "あ・"),
            ("a:", "あ："),
            ("a;", "あ；"),
            ("a<", "あ＜"),
            ("a=", "あ＝"),
            ("a>", "あ＞"),
            ("a?", "あ？"),
            ("a@", "あ＠"),
            ("a[", "あ「"),
            ("a\\", "あ＼"),
            ("a]", "あ」"),
            ("a^", "あ＾"),
            ("a_", "あ＿"),
            ("a`", "あ｀"),
            ("a{", "あ｛"),
            ("a|", "あ｜"),
            ("a}", "あ｝"),
            ("a~", "あ～"),
        ] {
            let preedit = compose(input).preedit();
            assert_eq!(preedit.display, expected, "input: {input}");
            assert_eq!(preedit.hard_boundaries, vec![2], "input: {input}");
        }

        for (punctuation, expected) in [
            ("!", "！"),
            ("\"", "＂"),
            ("#", "＃"),
            ("$", "＄"),
            ("%", "％"),
            ("&", "＆"),
            ("(", "（"),
            (")", "）"),
            ("*", "＊"),
            ("+", "＋"),
            (",", "、"),
            ("-", "ー"),
            (".", "。"),
            ("/", "・"),
            (":", "："),
            (";", "；"),
            ("<", "＜"),
            ("=", "＝"),
            (">", "＞"),
            ("?", "？"),
            ("@", "＠"),
            ("[", "「"),
            ("\\", "＼"),
            ("]", "」"),
            ("^", "＾"),
            ("_", "＿"),
            ("`", "｀"),
            ("{", "｛"),
            ("|", "｜"),
            ("}", "｝"),
            ("~", "～"),
        ] {
            let input = format!("n{punctuation}");
            let expected = format!("ん{expected}");
            let preedit = compose(&input).preedit();
            assert_eq!(preedit.display, expected, "input: {input}");
            assert!(preedit.pending_latin.is_empty(), "input: {input}");
            assert_eq!(preedit.hard_boundaries, vec![2], "input: {input}");
        }

        assert_eq!(compose("n'").preedit().display, "ん");
    }

    #[test]
    fn embedded_table_is_independently_attributed_and_validated() {
        assert!(MUU_ORIGINAL_TABLE.contains("Muu original romaji table"));
        assert!(MUU_ORIGINAL_TABLE.contains("not copied from any existing IME"));
        RomajiTable::from_source(MUU_ORIGINAL_TABLE).expect("embedded table is valid");
    }
}
