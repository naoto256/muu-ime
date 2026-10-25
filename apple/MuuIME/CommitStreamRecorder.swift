import Foundation
import Darwin
import OSLog

struct CommitStreamProcessConfiguration: Encodable, Equatable {
    let schema: String
    let processNonce: String
    let sourceCommit: String
    let build: UInt64
    init(schema: String = CommitStreamRecorder.schema, processNonce: String, sourceCommit: String, build: UInt64) {
        self.schema = schema
        self.processNonce = processNonce
        self.sourceCommit = sourceCommit
        self.build = build
    }
    enum CodingKeys: String, CodingKey {
        case schema, processNonce = "process_nonce", sourceCommit = "source_commit", build
    }
    // Process initialization metadata only. Rust creates Session nonces and
    // takes every other provenance fact from the conversion itself (docs/src/internals/storage.md「記録」).
    func makeEngineConfiguration() -> CommitStreamEngineConfiguration { self }
}
typealias CommitStreamEngineConfiguration = CommitStreamProcessConfiguration

/// What About shows for recording. `discarded` counts records finally
/// discarded in this process, not the loss weight they carried.
struct CommitStreamRecorderStatus: Sendable {
    // False when this build cannot record at all; About shows that apart from
    // "off by preference". The default true describes a recording-capable
    // build for tests that construct a status or recorder directly.
    let availableInBuild: Bool
    let requested: Bool
    let configured: Bool
    let stopped: Bool
    let pendingCount: Int
    let retrying: Bool
    let discarded: UInt64
    let reportPending: Bool
    let reportUnreportable: Bool
    let closing: Bool
    let metadataFailed: Bool
    init(requested: Bool, configured: Bool, stopped: Bool, pendingCount: Int = 0,
         retrying: Bool = false, discarded: UInt64 = 0, reportPending: Bool = false,
         reportUnreportable: Bool = false, closing: Bool = false, metadataFailed: Bool = false,
         availableInBuild: Bool = true) {
        self.availableInBuild = availableInBuild
        self.requested = requested; self.configured = configured; self.stopped = stopped
        self.pendingCount = pendingCount; self.retrying = retrying; self.discarded = discarded
        self.reportPending = reportPending; self.reportUnreportable = reportUnreportable; self.closing = closing; self.metadataFailed = metadataFailed
    }
}

/// Persists Rust-owned commit-stream JSON without entering the IMK key path.
///
/// This is deliberately a single-purpose local dogfood sink. It has no export
/// or networking surface. Only a dogfood build can read the opt-in preference.
final class CommitStreamRecorder {
    static let enabledDefaultsKey = "MuuCommitStreamEnabled"
    /// Requested from Rust at initialization and used to name output files.
    /// Payloads are opaque here; their schema is not decoded or validated (docs/src/internals/storage.md「記録」).
    static let schema = "muu.commit-stream.v5"
    static let shared = CommitStreamRecorder.makeProcessRecorder()

    private let processConfiguration: CommitStreamProcessConfiguration?
    private let recordingRequested: Bool
    private let recordingAvailableInBuild: Bool
    private let statusLock = NSLock()

    private let logger = Logger(
        subsystem: MuuConstants.bundleIdentifier,
        category: "commit-stream"
    )
    private let writer = DispatchQueue(
        label: "dev.muu.inputmethod.Muu.commit-stream",
        qos: .utility
    )
    private let directoryURL: URL?
    private let fileManager: FileManager
    private let moveLegacyFile: (URL, URL) throws -> Void
    private let reporter: CommitStreamLossReporter
    private var delivery: CommitStreamDelivery!
    private var metadataFailed = false
    private let clock: CommitStreamClock
    private var shutdownCompletion: ((ShutdownWorkResult) -> Void)?
    private var shutdownStarted = false
    private var shutdownProgress: ShutdownProgressSignal?
    #if MUU_TEST
        private var failureForTesting: String?
        private var injectWriteFailureForTesting = false
        private var failureLogCountForTesting = 0
    #endif

    // The app's only entry, reached through `shared`. It is private and takes
    // no arguments; only a matching dogfood build can read the main bundle
    // preference and resolve the user's Application Support directory.
    private static func makeProcessRecorder() -> CommitStreamRecorder {
        buildProcessRecorder(
            readBool: { UserDefaults.standard.bool(forKey: $0) },
            bundle: { .main },
            applicationSupportDirectory: {
                FileManager.default.urls(
                    for: .applicationSupportDirectory,
                    in: .userDomainMask
                ).first
            }
        )
    }

    // Test-only seam into the same decision. It exists only under MUU_TEST;
    // commit-stream-recorder-test also compiles a caller without the flag and
    // requires that build to fail on exactly this missing member.
    #if MUU_TEST
    static func makeProcessRecorderForTesting(
        readBool: (String) -> Bool,
        bundle: Bundle,
        applicationSupportDirectory: () -> URL?
    ) -> CommitStreamRecorder {
        buildProcessRecorder(
            readBool: readBool,
            bundle: { bundle },
            applicationSupportDirectory: applicationSupportDirectory
        )
    }
    #endif

    // Both callers use this decision. The compiled capability and bundle
    // variant precede preferences; an absent dogfood preference keeps storage off.
    // UserDefaults.bool reads an absent key as false. The lazy bundle is never
    // resolved by standard; storage is resolved only after dogfood opts in.
    private static func buildProcessRecorder(
        readBool: (String) -> Bool,
        bundle: () -> Bundle,
        applicationSupportDirectory: () -> URL?
    ) -> CommitStreamRecorder {
        // Reject distribution builds before reading preferences or resolving
        // storage. Changing defaults or just the plist cannot grant access.
        // Defaults are writable by any same-user process, so they may only
        // narrow what the build allows (see MuuBuildCapabilities).
        // The plist must also say dogfood: a dogfood binary in a bundle
        // labelled standard, or an unknown label, is not what the installer
        // and verifiers reviewed, so it records nothing either.
        guard MuuBuildCapabilities.isDogfood else {
            return CommitStreamRecorder(processConfiguration: nil, directoryURL: nil, availableInBuild: false)
        }
        let buildBundle = bundle()
        guard buildBundle.object(forInfoDictionaryKey: "MuuBuildVariant") as? String == MuuBuildCapabilities.variant else {
            return CommitStreamRecorder(processConfiguration: nil, directoryURL: nil, availableInBuild: false)
        }
        let enabled = readBool(enabledDefaultsKey)
        guard enabled else {
            return CommitStreamRecorder(
                processConfiguration: nil,
                directoryURL: nil
            )
        }
        guard let configuration = productionConfiguration(bundle: buildBundle),
              let applicationSupport = applicationSupportDirectory()
        else {
            return CommitStreamRecorder(
                processConfiguration: nil,
                directoryURL: nil,
                recordingRequested: true
            )
        }
        let directory = applicationSupport
            .appendingPathComponent("MuuIME", isDirectory: true)
            .appendingPathComponent("Dogfood", isDirectory: true)
            .appendingPathComponent("CommitStream", isDirectory: true)
        return CommitStreamRecorder(
            processConfiguration: configuration,
            directoryURL: directory,
            recordingRequested: true
        )
    }

    init(
        processConfiguration: CommitStreamProcessConfiguration?,
        directoryURL: URL?,
        recordingRequested: Bool? = nil,
        fileManager: FileManager = .default,
        moveLegacyFile: @escaping (URL, URL) throws -> Void = CommitStreamRecorder.renameWithoutReplacing,
        lastByteReader: ((URL) throws -> UInt8?)? = nil,
        clock: CommitStreamClock = .live,
        reportDropped: ((Data) -> Int32)? = nil,
        checkpoint: @escaping (String) throws -> Void = { _ in },
        availableInBuild: Bool = true
    ) {
        self.processConfiguration = processConfiguration
        self.clock = clock
        self.recordingRequested = recordingRequested ?? (processConfiguration != nil)
        self.recordingAvailableInBuild = availableInBuild
        self.directoryURL = directoryURL?.standardizedFileURL
        self.fileManager = fileManager
        self.moveLegacyFile = moveLegacyFile
        self.reporter = CommitStreamLossReporter(clock: clock, report: reportDropped)
        let files = directoryURL.map { CommitStreamFileWriter(directory: $0, checkpoint: checkpoint, tailReader: lastByteReader) }
        let queue = writer
        self.delivery = CommitStreamDelivery(clock: clock,
            execute: { queue.async(execute: DispatchWorkItem(block: $0)) }, reporter: reporter,
            event: { [weak self] phase in self?.log(phase) }, write: { [weak self] data in
                #if MUU_TEST
                if let self, self.statusLock.withLock({ () -> Bool in
                    let fail = self.injectWriteFailureForTesting; self.injectWriteFailureForTesting = false; return fail
                }) { throw POSIXError(.EIO) }
                #endif
                try files?.write(data)
            })
        // FIFO puts migration before every append without delaying IMK.
        // Failure leaves v4 in place and never blocks v5 (docs/src/internals/storage.md「記録」).
        writer.async { [weak self] in self?.archiveV4IfPresent() }
    }

    func makeEngineConfiguration() -> CommitStreamEngineConfiguration? {
        processConfiguration?.makeEngineConfiguration()
    }

    /// Memory facts only: main never waits for IO, the timer, or a Core ACK.
    var status: CommitStreamRecorderStatus {
        let state = delivery.status, debt = reporter.status
        return .init(requested: recordingRequested,
            configured: processConfiguration != nil && directoryURL != nil,
            stopped: state.stopped, pendingCount: state.count, retrying: state.retrying,
            discarded: state.discarded, reportPending: debt.pending,
            reportUnreportable: debt.unreportable, closing: state.closing, metadataFailed: !canDrain, availableInBuild: recordingAvailableInBuild)
    }
    var canDrain: Bool { statusLock.withLock { !metadataFailed } }
    func connectCore(reportDropped: @escaping (Data) -> Int32) { reporter.connect(reportDropped) }
    func append(_ record: Data, weight: UInt64 = 1) {
        // Returning without a weight is safe only because an unconfigured
        // recorder gives Core no commit-stream setting, so nothing is queued.
        guard processConfiguration != nil, directoryURL != nil else { return }
        delivery.append(record, weight: weight)
    }
    func discardUnreadableMetadata(_ metadata: Data) {
        let first = statusLock.withLock { () -> Bool in
            guard !metadataFailed else { return false }
            metadataFailed = true; return true
        }
        if first { delivery.discardUnreadable(metadata) }
    }
    func beginShutdown() { beginShutdown(deadline: clock.now() + 2) { _ in } }
    func beginShutdown(deadline: TimeInterval, completion: @escaping (ShutdownWorkResult) -> Void) {
        let first = statusLock.withLock { () -> Bool in
            guard !shutdownStarted else { return false }
            shutdownStarted = true; shutdownCompletion = completion; return true
        }
        if first {
            let signal = ShutdownProgressSignal(clock: clock) { [weak self] in self?.finishShutdownIfSettled() }
            shutdownProgress = signal
            reporter.observeShutdown(signal); delivery.observeShutdown(signal)
        }
        delivery.beginShutdown(deadline: deadline)
        finishShutdownIfSettled()
    }
    /// Called by the process deadline before it replies to AppKit. Waiting
    /// ownership is settled synchronously; this starts neither IO nor Core.
    func expireShutdown() {
        delivery.expireShutdown(); reporter.expireShutdown()
        finishShutdownIfSettled()
    }
    private func finishShutdownIfSettled() {
        let state = delivery.status, debt = reporter.status
        guard state.closing, state.count == 0, reporter.shutdownSettled else { return }
        let completion = statusLock.withLock { let action = shutdownCompletion; shutdownCompletion = nil; return action }
        guard let completion else { return }
        shutdownProgress?.cancel()
        completion(debt.pending || state.stopped ? .failed : .settled)
    }

    private func log(_ phase: String) {
        // Fixed categories only; neither opaque bytes nor storage paths enter
        // diagnostics. A recurring IO failure logs on the episode transition.
        logger.error("Commit stream delivery; phase=\(phase, privacy: .public)")
        #if MUU_TEST
        statusLock.withLock {
            failureForTesting = phase
            if phase == "retrying" || phase == "unsafe_path" { failureLogCountForTesting += 1 }
        }
        #endif
    }
    #if MUU_TEST
        func waitUntilIdleForTesting() {
            writer.sync {} // Includes one-time v4 migration.
            delivery.waitUntilIdleForTesting()
            writer.sync {} // Includes the content-free transition log.
        }
        var disabledForProcessForTesting: Bool { delivery.status.stopped }
        var failureForTestingDescription: String? { statusLock.withLock { failureForTesting } }
        var failureLogCount: Int { statusLock.withLock { failureLogCountForTesting } }
        func failNextWriteForTesting() { statusLock.withLock { injectWriteFailureForTesting = true } }
    #endif

    private static func renameWithoutReplacing(_ source: URL, _ destination: URL) throws {
        // No-overwrite must be atomic, not a racing destination preflight (docs/src/internals/storage.md「記録」).
        let result = source.path.withCString { src in
            destination.path.withCString { dst in renamex_np(src, dst, UInt32(RENAME_EXCL)) }
        }
        if result != 0 { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
    }

    /// Moves v4 files out of the directory v5 writes to, once per launch and
    /// only when recording is enabled, so the two formats never share a file
    /// set. The name pattern limits the move to numbered v4 files: v5 files,
    /// the archive itself and anything else are left alone. A fresh directory
    /// is created just before a verified regular file can be moved; a failed
    /// launch leaves no empty UUID directories. Each file is moved on
    /// its own; a file that cannot be moved stays where it was, and the
    /// failure does not disable v5 recording.
    private func archiveV4IfPresent() {
        dispatchPrecondition(condition: .onQueue(writer))
        guard processConfiguration != nil, let directoryURL else { return }
        var failures = 0
        do {
            try rejectSymbolicLinkAncestors(of: directoryURL)
            guard fileManager.fileExists(atPath: directoryURL.path) else { return }
            let names = try fileManager.contentsOfDirectory(atPath: directoryURL.path)
                .filter { $0.range(of: #"^commit-stream-v4-[0-9]+\.jsonl$"#, options: .regularExpression) != nil }
            guard !names.isEmpty else { return }
            let archive = directoryURL.appendingPathComponent("archive", isDirectory: true)
            let family = archive.appendingPathComponent("v4", isDirectory: true)
            let destination = family.appendingPathComponent(UUID().uuidString.lowercased(), isDirectory: true)
            var createdDestination = false
            for name in names {
                let source = directoryURL.appendingPathComponent(name)
                do {
                    let attributes = try fileManager.attributesOfItem(atPath: source.path)
                    guard attributes[.type] as? FileAttributeType == .typeRegular else { failures += 1; continue }
                    try rejectSymbolicLink(source)
                    if !createdDestination {
                        try prepareDirectory(archive)
                        try prepareDirectory(family)
                        // Exclusive creation proves that cleanup owns this UUID
                        // directory; never remove a preexisting destination (docs/src/internals/storage.md「記録」).
                        let result = destination.path.withCString { mkdir($0, 0o700) }
                        guard result == 0 else {
                            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
                        }
                        createdDestination = true
                        try fileManager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: destination.path)
                    }
                    try moveLegacyFile(source, destination.appendingPathComponent(name))
                } catch { failures += 1 }
            }
            if createdDestination {
                // rmdir removes only an empty directory. A successful move (or
                // anything else placed here) prevents deletion; archives survive.
                _ = destination.path.withCString { rmdir($0) }
            }
        } catch { failures += 1 }
        if failures > 0 { logger.error("Commit stream archive incomplete; category=v4_archive_failure count=\(failures)") }
    }

    private func prepareDirectory(_ directory: URL) throws {
        try rejectSymbolicLinkAncestors(of: directory)
        try fileManager.createDirectory(
            at: directory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
        try rejectSymbolicLink(directory)
        try fileManager.setAttributes(
            [.posixPermissions: 0o700],
            ofItemAtPath: directory.path
        )

    }

    private func rejectSymbolicLink(_ url: URL) throws {
        try FileSafety.rejectSymbolicLink(
            url,
            fileManager: fileManager,
            symbolicLinkError: CommitStreamWriteError.symbolicLink
        )
    }

    private func rejectSymbolicLinkAncestors(of url: URL) throws {
        try FileSafety.rejectSymbolicLinkAncestors(
            of: url,
            fileManager: fileManager,
            nonAbsolutePathError: { CommitStreamWriteError.nonAbsolutePath },
            symbolicLinkError: CommitStreamWriteError.symbolicLink
        )
    }

    private static func productionConfiguration(
        bundle: Bundle,
        processNonce: String = UUID().uuidString.lowercased()
    ) -> CommitStreamProcessConfiguration? {
        guard let sourceCommit = bundle.object(
            forInfoDictionaryKey: "MuuSourceCommit"
        ) as? String,
        sourceCommit.count == 40,
        sourceCommit.allSatisfy(\.isHexDigit),
        let buildString = bundle.object(forInfoDictionaryKey: "CFBundleVersion") as? String,
        let build = UInt64(buildString)
        else {
            return nil
        }
        return CommitStreamProcessConfiguration(processNonce: processNonce, sourceCommit: sourceCommit, build: build)
    }

    #if MUU_TEST
        /// Test-only access to the production factory, which resolves
        /// solely from bundle resources.
        static func productionConfigurationForTesting(
            bundle: Bundle,
            processNonce: String = "test-native-process"
        ) -> CommitStreamProcessConfiguration? {
            productionConfiguration(bundle: bundle, processNonce: processNonce)
        }
    #endif

}

private enum CommitStreamWriteError: Error {
    case nonAbsolutePath
    case symbolicLink(String)
}
