//! The offline ETL that turns the pinned Mozc sources, the SudachiDict
//! supplement and Muu's own tables into a `DictionaryBuilder`. Every input is
//! validated row by row with its file and line in the error.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use muu_dict::{
    CoarsePos, DictionaryBuilder, LeftContextId, LexicalClass, LexiconEntry, PosId, RightContextId,
    UnknownClass, UnknownDefinition,
};
use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

// Mozc ships its lexicon as dictionary00..09.txt.
const MOZC_DICTIONARY_SHARDS: usize = 10;
const MOZC_SPECIAL_POS_COUNT: u32 = 3;
const MOZC_SPECIAL_CONNECTION_COST: i32 = 30_000;
const SYMBOL_SENTENCE_PATH_COST_BASE: i32 = 12_000;

#[derive(Debug)]
pub struct EtlError {
    message: String,
}

impl EtlError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn at(path: &Path, line: usize, message: impl fmt::Display) -> Self {
        Self::new(format!("{}:{line}: {message}", path.display()))
    }
}

impl fmt::Display for EtlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EtlError {}

impl From<io::Error> for EtlError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct PosMapSource {
    schema_version: u32,
    classes: Vec<PosMapClass>,
}

#[derive(Debug, Deserialize)]
struct PosMapClass {
    mozc_major: String,
    muu: String,
    id: u16,
}

#[derive(Debug, Deserialize)]
struct UnknownSource {
    schema_version: u32,
    classes: Vec<UnknownSourceClass>,
}

#[derive(Debug, Deserialize)]
struct UnknownSourceClass {
    class: String,
    group: bool,
    max_length: u32,
    left_context_id: u16,
    right_context_id: u16,
    word_cost: i32,
}

#[derive(Clone, Debug)]
pub struct PosTable {
    by_context: Vec<PosId>,
    lexical_classes: Vec<LexicalClass>,
    descriptors: Vec<String>,
}

impl PosTable {
    pub fn load(id_def: &Path, mapping: &Path) -> Result<Self, EtlError> {
        let mapping_text = std::fs::read_to_string(mapping)
            .map_err(|error| EtlError::new(format!("{}: {error}", mapping.display())))?;
        let source: PosMapSource = toml::from_str(&mapping_text)
            .map_err(|error| EtlError::new(format!("{}: {error}", mapping.display())))?;
        if source.schema_version != 1 {
            return Err(EtlError::new(format!(
                "{}: unsupported POS map schema {}",
                mapping.display(),
                source.schema_version
            )));
        }

        let mut major_to_pos = BTreeMap::new();
        let mut mapped_ids = BTreeSet::new();
        for class in source.classes {
            let coarse = CoarsePos::from_stable_name(&class.muu).ok_or_else(|| {
                EtlError::new(format!(
                    "{}: unknown Muu POS class {:?}",
                    mapping.display(),
                    class.muu
                ))
            })?;
            if coarse.id().get() != class.id {
                return Err(EtlError::new(format!(
                    "{}: POS class {:?} declares id {}, expected {}",
                    mapping.display(),
                    class.muu,
                    class.id,
                    coarse.id().get()
                )));
            }
            if major_to_pos
                .insert(class.mozc_major.clone(), coarse.id())
                .is_some()
            {
                return Err(EtlError::new(format!(
                    "{}: duplicate Mozc major POS {:?}",
                    mapping.display(),
                    class.mozc_major
                )));
            }
            if !mapped_ids.insert(class.id) {
                return Err(EtlError::new(format!(
                    "{}: duplicate Muu POS id {}",
                    mapping.display(),
                    class.id
                )));
            }
        }

        let file = File::open(id_def)
            .map_err(|error| EtlError::new(format!("{}: {error}", id_def.display())))?;
        let mut by_context = Vec::new();
        let mut lexical_classes = Vec::new();
        let mut descriptors = Vec::new();
        for (zero_based_line, line) in BufReader::new(file).lines().enumerate() {
            let line_number = zero_based_line + 1;
            let line = line.map_err(|error| EtlError::at(id_def, line_number, error))?;
            let (raw_id, descriptor) = line
                .split_once(' ')
                .ok_or_else(|| EtlError::at(id_def, line_number, "missing POS descriptor"))?;
            let context_id = raw_id
                .parse::<usize>()
                .map_err(|error| EtlError::at(id_def, line_number, error))?;
            if context_id != by_context.len() {
                return Err(EtlError::at(
                    id_def,
                    line_number,
                    format!(
                        "context IDs must be contiguous; expected {}, got {context_id}",
                        by_context.len()
                    ),
                ));
            }
            let major = descriptor.split(',').next().unwrap_or(descriptor);
            let pos_id = major_to_pos.get(major).copied().ok_or_else(|| {
                EtlError::at(
                    id_def,
                    line_number,
                    format!("unmapped Mozc major POS {major:?}"),
                )
            })?;
            by_context.push(pos_id);
            lexical_classes.push(lexical_class_from_mozc_descriptor(descriptor));
            descriptors.push(descriptor.to_owned());
        }
        if by_context.is_empty() {
            return Err(EtlError::new(format!(
                "{}: empty POS table",
                id_def.display()
            )));
        }
        Ok(Self {
            by_context,
            lexical_classes,
            descriptors,
        })
    }

    pub fn len(&self) -> usize {
        self.by_context.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_context.is_empty()
    }

    pub fn get(&self, context_id: u16) -> Option<PosId> {
        self.by_context.get(usize::from(context_id)).copied()
    }

    pub fn lexical_class(&self, context_id: u16) -> Option<LexicalClass> {
        self.lexical_classes.get(usize::from(context_id)).copied()
    }

    // The lowest context id whose id.def descriptor matches every
    // comma-separated field of `pattern`, where `*` matches anything.
    fn resolve_pattern(&self, pattern: &str) -> Option<(u16, PosId)> {
        let expected = pattern.split(',').collect::<Vec<_>>();
        self.descriptors
            .iter()
            .enumerate()
            .filter(|(_, descriptor)| {
                descriptor
                    .split(',')
                    .zip(&expected)
                    .all(|(actual, expected)| *expected == "*" || actual == *expected)
            })
            .min_by_key(|(context_id, _)| *context_id)
            .and_then(|(context_id, _)| {
                let context_id = u16::try_from(context_id).ok()?;
                Some((context_id, self.get(context_id)?))
            })
    }
}

fn lexical_class_from_mozc_descriptor(descriptor: &str) -> LexicalClass {
    if descriptor.starts_with("名詞,固有名詞,人名,") {
        LexicalClass::Person
    } else if descriptor.starts_with("名詞,固有名詞,地域,") {
        LexicalClass::Place
    } else if descriptor.starts_with("名詞,固有名詞,組織,") {
        LexicalClass::Organization
    } else if descriptor.starts_with("名詞,固有名詞,") {
        LexicalClass::ProperOther
    } else if descriptor.starts_with("記号,") {
        LexicalClass::Symbol
    } else {
        LexicalClass::General
    }
}

pub fn load_mozc_builder(
    source_dir: &Path,
    sudachi_supplement: &Path,
    pos_mapping: &Path,
    unknown_source: &Path,
    provenance_source: &Path,
) -> Result<DictionaryBuilder, EtlError> {
    let id_def = source_dir.join("id.def");
    let pos_table = PosTable::load(&id_def, pos_mapping)?;
    let connection = source_dir.join("connection_single_column.txt");
    let (mut builder, matrix_size, boundary_costs) = builder_from_connection(&connection)?;
    if usize::try_from(matrix_size).ok() != Some(pos_table.len()) {
        return Err(EtlError::new(format!(
            "{}: matrix size {matrix_size} does not match {} POS rows",
            connection.display(),
            pos_table.len()
        )));
    }
    load_lexicon(source_dir, &pos_table, &mut builder)?;
    load_lexicon_supplement(sudachi_supplement, &pos_table, &mut builder)?;
    load_symbol_lexicon(source_dir, &pos_table, &boundary_costs, &mut builder)?;
    load_unknown_definitions(unknown_source, &pos_table, &mut builder)?;
    let provenance = std::fs::read(provenance_source)
        .map_err(|error| EtlError::new(format!("{}: {error}", provenance_source.display())))?;
    builder.provenance(provenance);
    Ok(builder)
}

#[derive(Debug)]
struct SymbolToken {
    reading: String,
    surface: String,
    context_id: u16,
    pos_id: PosId,
    sorting_key: u64,
    source_order: usize,
}

#[derive(Debug)]
struct SentenceBoundaryCosts {
    bos_to_left: Vec<i32>,
    right_to_eos: Vec<i32>,
}

fn load_symbol_lexicon(
    dictionary_source_dir: &Path,
    pos_table: &PosTable,
    boundary_costs: &SentenceBoundaryCosts,
    builder: &mut DictionaryBuilder,
) -> Result<(), EtlError> {
    let data_dir = dictionary_source_dir.parent().ok_or_else(|| {
        EtlError::new(format!(
            "{}: Mozc dictionary source has no data parent",
            dictionary_source_dir.display()
        ))
    })?;
    for entry in parse_symbol_lexicon(
        &data_dir.join("symbol/symbol.tsv"),
        &data_dir.join("symbol/ordering_rule.txt"),
        pos_table,
        boundary_costs,
    )? {
        builder.add_entry_with_lexical_class(entry, LexicalClass::Symbol);
    }
    Ok(())
}

// Mozc's symbol.tsv becomes ordinary tokens. The first row of each NFC surface
// wins, readings are normalized (full-width ASCII and U+3000 to ASCII, lower
// case, NFC), and each reading's symbols are ordered by ordering_rule.txt
// (unlisted ones after, by code point), then surface and source line.
fn parse_symbol_lexicon(
    symbol_path: &Path,
    ordering_path: &Path,
    pos_table: &PosTable,
    boundary_costs: &SentenceBoundaryCosts,
) -> Result<Vec<LexiconEntry>, EtlError> {
    let ordering = load_symbol_ordering(ordering_path)?;
    let ordering_tail = ordering.values().copied().max().map_or(Ok(0), |last| {
        last.checked_add(1)
            .ok_or_else(|| EtlError::new("symbol ordering table is too large"))
    })?;
    let file = File::open(symbol_path)
        .map_err(|error| EtlError::new(format!("{}: {error}", symbol_path.display())))?;
    let mut lines = BufReader::new(file).lines();
    let header = lines
        .next()
        .ok_or_else(|| EtlError::new(format!("{}: empty symbol source", symbol_path.display())))?
        .map_err(|error| EtlError::at(symbol_path, 1, error))?;
    if header
        != "POS\tCHAR\tReading (space separated)\tdescription\tadditional description\tcategory\tmemo\tunicode"
    {
        return Err(EtlError::at(symbol_path, 1, "unexpected symbol TSV header"));
    }

    let mut seen_surfaces = BTreeSet::new();
    let mut grouped = BTreeMap::<String, Vec<SymbolToken>>::new();
    for (zero_based_line, line) in lines.enumerate() {
        let line_number = zero_based_line + 2;
        let line = line.map_err(|error| EtlError::at(symbol_path, line_number, error))?;
        if line.is_empty() {
            continue;
        }
        let columns = line.split('\t').collect::<Vec<_>>();
        if columns.len() < 3 || (columns[1].is_empty() && columns[2].is_empty()) {
            continue;
        }
        let pos_pattern = symbol_pos_pattern(columns[0]).ok_or_else(|| {
            EtlError::at(
                symbol_path,
                line_number,
                format!("unsupported symbol POS {:?}", columns[0]),
            )
        })?;
        let surface = columns[1].nfc().collect::<String>();
        if surface.is_empty() || !seen_surfaces.insert(surface.clone()) {
            continue;
        }
        let (context_id, pos_id) = pos_table.resolve_pattern(pos_pattern).ok_or_else(|| {
            EtlError::at(
                symbol_path,
                line_number,
                format!("symbol POS pattern {pos_pattern:?} has no id.def match"),
            )
        })?;
        let first = surface.chars().next().expect("non-empty symbol surface");
        let sorting_key = ordering
            .get(&first.to_string())
            .copied()
            .unwrap_or_else(|| ordering_tail.saturating_add(u64::from(first as u32)));
        let mut row_readings = BTreeSet::new();
        for raw_reading in columns[2].split(' ').filter(|reading| !reading.is_empty()) {
            let reading = normalize_symbol_reading(raw_reading);
            if reading.is_empty() || !row_readings.insert(reading.clone()) {
                continue;
            }
            grouped
                .entry(reading.clone())
                .or_default()
                .push(SymbolToken {
                    reading,
                    surface: surface.clone(),
                    context_id,
                    pos_id,
                    sorting_key,
                    source_order: line_number,
                });
        }
    }

    let mut entries = Vec::new();
    for tokens in grouped.values_mut() {
        tokens.sort_by(|left, right| {
            (left.sorting_key, &left.surface, left.source_order).cmp(&(
                right.sorting_key,
                &right.surface,
                right.source_order,
            ))
        });
        for (rank, token) in tokens.drain(..).enumerate() {
            let rank = i32::try_from(rank)
                .map_err(|_| EtlError::new("too many symbols for one reading"))?;
            let boundary_cost = boundary_costs.bos_to_left[usize::from(token.context_id)]
                .checked_add(boundary_costs.right_to_eos[usize::from(token.context_id)])
                .ok_or_else(|| EtlError::new("symbol boundary cost overflow"))?;
            let final_path_cost = SYMBOL_SENTENCE_PATH_COST_BASE
                .checked_add(rank.saturating_mul(10))
                .ok_or_else(|| EtlError::new("symbol rank cost overflow"))?;
            entries.push(LexiconEntry {
                reading: token.reading,
                surface: token.surface,
                left_id: LeftContextId::new(token.context_id),
                right_id: RightContextId::new(token.context_id),
                left_pos_id: token.pos_id,
                right_pos_id: token.pos_id,
                // Local-search ordering includes BOS/EOS connection costs.
                // Remove that POS-dependent component so Mozc's equal-reading
                // ordering rule remains the final candidate order.
                word_cost: final_path_cost
                    .checked_sub(boundary_cost)
                    .ok_or_else(|| EtlError::new("symbol word cost underflow"))?,
            });
        }
    }
    Ok(entries)
}

// One symbol per line, in rank order. A line starting with `#` is a comment
// only when it is longer than the `#` symbol itself.
fn load_symbol_ordering(path: &Path) -> Result<BTreeMap<String, u64>, EtlError> {
    let file =
        File::open(path).map_err(|error| EtlError::new(format!("{}: {error}", path.display())))?;
    let mut result = BTreeMap::new();
    let mut sorting_key = 0_u64;
    for (zero_based_line, line) in BufReader::new(file).lines().enumerate() {
        let line_number = zero_based_line + 1;
        let line = line.map_err(|error| EtlError::at(path, line_number, error))?;
        if line.is_empty() || (line.starts_with('#') && line.chars().count() > 1) {
            continue;
        }
        result.entry(line).or_insert(sorting_key);
        sorting_key = sorting_key
            .checked_add(1)
            .ok_or_else(|| EtlError::at(path, line_number, "symbol ordering is too large"))?;
    }
    Ok(result)
}

fn symbol_pos_pattern(pos: &str) -> Option<&'static str> {
    match pos {
        "記号" => Some("記号,一般,*,*,*,*,*"),
        "アルファベット" => Some("記号,アルファベット,*,*,*,*,*"),
        "数" => Some("名詞,数,アラビア数字"),
        "句読点" => Some("記号,読点,*,*,*,*"),
        "括弧開" => Some("記号,括弧開,*,*,*,*,*"),
        "括弧閉" => Some("記号,括弧閉,*,*,*,*,*"),
        _ => None,
    }
}

fn normalize_symbol_reading(reading: &str) -> String {
    reading
        .chars()
        .flat_map(|character| {
            let character = match character {
                '\u{3000}' => ' ',
                '\u{ff01}'..='\u{ff5e}' => {
                    char::from_u32(character as u32 - 0xfee0).expect("fullwidth ASCII projection")
                }
                _ => character,
            };
            character.to_lowercase()
        })
        .collect::<String>()
        .nfc()
        .collect()
}

// dict-src/muu-unknown.toml: exactly one definition for each of the five
// unknown classes, with context ids that exist in id.def.
fn load_unknown_definitions(
    path: &Path,
    pos_table: &PosTable,
    builder: &mut DictionaryBuilder,
) -> Result<(), EtlError> {
    let source_text = std::fs::read_to_string(path)
        .map_err(|error| EtlError::new(format!("{}: {error}", path.display())))?;
    let source: UnknownSource = toml::from_str(&source_text)
        .map_err(|error| EtlError::new(format!("{}: {error}", path.display())))?;
    if source.schema_version != 1 {
        return Err(EtlError::new(format!(
            "{}: unsupported unknown model schema {}",
            path.display(),
            source.schema_version
        )));
    }

    let mut classes = BTreeSet::new();
    for row in source.classes {
        let class = parse_unknown_class(&row.class).ok_or_else(|| {
            EtlError::new(format!(
                "{}: unknown character class {:?}",
                path.display(),
                row.class
            ))
        })?;
        if !classes.insert(class as u8) {
            return Err(EtlError::new(format!(
                "{}: duplicate unknown class {:?}",
                path.display(),
                row.class
            )));
        }
        if row.max_length == 0 {
            return Err(EtlError::new(format!(
                "{}: max_length must be positive for {:?}",
                path.display(),
                row.class
            )));
        }
        let left_pos_id = pos_table.get(row.left_context_id).ok_or_else(|| {
            EtlError::new(format!(
                "{}: left context ID {} is absent from id.def",
                path.display(),
                row.left_context_id
            ))
        })?;
        let right_pos_id = pos_table.get(row.right_context_id).ok_or_else(|| {
            EtlError::new(format!(
                "{}: right context ID {} is absent from id.def",
                path.display(),
                row.right_context_id
            ))
        })?;
        builder.add_unknown(UnknownDefinition {
            class,
            group: row.group,
            max_length: row.max_length,
            left_id: LeftContextId::new(row.left_context_id),
            right_id: RightContextId::new(row.right_context_id),
            left_pos_id,
            right_pos_id,
            word_cost: row.word_cost,
        });
    }

    let required = [
        UnknownClass::Number,
        UnknownClass::AsciiAlphabetic,
        UnknownClass::Katakana,
        UnknownClass::Kanji,
        UnknownClass::Other,
    ];
    for class in required {
        if !classes.contains(&(class as u8)) {
            return Err(EtlError::new(format!(
                "{}: missing unknown class {}",
                path.display(),
                unknown_class_name(class)
            )));
        }
    }
    if classes.len() != required.len() {
        return Err(EtlError::new(format!(
            "{}: expected exactly {} unknown classes, got {}",
            path.display(),
            required.len(),
            classes.len()
        )));
    }
    Ok(())
}

fn parse_unknown_class(name: &str) -> Option<UnknownClass> {
    Some(match name {
        "number" => UnknownClass::Number,
        "ascii_alphabetic" => UnknownClass::AsciiAlphabetic,
        "katakana" => UnknownClass::Katakana,
        "kanji" => UnknownClass::Kanji,
        "other" => UnknownClass::Other,
        _ => return None,
    })
}

const fn unknown_class_name(class: UnknownClass) -> &'static str {
    match class {
        UnknownClass::Number => "number",
        UnknownClass::AsciiAlphabetic => "ascii_alphabetic",
        UnknownClass::Katakana => "katakana",
        UnknownClass::Kanji => "kanji",
        UnknownClass::Other => "other",
    }
}

// connection_single_column.txt: the size N, then N*N costs with the right
// (previous) id varying slowest. The matrix is widened by three special ids
// (N..N+3) that Mozc's lexicon never uses; Muu's own entries do (the postal
// layer uses N). Every transition into or out of them costs 30,000 except
// from BOS and to EOS (id 0). The BOS row and EOS column are also returned so
// symbol costs can be corrected.
fn builder_from_connection(
    path: &Path,
) -> Result<(DictionaryBuilder, u32, SentenceBoundaryCosts), EtlError> {
    let file =
        File::open(path).map_err(|error| EtlError::new(format!("{}: {error}", path.display())))?;
    let mut lines = BufReader::new(file).lines();
    let header = lines
        .next()
        .ok_or_else(|| EtlError::new(format!("{}: empty connection matrix", path.display())))?
        .map_err(|error| EtlError::at(path, 1, error))?;
    let size = header
        .parse::<u32>()
        .map_err(|error| EtlError::at(path, 1, error))?;
    if size == 0 || size > u32::from(u16::MAX) + 1 {
        return Err(EtlError::at(
            path,
            1,
            format!("matrix size {size} is outside the u16 context-ID domain"),
        ));
    }
    let expanded_size = size
        .checked_add(MOZC_SPECIAL_POS_COUNT)
        .ok_or_else(|| EtlError::new("expanded connection matrix size overflow"))?;
    let mut builder = DictionaryBuilder::new(
        expanded_size,
        expanded_size,
        RightContextId::new(0),
        LeftContextId::new(0),
    )
    .map_err(EtlError::new)?;
    let expected_values = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| EtlError::new("connection matrix size overflow"))?;
    let mut value_count = 0_u64;
    let mut bos_to_left = vec![0; usize::try_from(size).unwrap()];
    let mut right_to_eos = vec![0; usize::try_from(size).unwrap()];
    for (zero_based_value, line) in lines.enumerate() {
        let line_number = zero_based_value + 2;
        if value_count >= expected_values {
            return Err(EtlError::at(
                path,
                line_number,
                "connection matrix has more values than declared",
            ));
        }
        let line = line.map_err(|error| EtlError::at(path, line_number, error))?;
        let cost = line
            .parse::<i32>()
            .map_err(|error| EtlError::at(path, line_number, error))?;
        let right_id = u16::try_from(value_count / u64::from(size))
            .map_err(|_| EtlError::at(path, line_number, "right context ID exceeds u16"))?;
        let left_id = u16::try_from(value_count % u64::from(size))
            .map_err(|_| EtlError::at(path, line_number, "left context ID exceeds u16"))?;
        if right_id == 0 {
            bos_to_left[usize::from(left_id)] = cost;
        }
        if left_id == 0 {
            right_to_eos[usize::from(right_id)] = cost;
        }
        builder
            .set_connection_cost(
                RightContextId::new(right_id),
                LeftContextId::new(left_id),
                cost,
            )
            .map_err(|message| EtlError::at(path, line_number, message))?;
        value_count += 1;
    }
    if value_count != expected_values {
        return Err(EtlError::new(format!(
            "{}: declared {expected_values} connection values, found {value_count}",
            path.display()
        )));
    }
    for right_id in 1..expanded_size {
        for left_id in size..expanded_size {
            builder
                .set_connection_cost(
                    RightContextId::new(u16::try_from(right_id).unwrap()),
                    LeftContextId::new(u16::try_from(left_id).unwrap()),
                    MOZC_SPECIAL_CONNECTION_COST,
                )
                .map_err(EtlError::new)?;
        }
    }
    for right_id in size..expanded_size {
        for left_id in 1..expanded_size {
            builder
                .set_connection_cost(
                    RightContextId::new(u16::try_from(right_id).unwrap()),
                    LeftContextId::new(u16::try_from(left_id).unwrap()),
                    MOZC_SPECIAL_CONNECTION_COST,
                )
                .map_err(EtlError::new)?;
        }
    }
    Ok((
        builder,
        size,
        SentenceBoundaryCosts {
            bos_to_left,
            right_to_eos,
        },
    ))
}

fn load_lexicon(
    source_dir: &Path,
    pos_table: &PosTable,
    builder: &mut DictionaryBuilder,
) -> Result<(), EtlError> {
    for shard in 0..MOZC_DICTIONARY_SHARDS {
        let path = source_dir.join(format!("dictionary{shard:02}.txt"));
        let file = File::open(&path)
            .map_err(|error| EtlError::new(format!("{}: {error}", path.display())))?;
        for (zero_based_line, line) in BufReader::new(file).lines().enumerate() {
            let line_number = zero_based_line + 1;
            let line = line.map_err(|error| EtlError::at(&path, line_number, error))?;
            let (entry, lexical_class) = parse_lexicon_line(&path, line_number, &line, pos_table)?;
            builder.add_entry_with_lexical_class(entry, lexical_class);
        }
    }
    Ok(())
}

fn load_lexicon_supplement(
    path: &Path,
    pos_table: &PosTable,
    builder: &mut DictionaryBuilder,
) -> Result<(), EtlError> {
    let file =
        File::open(path).map_err(|error| EtlError::new(format!("{}: {error}", path.display())))?;
    let mut entry_count = 0_usize;
    for (zero_based_line, line) in BufReader::new(file).lines().enumerate() {
        let line_number = zero_based_line + 1;
        let line = line.map_err(|error| EtlError::at(path, line_number, error))?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (entry, lexical_class) = parse_lexicon_line(path, line_number, &line, pos_table)?;
        builder.add_entry_with_lexical_class(entry, lexical_class);
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| EtlError::new("supplement entry count overflow"))?;
        if entry_count > 1_000_000 {
            return Err(EtlError::at(
                path,
                line_number,
                "supplement exceeds the 1,000,000-entry bound",
            ));
        }
    }
    if entry_count == 0 {
        return Err(EtlError::new(format!(
            "{}: empty lexicon supplement",
            path.display()
        )));
    }
    Ok(())
}

// One Mozc lexicon row: reading, left id, right id, cost, surface, and an
// optional SPELLING_CORRECTION label (any other label is refused). Both ids
// must exist in id.def; reading and surface are NFC-normalized. The lexical
// class comes from the left id, or the right one when the left is general.
fn parse_lexicon_line(
    path: &Path,
    line_number: usize,
    line: &str,
    pos_table: &PosTable,
) -> Result<(LexiconEntry, LexicalClass), EtlError> {
    let columns = line.split('\t').collect::<Vec<_>>();
    if !(columns.len() == 5 || columns.len() == 6) {
        return Err(EtlError::at(
            path,
            line_number,
            format!("expected 5 or 6 TSV columns, got {}", columns.len()),
        ));
    }
    if columns.len() == 6 && columns[5] != "SPELLING_CORRECTION" {
        return Err(EtlError::at(
            path,
            line_number,
            format!("unsupported Mozc token label {:?}", columns[5]),
        ));
    }
    let left_raw = columns[1]
        .parse::<u16>()
        .map_err(|error| EtlError::at(path, line_number, error))?;
    let right_raw = columns[2]
        .parse::<u16>()
        .map_err(|error| EtlError::at(path, line_number, error))?;
    let left_pos_id = pos_table.get(left_raw).ok_or_else(|| {
        EtlError::at(
            path,
            line_number,
            format!("left context ID {left_raw} is absent from id.def"),
        )
    })?;
    let right_pos_id = pos_table.get(right_raw).ok_or_else(|| {
        EtlError::at(
            path,
            line_number,
            format!("right context ID {right_raw} is absent from id.def"),
        )
    })?;
    let left_lexical_class = pos_table.lexical_class(left_raw).ok_or_else(|| {
        EtlError::at(
            path,
            line_number,
            format!("left context ID {left_raw} is absent from id.def"),
        )
    })?;
    let right_lexical_class = pos_table.lexical_class(right_raw).ok_or_else(|| {
        EtlError::at(
            path,
            line_number,
            format!("right context ID {right_raw} is absent from id.def"),
        )
    })?;
    let word_cost = columns[3]
        .parse::<i32>()
        .map_err(|error| EtlError::at(path, line_number, error))?;
    let reading = columns[0].nfc().collect::<String>();
    let surface = columns[4].nfc().collect::<String>();
    if reading.is_empty() || surface.is_empty() {
        return Err(EtlError::at(
            path,
            line_number,
            "reading and surface must be non-empty",
        ));
    }
    Ok((
        LexiconEntry {
            reading,
            surface,
            left_id: LeftContextId::new(left_raw),
            right_id: RightContextId::new(right_raw),
            left_pos_id,
            right_pos_id,
            word_cost,
        },
        if left_lexical_class != LexicalClass::General {
            left_lexical_class
        } else {
            right_lexical_class
        },
    ))
}

pub fn default_source_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("third_party/mozc-oss/src/data/dictionary_oss")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use muu_conversion::{
        BoundaryContext, CostSnapshot, ScalarIndex, ScalarRange, candidates_from_paths,
        local_search,
    };
    use muu_dict::{Dictionary, UnknownDefinition};
    use tempfile::NamedTempFile;

    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn vendored_id_def_is_fully_covered_by_the_coarse_pos_map() {
        let root = repo_root();
        let source = default_source_dir(&root);
        let table = PosTable::load(
            &source.join("id.def"),
            &root.join("dict-src/mozc-pos-map.toml"),
        )
        .unwrap();

        assert_eq!(table.len(), 2_672);
        assert_eq!(table.get(0), Some(CoarsePos::BosEos.id()));
        assert_eq!(table.get(1_851), Some(CoarsePos::Noun.id()));
    }

    #[test]
    fn lexicon_mapping_keeps_distinct_left_and_right_pos_endpoints() {
        let root = repo_root();
        let source = default_source_dir(&root);
        let table = PosTable::load(
            &source.join("id.def"),
            &root.join("dict-src/mozc-pos-map.toml"),
        )
        .unwrap();
        let (entry, lexical_class) = parse_lexicon_line(
            Path::new("fixture"),
            1,
            "あいあんと\t1851\t371\t7161\tアイアンと",
            &table,
        )
        .unwrap();

        assert_eq!(entry.left_pos_id, CoarsePos::Noun.id());
        assert_eq!(entry.right_pos_id, CoarsePos::Particle.id());
        assert_eq!(lexical_class, LexicalClass::General);
    }

    #[test]
    fn mozc_detailed_pos_maps_to_lexical_classes() {
        assert_eq!(
            lexical_class_from_mozc_descriptor("名詞,固有名詞,人名,姓,*,*,*"),
            LexicalClass::Person
        );
        assert_eq!(
            lexical_class_from_mozc_descriptor("名詞,固有名詞,地域,一般,*,*,*"),
            LexicalClass::Place
        );
        assert_eq!(
            lexical_class_from_mozc_descriptor("名詞,固有名詞,組織,*,*,*,*"),
            LexicalClass::Organization
        );
        assert_eq!(
            lexical_class_from_mozc_descriptor("名詞,固有名詞,一般,*,*,*,*"),
            LexicalClass::ProperOther
        );
        assert_eq!(
            lexical_class_from_mozc_descriptor("記号,一般,*,*,*,*,*"),
            LexicalClass::Symbol
        );
    }

    #[test]
    fn pinned_symbol_source_expands_generic_and_category_readings() {
        let root = repo_root();
        let source = default_source_dir(&root);
        let table = PosTable::load(
            &source.join("id.def"),
            &root.join("dict-src/mozc-pos-map.toml"),
        )
        .unwrap();
        let (_, _, boundary_costs) =
            builder_from_connection(&source.join("connection_single_column.txt")).unwrap();
        let symbols = source.parent().unwrap().join("symbol");
        let entries = parse_symbol_lexicon(
            &symbols.join("symbol.tsv"),
            &symbols.join("ordering_rule.txt"),
            &table,
            &boundary_costs,
        )
        .unwrap();

        let generic = entries
            .iter()
            .filter(|entry| entry.reading == "きごう")
            .collect::<Vec<_>>();
        assert_eq!(generic.len(), 349);
        for (rank, entry) in generic.iter().enumerate() {
            let context_id = usize::from(entry.left_id.get());
            let final_cost = entry.word_cost
                + boundary_costs.bos_to_left[context_id]
                + boundary_costs.right_to_eos[context_id];
            assert_eq!(
                final_cost,
                SYMBOL_SENTENCE_PATH_COST_BASE + i32::try_from(rank).unwrap() * 10
            );
        }
        assert!(generic.iter().all(|entry| {
            entry.left_id.get() == entry.right_id.get()
                && entry.left_pos_id == entry.right_pos_id
                && matches!(
                    entry.left_pos_id,
                    id if id == CoarsePos::Symbol.id() || id == CoarsePos::Noun.id()
                )
        }));

        for (reading, expected) in [
            ("さんかく", ["△", "▲"]),
            ("まる", ["○", "●"]),
            ("ほし", ["★", "☆"]),
        ] {
            let surfaces = entries
                .iter()
                .filter(|entry| entry.reading == reading)
                .map(|entry| entry.surface.as_str())
                .collect::<BTreeSet<_>>();
            for expected in expected {
                assert!(surfaces.contains(expected), "{reading}: missing {expected}");
            }
        }

        assert!(entries.iter().any(|entry| {
            entry.reading == "えー"
                && entry.surface == "Ａ"
                && entry.left_pos_id == CoarsePos::Symbol.id()
        }));

        let expected = generic
            .iter()
            .map(|entry| entry.surface.clone())
            .collect::<Vec<_>>();
        let expected_set = expected.iter().cloned().collect::<BTreeSet<_>>();
        let (mut builder, _, _) =
            builder_from_connection(&source.join("connection_single_column.txt")).unwrap();
        for entry in entries
            .into_iter()
            .filter(|entry| entry.reading == "きごう")
        {
            builder.add_entry(entry);
        }
        builder.add_unknown(UnknownDefinition::default());
        builder.provenance(b"name = \"symbol-final-order-test\"\n");
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), builder.build().unwrap()).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let paths =
            local_search("きごう", &BoundaryContext::sentence(&snapshot), &snapshot).unwrap();
        let actual = candidates_from_paths(
            ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(3)),
            paths,
        )
        .unwrap()
        .into_iter()
        .map(|candidate| candidate.surface().to_owned())
        .filter(|surface| expected_set.contains(surface))
        .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn real_mozc_connection_axes_round_trip_without_transposition() {
        let root = repo_root();
        let source = default_source_dir(&root);
        let (mut builder, size, _) =
            builder_from_connection(&source.join("connection_single_column.txt")).unwrap();
        assert_eq!(size, 2_672);
        builder.add_entry(LexiconEntry {
            reading: "むう".into(),
            surface: "Muu".into(),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: CoarsePos::Other.id(),
            right_pos_id: CoarsePos::Other.id(),
            word_cost: 0,
        });
        builder.provenance(b"name = \"axis-round-trip-test\"\n");
        builder.add_unknown(UnknownDefinition::default());
        let bytes = builder.build().unwrap();
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();

        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(1), LeftContextId::new(2))
                .unwrap(),
            4_743
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(2), LeftContextId::new(1))
                .unwrap(),
            4_831
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(0), LeftContextId::new(2_672))
                .unwrap(),
            0
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(2_672), LeftContextId::new(0))
                .unwrap(),
            0
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(1), LeftContextId::new(2_673))
                .unwrap(),
            MOZC_SPECIAL_CONNECTION_COST
        );
        assert_eq!(
            dictionary
                .connection_cost(RightContextId::new(2_674), LeftContextId::new(1))
                .unwrap(),
            MOZC_SPECIAL_CONNECTION_COST
        );
    }

    #[test]
    fn muu_original_unknown_model_is_complete_and_uses_endpoint_pos() {
        let root = repo_root();
        let source = default_source_dir(&root);
        let table = PosTable::load(
            &source.join("id.def"),
            &root.join("dict-src/mozc-pos-map.toml"),
        )
        .unwrap();
        let (mut builder, _, _) =
            builder_from_connection(&source.join("connection_single_column.txt")).unwrap();
        load_unknown_definitions(
            &root.join("dict-src/muu-unknown.toml"),
            &table,
            &mut builder,
        )
        .unwrap();
        builder.add_entry(LexiconEntry {
            reading: "むう".into(),
            surface: "Muu".into(),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: CoarsePos::Other.id(),
            right_pos_id: CoarsePos::Other.id(),
            word_cost: 0,
        });
        builder.provenance(b"name = \"unknown-model-test\"\n");
        let bytes = builder.build().unwrap();
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        let dictionary = Dictionary::open(file.path()).unwrap();

        let number = dictionary.unknown_definition(UnknownClass::Number).unwrap();
        assert_eq!(number.left_id, LeftContextId::new(2_044));
        assert_eq!(number.right_id, RightContextId::new(2_044));
        assert_eq!(number.left_pos_id, CoarsePos::Noun.id());
        assert_eq!(number.right_pos_id, CoarsePos::Noun.id());
        assert_eq!(number.word_cost, 3_333);

        let other = dictionary.unknown_definition(UnknownClass::Other).unwrap();
        assert!(!other.group);
        assert_eq!(other.max_length, 1);
        assert_eq!(other.left_pos_id, CoarsePos::Symbol.id());
        assert_eq!(other.right_pos_id, CoarsePos::Symbol.id());
    }
}
