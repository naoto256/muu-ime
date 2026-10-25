// Learning persistence, kept apart from the Core bridge in RustEngine.swift.
// The store touches Core only through the two closures its initializer takes
// (query and clear, defaulting to RustCore) and through the values Core hands
// it: LearningVersion and CoreInitializationResult. Everything else here is
// file IO and save bookkeeping, which the bridge never needs.
import Foundation
import OSLog

enum PersonalFrequencyStoreError: Error {
    case unsupportedFileType
    case fileTooLarge
    case io(operation: String, code: Int32)
}
// What `clear` committed. A precommit failure leaves current learning and
// Core unchanged, though some rejected backups may already be gone. A
// durability warning means every file was unlinked and Core cleared, but the
// directory sync failed. A runtime warning means the files are gone but Core
// kept the old learning, so persistence has stopped for this process.
enum PersonalFrequencyClearResult {
    case cleared
    case clearedWithDurabilityWarning(Error)
    case precommitFailure(Error)
    case committedWithRuntimeWarning(Error)
}

/// Memory-only observations. These never enter the opaque learning file and
/// never change the writer's debounce, retry, clear or generation contracts.
struct PersonalFrequencyPersistenceStatus: Sendable {
    enum FileState: Sendable { case unknown, present, absent, cleared }
    enum SavedAt: Equatable, Sendable { case fileModification(Date), completedSave(Date) }
    enum SaveIssue: Sendable { case writeFailed, durabilityWarning, clearDurabilityWarning }
    enum StopReason: Sendable { case unverifiable, quarantineFailed, clearRejected, unsafeSave, invalidSave }
    let initialized: Bool
    let stopped: Bool
    let stopReason: StopReason?
    let fileState: FileState
    let lastSavedAt: SavedAt?
    let pending: Bool
    let saveIssue: SaveIssue?
    let queryFailed: Bool
}

/// Only persistence state lives here. The blob's schema and dictionary identity
/// belong to Rust; Swift never decodes or constructs learning contents.
///
/// The core does not know whether anything was saved. This class keeps that
/// knowledge and follows the rules of docs/src/internals/storage.md「変換学習の保存」:
///
/// - All file operations run on one serial queue, so a write and a delete
///   never interleave.
/// - A newer version reschedules one write 250 ms after the last update; the
///   same version never extends the wait.
/// - A write is abandoned if its generation is older than the newest one known
///   here, so data from before a clear cannot return to disk.
/// - `poll`, `receive`, `acceptStartup` and `clear` are main-thread only. That
///   is what makes `clear` atomic with respect to commits: sessions also run
///   only on main.
/// - The one exception is the final query in `beginShutdown`, made on the
///   serial queue once closing has stopped `poll` and `receive`.
final class PersonalFrequencyStore {
    #if MUU_TEST
    static var receiveObserverForTesting: ((Data, LearningVersion) -> Void)?
    #endif
    // Test builds point the singleton at the working directory's target/, so a
    // fixture that reaches it never reads or writes the user's learning.
    static let shared: PersonalFrequencyStore = {
        #if MUU_TEST
            let base = URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true)
                .appendingPathComponent("target/swift-learning-fixtures/shared-\(getpid())", isDirectory: true)
        #else
            let base = FileManager.default.urls(
                for: .applicationSupportDirectory, in: .userDomainMask
            ).first!
        #endif
        return PersonalFrequencyStore(
            baseDirectory: base,
            bundleIdentifier: Bundle.main.bundleIdentifier ?? "dev.muu.inputmethod.Muu")
    }()
    private struct Update {
        let data: Data
        let version: LearningVersion
    }
    private final class Reservation {}
    private let logger = Logger(subsystem: "dev.muu.inputmethod.Muu", category: "PersonalFrequency")
    private let lock = NSLock()
    private let queue = DispatchQueue(label: "dev.muu.personal-frequency.persistence")
    private let fileURL: URL
    private let scheduler: (TimeInterval, @escaping () -> Void) -> Void
    private let writer: (Data, URL) throws -> Error?
    private let remover: (URL) throws -> Error?
    private let quarantiner: (URL) throws -> Void
    private let coreClear: () -> Result<LearningVersion, Error>
    private let query: (LearningVersion?) -> Result<(Data, LearningVersion)?, Error>
    private let clock: () -> Date
    // "Received" means the correct bytes for this version are held here (in
    // `pending`) or are already on disk. It is the cursor sent to the core, so
    // advancing it without holding the bytes would lose that version for good.
    // nil asks the core to return its state unconditionally.
    private var received: LearningVersion?
    // The newest version known to be on disk. Lags `received` by the debounce
    // window and by any failed write.
    private var saved: LearningVersion?
    // The newest bytes not yet written. Kept across a failed write so the
    // retry needs nothing from the core.
    private var pending: Update?
    // Identity of the one scheduled write that is still wanted. Replacing or
    // clearing it cancels a timer that has already been armed.
    private var reservation: Reservation?
    // Retain pending bytes indefinitely, but avoid repeatedly hitting an
    // unavailable disk. A newer version starts a fresh debounce window.
    private var retryDelay: TimeInterval = 0.250
    // Bumped by `clear`. A write that started before the clear compares this
    // afterward and leaves the cursors alone.
    private var persistenceEpoch: UInt64 = 0
    // The newest generation seen from Core. Bytes from any older one are
    // never written (see `writePending`).
    private var latestGeneration: UInt64?
    // Set when writing could overwrite a file that must be left alone, or put
    // deleted data back. Nothing is queried or written while it is set.
    // Only a successful `clear` lifts it: by then the file in question has
    // been deleted at the user's request.
    private var stopped = false
    private var warning: Error?
    private var startupAccepted = false
    private var closing = false
    private var finalQueryCount = 0
    // The queue owns completion; main only closes admission. Already-started
    // writes may still advance saved/time after closing, as normal IO facts.
    private var shutdownResult: ShutdownWorkResult = .settled
    var isClosing: Bool { lock.withLock { closing } }
    var shutdownQueryCount: Int { lock.withLock { finalQueryCount } }
    private var stopReason: PersonalFrequencyPersistenceStatus.StopReason?
    private var fileState = PersonalFrequencyPersistenceStatus.FileState.unknown
    private var lastSavedAt: PersonalFrequencyPersistenceStatus.SavedAt?
    private var saveIssue: PersonalFrequencyPersistenceStatus.SaveIssue?
    // A Core read failure is not a failed write. Conversely, reading Core
    // successfully cannot establish the durability of an earlier save.
    private var queryFailed = false
    // What startup passes to `RustCore.initialize`: nil for no file, the
    // bytes for a readable one, and empty Data for an unsafe or unreadable
    // path, which Rust rejects so that `acceptStartup` moves it aside.
    // Dropped once startup has been accepted.
    private(set) var startupData: Data?
    private(set) var loadError: Error?

    var receivedVersion: LearningVersion? {
        lock.lock()
        defer { lock.unlock() }
        return received
    }
    var savedVersion: LearningVersion? {
        lock.lock()
        defer { lock.unlock() }
        return saved
    }
    var durabilityWarning: Error? {
        lock.lock()
        defer { lock.unlock() }
        return warning
    }
    var persistenceStopped: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }

    var persistenceStatus: PersonalFrequencyPersistenceStatus {
        lock.lock()
        defer { lock.unlock() }
        return .init(initialized: startupAccepted, stopped: stopped, stopReason: stopReason,
            fileState: fileState, lastSavedAt: lastSavedAt, pending: pending != nil,
            saveIssue: saveIssue, queryFailed: queryFailed)
    }

    init(
        baseDirectory: URL, bundleIdentifier: String,
        scheduler: @escaping (TimeInterval, @escaping () -> Void) -> Void = { delay, work in
            DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + delay, execute: work)
        },
        persistenceOverride: ((Data, URL) throws -> Error?)? = nil,
        deletionOverride: ((URL) throws -> Void)? = nil,
        quarantineOverride: ((URL) throws -> Void)? = nil,
        coreClear: @escaping () -> Result<LearningVersion, Error> = RustCore.clearPersonalFrequency,
        query: @escaping (LearningVersion?) -> Result<(Data, LearningVersion)?, Error> = RustCore.personalFrequency,
        clock: @escaping () -> Date = Date.init
    ) {
        fileURL = baseDirectory.appendingPathComponent(bundleIdentifier, isDirectory: true)
            .appendingPathComponent("personal_frequency", isDirectory: true).appendingPathComponent(
                "personal-frequency-v1.json")
        self.scheduler = scheduler
        self.coreClear = coreClear
        self.query = query
        self.clock = clock
        writer =
            persistenceOverride ?? { data, url in
                guard data.count <= 512 * 1024 else { throw PersonalFrequencyStoreError.fileTooLarge }
                try Self.safePath(url)
                // The bundle folder and `personal_frequency/` become 0700 and
                // are linked durably; the caller's base keeps its mode. Unsafe
                // type or path failures stop; ordinary IO failures retry.
                // Over-limit bytes stop before any path is touched: startup
                // could not read them back (it reads at most 512 KiB).
                try FileSafety.ensurePrivateDirectories(url.deletingLastPathComponent(), below: baseDirectory,
                    unsupportedPath: { PersonalFrequencyStoreError.unsupportedFileType },
                    posixError: Self.posixError)
                #if MUU_TEST
                let checkpoint = Self.persistenceCheckpointForTesting
                #else
                let checkpoint: ((String) throws -> Void)? = nil
                #endif
                return try FileSafety.atomicWrite(
                    data, to: url, temporaryPrefix: ".personal-frequency",
                    directoryOpenFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
                    checkpoint: checkpoint, posixError: Self.posixError)
            }
        if let deletionOverride { remover = { try deletionOverride($0); return nil } }
        else { remover = Self.removeSavedLearning }
        quarantiner =
            quarantineOverride ?? { url in
                try Self.safePath(url)
                let destination = url.deletingLastPathComponent().appendingPathComponent(
                    "personal-frequency-rejected-\(UUID().uuidString).json")
                try Self.safePath(destination)
                guard Darwin.rename(url.path, destination.path) == 0 else {
                    throw Self.posixError("quarantine", errno)
                }
            }
        // Not FileSafety.readState: the modification date shown as the last
        // save must come from the same descriptor that read the bytes. The
        // classification is the same: only ENOENT from the open is absence,
        // and a safePath failure is unreadable. Absence is noted where the
        // error is made, because the thrown error is the store's own type.
        var missing = false
        do {
            try Self.safePath(fileURL)
            let read = try FileSafety.boundedReadWithMetadata(
                fileURL, maxBytes: 512 * 1024,
                unsupportedFileType: { PersonalFrequencyStoreError.unsupportedFileType },
                fileTooLarge: { PersonalFrequencyStoreError.fileTooLarge },
                posixError: { operation, code in
                    // Only a failed open can prove absence. A stat/read failure
                    // must not let Core accept empty learning as the baseline.
                    missing = operation == "open-read" && code == ENOENT
                    return Self.posixError(operation, code)
                })
            startupData = read.data
            fileState = .present
            lastSavedAt = read.modificationDate.map { .fileModification($0) }
        } catch {
            if missing {
                fileState = .absent
            } else {
                loadError = error
                // An unsafe or unreadable path must not be reported as absence.
                startupData = Data()
            }
        }
    }

    /// Establishes the persistence baseline from the initialization result.
    /// Call on main, right after `RustCore.initialize` and before any session
    /// can commit.
    ///
    /// - File accepted, or no file: the returned version is both received and
    ///   saved. An absent file already is the saved form of empty learning.
    /// - File rejected and moved aside: nothing is received, so the first
    ///   query returns the empty state and it gets written.
    /// - Dictionary unavailable: stop persisting without touching the file or
    ///   advancing either persistence baseline; its validity is unknown.
    /// - File rejected and could not be moved aside: stop persisting for this
    ///   process. "Rejected" includes a file that is valid for another
    ///   dictionary build, and writing would destroy it.
    func acceptStartup(_ result: CoreInitializationResult) {
        precondition(Thread.isMainThread)
        lock.lock()
        guard !closing else { lock.unlock(); return }
        startupAccepted = true
        lock.unlock()
        if result.degraded.personalFrequencyUnverifiable {
            // The saved bytes may be valid when the dictionary returns (docs/src/internals/storage.md「変換学習の保存」).
            // Preserve them in place; explicit successful clear can resume
            // persistence in a new generation, as for quarantine failure.
            lock.lock()
            stopped = true
            stopReason = .unverifiable
            latestGeneration = result.personalFrequencyVersion.generation
            lock.unlock()
            logger.error("Learning persistence stopped; code=learning_unverifiable")
        } else if result.degraded.personalFrequencyInvalid {
            // Also reached for a present file that could not be read: it was
            // passed as empty Data, which Rust rejects. Whatever sits at the
            // path is moved aside; an unsafe path fails safePath and stops.
            do {
                try queue.sync { try quarantiner(fileURL) }
                lock.lock()
                latestGeneration = result.personalFrequencyVersion.generation
                fileState = .absent
                lastSavedAt = nil
                lock.unlock()
                poll()
            } catch {
                loadError = error
                lock.lock()
                stopped = true
                stopReason = .quarantineFailed
                latestGeneration = result.personalFrequencyVersion.generation
                lock.unlock()
                logger.error("Learning persistence stopped; code=quarantine_failed")
            }
        } else {
            lock.lock()
            received = result.personalFrequencyVersion
            saved = result.personalFrequencyVersion
            latestGeneration = result.personalFrequencyVersion.generation
            lock.unlock()
        }
        startupData = nil
    }

    /// Asks the core for anything newer than `received`. Cheap enough to call
    /// after every event: the query does not change the core, and an unchanged
    /// version returns nothing and leaves the scheduled write where it is.
    func poll() {
        precondition(Thread.isMainThread)
        guard lock.withLock({ !stopped && !closing }) else { return }
        switch query(receivedVersion) {
        case .success(let update):
            lock.lock()
            queryFailed = false
            lock.unlock()
            if let (data, version) = update { receive(data, version: version) }
        case .failure(let error):
            logger.error("Learning query failed; category=ffi")
            lock.lock()
            warning = error
            queryFailed = true
            lock.unlock()
        }
    }

    func receive(_ data: Data, version: LearningVersion) {
        precondition(Thread.isMainThread)
        lock.lock()
        guard !stopped, !closing, received.map({ version > $0 }) ?? true else {
            lock.unlock()
            return
        }
        // Hand the bytes to the writer before advancing the received cursor.
        // A genuinely newer update restarts the debounce window. Repeated
        // receipt of the same version returned above without touching it.
        reservation = nil
        pending = Update(data: data, version: version)
        retryDelay = 0.250
        latestGeneration = version.generation
        received = version
        lock.unlock()
        #if MUU_TEST
            Self.receiveObserverForTesting?(data, version)
        #endif
        schedule()
    }

    private func schedule() {
        lock.lock()
        guard !stopped, !closing, pending != nil, reservation == nil else {
            lock.unlock()
            return
        }
        let token = Reservation()
        reservation = token
        let delay = retryDelay
        lock.unlock()
        scheduler(delay) { [weak self] in
            guard let self else { return }
            self.queue.async { self.writePending(token) }
        }
    }

    // Runs on the serial queue. The first guard is the last check before the
    // file is touched: the reservation must still be the current one (not
    // superseded by a newer update or cancelled by `clear`) and the bytes must
    // belong to the newest generation. The generation check is defense in
    // depth: with the current ordering, reservation identity (before writing)
    // and persistenceEpoch (after writing) already reject a clear's old work.
    // It is not an independently reachable rejection under those invariants.
    private func writePending(_ token: Reservation, mayStart: () -> Bool = { true }) {
        guard mayStart() else { return }
        lock.lock()
        guard !stopped, reservation === token, let update = pending,
            update.version.generation == latestGeneration
        else {
            lock.unlock()
            return
        }
        let epoch = persistenceEpoch
        reservation = nil
        lock.unlock()
        let outcome: Result<Error?, Error>
        do { outcome = .success(try writer(update.data, fileURL)) } catch {
            outcome = .failure(error)
        }
        // Read before the epoch check, but stored only by a write that
        // passes it. A failed or stale write keeps the previous time.
        let completedAt = clock()
        lock.lock()
        // Clear waits for this queue before deleting. A write from an older
        // persistence epoch must not advance either cursor afterward.
        guard epoch == persistenceEpoch, !stopped, update.version.generation == latestGeneration
        else {
            lock.unlock()
            return
        }
        switch outcome {
        case .success(let durabilityWarning):
            retryDelay = 0.250
            if saved.map({ update.version > $0 }) ?? true { saved = update.version }
            // A newer update may have arrived during the write; keep it.
            if pending?.version == update.version { pending = nil }
            warning = durabilityWarning
            fileState = .present
            lastSavedAt = .completedSave(completedAt)
            saveIssue = durabilityWarning == nil ? nil : .durabilityWarning
        case .failure(let error):
            // A newer update may have arrived during this write and already
            // reserved its own 250ms deadline. An old failure must not delay it.
            if pending?.version == update.version {
                retryDelay = min(retryDelay * 2, 30)
            }
            warning = error
            saveIssue = .writeFailed
            if let reason = Self.permanentSaveFailure(error) {
                stopped = true
                stopReason = reason
                reservation = nil
                logger.error("Learning persistence stopped; code=permanent_save_failure")
            } else {
                logger.error("Learning save failed; code=write_failed")
            }
        }
        lock.unlock()
        // Failure keeps the exact pending bytes and backs off to at most
        // 30 seconds. Success with no newer bytes leaves no retry to arm.
        // After a permanent failure `stopped` is set, so this arms nothing;
        // pending and the cursors stay for a later clear to reset.
        schedule()
    }

    /// Cancels ordinary reservations before closing receipt. This worker is
    /// serialized after any active write and never waits for main. Its one
    /// final query is the only closing receipt; failures retain known bytes.
    func beginShutdown(deadline: TimeInterval, now: @escaping () -> TimeInterval,
                       completion: @escaping (ShutdownWorkResult) -> Void) {
        precondition(Thread.isMainThread)
        lock.lock()
        let first = !closing
        if first { reservation = nil; closing = true }
        lock.unlock()
        queue.async { [self] in
            guard first else { completion(shutdownResult); return }
            let cursor: LearningVersion?
            lock.lock()
            guard !stopped, startupAccepted else {
                // A permanent failure retains unsaved bytes but must not
                // query Core or retry the unsafe writer during shutdown.
                shutdownResult = stopped && pending != nil ? .failed : .settled
                lock.unlock(); completion(shutdownResult); return
            }
            cursor = received
            lock.unlock()
            guard now() < deadline else { shutdownResult = .failed; completion(shutdownResult); return }
            lock.withLock { finalQueryCount += 1 }
            // The only snapshot query made off main. The ABI has no main-thread
            // requirement, and closing has already stopped poll and receive.
            let result = query(cursor)
            lock.lock()
            switch result {
            case .success(let update):
                queryFailed = false
                if let (data, version) = update, received.map({ version > $0 }) ?? true {
                    pending = Update(data: data, version: version)
                    latestGeneration = version.generation
                    received = version
                }
            case .failure(let error):
                warning = error
                queryFailed = true
            }
            let token = Reservation()
            reservation = token
            lock.unlock()
            // A query already in progress may cross the deadline. Do not
            // begin its final write afterward, nor create shutdown retries.
            writePending(token, mayStart: { now() < deadline })
            lock.lock()
            reservation = nil
            shutdownResult = queryFailed || pending != nil || saveIssue != nil ? .failed : .settled
            lock.unlock()
            completion(shutdownResult)
        }
    }

    /// Erases learning on disk first, then in the core, as one synchronous
    /// stretch on main (docs/src/internals/storage.md「変換学習の保存」):
    ///
    /// 1. Cancel the scheduled write and start a new epoch.
    /// 2. Delete the rejected backups, then the current file, and sync the
    ///    directory, on the serial queue, synchronously. This waits for a
    ///    write already in progress, so the delete lands after it.
    /// 3. Tell the core, without returning to the run loop in between. A
    ///    commit from any session needs main, so none can be observed between
    ///    the delete and the new generation.
    /// 4. Adopt the new generation as received and saved, and drop what was
    ///    pending.
    ///
    /// If deletion fails, current learning stays intact; rejected backups
    /// already deleted may be gone. The core is not told in that case. If
    /// the delete succeeds but the core refuses, persistence stops for this
    /// process: the core still holds the old learning, and saving it again
    /// would undo the delete. Do not make this asynchronous without a new
    /// exclusion rule covering commits, queries and writes during the clear.
    func clear() -> PersonalFrequencyClearResult {
        precondition(Thread.isMainThread)
        lock.lock()
        guard !closing else { lock.unlock(); return .precommitFailure(StoreShutdownCancellation()) }
        reservation = nil
        persistenceEpoch += 1
        lock.unlock()
        #if MUU_TEST
            Self.clearWillWaitForTesting?()
        #endif
        let deletionWarning: Error?
        do { deletionWarning = try queue.sync { try remover(fileURL) } } catch {
            // Current learning is intact. Some rejected backups may already
            // be gone; the remover syncs those deletions before propagating.
            schedule()
            return .precommitFailure(error)
        }
        // No main queue yield is allowed between deletion and the core clear.
        // A sync warning still means every unlink committed, so Core is
        // cleared and the old pending dropped as usual. Saving that pending
        // again to settle durability would write back the learning the user
        // just deleted: a certain resurrection to cure a possible one.
        switch coreClear() {
        case .success(let version):
            lock.lock()
            reservation = nil
            pending = nil
            // The file that had to be protected is gone, so saving resumes
            // from the empty state of the new generation.
            stopped = false
            retryDelay = 0.250
            received = version
            saved = version
            latestGeneration = version.generation
            warning = deletionWarning
            // Only an accepted Core can clear. A successful clear therefore
            // also counts as a started store.
            startupAccepted = true
            stopReason = nil
            fileState = .cleared
            lastSavedAt = nil
            saveIssue = deletionWarning == nil ? nil : .clearDurabilityWarning
            queryFailed = false
            lock.unlock()
            if let deletionWarning { return .clearedWithDurabilityWarning(deletionWarning) }
            return .cleared
        case .failure(let error):
            lock.lock()
            reservation = nil
            pending = nil
            stopped = true
            stopReason = .clearRejected
            // The file was already removed before Core rejected the clear.
            fileState = .cleared
            lastSavedAt = nil
            saveIssue = deletionWarning == nil ? nil : .clearDurabilityWarning
            warning = error
            lock.unlock()
            logger.error("Learning persistence stopped; code=clear_rejected")
            return .committedWithRuntimeWarning(error)
        }
    }

    #if MUU_TEST
        static var clearWillWaitForTesting: (() -> Void)?
        func flushForTesting() throws { queue.sync {} }
    #endif
    #if MUU_TEST
    static var deletionCheckpointForTesting: ((String) throws -> Void)?
    static var persistenceCheckpointForTesting: ((String) throws -> Void)?
    var pendingDataForTesting: Data? { lock.withLock { pending?.data } }
    #endif

    private static func deletionCheckpoint(_ stage: String) throws {
        #if MUU_TEST
        try deletionCheckpointForTesting?(stage)
        #endif
    }

    // Enumerate and unlink through one open directory. Do not read rejected
    // contents or follow child links. The current file is deleted last, so a
    // partial failure cannot cause pending bytes to overwrite a removed file.
    // A throw retains current learning; a returned warning means all intended
    // unlinks committed and Core must clear even if directory sync failed.
    //
    // Why this order: a throw becomes a precommit failure, which leaves Core
    // as it was and resumes saving. That is only coherent while the current
    // file still exists; deleted first, a later failure would leave Core's
    // old learning to be written straight back. The directory sync comes
    // before Core.clear because Core.clear is the irreversible step: the
    // disk part, durability attempt included, is finished and its outcome
    // known first, so the result can name both.
    private static func removeSavedLearning(_ url: URL) throws -> Error? {
        try safePath(url)
        let parent = url.deletingLastPathComponent()
        let descriptor = Darwin.open(parent.path, O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW)
        guard descriptor >= 0 else {
            if errno == ENOENT { return nil }
            throw posixError("open-clear-directory", errno)
        }
        defer { _ = Darwin.close(descriptor) }
        try deletionCheckpoint("enumerate_clear_directory")
        let iteratorFD = Darwin.dup(descriptor)
        guard iteratorFD >= 0 else { throw posixError("dup-clear-directory", errno) }
        guard let iterator = Darwin.fdopendir(iteratorFD) else {
            let code = errno; _ = Darwin.close(iteratorFD)
            throw posixError("enumerate-clear-directory", code)
        }
        defer { _ = Darwin.closedir(iterator) }
        func regular(_ name: String) throws -> Bool {
            var info = stat()
            guard Darwin.fstatat(descriptor, name, &info, AT_SYMLINK_NOFOLLOW) == 0 else {
                if errno == ENOENT { return false }
                throw posixError("stat-clear-child", errno)
            }
            return info.st_mode & S_IFMT == S_IFREG
        }
        var rejected: [String] = []
        while true {
            errno = 0
            guard let entry = Darwin.readdir(iterator) else {
                guard errno == 0 else { throw posixError("read-clear-directory", errno) }
                break
            }
            let name = withUnsafePointer(to: &entry.pointee.d_name) {
                $0.withMemoryRebound(to: CChar.self, capacity: Int(NAME_MAX) + 1) { String(validatingCString: $0) }
            }
            guard let name else { throw PersonalFrequencyStoreError.unsupportedFileType }
            if name.hasPrefix("personal-frequency-rejected-") && name.hasSuffix(".json"), try regular(name) {
                rejected.append(name)
            }
        }
        // safePath already rejects a primary symlink. Refuse other special
        // primary files before deleting any rejected backup as well.
        var primary = stat()
        if Darwin.fstatat(descriptor, url.lastPathComponent, &primary, AT_SYMLINK_NOFOLLOW) == 0 {
            guard primary.st_mode & S_IFMT == S_IFREG else { throw PersonalFrequencyStoreError.unsupportedFileType }
        } else if errno != ENOENT { throw posixError("stat-clear-primary", errno) }
        func sync() throws {
            try deletionCheckpoint("fsync_clear_directory")
            guard Darwin.fsync(descriptor) == 0 else { throw posixError("fsync-clear-directory", errno) }
        }
        var removed = false
        do {
            for name in rejected.sorted() {
                try deletionCheckpoint("unlink_rejected")
                if Darwin.unlinkat(descriptor, name, 0) == 0 { removed = true }
                else if errno != ENOENT { throw posixError("unlink-rejected", errno) }
            }
            try deletionCheckpoint("unlink_current")
            if Darwin.unlinkat(descriptor, url.lastPathComponent, 0) == 0 { removed = true }
            else if errno != ENOENT { throw posixError("unlink-current", errno) }
        } catch {
            // Some backups are already gone. Sync those removals too, then
            // report the failure; the current file is still in place.
            if removed {
                let log = Logger(subsystem: "dev.muu.inputmethod.Muu", category: "PersonalFrequency")
                log.error("Learning clear incomplete; code=learning_clear_partial")
                do { try sync() }
                catch { log.error("Learning clear incomplete; code=learning_clear_partial_durability_warning") }
            }
            throw error
        }
        do { try sync(); return nil }
        catch { return error }
    }

    // The boundary between stopping and retrying. Stop only when retrying
    // cannot help: the path or file type is unsafe, the bytes exceed the
    // save limit, or a NOFOLLOW directory open met a link or non-directory.
    // Everything else, ordinary EACCES and EIO included, keeps the backoff.
    // The decision uses the error's type and operation, never its text.
    //
    // Which NOFOLLOW opens can arrive here: ensurePrivateDirectories' syncs
    // (`directory_fsync`, `directory_parent_fsync`) throw before the rename.
    // atomicWrite's own post-rename directory open (`open-directory`) is
    // returned as a durability warning instead, so the writer reports it as
    // saved with a warning and it does not reach this function.
    private static func permanentSaveFailure(_ error: Error) -> PersonalFrequencyPersistenceStatus.StopReason? {
        guard let error = error as? PersonalFrequencyStoreError else { return nil }
        switch error {
        case .unsupportedFileType: return .unsafeSave
        case .fileTooLarge: return .invalidSave
        case .io(let operation, let code):
            // Only these directory opens use NOFOLLOW. Other operations and
            // ordinary EACCES/EIO retain their existing retry behavior.
            let noFollow = ["open-directory", "directory_fsync", "directory_parent_fsync"].contains(operation)
            return noFollow && (code == ELOOP || code == ENOTDIR) ? .unsafeSave : nil
        }
    }

    private static func safePath(_ url: URL) throws {
        try FileSafety.rejectSymbolicLinkAncestors(
            of: url, fileManager: .default,
            nonAbsolutePathError: { PersonalFrequencyStoreError.unsupportedFileType },
            symbolicLinkError: { _ in PersonalFrequencyStoreError.unsupportedFileType })
    }

    private static func posixError(_ operation: String, _ code: Int32) -> Error {
        PersonalFrequencyStoreError.io(operation: operation, code: code)
    }
}
