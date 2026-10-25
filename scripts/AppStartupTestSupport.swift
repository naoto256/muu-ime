import AppKit
import InputMethodKit

// Link-time doubles observe the real AppDelegate. They do not run Core, open a
// store, register an input method, or mirror the delegate's startup decisions.
// The separate store/ABI process test covers the real persistence boundary.
//
// They are compiled into the same module as AppDelegate.swift in place of the
// real files, so the delegate's unchanged source resolves RustCore, the
// stores, AboutStatusProvider and IMKServer to these. Each records an event;
// the test asserts on that sequence. `mode` picks which startup branch the
// doubles drive.
@MainActor enum StartupFixture {
    static var mode = ""
    static var events: [String] = []
    static let learning = Data([0x00, 0xff, 0x31])
    static let userFile = Data([0x00, 0xfe, 0x32])
    static var acceptedResult: FixtureInitializationResult?
    static var about: FixtureInitialization?
    static var aboutStores: Set<String> = []
    static func record(_ event: String) { events.append(event) }
}

struct FixtureInitializationResult: Equatable { let degraded: Bool }
enum FixtureInitialization { case ready, failed, abiMismatch }
enum FixtureStartupFileState { case present }
@MainActor struct FixtureStartupFile {
    let state = FixtureStartupFileState.present
    let bytes = StartupFixture.userFile
}
struct FixtureCoreConfiguration { let state: FixtureStartupFileState }
enum FixtureFailure: Error { case initialization }

// abi-mismatch reports 11, so the delegate must take its mismatch branch.
@MainActor func muu_abi_version() -> UInt32 {
    StartupFixture.record("abi")
    return StartupFixture.mode == "abi-mismatch" ? 12 : 13
}
private let fixtureVersion = strdup("startup-fixture")!
func muu_core_version() -> UnsafePointer<CChar> { UnsafePointer(fixtureVersion) }

@MainActor enum RustCore {
    static func defaultConfiguration(userDictionaryFileState: FixtureStartupFileState) -> FixtureCoreConfiguration {
        StartupFixture.record("configuration")
        return .init(state: userDictionaryFileState)
    }
    // The bytes must arrive exactly as the stores handed them over. Both are
    // opaque here; the delegate must not parse, replace or drop either.
    static func initialize(_ configuration: FixtureCoreConfiguration, learning: Data?, userDictionaryFile: Data) -> Result<FixtureInitializationResult, FixtureFailure> {
        precondition(configuration.state == .present)
        precondition(learning == StartupFixture.learning && userDictionaryFile == StartupFixture.userFile,
            "the delegate must pass both opaque startup inputs before overlay loading")
        StartupFixture.record("initialize")
        if StartupFixture.mode == "initialization-failure" { return .failure(.initialization) }
        return .success(.init(degraded: StartupFixture.mode == "degraded-success"))
    }
}

@MainActor final class PersonalFrequencyStore {
    static let shared = PersonalFrequencyStore()
    private init() { StartupFixture.record("learning.construct") }
    var startupData: Data? { StartupFixture.record("learning.read"); return StartupFixture.learning }
    var shutdownQueryCount = 0
    enum SaveIssue { case durabilityWarning }
    struct PersistenceStatus { var saveIssue: SaveIssue? }
    var persistenceStatus = PersistenceStatus()
    func acceptStartup(_ result: FixtureInitializationResult) {
        StartupFixture.record("learning.accept")
        StartupFixture.acceptedResult = result
    }
    func beginShutdown(deadline: TimeInterval, now: () -> TimeInterval, completion: (ShutdownWorkResult) -> Void) {
        precondition(deadline >= now())
        shutdownQueryCount += 1
        StartupFixture.record("learning.shutdown")
        completion(.settled)
    }
}

@MainActor final class UserDictionaryStore {
    static let shared = UserDictionaryStore()
    private init() { StartupFixture.record("user.construct") }
    var startupFile: FixtureStartupFile { StartupFixture.record("user.read"); return .init() }
    func acceptInitializedCore() throws { StartupFixture.record("user.accept") }
    func beginShutdown(completion: (ShutdownWorkResult) -> Void) {
        StartupFixture.record("user.shutdown"); completion(.settled)
    }
}

@MainActor final class CommitStreamRecorder {
    static let shared = CommitStreamRecorder()
    private init() { StartupFixture.record("recorder.construct") }
    struct Status { let pendingCount = 0; let discarded: UInt64 = 0; let reportPending = false }
    let status = Status()
    func beginShutdown(deadline: TimeInterval, completion: (ShutdownWorkResult) -> Void) {
        StartupFixture.record("recorder.shutdown"); completion(.settled)
    }
    func expireShutdown() { StartupFixture.record("recorder.expire") }
}

@MainActor final class PostalCodeStore {
    static let shared = PostalCodeStore()
    private init() { StartupFixture.record("postal.construct") }
    func start() { StartupFixture.record("postal.start") }
    func beginShutdown(completion: (ShutdownWorkResult) -> Void) {
        StartupFixture.record("postal.shutdown"); completion(.settled)
    }
}

@MainActor final class ImportedDictionaryStore {
    static let shared = ImportedDictionaryStore()
    private init() { StartupFixture.record("imported.construct") }
    func start() { StartupFixture.record("imported.start") }
    func beginShutdown(completion: (ShutdownWorkResult) -> Void) {
        StartupFixture.record("imported.shutdown"); completion(.settled)
    }
}

// Records which state About receives and which stores it can query, so a
// branch that passes the wrong state or omits a started store fails.
@MainActor final class AboutStatusProvider {
    static let shared = AboutStatusProvider()
    func configureModelDownload(_ store: ModelDownloadStore) { StartupFixture.record("model.about") }
    struct Build { init(bundle: Bundle) {} }
    func configure(initialization: FixtureInitialization, build: Build,
                   learning: PersonalFrequencyStore? = nil, user: UserDictionaryStore? = nil,
                   imported: ImportedDictionaryStore? = nil, postal: PostalCodeStore? = nil,
                   recorder: CommitStreamRecorder? = nil) {
        StartupFixture.record("about")
        StartupFixture.about = initialization
        StartupFixture.aboutStores = Set([
            learning == nil ? nil : "learning", user == nil ? nil : "user",
            imported == nil ? nil : "imported", postal == nil ? nil : "postal",
            recorder == nil ? nil : "recorder",
        ].compactMap { $0 })
    }
}

@MainActor final class ModelDownloadStore {
    static let shared = ModelDownloadStore()
    private init() { StartupFixture.record("model.construct") }
    func start() { StartupFixture.record("model.start") }
    func beginShutdown(completion: (ShutdownWorkResult) -> Void) {
        StartupFixture.record("model.shutdown"); completion(.settled)
    }
}

// A module-local alias shadows the imported IMK type. The actual production
// constructor call and its ordering are exercised without registering IMKServer.
typealias IMKServer = StartupServerFixture
@MainActor final class StartupServerFixture {
    init(name: String, bundleIdentifier: String) {
        // The delegate names the server from Bundle.main: the recipe's bundle.
        precondition(bundleIdentifier == "dev.muu.test.Startup")
        precondition(name == "dev.muu.test.Startup_Connection")
        StartupFixture.record("server")
    }
}
