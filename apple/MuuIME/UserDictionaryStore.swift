import Darwin
import Foundation
import OSLog

enum UserDictionaryStoreError: Error, LocalizedError, Equatable {
    case invalidReading
    case invalidSurface
    case fileTooLarge
    case unsupportedFileType
    case posix(operation: String, code: Int32)

    var errorDescription: String? {
        switch self {
        case .invalidReading: "よみは、ひらがな128文字・UTF-8 512 bytes以内にしてください。"
        case .invalidSurface: "単語は256文字・UTF-8 4 KiB以内にしてください。"
        case .fileTooLarge: "ユーザー辞書ファイルが8 MiBの上限を超えています。"
        case .unsupportedFileType: "ユーザー辞書が通常ファイルではありません。"
        case let .posix(operation, code): "辞書の保存に失敗しました（\(operation): \(code)）。"
        }
    }
}

// Display data comes from Core; Swift neither sorts rows nor interprets TSV
// (rule 5 of docs/src/internals/architecture.md「2 つに分けている理由」; docs/src/internals/storage.md「Swift が書き、Rust が形式を決める」). IDs are opaque and valid only with their management version.
struct UserDictionaryDisplayEntry: Decodable {
    let entryId: String
    let reading: String
    let surface: String
    let partOfSpeech: MuuDictionaryPartOfSpeech
    let priority: UserDictionaryPriority
    enum CodingKeys: String, CodingKey {
        case entryId = "entry_id", reading, surface, partOfSpeech = "part_of_speech", priority
    }
}

struct UserDictionaryManagementView: Decodable {
    let schemaVersion: Int
    let managementRevision: UInt64
    let access: String
    let reason: String?
    let layerStatus: String
    let entries: [UserDictionaryDisplayEntry]?
    let entryCount: Int?
    let dormantCount: Int?
    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version", managementRevision = "management_revision", access, reason
        case layerStatus = "layer_status", entries, entryCount = "entry_count", dormantCount = "dormant_count"
    }
}

struct UserDictionaryEditPlan: Decodable {
    let schemaVersion: Int
    // Forwarded unchanged in meaning; Swift does not inspect these values.
    let ticket: [String: UInt64]
    let expectedRevision: UInt64
    let nextRevision: UInt64
    let view: UserDictionaryManagementView
    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version", ticket, expectedRevision = "expected_revision"
        case nextRevision = "next_revision", view
    }
    static func ticketBytes(from data: Data) throws -> Data {
        struct Envelope: Decodable { let ticket: [String: UInt64] }
        let ticket = try JSONEncoder().encode(JSONDecoder().decode(Envelope.self, from: data).ticket)
        guard ticket.count <= 1024 else {
            throw UserDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "user_dictionary_invalid_ticket")
        }
        return ticket
    }
}

struct UserDictionaryOperationError: Error, LocalizedError {
    let status: Int32
    let code: String
    var errorDescription: String? {
        switch code {
        case "user_dictionary_deletion_required", "user_dictionary_layer_invalid":
            "辞書が上限を超えているため、まず削除で直してください。"
        case "user_dictionary_invalid_reading": "よみは、ひらがな128文字・UTF-8 512 bytes以内にしてください。"
        case "user_dictionary_invalid_surface": "単語は256文字・UTF-8 4 KiB以内にしてください。"
        case "user_dictionary_invalid_base_form": "品詞に合う基本形を入力してください。"
        case "user_dictionary_revision_mismatch": "辞書が更新されました。一覧を確認して操作し直してください。"
        case "user_dictionary_busy": "辞書の保存が終わるまでお待ちください。"
        case "user_dictionary_recovery_failed": "辞書の復旧に失敗しました。編集を停止しています。Muuを再起動してください。"
        case "user_dictionary_core_invalid": "辞書は保存済みですが反映を確認できません。Muuを再起動してください。"
        case "user_dictionary_read_only": "辞書を保護するため、編集を停止しています。"
        default: "ユーザー辞書を更新できませんでした（\(code): \(status)）。"
        }
    }
}

struct UserDictionaryCoreBridge {
    var view: () throws -> Data
    var prepare: (Data) throws -> (file: Data, plan: Data)
    var publish: (Data) -> Int32
    var cancel: (Data) -> Int32
    // No Core (test fixtures, the default bridge). The stub sends nothing, so
    // publish is a definite refusal: CREATE_FAILED, matching view and prepare.
    // The Imported and Postal stubs return the same.
    static let unavailable = Self(
        view: { throw UserDictionaryOperationError(status: MUU_RESULT_CREATE_FAILED, code: "core_not_initialized") },
        prepare: { _ in throw UserDictionaryOperationError(status: MUU_RESULT_CREATE_FAILED, code: "core_not_initialized") },
        publish: { _ in MUU_RESULT_CREATE_FAILED }, cancel: { _ in MUU_RESULT_CREATE_FAILED })
}

struct UserDictionaryStartupFile {
    let state: FileSafety.StoredFileState
    let bytes: Data
}

/// One process-wide coordinator serializes the entire disk/Core/UI operation.
/// Main owns display state; the IO queue owns no UI and never syncs to main.
/// Busy ends after the completion has rendered, not merely after rename
/// (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」): a second edit must not race a stale displayed management version.
final class UserDictionaryStore {
    static let maxFileBytes = 8 * 1024 * 1024
    static let maxViewBytes = 32 * 1024 * 1024
    static let shared: UserDictionaryStore = {
        #if MUU_TEST
        // A menu/controller fixture may touch the singleton. It must never
        // read the user's real dictionary, even if the test has no edits.
        let base = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".muu-user-store-fixture-\(getpid())")
        let bridge = UserDictionaryCoreBridge.unavailable
        let identifier = "dev.muu.user-store-test"
        #else
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        let bridge = RustCore.userDictionaryBridge
        let identifier = Bundle.main.bundleIdentifier ?? "dev.muu.inputmethod.Muu"
        #endif
        return UserDictionaryStore(baseDirectory: base,
            bundleIdentifier: identifier, bridge: bridge)
    }()

    let fileURL: URL
    private let baseDirectory: URL
    let startupFile: UserDictionaryStartupFile
    private let bridge: UserDictionaryCoreBridge
    private let ioQueue = DispatchQueue(label: "dev.muu.user-dictionary.io")
    private let shutdownGate = StoreShutdownGate()
    // IO owns an outcome only until main acknowledges it, or closing consumes
    // it at the IO fence. A completed ordinary edit is not a shutdown result.
    private var pendingShutdownResult: ShutdownWorkResult?
    // Screen acknowledgement can still be queued when shutdown starts.
    // Keep this fact on IO, including .preserve from a later private failure.
    private var ioDurabilityWarning = false
    var isClosing: Bool { shutdownGate.isClosing }
    private let injection: ((String) throws -> Void)?
    private static let logger = Logger(subsystem: "dev.muu.inputmethod.Muu", category: "user-dictionary-store")
    #if MUU_TEST
    var recoveryDelayForTesting: ((TimeInterval) -> Void)?
    var recoveryLogForTesting: ((String) -> Void)?
    #endif
    private var operation: UInt64 = 0
    private(set) var view: UserDictionaryManagementView?
    private(set) var isBusy = false
    private(set) var loadError: Error?
    private(set) var durabilityWarning: Error?
    private var stopped = false
    // Retain the exact recovery bytes after failed restoration. Neither a
    // retry nor destruction of a window may silently discard this evidence.
    private var recoveryOriginal: OriginalFile?
    var entries: [UserDictionaryDisplayEntry] { view?.entries ?? [] }
    var canEdit: Bool { !isClosing && !stopped && !isBusy && view?.access == "editable" }
    var isDegraded: Bool { view?.layerStatus == "empty_degraded" && view?.access == "editable" }
    var editingStopped: Bool { stopped }

    init(baseDirectory: URL, bundleIdentifier: String,
         bridge: UserDictionaryCoreBridge = .unavailable, injection: ((String) throws -> Void)? = nil) {
        let file = baseDirectory.appendingPathComponent(bundleIdentifier, isDirectory: true)
            .appendingPathComponent("user_dict", isDirectory: true).appendingPathComponent("user-dictionary.tsv")
        fileURL = file; self.baseDirectory = baseDirectory; self.bridge = bridge; self.injection = injection
        // Preserve launch ordering. Merely constructing the store reads
        // bounded bytes; it creates no directory/file and sends no edit.
        let initial = ioQueue.sync { Self.read(file) }
        startupFile = initial.file; loadError = initial.error
    }

    func acceptInitializedCore() throws {
        precondition(Thread.isMainThread)
        do { view = try Self.decodeView(bridge.view()) }
        catch {
            stopped = true; loadError = error
            throw error
        }
        if view?.access == "read_only" {
            loadError = loadError ?? UserDictionaryOperationError(status: MUU_RESULT_DECODE_ERR, code: "user_dictionary_read_only")
        } else { loadError = nil }
    }

    func upsert(reading: String, surface: String, partOfSpeech: MuuDictionaryPartOfSpeech,
                priority: UserDictionaryPriority, replacing index: Int?, completion: @escaping (Result<Void, Error>) -> Void) {
        precondition(Thread.isMainThread)
        var command: [String: Any] = ["operation": "upsert", "expected_revision": view?.managementRevision ?? 0,
            "reading": reading, "surface": surface, "part_of_speech": partOfSpeech.rawValue, "priority": priority.rawValue]
        if let index {
            guard entries.indices.contains(index) else { completion(.failure(Self.failure("user_dictionary_entry_not_found"))); return }
            command["entry_id"] = entries[index].entryId
        }
        perform(command, completion: completion)
    }

    func remove(at index: Int, completion: @escaping (Result<Void, Error>) -> Void) {
        precondition(Thread.isMainThread)
        guard entries.indices.contains(index) else { completion(.failure(Self.failure("user_dictionary_entry_not_found"))); return }
        perform(["operation": "remove", "expected_revision": view?.managementRevision ?? 0,
            "entry_id": entries[index].entryId], completion: completion)
    }

    private func perform(_ command: [String: Any], completion: @escaping (Result<Void, Error>) -> Void) {
        guard canEdit else { completion(.failure(Self.failure(isBusy ? "user_dictionary_busy" : "user_dictionary_read_only"))); return }
        let commandData: Data
        do {
            commandData = try JSONSerialization.data(withJSONObject: command, options: [.sortedKeys])
            guard commandData.count <= 64 * 1024 else { throw Self.failure("user_dictionary_edit_too_large") }
        } catch { completion(.failure(error)); return }
        isBusy = true; operation += 1; let id = operation
        ioQueue.async { [self] in
            let outcome = transact(commandData)
            if case .replace(let warning) = outcome.warning { ioDurabilityWarning = warning != nil }
            let newWarning: Bool
            if case .replace(let warning) = outcome.warning { newWarning = warning != nil }
            else { newWarning = false }
            pendingShutdownResult = outcome.error == nil && !newWarning ? .settled : .failed
            DispatchQueue.main.async { [self] in
                guard !isClosing else { return }
                guard operation == id else { return }
                // Queue this acknowledgement before calling the client: even
                // a reentrant shutdown fence must follow it on IO.
                ioQueue.async { [self] in pendingShutdownResult = nil }
                if let next = outcome.view { view = next }
                if case .replace(let warning) = outcome.warning { durabilityWarning = warning }
                if outcome.stop { stopped = true; loadError = outcome.error }
                recoveryOriginal = outcome.recoveryOriginal
                if let error = outcome.error { completion(.failure(error)) } else { completion(.success(())) }
                isBusy = false
            }
        }
    }

    private enum OriginalFile { case missing, present(Data) }
    // A failed preparation or pre-rename write says nothing about the last
    // save's durability. Only a completed save/restore can replace its warning;
    // .replace(nil) means that directory fsync actually succeeded.
    private enum DurabilityWarningUpdate { case preserve, replace(Error?) }
    private struct Outcome {
        var view: UserDictionaryManagementView? = nil
        var error: Error? = nil
        var warning: DurabilityWarningUpdate = .preserve
        var stop = false
        var recoveryOriginal: OriginalFile? = nil
    }

    func beginShutdown(completion: @escaping (ShutdownWorkResult) -> Void) {
        precondition(Thread.isMainThread)
        shutdownGate.close()
        ioQueue.async { [self] in
            if let pendingShutdownResult { shutdownGate.recordShutdownResult(pendingShutdownResult) }
            pendingShutdownResult = nil
            if ioDurabilityWarning { LifecycleLog.shared.storeWarning(.user) }
            completion(shutdownGate.shutdownResult)
        }
    }

    // Runs on the IO queue. Order: prepare in Core (nothing published) ->
    // read the current file as the rollback point -> rename the new file ->
    // publish -> on a definite refusal, put the rollback point back.
    //
    // The rollback point is read after prepare and immediately before the
    // write, so a restore returns exactly what was on disk, not what Core last
    // saw. The deferred cancel releases Core's one prepared slot on every exit
    // except a successful publish; without it Core would answer Busy forever.
    private func transact(_ command: Data) -> Outcome {
        var ticket: Data?
        var published = false
        func run() -> Outcome {
            do {
                let prepared = try bridge.prepare(command)
                // The plan must be bounded before ticket decoding. An unreadable
                // ticket cannot be cancelled safely; the native bridge logs only
                // a code and fails closed, retaining the slot until restart.
                guard prepared.plan.count <= Self.maxViewBytes else {
                    throw Self.failure("user_dictionary_output_too_large")
                }
                let preparedTicket = try UserDictionaryEditPlan.ticketBytes(from: prepared.plan)
                ticket = preparedTicket
                guard prepared.file.count <= Self.maxFileBytes else {
                    throw Self.failure("user_dictionary_output_too_large")
                }
                // Own cancellation before decoding the view: e.g. an unknown POS
                // in a mixed-version response must not leave Core permanently busy.
                let plan = try JSONDecoder().decode(UserDictionaryEditPlan.self, from: prepared.plan)
                guard plan.schemaVersion == 1, plan.view.schemaVersion == 1 else {
                    throw Self.failure("user_dictionary_invalid_plan")
                }
                let original: OriginalFile
                let current = Self.read(fileURL)
                switch current.file.state {
                case .missing: original = .missing
                case .present: original = .present(current.file.bytes)
                case .unreadable: throw current.error ?? Self.failure("user_dictionary_unreadable")
                }
                try injection?("before_save")
                // A throw here is always before the rename, so the old file is
                // still on disk and the final cancel is the whole recovery.
                let warning = try write(prepared.file, restoring: false)
                // Rename committed even when directory fsync produced a warning.
                // Publish has no allocating response work after its ACK (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
                let status = bridge.publish(preparedTicket)
                let ack = DictionaryAcknowledgement(status)
                if ack == .success { published = true; return Outcome(view: plan.view, warning: .replace(warning)) }
                // 2/3/4/6/8 are definite nonpublication for this new publish API.
                // Unknown statuses are treated as ambiguous, never guessed safe.
                // That includes 9, which should not occur: operations are
                // serialized here and hold Core's only prepared slot. On an
                // ambiguous ACK the renamed file stays as it is: restoring the old
                // bytes could disagree with a Core that did publish.
                guard ack.isDefinitelyUnpublished(allowBusy: false) else {
                    return Outcome(error: Self.failure("user_dictionary_core_invalid", status: status), warning: .replace(warning), stop: true)
                }
                let restoredWarning: Error?
                do { restoredWarning = try restoreWithRetry(original) }
                catch {
                    let recoveryWarning = saveRecovery(original)
                    return Outcome(error: Self.failure("user_dictionary_recovery_failed"),
                        warning: .replace(recoveryWarning ?? warning), stop: true, recoveryOriginal: original)
                }
                let next = ack == .revisionMismatch ? try? Self.decodeView(bridge.view()) : nil
                return Outcome(view: next, error: Self.failure(ack == .revisionMismatch ? "user_dictionary_revision_mismatch" : "user_dictionary_publish_failed", status: status), warning: .replace(restoredWarning))
            } catch {
                return Outcome(error: error, stop: (error as? UserDictionaryOperationError)?.status == MUU_RESULT_CORE_INVALID)
            }
        }
        // Cancellation completes the transaction rather than trailing it. A
        // `defer` could not change the value already returned, so the outcome
        // is built first and the cancel ACK applied before it goes back to the
        // screen. A failed cancel may leave Core's one prepared slot reserved;
        // the completion must already see editing stopped, never an editable
        // store whose next edit would start against that slot. The first error
        // is kept; a cancel failure only adds the stop.
        var outcome = run()
        // A local failure inside the bridge's prepare, which already tried
        // the cancel itself. `ticket` was never assigned then, so the cancel
        // below cannot run and this is the only place the stop can come from:
        // a failed ACK, or a CORE_INVALID original, which run()'s own check
        // could not see through the wrapper.
        if let failure = outcome.error as? DictionaryPreparationFailure {
            outcome.error = failure.original
            outcome.stop = outcome.stop || failure.cancellationStatus != MUU_RESULT_OK
                || (failure.original as? UserDictionaryOperationError)?.status == MUU_RESULT_CORE_INVALID
        }
        if let ticket, !published {
            let status = bridge.cancel(ticket)
            if status != MUU_RESULT_OK {
                // The native bridge logs the ACK without the ticket. Resolve
                // cancellation before returning the screen's completion.
                outcome.stop = true
                if outcome.error == nil {
                    outcome.error = Self.failure("user_dictionary_cancel_failed", status: status)
                }
            }
        }
        return outcome
    }

    private func write(_ data: Data, restoring: Bool) throws -> Error? {
        try Self.rejectSymlinks(fileURL)
        // Runs for saves and restores alike (restore checkpoints carry the
        // `restore_` prefix). A failure throws before the rename: a save is
        // then cancelled unpublished, and a restore stops editing.
        try FileSafety.ensurePrivateDirectories(fileURL.deletingLastPathComponent(), below: baseDirectory,
            unsupportedPath: { UserDictionaryStoreError.unsupportedFileType },
            checkpoint: { try self.injection?(restoring ? "restore_\($0)" : $0) },
            posixError: { UserDictionaryStoreError.posix(operation: $0, code: $1) })
        return try FileSafety.atomicWrite(data, to: fileURL, temporaryPrefix: ".user-dictionary",
            directoryOpenFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
            beforeRename: { try self.injection?(restoring ? "restore_before_rename" : "before_rename") },
            afterRename: {
                do { try self.injection?(restoring ? "restore_after_rename" : "after_rename"); return nil }
                catch { return error }
            }, checkpoint: { try self.injection?(restoring ? "restore_\($0)" : $0) },
            posixError: { UserDictionaryStoreError.posix(operation: $0, code: $1) })
    }

    // Only definitely unpublished edits reach here. The waits stay on IO,
    // within the existing shutdown fence; main never waits for these retries.
    // Three attempts in all (0.1 s, then 0.25 s apart). A restore that
    // returns a directory-sync warning has committed and is not retried.
    private func restoreWithRetry(_ original: OriginalFile) throws -> Error? {
        for attempt in 0..<3 {
            do { return try restore(original) }
            catch {
                guard attempt < 2 else { throw error }
                let delay: TimeInterval = attempt == 0 ? 0.1 : 0.25
                #if MUU_TEST
                if let recoveryDelayForTesting { recoveryDelayForTesting(delay); continue }
                #endif
                Thread.sleep(forTimeInterval: delay)
            }
        }
        preconditionFailure("bounded restore loop must return or throw")
    }

    private enum RecoveryCode: String {
        case saved = "user_dictionary_recovery_saved"
        case saveFailed = "user_dictionary_recovery_save_failed"
        case durabilityWarning = "user_dictionary_recovery_durability_warning"
        case originalMissing = "user_dictionary_recovery_original_missing"
    }

    private func logRecovery(_ code: RecoveryCode) {
        Self.logger.error("Dictionary recovery; code=\(code.rawValue, privacy: .public)")
        #if MUU_TEST
        recoveryLogForTesting?(code.rawValue)
        #endif
    }

    // Preserve bytes without parsing, re-encoding, or applying them to Core.
    // Startup reads only user-dictionary.tsv, never these recovery files.
    // Keep recoveryOriginal in memory as well, including when this save fails.
    //
    // Why startup ignores them: by now user-dictionary.tsv holds the new
    // bytes Core refused, and the recovery file the old ones. Which version
    // the user wants back is their decision. Loading a recovery file would
    // silently pick one, and repeated failures can leave several, with no
    // order the app could trust. The store stops instead.
    private func saveRecovery(_ original: OriginalFile) -> Error? {
        // Missing means there were no bytes to keep: what failed to come back
        // is the absence of the file. An empty TSV written here would invent a
        // file the user never had, which could be taken for a backup of their
        // data. The finite code records "absence not restored" separately from
        // "bytes saved".
        guard case .present(let bytes) = original else { logRecovery(.originalMissing); return nil }
        let parent = fileURL.deletingLastPathComponent()
        let recovery = parent.appendingPathComponent("user-dictionary.recovery-\(UUID().uuidString).tsv")
        do {
            try Self.rejectSymlinks(recovery)
            try FileSafety.ensurePrivateDirectories(parent, below: baseDirectory,
                unsupportedPath: { UserDictionaryStoreError.unsupportedFileType },
                checkpoint: { try self.injection?("recovery_\($0)") },
                posixError: { UserDictionaryStoreError.posix(operation: $0, code: $1) })
            let warning = try FileSafety.atomicWrite(bytes, to: recovery, temporaryPrefix: ".user-dictionary-recovery",
                directoryOpenFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
                beforeRename: { try self.injection?("recovery_before_rename") },
                checkpoint: { try self.injection?("recovery_\($0)") },
                posixError: { UserDictionaryStoreError.posix(operation: $0, code: $1) })
            logRecovery(warning == nil ? .saved : .durabilityWarning)
            return warning
        } catch {
            logRecovery(.saveFailed)
            return nil
        }
    }

    private func restore(_ original: OriginalFile) throws -> Error? {
        try injection?("before_restore")
        switch original {
        case .present(let bytes): return try write(bytes, restoring: true)
        case .missing:
            try Self.rejectSymlinks(fileURL); try injection?("before_unlink")
            guard Darwin.unlink(fileURL.path) == 0 else { throw UserDictionaryStoreError.posix(operation: "unlink-restore", code: errno) }
            do {
                try injection?("after_unlink")
                try FileSafety.fsyncDirectory(fileURL.deletingLastPathComponent(), openFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
                    posixError: { UserDictionaryStoreError.posix(operation: $0, code: $1) })
                return nil
            } catch { return error }
        }
    }

    private static func decodeView(_ data: Data) throws -> UserDictionaryManagementView {
        guard data.count <= maxViewBytes else { throw failure("user_dictionary_output_too_large") }
        let view = try JSONDecoder().decode(UserDictionaryManagementView.self, from: data)
        guard view.schemaVersion == 1 else { throw failure("user_dictionary_invalid_view") }
        return view
    }
    private static func failure(_ code: String, status: Int32 = MUU_RESULT_DECODE_ERR) -> UserDictionaryOperationError { .init(status: status, code: code) }
    private static func rejectSymlinks(_ file: URL) throws {
        try FileSafety.rejectSymbolicLinkAncestors(of: file, fileManager: .default,
            nonAbsolutePathError: { failure("user_dictionary_invalid_path") }, symbolicLinkError: { _ in failure("user_dictionary_symlink") })
    }
    private static func read(_ file: URL) -> (file: UserDictionaryStartupFile, error: Error?) {
        let result = FileSafety.readState(file, maxBytes: maxFileBytes, validate: { try rejectSymlinks(file) },
            unsupportedFileType: { UserDictionaryStoreError.unsupportedFileType }, fileTooLarge: { UserDictionaryStoreError.fileTooLarge },
            posixError: { UserDictionaryStoreError.posix(operation: $0, code: $1) })
        return (.init(state: result.state, bytes: result.bytes), result.error)
    }
}
