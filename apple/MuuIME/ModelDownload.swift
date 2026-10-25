import CryptoKit
import Darwin
import Foundation
import Synchronization

#if MUU_TEST
struct ModelDownloadLateDeliveryForTesting {
    let deliver: (Data, DispatchSemaphore) -> Void
}
#endif

// The only failure facts the UI sees. Messages are built from these codes, not
// from URLSession or POSIX error text, so no URL or path reaches the screen.
enum ModelDownloadFailure: String, Error {
    case unsafePath = "unsafe_path", busy, io, http, redirect, response
    case length, hash, network, cancelled, durability
    case supportUnavailable = "support_unavailable"

    var message: String {
        switch self {
        case .unsafePath: return "安全でない保存先です"
        case .supportUnavailable: return "保存先を用意できません"
        case .busy: return "別の取得処理が実行中です"
        case .io: return "保存に失敗しました"
        case .http: return "サーバーから取得できませんでした"
        case .redirect: return "許可されていない転送先です"
        case .response: return "応答の形式が正しくありません"
        case .length, .hash: return "照合に失敗しました"
        case .network: return "通信に失敗しました"
        case .cancelled: return "中止しました"
        case .durability: return "配置済みですが、保存の耐久性を確認できませんでした"
        }
    }
}

enum ModelDownloadHTTP {
    // The redirect boundary, applied to the first URL, every redirect and
    // the final response. HTTPS only, no userinfo, port 443 or none, and a
    // host that is huggingface.co or hf.co or a dot-separated subdomain of
    // one (so `huggingface.co.evil` and `evilhf.co` fail). The query of a
    // signed CDN URL is kept as is. The SHA-256, not this check, is what
    // finally decides that the bytes are the right model.
    static func allows(_ url: URL) -> Bool {
        guard url.scheme?.lowercased() == "https", url.user == nil, url.password == nil,
              url.port == nil || url.port == 443, let host = url.host?.lowercased() else { return false }
        return ["huggingface.co", "hf.co"].contains { host == $0 || host.hasSuffix("." + $0) }
    }

    // A fresh GET for every hop: no cookies, no cached response, no carried
    // headers, and identity encoding so the bytes hashed are the bytes sent.
    // 60 s is the idle limit per request; the session bounds the whole
    // transfer to 15 minutes (see liveConfiguration).
    static func request(_ url: URL) -> URLRequest {
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 60)
        request.httpMethod = "GET"
        request.httpShouldHandleCookies = false
        request.setValue("identity", forHTTPHeaderField: "Accept-Encoding")
        return request
    }

    // Only 200 is accepted, so a 206 partial response is refused (nothing
    // is resumed). A Content-Length, when present, must equal the pinned
    // size; when absent, the running byte count enforces it instead.
    static func validate(_ response: URLResponse, expected: Int) throws {
        guard let response = response as? HTTPURLResponse,
              let url = response.url, allows(url) else { throw ModelDownloadFailure.response }
        guard response.statusCode == 200 else { throw ModelDownloadFailure.http }
        let encoding = response.value(forHTTPHeaderField: "Content-Encoding")?.lowercased()
        guard encoding == nil || encoding == "identity" else { throw ModelDownloadFailure.response }
        guard response.expectedContentLength < 0 || response.expectedContentLength == Int64(expected) else {
            throw ModelDownloadFailure.length
        }
    }
}

@MainActor
/// The two existing startup keys, read the way RustCore resolves them: an
/// unset Enabled is false, and an unset or unknown Model is zenz. Reading
/// writes nothing. `enableZenz` runs only after a successful, verified
/// acquisition, and writes Model before Enabled, so a crash between the two
/// can never enable a model that is not in place. When zenz is already
/// enabled it writes nothing, so an existing setup is left untouched.
struct ModelDownloadPreferences {
    private let boolSetting: (String) -> Bool
    private let stringSetting: (String) -> String?
    private let setString: (String, String) -> Void
    private let setBool: (Bool, String) -> Void
    static let live = Self(boolSetting: { UserDefaults.standard.bool(forKey: $0) },
        stringSetting: { UserDefaults.standard.string(forKey: $0) },
        setString: { UserDefaults.standard.set($0, forKey: $1) },
        setBool: { UserDefaults.standard.set($0, forKey: $1) })

    // Match the existing startup resolver: unset/unknown model means zenz;
    // unset Enabled stays false. Merely reading these facts changes nothing.
    var zenzEnabled: Bool { boolSetting("MuuConversionLmEnabled") && stringSetting("MuuConversionLmModel") != ConversionLmModel.edge.rawValue }
    func enableZenz() {
        guard !zenzEnabled else { return }
        setString(ConversionLmModel.zenz.rawValue, "MuuConversionLmModel")
        setBool(true, "MuuConversionLmEnabled")
    }

    #if MUU_TEST
    static func forTesting(boolSetting: @escaping (String) -> Bool = { _ in false },
        stringSetting: @escaping (String) -> String? = { _ in nil },
        setString: @escaping (String, String) -> Void = { _, _ in },
        setBool: @escaping (Bool, String) -> Void = { _, _ in }) -> Self {
        Self(boolSetting: boolSetting, stringSetting: stringSetting, setString: setString, setBool: setBool)
    }
    #endif
}

// Main never waits for the worker's filesystem IO. An atomic transition just
// before rename chooses cancellation or publication. Once publication claims
// the operation, cancel is a no-op even while the OS rename is still pending.
// This is the same winner boundary as mutual exclusion, without making main
// acquire an IO-held lock. Preparation/checkpoints happen before the claim.
private final class ModelPublicationGate {
    // 0 active, 1 cancelled, 2 publication claimed, 3 publication settled.
    private let state = Atomic<UInt8>(0)
    func cancel() {
        _ = state.compareExchange(expected: 0, desired: 1, ordering: .acquiringAndReleasing)
    }
    func check() throws {
        if state.load(ordering: .acquiring) == 1 { throw ModelDownloadFailure.cancelled }
    }
    // A lost claim means cancel won (reported as cancelled); a second claim
    // on one gate has no caller and is reported as io. If `body` throws, the
    // gate still settles at 3, so a later cancel cannot relabel the failure.
    func publish(_ body: () throws -> Void) throws {
        guard state.compareExchange(expected: 0, desired: 2, ordering: .acquiringAndReleasing).exchanged else {
            try check()
            throw ModelDownloadFailure.io
        }
        defer { state.store(3, ordering: .releasing) }
        try body()
    }
}

// File ownership stays on a serial worker, including cleanup. Each attempt
// owns one unique partial, so a late callback cannot unlink a later attempt.
//
// Order of a download: create `.muu-zenz-<UUID>.partial` (O_EXCL, 0600) in the
// model directory; append each chunk, never past the pinned size; then check
// the received and on-disk lengths, fsync the file, hash it through the same
// FD from offset 0, rename it over the destination inside the publication
// gate, and fsync the directory. A cache that is already valid gets the same
// durability order without a download: fsync through the descriptor it was
// hashed on, then the directory. The destination is touched only by that
// final rename of verified bytes, so any earlier failure leaves the previous
// file, valid or not, exactly as it was.
private final class ModelCacheTransaction {
    let directory: URL
    let destination: URL
    private let profile: ModelDownloadProfile
    private let checkpoint: (String) throws -> Void
    private var lockFD: Int32 = -1
    private var partialFD: Int32 = -1
    private var partial: URL?
    private var received = 0

    private static func safe(_ url: URL) throws {
        try FileSafety.rejectSymbolicLinkAncestors(of: url, fileManager: .default,
            nonAbsolutePathError: { ModelDownloadFailure.unsafePath },
            symbolicLinkError: { _ in ModelDownloadFailure.unsafePath })
    }
    private static func posix(_ operation: String, _ code: Int32) -> Error { ModelDownloadFailure.io }

    // A cache is valid by content alone: a regular file (not followed through
    // a symlink) of the pinned length and SHA-256. No receipt or manifest is
    // needed, so a file the CLI fetched counts too. A missing file or a wrong
    // length is "not valid"; a non-regular file is an unsafe path. `verified`
    // runs only after a match, on the same descriptor that was hashed, so the
    // caller can fsync exactly the bytes it checked.
    static func isValid(support: URL, profile: ModelDownloadProfile, check: () throws -> Void,
                        verified: ((Int32) throws -> Void)? = nil) throws -> Bool {
        let url = profile.destination(under: support)
        try safe(url)
        let fd = open(url.path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK)
        if fd < 0 {
            if errno == ENOENT { return false }
            throw ModelDownloadFailure.io
        }
        defer { close(fd) }
        var metadata = stat()
        guard fstat(fd, &metadata) == 0 else { throw ModelDownloadFailure.io }
        guard metadata.st_mode & S_IFMT == S_IFREG else { throw ModelDownloadFailure.unsafePath }
        guard metadata.st_size == profile.byteCount else { return false }
        guard try digest(fd, expected: profile.byteCount, check: check) == profile.sha256 else { return false }
        try verified?(fd)
        return true
    }

    private static func digest(_ fd: Int32, expected: Int, check: () throws -> Void) throws -> String {
        guard lseek(fd, 0, SEEK_SET) == 0 else { throw ModelDownloadFailure.io }
        var digest = SHA256()
        var buffer = [UInt8](repeating: 0, count: 64 * 1024)
        var total = 0
        while true {
            try check()
            let count = Darwin.read(fd, &buffer, buffer.count)
            if count < 0 {
                if errno == EINTR { continue }
                throw ModelDownloadFailure.io
            }
            if count == 0 { break }
            let (next, overflow) = total.addingReportingOverflow(count)
            guard !overflow, next <= expected else { throw ModelDownloadFailure.length }
            total = next
            digest.update(data: Data(buffer.prefix(count)))
        }
        guard total == expected else { throw ModelDownloadFailure.length }
        return FileSafety.hexLower(digest.finalize())
    }

    // Takes the profile's lock: a nonblocking flock on a 0600 file in the model
    // directory. A second app instance gets `busy` at once instead of waiting.
    // The lock is held until this transaction is released, and the lock file
    // stays in place. scripts/fetch-conversion-lm.sh does not take it; both
    // only ever rename verified bytes, so the two cannot leave a bad file.
    init(support: URL, profile: ModelDownloadProfile, checkpoint: @escaping (String) throws -> Void) throws {
        self.profile = profile; self.checkpoint = checkpoint
        destination = profile.destination(under: support)
        directory = destination.deletingLastPathComponent()
        try Self.safe(destination)
        try FileSafety.ensurePrivateDirectories(directory, below: support,
            unsupportedPath: { ModelDownloadFailure.unsafePath }, posixError: Self.posix)
        let lockURL = directory.appendingPathComponent(".muu-model-download.lock")
        try Self.safe(lockURL)
        lockFD = open(lockURL.path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK, 0o600)
        guard lockFD >= 0 else { throw ModelDownloadFailure.io }
        var metadata = stat()
        guard fstat(lockFD, &metadata) == 0, metadata.st_mode & S_IFMT == S_IFREG,
              fchmod(lockFD, 0o600) == 0 else { close(lockFD); lockFD = -1; throw ModelDownloadFailure.unsafePath }
        guard flock(lockFD, LOCK_EX | LOCK_NB) == 0 else {
            close(lockFD); lockFD = -1; throw ModelDownloadFailure.busy
        }
        // Only our crash leftovers, under the cross-process lock. No CLI or
        // edge partial is eligible, and symlinks are never followed/deleted.
        do {
            try FileSafety.removeUnreferencedRegularFiles(in: directory, keeping: [],
                eligible: { $0.hasPrefix(".muu-zenz-") && $0.hasSuffix(".partial") }, posixError: Self.posix)
        } catch {
            flock(lockFD, LOCK_UN); close(lockFD); lockFD = -1
            throw error
        }
    }

    deinit {
        // A test may observe the last actual size before unlink. It must not
        // infer a bounded write from the absence of a cleaned-up partial.
        try? checkpoint("before_cleanup")
        if partialFD >= 0 { close(partialFD) }
        if let partial { unlink(partial.path) }
        if lockFD >= 0 { flock(lockFD, LOCK_UN); close(lockFD) }
    }

    func start() throws {
        let url = directory.appendingPathComponent(".muu-zenz-\(UUID().uuidString).partial")
        try checkpoint("open_partial")
        partialFD = open(url.path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard partialFD >= 0 else { throw ModelDownloadFailure.io }
        partial = url
    }

    func append(_ data: Data) throws -> Int {
        let (total, overflow) = received.addingReportingOverflow(data.count)
        guard !overflow, total <= profile.byteCount else { throw ModelDownloadFailure.length }
        try checkpoint("write")
        try FileSafety.writeAll(data, to: partialFD, operation: "write_model", posixError: Self.posix)
        received = total
        try checkpoint("chunk_written")
        return total * 100 / profile.byteCount
    }

    func finish(gate: ModelPublicationGate) throws {
        guard received == profile.byteCount else { throw ModelDownloadFailure.length }
        var metadata = stat()
        guard fstat(partialFD, &metadata) == 0, metadata.st_size == profile.byteCount else {
            throw ModelDownloadFailure.length
        }
        try checkpoint("file_fsync")
        guard fsync(partialFD) == 0 else { throw ModelDownloadFailure.io }
        try checkpoint("hash")
        guard try Self.digest(partialFD, expected: profile.byteCount, check: gate.check) == profile.sha256 else {
            throw ModelDownloadFailure.hash
        }
        // Everything up to the claim can still be cancelled, including a
        // stall at the `rename` checkpoint. Inside the claim only the rename
        // itself runs; `publication_claimed` marks that point for tests.
        try checkpoint("before_publish")
        try Self.safe(destination)
        guard let partial else { throw ModelDownloadFailure.io }
        try checkpoint("rename")
        try gate.publish {
            try checkpoint("publication_claimed")
            guard rename(partial.path, destination.path) == 0 else { throw ModelDownloadFailure.io }
            self.partial = nil
        }
        // A post-rename failure is not a completed download. A later explicit
        // action can reverify and sync this valid cache without another GET.
        try syncPublished()
    }

    // The cache has been hashed and file-synced through the same descriptor
    // before this call. Sync the directory without a download or rewrite, so
    // a prior durability failure can complete without another GET. The cache's
    // bytes, inode and mtime stay unchanged.
    func syncExisting(gate: ModelPublicationGate) throws {
        try gate.check()
        try gate.publish { }
        try syncPublished()
    }

    func syncVerifiedFile(_ descriptor: Int32) throws {
        // Called before isValid closes the descriptor it hashed. CLI caches
        // receive the same file-before-directory durability treatment too.
        try checkpoint("existing_file_verified")
        try checkpoint("file_fsync")
        guard fsync(descriptor) == 0 else { throw ModelDownloadFailure.io }
    }

    private func syncPublished() throws {
        do {
            try checkpoint("published_directory_fsync")
            try FileSafety.fsyncDirectory(directory, openFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
                posixError: Self.posix)
        } catch { throw ModelDownloadFailure.durability }
    }
}

// One explicit acquisition. The URLSession delegate runs on `queue`, the same
// serial queue that owns the transaction, so network callbacks, file IO and
// cancellation are ordered without extra locks. A callback that arrives after
// `finish` (completion is nil) or for another task does nothing. `finish`
// delivers exactly one terminal event and releases session, FD and lock first.
private final class ModelDownloadAttempt: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    enum Event { case progress(Int), verifying, finished(Result<Void, ModelDownloadFailure>) }
    private let profile: ModelDownloadProfile
    private let support: URL
    private let configuration: () -> URLSessionConfiguration
    private let checkpoint: (String) throws -> Void
    private let queue = DispatchQueue(label: "net.muu-ime.model-download", qos: .utility)
    private let gate = ModelPublicationGate()
    private var completion: ((Event) -> Void)?
    private var transaction: ModelCacheTransaction?
    private var session: URLSession?
    private var task: URLSessionDataTask?
    private var responseAccepted = false
    private var redirects = 0
    private var lastPercent = -1

    init(profile: ModelDownloadProfile, support: URL, configuration: @escaping () -> URLSessionConfiguration,
         checkpoint: @escaping (String) throws -> Void, completion: @escaping (Event) -> Void) {
        self.profile = profile; self.support = support; self.configuration = configuration
        self.checkpoint = checkpoint; self.completion = completion
    }

    func start() {
        queue.async { [self] in
            do {
                try gate.check()
                transaction = try ModelCacheTransaction(support: support, profile: profile, checkpoint: checkpoint)
                if try ModelCacheTransaction.isValid(support: support, profile: profile, check: gate.check,
                    verified: { [self] descriptor in try gate.check(); try transaction!.syncVerifiedFile(descriptor) }) {
                    try transaction!.syncExisting(gate: gate)
                    finish(.success(()))
                    return
                }
                try gate.check()
                try transaction!.start()
                let delegateQueue = OperationQueue()
                delegateQueue.maxConcurrentOperationCount = 1
                delegateQueue.underlyingQueue = queue
                session = URLSession(configuration: configuration(), delegate: self, delegateQueue: delegateQueue)
                task = session!.dataTask(with: ModelDownloadHTTP.request(profile.url))
                task!.resume()
            } catch { finish(.failure(error as? ModelDownloadFailure ?? .io)) }
        }
    }

    func cancel(completion: (() -> Void)? = nil) {
        gate.cancel()
        queue.async { [self] in
            // A claimed publication owns the outcome, including a rename
            // still inside the OS. Cancellation must not relabel it.
            do { try gate.check() } catch { finish(.failure(.cancelled)) }
            completion?()
        }
    }

    private func finish(_ result: Result<Void, ModelDownloadFailure>) {
        guard let completion else { return }
        self.completion = nil
        task?.cancel(); task = nil
        session?.invalidateAndCancel(); session = nil
        transaction = nil // Close/unlink/release lock before admitting another attempt.
        completion(.finished(result))
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        guard completion != nil, self.task === task else { completionHandler(nil); return }
        // At most 10 redirects, each inside the HTTPS boundary of `allows`.
        // The next hop is rebuilt as a plain GET rather than reusing the
        // system's request, so nothing from the previous hop is carried.
        redirects += 1
        guard redirects <= 10, let url = request.url, ModelDownloadHTTP.allows(url) else {
            completionHandler(nil); finish(.failure(.redirect)); return
        }
        do { try gate.check() } catch { completionHandler(nil); finish(.failure(.cancelled)); return }
        completionHandler(ModelDownloadHTTP.request(url))
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
                    completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
        guard completion != nil, task === dataTask else { completionHandler(.cancel); return }
        do {
            try gate.check()
            try ModelDownloadHTTP.validate(response, expected: profile.byteCount)
            responseAccepted = true
            completionHandler(.allow)
        } catch {
            completionHandler(.cancel)
            finish(.failure(error as? ModelDownloadFailure ?? .response))
        }
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        guard completion != nil, task === dataTask, responseAccepted else { return }
        do {
            try gate.check()
            let percent = try transaction!.append(data)
            if percent != lastPercent { lastPercent = percent; completion?(.progress(percent)) }
        } catch { finish(.failure(error as? ModelDownloadFailure ?? .io)) }
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard completion != nil, self.task === task else { return }
        do {
            try gate.check()
            guard error == nil else { throw ModelDownloadFailure.network }
            guard responseAccepted else { throw ModelDownloadFailure.response }
            completion?(.verifying)
            try transaction!.finish(gate: gate)
            finish(.success(()))
        } catch { finish(.failure(error as? ModelDownloadFailure ?? .io)) }
    }

    #if MUU_TEST
    // Capture the real delegate/task pair while live, then replay its data
    // and completion after cancellation. This reaches our stale guards even
    // though URLSession itself would suppress callbacks after invalidation.
    func captureLateDelivery(_ callback: @escaping (ModelDownloadLateDeliveryForTesting?) -> Void) {
        queue.async { [self] in
            guard let session, let task, completion != nil else {
                DispatchQueue.main.async { callback(nil) }; return
            }
            let delivery = ModelDownloadLateDeliveryForTesting { [self] data, finished in
                queue.async { [self] in
                    urlSession(session, dataTask: task, didReceive: data)
                    urlSession(session, task: task, didCompleteWithError: nil)
                    finished.signal()
                }
            }
            DispatchQueue.main.async { callback(delivery) }
        }
    }
    #endif
}

/// State and user actions for model acquisition, on main. The worker owns
/// everything else. There is no automatic retry: a failed or cancelled
/// attempt stays in that state until the user starts another, and a
/// `generation` token keeps a stale attempt's events from overwriting a newer
/// state.
@MainActor
final class ModelDownloadStore {
    enum Cache: Equatable { case checking, missing, valid, unavailable(ModelDownloadFailure) }
    enum State: Equatable { case idle, receiving(Int), verifying, completed, cancelled, failed(ModelDownloadFailure) }
    static let shared = ModelDownloadStore(support: FileManager.default.urls(for: .applicationSupportDirectory,
        in: .userDomainMask).first, profile: .zenz, configuration: liveConfiguration, checkpoint: { _ in }, preferences: .live)

    private(set) var cache: Cache = .checking
    private(set) var state: State = .idle
    var onChange: (() -> Void)?
    private let support: URL?
    private let profile: ModelDownloadProfile
    private let configuration: () -> URLSessionConfiguration
    private let checkpoint: (String) throws -> Void
    private let preferences: ModelDownloadPreferences
    private var attempt: ModelDownloadAttempt?
    private var generation = 0
    private var closing = false

    private init(support: URL?, profile: ModelDownloadProfile, configuration: @escaping () -> URLSessionConfiguration,
                 checkpoint: @escaping (String) throws -> Void, preferences: ModelDownloadPreferences) {
        self.support = support; self.profile = profile; self.configuration = configuration; self.checkpoint = checkpoint
        self.preferences = preferences
    }

    var isConfiguredForZenz: Bool { preferences.zenzEnabled }

    // Ephemeral, with no URL cache, cookies or credentials. 60 s without data
    // ends a request, 15 minutes bounds the whole download, and a missing
    // network fails at once instead of waiting for connectivity.
    nonisolated private static func liveConfiguration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        configuration.urlCredentialStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 60
        configuration.timeoutIntervalForResource = 15 * 60
        configuration.waitsForConnectivity = false
        return configuration
    }

    // Once at startup, read-only and off main. This never creates a session,
    // edits the cache, or enables a model. Core startup does not wait for it.
    func start() {
        guard cache == .checking, generation == 0, !closing else { return }
        generation += 1
        let token = generation
        let support = support, profile = profile
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let result: Cache
            do {
                guard let support else { throw ModelDownloadFailure.supportUnavailable }
                result = try ModelCacheTransaction.isValid(support: support, profile: profile, check: {}) ? .valid : .missing
            } catch { result = .unavailable(error as? ModelDownloadFailure ?? .io) }
            DispatchQueue.main.async { [weak self] in
                guard let self, self.generation == token, !self.closing else { return }
                self.cache = result; self.onChange?()
            }
        }
    }

    // Only an explicit About action calls this. There is no timer or automatic
    // retry; even a good cache is rechecked under the lock before success.
    func acquire() {
        guard !closing, attempt == nil else { return }
        // No Application Support location: report it on every press, as both a
        // failed state and an unavailable cache, rather than doing nothing.
        guard let support else {
            cache = .unavailable(.supportUnavailable)
            state = .failed(.supportUnavailable)
            onChange?()
            return
        }
        generation += 1
        let token = generation
        state = .receiving(0)
        let next = ModelDownloadAttempt(profile: profile, support: support, configuration: configuration,
            checkpoint: checkpoint) { [weak self] event in
            DispatchQueue.main.async { [weak self] in
                guard let self, self.generation == token else { return }
                switch event {
                case .progress(let percent): self.state = .receiving(percent)
                case .verifying: self.state = .verifying
                case .finished(let result):
                    self.attempt = nil
                    switch result {
                    case .success:
                        self.cache = .valid; self.state = .completed
                        // This does not reload/reinitialize Core. Its immutable
                        // startup configuration is used until the next process.
                        if !self.closing { self.preferences.enableZenz() }
                    case .failure(.cancelled): self.state = .cancelled
                    case .failure(let failure): self.state = .failed(failure)
                    }
                }
                if !self.closing { self.onChange?() }
            }
        }
        attempt = next
        onChange?()
        next.start()
    }

    func cancel() { attempt?.cancel() }

    func beginShutdown(completion: @escaping (ShutdownWorkResult) -> Void) {
        closing = true; onChange = nil
        guard let attempt else { completion(.settled); return }
        // Completion runs on the worker, with its FD and lock released. It
        // does not depend on processing a main-queue UI completion.
        attempt.cancel { completion(.settled) }
    }

    #if MUU_TEST
    static func forTesting(support: URL?, bytes: Data,
        configuration: @escaping () -> URLSessionConfiguration,
        checkpoint: @escaping (String) throws -> Void = { _ in },
        preferences: ModelDownloadPreferences? = nil) -> ModelDownloadStore {
        ModelDownloadStore(support: support, profile: .fixture(bytes: bytes),
            configuration: configuration, checkpoint: checkpoint, preferences: preferences ?? .forTesting())
    }
    static func configurationForTesting() -> URLSessionConfiguration { liveConfiguration() }
    func captureLateDeliveryForTesting(_ callback: @escaping (ModelDownloadLateDeliveryForTesting?) -> Void) {
        guard let attempt else { callback(nil); return }
        attempt.captureLateDelivery(callback)
    }
    #endif
}
