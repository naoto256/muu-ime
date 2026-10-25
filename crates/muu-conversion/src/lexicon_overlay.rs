use std::collections::BTreeMap;

use muu_dict::{LeftContextId, PosId, RightContextId};
use muu_digest::sha256_hex;

/// Identifies the provenance of a runtime lexicon entry without coupling the
/// conversion core to any persistence format.
///
/// The discriminants are wire tags and never change. Order comes from
/// `authority_rank`, not from the tags or the declaration order, so adding or
/// reordering a variant cannot silently change which source wins.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum LexiconOverlayTier {
    User = 1,
    Imported = 2,
    PostalCode = 3,
}

impl LexiconOverlayTier {
    // These ranks encode source authority; repr tags identify stored
    // tokens and fingerprints and must not decide operational ordering.
    const fn authority_rank(self) -> u8 {
        match self {
            Self::User => 0,
            Self::Imported => 1,
            Self::PostalCode => 2,
        }
    }

    /// Explicit input sources outrank less-direct sources. Built-in dictionary
    /// entries sit below every overlay and therefore have no enum variant.
    pub const fn outranks(self, other: Self) -> bool {
        self.authority_rank() < other.authority_rank()
    }

    pub fn at_least_as_authoritative_as(self, other: Self) -> bool {
        self == other || self.outranks(other)
    }
}

// Written by hand rather than derived, so that sorting by tier follows
// authority (User first) and not the declaration order.
impl Ord for LexiconOverlayTier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.authority_rank().cmp(&other.authority_rank())
    }
}

impl PartialOrd for LexiconOverlayTier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexiconOverlayEntry {
    pub tier: LexiconOverlayTier,
    /// Stable opaque provenance for one named imported dictionary. Built-in
    /// user/postal tiers use zero.
    pub dictionary_key: u64,
    pub entry_id: u32,
    pub form_id: u16,
    pub reading: String,
    pub surface: String,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos_id: PosId,
    pub right_pos_id: PosId,
    pub word_cost: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexiconSuppression {
    pub tier: LexiconOverlayTier,
    pub reading: Option<String>,
    pub surface: Option<String>,
}

impl LexiconSuppression {
    fn matches(&self, reading: &str, surface: &str) -> bool {
        self.reading
            .as_deref()
            .is_none_or(|expected| readings_match(expected, reading))
            && self
                .surface
                .as_deref()
                .is_none_or(|expected| expected == surface)
    }
}

/// Immutable, deterministic runtime lexicon snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexiconOverlay {
    by_reading: BTreeMap<String, Vec<LexiconOverlayEntry>>,
    suppressions: Vec<LexiconSuppression>,
    fingerprint: OverlayFingerprint,
}

impl Default for LexiconOverlay {
    fn default() -> Self {
        Self::with_suppressions([], [])
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct OverlayFingerprint {
    pub schema: String,
    pub domain: String,
    pub sha256: String,
    pub empty: bool,
}

impl LexiconOverlay {
    pub fn new(entries: impl IntoIterator<Item = LexiconOverlayEntry>) -> Self {
        Self::with_suppressions(entries, [])
    }

    pub fn with_suppressions(
        entries: impl IntoIterator<Item = LexiconOverlayEntry>,
        suppressions: impl IntoIterator<Item = LexiconSuppression>,
    ) -> Self {
        let mut by_reading: BTreeMap<String, Vec<LexiconOverlayEntry>> = BTreeMap::new();
        for entry in entries {
            by_reading
                .entry(entry.reading.clone())
                .or_default()
                .push(entry);
        }
        // Tiers come in authority order. Within a tier, User and Imported
        // entries sort by surface; Postal keeps the official source order
        // through entry_id. The fingerprint does not depend on this order.
        for entries in by_reading.values_mut() {
            entries.sort_by(|left, right| {
                left.tier.cmp(&right.tier).then_with(|| match left.tier {
                    LexiconOverlayTier::User | LexiconOverlayTier::Imported => left
                        .surface
                        .cmp(&right.surface)
                        .then_with(|| left.entry_id.cmp(&right.entry_id))
                        .then_with(|| left.form_id.cmp(&right.form_id)),
                    LexiconOverlayTier::PostalCode => left
                        .entry_id
                        .cmp(&right.entry_id)
                        .then_with(|| left.surface.cmp(&right.surface))
                        .then_with(|| left.form_id.cmp(&right.form_id)),
                })
            });
        }
        let mut suppressions = suppressions.into_iter().collect::<Vec<_>>();
        suppressions.sort_by(|left, right| {
            left.tier
                .cmp(&right.tier)
                .then_with(|| left.reading.cmp(&right.reading))
                .then_with(|| left.surface.cmp(&right.surface))
        });
        suppressions.dedup();
        let mut overlay = Self {
            by_reading,
            suppressions,
            fingerprint: OverlayFingerprint {
                schema: String::new(),
                domain: String::new(),
                sha256: String::new(),
                empty: false,
            },
        };
        overlay.fingerprint = overlay.compute_fingerprint();
        overlay
    }

    pub fn is_empty(&self) -> bool {
        self.by_reading.is_empty() && self.suppressions.is_empty()
    }

    /// Canonical content fingerprint; revisions are deliberately excluded.
    pub fn fingerprint(&self) -> &OverlayFingerprint {
        &self.fingerprint
    }

    fn compute_fingerprint(&self) -> OverlayFingerprint {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"muu.lexicon-overlay.v1\0conversion\0");
        // Fingerprint order is deliberately independent from operational
        // lookup order and HashMap/input iteration.  The complete encoded
        // tuple is also the sort key, so fields omitted from an older partial
        // comparator cannot collide.
        for (reading, group) in &self.by_reading {
            put_raw_text(&mut bytes, reading);
            let mut entries = group
                .iter()
                .map(encode_entry_fingerprint)
                .collect::<Vec<_>>();
            entries.sort();
            put_u64(&mut bytes, entries.len() as u64);
            for entry in entries {
                bytes.extend_from_slice(&entry);
            }
        }

        let mut suppressions = self
            .suppressions
            .iter()
            .map(encode_suppression_fingerprint)
            .collect::<Vec<_>>();
        suppressions.sort();
        put_u64(&mut bytes, suppressions.len() as u64);
        for suppression in suppressions {
            bytes.extend_from_slice(&suppression);
        }
        // The identity is defined by `bytes` above: the schema tag, the
        // encoding and the sort. The shared digest only hashes and spells it.
        OverlayFingerprint {
            schema: "muu.lexicon-overlay.v1".to_owned(),
            domain: "conversion".to_owned(),
            sha256: sha256_hex(&bytes),
            empty: self.is_empty(),
        }
    }

    // An exact match wins; otherwise a postal-shaped reading (123ー4567,
    // full-width digits) is looked up by its seven canonical digits.
    pub(crate) fn entries_for_reading(&self, reading: &str) -> &[LexiconOverlayEntry] {
        if let Some(entries) = self.by_reading.get(reading) {
            return entries;
        }
        let Some(canonical) = canonical_postal_code_reading(reading) else {
            return &[];
        };
        self.by_reading
            .get(&canonical)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn has_higher_tier_entry_for_reading(
        &self,
        reading: &str,
        target_tier: LexiconOverlayTier,
    ) -> bool {
        self.entries_for_reading(reading)
            .iter()
            .any(|entry| entry.tier.outranks(target_tier))
    }

    // `target_tier` None means a built-in dictionary entry, which any
    // suppression may hide. An overlay entry is hidden only by a suppression
    // from its own tier or a higher one.
    pub(crate) fn suppresses(
        &self,
        reading: &str,
        surface: &str,
        target_tier: Option<LexiconOverlayTier>,
    ) -> bool {
        self.suppressions.iter().any(|suppression| {
            target_tier.is_none_or(|tier| suppression.tier.at_least_as_authoritative_as(tier))
                && suppression.matches(reading, surface)
        })
    }
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
fn put_raw_text(bytes: &mut Vec<u8>, value: &str) {
    put_u64(bytes, value.len() as u64);
    bytes.extend_from_slice(value.as_bytes());
}
fn put_raw_optional_text(bytes: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            bytes.push(1);
            put_raw_text(bytes, value);
        }
        None => bytes.push(0),
    }
}

fn encode_entry_fingerprint(entry: &LexiconOverlayEntry) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(entry.tier as u8);
    put_u64(&mut bytes, entry.dictionary_key);
    put_u64(&mut bytes, u64::from(entry.entry_id));
    put_u64(&mut bytes, u64::from(entry.form_id));
    put_raw_text(&mut bytes, &entry.reading);
    put_raw_text(&mut bytes, &entry.surface);
    put_u64(&mut bytes, u64::from(entry.left_id.get()));
    put_u64(&mut bytes, u64::from(entry.right_id.get()));
    put_u64(&mut bytes, u64::from(entry.left_pos_id.get()));
    put_u64(&mut bytes, u64::from(entry.right_pos_id.get()));
    bytes.extend_from_slice(&entry.word_cost.to_le_bytes());
    bytes
}

fn encode_suppression_fingerprint(suppression: &LexiconSuppression) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(suppression.tier as u8);
    put_raw_optional_text(&mut bytes, suppression.reading.as_deref());
    put_raw_optional_text(&mut bytes, suppression.surface.as_deref());
    bytes
}

fn canonical_postal_code_reading(reading: &str) -> Option<String> {
    let mut digits = String::with_capacity(7);
    let mut separator_seen = false;
    for character in reading.chars() {
        match character {
            '0'..='9' => digits.push(character),
            '０'..='９' => digits.push(char::from_u32(character as u32 - 0xfee0)?),
            // The Hiragana composer displays a physical `-` key as `ー`.
            // Accept it only in the exact 3+4 digit postal shape; it is never
            // treated as a general-purpose hyphen.
            '-' | '－' | 'ー' if digits.len() == 3 && !separator_seen => {
                separator_seen = true;
            }
            _ => return None,
        }
    }
    (digits.len() == 7).then_some(digits)
}

fn readings_match(left: &str, right: &str) -> bool {
    left == right
        || canonical_postal_code_reading(left)
            .zip(canonical_postal_code_reading(right))
            .is_some_and(|(left, right)| left == right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        tier: LexiconOverlayTier,
        entry_id: u32,
        reading: &str,
        surface: &str,
        word_cost: i32,
    ) -> LexiconOverlayEntry {
        LexiconOverlayEntry {
            tier,
            dictionary_key: 0,
            entry_id,
            form_id: 0,
            reading: reading.to_owned(),
            surface: surface.to_owned(),
            left_id: LeftContextId::new(0),
            right_id: RightContextId::new(0),
            left_pos_id: PosId::new(0),
            right_pos_id: PosId::new(0),
            word_cost,
        }
    }

    #[test]
    fn overlay_groups_and_orders_entries_deterministically() {
        let overlay = LexiconOverlay::new([
            entry(LexiconOverlayTier::User, 2, "むう", "夢有", -6_500),
            entry(LexiconOverlayTier::User, 1, "むう", "Muu", -9_500),
        ]);

        assert_eq!(
            overlay
                .entries_for_reading("むう")
                .iter()
                .map(|entry| entry.surface.as_str())
                .collect::<Vec<_>>(),
            ["Muu", "夢有"]
        );
        assert!(overlay.entries_for_reading("みゅう").is_empty());
        // Mixed tiers supplied out of order: entries come back by authority,
        // User and Imported by surface then entry and form ID, and Postal by
        // entry ID (its source order). Suppressions sort by tier, then reading
        // and surface, and the duplicate collapses. The tags happen to share
        // the authority order; the tier test below pins rank and tags apart.
        use LexiconOverlayTier::{Imported, PostalCode, User};
        let mut tied_form = entry(User, 1, "むう", "A", -1);
        tied_form.form_id = 1;
        let mixed = LexiconOverlay::with_suppressions(
            [
                entry(PostalCode, 1, "むう", "A", -1),
                entry(Imported, 1, "むう", "Z", -1),
                entry(User, 2, "むう", "A", -1),
                tied_form,
                entry(User, 1, "むう", "A", -1),
                entry(PostalCode, 0, "むう", "Z", -1),
                entry(Imported, 2, "むう", "A", -1),
            ],
            [
                LexiconSuppression {
                    tier: PostalCode,
                    reading: Some("むう".into()),
                    surface: None,
                },
                LexiconSuppression {
                    tier: User,
                    reading: None,
                    surface: Some("Z".into()),
                },
                LexiconSuppression {
                    tier: User,
                    reading: None,
                    surface: Some("A".into()),
                },
                LexiconSuppression {
                    tier: User,
                    reading: None,
                    surface: Some("A".into()),
                },
            ],
        );
        assert_eq!(
            mixed
                .entries_for_reading("むう")
                .iter()
                .map(|e| (e.tier, e.surface.as_str(), e.entry_id, e.form_id))
                .collect::<Vec<_>>(),
            [
                (User, "A", 1, 0),
                (User, "A", 1, 1),
                (User, "A", 2, 0),
                (Imported, "A", 2, 0),
                (Imported, "Z", 1, 0),
                (PostalCode, "Z", 0, 0),
                (PostalCode, "A", 1, 0)
            ]
        );
        assert_eq!(
            mixed
                .suppressions
                .iter()
                .map(|x| (x.tier, x.reading.as_deref(), x.surface.as_deref()))
                .collect::<Vec<_>>(),
            [
                (User, None, Some("A")),
                (User, None, Some("Z")),
                (PostalCode, Some("むう"), None)
            ]
        );
    }

    #[test]
    fn content_fingerprint_is_deterministic_and_revision_independent() {
        let first = LexiconOverlay::new([entry(LexiconOverlayTier::User, 1, "むう", "夢有", -1)]);
        let second = LexiconOverlay::new([entry(LexiconOverlayTier::User, 1, "むう", "夢有", -1)]);
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert!(!first.fingerprint().empty);
        let changed = LexiconOverlay::new([entry(LexiconOverlayTier::User, 2, "むう", "夢有", -1)]);
        assert_ne!(first.fingerprint().sha256, changed.fingerprint().sha256);
    }

    #[test]
    fn cached_fingerprint_getter_is_constant_reference() {
        let overlay = LexiconOverlay::default();
        let first = overlay.fingerprint() as *const OverlayFingerprint;
        let second = overlay.fingerprint() as *const OverlayFingerprint;
        assert_eq!(first, second);
        assert_eq!(
            overlay.fingerprint().sha256,
            "c8fbf1278f8bf0b270d659035b7741b1343e0d1c40bc3ac28a21b1d7a585eabe"
        );
    }

    #[test]
    fn fingerprint_binds_raw_content_and_total_field_order() {
        let base = entry(LexiconOverlayTier::User, 1, "e\u{301}", "é", -1);
        let reordered = LexiconOverlay::new([
            entry(LexiconOverlayTier::User, 2, "むう", "夢有", -2),
            base.clone(),
        ]);
        let canonical = LexiconOverlay::new([
            base.clone(),
            entry(LexiconOverlayTier::User, 2, "むう", "夢有", -2),
        ]);
        assert_eq!(reordered.fingerprint(), canonical.fingerprint());
        let composed = LexiconOverlay::new([entry(LexiconOverlayTier::User, 1, "é", "é", -1)]);
        assert_ne!(
            LexiconOverlay::new([base.clone()]).fingerprint(),
            composed.fingerprint()
        );

        // The old operational sort key intentionally ignores context and
        // dictionary provenance.  Fingerprint ordering must still be total
        // when those otherwise-colliding fields differ.
        let mut context_a = base.clone();
        context_a.dictionary_key = 9;
        context_a.left_id = LeftContextId::new(4);
        let mut context_b = context_a.clone();
        context_b.left_id = LeftContextId::new(5);
        let forward = LexiconOverlay::new([context_a.clone(), context_b.clone()]);
        let reverse = LexiconOverlay::new([context_b, context_a]);
        assert_eq!(forward.fingerprint(), reverse.fingerprint());

        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.tier = LexiconOverlayTier::Imported;
        variants.push(changed);
        let mut changed = base.clone();
        changed.dictionary_key = 7;
        variants.push(changed);
        let mut changed = base.clone();
        changed.entry_id = 7;
        variants.push(changed);
        let mut changed = base.clone();
        changed.form_id = 7;
        variants.push(changed);
        let mut changed = base.clone();
        changed.reading = "別".to_owned();
        variants.push(changed);
        let mut changed = base.clone();
        changed.surface = "別".to_owned();
        variants.push(changed);
        let mut changed = base.clone();
        changed.left_id = LeftContextId::new(7);
        variants.push(changed);
        let mut changed = base.clone();
        changed.right_id = RightContextId::new(7);
        variants.push(changed);
        let mut changed = base.clone();
        changed.left_pos_id = PosId::new(7);
        variants.push(changed);
        let mut changed = base.clone();
        changed.right_pos_id = PosId::new(7);
        variants.push(changed);
        let mut changed = base.clone();
        changed.word_cost = 7;
        variants.push(changed);
        for variant in variants {
            assert_ne!(
                LexiconOverlay::new([base.clone()]).fingerprint(),
                LexiconOverlay::new([variant]).fingerprint()
            );
        }

        let suppression = LexiconSuppression {
            tier: LexiconOverlayTier::User,
            reading: Some("e\u{301}".to_owned()),
            surface: Some("é".to_owned()),
        };
        let with_suppression =
            LexiconOverlay::with_suppressions([base.clone()], [suppression.clone()]);
        let composed_suppression = LexiconOverlay::with_suppressions(
            [base.clone()],
            [LexiconSuppression {
                reading: Some("é".to_owned()),
                ..suppression.clone()
            }],
        );
        assert_ne!(
            with_suppression.fingerprint(),
            composed_suppression.fingerprint()
        );
        let base_digest = LexiconOverlay::new([base.clone()]).fingerprint().clone();
        assert_ne!(base_digest, *with_suppression.fingerprint());
        for variant in [
            LexiconSuppression {
                tier: LexiconOverlayTier::Imported,
                ..suppression.clone()
            },
            LexiconSuppression {
                reading: Some("別".to_owned()),
                ..suppression.clone()
            },
            LexiconSuppression {
                surface: Some("別".to_owned()),
                ..suppression
            },
        ] {
            let changed = LexiconOverlay::with_suppressions([base.clone()], [variant]);
            assert_ne!(with_suppression.fingerprint(), changed.fingerprint());
        }
    }

    #[test]
    fn tier_authority_is_explicit_instead_of_inferred_from_discriminants() {
        use LexiconOverlayTier::{Imported, PostalCode, User};
        let authority = [User, Imported, PostalCode];
        // Independent ordered contract: all nine pairs agree across the
        // outranks relation, total ordering and partial ordering.
        for (left_rank, left) in authority.iter().enumerate() {
            for (right_rank, right) in authority.iter().enumerate() {
                assert_eq!(left.outranks(*right), left_rank < right_rank);
                assert_eq!(
                    left.at_least_as_authoritative_as(*right),
                    left_rank <= right_rank
                );
                assert_eq!(left.cmp(right), left_rank.cmp(&right_rank));
                assert_eq!(left.partial_cmp(right), Some(left_rank.cmp(&right_rank)));
            }
        }
        // The wire tags are pinned separately and stay 1, 2, 3 whatever the
        // ranks are.
        assert_eq!([User as u8, Imported as u8, PostalCode as u8], [1, 2, 3]);
    }

    #[test]
    fn postal_reading_accepts_declared_forms_and_composer_hyphen_projection() {
        let overlay = LexiconOverlay::new([entry(
            LexiconOverlayTier::PostalCode,
            0,
            "1000001",
            "東京都千代田区千代田",
            -5_906,
        )]);

        for reading in [
            "1000001",
            "100-0001",
            "１０００００１",
            "１００－０００１",
            "100ー0001",
        ] {
            assert_eq!(
                overlay.entries_for_reading(reading)[0].surface,
                "東京都千代田区千代田"
            );
        }
        for reading in [
            "10ー00001",
            "10-00001",
            "100--0001",
            "100ーー0001",
            "100－ー0001",
            "100000",
            "10000011",
        ] {
            assert!(overlay.entries_for_reading(reading).is_empty(), "{reading}");
        }
    }

    #[test]
    fn postal_canonical_reading_applies_to_tier_precedence_and_suppression() {
        let higher_tier = LexiconOverlay::new([entry(
            LexiconOverlayTier::User,
            0,
            "1000001",
            "ユーザー住所",
            -10_000,
        )]);
        assert!(
            higher_tier.has_higher_tier_entry_for_reading(
                "１００－０００１",
                LexiconOverlayTier::PostalCode
            )
        );

        let suppressor = LexiconOverlay::with_suppressions(
            [],
            [LexiconSuppression {
                tier: LexiconOverlayTier::User,
                reading: Some("1000001".to_owned()),
                surface: Some("東京都千代田区千代田".to_owned()),
            }],
        );
        assert!(suppressor.suppresses(
            "１００ー０００１",
            "東京都千代田区千代田",
            Some(LexiconOverlayTier::PostalCode)
        ));
        assert!(!suppressor.suppresses(
            "100ーー0001",
            "東京都千代田区千代田",
            Some(LexiconOverlayTier::PostalCode)
        ));
    }

    #[test]
    fn postal_candidates_preserve_official_source_order() {
        let overlay = LexiconOverlay::new([
            entry(
                LexiconOverlayTier::PostalCode,
                1,
                "1638001",
                "東京都新宿区西新宿二丁目",
                -5_906,
            ),
            entry(
                LexiconOverlayTier::PostalCode,
                0,
                "1638001",
                "東京都新宿区西新宿一丁目",
                -5_906,
            ),
        ]);

        assert_eq!(
            overlay
                .entries_for_reading("1638001")
                .iter()
                .map(|entry| entry.entry_id)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn suppression_can_match_a_pair_or_one_declared_side() {
        let overlay = LexiconOverlay::with_suppressions(
            [],
            [
                LexiconSuppression {
                    tier: LexiconOverlayTier::Imported,
                    reading: Some("きょう".to_owned()),
                    surface: Some("京".to_owned()),
                },
                LexiconSuppression {
                    tier: LexiconOverlayTier::Imported,
                    reading: None,
                    surface: Some("表記".to_owned()),
                },
            ],
        );
        assert!(overlay.suppresses("きょう", "京", None));
        assert!(!overlay.suppresses("きょう", "今日", None));
        assert!(overlay.suppresses("ひょうき", "表記", None));
        assert!(!overlay.suppresses("きょう", "京", Some(LexiconOverlayTier::User)));
        assert!(overlay.suppresses("きょう", "京", Some(LexiconOverlayTier::PostalCode)));
    }
}
