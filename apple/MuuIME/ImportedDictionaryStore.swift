import Foundation
import os

extension Notification.Name {
    static let muuImportedDictionariesDidChange = Notification.Name("MuuImportedDictionariesDidChange")
}
struct ImportedDictionaryOperationError: Error, LocalizedError {
    let status: Int32
    let code: String
    let line: Int?
    let field: String?
    init(status: Int32, code: String, line: Int? = nil, field: String? = nil) {
        self.status = status; self.code = code; self.line = line; self.field = field
    }
    var errorDescription: String? {
        if let line {
            switch code {
            case "unsupported_part_of_speech": return "Muu辞書の\(line)行目の品詞は未対応です。"
            case "invalid_base_form": return "Muu辞書の\(line)行目は終止形ではありません。"
            case "duplicate_entry": return "Muu辞書の\(line)行目は既存行と重複しています。"
            default: return "Muu辞書の\(line)行目が不正です。"
            }
        }
        return switch code {
        case "busy": "辞書の処理中です。完了までお待ちください。"
        case "imported_dictionary_read_only": "辞書を読み込めないため、編集できません。"
        case "imported_dictionary_repair_required": "辞書を有効にできないため、まず削除または無効化で直してください。"
        case "revision_mismatch": "辞書の状態が変わったため、反映できませんでした。"
        case "core_invalid", "recovery_failed": "辞書の状態を復旧できません。入力メソッドを再起動してください。"
        case "completion_failed": "辞書の終了の処理に異常がありました。入力メソッドを再起動すると編集できるようになります。"
        case "unsupported_file_type": "通常のファイルを選んでください。"
        case "source_too_large", "catalog_too_large": "辞書ファイルが上限を超えています。"
        case "invalid_encoding": "辞書ファイルはUTF-8ではありません。"
        case "invalid_header": "Muu辞書のversion headerが不正です。"
        case "invalid_columns": "Muu辞書の列見出しが不正です。"
        case "missing_metadata": "必須metadataがありません: \(field ?? "")"
        case "duplicate_metadata": "metadataが重複しています: \(field ?? "")"
        case "invalid_metadata": "metadataが不正です: \(field ?? "")"
        case "too_many_entries": "辞書の件数が上限を超えています。"
        case "too_many_dictionaries": "辞書の数が上限を超えています。"
        case "too_many_expanded_tokens", "too_many_expanded_entries": "辞書の活用展開後件数が上限を超えています。"
        case "snapshot_too_large", "catalog_semantic_too_large": "辞書の展開後サイズが上限を超えています。"
        case "invalid_dictionary_id": "辞書のIDが有効な層の規則に合いません。"
        case "invalid_base_form": "辞書の終止形が有効な層の規則に合いません。"
        // ABI 13 returns the known overflow reason with INTERNAL. Preserve the
        // earlier generic presentation; the actual reason remains in code.
        case "revision_overflow": "辞書を反映できませんでした（imported_dictionary_operation_failed）。"
        default: "辞書を反映できませんでした（\(code)）。"
        }
    }
}

// Display and transaction DTOs are ABI messages, never stored catalog/blob
// contents. Swift draws summaries and executes IO; Rust defines the formats.
struct ImportedDictionaryDisplayEntry: Decodable {
    let metadata: MuuDictionaryMetadata
    let enabled: Bool
    let entryCount: Int
    let dormantEntryCount: Int
    let importedAtUnixMilliseconds: Int64
    let sourceSHA256: String
    enum CodingKeys: String, CodingKey {
        case metadata, enabled, entryCount = "entry_count", dormantEntryCount = "dormant_entry_count"
        case importedAtUnixMilliseconds = "imported_at_unix_ms", sourceSHA256 = "source_sha256"
    }
}
struct ImportedDictionaryManagementView: Decodable {
    let schemaVersion: Int
    let managementRevision: UInt64
    let access: String
    let layerStatus: String
    let reason: String?
    let dictionaries: [ImportedDictionaryDisplayEntry]?
    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version", managementRevision = "management_revision"
        case access, layerStatus = "layer_status", reason, dictionaries
    }
}
struct ImportedDictionaryTicket: Codable {
    let domain: String
    let owner: UInt64
    let operation: UInt64
    let expectedRevision: UInt64
    enum CodingKeys: String, CodingKey { case domain, owner, operation, expectedRevision = "expected_revision" }
    var bytes: Data { get throws { try JSONEncoder().encode(self) } }
}
struct ImportedDictionaryPreview: Decodable {
    let metadata: MuuDictionaryMetadata
    let entryCount: Int
    let replacesExisting: Bool
    enum CodingKeys: String, CodingKey { case metadata, entryCount = "entry_count", replacesExisting = "replaces_existing" }
}
struct ImportedDictionaryReadRequest: Decodable {
    enum Kind: String, Decodable { case generation, blob, legacyCatalog = "legacy_catalog" }
    let requestId: UInt64
    let kind: Kind
    let sha256: String?
    enum CodingKeys: String, CodingKey { case requestId = "request_id", kind, sha256 }
}
struct ImportedDictionaryBlobStage: Decodable {
    let sha256: String
    let byteLength: Int
    enum CodingKeys: String, CodingKey { case sha256, byteLength = "byte_length" }
}
struct ImportedDictionaryReadyPlan: Decodable {
    let expectedRevision: UInt64
    let publishedRevision: UInt64
    let rollbackRevision: UInt64
    let generationSHA256: String?
    let previousGenerationSHA256: String?
    let currentBytesBase64: String?
    let referencedBlobs: [String]
    let previousReferencedBlobs: [String]
    let nextView: ImportedDictionaryManagementView
    let rollbackView: ImportedDictionaryManagementView
    let preview: ImportedDictionaryPreview?
    enum CodingKeys: String, CodingKey {
        case expectedRevision = "expected_revision", publishedRevision = "published_revision", rollbackRevision = "rollback_revision"
        case generationSHA256 = "generation_sha256", previousGenerationSHA256 = "previous_generation_sha256"
        case currentBytesBase64 = "current_bytes_base64", referencedBlobs = "referenced_blobs"
        case previousReferencedBlobs = "previous_referenced_blobs", nextView = "next_view", rollbackView = "rollback_view", preview
    }
}
struct ImportedDictionaryStep: Decodable {
    let schemaVersion: Int
    let state: String
    let ticket: ImportedDictionaryTicket
    let request: ImportedDictionaryReadRequest?
    let writeBlob: ImportedDictionaryBlobStage?
    let preview: ImportedDictionaryPreview?
    let ready: ImportedDictionaryReadyPlan?
    enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", state, ticket, request, writeBlob = "write_blob", preview, ready }
    static func ticket(from bytes: Data) throws -> ImportedDictionaryTicket {
        struct Envelope: Decodable { let ticket: ImportedDictionaryTicket }
        return try JSONDecoder().decode(Envelope.self, from: bytes).ticket
    }
    static func decode(_ bytes: Data) throws -> Self {
        guard bytes.count <= ImportedDictionaryStore.maxCatalogBytes else { throw importedFailure("catalog_too_large") }
        let step = try JSONDecoder().decode(Self.self, from: bytes)
        guard step.schemaVersion == 1, step.ticket.domain == "imported_dictionary",
              (step.state == "ready" && step.ready != nil && step.request == nil && step.writeBlob == nil)
                || (step.state == "needs_blob" && step.ready == nil && ((step.request == nil) != (step.writeBlob == nil)))
        else { throw importedFailure("invalid_imported_dictionary_plan") }
        return step
    }
}
struct ImportedDictionaryCoreBridge {
    typealias Reply = (generation: Data, plan: Data)
    var view: () throws -> Data
    var loadBegin: (Data, Data) throws -> Reply
    var loadSupply: (Data, Data) throws -> Reply
    var prepare: (Data, Data) throws -> Reply
    var prepareSupply: (Data, Data) throws -> Reply
    var preparedBlob: (Data) throws -> Data
    var publish: (Data) -> Int32
    var rollback: (Data) -> Int32
    var finish: (Data) -> Int32
    var cancel: (Data) -> Int32
    var template: () throws -> Data
    // No Core (test fixtures, the default bridge). The stub sends nothing, so
    // every ACK is a definite refusal: CREATE_FAILED, read as refused, which
    // does not stop the store. The User and Postal stubs return the same.
    static let unavailable = Self(view: { throw importedFailure("core_unavailable") },
        loadBegin: { _, _ in throw importedFailure("core_unavailable") }, loadSupply: { _, _ in throw importedFailure("core_unavailable") },
        prepare: { _, _ in throw importedFailure("core_unavailable") }, prepareSupply: { _, _ in throw importedFailure("core_unavailable") },
        preparedBlob: { _ in throw importedFailure("core_unavailable") }, publish: { _ in MUU_RESULT_CREATE_FAILED }, rollback: { _ in MUU_RESULT_CREATE_FAILED },
        finish: { _ in MUU_RESULT_CREATE_FAILED }, cancel: { _ in MUU_RESULT_CREATE_FAILED }, template: { throw importedFailure("core_unavailable") })
}
struct PreparedImportedDictionary {
    let preview: ImportedDictionaryPreview
    fileprivate let reply: ImportedDictionaryCoreBridge.Reply
    fileprivate let step: ImportedDictionaryStep
    fileprivate let operation: UInt64
}
private func importedFailure(_ code: String, status: Int32 = MUU_RESULT_DECODE_ERR) -> ImportedDictionaryOperationError {
    ImportedDictionaryOperationError(status: status, code: code)
}

/// Main owns UI state; one serial IO queue owns the entire operation. The
/// ticket remains reserved through persistence, recovery, GC and main completion.
/// No IO-to-main synchronous call is allowed: IMK callbacks may need main.
final class ImportedDictionaryStore {
    static let maxSourceBytes = 16 * 1024 * 1024
    static let maxCatalogBytes = 32 * 1024 * 1024
    private static let logger = Logger(subsystem: "dev.muu.inputmethod.Muu", category: "imported-dictionary-store")
    static let shared: ImportedDictionaryStore = {
        #if MUU_TEST
        let base = URL(fileURLWithPath: FileManager.default.currentDirectoryPath).appendingPathComponent(".muu-imported-store-fixture-\(getpid())")
        let bridge = ImportedDictionaryCoreBridge.unavailable
        #else
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        let bridge = RustCore.importedDictionaryBridge
        #endif
        return ImportedDictionaryStore(baseDirectory: base, bundleIdentifier: Bundle.main.bundleIdentifier ?? "dev.muu.inputmethod.Muu", bridge: bridge)
    }()
    let fileURL: URL
    let rootURL: URL
    let blobsURL: URL
    let generationsURL: URL
    let legacyFileURL: URL
    private let baseDirectory: URL
    private let bridge: ImportedDictionaryCoreBridge
    private let center: NotificationCenter
    private let queue = DispatchQueue(label: "dev.muu.imported-dictionary.io", qos: .utility)
    private let injection: ((String) throws -> Void)?
    private let fatalRuntimeRecovery: () -> Void
    private let recoveryDelay: (TimeInterval) -> Void
    private(set) var startupState: DataStartupState = .notStarted
    private var startupToken: UUID?
    private var operation: UInt64 = 0
    private var confirmationOperation: UInt64?
    private let shutdownGate = StoreShutdownGate()
    // Kept on queue while main owns only a preview or screen callback.
    private var confirmationTicket: ImportedDictionaryTicket?
    private var pendingCompletion: IOCompletionLease?
    // Screen acknowledgement can still be queued when shutdown starts.
    // Keep this fact on IO, including .preserve from a later private failure.
    private var ioDurabilityWarning = false
    private var stopped = false
    private(set) var isBusy = false
    private(set) var view: ImportedDictionaryManagementView?
    private(set) var loadError: Error?
    private(set) var durabilityWarning: Error?
    private(set) var recoveryNotice: String?
    var dictionaries: [ImportedDictionaryDisplayEntry] { view?.dictionaries ?? [] }
    var canEdit: Bool { startupState == .ready && !isClosing && !stopped && !isBusy && view?.access == "editable" }
    var isDegraded: Bool { view?.access == "editable" && view?.layerStatus == "empty_degraded" }
    var hasStarted: Bool { startupState != .notStarted }
    var canExportTemplate: Bool { startupState.finished && !isClosing }
    var isClosing: Bool { shutdownGate.isClosing }
    var editingStopped: Bool { stopped }

    init(baseDirectory: URL, bundleIdentifier: String, notificationCenter: NotificationCenter = .default,
         bridge: ImportedDictionaryCoreBridge = .unavailable, injection: ((String) throws -> Void)? = nil,
         fatalRuntimeRecovery: @escaping () -> Void = { exit(EXIT_FAILURE) },
         recoveryDelay: @escaping (TimeInterval) -> Void = { Thread.sleep(forTimeInterval: $0) }) {
        rootURL = baseDirectory.appendingPathComponent(bundleIdentifier, isDirectory: true)
            .appendingPathComponent("imported_dict", isDirectory: true).appendingPathComponent("v2", isDirectory: true)
        fileURL = rootURL.appendingPathComponent("current")
        blobsURL = rootURL.appendingPathComponent("blobs", isDirectory: true)
        generationsURL = rootURL.appendingPathComponent("generations", isDirectory: true)
        legacyFileURL = rootURL.deletingLastPathComponent().appendingPathComponent("catalog-v1.json")
        self.baseDirectory = baseDirectory
        self.bridge = bridge; center = notificationCenter; self.injection = injection
        self.fatalRuntimeRecovery = fatalRuntimeRecovery
        self.recoveryDelay = recoveryDelay
        // Construction performs no IO/mutation and sends no Core operation.
        // AppDelegate starts the named load only AFTER Core initialization.
    }
    func start(completion: @escaping (Result<Void, Error>) -> Void = { _ in }) {
        precondition(Thread.isMainThread)
        guard !isClosing else { completion(.failure(StoreShutdownCancellation())); return }
        guard startupState == .notStarted else { completion(.success(())); return }
        let token = UUID(); startupToken = token
        startupState = .loading; isBusy = true
        LifecycleLog.shared.startup(.imported, .loading)
        center.post(name: .muuImportedDictionariesDidChange, object: self)
        // Any startup failure stops editing for this process, unlike Postal,
        // whose failed load an explicit update can repair.
        queue.async { [self] in
            var ticket: ImportedDictionaryTicket?
            let outcome: Outcome
            do {
                LifecycleLog.shared.startupWorker(.imported, phase: .begin)
                try shutdownGate.checkOpen()
                let current = readState(fileURL, limit: 128)
                let command = try json(["expected_revision": 0, "file_state": current.state, "now_unix_ms": Int64(Date().timeIntervalSince1970 * 1_000)])
                let initial = try bridge.loadBegin(command, current.bytes)
                ticket = try ImportedDictionaryStep.ticket(from: initial.plan)
                let step = try ImportedDictionaryStep.decode(initial.plan)
                let ready = try drive(initial, step: step, loading: true)
                let plan = ready.step.ready!
                try validate(plan, ticket: step.ticket, loading: true)
                try shutdownGate.checkOpen()
                try acknowledged(bridge.publish(try step.ticket.bytes))
                LifecycleLog.shared.startupWorker(.imported, phase: .published)
                try finish(step.ticket, outcome: "loaded", revision: plan.publishedRevision, generation: plan.generationSHA256)
                outcome = Outcome(view: plan.nextView)
                LifecycleLog.shared.startupWorker(.imported, phase: .finished)
            } catch { outcome = Outcome(error: error, stop: true) }
            if let error = outcome.error { LifecycleLog.shared.startupWorker(.imported, phase: error is StoreShutdownCancellation ? .cancelled : .failed) }
            deliver(outcome, ticket: ticket, startup: token, completion: completion)
        }
    }
    func prepareDictionaryFile(at url: URL, completion: @escaping (Result<PreparedImportedDictionary, Error>) -> Void) {
        precondition(Thread.isMainThread)
        guard begin() else { completion(.failure(editRejection())); return }
        let serial = operation; let revision = view?.managementRevision ?? 0
        queue.async { [self] in
            var ticket: ImportedDictionaryTicket?
            do {
                try shutdownGate.checkOpen()
                let source = try read(url, limit: Self.maxSourceBytes)
                let command = try json(["action": "import", "expected_revision": revision, "source_file_name": url.lastPathComponent,
                    "imported_at_unix_ms": Int64(Date().timeIntervalSince1970 * 1_000), "enabled": true])
                let reply = try bridge.prepare(command, source)
                ticket = try ImportedDictionaryStep.ticket(from: reply.plan)
                let step = try ImportedDictionaryStep.decode(reply.plan)
                guard let preview = step.preview ?? step.ready?.preview else { throw importedFailure("invalid_imported_dictionary_plan") }
                let prepared = PreparedImportedDictionary(preview: preview, reply: reply, step: step, operation: serial)
                confirmationTicket = step.ticket
                DispatchQueue.main.async { [self] in
                    guard !isClosing else { return }
                    confirmationOperation = serial
                    completion(.success(prepared))
                }
                // Confirmation is the only main-thread gap; busy and ticket
                // remain reserved. No immutable file has been staged yet.
            } catch {
                // Use the error deliver resolved, which is the unwrapped
                // original, rather than the raw catch value.
                deliver(Outcome(error: error), ticket: ticket) { result in
                    completion(result.flatMap { .failure(error) })
                }
            }
        }
    }
    func install(_ prepared: PreparedImportedDictionary, completion: @escaping (Result<Void, Error>) -> Void) {
        precondition(Thread.isMainThread)
        guard !isClosing && isBusy && confirmationOperation == prepared.operation else { completion(.failure(importedFailure("busy"))); return }
        confirmationOperation = nil // A confirmation may be consumed once only.
        queue.async { [self] in
            confirmationTicket = nil
            deliver(transact(initial: prepared.reply, step: prepared.step), ticket: prepared.step.ticket, completion: completion)
        }
    }
    func cancel(_ prepared: PreparedImportedDictionary, completion: @escaping () -> Void = {}) {
        precondition(Thread.isMainThread)
        guard !isClosing && isBusy && confirmationOperation == prepared.operation else { completion(); return }
        confirmationOperation = nil
        queue.async { [self] in
            confirmationTicket = nil
            deliver(Outcome(), ticket: prepared.step.ticket) { _ in completion() }
        }
    }
    func setEnabled(_ enabled: Bool, dictionaryID: String, completion: @escaping (Result<Void, Error>) -> Void) {
        perform(["action": "set_enabled", "dictionary_id": dictionaryID, "enabled": enabled], completion: completion)
    }
    func remove(dictionaryID: String, completion: @escaping (Result<Void, Error>) -> Void) {
        perform(["action": "remove", "dictionary_id": dictionaryID], completion: completion)
    }
    func exportTemplate(to url: URL, completion: @escaping (Result<Void, Error>) -> Void) {
        precondition(Thread.isMainThread)
        guard canExportTemplate else { completion(.failure(importedFailure("busy"))); return }
        queue.async { [self] in
            let result = Result { try shutdownGate.checkOpen(); try bridge.template().write(to: url, options: .atomic) }
            DispatchQueue.main.async { [self] in if !isClosing { completion(result) } }
        }
    }
    private func begin() -> Bool {
        guard canEdit else { return false }
        operation &+= 1; isBusy = true
        center.post(name: .muuImportedDictionariesDidChange, object: self)
        return true
    }
    private func editRejection() -> Error {
        stopped ? (loadError ?? importedFailure("completion_failed")) : importedFailure("busy")
    }
    private func perform(_ input: [String: Any], completion: @escaping (Result<Void, Error>) -> Void) {
        precondition(Thread.isMainThread)
        guard begin() else { completion(.failure(editRejection())); return }
        var command = input; command["expected_revision"] = view?.managementRevision ?? 0
        queue.async { [self] in
            var ticket: ImportedDictionaryTicket?
            let outcome: Outcome
            do {
                try shutdownGate.checkOpen()
                let reply = try bridge.prepare(json(command), Data())
                ticket = try ImportedDictionaryStep.ticket(from: reply.plan)
                let step = try ImportedDictionaryStep.decode(reply.plan)
                outcome = transact(initial: reply, step: step)
            } catch { outcome = Outcome(error: error) }
            deliver(outcome, ticket: ticket, completion: completion)
        }
    }
    private enum WarningUpdate { case preserve, replace(Error?) }
    private struct Outcome {
        var view: ImportedDictionaryManagementView?
        var error: Error?
        var warning: WarningUpdate = .preserve
        var stop = false
        var notice: String?
    }
    func beginShutdown(completion: @escaping (ShutdownWorkResult) -> Void) {
        precondition(Thread.isMainThread)
        shutdownGate.close()
        confirmationOperation = nil
        queue.async { [self] in
            pendingCompletion?.finish()
            if let ticket = confirmationTicket {
                confirmationTicket = nil
                if (try? ticket.bytes).map(bridge.cancel) != 0 { shutdownGate.recordShutdownResult(.failed) }
            }
            if ioDurabilityWarning { LifecycleLog.shared.storeWarning(.imported) }
            completion(shutdownGate.shutdownResult)
        }
    }
    private func deliver(_ outcome: Outcome, ticket: ImportedDictionaryTicket?, startup: UUID? = nil, completion: @escaping (Result<Void, Error>) -> Void) {
        precondition(!Thread.isMainThread)
        // Bridge failures reach the screen only through here (the direct
        // failures above are guards before any IO), so one unwrap covers them:
        // the screen gets the original, and the store stops if the helper's
        // cancel failed.
        var outcome = outcome
        let localCancellation = outcome.error as? DictionaryPreparationFailure
        if let failure = localCancellation {
            outcome.error = failure.original
            outcome.stop = outcome.stop || failure.cancellationStatus != MUU_RESULT_OK
        }
        // All replies of this prepared operation share Core's one ticket.
        // A native helper already attempted its cancellation, even on ACK 0.
        // A later supply step can fail locally while this store still holds
        // the ticket from the first reply; the lease must not cancel it again.
        let completionTicket = localCancellation == nil ? ticket : nil
        let newWarning: Bool
        if case .replace(let warning) = outcome.warning { ioDurabilityWarning = warning != nil; newWarning = warning != nil }
        else { newWarning = false }
        let terminal = outcome.stop || (outcome.error as? ImportedDictionaryOperationError)?.status == MUU_RESULT_CORE_INVALID
        let lease = IOCompletionLease { [self] in
            pendingCompletion = nil
            let status = completionTicket.map { (try? $0.bytes).map(bridge.cancel) ?? MUU_RESULT_INTERNAL } ?? MUU_RESULT_OK
            if status != MUU_RESULT_OK { Self.logger.error("Imported dictionary completion failed; result=\(status, privacy: .public)") }
            shutdownGate.recordShutdownResult(status != MUU_RESULT_OK || newWarning || (!(outcome.error is StoreShutdownCancellation) && (terminal || outcome.error != nil)) ? .failed : .settled)
            if isClosing { return }
            DispatchQueue.main.async { [self] in
                guard !isClosing else { return }
                if status != MUU_RESULT_OK && !terminal {
                    stopped = true; loadError = importedFailure("completion_failed", status: status)
                }
                if let startup, startupToken == startup {
                    startupState = .completed(access: view?.access, failed: terminal || status != MUU_RESULT_OK || outcome.error != nil)
                    LifecycleLog.shared.startup(.imported, startupState)
                }
                isBusy = false
                center.post(name: .muuImportedDictionariesDidChange, object: self)
            }
        }
        pendingCompletion = lease
        if isClosing { lease.finish(); return }
        DispatchQueue.main.async { [self] in
            guard !isClosing else { return }
            if let view = outcome.view { self.view = view }
            if case let .replace(warning) = outcome.warning { durabilityWarning = warning }
            if terminal { stopped = true; loadError = outcome.error }
            else if let view = outcome.view { loadError = view.access == "read_only" ? importedFailure(view.reason ?? "imported_dictionary_read_only") : nil }
            if let notice = outcome.notice { recoveryNotice = notice }
            completion(outcome.error.map(Result.failure) ?? .success(()))
            // Normal delivery holds the reservation through screen completion.
            // Shutdown consumes this same lease on queue without waiting for UI.
            queue.async { lease.finish() }
        }
    }
    private func drive(_ initial: ImportedDictionaryCoreBridge.Reply, step initialStep: ImportedDictionaryStep,
                       loading: Bool) throws -> (reply: ImportedDictionaryCoreBridge.Reply, step: ImportedDictionaryStep) {
        var reply = initial; var step = initialStep
        while step.state != "ready" {
            try shutdownGate.checkOpen()
            let next: ImportedDictionaryCoreBridge.Reply
            if let request = step.request {
                let url: URL; let limit: Int
                switch request.kind {
                case .generation: url = try named(request.sha256, suffix: ".json", directory: generationsURL); limit = Self.maxCatalogBytes
                case .blob: url = try named(request.sha256, suffix: ".muudict", directory: blobsURL); limit = Self.maxSourceBytes
                case .legacyCatalog:
                    if let hash = request.sha256 { try checkHash(hash) }
                    url = legacyFileURL; limit = Self.maxCatalogBytes
                }
                let file = readState(url, limit: limit)
                let command = try json(["response": "read", "ticket": ticketObject(step.ticket), "request_id": request.requestId, "file_state": file.state])
                next = try (loading ? bridge.loadSupply : bridge.prepareSupply)(command, file.bytes)
            } else if let stage = step.writeBlob, !loading {
                try checkHash(stage.sha256)
                guard (0...Self.maxSourceBytes).contains(stage.byteLength) else { throw importedFailure("source_too_large") }
                // The only blob Data is scoped to this iteration, and released
                // before ACK allows Core to prepare or request the next one.
                do {
                    let bytes = try bridge.preparedBlob(json(["ticket": ticketObject(step.ticket), "sha256": stage.sha256]))
                    guard bytes.count == stage.byteLength else { throw importedFailure("invalid_imported_dictionary_plan") }
                    try writeImmutable(bytes, hash: stage.sha256, suffix: ".muudict", directory: blobsURL)
                }
                next = try bridge.prepareSupply(json(["response": "written", "ticket": ticketObject(step.ticket), "sha256": stage.sha256]), Data())
            } else { throw importedFailure("invalid_imported_dictionary_plan") }
            let decoded = try ImportedDictionaryStep.decode(next.plan)
            guard decoded.ticket.owner == initialStep.ticket.owner, decoded.ticket.operation == initialStep.ticket.operation,
                  decoded.ticket.expectedRevision == initialStep.ticket.expectedRevision else { throw importedFailure("invalid_imported_dictionary_plan") }
            reply = next; step = decoded
        }
        return (reply, step)
    }
    private func transact(initial: ImportedDictionaryCoreBridge.Reply, step: ImportedDictionaryStep) -> Outcome {
        do {
            let ready = try drive(initial, step: step, loading: false)
            let plan = ready.step.ready!
            try validate(plan, ticket: step.ticket, loading: false)
            guard let hash = plan.generationSHA256, let encoded = plan.currentBytesBase64,
                  let pointer = Data(base64Encoded: encoded), pointer.count <= 128 else { throw importedFailure("invalid_imported_dictionary_plan") }
            try writeImmutable(ready.reply.generation, hash: hash, suffix: ".json", directory: generationsURL)
            _ = try originalPointer() // Opaque old bytes must be readable before ACK.
            try injection?("before_publish")
            try shutdownGate.checkOpen()
            let status = bridge.publish(try step.ticket.bytes)
            // A known refusal (2/3/4/6, 8 or BUSY) left Core unchanged: the
            // edit fails and editing stays available. CORE_INVALID or an
            // unknown value cannot prove that, so the store also stops.
            let ack = DictionaryAcknowledgement(status)
            guard ack == .success else {
                return Outcome(error: importedFailure(ack == .revisionMismatch ? "revision_mismatch" : status == MUU_RESULT_CORE_INVALID ? "core_invalid" : "publication_failed", status: status),
                    stop: !ack.isDefinitelyUnpublished(allowBusy: true))
            }
            // A pointer write that throws has not renamed (FileSafety returns
            // only warnings after rename), so the old pointer is still on disk.
            // Try to make Core match it (rollback, twice); failing that, make
            // the disk match Core (write the new pointer, three times). If
            // neither converges, exit: the next launch loads whatever pointer
            // is on disk, which makes the disk the single authority again.
            do { return completeNew(plan, ticket: step.ticket, warning: try writePointer(pointer), forwarded: false) }
            catch {
                let persistenceError = error
                for _ in 0..<2 {
                    let rollback = bridge.rollback(try step.ticket.bytes)
                    let ack = DictionaryAcknowledgement(rollback)
                    if ack == .success {
                        do {
                            try finish(step.ticket, outcome: "rolled_back", revision: plan.rollbackRevision, generation: plan.previousGenerationSHA256)
                            // Rename never happened; old pointer is untouched.
                            // No fresh fsync was checked, so preserve warnings.
                            return Outcome(view: plan.rollbackView, error: persistenceError)
                        } catch { return fatal(error, view: plan.rollbackView) }
                    }
                    // A refusal or BUSY is retried once more. A revision
                    // conflict, CORE_INVALID or an unknown value ends recovery
                    // at once: no forward save, no GC, one fatal exit.
                    if !ack.canRetryRecovery { return fatal(importedFailure("recovery_failed", status: rollback)) }
                }
                for delay in [0.1, 0.25, 0.5] {
                    recoveryDelay(delay) // Bounded recovery on IO queue only; injectable in tests.
                    do { return completeNew(plan, ticket: step.ticket, warning: try writePointer(pointer), forwarded: true) }
                    catch { continue }
                }
                return fatal(importedFailure("recovery_failed"))
            }
        } catch { return Outcome(error: error) }
    }
    private func completeNew(_ plan: ImportedDictionaryReadyPlan, ticket: ImportedDictionaryTicket, warning: Error?, forwarded: Bool) -> Outcome {
        do { try finish(ticket, outcome: warning == nil ? "new_pointer_durable" : "new_pointer_warning", revision: plan.publishedRevision, generation: plan.generationSHA256) }
        catch { return fatal(error, view: plan.nextView, warning: .replace(warning)) }
        if warning == nil {
            do { try collect(plan) }
            catch { Self.logger.error("Imported dictionary cleanup failed; category=garbage_collection") }
        }
        if forwarded { Self.logger.warning("Imported dictionary recovered by committing the new pointer") }
        return Outcome(view: plan.nextView, warning: .replace(warning), notice: forwarded ? "保存の復旧のため、新しい辞書を反映しました。" : nil)
    }
    private func fatal(_ error: Error, view: ImportedDictionaryManagementView? = nil, warning: WarningUpdate = .preserve) -> Outcome {
        Self.logger.fault("Imported dictionary recovery stopped; category=fatal_recovery")
        fatalRuntimeRecovery()
        return Outcome(view: view, error: error, warning: warning, stop: true)
    }
    private func validate(_ plan: ImportedDictionaryReadyPlan, ticket: ImportedDictionaryTicket, loading: Bool) throws {
        guard plan.expectedRevision == ticket.expectedRevision, plan.publishedRevision == plan.nextView.managementRevision,
              plan.rollbackRevision == plan.rollbackView.managementRevision, plan.nextView.schemaVersion == 1, plan.rollbackView.schemaVersion == 1
        else { throw importedFailure("invalid_imported_dictionary_plan") }
        for hash in plan.referencedBlobs + plan.previousReferencedBlobs { try checkHash(hash) }
        if let hash = plan.generationSHA256 { try checkHash(hash) }
        if let hash = plan.previousGenerationSHA256 { try checkHash(hash) }
        if !loading && plan.generationSHA256 == nil { throw importedFailure("invalid_imported_dictionary_plan") }
    }
    private func finish(_ ticket: ImportedDictionaryTicket, outcome: String, revision: UInt64, generation: String?) throws {
        try acknowledged(bridge.finish(json(["ticket": ticketObject(ticket), "outcome": outcome, "revision": revision, "generation_sha256": generation as Any? ?? NSNull()])))
    }
    private func acknowledged(_ status: Int32) throws {
        guard DictionaryAcknowledgement(status) == .success else { throw importedFailure(status == MUU_RESULT_CORE_INVALID ? "core_invalid" : "recovery_failed", status: status) }
    }
    private func ticketObject(_ ticket: ImportedDictionaryTicket) -> [String: Any] {
        ["domain": ticket.domain, "owner": ticket.owner, "operation": ticket.operation, "expected_revision": ticket.expectedRevision]
    }
    private func json(_ value: [String: Any]) throws -> Data { try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]) }
    private func checkHash(_ hash: String) throws {
        guard FileSafety.isLowercaseSHA256(hash) else { throw importedFailure("invalid_imported_dictionary_name") }
    }
    private func named(_ hash: String?, suffix: String, directory: URL) throws -> URL {
        guard let hash else { throw importedFailure("invalid_imported_dictionary_name") }
        try checkHash(hash); return directory.appendingPathComponent(hash + suffix, isDirectory: false)
    }
    // Only the state and bytes go to Core, which decides what each state means
    // for the request. The original error is not kept here; User keeps it.
    private func readState(_ url: URL, limit: Int) -> (state: String, bytes: Data) {
        let result = FileSafety.readState(url, maxBytes: limit, validate: { try validateRead(url) },
            unsupportedFileType: { importedFailure("unsupported_file_type") },
            fileTooLarge: { importedFailure("source_too_large") }, posixError: posix)
        return (result.state.rawValue, result.bytes)
    }
    private func validateRead(_ url: URL) throws {
        try FileSafety.rejectSymbolicLinkAncestors(of: url, fileManager: .default, nonAbsolutePathError: { importedFailure("unsupported_file_type") }, symbolicLinkError: { _ in importedFailure("unsupported_file_type") })
        try injection?("read:\(url.lastPathComponent)")
    }
    private func read(_ url: URL, limit: Int) throws -> Data {
        try validateRead(url)
        return try FileSafety.boundedRead(url, maxBytes: limit, unsupportedFileType: { importedFailure("unsupported_file_type") }, fileTooLarge: { importedFailure("source_too_large") }, posixError: posix)
    }
    private func originalPointer() throws -> Data? {
        do { return try read(fileURL, limit: 128) }
        catch let error as NSError where error.domain == NSPOSIXErrorDomain && error.code == Int(ENOENT) { return nil }
    }
    private func writeImmutable(_ bytes: Data, hash: String, suffix: String, directory: URL) throws {
        let url = try named(hash, suffix: suffix, directory: directory)
        guard FileSafety.sha256(bytes) == hash else { throw importedFailure("imported_dictionary_hash_mismatch") }
        try FileSafety.rejectSymbolicLinkAncestors(of: url, fileManager: .default, nonAbsolutePathError: { importedFailure("unsupported_file_type") }, symbolicLinkError: { _ in importedFailure("unsupported_file_type") })
        do {
            _ = try read(url, limit: suffix == ".muudict" ? Self.maxSourceBytes : Self.maxCatalogBytes)
        } catch let error as NSError where error.domain == NSPOSIXErrorDomain && error.code == Int(ENOENT) { }
        catch let error as ImportedDictionaryOperationError where error.code == "source_too_large" { }
        // Even matching bytes may be an orphan left by a failed directory
        // fsync. Rewrite atomically so both the file and its parent are synced
        // before acknowledging this stage; the hash alone proves no durability.
        // The NOFOLLOW read above still rejects non-regular existing files.
        try injection?("write_immutable:\(url.lastPathComponent)")
        // Every directory from the bundle folder down to `blobs/` or
        // `generations/` is private and linked durably before the file is
        // renamed in; a failure here, as below, leaves the stage unpublished.
        try FileSafety.ensurePrivateDirectories(directory, below: baseDirectory,
            unsupportedPath: { importedFailure("unsupported_file_type") },
            checkpoint: { [self] in try injection?("immutable:\($0)") }, posixError: posix)
        // Unlike the pointer, a directory fsync warning fails this write.
        // Nothing has been published yet, so aborting costs only an orphan.
        if let warning = try FileSafety.atomicWrite(bytes, to: url, temporaryPrefix: ".immutable",
            beforeRename: { [self] in try injection?("immutable:before_rename") },
            checkpoint: { [self] in try injection?("immutable:\($0)") }, posixError: posix) { throw warning }
    }
    private func writePointer(_ bytes: Data) throws -> Error? {
        try FileSafety.rejectSymbolicLinkAncestors(of: fileURL, fileManager: .default, nonAbsolutePathError: { importedFailure("unsupported_file_type") }, symbolicLinkError: { _ in importedFailure("unsupported_file_type") })
        // The same chain for `v2/` itself. A failure throws before the
        // pointer's rename, so the caller's recovery treats it like any other
        // failed pointer write: rollback first, then the forward retries.
        try FileSafety.ensurePrivateDirectories(rootURL, below: baseDirectory,
            unsupportedPath: { importedFailure("unsupported_file_type") },
            checkpoint: { [self] in try injection?("current:\($0)") }, posixError: posix)
        return try FileSafety.atomicWrite(bytes, to: fileURL, temporaryPrefix: ".current",
            beforeRename: { [self] in try injection?("before_current_rename") },
            afterRename: { [self] in do { try injection?("after_current_rename"); return nil } catch { return error } },
            checkpoint: { [self] in try injection?("current:\($0)") }, posixError: posix)
    }
    // Deletes every hash-named blob and generation that the new generation
    // does not reference, including the previous generation and orphans from
    // earlier failed operations. Called only after the new pointer is durable
    // and finish is acknowledged, while this operation still holds the
    // ticket, so no other operation can be staging files concurrently.
    private func collect(_ plan: ImportedDictionaryReadyPlan) throws {
        try injection?("before_gc")
        let blobs = Set(plan.referencedBlobs.map { $0 + ".muudict" })
        let generations = Set([plan.generationSHA256! + ".json"])
        for (directory, keeping, suffix) in [(blobsURL, blobs, ".muudict"), (generationsURL, generations, ".json")] {
            try FileSafety.rejectSymbolicLinkAncestors(of: directory, fileManager: .default, nonAbsolutePathError: { importedFailure("unsupported_file_type") }, symbolicLinkError: { _ in importedFailure("unsupported_file_type") })
            guard FileManager.default.fileExists(atPath: directory.path) else { continue }
            try FileSafety.removeUnreferencedRegularFiles(in: directory, keeping: keeping, eligible: { name in
                guard name.hasSuffix(suffix) else { return false }
                return (try? self.checkHash(String(name.dropLast(suffix.count)))) != nil
            }, posixError: posix)
        }
    }
    private func posix(_ operation: String, _ code: Int32) -> Error {
        NSError(domain: NSPOSIXErrorDomain, code: Int(code), userInfo: [NSLocalizedDescriptionKey: "辞書ファイルの処理に失敗しました（\(operation)）。"])
    }
}
