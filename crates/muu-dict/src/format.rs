use std::ops::Range;

// Format 1.1 is little-endian. The fixed prefix contains magic/version,
// parameterized integer widths, the encoded header size, and SHA-256 of every
// byte after the header. The final section embeds the exact UTF-8 provenance
// manifest used by the deterministic builder.
//
// Fixed prefix (64 bytes): 0..8 magic, 8..10 major, 10..12 minor, 12 byte
// order, 13..22 the nine integer widths in `IntegerWidths` order, 24..28 the
// header size, 32..64 the payload SHA-256; bytes 22..24 and 28..32 are written
// as zero and not checked on read. The variable part follows: file size, then
// (offset, length) for each of the seven sections in `SectionKind` order, then
// the entry, token, right-axis, left-axis and unknown counts, then the BOS
// right id and EOS left id. A major or minor mismatch is refused outright;
// there is no compatibility reader.
pub(crate) const MAGIC: &[u8; 8] = b"MUUDICT\0";
pub(crate) const FORMAT_MAJOR: u16 = 1;
pub(crate) const FORMAT_MINOR: u16 = 1;
pub(crate) const LITTLE_ENDIAN: u8 = 1;
pub(crate) const PREFIX_SIZE: usize = 64;
pub(crate) const SECTION_COUNT: usize = 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntegerWidths {
    pub offset: u8,
    pub count: u8,
    pub trie_value: u8,
    pub token_index: u8,
    pub string_length: u8,
    pub context_id: u8,
    pub word_cost: u8,
    pub connection_cost: u8,
    pub pos_id: u8,
}

impl Default for IntegerWidths {
    fn default() -> Self {
        Self {
            offset: 8,
            count: 4,
            trie_value: 4,
            token_index: 4,
            string_length: 4,
            context_id: 2,
            word_cost: 2,
            connection_cost: 2,
            pos_id: 2,
        }
    }
}

impl IntegerWidths {
    pub(crate) fn validate(self) -> Result<(), &'static str> {
        validate_unsigned(self.offset)?;
        validate_unsigned(self.count)?;
        if self.trie_value != 4 {
            return Err("the provisional DARTS backend requires 4-byte trie values");
        }
        validate_unsigned(self.token_index)?;
        validate_unsigned(self.string_length)?;
        validate_unsigned(self.context_id)?;
        validate_signed(self.word_cost)?;
        validate_signed(self.connection_cost)?;
        validate_unsigned(self.pos_id)
    }
}

fn validate_unsigned(width: u8) -> Result<(), &'static str> {
    if matches!(width, 1 | 2 | 4 | 8) {
        Ok(())
    } else {
        Err("unsigned integer width must be 1, 2, 4, or 8")
    }
}

fn validate_signed(width: u8) -> Result<(), &'static str> {
    if matches!(width, 1 | 2 | 4) {
        Ok(())
    } else {
        Err("signed cost width must be 1, 2, or 4")
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Section {
    pub offset: u64,
    pub length: u64,
}

impl Section {
    pub(crate) fn range(self, file_len: usize) -> Result<Range<usize>, &'static str> {
        let start = usize::try_from(self.offset).map_err(|_| "section offset is too large")?;
        let length = usize::try_from(self.length).map_err(|_| "section length is too large")?;
        let end = start
            .checked_add(length)
            .ok_or("section range overflows address space")?;
        if end > file_len {
            return Err("section extends beyond the file");
        }
        Ok(start..end)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SectionKind {
    Darts = 0,
    Entries = 1,
    Tokens = 2,
    Strings = 3,
    Connection = 4,
    Unknown = 5,
    Provenance = 6,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Header {
    pub widths: IntegerWidths,
    pub header_size: u32,
    pub payload_sha256: [u8; 32],
    pub file_size: u64,
    pub sections: [Section; SECTION_COUNT],
    pub entry_count: u64,
    pub token_count: u64,
    pub right_axis_size: u64,
    pub left_axis_size: u64,
    pub unknown_count: u64,
    pub bos_right_id: u64,
    pub eos_left_id: u64,
}

impl Header {
    pub(crate) fn encoded_size(widths: IntegerWidths) -> Result<usize, &'static str> {
        widths.validate()?;
        PREFIX_SIZE
            .checked_add(
                (1 + SECTION_COUNT * 2)
                    .checked_mul(usize::from(widths.offset))
                    .ok_or("header size overflow")?,
            )
            .and_then(|size| size.checked_add(5_usize.checked_mul(usize::from(widths.count))?))
            .and_then(|size| size.checked_add(2_usize.checked_mul(usize::from(widths.context_id))?))
            .ok_or("header size overflow")
    }

    pub(crate) fn section(&self, kind: SectionKind) -> Section {
        self.sections[kind as usize]
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, &'static str> {
        self.widths.validate()?;
        let expected_size = Self::encoded_size(self.widths)?;
        if usize::try_from(self.header_size).ok() != Some(expected_size) {
            return Err("header_size does not match declared widths");
        }

        let mut output = vec![0; expected_size];
        output[..8].copy_from_slice(MAGIC);
        output[8..10].copy_from_slice(&FORMAT_MAJOR.to_le_bytes());
        output[10..12].copy_from_slice(&FORMAT_MINOR.to_le_bytes());
        output[12] = LITTLE_ENDIAN;
        output[13] = self.widths.offset;
        output[14] = self.widths.count;
        output[15] = self.widths.trie_value;
        output[16] = self.widths.token_index;
        output[17] = self.widths.string_length;
        output[18] = self.widths.context_id;
        output[19] = self.widths.word_cost;
        output[20] = self.widths.connection_cost;
        output[21] = self.widths.pos_id;
        output[24..28].copy_from_slice(&self.header_size.to_le_bytes());
        output[32..64].copy_from_slice(&self.payload_sha256);

        let mut cursor = PREFIX_SIZE;
        write_unsigned(&mut output, &mut cursor, self.widths.offset, self.file_size)?;
        for section in self.sections {
            write_unsigned(&mut output, &mut cursor, self.widths.offset, section.offset)?;
            write_unsigned(&mut output, &mut cursor, self.widths.offset, section.length)?;
        }
        for value in [
            self.entry_count,
            self.token_count,
            self.right_axis_size,
            self.left_axis_size,
            self.unknown_count,
        ] {
            write_unsigned(&mut output, &mut cursor, self.widths.count, value)?;
        }
        write_unsigned(
            &mut output,
            &mut cursor,
            self.widths.context_id,
            self.bos_right_id,
        )?;
        write_unsigned(
            &mut output,
            &mut cursor,
            self.widths.context_id,
            self.eos_left_id,
        )?;
        Ok(output)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        let prefix = bytes
            .get(..PREFIX_SIZE)
            .ok_or("dictionary header is truncated")?;
        if &prefix[..8] != MAGIC {
            return Err("dictionary magic does not match");
        }
        if u16::from_le_bytes([prefix[8], prefix[9]]) != FORMAT_MAJOR {
            return Err("dictionary major version is unsupported");
        }
        if u16::from_le_bytes([prefix[10], prefix[11]]) != FORMAT_MINOR {
            return Err("dictionary minor version is unsupported; rebuild from source");
        }
        if prefix[12] != LITTLE_ENDIAN {
            return Err("dictionary byte order is unsupported");
        }

        let widths = IntegerWidths {
            offset: prefix[13],
            count: prefix[14],
            trie_value: prefix[15],
            token_index: prefix[16],
            string_length: prefix[17],
            context_id: prefix[18],
            word_cost: prefix[19],
            connection_cost: prefix[20],
            pos_id: prefix[21],
        };
        widths.validate()?;
        let header_size = u32::from_le_bytes(prefix[24..28].try_into().unwrap());
        let payload_sha256 = prefix[32..64].try_into().unwrap();
        let expected_size = Self::encoded_size(widths)?;
        if usize::try_from(header_size).ok() != Some(expected_size) || bytes.len() < expected_size {
            return Err("dictionary header size is inconsistent");
        }

        let mut cursor = PREFIX_SIZE;
        let file_size = read_unsigned(bytes, &mut cursor, widths.offset)?;
        let mut sections = [Section::default(); SECTION_COUNT];
        for section in &mut sections {
            section.offset = read_unsigned(bytes, &mut cursor, widths.offset)?;
            section.length = read_unsigned(bytes, &mut cursor, widths.offset)?;
        }
        let entry_count = read_unsigned(bytes, &mut cursor, widths.count)?;
        let token_count = read_unsigned(bytes, &mut cursor, widths.count)?;
        let right_axis_size = read_unsigned(bytes, &mut cursor, widths.count)?;
        let left_axis_size = read_unsigned(bytes, &mut cursor, widths.count)?;
        let unknown_count = read_unsigned(bytes, &mut cursor, widths.count)?;
        let bos_right_id = read_unsigned(bytes, &mut cursor, widths.context_id)?;
        let eos_left_id = read_unsigned(bytes, &mut cursor, widths.context_id)?;

        Ok(Self {
            widths,
            header_size,
            payload_sha256,
            file_size,
            sections,
            entry_count,
            token_count,
            right_axis_size,
            left_axis_size,
            unknown_count,
            bos_right_id,
            eos_left_id,
        })
    }
}

pub(crate) fn read_unsigned(
    bytes: &[u8],
    cursor: &mut usize,
    width: u8,
) -> Result<u64, &'static str> {
    let end = cursor
        .checked_add(usize::from(width))
        .ok_or("integer offset overflow")?;
    let source = bytes.get(*cursor..end).ok_or("integer is truncated")?;
    *cursor = end;
    Ok(match width {
        1 => u64::from(source[0]),
        2 => u64::from(u16::from_le_bytes(source.try_into().unwrap())),
        4 => u64::from(u32::from_le_bytes(source.try_into().unwrap())),
        8 => u64::from_le_bytes(source.try_into().unwrap()),
        _ => return Err("unsupported unsigned integer width"),
    })
}

pub(crate) fn read_signed(
    bytes: &[u8],
    cursor: &mut usize,
    width: u8,
) -> Result<i32, &'static str> {
    let end = cursor
        .checked_add(usize::from(width))
        .ok_or("integer offset overflow")?;
    let source = bytes.get(*cursor..end).ok_or("integer is truncated")?;
    *cursor = end;
    Ok(match width {
        1 => i32::from(i8::from_le_bytes(source.try_into().unwrap())),
        2 => i32::from(i16::from_le_bytes(source.try_into().unwrap())),
        4 => i32::from_le_bytes(source.try_into().unwrap()),
        _ => return Err("unsupported signed integer width"),
    })
}

pub(crate) fn write_unsigned(
    output: &mut [u8],
    cursor: &mut usize,
    width: u8,
    value: u64,
) -> Result<(), &'static str> {
    let encoded = value.to_le_bytes();
    let width = usize::from(width);
    if width < 8 && value >= (1_u64 << (width * 8)) {
        return Err("unsigned value does not fit its declared width");
    }
    write_bytes(output, cursor, &encoded[..width])
}

pub(crate) fn append_unsigned(
    output: &mut Vec<u8>,
    width: u8,
    value: u64,
) -> Result<(), &'static str> {
    let mut cursor = output.len();
    output.resize(cursor + usize::from(width), 0);
    write_unsigned(output, &mut cursor, width, value)
}

pub(crate) fn append_signed(
    output: &mut Vec<u8>,
    width: u8,
    value: i32,
) -> Result<(), &'static str> {
    let fits = match width {
        1 => i8::try_from(value).is_ok(),
        2 => i16::try_from(value).is_ok(),
        4 => true,
        _ => false,
    };
    if !fits {
        return Err("signed value does not fit its declared width");
    }
    output.extend_from_slice(&value.to_le_bytes()[..usize::from(width)]);
    Ok(())
}

fn write_bytes(output: &mut [u8], cursor: &mut usize, value: &[u8]) -> Result<(), &'static str> {
    let end = cursor
        .checked_add(value.len())
        .ok_or("integer offset overflow")?;
    let destination = output.get_mut(*cursor..end).ok_or("header is truncated")?;
    destination.copy_from_slice(value);
    *cursor = end;
    Ok(())
}
