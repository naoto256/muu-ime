use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use muu_conversion::{
    BoundaryContext, BoundaryDecision, CostSnapshot, Lattice, SegmentRule, Segmenter,
    convert_to_segments, local_search, viterbi_1best,
};
use muu_dict::{
    Dictionary, DictionaryBuilder, LeftContextId, LexiconEntry, PosId, RightContextId,
    UnknownClass, UnknownDefinition,
};
use tempfile::NamedTempFile;

const WARMUP_ITERATIONS: usize = 200;
const MEASURED_ITERATIONS: usize = 2_000;
const REAL_WARMUP_ITERATIONS: usize = 10;
const REAL_MEASURED_ITERATIONS: usize = 100;
const PROVISIONAL_CORE_P95_BUDGET: Duration = Duration::from_millis(5);

fn main() {
    if let Some(path) = std::env::var_os("MUU_DICTIONARY_PATH") {
        let started = Instant::now();
        let dictionary = Dictionary::open(Path::new(&path)).unwrap();
        let open_time = started.elapsed();
        let segmenter = Segmenter::new(Vec::new(), BoundaryDecision::Break);
        run_benchmark(
            &dictionary,
            &segmenter,
            "mozc-oss",
            "にほんご",
            REAL_WARMUP_ITERATIONS,
            REAL_MEASURED_ITERATIONS,
            Some(open_time),
        );
        return;
    }

    let (_file, dictionary, segmenter) = fixture();
    run_benchmark(
        &dictionary,
        &segmenter,
        "synthetic",
        "あいうえおか",
        WARMUP_ITERATIONS,
        MEASURED_ITERATIONS,
        None,
    );
}

fn run_benchmark(
    dictionary: &Dictionary,
    segmenter: &Segmenter,
    fixture_name: &str,
    reading: &str,
    warmup_iterations: usize,
    measured_iterations: usize,
    open_time: Option<Duration>,
) {
    let snapshot = CostSnapshot::baseline(dictionary);
    let boundary = BoundaryContext::sentence(&snapshot);

    for _ in 0..warmup_iterations {
        black_box(convert_to_segments(black_box(reading), segmenter, &snapshot).unwrap());
    }

    let conversion_p95 = p95(measured_iterations, || {
        black_box(convert_to_segments(black_box(reading), segmenter, &snapshot).unwrap());
    });
    let local_search_p95 = p95(measured_iterations, || {
        black_box(local_search(black_box(reading), &boundary, &snapshot).unwrap());
    });
    let lattice_p95 = p95(measured_iterations, || {
        black_box(Lattice::build(black_box(reading), dictionary).unwrap());
    });
    let prefix_p95 = p95(measured_iterations, || {
        black_box(dictionary.common_prefix_search(black_box(reading)).unwrap());
    });
    let prefix_matches = dictionary.common_prefix_search(reading).unwrap();
    let prefix_tokens = prefix_matches
        .iter()
        .map(|prefix| prefix.tokens.len())
        .sum::<usize>();
    let lattice = Lattice::build(reading, dictionary).unwrap();
    let viterbi_p95 = p95(measured_iterations, || {
        black_box(viterbi_1best(&lattice, &snapshot).unwrap());
    });

    println!(
        "conversion_latency fixture={fixture_name} reading={reading} \
         iterations={measured_iterations} open_us={} conversion_p95_us={} \
         local_search_p95_us={} lattice_p95_us={} viterbi_p95_us={} \
         prefix_p95_us={} prefix_matches={} prefix_tokens={} provisional_budget_us={}",
        open_time.map_or(0, |duration| duration.as_micros()),
        conversion_p95.as_micros(),
        local_search_p95.as_micros(),
        lattice_p95.as_micros(),
        viterbi_p95.as_micros(),
        prefix_p95.as_micros(),
        prefix_matches.len(),
        prefix_tokens,
        PROVISIONAL_CORE_P95_BUDGET.as_micros()
    );
    if fixture_name == "synthetic" {
        assert!(
            conversion_p95 < PROVISIONAL_CORE_P95_BUDGET,
            "synthetic conversion p95 {conversion_p95:?} exceeded provisional budget \
             {PROVISIONAL_CORE_P95_BUDGET:?}"
        );
    }
}

fn p95(iterations: usize, mut operation: impl FnMut()) -> Duration {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        operation();
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    samples[(samples.len() * 95).div_ceil(100) - 1]
}

fn fixture() -> (NamedTempFile, Dictionary, Segmenter) {
    let mut builder =
        DictionaryBuilder::new(8, 8, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.provenance(b"name = \"conversion_latency\"\n");
    for (reading, surface, pos, context) in [
        ("あ", "亜", 1, 1),
        ("い", "伊", 2, 2),
        ("う", "宇", 1, 1),
        ("え", "江", 2, 2),
        ("お", "於", 1, 1),
        ("か", "加", 2, 2),
    ] {
        builder.add_entry(LexiconEntry {
            reading: reading.into(),
            surface: surface.into(),
            left_id: LeftContextId::new(context),
            right_id: RightContextId::new(context),
            left_pos_id: PosId::new(pos),
            right_pos_id: PosId::new(pos),
            word_cost: 0,
        });
    }
    builder.add_unknown(UnknownDefinition {
        class: UnknownClass::Other,
        group: false,
        max_length: 1,
        left_id: LeftContextId::new(7),
        right_id: RightContextId::new(7),
        left_pos_id: PosId::new(7),
        right_pos_id: PosId::new(7),
        word_cost: 1_000,
    });
    let bytes = builder.build().unwrap();
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), bytes).unwrap();
    let dictionary = Dictionary::open(file.path()).unwrap();
    let segmenter = Segmenter::new(
        vec![SegmentRule {
            previous_right_pos: None,
            next_left_pos: Some(PosId::new(2)),
            previous_surface_suffix: None,
            next_surface_prefix: None,
            decision: BoundaryDecision::Join,
        }],
        BoundaryDecision::Break,
    );
    (file, dictionary, segmenter)
}
