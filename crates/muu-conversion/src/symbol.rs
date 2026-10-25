const UP: &[&str] = &["↑"];
const DOWN: &[&str] = &["↓"];
const LEFT: &[&str] = &["←"];
const RIGHT: &[&str] = &["→"];
const ARROWS: &[&str] = &["↑", "↓", "←", "→"];

// Keep symbols in the first page without displacing the ordinary lexical
// winner (for example, 上 for うえ) in the production dictionary.
pub(crate) const WORD_COST_BONUS: i32 = 10_000;

/// Returns symbol candidates only when the complete conversion span matches.
/// The span is the lattice's whole reading, so a segment that is exactly
/// `うえ` gets ↑ when its span is searched on its own.
///
/// Direction words are ordinary vocabulary too. Restricting the rewrite to an
/// exact span keeps phrases such as `うえの` on the dictionary path while still
/// exposing symbols when the user converts the direction by itself.
pub(crate) fn surfaces(reading: &str) -> &'static [&'static str] {
    match reading {
        "うえ" => UP,
        "した" => DOWN,
        "ひだり" => LEFT,
        "みぎ" => RIGHT,
        "やじるし" => ARROWS,
        _ => &[],
    }
}

/// Returns the ASCII alternative for one canonical kana-mode punctuation mark.
/// The keys are what the kana composer writes for the 32 ASCII symbols.
pub(crate) fn ascii_variant(reading: &str) -> Option<&'static str> {
    match reading {
        "！" => Some("!"),
        "＂" => Some("\""),
        "＃" => Some("#"),
        "＄" => Some("$"),
        "％" => Some("%"),
        "＆" => Some("&"),
        "＇" => Some("'"),
        "（" => Some("("),
        "）" => Some(")"),
        "＊" => Some("*"),
        "＋" => Some("+"),
        "、" => Some(","),
        "ー" => Some("-"),
        "。" => Some("."),
        "・" => Some("/"),
        "：" => Some(":"),
        "；" => Some(";"),
        "＜" => Some("<"),
        "＝" => Some("="),
        "＞" => Some(">"),
        "？" => Some("?"),
        "＠" => Some("@"),
        "「" => Some("["),
        "＼" => Some("\\"),
        "」" => Some("]"),
        "＾" => Some("^"),
        "＿" => Some("_"),
        "｀" => Some("`"),
        "｛" => Some("{"),
        "｜" => Some("|"),
        "｝" => Some("}"),
        "～" => Some("~"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{ascii_variant, surfaces};

    #[test]
    fn maps_cardinal_directions_and_generic_arrow_reading() {
        assert_eq!(surfaces("うえ"), &["↑"]);
        assert_eq!(surfaces("した"), &["↓"]);
        assert_eq!(surfaces("ひだり"), &["←"]);
        assert_eq!(surfaces("みぎ"), &["→"]);
        assert_eq!(surfaces("やじるし"), &["↑", "↓", "←", "→"]);
    }

    #[test]
    fn rejects_prefix_and_unrelated_readings() {
        assert!(surfaces("うえの").is_empty());
        assert!(surfaces("したがう").is_empty());
        assert!(surfaces("みぎて").is_empty());
    }

    #[test]
    fn maps_every_canonical_kana_punctuation_to_ascii() {
        let variants = [
            ("！", "!"),
            ("＂", "\""),
            ("＃", "#"),
            ("＄", "$"),
            ("％", "%"),
            ("＆", "&"),
            ("＇", "'"),
            ("（", "("),
            ("）", ")"),
            ("＊", "*"),
            ("＋", "+"),
            ("、", ","),
            ("ー", "-"),
            ("。", "."),
            ("・", "/"),
            ("：", ":"),
            ("；", ";"),
            ("＜", "<"),
            ("＝", "="),
            ("＞", ">"),
            ("？", "?"),
            ("＠", "@"),
            ("「", "["),
            ("＼", "\\"),
            ("」", "]"),
            ("＾", "^"),
            ("＿", "_"),
            ("｀", "`"),
            ("｛", "{"),
            ("｜", "|"),
            ("｝", "}"),
            ("～", "~"),
        ];
        assert_eq!(variants.len(), 32);
        for (canonical, ascii) in variants {
            assert_eq!(ascii_variant(canonical), Some(ascii));
        }
        assert_eq!(ascii_variant("あ"), None);
    }
}
