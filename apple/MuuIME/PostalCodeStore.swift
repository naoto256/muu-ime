import Darwin
import Foundation
import OSLog

// Display-only metadata returned by Core. Never encode this as a saved file.
struct PostalCodeManifest: Decodable, Equatable {
    let schemaVersion: Int
    let sourceURL: String
    let sourcePageURL: String
    let sourceLastModified: String
    let retrievedAt: String
    let zipSHA256: String
    let csvSHA256: String
    let snapshotSHA256: String
    let entryCount: Int

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case sourceURL = "source_url"
        case sourcePageURL = "source_page_url"
        case sourceLastModified = "source_last_modified"
        case retrievedAt = "retrieved_at"
        case zipSHA256 = "zip_sha256"
        case csvSHA256 = "csv_sha256"
        case snapshotSHA256 = "snapshot_sha256"
        case entryCount = "entry_count"
    }
}

enum PostalCodeStoreError: Error, LocalizedError, Equatable {
    case unavailableSeed
    case downloadFailed
    case responseTooLarge
    case invalidResponse
    case extractionFailed
    case invalidEncoding
    case invalidCSV
    case invalidSnapshot
    case invalidManifest
    case unsupportedFileType
    case fileTooLarge
    case operationInProgress
    case rollbackForwardCommitted
    case runtimeUnavailable
    case posix(operation: String, code: Int32)

    var errorDescription: String? {
        switch self {
        case .unavailableSeed: "初期郵便番号データが見つかりません。"
        case .downloadFailed: "郵便番号データを取得できませんでした。"
        case .responseTooLarge: "取得した郵便番号データが上限を超えています。"
        case .invalidResponse: "日本郵便から不正な応答を受信しました。"
        case .extractionFailed: "郵便番号ZIPを展開できませんでした。"
        case .invalidEncoding: "郵便番号データがUTF-8ではありません。"
        case .invalidCSV: "郵便番号CSVの形式が不正です。"
        case .invalidSnapshot: "郵便番号辞書の形式が不正です。"
        case .invalidManifest: "郵便番号辞書の来歴情報が不正です。"
        case .unsupportedFileType: "郵便番号辞書が通常ファイルではありません。"
        case .fileTooLarge: "郵便番号辞書がサイズ上限を超えています。"
        case .operationInProgress: "郵便番号データの更新が進行中です。"
        case .rollbackForwardCommitted: "旧辞書への復旧に失敗したため、検証済みの新しい辞書へ状態を揃えました。"
        case .runtimeUnavailable: "郵便番号変換を一時停止しました。Muuを再起動してください。"
        case .posix(let operation, let code): "郵便番号辞書の\(operation)に失敗しました (\(code))。"
        }
    }
}

private final class PostalCodeDownloader: NSObject, URLSessionDataDelegate {
    private var completion: ((Result<(Data, HTTPURLResponse), Error>) -> Void)?
    private var data = Data()
    // Assigned only by main; cancellation does not mutate delegate-owned data.
    private var task: URLSessionDataTask?
    private lazy var session = URLSession(
        configuration: .ephemeral, delegate: self, delegateQueue: nil)

    func download(
        _ url: URL, completion: @escaping (Result<(Data, HTTPURLResponse), Error>) -> Void
    ) {
        self.completion = completion
        data = Data()
        let task = session.dataTask(with: url)
        self.task = task
        task.resume()
    }

    func cancel() { precondition(Thread.isMainThread); task?.cancel() }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        let size = self.data.count.addingReportingOverflow(data.count)
        guard !size.overflow, size.partialValue <= PostalCodeStore.maxZIPBytes else {
            dataTask.cancel()
            finish(.failure(PostalCodeStoreError.responseTooLarge))
            return
        }
        self.data.append(data)
    }

    func urlSession(
        _ session: URLSession,
        dataTask: URLSessionDataTask,
        didReceive response: URLResponse,
        completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
    ) {
        guard let response = response as? HTTPURLResponse,
            response.statusCode == 200,
            response.expectedContentLength <= Int64(PostalCodeStore.maxZIPBytes)
                || response.expectedContentLength == NSURLSessionTransferSizeUnknown
        else {
            completionHandler(.cancel)
            finish(.failure(PostalCodeStoreError.invalidResponse))
            return
        }
        completionHandler(.allow)
    }

    // Redirects must stay on https and on the source host; refusing one ends
    // the transfer with the redirect response, which fails the 200 check.
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        let url = newRequest.url
        completionHandler(
            url?.scheme == "https" && url?.host == PostalCodeStore.sourceURL.host
                ? newRequest
                : nil
        )
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?)
    {
        if let error {
            finish(.failure(error))
            return
        }
        guard let response = task.response as? HTTPURLResponse,
            response.statusCode == 200,
            response.value(forHTTPHeaderField: "Content-Type")?.lowercased().contains("zip") == true
        else {
            finish(.failure(PostalCodeStoreError.invalidResponse))
            return
        }
        finish(.success((data, response)))
    }

    private func finish(_ result: Result<(Data, HTTPURLResponse), Error>) {
        guard let completion else { return }
        self.completion = nil
        completion(result)
    }
}

// These are display/operation DTOs, never encoders for TSV or manifest files.
struct PostalCodeView: Decodable {
    let schemaVersion: Int
    let managementRevision: UInt64
    let access: String
    let layerStatus: String
    let reason: String?
    let manifest: PostalCodeManifest?
    let generation: String?
    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case managementRevision = "management_revision"
        case access
        case layerStatus = "layer_status"
        case reason, manifest, generation
    }
}
struct PostalCodeTicket: Codable {
    let domain: String
    let owner: UInt64
    let operation: UInt64
    let expectedRevision: UInt64
    enum CodingKeys: String, CodingKey {
        case domain, owner, operation
        case expectedRevision = "expected_revision"
    }
    var bytes: Data {
        get throws {
            let data = try JSONEncoder().encode(self)
            guard data.count <= 1024 else { throw postalFailure("invalid_ticket") }
            return data
        }
    }
}
struct PostalCodeRequest: Decodable {
    enum Kind: String, Decodable {
        case generationManifest = "generation_manifest"
        case generationTSV = "generation_tsv"
        case seedManifest = "seed_manifest"
        case seedTSV = "seed_tsv"
    }
    let requestID: UInt64
    let kind: Kind
    let generation: String?
    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case kind, generation
    }
}
struct PostalCodeReady: Decodable {
    enum Kind: String, Decodable {
        case loaded
        case installSeed = "install_seed"
        case update
    }
    let kind: Kind
    let publishedRevision: UInt64
    let recoveryRevision: UInt64
    let generation: String?
    let previousGeneration: String?
    let currentBytesBase64: String?
    let nextView: PostalCodeView
    let rollbackView: PostalCodeView
    let disabledView: PostalCodeView
    enum CodingKeys: String, CodingKey {
        case kind
        case publishedRevision = "published_revision"
        case recoveryRevision = "recovery_revision"
        case generation
        case previousGeneration = "previous_generation"
        case currentBytesBase64 = "current_bytes_base64"
        case nextView = "next_view"
        case rollbackView = "rollback_view"
        case disabledView = "disabled_view"
    }
}
struct PostalCodeStep: Decodable {
    let schemaVersion: Int
    let state: String
    let ticket: PostalCodeTicket
    let request: PostalCodeRequest?
    let ready: PostalCodeReady?
    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case state, ticket, request, ready
    }
    static func ticket(from bytes: Data) throws -> PostalCodeTicket {
        struct Envelope: Decodable { let ticket: PostalCodeTicket }
        return try JSONDecoder().decode(Envelope.self, from: bytes).ticket
    }
    static func decode(_ bytes: Data) throws -> Self {
        guard !bytes.isEmpty, bytes.count <= PostalCodeStore.maxPlanBytes else {
            throw postalFailure("invalid_plan")
        }
        let value = try JSONDecoder().decode(Self.self, from: bytes)
        guard value.schemaVersion == 1, value.ticket.domain == "postal_code",
            (value.state == "needs_file" && value.request != nil && value.ready == nil)
                || (value.state == "ready" && value.request == nil && value.ready != nil)
        else { throw postalFailure("invalid_plan") }
        return value
    }
}
struct PostalCodeCoreBridge {
    typealias Reply = (tsv: Data, manifest: Data, plan: Data)
    var view: () throws -> Data
    var loadBegin: (Data, Data) throws -> Reply
    var loadSupply: (Data, Data) throws -> Reply
    var prepare: (Data, Data) throws -> Reply
    var publish: (Data) -> Int32
    var rollback: (Data) -> Int32
    var disable: (Data) -> Int32
    var finish: (Data) -> Int32
    var cancel: (Data) -> Int32
    // No Core (test fixtures, the default bridge). The stub sends nothing, so
    // every ACK is a definite refusal: CREATE_FAILED, as in User and Imported,
    // which does not stop the store. A real bridge's CORE_INVALID remains
    // ambiguous and stops it.
    static let unavailable = Self(
        view: { throw postalFailure("core_unavailable") },
        loadBegin: { _, _ in throw postalFailure("core_unavailable") },
        loadSupply: { _, _ in throw postalFailure("core_unavailable") },
        prepare: { _, _ in throw postalFailure("core_unavailable") }, publish: { _ in MUU_RESULT_CREATE_FAILED },
        rollback: { _ in MUU_RESULT_CREATE_FAILED }, disable: { _ in MUU_RESULT_CREATE_FAILED }, finish: { _ in MUU_RESULT_CREATE_FAILED }, cancel: { _ in MUU_RESULT_CREATE_FAILED })
}
struct PostalCodeOperationError: Error, LocalizedError {
    let status: Int32
    let code: String
    var errorDescription: String? {
        switch code {
        case "completion_failed": return "更新の終了処理に異常がありました。入力メソッドを再起動すると更新できます。"
        case "runtime_unavailable":
            return PostalCodeStoreError.runtimeUnavailable.localizedDescription
        case "invalid_csv": return PostalCodeStoreError.invalidCSV.localizedDescription
        case "invalid_encoding": return PostalCodeStoreError.invalidEncoding.localizedDescription
        case "invalid_zip": return PostalCodeStoreError.extractionFailed.localizedDescription
        case "file_too_large", "invalid_postal_code_layer":
            return PostalCodeStoreError.fileTooLarge.localizedDescription
        case "invalid_manifest": return PostalCodeStoreError.invalidManifest.localizedDescription
        case "postal_file_missing": return PostalCodeStoreError.unavailableSeed.localizedDescription
        // Includes ABI 13's `revision_overflow`, shown like the earlier generic
        // `core_operation_failed`.
        default: return PostalCodeStoreError.invalidSnapshot.localizedDescription
        }
    }
}
private func postalFailure(_ code: String, status: Int32 = MUU_RESULT_DECODE_ERR) -> PostalCodeOperationError {
    PostalCodeOperationError(status: status, code: code)
}

final class PostalCodeStore {
    // Transport bounds are checked before copying/reading; Core independently
    // validates both file syntax and the actual inflated/semantic data size.
    static let maxZIPBytes = 4 * 1024 * 1024
    static let maxSnapshotBytes = 16 * 1024 * 1024
    static let maxManifestBytes = 64 * 1024
    static let maxPlanBytes = 1024 * 1024
    static let sourceURL = URL(
        string:
            "https://www.post.japanpost.jp/service/search/zipcode/download/utf/zip/utf_ken_all.zip")!
    static let sourcePageURL =
        "https://www.post.japanpost.jp/service/search/zipcode/download/utf-zip.html"
    typealias Download = (URL, @escaping (Result<(Data, HTTPURLResponse), Error>) -> Void) -> Void
    static let shared: PostalCodeStore = {
        #if MUU_TEST
            let base = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
                .appendingPathComponent(".muu-postal-fixture-\(getpid())")
            let bridge = PostalCodeCoreBridge.unavailable
        #else
            let base = FileManager.default.urls(
                for: .applicationSupportDirectory, in: .userDomainMask
            ).first!
            let bridge = RustCore.postalCodeBridge
        #endif
        return PostalCodeStore(
            baseDirectory: base,
            bundleIdentifier: Bundle.main.bundleIdentifier ?? "dev.muu.inputmethod.Muu",
            seedDataURL: Bundle.main.url(forResource: "postal-code-seed", withExtension: "tsv"),
            seedManifestURL: Bundle.main.url(
                forResource: "postal-code-seed-manifest", withExtension: "json"), bridge: bridge)
    }()
    private static let logger = Logger(
        subsystem: "dev.muu.inputmethod.Muu", category: "postal-code-store")
    let rootURL: URL
    let currentURL: URL
    let generationsURL: URL
    private let baseDirectory: URL
    private let seedDataURL: URL?
    private let seedManifestURL: URL?
    private let bridge: PostalCodeCoreBridge
    private let queue = DispatchQueue(label: "dev.muu.postal-code.io", qos: .utility)
    private let injection: ((String) throws -> Void)?
    private let fatalRuntimeRecovery: () -> Void
    private let download: Download
    private let downloader: PostalCodeDownloader?
    private let clock: () -> Int64
    // UI state is main-thread only. The serial queue owns the whole operation,
    // including recovery and GC; no main.sync call can deadlock a live client.
    private(set) var startupState: DataStartupState = .notStarted
    private var startupToken: UUID?
    // One existing management window observes phase changes; no polling bus.
    var startupStateDidChange: (() -> Void)?
    // Final cancel runs after the operation's main completion. If it stops
    // updates, refresh that window from the final facts, on main as well.
    var updatesDidStop: (() -> Void)?
    private var stopped = false
    private let shutdownGate = StoreShutdownGate()
    private var pendingCompletion: IOCompletionLease?
    // Screen acknowledgement can still be queued when shutdown starts.
    // Keep this fact on IO, including .preserve from a later private failure.
    private var ioDurabilityWarning = false
    private(set) var initialLoadFailed = false
    private(set) var isUpdating = false
    private(set) var view: PostalCodeView?
    private(set) var loadError: Error?
    private(set) var durabilityWarning: Error?
    private(set) var recoveryNotice: String?
    var manifest: PostalCodeManifest? { view?.manifest }
    var canUpdate: Bool { startupState.finished && !isClosing && !isUpdating && !stopped }
    var hasStarted: Bool { startupState != .notStarted }
    var updatesStopped: Bool { stopped }
    var isClosing: Bool { shutdownGate.isClosing }
    var statusMessages: [String] {
        var messages: [String] = []
        if let error = loadError { messages.append(error.localizedDescription) }
        if let warning = durabilityWarning {
            messages.append("更新は反映済みですが、注意事項があります: \(warning.localizedDescription)")
        }
        if let notice = recoveryNotice { messages.append(notice) }
        return messages
    }
    var dataDirectoryURL: URL { rootURL }
    init(
        baseDirectory: URL, bundleIdentifier: String, seedDataURL: URL?, seedManifestURL: URL?,
        bridge: PostalCodeCoreBridge = .unavailable, injection: ((String) throws -> Void)? = nil,
        fatalRuntimeRecovery: @escaping () -> Void = { exit(EXIT_FAILURE) },
        downloadOverride: Download? = nil,
        // ISO8601DateFormatter drops fractional seconds toward the preceding
        // second, including pre-epoch dates; Int64 alone truncates toward zero.
        clock: @escaping () -> Int64 = { Int64(floor(Date().timeIntervalSince1970)) }
    ) {
        self.baseDirectory = baseDirectory
        rootURL = baseDirectory.appendingPathComponent(bundleIdentifier, isDirectory: true)
            .appendingPathComponent("postal_code", isDirectory: true).appendingPathComponent(
                "v1", isDirectory: true)
        currentURL = rootURL.appendingPathComponent("current")
        generationsURL = rootURL.appendingPathComponent("generations", isDirectory: true)
        self.seedDataURL = seedDataURL
        self.seedManifestURL = seedManifestURL
        self.bridge = bridge
        self.injection = injection
        self.fatalRuntimeRecovery = fatalRuntimeRecovery
        self.clock = clock
        if let downloadOverride {
            downloader = nil
            download = downloadOverride
        } else {
            let downloader = PostalCodeDownloader()
            self.downloader = downloader
            download = downloader.download
        }
        // Construction performs no file IO, Core operation, or seed install.
        // AppDelegate calls start only after process Core initialization.
    }
    func start(completion: @escaping (Result<Void, Error>) -> Void = { _ in }) {
        precondition(Thread.isMainThread)
        guard !isClosing else { completion(.failure(StoreShutdownCancellation())); return }
        guard startupState == .notStarted else {
            completion(.success(()))
            return
        }
        let token = UUID(); startupToken = token
        startupState = .loading
        LifecycleLog.shared.startup(.postal, .loading)
        isUpdating = true
        startupStateDidChange?()
        queue.async { [self] in
            var ticket: PostalCodeTicket?
            let outcome: Outcome
            do {
                LifecycleLog.shared.startupWorker(.postal, phase: .begin)
                try shutdownGate.checkOpen()
                let baseline = try JSONDecoder().decode(PostalCodeView.self, from: bridge.view())
                let current = readState(currentURL, limit: 128)
                let initial = try bridge.loadBegin(
                    try json([
                        "expected_revision": baseline.managementRevision,
                        "current_state": current.state,
                    ]), current.bytes)
                ticket = try PostalCodeStep.ticket(from: initial.plan)
                let ready = try drive(initial)
                LifecycleLog.shared.startupWorker(.postal, phase: .prepared)
                outcome = transact(ready.reply, step: ready.step)
            } catch { outcome = Outcome(error: error) }
            LifecycleLog.shared.startupWorker(.postal, phase: outcome.error is StoreShutdownCancellation ? .cancelled : outcome.error == nil ? .finished : .failed)
            deliver(outcome, ticket: ticket, initialLoad: true, startup: token, completion: completion)
        }
    }
    func updateFromJapanPost(completion: @escaping (Result<Void, Error>) -> Void) {
        precondition(Thread.isMainThread)
        guard canUpdate else {
            completion(
                .failure(
                    stopped
                        ? postalFailure("completion_failed")
                        : PostalCodeStoreError.operationInProgress))
            return
        }
        isUpdating = true
        download(Self.sourceURL) { [self] result in
            // Runs on URLSession's delegate queue, not main; `isClosing` is
            // lock-backed. A late download cannot start a new prepare after
            // shutdown.
            guard !isClosing else { return }
            DispatchQueue.main.async { [self] in
                guard !isClosing else { return }
                let revision = view?.managementRevision
                queue.async { [self] in
                    var ticket: PostalCodeTicket?
                    let outcome: Outcome
                    do {
                        try shutdownGate.checkOpen()
                        let (zip, response) = try result.get()
                        guard zip.count <= Self.maxZIPBytes else {
                            throw PostalCodeStoreError.responseTooLarge
                        }
                        let currentRevision: UInt64
                        if let revision {
                            currentRevision = revision
                        } else {
                            currentRevision = try JSONDecoder().decode(
                                PostalCodeView.self, from: bridge.view()
                            ).managementRevision
                        }
                        let command = try json([
                            "expected_revision": currentRevision, "source": "japan_post",
                            "retrieved_unix_seconds": clock(),
                            "last_modified":
                                (response.value(forHTTPHeaderField: "Last-Modified") as Any?)
                                ?? NSNull(),
                        ])
                        let reply = try bridge.prepare(command, zip)
                        ticket = try PostalCodeStep.ticket(from: reply.plan)
                        let step = try PostalCodeStep.decode(reply.plan)
                        outcome = transact(reply, step: step)
                    } catch {
                        let latest = (try? bridge.view()).flatMap {
                            try? JSONDecoder().decode(PostalCodeView.self, from: $0)
                        }
                        outcome = Outcome(view: latest, error: error)
                    }
                    deliver(outcome, ticket: ticket, completion: completion)
                }
            }
        }
    }
    private enum Warning {
        case preserve
        case replace(Error?)
    }
    private struct Outcome {
        var view: PostalCodeView?
        var error: Error?
        var warning: Warning = .preserve
        var stop = false
        var notice: String?
    }
    func beginShutdown(completion: @escaping (ShutdownWorkResult) -> Void) {
        precondition(Thread.isMainThread)
        shutdownGate.close()
        downloader?.cancel()
        queue.async { [self] in
            pendingCompletion?.finish()
            if ioDurabilityWarning { LifecycleLog.shared.storeWarning(.postal) }
            completion(shutdownGate.shutdownResult)
        }
    }
    private func deliver(
        _ result: Outcome, ticket: PostalCodeTicket?, initialLoad: Bool = false, startup: UUID? = nil,
        completion: @escaping (Result<Void, Error>) -> Void
    ) {
        precondition(!Thread.isMainThread)
        // Bridge failures reach the screen only through here: unwrap the
        // original for the screen, and stop if the helper's cancel failed,
        // since the reservation may still be held. The helper's attempt also
        // spends this ticket's cancel, so the lease below skips it, as in
        // Imported.
        var result = result
        let localCancellation = result.error as? DictionaryPreparationFailure
        if let failure = localCancellation {
            result.error = failure.original
            result.stop = result.stop || failure.cancellationStatus != MUU_RESULT_OK
        }
        let completionTicket = localCancellation == nil ? ticket : nil
        let newWarning: Bool
        if case .replace(let warning) = result.warning { ioDurabilityWarning = warning != nil; newWarning = warning != nil }
        else { newWarning = false }
        let lease = IOCompletionLease { [self] in
            pendingCompletion = nil
            let status = completionTicket.map { (try? $0.bytes).map(bridge.cancel) ?? MUU_RESULT_INTERNAL } ?? MUU_RESULT_OK
            if status != MUU_RESULT_OK { Self.logger.error("Postal completion failed; result=\(status, privacy: .public)") }
            shutdownGate.recordShutdownResult(status != MUU_RESULT_OK || newWarning || (!(result.error is StoreShutdownCancellation) && (result.stop || result.error != nil)) ? .failed : .settled)
            if isClosing { return }
            DispatchQueue.main.async { [self] in
                guard !isClosing else { return }
                let stoppedByCleanup = status != MUU_RESULT_OK && !result.stop
                if stoppedByCleanup {
                    stopped = true
                    loadError = postalFailure("completion_failed", status: status)
                }
                let previous = startupState
                if (initialLoad && startup == startupToken) || (!initialLoad && result.error == nil && !stopped) {
                    startupState = .completed(access: view?.access, failed: stopped || status != MUU_RESULT_OK || result.error != nil)
                    LifecycleLog.shared.startup(.postal, startupState)
                }
                isUpdating = false
                if startupState != previous { startupStateDidChange?() }
                if stoppedByCleanup { updatesDidStop?() }
            }
        }
        pendingCompletion = lease
        if isClosing { lease.finish(); return }
        DispatchQueue.main.async { [self] in
            guard !isClosing else { return }
            if initialLoad || result.error == nil { initialLoadFailed = result.error != nil }
            if let view = result.view { self.view = view }
            if case .replace(let warning) = result.warning {
                durabilityWarning = warning
                recoveryNotice = result.notice
            }
            if result.stop || (result.error as? PostalCodeOperationError)?.status == MUU_RESULT_CORE_INVALID { stopped = true }
            loadError = result.error ?? (view?.reason.map { postalFailure($0) })
            completion(result.error.map(Result.failure) ?? .success(()))
            queue.async { lease.finish() }
        }
    }
    private func drive(_ initial: PostalCodeCoreBridge.Reply) throws -> (
        reply: PostalCodeCoreBridge.Reply, step: PostalCodeStep
    ) {
        var reply = initial
        while true {
            try shutdownGate.checkOpen()
            let step = try PostalCodeStep.decode(reply.plan)
            guard let request = step.request else { return (reply, step) }
            let url: URL?
            let limit: Int
            switch request.kind {
            case .generationManifest:
                url = try generation(request.generation).appendingPathComponent("manifest.json")
                limit = Self.maxManifestBytes
            case .generationTSV:
                url = try generation(request.generation).appendingPathComponent("postal-code.tsv")
                limit = Self.maxSnapshotBytes
            case .seedManifest:
                url = seedManifestURL
                limit = Self.maxManifestBytes
            case .seedTSV:
                url = seedDataURL
                limit = Self.maxSnapshotBytes
            }
            try injection?("read_\(request.kind.rawValue)")
            let file = url.map { readState($0, limit: limit) } ?? (state: "missing", bytes: Data())
            reply = try bridge.loadSupply(
                try json([
                    "ticket": try ticketObject(step.ticket), "request_id": request.requestID,
                    "file_state": file.state,
                ]), file.bytes)
        }
    }
    private func validate(_ reply: PostalCodeCoreBridge.Reply, step: PostalCodeStep) throws
        -> PostalCodeReady
    {
        guard let p = step.ready, step.ticket.domain == "postal_code",
            p.nextView.schemaVersion == 1,
            p.nextView.managementRevision == p.publishedRevision,
            p.nextView.generation == p.generation,
            p.rollbackView.managementRevision == p.recoveryRevision,
            p.disabledView.managementRevision == p.recoveryRevision,
            p.rollbackView.generation == p.previousGeneration,
            p.disabledView.generation == p.previousGeneration,
            step.ticket.expectedRevision.addingReportingOverflow(1).partialValue
                == p.publishedRevision,
            !step.ticket.expectedRevision.addingReportingOverflow(1).overflow,
            step.ticket.expectedRevision.addingReportingOverflow(2).partialValue
                == p.recoveryRevision,
            !step.ticket.expectedRevision.addingReportingOverflow(2).overflow
        else { throw postalFailure("invalid_plan") }
        if p.kind == .loaded {
            guard reply.tsv.isEmpty && reply.manifest.isEmpty else {
                throw postalFailure("invalid_plan")
            }
        } else {
            guard !reply.tsv.isEmpty, reply.tsv.count <= Self.maxSnapshotBytes,
                !reply.manifest.isEmpty, reply.manifest.count <= Self.maxManifestBytes,
                let name = p.generation, FileSafety.sha256(reply.manifest) == name,
                let encoded = p.currentBytesBase64, let pointer = Data(base64Encoded: encoded),
                pointer.count <= 128
            else { throw postalFailure("invalid_plan") }
            _ = try generation(name)
        }
        return p
    }
    private func transact(_ reply: PostalCodeCoreBridge.Reply, step: PostalCodeStep) -> Outcome {
        let p: PostalCodeReady
        do {
            p = try validate(reply, step: step)
            try shutdownGate.checkOpen()
            try acknowledged(bridge.publish(try step.ticket.bytes))
        } catch {
            // Local validation has not published. Once an ACK is present,
            // only its known refusals prove that Core stayed unchanged.
            // Local failures carry status 4 (postalFailure's default) or none,
            // so they never stop updates. With an ACK: a known refusal or
            // BUSY fails this update only, and 8 also adopts Core's current
            // view; CORE_INVALID or an unknown value stops updates. Disk is
            // untouched either way.
            let status = (error as? PostalCodeOperationError)?.status
            let ack = status.map(DictionaryAcknowledgement.init)
            let current =
                ack == .revisionMismatch
                ? (try? bridge.view()).flatMap {
                    try? JSONDecoder().decode(PostalCodeView.self, from: $0)
                }
                : nil
            return Outcome(view: current, error: error,
                stop: ack.map { !$0.isDefinitelyUnpublished(allowBusy: true) } ?? false)
        }
        if p.kind == .loaded {
            do {
                try finish(step.ticket, outcome: "loaded", view: p.nextView)
                return Outcome(view: p.nextView)
            } catch { return fatal(error) }
        }
        // Core is already live with the new data here, and nothing is on disk
        // yet. Any throw from completeNew happens before the pointer's rename
        // (FileSafety returns only warnings after it), so the disk pointer is
        // still old. Recovery first makes Core match the disk (rollback), then
        // the disk match Core (one forward save), then empties only the postal
        // layer (disable); if none of those converges, the process exits and
        // the next launch takes the old pointer as the single authority.
        // 2/3/4/6/9 are definite refusals and may be retried; 7 (unknown), 8
        // (a conflicting revision) and anything unexpected stop recovery.
        do { return try completeNew(reply, step: step, p: p, forwarded: false) } catch {
            let original = error
            // Postal's existing recovery policy is rollback twice, one forward
            // save, then Postal-only disable twice. Never retry unknown (7) or
            // conflicting (8) ACKs, or turn a failed finish into a fresh save.
            if error is FinishFailure { return fatal(error) }
            for _ in 0..<2 {
                let status = (try? step.ticket.bytes).map(bridge.rollback) ?? MUU_RESULT_INTERNAL
                let ack = DictionaryAcknowledgement(status)
                if ack == .success {
                    do {
                        try finish(step.ticket, outcome: "rolled_back", view: p.rollbackView)
                        return Outcome(view: p.rollbackView, error: original)
                    } catch { return fatal(error) }
                }
                if !ack.canRetryRecovery {
                    return fatal(postalFailure("recovery_failed", status: status))
                }
            }
            do { return try completeNew(reply, step: step, p: p, forwarded: true) } catch {
                if error is FinishFailure { return fatal(error) }
                for _ in 0..<2 {
                    let status = (try? step.ticket.bytes).map(bridge.disable) ?? MUU_RESULT_INTERNAL
                    let ack = DictionaryAcknowledgement(status)
                    if ack == .success {
                        do {
                            try finish(step.ticket, outcome: "disabled", view: p.disabledView)
                            return Outcome(
                                view: p.disabledView, error: PostalCodeStoreError.runtimeUnavailable
                            )
                        } catch { return fatal(error) }
                    }
                    if !ack.canRetryRecovery {
                        return fatal(postalFailure("recovery_failed", status: status))
                    }
                }
                return fatal(postalFailure("recovery_failed"))
            }
        }
    }
    private struct FinishFailure: Error { let underlying: Error }
    private func completeNew(
        _ reply: PostalCodeCoreBridge.Reply, step: PostalCodeStep, p: PostalCodeReady,
        forwarded: Bool
    ) throws -> Outcome {
        try persistGeneration(reply.tsv, manifest: reply.manifest, name: p.generation!)
        guard let encoded = p.currentBytesBase64, let pointer = Data(base64Encoded: encoded) else {
            throw postalFailure("invalid_plan")
        }
        try safePath(currentURL)
        let warning = try FileSafety.atomicWrite(
            pointer, to: currentURL, temporaryPrefix: ".current",
            directoryOpenFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
            beforeRename: { try self.injection?("before_current_rename") },
            afterRename: {
                do {
                    try self.injection?("after_current_rename")
                    return nil
                } catch { return error }
            }, checkpoint: { try self.injection?("current_\($0)") }, posixError: posix)
        do {
            try finish(
                step.ticket,
                outcome: warning == nil ? "new_pointer_durable" : "new_pointer_warning",
                view: p.nextView)
        } catch { throw FinishFailure(underlying: error) }
        if warning == nil {
            do {
                try injection?("gc")
                try collect(retaining: p.generation!)
            } catch { Self.logger.error("Postal generation GC failed; category=io") }
        }
        return Outcome(
            view: p.nextView, warning: .replace(warning),
            notice: forwarded
                ? PostalCodeStoreError.rollbackForwardCommitted.localizedDescription : nil)
    }
    private func finish(_ ticket: PostalCodeTicket, outcome: String, view: PostalCodeView) throws {
        try acknowledged(
            bridge.finish(
                try json([
                    "ticket": try ticketObject(ticket), "outcome": outcome,
                    "revision": view.managementRevision,
                    "generation": view.generation as Any? ?? NSNull(),
                ])))
    }
    private func fatal(_ error: Error) -> Outcome {
        Self.logger.fault("Postal recovery failed; category=transaction")
        fatalRuntimeRecovery()
        return Outcome(error: error, stop: true)
    }
    private func acknowledged(_ status: Int32) throws {
        guard DictionaryAcknowledgement(status) == .success else { throw postalFailure("core_operation_failed", status: status) }
    }
    private func json(_ value: [String: Any]) throws -> Data {
        try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    }
    private func ticketObject(_ ticket: PostalCodeTicket) throws -> Any {
        try JSONSerialization.jsonObject(with: ticket.bytes)
    }
    private func generation(_ name: String?) throws -> URL {
        guard let name, FileSafety.isLowercaseSHA256(name)
        else { throw postalFailure("invalid_generation_name") }
        return generationsURL.appendingPathComponent(name, isDirectory: true)
    }
    // Only the state and bytes go to Core, which decides what each state means
    // for the request. The original error is not kept here; User keeps it.
    private func readState(_ url: URL, limit: Int) -> (state: String, bytes: Data) {
        let result = FileSafety.readState(url, maxBytes: limit, validate: { try safePath(url) },
            unsupportedFileType: { PostalCodeStoreError.unsupportedFileType },
            fileTooLarge: { PostalCodeStoreError.fileTooLarge }, posixError: posix)
        return (result.state.rawValue, result.bytes)
    }
    private func managedRead(_ url: URL, limit: Int) throws -> Data {
        try safePath(url)
        return try FileSafety.boundedRead(
            url, maxBytes: limit, unsupportedFileType: { PostalCodeStoreError.unsupportedFileType },
            fileTooLarge: { PostalCodeStoreError.fileTooLarge }, posixError: posix)
    }
    // Only directory creation and its durability are shared through
    // FileSafety. Staging, verification and GC of generations stay here: their
    // success and recovery rules are Postal's own (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
    private func ensureDirectories(_ target: URL) throws {
        try FileSafety.ensurePrivateDirectories(
            target, below: baseDirectory,
            unsupportedPath: { PostalCodeStoreError.unsupportedFileType },
            checkpoint: injection, posixError: posix)
    }
    private func write(_ data: Data, to url: URL, kind: String) throws {
        try injection?("write_\(kind)")
        var fd = open(url.path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard fd >= 0 else {
            throw PostalCodeStoreError.posix(operation: "create_\(kind)", code: errno)
        }
        defer { if fd >= 0 { close(fd) } }
        try FileSafety.writeAll(data, to: fd, operation: "write_\(kind)", posixError: posix)
        try injection?("\(kind)_fsync")
        guard fsync(fd) == 0 else {
            throw PostalCodeStoreError.posix(operation: "\(kind)_fsync", code: errno)
        }
        try injection?("close_\(kind)")
        let status = close(fd)
        fd = -1
        // POSIX close returns 0 on success. An ABI result constant happened to
        // share that value but is not the contract here.
        guard status == 0 else {
            throw PostalCodeStoreError.posix(operation: "close_\(kind)", code: errno)
        }
    }
    private func verifiedFile(_ expected: Data, name: String, directory: Int32) throws -> Int32 {
        let fd = openat(directory, name, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK)
        guard fd >= 0 else {
            throw PostalCodeStoreError.posix(operation: "open_existing", code: errno)
        }
        do {
            var info = stat()
            guard fstat(fd, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
                info.st_size == expected.count
            else { throw PostalCodeStoreError.invalidSnapshot }
            var buffer = [UInt8](repeating: 0, count: 64 * 1024)
            var offset = 0
            while true {
                let n = Darwin.read(fd, &buffer, buffer.count)
                if n < 0 && errno == EINTR { continue }
                guard n >= 0 else {
                    throw PostalCodeStoreError.posix(operation: "read_existing", code: errno)
                }
                if n == 0 { break }
                guard n <= expected.count - offset,
                    expected.withUnsafeBytes({ bytes in
                        memcmp(bytes.baseAddress!.advanced(by: offset), buffer, n) == 0
                    })
                else { throw PostalCodeStoreError.invalidSnapshot }
                offset += n
            }
            guard offset == expected.count else { throw PostalCodeStoreError.invalidSnapshot }
            return fd
        } catch {
            close(fd)
            throw error
        }
    }
    // A generation is named by its manifest's SHA-256. An existing directory
    // of that name is reused only if both files match byte for byte; it is
    // never overwritten. A corrupt seed-named generation therefore fails here
    // before any write, on every launch, and only an explicit update (whose
    // manifest, and so name, differs) can replace the seed's role.
    private func persistGeneration(_ tsv: Data, manifest: Data, name: String) throws {
        let destination = try generation(name)
        try safePath(destination)
        var info = stat()
        if lstat(destination.path, &info) == 0 {
            guard info.st_mode & S_IFMT == S_IFDIR else {
                throw PostalCodeStoreError.unsupportedFileType
            }
            let fd = open(destination.path, O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW)
            guard fd >= 0 else {
                throw PostalCodeStoreError.posix(operation: "open_generation", code: errno)
            }
            defer { close(fd) }
            // Validate BOTH bytes on the same FDs before any durability work.
            // A corrupt fixed seed generation therefore retries without writes.
            let tsvFD = try verifiedFile(tsv, name: "postal-code.tsv", directory: fd)
            defer { close(tsvFD) }
            let manifestFD = try verifiedFile(manifest, name: "manifest.json", directory: fd)
            defer { close(manifestFD) }
            for (file, point) in [
                (tsvFD, "existing_tsv_fsync"), (manifestFD, "existing_manifest_fsync"),
                (fd, "existing_generation_fsync"),
            ] {
                try injection?(point)
                guard fsync(file) == 0 else {
                    throw PostalCodeStoreError.posix(operation: point, code: errno)
                }
            }
            try injection?("existing_generations_fsync")
            try syncDirectory(generationsURL, operation: "existing_generations_fsync")
            return
        }
        guard errno == ENOENT else {
            throw PostalCodeStoreError.posix(operation: "lstat_generation", code: errno)
        }
        try ensureDirectories(generationsURL)
        let stage = generationsURL.appendingPathComponent(
            ".stage-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(
            at: stage, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: stage) }
        try write(tsv, to: stage.appendingPathComponent("postal-code.tsv"), kind: "tsv")
        try write(manifest, to: stage.appendingPathComponent("manifest.json"), kind: "manifest")
        try injection?("staging_fsync")
        try syncDirectory(stage, operation: "staging_fsync")
        try injection?("before_generation_rename")
        // Never replace a same-name generation, even an empty/corrupt one.
        guard
            renameatx_np(AT_FDCWD, stage.path, AT_FDCWD, destination.path, UInt32(RENAME_EXCL)) == 0
        else { throw PostalCodeStoreError.posix(operation: "rename_generation", code: errno) }
        try injection?("after_generation_rename")
        try injection?("generations_fsync")
        try syncDirectory(generationsURL, operation: "generations_fsync")
    }
    // Removes every other hash-named generation whose contents are only the
    // two expected regular files, including the previous generation and a
    // corrupt same-name seed generation. Anything unexpected inside is left
    // alone. Called only after the new pointer is durable and finish is
    // acknowledged, while this operation still holds Core's reservation.
    private func collect(retaining generation: String) throws {
        let generationsDescriptor = Darwin.open(
            generationsURL.path,
            O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW
        )
        guard generationsDescriptor >= 0 else { throw posix("open-prune-generations") }
        defer { _ = Darwin.close(generationsDescriptor) }

        let expectedChildren: Set<String> = ["postal-code.tsv", "manifest.json"]
        let names = try FileManager.default.contentsOfDirectory(atPath: generationsURL.path)
        var removed = false
        for name in names where name != generation && FileSafety.isLowercaseSHA256(name) {
            let generationDescriptor = Darwin.openat(
                generationsDescriptor,
                name,
                O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW
            )
            if generationDescriptor < 0, errno == ENOENT { continue }
            guard generationDescriptor >= 0 else { throw posix("open-prune-generation") }

            let generationURL = generationsURL.appendingPathComponent(name, isDirectory: true)
            let childNames: [String]
            do {
                childNames = try FileManager.default.contentsOfDirectory(
                    atPath: generationURL.path
                )
            } catch {
                _ = Darwin.close(generationDescriptor)
                throw error
            }
            guard Set(childNames).isSubset(of: expectedChildren) else {
                _ = Darwin.close(generationDescriptor)
                continue
            }

            var safeToRemove = true
            for childName in childNames {
                var metadata = stat()
                guard
                    Darwin.fstatat(
                        generationDescriptor,
                        childName,
                        &metadata,
                        AT_SYMLINK_NOFOLLOW
                    ) == 0,
                    metadata.st_mode & S_IFMT == S_IFREG
                else {
                    safeToRemove = false
                    break
                }
            }
            guard safeToRemove else {
                _ = Darwin.close(generationDescriptor)
                continue
            }

            for childName in childNames {
                guard Darwin.unlinkat(generationDescriptor, childName, 0) == 0 else {
                    let code = errno
                    _ = Darwin.close(generationDescriptor)
                    throw posix("unlinkat-prune-generation", code)
                }
            }
            _ = Darwin.close(generationDescriptor)
            guard Darwin.unlinkat(generationsDescriptor, name, AT_REMOVEDIR) == 0 else {
                if errno == ENOENT { continue }
                throw posix("unlinkat-prune-generation-directory")
            }
            removed = true
        }
        if removed, Darwin.fsync(generationsDescriptor) != 0 {
            throw posix("fsync-prune-generations")
        }
    }

    private func safePath(_ url: URL) throws {
        try FileSafety.rejectSymbolicLinkAncestors(
            of: url, fileManager: .default,
            nonAbsolutePathError: { PostalCodeStoreError.unsupportedFileType },
            symbolicLinkError: { _ in PostalCodeStoreError.unsupportedFileType })
    }
    private func posix(_ operation: String, _ code: Int32 = errno) -> Error {
        PostalCodeStoreError.posix(operation: operation, code: code)
    }
    private func syncDirectory(_ url: URL, operation: String) throws {
        try FileSafety.fsyncDirectory(
            url, openFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
            posixError: { _, code in PostalCodeStoreError.posix(operation: operation, code: code) })
    }
}
