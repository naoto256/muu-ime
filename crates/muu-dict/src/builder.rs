use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use yada::builder::DoubleArrayBuilder;

use crate::format::{
    Header, IntegerWidths, SECTION_COUNT, Section, SectionKind, append_signed, append_unsigned,
};
use crate::{LeftContextId, LexicalClass, PosId, RightContextId, UnknownClass, UnknownDefinition};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexiconEntry {
    pub reading: String,
    pub surface: String,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos_id: PosId,
    pub right_pos_id: PosId,
    pub word_cost: i32,
}

#[derive(Clone, Debug)]
struct BuilderEntry {
    entry: LexiconEntry,
    lexical_class: LexicalClass,
}

#[derive(Clone, Debug)]
pub struct DictionaryBuilder {
    widths: IntegerWidths,
    right_axis_size: u32,
    left_axis_size: u32,
    bos_right_id: RightContextId,
    eos_left_id: LeftContextId,
    entries: Vec<BuilderEntry>,
    connection_costs: Vec<i32>,
    unknown: Vec<UnknownDefinition>,
    provenance: Vec<u8>,
}

impl DictionaryBuilder {
    pub fn new(
        right_axis_size: u32,
        left_axis_size: u32,
        bos_right_id: RightContextId,
        eos_left_id: LeftContextId,
    ) -> Result<Self, &'static str> {
        let matrix_len = usize::try_from(right_axis_size)
            .ok()
            .and_then(|right| {
                usize::try_from(left_axis_size)
                    .ok()
                    .and_then(|left| right.checked_mul(left))
            })
            .ok_or("connection matrix size overflow")?;
        let mut connection_costs = Vec::new();
        connection_costs
            .try_reserve_exact(matrix_len)
            .map_err(|_| "connection matrix allocation failed")?;
        connection_costs.resize(matrix_len, 0);
        Ok(Self {
            widths: IntegerWidths::default(),
            right_axis_size,
            left_axis_size,
            bos_right_id,
            eos_left_id,
            entries: Vec::new(),
            connection_costs,
            unknown: Vec::new(),
            provenance: Vec::new(),
        })
    }

    pub fn widths(mut self, widths: IntegerWidths) -> Self {
        self.widths = widths;
        self
    }

    pub fn add_entry(&mut self, entry: LexiconEntry) {
        self.add_entry_with_lexical_class(entry, LexicalClass::General);
    }

    pub fn add_entry_with_lexical_class(
        &mut self,
        entry: LexiconEntry,
        lexical_class: LexicalClass,
    ) {
        self.entries.push(BuilderEntry {
            entry,
            lexical_class,
        });
    }

    pub fn add_unknown(&mut self, definition: UnknownDefinition) {
        self.unknown.push(definition);
    }

    /// Embeds the exact human-readable provenance manifest in the dictionary.
    pub fn provenance(&mut self, provenance: impl Into<Vec<u8>>) {
        self.provenance = provenance.into();
    }

    /// Sets `matrix[previous.right_id][next.left_id]`.
    pub fn set_connection_cost(
        &mut self,
        previous: RightContextId,
        next: LeftContextId,
        cost: i32,
    ) -> Result<(), &'static str> {
        let index = self.matrix_index(previous, next)?;
        self.connection_costs[index] = cost;
        Ok(())
    }

    // Deterministic: entries are sorted by every field and grouped by reading,
    // unknown definitions by class, and sections are laid out in a fixed
    // order, so the same inputs always give the same bytes (and hash). One
    // DARTS value per distinct reading points at that reading's token run.
    pub fn build(mut self) -> Result<Vec<u8>, &'static str> {
        self.validate()?;
        self.entries.sort_by(|left, right| {
            let left_entry = &left.entry;
            let right_entry = &right.entry;
            (
                left_entry.reading.as_bytes(),
                left_entry.surface.as_bytes(),
                left_entry.left_id,
                left_entry.right_id,
                left_entry.left_pos_id,
                left_entry.right_pos_id,
                left.lexical_class,
                left_entry.word_cost,
            )
                .cmp(&(
                    right_entry.reading.as_bytes(),
                    right_entry.surface.as_bytes(),
                    right_entry.left_id,
                    right_entry.right_id,
                    right_entry.left_pos_id,
                    right_entry.right_pos_id,
                    right.lexical_class,
                    right_entry.word_cost,
                ))
        });
        self.unknown
            .sort_by_key(|definition| definition.class as u8);

        let mut grouped: BTreeMap<String, Vec<BuilderEntry>> = BTreeMap::new();
        for entry in self.entries {
            let reading = entry.entry.reading.clone();
            grouped.entry(reading).or_default().push(entry);
        }

        let mut entry_bytes = Vec::new();
        let mut token_bytes = Vec::new();
        let mut strings = Vec::new();
        let mut key_values = Vec::new();
        let mut token_count = 0_u64;

        for (entry_index, (reading, tokens)) in grouped.into_iter().enumerate() {
            let entry_index =
                u32::try_from(entry_index).map_err(|_| "too many DARTS entry records")?;
            if entry_index >= (1 << 31) {
                return Err("DARTS entry index exceeds the 31-bit value limit");
            }
            key_values.push((reading.into_bytes(), entry_index));
            append_unsigned(&mut entry_bytes, self.widths.token_index, token_count)?;
            append_unsigned(
                &mut entry_bytes,
                self.widths.count,
                u64::try_from(tokens.len()).map_err(|_| "token count is too large")?,
            )?;

            for token in tokens {
                let entry = token.entry;
                let surface_offset =
                    u64::try_from(strings.len()).map_err(|_| "string pool is too large")?;
                strings.extend_from_slice(entry.surface.as_bytes());
                append_unsigned(&mut token_bytes, self.widths.offset, surface_offset)?;
                append_unsigned(
                    &mut token_bytes,
                    self.widths.string_length,
                    u64::try_from(entry.surface.len())
                        .map_err(|_| "surface string is too large")?,
                )?;
                append_unsigned(
                    &mut token_bytes,
                    self.widths.context_id,
                    u64::from(entry.left_id.get()),
                )?;
                append_unsigned(
                    &mut token_bytes,
                    self.widths.context_id,
                    u64::from(entry.right_id.get()),
                )?;
                append_unsigned(
                    &mut token_bytes,
                    self.widths.pos_id,
                    u64::from(entry.left_pos_id.get()),
                )?;
                append_unsigned(
                    &mut token_bytes,
                    self.widths.pos_id,
                    u64::from(entry.right_pos_id.get()),
                )?;
                token_bytes.push(token.lexical_class as u8);
                append_signed(&mut token_bytes, self.widths.word_cost, entry.word_cost)?;
                token_count = token_count.checked_add(1).ok_or("token count overflow")?;
            }
        }

        let keys: Vec<(&[u8], u32)> = key_values
            .iter()
            .map(|(key, value)| (key.as_slice(), *value))
            .collect();
        let darts = DoubleArrayBuilder::build(&keys).map_err(|_| "DARTS construction failed")?;

        let mut connection = Vec::new();
        for cost in self.connection_costs {
            append_signed(&mut connection, self.widths.connection_cost, cost)?;
        }

        let mut unknown = Vec::new();
        for definition in &self.unknown {
            unknown.push(definition.class as u8);
            unknown.push(u8::from(definition.group));
            append_unsigned(
                &mut unknown,
                self.widths.count,
                u64::from(definition.max_length),
            )?;
            append_unsigned(
                &mut unknown,
                self.widths.context_id,
                u64::from(definition.left_id.get()),
            )?;
            append_unsigned(
                &mut unknown,
                self.widths.context_id,
                u64::from(definition.right_id.get()),
            )?;
            append_unsigned(
                &mut unknown,
                self.widths.pos_id,
                u64::from(definition.left_pos_id.get()),
            )?;
            append_unsigned(
                &mut unknown,
                self.widths.pos_id,
                u64::from(definition.right_pos_id.get()),
            )?;
            append_signed(&mut unknown, self.widths.word_cost, definition.word_cost)?;
        }

        let header_size = Header::encoded_size(self.widths)?;
        let mut output = vec![0; header_size];
        let mut sections = [Section::default(); SECTION_COUNT];
        for (kind, bytes) in [
            (SectionKind::Darts, darts.as_slice()),
            (SectionKind::Entries, entry_bytes.as_slice()),
            (SectionKind::Tokens, token_bytes.as_slice()),
            (SectionKind::Strings, strings.as_slice()),
            (SectionKind::Connection, connection.as_slice()),
            (SectionKind::Unknown, unknown.as_slice()),
            (SectionKind::Provenance, self.provenance.as_slice()),
        ] {
            sections[kind as usize] = Section {
                offset: u64::try_from(output.len()).map_err(|_| "file is too large")?,
                length: u64::try_from(bytes.len()).map_err(|_| "section is too large")?,
            };
            output.extend_from_slice(bytes);
        }

        let header = Header {
            widths: self.widths,
            header_size: u32::try_from(header_size).map_err(|_| "header is too large")?,
            payload_sha256: Sha256::digest(&output[header_size..]).into(),
            file_size: u64::try_from(output.len()).map_err(|_| "file is too large")?,
            sections,
            entry_count: u64::try_from(keys.len()).map_err(|_| "entry count is too large")?,
            token_count,
            right_axis_size: u64::from(self.right_axis_size),
            left_axis_size: u64::from(self.left_axis_size),
            unknown_count: u64::try_from(self.unknown.len())
                .map_err(|_| "unknown definition count is too large")?,
            bos_right_id: u64::from(self.bos_right_id.get()),
            eos_left_id: u64::from(self.eos_left_id.get()),
        };
        let encoded_header = header.encode()?;
        output[..header_size].copy_from_slice(&encoded_header);
        Ok(output)
    }

    fn validate(&self) -> Result<(), &'static str> {
        self.widths.validate()?;
        if self.entries.is_empty() {
            return Err("the DARTS dictionary must contain an entry");
        }
        if self.provenance.is_empty() {
            return Err("dictionary provenance must not be empty");
        }
        std::str::from_utf8(&self.provenance)
            .map_err(|_| "dictionary provenance must be valid UTF-8")?;
        if self.right_axis_size == 0 || self.left_axis_size == 0 {
            return Err("connection matrix axes must not be empty");
        }
        if u32::from(self.bos_right_id.get()) >= self.right_axis_size {
            return Err("BOS right context ID is outside the matrix");
        }
        if u32::from(self.eos_left_id.get()) >= self.left_axis_size {
            return Err("EOS left context ID is outside the matrix");
        }
        let expected_matrix_len = usize::try_from(self.right_axis_size)
            .ok()
            .and_then(|right| {
                usize::try_from(self.left_axis_size)
                    .ok()
                    .and_then(|left| right.checked_mul(left))
            })
            .ok_or("connection matrix size overflow")?;
        if self.connection_costs.len() != expected_matrix_len {
            return Err("connection matrix length is inconsistent");
        }
        for entry in &self.entries {
            let entry = &entry.entry;
            if entry.reading.is_empty() {
                return Err("dictionary readings must not be empty");
            }
            if u32::from(entry.left_id.get()) >= self.left_axis_size
                || u32::from(entry.right_id.get()) >= self.right_axis_size
            {
                return Err("token context ID is outside the matrix");
            }
        }
        for definition in &self.unknown {
            if u32::from(definition.left_id.get()) >= self.left_axis_size
                || u32::from(definition.right_id.get()) >= self.right_axis_size
            {
                return Err("unknown context ID is outside the matrix");
            }
        }
        if self
            .unknown
            .windows(2)
            .any(|pair| pair[0].class == pair[1].class)
        {
            return Err("unknown character classes must be unique");
        }
        Ok(())
    }

    fn matrix_index(
        &self,
        previous: RightContextId,
        next: LeftContextId,
    ) -> Result<usize, &'static str> {
        if u32::from(previous.get()) >= self.right_axis_size
            || u32::from(next.get()) >= self.left_axis_size
        {
            return Err("connection context ID is outside the matrix");
        }
        // Physical layout: previous-right varies fastest within each next-left row.
        usize::from(previous.get())
            .checked_add(
                usize::try_from(self.right_axis_size)
                    .map_err(|_| "right axis is too large")?
                    .checked_mul(usize::from(next.get()))
                    .ok_or("connection matrix index overflow")?,
            )
            .ok_or("connection matrix index overflow")
    }
}

impl Default for UnknownDefinition {
    fn default() -> Self {
        Self {
            class: UnknownClass::Other,
            group: false,
            max_length: 1,
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost: 0,
        }
    }
}
