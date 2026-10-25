//! Fixtures shared by the crate's unit tests: small dictionaries built into
//! temporary files, request builders, and session and Core factories with
//! the production wiring. Compiled only for tests.
mod dictionaries;

use muu_conversion::Segmenter;
use std::collections::BTreeSet;

use std::fs;
use std::sync::Arc;

use muu_conversion::{
    Candidate, PathCandidate, PathNode, ScalarRange, Segments, SnapshotRevision, TokenKey,
};
use muu_dict::{
    DictionaryBuilder, DictionaryTokenId, LeftContextId, LexiconEntry, PosId, RightContextId,
    UnknownClass, UnknownDefinition,
};
use tempfile::NamedTempFile;

use crate::session::*;
use crate::*;
use muu_dict::CoarsePos;
use std::path::{Path, PathBuf};

pub(crate) const COMMON_ROMAJI_COMPATIBILITY: &str =
    include_str!("../../muu-romaji/data/common-compatibility.tsv");

#[derive(Default)]
pub(crate) struct TestSessionConfig {
    pub(crate) conversion_lm_dir: Option<PathBuf>,
    pub(crate) commit_stream_config: Option<CommitStreamConfig>,
    pub(crate) user_dictionary: UserDictionarySnapshot,
    pub(crate) personal_frequency: PersonalFrequencySnapshot,
    pub(crate) vocabulary: SharedVocabulary,
}

#[derive(Clone, Copy)]
pub(crate) struct TestDictionary;

impl TestDictionary {
    pub(crate) fn path(self) -> &'static Path {
        static DICTIONARY: std::sync::OnceLock<NamedTempFile> = std::sync::OnceLock::new();
        DICTIONARY.get_or_init(build_conversion_dictionary).path()
    }
}

pub(crate) trait TestDictionaryPath {
    fn test_dictionary_path(&self) -> &Path;
}

impl TestDictionaryPath for TestDictionary {
    fn test_dictionary_path(&self) -> &Path {
        self.path()
    }
}

impl TestDictionaryPath for NamedTempFile {
    fn test_dictionary_path(&self) -> &Path {
        self.path()
    }
}

pub(crate) fn learning_snapshot(session: &Session) -> Option<PersonalFrequencySnapshot> {
    session
        .core
        .personal_frequency_update(Some(LearningVersion::default()))
        .unwrap()
        .map(|update| serde_json::from_slice(&update.data).unwrap())
}

pub(crate) fn common_romaji_compatibility_entries()
-> Vec<(&'static str, &'static str, &'static str)> {
    COMMON_ROMAJI_COMPATIBILITY
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split('\t');
            let input = fields.next().expect("compatibility input");
            let output = fields.next().expect("compatibility output");
            let next = fields.next().unwrap_or("");
            assert!(
                fields.next().is_none(),
                "unexpected compatibility field: {line}"
            );
            (input, output, next)
        })
        .collect()
}

pub(crate) fn conversion_candidate(
    range: ScalarRange,
    surface: &str,
    cost: i32,
    token: u64,
) -> Candidate {
    Candidate::from_group(
        range,
        vec![PathCandidate {
            nodes: vec![PathNode {
                start: range.start,
                end: range.end,
                surface: surface.into(),
                left_id: LeftContextId::new(1),
                right_id: RightContextId::new(1),
                left_pos_id: PosId::new(1),
                right_pos_id: PosId::new(1),
                lexical_class: muu_dict::LexicalClass::General,
                token_key: TokenKey::Dictionary(DictionaryTokenId::new(token)),
                word_cost: cost,
                kind: muu_conversion::NodeKind::Known,
            }],
            cost,
            snapshot_revision: SnapshotRevision::new(1),
        }],
    )
    .unwrap()
}

pub(crate) fn key(character: &str, keycode: u16) -> Request {
    key_with_modifiers(character, keycode, 0)
}

pub(crate) fn key_with_modifiers(character: &str, keycode: u16, modifiers: u64) -> Request {
    Request::Key {
        key: KeyEvent {
            event_type: "key_down".to_owned(),
            keycode,
            modifiers,
            characters_utf8: Some(character.to_owned()),
            characters_ignoring_modifiers: Some(character.to_owned()),
            is_repeat: false,
        },
    }
}

pub(crate) fn flags_changed(keycode: u16, modifiers: u64) -> Request {
    Request::Key {
        key: KeyEvent {
            event_type: "flags_changed".to_owned(),
            keycode,
            modifiers,
            characters_utf8: None,
            characters_ignoring_modifiers: None,
            is_repeat: false,
        },
    }
}

pub(crate) fn command(name: &str, arg: serde_json::Value) -> Request {
    Request::Command {
        command: name.to_owned(),
        arg,
    }
}

pub(crate) fn user_dictionary_snapshot(
    entries: impl IntoIterator<Item = (&'static str, &'static str, UserDictionaryPriority)>,
) -> UserDictionarySnapshot {
    UserDictionarySnapshot {
        schema_version: USER_DICTIONARY_SCHEMA_VERSION,
        entries: entries
            .into_iter()
            .map(|(reading, surface, priority)| UserDictionaryRecord {
                reading: reading.to_owned(),
                surface: surface.to_owned(),
                part_of_speech: UserDictionaryPartOfSpeech::noun(),
                priority,
            })
            .collect(),
    }
}

pub(crate) fn postal_code_snapshot(
    entries: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> PostalCodeSnapshot {
    PostalCodeSnapshot {
        schema_version: POSTAL_CODE_SCHEMA_VERSION,
        entries: entries
            .into_iter()
            .map(|(reading, surface)| PostalCodeRecord {
                reading: reading.to_owned(),
                surface: surface.to_owned(),
            })
            .collect(),
    }
}

pub(crate) fn bundled_postal_code_snapshot() -> PostalCodeSnapshot {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apple/MuuIME/PostalCodeSeed/postal-code-seed.tsv");
    let text = std::fs::read_to_string(path).expect("bundled postal seed");
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("# muu-postal-code\t1"));
    let entries = lines
        .map(|line| {
            let (reading, surface) = line.split_once('\t').expect("postal seed row");
            PostalCodeRecord {
                reading: reading.to_owned(),
                surface: surface.to_owned(),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 124_512);
    PostalCodeSnapshot {
        schema_version: POSTAL_CODE_SCHEMA_VERSION,
        entries,
    }
}

pub(crate) fn session_from_core(
    core: Arc<Core>,
    config: TestSessionConfig,
) -> Result<Session, SessionError> {
    core.initialize_fixture_data(&config.user_dictionary, &config.personal_frequency)?;
    let _ = core.configure_sessions(CoreSessionSettings {
        conversion_lm_dir: config.conversion_lm_dir,
        conversion_lm_load_synchronously: true,
        commit_stream_config: config.commit_stream_config,
        ..Default::default()
    });
    Session::new(core)
}

pub(crate) fn new_session(
    dictionary_path: Option<&Path>,
    conversion_lm_dir: Option<&Path>,
    mut config: TestSessionConfig,
) -> Result<Session, SessionError> {
    config.conversion_lm_dir = conversion_lm_dir.map(Path::to_path_buf);
    let core = Arc::new(test_core(dictionary_path, config.vocabulary.clone())?);
    session_from_core(core, config)
}

fn test_core(
    dictionary_path: Option<&Path>,
    vocabulary: SharedVocabulary,
) -> Result<Core, SessionError> {
    // The real-asset file is immutable for this run. Reuse its validated mmap,
    // but construct every mutable store through the ordinary Core constructor.
    // Other paths retain production open/degradation behaviour, including
    // synthetic files that a test may intentionally rewrite.
    let real_path = std::env::var_os("MUU_REAL_DICT_PATH").map(PathBuf::from);
    if let Some(path) = dictionary_path.filter(|path| Some(*path) == real_path.as_deref()) {
        let mut core = Core::with_vocabulary(CoreConfig::default(), vocabulary)?;
        core.dictionary = Some(crate::real_dictionary::shared_at(path));
        Ok(core)
    } else {
        Core::with_vocabulary(CoreConfig { dictionary_path }, vocabulary)
    }
}

pub(crate) fn session_with_dictionary_path(path: Option<&Path>) -> Result<Session, SessionError> {
    new_session(
        path,
        None,
        TestSessionConfig {
            ..TestSessionConfig::default()
        },
    )
}

pub(crate) fn session_with_user_dictionary(
    dictionary_path: Option<&Path>,
    commit_stream_config: Option<CommitStreamConfig>,
    user_dictionary: UserDictionarySnapshot,
) -> Result<Session, SessionError> {
    new_session(
        dictionary_path,
        None,
        TestSessionConfig {
            conversion_lm_dir: None,
            commit_stream_config,
            user_dictionary,
            ..TestSessionConfig::default()
        },
    )
}

pub(crate) fn session_with_dictionaries(
    dictionary_path: Option<&Path>,
    commit_stream_config: Option<CommitStreamConfig>,
    user_dictionary: UserDictionarySnapshot,
    vocabulary: SharedVocabulary,
) -> Result<Session, SessionError> {
    session_with_shared_catalogs(
        dictionary_path,
        commit_stream_config,
        user_dictionary,
        vocabulary,
    )
}

pub(crate) fn session_with_catalogs(
    dictionary_path: Option<&Path>,
    commit_stream_config: Option<CommitStreamConfig>,
    user_dictionary: UserDictionarySnapshot,
    imported_dictionaries: ImportedDictionaryCatalogSnapshot,
    vocabulary: SharedVocabulary,
) -> Result<Session, SessionError> {
    let core = Arc::new(test_core(dictionary_path, vocabulary)?);
    core.set_imported_fixture(imported_dictionaries)
        .expect("synthetic imported files");
    session_from_core(
        core,
        TestSessionConfig {
            commit_stream_config,
            user_dictionary,
            ..TestSessionConfig::default()
        },
    )
}

pub(crate) fn session_with_shared_catalogs(
    dictionary_path: Option<&Path>,
    commit_stream_config: Option<CommitStreamConfig>,
    user_dictionary: UserDictionarySnapshot,
    vocabulary: SharedVocabulary,
) -> Result<Session, SessionError> {
    new_session(
        dictionary_path,
        None,
        TestSessionConfig {
            commit_stream_config,
            user_dictionary,
            vocabulary,
            ..TestSessionConfig::default()
        },
    )
}

pub(crate) fn commit_stream_session(dictionary: &impl TestDictionaryPath) -> Session {
    commit_stream_session_with_engine(dictionary, "test-engine")
}

pub(crate) fn commit_stream_session_with_engine(
    dictionary: &impl TestDictionaryPath,
    engine_nonce: &str,
) -> Session {
    new_session(
        Some(dictionary.test_dictionary_path()),
        None,
        TestSessionConfig {
            commit_stream_config: Some(test_commit_stream_config(engine_nonce)),
            ..TestSessionConfig::default()
        },
    )
    .expect("session data")
}

/// Recording configuration carries provenance, not candidate authority (docs/src/internals/storage.md「記録」).
pub(crate) fn test_commit_stream_config(_engine_nonce: &str) -> CommitStreamConfig {
    CommitStreamConfig {
        schema: commit_stream::COMMIT_STREAM_V5_SCHEMA.to_owned(),
        process_nonce: "test-process".to_owned(),
        source_commit: "0123456789abcdef".to_owned(),
        build: 180,
    }
}

pub(crate) fn commit_stream_dictionary() -> NamedTempFile {
    build_conversion_dictionary()
}

pub(crate) fn conversion_dictionary() -> TestDictionary {
    TestDictionary
}

pub(crate) fn build_conversion_dictionary() -> NamedTempFile {
    let mut builder =
        DictionaryBuilder::new(2_675, 2_675, RightContextId::new(0), LeftContextId::new(0))
            .unwrap();
    builder.provenance(b"name = \"session-conversion-test\"\n");
    for (surface, cost) in [("今日", 0), ("京", 10)] {
        builder.add_entry(LexiconEntry {
            reading: "きょう".into(),
            surface: surface.into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: CoarsePos::Noun.id(),
            right_pos_id: CoarsePos::Noun.id(),
            word_cost: cost,
        });
    }
    builder.add_entry(LexiconEntry {
        reading: "ふぁ".into(),
        surface: "ファ".into(),
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Noun.id(),
        right_pos_id: CoarsePos::Noun.id(),
        word_cost: 0,
    });
    builder.add_entry(LexiconEntry {
        reading: "じぇ".into(),
        surface: "ジェ".into(),
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Noun.id(),
        right_pos_id: CoarsePos::Noun.id(),
        word_cost: 0,
    });
    builder.add_unknown(UnknownDefinition {
        class: UnknownClass::Other,
        group: false,
        max_length: 1,
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Other.id(),
        right_pos_id: CoarsePos::Other.id(),
        word_cost: 1_000,
    });
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), builder.build().unwrap()).unwrap();
    file
}

pub(crate) fn multi_segment_dictionary() -> NamedTempFile {
    let mut builder =
        DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.provenance(b"name = \"session-multi-segment-test\"\n");
    for (reading, surface, pos, cost) in [
        ("あ", "亜", CoarsePos::Noun, 0),
        ("あ", "阿", CoarsePos::Noun, 10),
        ("い", "伊", CoarsePos::Particle, 0),
        ("う", "宇", CoarsePos::Noun, 0),
        ("う", "雨", CoarsePos::Noun, 10),
        ("え", "江", CoarsePos::Noun, 0),
        ("お", "於", CoarsePos::Particle, 0),
        ("か", "加", CoarsePos::Particle, 0),
    ] {
        builder.add_entry(LexiconEntry {
            reading: reading.into(),
            surface: surface.into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: pos.id(),
            right_pos_id: pos.id(),
            word_cost: cost,
        });
    }
    builder.add_unknown(UnknownDefinition {
        class: UnknownClass::Other,
        group: false,
        max_length: 1,
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Other.id(),
        right_pos_id: CoarsePos::Other.id(),
        word_cost: 1_000,
    });
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), builder.build().unwrap()).unwrap();
    file
}

pub(crate) fn constrained_resize_failure_dictionary() -> NamedTempFile {
    let mut builder =
        DictionaryBuilder::new(2, 2, RightContextId::new(0), LeftContextId::new(0)).unwrap();
    builder.provenance(b"name = \"session-resize-failure-test\"\n");
    for (reading, surface, cost) in [("あいう", "甲", 0), ("あいう", "丙", 10), ("え", "乙", 0)]
    {
        builder.add_entry(LexiconEntry {
            reading: reading.into(),
            surface: surface.into(),
            left_id: LeftContextId::new(1),
            right_id: RightContextId::new(1),
            left_pos_id: CoarsePos::Noun.id(),
            right_pos_id: CoarsePos::Noun.id(),
            word_cost: cost,
        });
    }
    builder.add_unknown(UnknownDefinition {
        class: UnknownClass::Other,
        group: true,
        max_length: 64,
        left_id: LeftContextId::new(1),
        right_id: RightContextId::new(1),
        left_pos_id: CoarsePos::Other.id(),
        right_pos_id: CoarsePos::Other.id(),
        word_cost: 10_000,
    });
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), builder.build().unwrap()).unwrap();
    file
}

// Production assets fix actual LM decisions; private learning/logs are never
// consulted. Share only the immutable dictionary; each call owns a fresh Core
// and runs its ordinary synchronous model loader, not a cached Core or oracle.
pub(crate) fn run_selection_core() -> Option<Arc<Core>> {
    let model = real_assets::real_asset_path("MUU_LM_MODEL_DIR")?;
    let dictionary = crate::real_dictionary::shared()?;
    let mut core = Core::new(CoreConfig::default()).unwrap();
    core.dictionary = Some(dictionary);
    let core = Arc::new(core);
    core.configure_sessions(CoreSessionSettings {
        conversion_lm_dir: Some(model),
        conversion_lm_load_synchronously: true,
        ..Default::default()
    })
    .unwrap_or_else(|_| panic!("first settings"));
    Some(core)
}

pub(crate) fn assert_partial_protection_through_scoring_and_application(
    input: &Segments,
    captured: &ConversionSnapshot,
    lm: &LanguageModelSnapshot,
    expected_run_prefix: &str,
    segmenter: &Segmenter,
) {
    let selected = selection::select(
        input,
        true,
        &captured.costs().unwrap(),
        &captured.learning.frequency,
        "",
        lm,
        segmenter,
    );
    assert_eq!(selected.report.runs.len(), 1);
    let run = &selected.report.runs[0];
    assert_eq!(run.protected_segments, vec![0]);
    assert_eq!(run.skipped, None);
    assert!(run.hypotheses.len() > 1);
    assert_ne!(run.selected_rank, run.baseline_rank);
    let frozen = &input.segments()[0];
    for h in &run.hypotheses {
        let original_path = &frozen.focused().paths()[0];
        assert_eq!(
            &h.hypothesis.nodes()[..original_path.nodes.len()],
            &original_path.nodes
        );
        assert!(
            h.hypothesis
                .surface()
                .starts_with(frozen.focused().surface())
        );
        assert!(h.surface.starts_with(expected_run_prefix));
    }
    assert_eq!(selected.segments.segments()[0], *frozen);
    assert_eq!(
        selected.segments.segments()[0].focused().id(),
        frozen.focused().id()
    );
    assert_eq!(
        selected.segments.segments()[0].focused().paths()[0],
        frozen.focused().paths()[0]
    );
    assert_eq!(
        selected.segments.segments()[0].focused().surface(),
        frozen.focused().surface()
    );
    let render = |segments: &Segments| {
        segments
            .segments()
            .iter()
            .map(|s| s.focused().surface())
            .collect::<String>()
    };
    assert_ne!(render(&selected.segments), render(input));
    assert_eq!(
        render(&selected.segments),
        run.hypotheses[run.selected_rank.unwrap()]
            .hypothesis
            .surface()
    );
}

pub(crate) fn selected_dictionary_tokens(session: &Session) -> BTreeSet<u64> {
    session
        .segments
        .as_ref()
        .unwrap()
        .focused()
        .unwrap()
        .focused()
        .paths()[0]
        .token_keys()
        .filter_map(|key| match key {
            TokenKey::Dictionary(id) => Some(id.get()),
            _ => None,
        })
        .collect()
}

// Retained contracts concern accepted commit boundaries and focus movement:
// only the explicitly selected path is observed in the current Core store.
pub(crate) fn assert_frequency_tokens(session: &Session, expected: &BTreeSet<u64>) {
    let learned = learning_snapshot(session).expect("selection must publish learning");
    assert_eq!(learned.observation_count, 1);
    assert!(!expected.is_empty());
    assert_eq!(
        learned
            .entries
            .iter()
            .map(|entry| entry.dictionary_token_id)
            .collect::<BTreeSet<_>>(),
        *expected
    );
    assert!(learned.entries.iter().all(|entry| entry.count == 1));
}
