// `hex_encode` also spells the view envelope, which is not a digest.
use muu_digest::{hex_lower as hex_encode, sha256_hex};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use muu_conversion::{
    BoundaryDecision, CostSnapshot, Segmenter, convert_to_segments, trace_conversion,
};
use muu_dict::Dictionary;
use muu_dict_cli::load_mozc_builder;
use muu_session::{default_segmenter, encode_view, view_from_segments};

const USAGE: &str = "\
Usage:
  muu-dict build-mozc \\
    --source-dir <directory> \\
    --sudachi-supplement <file> \\
    --pos-map <file> \\
    --unknown <file> \\
    --provenance <file> \\
    --output <file>
  muu-dict verify --dictionary <file>
  muu-dict baseline --dictionary <file> --reading <text>
  muu-dict trace --dictionary <file> --reading <text>
  muu-dict postal-seed --csv <file> --zip-sha256 <hash> --last-modified <text> --retrieved-at <UTC> --output-dir <directory>
";

fn main() {
    if let Err(error) = run(env::args().skip(1)) {
        eprintln!("muu-dict: {error}");
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
}

fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let mut arguments = arguments.into_iter();
    let command = arguments.next().ok_or("missing command")?;
    if command == "--help" || command == "-h" {
        print!("{USAGE}");
        return Ok(());
    }
    match command.as_str() {
        "build-mozc" => build_mozc(arguments),
        "verify" => verify(arguments),
        "baseline" => baseline(arguments),
        "trace" => trace(arguments),
        "postal-seed" => postal_seed(arguments),
        _ => Err(format!("unknown command {command:?}")),
    }
}

fn build_mozc(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let options = parse_options(
        arguments,
        &[
            "--source-dir",
            "--sudachi-supplement",
            "--pos-map",
            "--unknown",
            "--provenance",
            "--output",
        ],
    )?;
    let source_dir = required_path(&options, "--source-dir")?;
    let sudachi_supplement = required_path(&options, "--sudachi-supplement")?;
    let pos_map = required_path(&options, "--pos-map")?;
    let unknown = required_path(&options, "--unknown")?;
    let provenance = required_path(&options, "--provenance")?;
    let output = required_path(&options, "--output")?;

    let builder = load_mozc_builder(
        &source_dir,
        &sudachi_supplement,
        &pos_map,
        &unknown,
        &provenance,
    )
    .map_err(|error| error.to_string())?;
    let bytes = builder.build().map_err(str::to_owned)?;
    publish_atomically(&output, &bytes)?;

    println!(
        "built {} bytes sha256={} output={}",
        bytes.len(),
        sha256_hex(&bytes),
        output.display()
    );
    Ok(())
}

fn verify(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let options = parse_options(arguments, &["--dictionary"])?;
    let path = required_path(&options, "--dictionary")?;
    let started = Instant::now();
    let dictionary = Dictionary::open(&path).map_err(|error| error.to_string())?;
    let open_time = started.elapsed();
    let provenance_size = dictionary
        .provenance()
        .map_err(|error| error.to_string())?
        .len();
    println!(
        "verified dictionary={} open_us={} provenance_bytes={provenance_size}",
        path.display(),
        open_time.as_micros()
    );
    Ok(())
}

// Prints the hex view envelope of a dictionary-only conversion with a
// segmenter that breaks at every boundary. `just dict-reproducibility` compares
// these against a pinned golden, so the output must not depend on learning,
// overlays, the LM or the production segmentation rules.
fn baseline(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let options = parse_options(arguments, &["--dictionary", "--reading"])?;
    let path = required_path(&options, "--dictionary")?;
    let reading = options
        .get("--reading")
        .ok_or("missing required option --reading")?;
    let dictionary = Dictionary::open(&path).map_err(|error| error.to_string())?;
    let snapshot = CostSnapshot::baseline(&dictionary);
    let segmenter = Segmenter::new(Vec::new(), BoundaryDecision::Break);
    let segments =
        convert_to_segments(reading, &segmenter, &snapshot).map_err(|error| error.to_string())?;
    let encoded =
        encode_view(&view_from_segments(&segments, true)).map_err(|error| error.to_string())?;
    println!("{}", hex_encode(&encoded));
    Ok(())
}

fn trace(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let options = parse_options(arguments, &["--dictionary", "--reading"])?;
    let path = required_path(&options, "--dictionary")?;
    let reading = options
        .get("--reading")
        .ok_or("missing required option --reading")?;
    let dictionary = Dictionary::open(&path).map_err(|error| error.to_string())?;
    let snapshot = CostSnapshot::baseline(&dictionary);
    let trace = trace_conversion(reading, &default_segmenter(), &snapshot)
        .map_err(|error| error.to_string())?;
    print!("{trace}");
    Ok(())
}

fn parse_options(
    mut arguments: impl Iterator<Item = String>,
    allowed: &[&str],
) -> Result<BTreeMap<String, String>, String> {
    let mut options = BTreeMap::new();
    while let Some(name) = arguments.next() {
        if !name.starts_with("--") {
            return Err(format!("expected an option, got {name:?}"));
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("missing value for {name}"))?;
        if value.starts_with("--") {
            return Err(format!("missing value for {name}"));
        }
        if options.insert(name.clone(), value).is_some() {
            return Err(format!("duplicate option {name}"));
        }
    }
    if let Some(unknown) = options
        .keys()
        .find(|name| !allowed.contains(&name.as_str()))
    {
        return Err(format!("unknown option {unknown}"));
    }
    Ok(options)
}

fn required_path(options: &BTreeMap<String, String>, name: &str) -> Result<PathBuf, String> {
    options
        .get(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing required option {name}"))
}

fn publish_atomically(output: &Path, bytes: &[u8]) -> Result<(), String> {
    write_atomically(output, bytes, |temporary| {
        let dictionary = Dictionary::open(temporary)
            .map_err(|error| format!("generated dictionary validation failed: {error}"))?;
        dictionary
            .provenance()
            .map_err(|error| format!("generated dictionary provenance failed: {error}"))?;
        Ok(())
    })
}

// Atomic file publication is shared, but each protocol owns its validation.
// Postal's TSV/manifest have already been checked together by the codec; they
// are not dictionary binaries and must never go through Dictionary::open.
//
// The bytes are written to a sibling temporary file, synced, validated and
// then renamed over `output`, so a reader sees the old file or the complete
// new one. The parent directory is not synced. Two outputs written in turn
// (the postal TSV and manifest) are two separate renames.
fn write_atomically(
    output: &Path,
    bytes: &[u8],
    validate: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let file_name = output
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("invalid output path {}", output.display()))?;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".{file_name}.partial-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        drop(file);
        validate(&temporary)?;
        fs::rename(&temporary, output).map_err(|error| {
            format!(
                "cannot publish {} as {}: {error}",
                temporary.display(),
                output.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// The seed generator is offline. Core's file codec is also used by updates,
/// so generating bundled data cannot drift into a second format definition.
fn postal_seed(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let opts = parse_options(
        arguments,
        &[
            "--csv",
            "--zip-sha256",
            "--last-modified",
            "--retrieved-at",
            "--output-dir",
        ],
    )?;
    use std::io::Read;
    // A developer-supplied file is still an external boundary. Do not read an
    // unbounded CSV into memory before the shared streaming codec can reject it.
    let mut input = Vec::new();
    std::fs::File::open(required_path(&opts, "--csv")?)
        .map_err(|e| e.to_string())?
        .take((muu_session::MAX_POSTAL_CSV_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|e| e.to_string())?;
    if input.len() > muu_session::MAX_POSTAL_CSV_BYTES {
        return Err("file_too_large".into());
    }
    let snapshot = muu_session::decode_postal_code_csv(&input[..]).map_err(|e| e.to_string())?;
    let tsv = muu_session::encode_postal_code_tsv(&snapshot).map_err(|e| e.to_string())?;
    let get = |key: &str| {
        opts.get(key)
            .cloned()
            .ok_or_else(|| format!("missing {key}"))
    };
    let manifest = muu_session::PostalCodeManifest {
        schema_version: 1,
        source_url: muu_session::POSTAL_SOURCE_URL.into(),
        source_page_url: muu_session::POSTAL_SOURCE_PAGE_URL.into(),
        source_last_modified: get("--last-modified")?,
        retrieved_at: get("--retrieved-at")?,
        zip_sha256: get("--zip-sha256")?,
        // Session validates these as lowercase hex and recomputes the
        // snapshot hash; the shared spelling keeps the manifest acceptable.
        csv_sha256: sha256_hex(&input),
        snapshot_sha256: sha256_hex(&tsv),
        entry_count: snapshot.entries.len(),
    };
    let canonical =
        muu_session::encode_postal_code_manifest(&manifest).map_err(|e| e.to_string())?;
    muu_session::decode_postal_code_manifest(&canonical, &tsv, snapshot.entries.len())
        .map_err(|e| e.to_string())?;
    let pretty =
        muu_session::encode_postal_code_seed_manifest(&manifest).map_err(|e| e.to_string())?;
    let dir = required_path(&opts, "--output-dir")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    write_atomically(&dir.join("postal-code-seed.tsv"), &tsv, |_| Ok(()))?;
    write_atomically(&dir.join("postal-code-seed-manifest.json"), &pretty, |_| {
        Ok(())
    })?;
    println!(
        "entries={} snapshot_sha256={}",
        snapshot.entries.len(),
        manifest.snapshot_sha256
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_parser_rejects_duplicate_and_unknown_inputs() {
        assert_eq!(
            parse_options(
                ["--output", "one", "--output", "two"]
                    .map(str::to_owned)
                    .into_iter(),
                &["--output"],
            )
            .unwrap_err(),
            "duplicate option --output"
        );
        assert_eq!(
            parse_options(
                ["--surprise", "value"].map(str::to_owned).into_iter(),
                &["--output"],
            )
            .unwrap_err(),
            "unknown option --surprise"
        );
    }

    #[test]
    fn postal_seed_publishes_protocol_files_and_bounds_csv_before_output() {
        let root = tempfile::tempdir().unwrap();
        let csv = root.path().join("self.csv");
        let out = root.path().join("seed");
        std::fs::write(&csv, "\"\",\"\",\"1000001\",\"\",\"\",\"\",\"東京都\",\"千代田区\",\"か\u{3099}\",\"\",\"\",\"\",\"\",\"\",\"\"\r\n").unwrap();
        let args = |out: &Path| {
            vec![
                "--csv".into(),
                csv.display().to_string(),
                "--zip-sha256".into(),
                "1".repeat(64),
                "--last-modified".into(),
                "self".into(),
                "--retrieved-at".into(),
                "2026-07-31T07:59:02Z".into(),
                "--output-dir".into(),
                out.display().to_string(),
            ]
        };
        postal_seed(args(&out).into_iter()).unwrap();
        let tsv = std::fs::read(out.join("postal-code-seed.tsv")).unwrap();
        assert_eq!(
            tsv,
            "# muu-postal-code\t1\n1000001\t東京都千代田区が\n".as_bytes()
        );
        let manifest = std::fs::read(out.join("postal-code-seed-manifest.json")).unwrap();
        assert!(manifest.ends_with(b"\n"));
        assert!(muu_session::decode_postal_code_manifest(&manifest, &tsv, 1).is_ok());
        let rejected = root.path().join("rejected");
        std::fs::File::create(&csv)
            .unwrap()
            .set_len((muu_session::MAX_POSTAL_CSV_BYTES + 1) as u64)
            .unwrap();
        assert_eq!(
            postal_seed(args(&rejected).into_iter()),
            Err("file_too_large".into())
        );
        assert!(!rejected.exists());
    }
}
