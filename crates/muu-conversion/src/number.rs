#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NumberRewrite {
    pub byte_len: usize,
    pub surface: String,
    pub word_cost_bonus: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    Digit(u64),
    SmallUnit(u64),
    LargeUnit(u64),
}

#[derive(Clone, Copy)]
struct Lexeme {
    reading: &'static str,
    events: &'static [Event],
}

const D1: [Event; 1] = [Event::Digit(1)];
const D2: [Event; 1] = [Event::Digit(2)];
const D3: [Event; 1] = [Event::Digit(3)];
const D4: [Event; 1] = [Event::Digit(4)];
const D5: [Event; 1] = [Event::Digit(5)];
const D6: [Event; 1] = [Event::Digit(6)];
const D7: [Event; 1] = [Event::Digit(7)];
const D8: [Event; 1] = [Event::Digit(8)];
const D9: [Event; 1] = [Event::Digit(9)];
const D0: [Event; 1] = [Event::Digit(0)];
const TEN: [Event; 1] = [Event::SmallUnit(10)];
const HUNDRED: [Event; 1] = [Event::SmallUnit(100)];
const THOUSAND: [Event; 1] = [Event::SmallUnit(1_000)];
const TEN_THOUSAND: [Event; 1] = [Event::LargeUnit(10_000)];
const HUNDRED_MILLION: [Event; 1] = [Event::LargeUnit(100_000_000)];
const TRILLION: [Event; 1] = [Event::LargeUnit(1_000_000_000_000)];

const D1_THOUSAND: [Event; 2] = [Event::Digit(1), Event::SmallUnit(1_000)];
const D3_THOUSAND: [Event; 2] = [Event::Digit(3), Event::SmallUnit(1_000)];
const D8_THOUSAND: [Event; 2] = [Event::Digit(8), Event::SmallUnit(1_000)];
const D3_HUNDRED: [Event; 2] = [Event::Digit(3), Event::SmallUnit(100)];
const D6_HUNDRED: [Event; 2] = [Event::Digit(6), Event::SmallUnit(100)];
const D8_HUNDRED: [Event; 2] = [Event::Digit(8), Event::SmallUnit(100)];

// Longest and phonologically irregular forms precede their regular parts.
const PLACE_LEXEMES: &[Lexeme] = &[
    Lexeme {
        reading: "さんびゃく",
        events: &D3_HUNDRED,
    },
    Lexeme {
        reading: "ろっぴゃく",
        events: &D6_HUNDRED,
    },
    Lexeme {
        reading: "はっぴゃく",
        events: &D8_HUNDRED,
    },
    Lexeme {
        reading: "いっせん",
        events: &D1_THOUSAND,
    },
    Lexeme {
        reading: "さんぜん",
        events: &D3_THOUSAND,
    },
    Lexeme {
        reading: "はっせん",
        events: &D8_THOUSAND,
    },
    Lexeme {
        reading: "じゅう",
        events: &TEN,
    },
    Lexeme {
        reading: "ひゃく",
        events: &HUNDRED,
    },
    Lexeme {
        reading: "せん",
        events: &THOUSAND,
    },
    Lexeme {
        reading: "まん",
        events: &TEN_THOUSAND,
    },
    Lexeme {
        reading: "おく",
        events: &HUNDRED_MILLION,
    },
    Lexeme {
        reading: "ちょう",
        events: &TRILLION,
    },
    Lexeme {
        reading: "きゅう",
        events: &D9,
    },
    Lexeme {
        reading: "いち",
        events: &D1,
    },
    Lexeme {
        reading: "さん",
        events: &D3,
    },
    Lexeme {
        reading: "よん",
        events: &D4,
    },
    Lexeme {
        reading: "ろく",
        events: &D6,
    },
    Lexeme {
        reading: "なな",
        events: &D7,
    },
    Lexeme {
        reading: "はち",
        events: &D8,
    },
    Lexeme {
        reading: "ぜろ",
        events: &D0,
    },
    Lexeme {
        reading: "れい",
        events: &D0,
    },
    Lexeme {
        reading: "に",
        events: &D2,
    },
    Lexeme {
        reading: "ご",
        events: &D5,
    },
];

const DIGIT_LEXEMES: &[Lexeme] = &[
    Lexeme {
        reading: "きゅう",
        events: &D9,
    },
    Lexeme {
        reading: "いち",
        events: &D1,
    },
    Lexeme {
        reading: "さん",
        events: &D3,
    },
    Lexeme {
        reading: "よん",
        events: &D4,
    },
    Lexeme {
        reading: "ろく",
        events: &D6,
    },
    Lexeme {
        reading: "なな",
        events: &D7,
    },
    Lexeme {
        reading: "はち",
        events: &D8,
    },
    Lexeme {
        reading: "ぜろ",
        events: &D0,
    },
    Lexeme {
        reading: "れい",
        events: &D0,
    },
    Lexeme {
        reading: "に",
        events: &D2,
    },
    Lexeme {
        reading: "ご",
        events: &D5,
    },
];

#[derive(Clone, Debug)]
struct NumericPrefix {
    byte_len: usize,
    value: u64,
    rendered: String,
    atom_count: usize,
    has_magnitude: bool,
}

// Number and date surfaces for every prefix of `input` that reads as one.
// A lone digit word (に, ご) is not rewritten, only place values and runs of
// two or more digits, but any run qualifies, so ordinary words such as いちご
// also get a "15" node; nothing filters it, and it competes with dictionary
// words through the Number unknown cost. Dates get a 1,000 bonus. Day words
// like ついたち count only after a month. Sorted by length, then surface.
pub(crate) fn rewrites(input: &str) -> Vec<NumberRewrite> {
    let Some(first) = input.chars().next() else {
        return Vec::new();
    };
    // Cheap gate: the first characters of every lexeme and special date.
    if !matches!(
        first,
        'い' | 'に'
            | 'ご'
            | 'さ'
            | 'よ'
            | 'ろ'
            | 'な'
            | 'は'
            | 'き'
            | 'ぜ'
            | 'れ'
            | 'じ'
            | 'ひ'
            | 'せ'
            | 'ま'
            | 'お'
            | 'ち'
            | 'し'
            | 'く'
    ) {
        return Vec::new();
    }
    let prefixes = numeric_prefixes(input);

    let mut rewrites = Vec::new();
    for prefix in prefixes {
        if prefix.has_magnitude || prefix.atom_count >= 2 {
            push_unique(
                &mut rewrites,
                NumberRewrite {
                    byte_len: prefix.byte_len,
                    surface: prefix.rendered.clone(),
                    word_cost_bonus: 0,
                },
            );
        }
        add_date_suffixes(input, prefix, &mut rewrites);
    }
    add_special_dates(input, &mut rewrites);
    add_compound_dates(input, &mut rewrites);
    rewrites.sort_by(|left, right| {
        left.byte_len
            .cmp(&right.byte_len)
            .then_with(|| left.surface.cmp(&right.surface))
    });
    rewrites
}

fn numeric_prefixes(input: &str) -> Vec<NumericPrefix> {
    let mut prefixes = place_value_prefixes(input);
    prefixes.extend(digit_sequence_prefixes(input));
    prefixes.sort_by(|left, right| {
        (left.byte_len, left.rendered.as_str()).cmp(&(right.byte_len, right.rendered.as_str()))
    });
    prefixes
        .dedup_by(|left, right| left.byte_len == right.byte_len && left.rendered == right.rendered);
    prefixes
}

// Units must strictly decrease within a section (百 then 十) and between
// sections (億 then 万), so readings such as ひゃくひゃく stop at the first
// valid prefix. A large unit needs a nonzero multiplier: まん alone is not
// 10000. Sokuon forms other than the listed ones (いっちょう) match nothing.
fn place_value_prefixes(input: &str) -> Vec<NumericPrefix> {
    let mut result = Vec::new();
    let mut offset = 0;
    let mut total = 0_u64;
    let mut section = 0_u64;
    let mut pending_digit = None;
    let mut last_small_unit = 10_000_u64;
    let mut last_large_unit = u64::MAX;
    let mut atom_count = 0;
    let mut has_magnitude = false;

    while let Some(lexeme) = match_lexeme(&input[offset..], PLACE_LEXEMES) {
        let mut valid = true;
        for event in lexeme.events {
            match *event {
                Event::Digit(value) => {
                    if pending_digit.is_some() {
                        valid = false;
                        break;
                    }
                    pending_digit = Some(value);
                    atom_count += 1;
                }
                Event::SmallUnit(unit) => {
                    if unit >= last_small_unit {
                        valid = false;
                        break;
                    }
                    let multiplier = pending_digit.take().unwrap_or(1);
                    let Some(product) = multiplier.checked_mul(unit) else {
                        valid = false;
                        break;
                    };
                    let Some(next_section) = section.checked_add(product) else {
                        valid = false;
                        break;
                    };
                    section = next_section;
                    last_small_unit = unit;
                    atom_count += usize::from(lexeme.events.len() == 1);
                    has_magnitude = true;
                }
                Event::LargeUnit(unit) => {
                    if unit >= last_large_unit {
                        valid = false;
                        break;
                    }
                    let group = section.saturating_add(pending_digit.take().unwrap_or(0));
                    if group == 0 {
                        valid = false;
                        break;
                    }
                    let Some(product) = group.checked_mul(unit) else {
                        valid = false;
                        break;
                    };
                    let Some(next_total) = total.checked_add(product) else {
                        valid = false;
                        break;
                    };
                    total = next_total;
                    section = 0;
                    last_small_unit = 10_000;
                    last_large_unit = unit;
                    atom_count += 1;
                    has_magnitude = true;
                }
            }
        }
        if !valid {
            break;
        }
        offset += lexeme.reading.len();
        let Some(value) = total
            .checked_add(section)
            .and_then(|value| value.checked_add(pending_digit.unwrap_or(0)))
        else {
            break;
        };
        result.push(NumericPrefix {
            byte_len: offset,
            value,
            rendered: value.to_string(),
            atom_count,
            has_magnitude,
        });
    }
    result
}

fn digit_sequence_prefixes(input: &str) -> Vec<NumericPrefix> {
    let mut result = Vec::new();
    let mut offset = 0;
    let mut value = 0_u64;
    let mut rendered = String::new();
    let mut digits = 0;
    while let Some(lexeme) = match_lexeme(&input[offset..], DIGIT_LEXEMES) {
        let Event::Digit(digit) = lexeme.events[0] else {
            unreachable!("digit lexeme must contain a digit")
        };
        let Some(next_value) = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
        else {
            break;
        };
        value = next_value;
        rendered.push(char::from_digit(digit as u32, 10).expect("single decimal digit"));
        offset += lexeme.reading.len();
        digits += 1;
        result.push(NumericPrefix {
            byte_len: offset,
            value,
            rendered: rendered.clone(),
            atom_count: digits,
            has_magnitude: false,
        });
    }
    result
}

// The surface uses the numeric value, not the digits as read: ぜろさんねん
// becomes "3年", while the plain rewrite keeps "03".
fn add_date_suffixes(input: &str, prefix: NumericPrefix, rewrites: &mut Vec<NumberRewrite>) {
    let tail = &input[prefix.byte_len..];
    let suffix = if tail.starts_with("ねんど") && (1..=9_999).contains(&prefix.value) {
        Some(("ねんど", "年度"))
    } else if tail.starts_with("ねん") && (1..=9_999).contains(&prefix.value) {
        Some(("ねん", "年"))
    } else if tail.starts_with("がつ") && (1..=12).contains(&prefix.value) {
        Some(("がつ", "月"))
    } else if tail.starts_with("にち") && (1..=31).contains(&prefix.value) {
        Some(("にち", "日"))
    } else {
        None
    };
    let Some((suffix_reading, suffix_surface)) = suffix else {
        return;
    };
    push_unique(
        rewrites,
        NumberRewrite {
            byte_len: prefix.byte_len + suffix_reading.len(),
            surface: format!("{}{}", prefix.value, suffix_surface),
            word_cost_bonus: 1_000,
        },
    );
}

fn add_special_dates(input: &str, rewrites: &mut Vec<NumberRewrite>) {
    const SPECIAL: &[(&str, &str)] = &[
        ("にじゅうしちにち", "27日"),
        ("にじゅうくにち", "29日"),
        ("じゅうしちにち", "17日"),
        ("じゅうくにち", "19日"),
    ];
    for (reading, surface) in SPECIAL {
        if input.starts_with(reading) {
            push_unique(
                rewrites,
                NumberRewrite {
                    byte_len: reading.len(),
                    surface: (*surface).to_owned(),
                    word_cost_bonus: 1_000,
                },
            );
        }
    }
}

fn add_compound_dates(input: &str, rewrites: &mut Vec<NumberRewrite>) {
    if !input.contains("がつ") {
        return;
    }

    for (month_len, month) in month_prefixes(input) {
        for (day_len, day) in day_prefixes(&input[month_len..]) {
            push_unique(
                rewrites,
                NumberRewrite {
                    byte_len: month_len + day_len,
                    surface: format!("{month}月{day}日"),
                    word_cost_bonus: 1_000,
                },
            );
        }
    }

    for year in numeric_prefixes(input) {
        if !(1..=9_999).contains(&year.value) {
            continue;
        }
        let tail = &input[year.byte_len..];
        let Some(after_year) = tail.strip_prefix("ねん") else {
            continue;
        };
        let year_len = year.byte_len + "ねん".len();
        for (month_len, month) in month_prefixes(after_year) {
            let month_end = year_len + month_len;
            push_unique(
                rewrites,
                NumberRewrite {
                    byte_len: month_end,
                    surface: format!("{}年{month}月", year.value),
                    word_cost_bonus: 1_000,
                },
            );
            for (day_len, day) in day_prefixes(&after_year[month_len..]) {
                push_unique(
                    rewrites,
                    NumberRewrite {
                        byte_len: month_end + day_len,
                        surface: format!("{}年{month}月{day}日", year.value),
                        word_cost_bonus: 1_000,
                    },
                );
            }
        }
    }
}

fn month_prefixes(input: &str) -> Vec<(usize, u64)> {
    let mut result = numeric_prefixes(input)
        .into_iter()
        .filter_map(|prefix| {
            ((1..=12).contains(&prefix.value) && input[prefix.byte_len..].starts_with("がつ"))
                .then_some((prefix.byte_len + "がつ".len(), prefix.value))
        })
        .collect::<Vec<_>>();
    for (reading, value) in [("しがつ", 4), ("しちがつ", 7), ("くがつ", 9)] {
        if input.starts_with(reading) {
            result.push((reading.len(), value));
        }
    }
    result.sort_unstable();
    result.dedup();
    result
}

fn day_prefixes(input: &str) -> Vec<(usize, u64)> {
    let mut result = numeric_prefixes(input)
        .into_iter()
        .filter_map(|prefix| {
            ((1..=31).contains(&prefix.value) && input[prefix.byte_len..].starts_with("にち"))
                .then_some((prefix.byte_len + "にち".len(), prefix.value))
        })
        .collect::<Vec<_>>();
    const SPECIAL: &[(&str, u64)] = &[
        ("にじゅうしちにち", 27),
        ("にじゅうよっか", 24),
        ("にじゅうくにち", 29),
        ("じゅうしちにち", 17),
        ("じゅうよっか", 14),
        ("じゅうくにち", 19),
        ("ついたち", 1),
        ("ふつか", 2),
        ("みっか", 3),
        ("よっか", 4),
        ("いつか", 5),
        ("むいか", 6),
        ("なのか", 7),
        ("ようか", 8),
        ("ここのか", 9),
        ("とおか", 10),
        ("はつか", 20),
    ];
    for (reading, value) in SPECIAL {
        if input.starts_with(reading) {
            result.push((reading.len(), *value));
        }
    }
    result.sort_unstable();
    result.dedup();
    result
}

fn match_lexeme<'a>(input: &str, lexemes: &'a [Lexeme]) -> Option<&'a Lexeme> {
    lexemes
        .iter()
        .find(|lexeme| input.starts_with(lexeme.reading))
}

fn push_unique(rewrites: &mut Vec<NumberRewrite>, rewrite: NumberRewrite) {
    if !rewrites.iter().any(|existing| existing == &rewrite) {
        rewrites.push(rewrite);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surfaces(input: &str) -> Vec<(usize, String)> {
        rewrites(input)
            .into_iter()
            .map(|rewrite| (rewrite.byte_len, rewrite.surface))
            .collect()
    }

    #[test]
    fn rewrites_place_values_and_digit_sequences() {
        for (reading, expected) in [
            ("にじゅうなな", "27"),
            ("にせんにじゅうろく", "2026"),
            ("ぜろさん", "03"),
            ("きゅうさんねんど", "93年度"),
            ("にひゃくろくじゅうご", "265"),
            ("ごひゃくろくじゅうご", "565"),
            ("さんびゃくろくじゅうご", "365"),
            ("いちまんにせんさんびゃくよんじゅうご", "12345"),
        ] {
            assert!(surfaces(reading).contains(&(reading.len(), expected.to_owned())));
        }
    }

    #[test]
    fn rewrites_regular_and_irregular_dates() {
        for (reading, expected) in [
            ("にじゅうしちにち", "27日"),
            ("じゅうくにち", "19日"),
            ("にじゅうよんにち", "24日"),
            ("しがついつか", "4月5日"),
            ("にせんにじゅうろくねんしがついつか", "2026年4月5日"),
        ] {
            assert!(surfaces(reading).contains(&(reading.len(), expected.to_owned())));
        }
    }

    #[test]
    fn rejects_single_digits_and_invalid_calendar_values() {
        assert!(!surfaces("にほん").iter().any(|(_, surface)| surface == "2"));
        assert!(surfaces("いつか").is_empty());
        assert!(surfaces("おくさん").is_empty());
        assert!(
            !surfaces("じゅうさんがつ")
                .iter()
                .any(|(_, surface)| surface == "13月")
        );
        assert!(
            !surfaces("さんじゅうににち")
                .iter()
                .any(|(_, surface)| surface == "32日")
        );
    }
}
