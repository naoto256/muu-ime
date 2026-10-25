//! Production dictionary/candidate contracts retained from retired experiments.
//! These self-authored fixtures pin generation, reachability and segmentation;
//! no discarded semantic model, parameter sweep or bigram correction runs here.
use std::collections::{BTreeMap, BTreeSet};
#[path = "../../../test-support/real_assets.rs"]
mod real_assets;
#[path = "../../../test-support/real_dictionary.rs"]
mod real_dictionary;
use muu_conversion::{ConversionTrace, CostSnapshot, convert_to_segments, trace_conversion};
use muu_dict::Dictionary;
use muu_session::default_segmenter;
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;
mod kisha {
    use super::*;
    const GOLD_SOURCE: &str = include_str!("data/kisha-candidate-cases.tsv");
    const BASELINE_SOURCE: &str = include_str!("data/kisha-candidate-baseline.tsv");
    #[derive(Debug)]
    struct GoldCase {
        id: String,
        split: String,
        reading_segments: Vec<String>,
        expected_segments: Vec<String>,
        tags: Vec<String>,
        provenance: String,
    }

    impl GoldCase {
        fn reading(&self) -> String {
            self.reading_segments.concat()
        }

        fn expected_surface(&self) -> String {
            self.expected_segments.concat()
        }
    }

    #[derive(Debug)]
    struct BaselineCase {
        best_surface: String,
        selected_surface: String,
        expected_candidate_ranks: Vec<usize>,
    }

    fn data_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
        source.lines().enumerate().filter_map(|(index, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then_some((index + 1, line))
        })
    }

    fn split_segments(field: &str, line: usize, name: &str) -> Vec<String> {
        let segments = field
            .split('|')
            .map(str::trim)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(
            !segments.is_empty() && segments.iter().all(|segment| !segment.is_empty()),
            "line {line}: {name} must contain non-empty pipe-delimited segments"
        );
        segments
    }

    fn parse_gold() -> Vec<GoldCase> {
        let mut ids = BTreeSet::new();
        data_lines(GOLD_SOURCE)
            .map(|(line, row)| {
                let fields = row.split('\t').collect::<Vec<_>>();
                assert_eq!(fields.len(), 6, "line {line}: expected six TSV fields");
                assert!(
                    ids.insert(fields[0]),
                    "line {line}: duplicate id {}",
                    fields[0]
                );
                let case = GoldCase {
                    id: fields[0].to_owned(),
                    split: fields[1].to_owned(),
                    reading_segments: split_segments(fields[2], line, "reading_segments"),
                    expected_segments: split_segments(fields[3], line, "expected_segments"),
                    tags: fields[4].split(';').map(str::to_owned).collect(),
                    provenance: fields[5].to_owned(),
                };
                assert_eq!(
                    case.reading_segments.len(),
                    case.expected_segments.len(),
                    "line {line}: reading and expected segment counts differ"
                );
                assert!(matches!(case.split.as_str(), "probe" | "dev" | "heldout"));
                assert!(!case.tags.is_empty() && case.tags.iter().all(|tag| !tag.is_empty()));
                assert!(!case.provenance.is_empty());
                for text in case.reading_segments.iter().chain(&case.expected_segments) {
                    assert_eq!(
                        text.nfc().collect::<String>(),
                        *text,
                        "line {line}: non-NFC text"
                    );
                }
                case
            })
            .collect()
    }

    fn parse_baseline() -> BTreeMap<String, BaselineCase> {
        data_lines(BASELINE_SOURCE)
            .map(|(line, row)| {
                let fields = row.split('\t').collect::<Vec<_>>();
                assert_eq!(fields.len(), 4, "line {line}: expected four TSV fields");
                let ranks = fields[3]
                    .split(',')
                    .map(|rank| rank.parse::<usize>().expect("baseline rank"))
                    .collect();
                (
                    fields[0].to_owned(),
                    BaselineCase {
                        best_surface: fields[1].to_owned(),
                        selected_surface: fields[2].to_owned(),
                        expected_candidate_ranks: ranks,
                    },
                )
            })
            .collect()
    }

    fn production_dictionary() -> Option<Arc<Dictionary>> {
        real_dictionary::shared()
    }

    fn expected_boundaries(case: &GoldCase) -> Vec<(usize, usize)> {
        let mut start = 0;
        case.reading_segments
            .iter()
            .map(|segment| {
                let end = start + segment.chars().count();
                let range = (start, end);
                start = end;
                range
            })
            .collect()
    }

    // Without assets: the nine kisha cases and their recorded baseline rows
    // exist one for one, so the asset test cannot pass on a shrunken fixture.
    #[test]
    fn ambiguous_gold_and_baseline_manifests_are_well_formed() {
        let gold = parse_gold();
        let baseline = parse_baseline();
        // Provenance labels may change; IDs, splits, inputs, expected output
        // and coverage tags remain a fixed set of nine cases in source order.
        let input_rows = data_lines(GOLD_SOURCE)
            .map(|(_, row)| row.rsplit_once('\t').unwrap().0)
            .collect::<Vec<_>>();
        assert_eq!(
            muu_digest::sha256_hex(input_rows.join("\n").as_bytes()),
            "3eb20ed3baddd9aca6dfcf6cb6363c2bcd2031e872ea966442df4d9c1fd9c1da"
        );
        assert_eq!(gold.len(), 9);
        assert_eq!(gold[0].provenance, "self-authored-accepted");
        assert_eq!(baseline.len(), gold.len());
        assert_eq!(
            baseline.keys().cloned().collect::<BTreeSet<_>>(),
            gold.iter().map(|case| case.id.clone()).collect()
        );
    }

    // The real dictionary reproduces the recorded baseline for every kisha case:
    // best and selected surfaces, segmentation, and each expected segment's
    // candidate rank. The totals (2 exact, 33 segments all reachable, MRR
    // 0.8131…) are pinned too. A deliberate dictionary or ranking change updates
    // kisha-candidate-baseline.tsv and these totals together.
    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn production_dictionary_matches_the_recorded_kisha_baseline() {
        let Some(dictionary) = production_dictionary() else {
            return;
        };
        let gold = parse_gold();
        let baseline = parse_baseline();
        let segmenter = default_segmenter();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let mut top1_exact = 0;
        let mut boundary_exact = 0;
        let mut covered_segments = 0;
        let mut reciprocal_rank_sum = 0.0_f64;
        let mut total_segments = 0;

        for case in &gold {
            let trace = trace_conversion(&case.reading(), &segmenter, &snapshot)
                .expect("production conversion trace");
            let recorded = baseline.get(&case.id).expect("baseline row");
            let selected_surface = trace
                .segments
                .iter()
                .map(|segment| segment.selected_surface.as_str())
                .collect::<String>();
            let actual_boundaries = trace
                .segments
                .iter()
                .map(|segment| {
                    (
                        segment.reading_range.start.get(),
                        segment.reading_range.end.get(),
                    )
                })
                .collect::<Vec<_>>();

            assert_eq!(
                trace.best_path.surface, recorded.best_surface,
                "{}",
                case.id
            );
            assert_eq!(selected_surface, recorded.selected_surface, "{}", case.id);
            assert_eq!(
                actual_boundaries,
                expected_boundaries(case),
                "{}: unexpected production segmentation",
                case.id
            );
            boundary_exact += 1;

            let ranks = case
                .expected_segments
                .iter()
                .zip(&trace.segments)
                .map(|(expected, segment)| {
                    segment
                        .candidates
                        .iter()
                        .find(|candidate| candidate.surface == *expected)
                        .map(|candidate| candidate.rank)
                        .unwrap_or(usize::MAX)
                })
                .collect::<Vec<_>>();
            assert_eq!(ranks, recorded.expected_candidate_ranks, "{}", case.id);

            for rank in ranks {
                total_segments += 1;
                if rank != usize::MAX {
                    covered_segments += 1;
                    reciprocal_rank_sum += 1.0 / (rank + 1) as f64;
                }
            }
            top1_exact += usize::from(trace.best_path.surface == case.expected_surface());
        }

        assert_eq!(
            top1_exact, 2,
            "record intentional baseline improvements separately"
        );
        assert_eq!(boundary_exact, gold.len());
        assert_eq!(covered_segments, total_segments);
        assert_eq!(total_segments, 33);
        assert!((reciprocal_rank_sum / total_segments as f64 - 0.813_131_313_1).abs() < 1e-9);
    }
}
mod homophone {
    use super::*;
    const GOLD_SOURCE: &str = include_str!("data/homophone-candidate-cases.tsv");
    const BASELINE_SOURCE: &str = include_str!("data/homophone-candidate-baseline.tsv");
    const BLIND_GOLD_SOURCE: &str = include_str!("data/independent-candidate-cases.tsv");
    #[derive(Debug)]
    struct GoldCase {
        id: String,
        family: String,
        split: String,
        reading: String,
        expected_surface: String,
        target_reading: String,
        target_surface: String,
        provenance: String,
    }

    fn data_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
        source.lines().enumerate().filter_map(|(index, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then_some((index + 1, line))
        })
    }

    fn parse_gold_source(source: &str) -> Vec<GoldCase> {
        data_lines(source)
            .map(|(line, row)| {
                let fields = row.split('\t').collect::<Vec<_>>();
                assert_eq!(fields.len(), 8, "line {line}");
                for field in &fields {
                    assert_eq!(field.nfc().collect::<String>(), *field, "line {line}");
                }
                GoldCase {
                    id: fields[0].to_owned(),
                    family: fields[1].to_owned(),
                    split: fields[2].to_owned(),
                    reading: fields[3].to_owned(),
                    expected_surface: fields[4].to_owned(),
                    target_reading: fields[5].to_owned(),
                    target_surface: fields[6].to_owned(),
                    provenance: fields[7].to_owned(),
                }
            })
            .collect()
    }

    fn parse_gold() -> Vec<GoldCase> {
        parse_gold_source(GOLD_SOURCE)
    }

    fn parse_blind_gold() -> Vec<GoldCase> {
        parse_gold_source(BLIND_GOLD_SOURCE)
    }

    fn production_dictionary() -> Option<Arc<Dictionary>> {
        real_dictionary::shared()
    }

    fn parse_baseline() -> BTreeMap<String, (String, usize)> {
        data_lines(BASELINE_SOURCE)
            .map(|(line, row)| {
                let fields = row.split('\t').collect::<Vec<_>>();
                assert_eq!(fields.len(), 3, "baseline line {line}");
                (
                    fields[0].to_owned(),
                    (
                        fields[1].to_owned(),
                        fields[2].parse().expect("target rank"),
                    ),
                )
            })
            .collect()
    }

    fn expected_candidate_path(trace: &ConversionTrace, expected: &str) -> Option<Vec<usize>> {
        fn visit(
            trace: &ConversionTrace,
            expected: &str,
            segment_index: usize,
            byte_offset: usize,
            selected: &mut Vec<usize>,
        ) -> bool {
            if segment_index == trace.segments.len() {
                return byte_offset == expected.len();
            }
            for (candidate_index, candidate) in
                trace.segments[segment_index].candidates.iter().enumerate()
            {
                let Some(remaining) = expected.get(byte_offset..) else {
                    return false;
                };
                if !remaining.starts_with(&candidate.surface) {
                    continue;
                }
                selected.push(candidate_index);
                if visit(
                    trace,
                    expected,
                    segment_index + 1,
                    byte_offset + candidate.surface.len(),
                    selected,
                ) {
                    return true;
                }
                selected.pop();
            }
            false
        }

        let mut selected = Vec::with_capacity(trace.segments.len());
        visit(trace, expected, 0, 0, &mut selected).then_some(selected)
    }

    // Without assets: the 81 homophone cases are self-authored, split into
    // probe/dev/holdout, contain their target reading, and match the baseline
    // rows one for one.
    #[test]
    fn homophone_family_gold_is_well_formed() {
        let gold = parse_gold();
        let baseline = parse_baseline();
        assert_eq!(gold.len(), 81);
        assert_eq!(baseline.len(), gold.len());
        assert_eq!(
            baseline.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            gold.iter()
                .map(|case| case.id.as_str())
                .collect::<BTreeSet<_>>(),
            "every gold case must have a recorded baseline, without extra keys"
        );
        assert_eq!(
            gold.iter()
                .map(|case| case.id.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            gold.len()
        );
        assert_eq!(
            gold.iter()
                .map(|case| case.family.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "atsui",
                "au",
                "hakaru",
                "kaeru",
                "kaitou",
                "kiku",
                "osameru",
                "tsutomeru",
            ])
        );
        for case in &gold {
            assert!(matches!(case.split.as_str(), "probe" | "dev" | "holdout"));
            assert_eq!(case.provenance, "self-authored");
            assert!(case.reading.contains(&case.target_reading), "{}", case.id);
            assert!(
                case.expected_surface.contains(&case.target_surface),
                "{}",
                case.id
            );
        }
    }

    // Every homophone case keeps its recorded best surface and target rank on
    // the real dictionary (24 exact, all reachable); a missing baseline row is a
    // failure, not a skip.
    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn inspect_production_candidate_coverage_for_homophone_families() {
        let Some(dictionary) = production_dictionary() else {
            return;
        };
        let segmenter = default_segmenter();
        let snapshot = CostSnapshot::baseline(&dictionary);
        let baseline = parse_baseline();
        let gold = parse_gold();
        let mut top1_exact = 0;
        let mut covered = 0;

        for case in &gold {
            let trace = trace_conversion(&case.reading, &segmenter, &snapshot)
                .expect("production conversion trace");
            let path = expected_candidate_path(&trace, &case.expected_surface);
            // A missing ID is a broken fixture, not permission to omit its
            // surface/rank assertions while keeping the same row count.
            let recorded = baseline.get(&case.id).expect("gold ID has a baseline");
            assert_eq!(trace.best_path.surface, recorded.0, "{}", case.id);
            assert_eq!(
                path.as_ref().map(|indices| indices.iter().sum::<usize>()),
                Some(recorded.1),
                "{}",
                case.id
            );
            top1_exact += usize::from(trace.best_path.surface == case.expected_surface);
            covered += usize::from(path.is_some());
        }

        eprintln!(
            "summary\ttop1={top1_exact}/{}\tcovered={covered}/{}",
            gold.len(),
            gold.len()
        );
        assert_eq!(top1_exact, 24);
        assert_eq!(covered, gold.len());
    }

    // The 72 blind cases each remain reachable as a candidate path.
    #[test]
    #[ignore = "requires public dictionary (check-assets)"]
    fn independent_candidate_cases_remain_reachable_in_production() {
        let Some(dictionary) = production_dictionary() else {
            return;
        };
        let cases = parse_blind_gold();
        assert_eq!(cases.len(), 72);
        assert_eq!(
            cases
                .iter()
                .map(|case| case.family.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            10
        );
        assert!(
            cases
                .iter()
                .all(|case| case.provenance == "self-authored-independent")
        );
        let snapshot = CostSnapshot::baseline(&dictionary);
        let covered = cases
            .iter()
            .filter(|case| {
                let trace = trace_conversion(&case.reading, &default_segmenter(), &snapshot)
                    .expect("production trace");
                expected_candidate_path(&trace, &case.expected_surface).is_some()
            })
            .count();
        assert_eq!(covered, cases.len());
    }
    mod sequence {
        use std::collections::{BTreeMap, BTreeSet};

        use super::*;

        const SEQUENCE_GOLD_SOURCE: &str = include_str!("data/sequence-candidate-cases.tsv");

        #[derive(Debug)]
        struct SequenceCase {
            id: String,
            family: String,
            split: String,
            reading: String,
            expected_surface: String,
        }

        fn parse_sequence_gold() -> Vec<SequenceCase> {
            data_lines(SEQUENCE_GOLD_SOURCE)
                .map(|(line_number, line)| {
                    let fields = line.split('\t').collect::<Vec<_>>();
                    assert_eq!(fields.len(), 6, "sequence gold line {line_number}");
                    assert_eq!(fields[5], "self-authored-independent");
                    SequenceCase {
                        id: fields[0].to_owned(),
                        family: fields[1].to_owned(),
                        split: fields[2].to_owned(),
                        reading: fields[3].to_owned(),
                        expected_surface: fields[4].to_owned(),
                    }
                })
                .collect()
        }

        fn whole_surface_reachable_at(
            segments: &muu_conversion::Segments,
            expected: &str,
            limit: usize,
        ) -> bool {
            fn visit(
                segments: &muu_conversion::Segments,
                expected: &str,
                segment_index: usize,
                byte_offset: usize,
                limit: usize,
            ) -> bool {
                if segment_index == segments.segments().len() {
                    return byte_offset == expected.len();
                }
                segments.segments()[segment_index]
                    .candidates()
                    .iter()
                    .take(limit)
                    .any(|candidate| {
                        expected[byte_offset..]
                            .strip_prefix(candidate.surface())
                            .is_some_and(|remainder| {
                                visit(
                                    segments,
                                    expected,
                                    segment_index + 1,
                                    expected.len() - remainder.len(),
                                    limit,
                                )
                            })
                    })
            }

            visit(segments, expected, 0, 0, limit)
        }

        fn fixed_surface(segments: &muu_conversion::Segments) -> String {
            segments
                .segments()
                .iter()
                .map(|segment| segment.focused().surface())
                .collect()
        }

        // Without assets: 42 unique sequence cases, dev 28 / final 14, with no
        // family shared between the splits.
        #[test]
        fn sequence_gold_keeps_distinct_cases_and_disjoint_splits() {
            let cases = parse_sequence_gold();
            assert_eq!(cases.len(), 42);
            assert_eq!(
                cases
                    .iter()
                    .map(|case| case.id.as_str())
                    .collect::<BTreeSet<_>>()
                    .len(),
                cases.len()
            );

            let mut split_counts = BTreeMap::new();
            let mut split_families: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
            for case in &cases {
                *split_counts.entry(case.split.as_str()).or_insert(0_usize) += 1;
                split_families
                    .entry(case.split.as_str())
                    .or_default()
                    .insert(case.family.as_str());
            }
            assert_eq!(split_counts, BTreeMap::from([("dev", 28), ("final", 14)]));
            assert!(split_families["dev"].is_disjoint(&split_families["final"]));
        }

        // On the real dictionary the baseline gets 19 of 42 right and every case is
        // reachable at each recall depth; these are the generation ceiling numbers
        // any ranking change is measured against.
        #[test]
        #[ignore = "requires public dictionary (check-assets)"]
        fn sequence_gold_is_disjoint_and_exposes_candidate_generation_ceiling() {
            let cases = parse_sequence_gold();
            let Some(dictionary) = production_dictionary() else {
                return;
            };
            let segmenter = default_segmenter();
            let snapshot = CostSnapshot::baseline(&dictionary);
            let converted = cases
                .iter()
                .map(|case| {
                    let segments = convert_to_segments(&case.reading, &segmenter, &snapshot)
                        .expect("sequence production segments");
                    (case, segments)
                })
                .collect::<Vec<_>>();

            let baseline = converted
                .iter()
                .filter(|(case, segments)| fixed_surface(segments) == case.expected_surface)
                .count();
            let baseline_misses = converted
                .iter()
                .filter(|(case, segments)| fixed_surface(segments) != case.expected_surface)
                .map(|(case, segments)| (case.id.as_str(), fixed_surface(segments)))
                .collect::<Vec<_>>();
            let recall = [8_usize, 16, 32].map(|limit| {
                converted
                    .iter()
                    .filter(|(case, segments)| {
                        whole_surface_reachable_at(segments, &case.expected_surface, limit)
                    })
                    .count()
            });
            let unreachable = converted
                .iter()
                .filter(|(case, segments)| {
                    !whole_surface_reachable_at(segments, &case.expected_surface, 32)
                })
                .map(|(case, segments)| (case.id.as_str(), fixed_surface(segments)))
                .collect::<Vec<_>>();
            eprintln!(
                "sequence-candidate-ceiling\tbaseline={baseline}/42\trecall@8={}/42\trecall@16={}/42\trecall@32={}/42\tbaseline-misses={baseline_misses:?}\tunreachable={unreachable:?}",
                recall[0], recall[1], recall[2]
            );

            assert_eq!((baseline, recall), (19, [42, 42, 42]));
        }
    }
}
