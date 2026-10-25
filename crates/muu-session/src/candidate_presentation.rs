//! Candidate catalog presentation after stages ① and ②. Moving a candidate
//! never changes its ID, costs or actual paths. The first two entries preserve
//! the one-Space return from the LM choice to the generation choice (docs/src/internals/conversion.md「候補一覧の見え方」).
use muu_digest::{hex_lower as hex, sha256_hex};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use muu_conversion::{Candidate, Segment, Segments};
use serde::Deserialize;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

const POLICY: &str = include_str!("../data/candidate-presentation.toml");
const SYMBOLS: &str = include_str!("../../../third_party/mozc-oss/src/data/symbol/symbol.tsv");

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PresentationMetadata {
    rules_version: u32,
    policy_sha256: String,
    auxiliary_data: Vec<AuxiliaryDataMetadata>,
}

#[derive(Clone, Debug, Serialize)]
struct AuxiliaryDataMetadata {
    kind: &'static str,
    data_sha256: String,
    source_sha256: String,
}

pub(crate) fn metadata() -> PresentationMetadata {
    PresentationMetadata {
        rules_version: 1,
        // The policy text is hashed here. The table digests below were
        // computed by auxiliary_data over its own canonical bytes; `hex`
        // only spells them.
        policy_sha256: sha256_hex(POLICY.as_bytes()),
        auxiliary_data: crate::auxiliary_data::catalogs()
            .iter()
            .map(|table| AuxiliaryDataMetadata {
                kind: table.kind.stable_name(),
                data_sha256: hex(&table.data_sha256),
                source_sha256: hex(&table.source_sha256),
            })
            .collect(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuxiliaryRule {
    kind: String,
    all_reading: Option<String>,
    position: Option<usize>,
}

// data/candidate-presentation.toml. Positions are 1-based and must lie after
// the protected prefix (at least 2), so promotion can never move the stage ②
// choice or the generation choice out of the first two slots.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    schema_version: u32,
    protected_prefix: usize,
    symbols: Vec<SymbolRule>,
    auxiliaries: Vec<AuxiliaryRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolRule {
    reading: String,
    position: usize,
    limit: Option<usize>,
}

struct Presentation {
    policy: Policy,
    surfaces: BTreeMap<String, BTreeSet<String>>,
}

impl Presentation {
    fn load() -> Result<Self, String> {
        let policy: Policy = toml::from_str(POLICY).map_err(|error| error.to_string())?;
        let mut readings = BTreeSet::new();
        if policy.schema_version != 1
            || policy.protected_prefix < 2
            || policy.auxiliaries.iter().any(|rule| {
                !crate::auxiliary_data::catalogs()
                    .iter()
                    .any(|table| table.kind.stable_name() == rule.kind)
                    || rule.position.is_some_and(|p| p <= policy.protected_prefix)
                    || rule.all_reading.as_ref().is_some_and(String::is_empty)
            })
            || policy.symbols.iter().any(|rule| {
                rule.reading.is_empty()
                    || rule.position <= policy.protected_prefix
                    || rule.limit == Some(0)
                    || !readings.insert(rule.reading.as_str())
            })
        {
            return Err("invalid candidate presentation policy".into());
        }
        let mut surfaces = BTreeMap::<String, BTreeSet<String>>::new();
        let mut seen = BTreeSet::new();
        // Match the existing ETL's first-row-per-NFC-surface rule. All nine
        // configured keys are hiragana, so its ASCII projection is irrelevant
        // here; this code does not introduce a second general reading codec.
        for line in SYMBOLS.lines().skip(1) {
            let columns = line.split('\t').collect::<Vec<_>>();
            if columns.len() < 3 {
                continue;
            }
            let surface: String = columns[1].nfc().collect();
            if surface.is_empty() || !seen.insert(surface.clone()) {
                continue;
            }
            for reading in columns[2].split(' ') {
                if readings.contains(reading) {
                    surfaces
                        .entry(reading.to_owned())
                        .or_default()
                        .insert(surface.clone());
                }
            }
        }
        if readings
            .iter()
            .any(|reading| !surfaces.contains_key(*reading))
        {
            return Err("symbol presentation reading missing from pinned source".into());
        }
        Ok(Self { policy, surfaces })
    }

    // Returns a reordered and possibly extended catalog for one segment, or
    // None when nothing changes. Symbol rules move existing candidates whose
    // surface is a pinned Mozc symbol for this exact reading up to their
    // position (counting any already visible in the prefix against the
    // limit); auxiliary rules then add new display-only candidates.
    fn segment(&self, reading: &str, segment: &Segment, auxiliary: bool) -> Option<Segment> {
        let current = segment.candidates();
        // Navigation never rebuilds a catalog. Resize replaces affected spans
        // with ordinary generation; an untouched augmented span is kept whole,
        // including its selected auxiliary and its original boundary context.
        if current
            .iter()
            .any(|candidate| candidate.auxiliary().is_some())
        {
            return None;
        }
        let prefix = current.len().min(self.policy.protected_prefix);
        let symbol_rule = self.policy.symbols.iter().find(|r| r.reading == reading);
        if symbol_rule.is_none()
            && (!auxiliary
                || !self.policy.auxiliaries.iter().any(|rule| {
                    rule.all_reading.as_deref() == Some(reading)
                        || crate::auxiliary_data::catalogs().iter().any(|table| {
                            table.kind.stable_name() == rule.kind
                                && !table.for_reading(reading).is_empty()
                        })
                }))
        {
            return None;
        }
        let symbols = self.surfaces.get(reading);
        let visible = current[..prefix]
            .iter()
            .filter(|candidate| symbols.is_some_and(|set| set.contains(candidate.surface())))
            .count();
        let available = symbol_rule.map_or(0, |rule| {
            rule.limit.unwrap_or(usize::MAX).saturating_sub(visible)
        });
        let mut promoted = Vec::new();
        let mut remaining = Vec::new();
        for candidate in &current[prefix..] {
            if promoted.len() < available
                && symbols.is_some_and(|set| set.contains(candidate.surface()))
            {
                promoted.push(candidate.clone());
            } else {
                remaining.push(candidate.clone());
            }
        }
        let promoted_count = promoted.len();
        let insertion =
            symbol_rule.map_or(0, |rule| (rule.position - 1 - prefix).min(remaining.len()));
        let mut candidates = current[..prefix].to_vec();
        candidates.extend(remaining.drain(..insertion));
        candidates.extend(promoted);
        candidates.extend(remaining);
        if auxiliary {
            // Each `?` below discards the whole rewrite of this segment,
            // symbol promotion included, rather than showing part of it.
            // load() has already checked that every rule names a table.
            // Share a single retained normal path for this span. It serves
            // only local resize context; the new candidates have no real path.
            let boundary = Arc::new(segment.focused().paths().first()?.clone());
            let mut seen = current
                .iter()
                .map(|c| c.surface().to_owned())
                .collect::<BTreeSet<_>>();
            let mut inserted = 0;
            for rule in &self.policy.auxiliaries {
                let table = crate::auxiliary_data::catalogs()
                    .iter()
                    .find(|t| t.kind.stable_name() == rule.kind)?;
                let entries = if rule.all_reading.as_deref() == Some(reading) {
                    table.entries.as_slice()
                } else {
                    table.for_reading(reading)
                };
                let mut added = Vec::new();
                for entry in entries {
                    // Existing actual paths win over an NFC-equivalent display
                    // entry; duplicate auxiliary tables also have one surface.
                    if seen.insert(entry.surface().to_owned()) {
                        added.push(
                            Candidate::from_auxiliary(
                                segment.reading_range(),
                                entry.clone(),
                                boundary.clone(),
                            )
                            .ok()?,
                        );
                    }
                }
                // Positioned groups follow the promoted symbols, in policy
                // order. Appending can take the list past the lattice's
                // MAX_CANDIDATE_SURFACES; no later stage limits the count.
                if let Some(position) = rule.position {
                    let at = (position - 1 + promoted_count + inserted).min(candidates.len());
                    inserted += added.len();
                    candidates.splice(at..at, added);
                } else {
                    candidates.extend(added);
                }
            }
        }
        if candidates
            .iter()
            .map(|c| c.id())
            .eq(current.iter().map(|c| c.id()))
        {
            return None;
        }
        let focused = candidates
            .iter()
            .position(|c| c.id() == segment.focused().id())?;
        Segment::new(
            segment.reading_range(),
            candidates,
            focused,
            segment.state(),
        )
        .ok()
    }
}

/// This consumes the result without cloning it on the overwhelmingly common
/// nonmatching readings. A construction failure preserves the original result.
pub(crate) fn present_candidates(segments: Segments) -> Segments {
    present(segments, true)
}

static PRESENTATION: OnceLock<Result<Presentation, String>> = OnceLock::new();

pub(crate) fn prepare() {
    // Core initialization runs on the host's startup worker. Parse and index
    // this immutable resource there, not on the first Space key on main.
    let _ = PRESENTATION.get_or_init(Presentation::load);
}

fn present(segments: Segments, auxiliary: bool) -> Segments {
    let Ok(presentation) = PRESENTATION.get_or_init(Presentation::load) else {
        return segments;
    };
    let reading = segments.reading().chars().collect::<Vec<_>>();
    let mut changed = None;
    for (index, segment) in segments.segments().iter().enumerate() {
        let range = segment.reading_range();
        let segment_reading = reading[range.start.get()..range.end.get()]
            .iter()
            .collect::<String>();
        if let Some(replacement) = presentation.segment(&segment_reading, segment, auxiliary) {
            changed.get_or_insert_with(|| segments.segments().to_vec())[index] = replacement;
        }
    }
    let Some(changed) = changed else {
        return segments;
    };
    Segments::new(
        segments.reading().to_owned(),
        changed,
        segments.focused_segment(),
    )
    .unwrap_or(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::conversion_candidate;
    use muu_conversion::{ScalarRange, SegmentState};
    use std::sync::Arc;

    fn attach_auxiliary(
        session: &mut crate::Session,
        kind: muu_conversion::AuxiliaryKind,
        surface: &str,
    ) -> String {
        use muu_conversion::{AuxiliaryEntry, Candidate};
        let current = session.segments.take().unwrap();
        let mut segments = current.segments().to_vec();
        let old = &segments[current.focused_segment()];
        let auxiliary = Candidate::from_auxiliary(
            old.reading_range(),
            Arc::new(AuxiliaryEntry::new(kind, [0x3a; 32], 7, surface).unwrap()),
            Arc::new(old.focused().paths()[0].clone()),
        )
        .unwrap();
        assert_eq!(auxiliary.cost(), None);
        assert!(auxiliary.paths().is_empty());
        let id = auxiliary.id().as_str().to_owned();
        let mut candidates = old.candidates().to_vec();
        candidates.push(auxiliary);
        segments[current.focused_segment()] = Segment::new(
            old.reading_range(),
            candidates,
            old.focused_candidate(),
            old.state(),
        )
        .unwrap();
        session.segments = Some(
            Segments::new(
                current.reading().to_owned(),
                segments,
                current.focused_segment(),
            )
            .unwrap(),
        );
        id
    }

    fn fixture(reading: &str, surfaces: &[&str], focus: usize) -> Segments {
        let range = ScalarRange::new(
            muu_conversion::ScalarIndex::new(0),
            muu_conversion::ScalarIndex::new(reading.chars().count()),
        );
        let candidates = surfaces
            .iter()
            .enumerate()
            .map(|(i, surface)| conversion_candidate(range, surface, i as i32 * 10, i as u64))
            .collect();
        Segments::new(
            reading.to_owned(),
            vec![Segment::new(range, candidates, focus, SegmentState::Unconfirmed).unwrap()],
            0,
        )
        .unwrap()
    }

    // The policy carries exactly the nine configured symbol readings and a
    // protected prefix of two; the symbol sets come from the pinned Mozc file
    // (★ for ほし, ※ for こめ), and no other reading gets one.
    #[test]
    fn pinned_policy_and_sources_keep_the_nine_configured_readings() {
        // Pin comment bytes separately from all parsed values: changing a
        // provenance label must not change candidate placement or aliases.
        assert_eq!(
            metadata().policy_sha256,
            "37b929ab54345360c6af7f400d658d2e7d5e446d858858aa80d44601a7f6d4e5"
        );
        assert_eq!(
            serde_json::to_value(toml::from_str::<toml::Value>(POLICY).unwrap()).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "protected_prefix": 2,
                "auxiliaries": [
                    {"kind": "emoticon", "all_reading": "かおもじ", "position": 3},
                    {"kind": "emoji", "all_reading": "えもじ", "position": 3},
                    {"kind": "single_kanji"}
                ],
                "symbols": [
                    {"reading": "きごう", "position": 3},
                    {"reading": "やじるし", "position": 3, "limit": 3},
                    {"reading": "ほし", "position": 3, "limit": 3},
                    {"reading": "まる", "position": 3, "limit": 3},
                    {"reading": "さんかく", "position": 3, "limit": 3},
                    {"reading": "しかく", "position": 3, "limit": 3},
                    {"reading": "おんぷ", "position": 3, "limit": 3},
                    {"reading": "こめ", "position": 3, "limit": 1},
                    {"reading": "はーと", "position": 3, "limit": 3}
                ]
            })
        );
        let p = Presentation::load().unwrap();
        assert_eq!(p.policy.symbols.len(), 9);
        assert_eq!(p.policy.protected_prefix, 2);
        assert!(p.surfaces["ほし"].contains("★"));
        assert!(p.surfaces["こめ"].contains("※"));
        assert!(!p.surfaces.contains_key("あ"));
    }

    #[test]
    fn all_emoticons_aliases_and_exact_single_kanji_are_reachable_without_truncation() {
        let original = fixture("かおもじ", &["顔文字", "かおもじ"], 0);
        let shown = present_candidates(original.clone());
        let list = shown.focused().unwrap().candidates();
        assert_eq!(list.len(), 439);
        assert_eq!(&list[..2], &original.focused().unwrap().candidates()[..2]);
        assert!(
            list[2..]
                .iter()
                .all(|c| c.auxiliary().unwrap().kind() == muu_conversion::AuxiliaryKind::Emoticon)
        );
        assert_eq!(present_candidates(shown.clone()), shown);
        assert!(
            present_candidates(fixture("かお", &["顔", "かお"], 0))
                .focused()
                .unwrap()
                .candidates()
                .len()
                < 437
        );
        let table = &crate::auxiliary_data::catalogs()[0];
        for entry in &table.entries {
            assert!(list.iter().any(|c| c.surface() == entry.surface()));
        }
        for (reading, entries) in [
            ("あ", crate::auxiliary_data::catalogs()[1].for_reading("あ")),
            (
                "きょう",
                crate::auxiliary_data::catalogs()[1].for_reading("きょう"),
            ),
        ] {
            let base = fixture(reading, &["普通", "別案"], 0);
            let shown = present_candidates(base.clone());
            assert_eq!(
                &shown.focused().unwrap().candidates()[..2],
                base.focused().unwrap().candidates()
            );
            assert!(!entries.is_empty());
            for entry in entries {
                assert!(
                    shown
                        .focused()
                        .unwrap()
                        .candidates()
                        .iter()
                        .any(|c| c.surface() == entry.surface())
                );
            }
        }
        let unmapped = fixture("たんかんじ", &["単漢字", "たんかんじ"], 0);
        assert_eq!(present_candidates(unmapped.clone()), unmapped);
    }

    #[test]
    fn existing_nfc_surface_keeps_its_real_path_and_no_duplicate_is_synthesized() {
        let source = fixture("かおもじ", &["顔文字", "かおもじ", "m(_ _)m"], 0);
        let shown = present_candidates(source.clone());
        let original = &source.focused().unwrap().candidates()[2];
        let matches = shown
            .focused()
            .unwrap()
            .candidates()
            .iter()
            .filter(|c| c.surface() == "m(_ _)m")
            .collect::<Vec<_>>();
        assert_eq!(matches, vec![original]);
        assert!(matches[0].auxiliary().is_none());
        assert_eq!(matches[0].paths(), original.paths());
    }

    #[test]
    fn emoji_catalog_retains_all_pinned_sequences_and_the_protected_choices() {
        let original = fixture("えもじ", &["絵文字", "えもじ"], 0);
        let shown = present_candidates(original.clone());
        let list = shown.focused().unwrap().candidates();
        assert_eq!(&list[..2], original.focused().unwrap().candidates());
        assert_eq!(list.len(), 1920);
        let table = crate::auxiliary_data::catalogs()
            .iter()
            .find(|t| t.kind == muu_conversion::AuxiliaryKind::Emoji)
            .unwrap();
        assert_eq!(
            list[2..].iter().map(|c| c.surface()).collect::<Vec<_>>(),
            table
                .entries
                .iter()
                .map(|e| e.surface())
                .collect::<Vec<_>>()
        );
        assert!(list.iter().any(|c| c.surface().contains('\u{200d}')));
        assert!(list.iter().any(|c| c.surface().contains('\u{fe0f}')));
        assert_eq!(present_candidates(shown.clone()), shown);
        let view = crate::view::view_from_segments(&shown, true);
        assert_eq!(view.candidates.as_ref().unwrap().items.len(), 1920);
        let mut last = shown;
        let id = last
            .focused()
            .unwrap()
            .candidates()
            .last()
            .unwrap()
            .id()
            .clone();
        last.select_focused_candidate_by_id(&id).unwrap();
        let view = crate::view::view_from_segments(&last, true);
        let chosen = last.focused().unwrap().focused().surface();
        assert_eq!(view.preedit[0].text, chosen);
        assert_eq!(view.preedit[0].utf16_len, chosen.encode_utf16().count());
        assert_eq!(view.candidates.as_ref().unwrap().selected_index, 1919);
    }

    #[test]
    fn mixed_commit_learns_only_the_normal_span_and_navigation_preserves_catalogs() {
        use crate::test_support::*;
        let dictionary = commit_stream_dictionary();
        let mut session = commit_stream_session(&dictionary);
        for c in "kyoukyou".chars() {
            session.handle(key(&c.to_string(), 0));
        }
        session.handle(key(" ", 49));
        while session.segments.as_ref().unwrap().segments()[0]
            .reading_range()
            .len()
            > 3
        {
            session.resize_segment(-1);
        }
        assert_eq!(session.segments.as_ref().unwrap().segments().len(), 2);
        let auxiliary = session.segments.as_ref().unwrap().segments()[0]
            .candidates()
            .iter()
            .find(|c| c.auxiliary().is_some())
            .unwrap()
            .clone();
        session.handle(command(
            "select_candidate",
            serde_json::json!({"id":auxiliary.id().as_str()}),
        ));
        let first = session.segments.as_ref().unwrap().segments()[0].clone();
        session.move_segment(1);
        let normal = session.segments.as_ref().unwrap().segments()[1]
            .candidates()
            .iter()
            .find(|c| c.surface() == "京")
            .unwrap()
            .clone();
        let tokens = normal.paths()[0]
            .token_keys()
            .filter_map(|key| match key {
                muu_conversion::TokenKey::Dictionary(id) => Some(id.get()),
                _ => None,
            })
            .collect::<Vec<_>>();
        session.handle(command(
            "select_candidate",
            serde_json::json!({"id":normal.id().as_str()}),
        ));
        let second = session.segments.as_ref().unwrap().segments()[1].clone();
        session.move_segment(-1);
        session.move_segment(1);
        assert_eq!(
            session.segments.as_ref().unwrap().segments(),
            &[first, second]
        );
        let committed = session.handle(key("\r", 36));
        assert_eq!(
            committed.commit_text,
            Some(format!("{}京", auxiliary.surface()))
        );
        let update = session
            .core
            .personal_frequency_update(None)
            .unwrap()
            .unwrap();
        let state: crate::PersonalFrequencySnapshot = serde_json::from_slice(&update.data).unwrap();
        assert_eq!(state.observation_count, 1);
        assert_eq!(
            state
                .entries
                .iter()
                .map(|e| e.dictionary_token_id)
                .collect::<Vec<_>>(),
            tokens
        );
        let event: serde_json::Value =
            serde_json::from_slice(&session.core.take_commit_stream_event().unwrap()).unwrap();
        assert_eq!(
            event["final"]["segments"][0]["surface"],
            auxiliary.surface()
        );
        assert_eq!(event["final"]["segments"][1]["surface"], "京");
        assert_eq!(event["final"]["segments"][0]["explicitly_selected"], true);
        assert_eq!(event["final"]["segments"][1]["explicitly_selected"], true);
    }

    #[test]
    fn promotion_keeps_choices_ids_costs_paths_and_normal_relative_order() {
        let original = fixture("ほし", &["星", "干し", "欲し", "★", "☆", "※", "✡"], 0);
        let shown = present(original.clone(), false);
        assert_eq!(
            shown
                .focused()
                .unwrap()
                .candidates()
                .iter()
                .map(|c| c.surface())
                .collect::<Vec<_>>(),
            ["星", "干し", "★", "☆", "※", "欲し", "✡"]
        );
        for candidate in shown.focused().unwrap().candidates() {
            let old = original
                .focused()
                .unwrap()
                .candidates()
                .iter()
                .find(|c| c.id() == candidate.id())
                .unwrap();
            assert_eq!(candidate, old);
        }
        assert_eq!(
            shown.focused().unwrap().focused().id(),
            original.focused().unwrap().focused().id()
        );
        assert_eq!(present(shown.clone(), false), shown);
    }

    #[test]
    fn an_already_visible_symbol_counts_towards_the_limit_without_demotion() {
        let original = fixture("ほし", &["星", "★", "欲し", "☆", "※", "✡"], 1);
        let shown = present(original, false);
        assert_eq!(
            shown
                .focused()
                .unwrap()
                .candidates()
                .iter()
                .map(|c| c.surface())
                .collect::<Vec<_>>(),
            ["星", "★", "☆", "※", "欲し", "✡"]
        );
        assert_eq!(shown.focused().unwrap().focused().surface(), "★");
        assert_eq!(shown.focused().unwrap().focused_candidate(), 1);
        let untouched = fixture("あ", &["亜", "阿", "ア", "ぁ"], 0);
        assert_eq!(present(untouched.clone(), false), untouched);
    }

    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_symbols_move_while_generation_choices_and_paths_stay_identical() {
        let Some(path) = crate::real_assets::real_asset_path("MUU_REAL_DICT_PATH") else {
            return;
        };
        let dictionary = crate::real_dictionary::shared_at(&path);
        let snapshot = muu_conversion::CostSnapshot::baseline(&dictionary);
        for (reading, symbol) in [("ほし", "★"), ("しかく", "■"), ("こめ", "※")] {
            let generated = muu_conversion::convert_to_segments(
                reading,
                &crate::default_segmenter(),
                &snapshot,
            )
            .unwrap();
            let shown = present(generated.clone(), false);
            assert_eq!(shown.focused().unwrap().candidates()[2].surface(), symbol);
            assert_eq!(
                &shown.focused().unwrap().candidates()[..2],
                &generated.focused().unwrap().candidates()[..2]
            );
            for candidate in shown.focused().unwrap().candidates() {
                assert!(
                    generated
                        .focused()
                        .unwrap()
                        .candidates()
                        .contains(candidate)
                );
            }
        }
    }

    #[test]
    fn auxiliary_identity_is_typed_without_a_fake_path_cost_or_unsafe_surface() {
        use muu_conversion::{AuxiliaryEntry, AuxiliaryKind, Candidate, ScalarIndex};
        let ordinary = fixture("ほし", &["星", "干し"], 0);
        let first = ordinary.focused().unwrap().focused();
        let entry =
            Arc::new(AuxiliaryEntry::new(AuxiliaryKind::Emoji, [1; 32], 2, "か\u{3099}").unwrap());
        let anchor = Arc::new(first.paths()[0].clone());
        let auxiliary =
            Candidate::from_auxiliary(first.reading_range(), entry.clone(), anchor.clone())
                .unwrap();
        assert_eq!(auxiliary.surface(), "が");
        assert_eq!(auxiliary.cost(), None);
        assert!(auxiliary.paths().is_empty());
        assert_ne!(auxiliary.id(), first.id());
        assert_eq!(
            auxiliary,
            Candidate::from_auxiliary(first.reading_range(), entry.clone(), anchor.clone())
                .unwrap()
        );
        assert!(
            Candidate::from_auxiliary(
                ScalarRange::new(ScalarIndex::new(0), ScalarIndex::new(1)),
                entry,
                anchor
            )
            .is_err()
        );
        for surface in ["", "x\ny", "x\0y"] {
            assert!(AuxiliaryEntry::new(AuxiliaryKind::Emoticon, [1; 32], 0, surface).is_err());
        }
        assert!(
            AuxiliaryEntry::new(
                AuxiliaryKind::Emoticon,
                [1; 32],
                0,
                &"\u{0344}".repeat(2048)
            )
            .is_err()
        );
    }

    #[test]
    fn auxiliary_explicit_commit_records_its_identity_and_neither_rewards_nor_rejects() {
        use crate::test_support::*;
        use muu_conversion::AuxiliaryKind;
        for (kind, surface) in [
            (AuxiliaryKind::Emoticon, "m(_ _)m"),
            (AuxiliaryKind::Emoji, "👩‍💻"),
            (AuxiliaryKind::SingleKanji, "凜"),
        ] {
            let dictionary = commit_stream_dictionary();
            let mut session = commit_stream_session(&dictionary);
            for c in "kyou".chars() {
                session.handle(key(&c.to_string(), 0));
            }
            session.handle(key(" ", 49));
            let id = attach_auxiliary(&mut session, kind, surface);
            let selected =
                session.handle(command("select_candidate", serde_json::json!({"id": id})));
            assert_eq!(
                selected
                    .preedit
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<String>(),
                surface
            );
            let after_move = session.handle(command(
                "move_segment",
                serde_json::json!({"direction":"left"}),
            ));
            assert_eq!(after_move.preedit, selected.preedit);
            assert_eq!(after_move.candidates, selected.candidates);
            assert_eq!(
                session.handle(key("\r", 36)).commit_text.as_deref(),
                Some(surface)
            );
            let update = session
                .core
                .personal_frequency_update(None)
                .unwrap()
                .unwrap();
            let learning: crate::PersonalFrequencySnapshot =
                serde_json::from_slice(&update.data).unwrap();
            assert_eq!(learning.observation_count, 0);
            assert!(learning.entries.is_empty());
            assert!(learning.rejected_entries.is_empty());
            let event: serde_json::Value =
                serde_json::from_slice(&session.core.take_commit_stream_event().unwrap()).unwrap();
            assert_eq!(event["schema"], "muu.commit-stream.v5");
            assert_eq!(event["candidate_format_version"], 2);
            assert_eq!(event["initial"]["selection_report"]["format_version"], 3);
            assert_eq!(
                event["initial"]["selected_segments"][0]["auxiliary"],
                serde_json::Value::Null
            );
            let final_segment = &event["final"]["segments"][0];
            assert_eq!(final_segment["candidate_id"], id);
            assert_eq!(final_segment["surface"], surface);
            assert_eq!(final_segment["path"], serde_json::Value::Null);
            assert_eq!(
                final_segment["origin_layers"],
                serde_json::json!(["auxiliary"])
            );
            assert_eq!(final_segment["auxiliary"]["kind"], kind.stable_name());
            assert_eq!(final_segment["auxiliary"]["entry_id"], 7);
            assert_eq!(final_segment["auxiliary"]["data_sha256"], "3a".repeat(32));
            assert_eq!(final_segment["explicitly_selected"], true);
            assert_eq!(event["final"]["candidate_selected_explicitly"], true);
            assert_eq!(event["final"]["surface"], surface);
        }
    }

    #[test]
    fn auxiliary_resizing_uses_only_retained_boundary_context_and_preserves_outside_choice() {
        use crate::test_support::*;
        use muu_conversion::{AuxiliaryKind, CostSnapshot, ScalarIndex};
        let dictionary = conversion_dictionary();
        let mut session = session_with_dictionary_path(Some(dictionary.path())).unwrap();
        let reading = "きょうきょうきょう";
        let segments = (0..3)
            .map(|i| {
                let range =
                    ScalarRange::new(ScalarIndex::new(i * 3), ScalarIndex::new((i + 1) * 3));
                Segment::new(
                    range,
                    vec![conversion_candidate(range, "今日", 0, 0)],
                    0,
                    SegmentState::Unconfirmed,
                )
                .unwrap()
            })
            .collect();
        session.segments = Some(Segments::new(reading.into(), segments, 0).unwrap());
        let original = session.segments.clone().unwrap();
        let id = attach_auxiliary(&mut session, AuxiliaryKind::Emoji, "⭐");
        session
            .segments
            .as_mut()
            .unwrap()
            .select_focused_candidate_by_id(&muu_conversion::CandidateId::new(id).unwrap())
            .unwrap();
        session.segments.as_mut().unwrap().move_focus(1).unwrap();
        let with_aux = session.segments.as_ref().unwrap();
        let snapshot = CostSnapshot::baseline(session.core.dictionary.as_ref().unwrap());
        let rewritten = muu_conversion::resize_focused_segment(with_aux, 2, &snapshot).unwrap();
        let mut without_aux = original;
        without_aux.move_focus(1).unwrap();
        let reference = muu_conversion::resize_focused_segment(&without_aux, 2, &snapshot).unwrap();
        assert_eq!(rewritten.segments()[0], with_aux.segments()[0]);
        assert_eq!(rewritten.segments()[0].focused().surface(), "⭐");
        assert_eq!(&rewritten.segments()[1..], &reference.segments()[1..]);
    }
}
