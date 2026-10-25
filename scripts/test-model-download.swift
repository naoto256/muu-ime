import CryptoKit
import Darwin
import Foundation

// Serves every request from `plan`: one response, the body in two chunks,
// then success or the planned error. `hold` keeps a request open so a test can
// cancel mid-transfer, and `finish: false` sends the body but never ends it.
// `redirects` answers the first requests with 302s to those URLs, each
// carrying Authorization and Cookie headers that must not be forwarded.
// Every request is recorded, so a test can prove how many GETs an action made
// (often zero).
private final class FakeModelProtocol: URLProtocol {
    struct Plan {
        var bytes: Data
        var status = 200
        var headers: [String: String] = [:]
        var error: Error?
        var hold = false
        var finish = true
        var redirects: [URL] = []
    }
    private static let lock = NSLock()
    private static var plan = Plan(bytes: Data())
    private static var requests: [URLRequest] = []
    private static var held: FakeModelProtocol?
    static func reset(_ next: Plan) {
        lock.lock(); defer { lock.unlock() }
        plan = next; requests = []; held = nil
    }
    static var count: Int { lock.lock(); defer { lock.unlock() }; return requests.count }
    static var lastRequest: URLRequest? { lock.lock(); defer { lock.unlock() }; return requests.last }
    // Delivers the plan to the request held by `hold`, at a moment the test
    // chooses.
    static func releaseHeld() {
        lock.lock(); let instance = held; let plan = plan; held = nil; lock.unlock()
        precondition(instance != nil)
        instance?.deliver(plan)
    }
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        Self.lock.lock()
        Self.requests.append(request)
        let index = Self.requests.count - 1
        let plan = Self.plan
        if plan.hold { Self.held = self }
        Self.lock.unlock()
        if plan.hold { return }
        if index < plan.redirects.count {
            let response = HTTPURLResponse(url: request.url!, statusCode: 302, httpVersion: "HTTP/1.1", headerFields: nil)!
            var next = URLRequest(url: plan.redirects[index])
            next.setValue("synthetic", forHTTPHeaderField: "Authorization")
            next.setValue("synthetic", forHTTPHeaderField: "Cookie")
            client?.urlProtocol(self, wasRedirectedTo: next, redirectResponse: response)
            return
        }
        deliver(plan)
    }
    private func deliver(_ plan: Plan) {
        let response = HTTPURLResponse(url: request.url!, statusCode: plan.status,
            httpVersion: "HTTP/1.1", headerFields: plan.headers)!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        let split = plan.bytes.count / 2
        client?.urlProtocol(self, didLoad: plan.bytes.prefix(split))
        client?.urlProtocol(self, didLoad: plan.bytes.dropFirst(split))
        if let error = plan.error { client?.urlProtocol(self, didFailWithError: error) }
        else if plan.finish { client?.urlProtocolDidFinishLoading(self) }
    }
    override func stopLoading() { }
}

// Records the transaction's checkpoint names in order, and throws an IO error
// at `fault`, to fail one step of the publish sequence.
private final class Checkpoints {
    private let lock = NSLock()
    private var values: [String] = []
    var fault: String?
    var inspect: ((String) -> Void)?
    func record(_ value: String) throws {
        lock.lock(); values.append(value); lock.unlock()
        inspect?(value)
        if value == fault { throw ModelDownloadFailure.io }
    }
    var events: [String] { lock.lock(); defer { lock.unlock() }; return values }
}

// Stops the worker at one checkpoint until the test releases it, with a 10 s
// guard so a regression fails rather than hangs.
private final class PublicationBarrier: @unchecked Sendable {
    let phase: String
    let release = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var reached = false
    init(_ phase: String) { self.phase = phase }
    var entered: Bool { lock.lock(); defer { lock.unlock() }; return reached }
    func checkpoint(_ value: String) {
        guard value == phase else { return }
        lock.lock(); reached = true; lock.unlock()
        precondition(release.wait(timeout: .now() + 10) == .success, "publication fixture was not released")
    }
}

@main @MainActor
private enum ModelDownloadTests {
    static let bytes = Data("synthetic model bytes, never a real model".utf8)
    static let fm = FileManager.default
    static var assertions = 0
    static func check(_ condition: Bool, _ message: String = #function, line: Int = #line) {
        assertions += 1
        precondition(condition, "\(message), line \(line)")
    }
    // A hang guard for the offline fixtures, not a timing requirement.
    static func wait(line: Int = #line, _ predicate: () -> Bool) {
        let deadline = Date().addingTimeInterval(10)
        while !predicate() && Date() < deadline {
            RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01))
        }
        check(predicate(), "asynchronous condition timed out at caller line \(line)")
    }
    static func configuration() -> URLSessionConfiguration {
        let configuration = ModelDownloadStore.configurationForTesting()
        // Capture every request, including an unexpected host. No test can
        // fall through to the network, even if the production URL changes.
        configuration.protocolClasses = [FakeModelProtocol.self]
        return configuration
    }
    static func make(_ root: URL, _ name: String, checkpoints: Checkpoints = Checkpoints()) throws -> (ModelDownloadStore, URL) {
        let support = root.appendingPathComponent(name)
        try fm.createDirectory(at: support, withIntermediateDirectories: false)
        return (ModelDownloadStore.forTesting(support: support, bytes: bytes,
            configuration: configuration, checkpoint: checkpoints.record), support)
    }
    static func destination(_ support: URL) -> URL {
        support.appendingPathComponent("MuuIME/Models/zenz-v3.2-small/fixture.gguf")
    }
    static func main() throws {
        // Like the store/ABI tests, avoid macOS's symlinked /var temporary
        // hierarchy: the real cache policy deliberately rejects ancestors.
        let root = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent("build/model-test-\(UUID())")
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: root) }
        profileAndHTTP()
        try successAndCache(root)
        try failures(root)
        try safetyAndCancellation(root)
        try activation(root)
        try receiveCapBeforeEOF(root)
        try redirectResponses(root)
        try existingFileDurability(root)
        try publicationCancellation(root)
        try staleCallbacksAndShutdown(root)
        try stalledPublicationDoesNotBlockMain(root)
        print("model download: \(assertions) assertions passed (offline)")
    }
    // The pinned profile, written out here as literals so a changed constant
    // cannot pass by comparison with itself; the redirect boundary's accepted
    // and refused URLs; and the session's cache, cookie, timeout and
    // connectivity settings.
    static func profileAndHTTP() {
        let profile = ModelDownloadProfile.zenz
        check(profile.name == "zenz-v3.2-small")
        check(profile.fileName == "ggml-model-Q5_K_M.gguf")
        check(profile.byteCount == 73_871_936)
        check(profile.sha256 == "29c223d4c23327b80fd13ebb5ab2555057a46317997d5da391584ffbef0db673")
        check(profile.url.absoluteString == "https://huggingface.co/Miwa-Keita/zenz-v3.2-small-gguf/resolve/c67e03e07d215c869f591b274c1631170d3e11fe/ggml-model-Q5_K_M.gguf")
        for url in ["https://huggingface.co/file", "https://hf.co/file", "https://deep.cdn.hf.co/file?signature=preserved"] {
            check(ModelDownloadHTTP.allows(URL(string: url)!))
        }
        for url in ["http://huggingface.co/file", "https://huggingface.co.evil/file", "https://evilhf.co/file",
                    "https://other.example/file", "https://user@hf.co/file", "https://hf.co:444/file"] {
            check(!ModelDownloadHTTP.allows(URL(string: url)!))
        }
        let configuration = ModelDownloadStore.configurationForTesting()
        check(configuration.urlCache == nil && configuration.httpCookieStorage == nil && configuration.urlCredentialStorage == nil)
        check(configuration.timeoutIntervalForRequest == 60 && configuration.timeoutIntervalForResource == 900)
        check(!configuration.waitsForConnectivity)
    }
    // Startup only reads the cache and never sends a request. A second
    // acquire during the first is ignored, so one GET publishes the file in
    // order: fsync, hash, rename, then directory fsync. Acquiring again with a
    // valid cache sends nothing and leaves the file's inode, mtime and 0600
    // mode unchanged; no partial is left behind. A cache with no record of how
    // it was fetched is still valid by its content.
    static func successAndCache(_ root: URL) throws {
        FakeModelProtocol.reset(.init(bytes: bytes))
        let checkpoints = Checkpoints()
        let (store, support) = try make(root, "success", checkpoints: checkpoints)
        store.start()
        wait { store.cache != .checking }
        check(store.cache == .missing && FakeModelProtocol.count == 0, "startup cache=\(store.cache)")
        store.acquire()
        store.acquire()
        wait { store.state == .completed }
        check(FakeModelProtocol.count == 1)
        check(FakeModelProtocol.lastRequest?.httpMethod == "GET")
        check(FakeModelProtocol.lastRequest?.value(forHTTPHeaderField: "Accept-Encoding") == "identity")
        check(try Data(contentsOf: destination(support)) == bytes)
        let events = checkpoints.events
        check(events.firstIndex(of: "file_fsync")! < events.firstIndex(of: "hash")!)
        check(events.firstIndex(of: "hash")! < events.firstIndex(of: "rename")!)
        check(events.firstIndex(of: "rename")! < events.firstIndex(of: "published_directory_fsync")!)
        let before = try fm.attributesOfItem(atPath: destination(support).path)
        store.acquire()
        wait { store.state == .completed }
        let after = try fm.attributesOfItem(atPath: destination(support).path)
        check(FakeModelProtocol.count == 1)
        check(before[.systemFileNumber] as? NSNumber == after[.systemFileNumber] as? NSNumber)
        check(before[.modificationDate] as? Date == after[.modificationDate] as? Date)
        check(after[.posixPermissions] as? NSNumber == 0o600)
        check(try fm.contentsOfDirectory(atPath: destination(support).deletingLastPathComponent().path).filter { $0.hasSuffix(".partial") }.isEmpty)
        // A legacy cache has no acquisition receipt; its content is enough.
        let cacheOnly = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: configuration)
        cacheOnly.start(); wait { cacheOnly.cache != .checking }
        check(cacheOnly.cache == .valid && FakeModelProtocol.count == 1)
    }
    // Each network or verification failure (short, wrong hash, one byte too
    // many, wrong Content-Length, compressed, connection lost, and the
    // statuses 206, 403, 404, 429 and 500) is reported by its code with one GET,
    // and an existing invalid file survives byte for byte. Then a fault at
    // each publish step: before the rename the destination never appears;
    // after it, the directory fsync failure is `durability`, the bytes are in
    // place, and the next acquire completes without another GET.
    static func failures(_ root: URL) throws {
        var cases: [(FakeModelProtocol.Plan, ModelDownloadFailure)] = [
            (.init(bytes: bytes.dropLast()), .length),
            (.init(bytes: Data(repeating: 0, count: bytes.count)), .hash),
            (.init(bytes: bytes + Data([0])), .length),
            (.init(bytes: bytes, headers: ["Content-Length": "999"]), .length),
            (.init(bytes: bytes, headers: ["Content-Encoding": "gzip"]), .response),
            (.init(bytes: bytes.prefix(2), error: URLError(.networkConnectionLost)), .network)
        ]
        for status in [206, 403, 404, 429, 500] { cases.append((.init(bytes: bytes, status: status), .http)) }
        for (index, item) in cases.enumerated() {
            let (store, support) = try make(root, "failure-\(index)")
            let file = destination(support)
            try fm.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            let old = Data("previous invalid cache; must survive failed acquisition".utf8)
            try old.write(to: file)
            FakeModelProtocol.reset(item.0)
            store.acquire()
            wait { if case .failed = store.state { return true }; return false }
            check(store.state == .failed(item.1), "unexpected failure: \(store.state)")
            check(try Data(contentsOf: file) == old)
            check(FakeModelProtocol.count == 1)
        }
        for (index, fault) in ["open_partial", "write", "file_fsync", "hash", "before_publish", "rename", "published_directory_fsync"].enumerated() {
            let checkpoints = Checkpoints(); checkpoints.fault = fault
            let (store, support) = try make(root, "io-\(index)", checkpoints: checkpoints)
            FakeModelProtocol.reset(.init(bytes: bytes))
            store.acquire()
            wait { if case .failed = store.state { return true }; return false }
            if fault == "published_directory_fsync" {
                check(store.state == .failed(.durability))
                check(try Data(contentsOf: destination(support)) == bytes)
                checkpoints.fault = nil
                store.acquire(); wait { store.state == .completed }
                check(FakeModelProtocol.count == 1)
            } else { check(!fm.fileExists(atPath: destination(support).path)) }
        }
    }
    // A missing or non-directory base and a symlinked ancestor fail as
    // `unsafePath` before any request. Crash leftovers: only this app's
    // `.muu-zenz-*.partial` regular files are removed, and only by an explicit
    // acquire under the lock; startup leaves them, and a CLI partial or a
    // symlink is kept. A second store on the same profile gets `busy` while
    // the first holds the lock. Cancel mid-transfer publishes nothing, a retry
    // works, and after shutdown acquire sends nothing.
    static func safetyAndCancellation(_ root: URL) throws {
        let unresolved = ModelDownloadStore.forTesting(support: nil, bytes: bytes, configuration: configuration)
        var changes = 0
        unresolved.onChange = { changes += 1 }
        unresolved.acquire()
        check(unresolved.state == .failed(.supportUnavailable) && changes == 1)
        unresolved.acquire()
        check(unresolved.state == .failed(.supportUnavailable) && changes == 2)
        for name in ["missing", "file"] {
            let support = root.appendingPathComponent("unsafe-\(name)")
            if name == "file" { try Data([1]).write(to: support) }
            let store = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: configuration)
            FakeModelProtocol.reset(.init(bytes: bytes))
            store.acquire()
            wait { store.state == .failed(.unsafePath) }
            check(FakeModelProtocol.count == 0)
        }
        let (unsafe, unsafeSupport) = try make(root, "symlink")
        try fm.createSymbolicLink(at: unsafeSupport.appendingPathComponent("MuuIME"), withDestinationURL: root)
        FakeModelProtocol.reset(.init(bytes: bytes))
        unsafe.acquire(); wait { unsafe.state == .failed(.unsafePath) }
        check(FakeModelProtocol.count == 0)
        let (crash, crashSupport) = try make(root, "crash")
        let directory = destination(crashSupport).deletingLastPathComponent()
        try fm.createDirectory(at: directory, withIntermediateDirectories: true)
        let stale = directory.appendingPathComponent(".muu-zenz-abandoned.partial")
        let cli = directory.appendingPathComponent("fixture.gguf.partial")
        let link = directory.appendingPathComponent(".muu-zenz-link.partial")
        try Data([1]).write(to: stale); try Data([2]).write(to: cli)
        try fm.createSymbolicLink(at: link, withDestinationURL: cli)
        crash.start(); wait { crash.cache != .checking }
        check(fm.fileExists(atPath: stale.path) && FakeModelProtocol.count == 0)
        FakeModelProtocol.reset(.init(bytes: bytes))
        crash.acquire(); wait { crash.state == .completed }
        check(!fm.fileExists(atPath: stale.path))
        check(try Data(contentsOf: cli) == Data([2]))
        check(try fm.destinationOfSymbolicLink(atPath: link.path) == cli.path)
        let (store, support) = try make(root, "cancel")
        FakeModelProtocol.reset(.init(bytes: bytes, hold: true))
        store.acquire(); wait { FakeModelProtocol.count == 1 }
        // A second instance cannot take over the same profile's partial.
        let contender = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: configuration)
        contender.acquire(); wait { contender.state == .failed(.busy) }
        store.cancel(); wait { store.state == .cancelled }
        check(!fm.fileExists(atPath: destination(support).path))
        FakeModelProtocol.reset(.init(bytes: bytes))
        store.acquire(); wait { store.state == .completed }
        check(FakeModelProtocol.count == 1)
        var shutdown = false
        let signal = DispatchSemaphore(value: 0)
        store.beginShutdown { _ in signal.signal() }
        shutdown = signal.wait(timeout: .now() + 1) == .success
        check(shutdown)
        let count = FakeModelProtocol.count
        store.acquire()
        check(FakeModelProtocol.count == count)
    }
    // Settings change only after a successful acquisition, and only when zenz
    // is not already the enabled model: from unset, disabled, or explicit
    // edge, it writes Model then Enabled; with Enabled=true and zenz, nothing.
    // A valid cache completes without a request and keeps its inode and mtime.
    // A failed or cancelled acquisition writes no setting.
    static func activation(_ root: URL) throws {
        for (index, initial) in [(nil, nil), (false, nil), (true, "zenz"), (true, "edge")]
            .enumerated() as EnumeratedSequence<[(Bool?, String?)]> {
            var enabled = initial.0, model = initial.1
            var writes: [String] = []
            let preferences = ModelDownloadPreferences.forTesting(boolSetting: { key in
                check(key == "MuuConversionLmEnabled"); return enabled ?? false
            }, stringSetting: { key in
                check(key == "MuuConversionLmModel"); return model
            }, setString: { value, key in
                check(key == "MuuConversionLmModel" && value == "zenz")
                writes.append(key); model = value
            }, setBool: { value, key in
                check(key == "MuuConversionLmEnabled" && value)
                writes.append(key); enabled = value
            })
            let support = root.appendingPathComponent("activation-\(index)")
            let file = destination(support)
            try fm.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try bytes.write(to: file)
            let before = try fm.attributesOfItem(atPath: file.path)
            FakeModelProtocol.reset(.init(bytes: bytes))
            let store = ModelDownloadStore.forTesting(support: support, bytes: bytes,
                configuration: configuration, preferences: preferences)
            store.start(); wait { store.cache == .valid }
            check(writes.isEmpty && FakeModelProtocol.count == 0)
            check(store.isConfiguredForZenz == (initial.0 == true && initial.1 != "edge"))
            store.acquire(); wait { store.state == .completed }
            check(FakeModelProtocol.count == 0 && store.isConfiguredForZenz)
            check(writes == (initial.0 == true && initial.1 != "edge" ? [] : ["MuuConversionLmModel", "MuuConversionLmEnabled"]))
            let after = try fm.attributesOfItem(atPath: file.path)
            check(before[.systemFileNumber] as? NSNumber == after[.systemFileNumber] as? NSNumber)
            check(before[.modificationDate] as? Date == after[.modificationDate] as? Date)
        }
        var writes = 0
        let preferences = ModelDownloadPreferences.forTesting(setString: { _, _ in writes += 1 }, setBool: { _, _ in writes += 1 })
        let support = root.appendingPathComponent("activation-failed")
        try fm.createDirectory(at: support, withIntermediateDirectories: false)
        let failed = ModelDownloadStore.forTesting(support: support, bytes: bytes,
            configuration: configuration, preferences: preferences)
        FakeModelProtocol.reset(.init(bytes: bytes, status: 500))
        failed.acquire(); wait { failed.state == .failed(.http) }
        check(writes == 0)
        FakeModelProtocol.reset(.init(bytes: bytes, hold: true))
        failed.acquire(); wait { FakeModelProtocol.count == 1 }
        failed.cancel(); wait { failed.state == .cancelled }
        check(writes == 0)
    }
    // A body larger than the pinned size, sent without ever finishing, must be
    // refused while streaming: the partial on disk never grows past the pinned
    // size, so the cap does not depend on the length check at completion.
    static func receiveCapBeforeEOF(_ root: URL) throws {
        let checkpoints = Checkpoints()
        let (store, support) = try make(root, "receive-cap", checkpoints: checkpoints)
        let directory = destination(support).deletingLastPathComponent()
        let lock = NSLock()
        var observed: [Int] = []
        checkpoints.inspect = { event in
            guard event == "chunk_written" || event == "before_cleanup" else { return }
            let sizes = (try? fm.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil))?.filter {
                $0.lastPathComponent.hasSuffix(".partial")
            }.compactMap { try? fm.attributesOfItem(atPath: $0.path)[.size] as? NSNumber }.map { $0.intValue } ?? []
            lock.lock(); observed.append(contentsOf: sizes); lock.unlock()
        }
        // No completion is sent. Only the streaming guard, not finish's final
        // length check, can reject this while the protocol is still open.
        // Enough data to pass URLSession's small-body coalescing without EOF.
        FakeModelProtocol.reset(.init(bytes: Data(repeating: 7, count: 256 * 1024), finish: false))
        store.acquire()
        wait { store.state == .failed(.length) }
        lock.lock(); let sizes = observed; lock.unlock()
        check(!sizes.isEmpty && sizes.max()! <= bytes.count,
            "actual partial sizes before cleanup exceeded the streaming cap: \(sizes)")
        check(FakeModelProtocol.count == 1)
    }
    // Redirects outside the boundary (another host, HTTP, userinfo, port 444)
    // and an eleventh redirect fail as `redirect` with nothing published. An
    // allowed CDN redirect keeps its query and arrives as a GET without the
    // Authorization or Cookie headers the redirect carried.
    static func redirectResponses(_ root: URL) throws {
        for (index, urls) in [[URL(string: "https://outside.example/file")!],
            [URL(string: "http://huggingface.co/file")!], [URL(string: "https://user@hf.co/file")!],
            [URL(string: "https://hf.co:444/file")!],
            Array(repeating: URL(string: "https://cdn.hf.co/next")!, count: 11)].enumerated() {
            FakeModelProtocol.reset(.init(bytes: bytes, redirects: urls))
            let (store, support) = try make(root, "redirect-\(index)")
            store.acquire()
            wait { if case .failed = store.state { return true }; return false }
            check(store.state == .failed(.redirect), "redirect was not rejected: \(store.state)")
            check(!fm.fileExists(atPath: destination(support).path))
            check(FakeModelProtocol.count == (index == 4 ? 11 : 1))
        }
        let url = URL(string: "https://deep.cdn.hf.co/file?signature=synthetic")!
        FakeModelProtocol.reset(.init(bytes: bytes, redirects: [url]))
        let (store, _) = try make(root, "redirect-good")
        store.acquire(); wait { store.state == .completed }
        check(FakeModelProtocol.count == 2 && FakeModelProtocol.lastRequest?.url == url)
        check(FakeModelProtocol.lastRequest?.httpMethod == "GET")
        check(FakeModelProtocol.lastRequest?.value(forHTTPHeaderField: "Authorization") == nil)
        check(FakeModelProtocol.lastRequest?.value(forHTTPHeaderField: "Cookie") == nil)
    }
    // An already valid cache is fsynced through the descriptor it was verified
    // on, before the directory. A file fsync failure stops there (no directory
    // fsync, no request); the next acquire completes with no request either.
    static func existingFileDurability(_ root: URL) throws {
        let checkpoints = Checkpoints()
        let (store, support) = try make(root, "existing-durability", checkpoints: checkpoints)
        let file = destination(support)
        try fm.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        try bytes.write(to: file)
        FakeModelProtocol.reset(.init(bytes: bytes))
        // These assertions observe the checkpoint and its fault propagation,
        // not the fsync syscall itself. Removing only the syscall while keeping
        // its checkpoint is outside what this harness can detect.
        checkpoints.fault = "file_fsync"
        store.acquire(); wait { store.state == .failed(.io) }
        check(!checkpoints.events.contains("published_directory_fsync") && FakeModelProtocol.count == 0)
        checkpoints.fault = nil
        store.acquire(); wait { store.state == .completed }
        let events = checkpoints.events
        check(events.lastIndex(of: "existing_file_verified")! < events.lastIndex(of: "file_fsync")!)
        check(events.lastIndex(of: "file_fsync")! < events.lastIndex(of: "published_directory_fsync")!)
        let stored = try Data(contentsOf: file)
        check(FakeModelProtocol.count == 0 && stored == bytes)
    }
    // Cancel while the worker is stopped at hashing or before the claim: the
    // download ends cancelled with no file and no setting written. Cancel
    // after publication (stopped at the directory fsync): it is a no-op, the
    // file is in place and the settings are written.
    static func publicationCancellation(_ root: URL) throws {
        for phase in ["hash", "before_publish", "published_directory_fsync"] {
            let support = root.appendingPathComponent("cancel-\(phase)")
            try fm.createDirectory(at: support, withIntermediateDirectories: false)
            let barrier = PublicationBarrier(phase)
            var writes: [String] = []
            let preferences = ModelDownloadPreferences.forTesting(setString: { _, key in writes.append(key) },
                setBool: { _, key in writes.append(key) })
            let store = ModelDownloadStore.forTesting(support: support, bytes: bytes,
                configuration: configuration, checkpoint: barrier.checkpoint, preferences: preferences)
            FakeModelProtocol.reset(.init(bytes: bytes))
            store.acquire(); wait { barrier.entered }
            // Gate cancellation runs independently of this blocked worker.
            store.cancel()
            barrier.release.signal()
            if phase == "published_directory_fsync" {
                wait { store.state == .completed }
                check(try Data(contentsOf: destination(support)) == bytes)
                check(writes == ["MuuConversionLmModel", "MuuConversionLmEnabled"])
            } else {
                wait { store.state == .cancelled }
                check(!fm.fileExists(atPath: destination(support).path) && writes.isEmpty)
            }
        }
    }
    // Callbacks captured from a cancelled attempt and replayed while a new
    // attempt runs must not touch the new partial (same inode, still empty) or
    // the new state. Shutdown with a request held completes off main while main
    // is deliberately blocked, publishes nothing, and admits no new acquire.
    static func staleCallbacksAndShutdown(_ root: URL) throws {
        let (store, support) = try make(root, "late-delivery")
        FakeModelProtocol.reset(.init(bytes: bytes, hold: true))
        store.acquire(); wait { FakeModelProtocol.count == 1 }
        var late: ModelDownloadLateDeliveryForTesting?
        store.captureLateDeliveryForTesting { late = $0 }
        wait { late != nil }
        store.cancel(); wait { store.state == .cancelled }
        FakeModelProtocol.reset(.init(bytes: bytes, hold: true))
        store.acquire(); wait { FakeModelProtocol.count == 1 }
        let directory = destination(support).deletingLastPathComponent()
        let partials = try fm.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil).filter {
            $0.lastPathComponent.hasSuffix(".partial")
        }
        check(partials.count == 1)
        let before = try fm.attributesOfItem(atPath: partials[0].path)
        let delivered = DispatchSemaphore(value: 0)
        late!.deliver(Data(repeating: 0, count: bytes.count * 4), delivered)
        check(delivered.wait(timeout: .now() + 2) == .success)
        let after = try fm.attributesOfItem(atPath: partials[0].path)
        check(before[.systemFileNumber] as? NSNumber == after[.systemFileNumber] as? NSNumber)
        check(after[.size] as? NSNumber == 0 && store.state == .receiving(0))
        FakeModelProtocol.releaseHeld(); wait { store.state == .completed }
        check(try Data(contentsOf: destination(support)) == bytes)

        let (closing, closingSupport) = try make(root, "shutdown-held")
        FakeModelProtocol.reset(.init(bytes: bytes, hold: true))
        closing.acquire(); wait { FakeModelProtocol.count == 1 }
        let done = DispatchSemaphore(value: 0)
        closing.beginShutdown { result in
            precondition(result == .settled && !Thread.isMainThread)
            done.signal()
        }
        // Deliberately block main: worker cleanup and lifecycle completion
        // must not depend on delivering its queued UI event to main.
        check(done.wait(timeout: .now() + 2) == .success)
        check(!fm.fileExists(atPath: destination(closingSupport).path))
        wait { closing.state == .cancelled }
        closing.acquire()
        check(closing.state == .cancelled && FakeModelProtocol.count == 1)
    }
    // A three-second stall at `rename` (before the claim) or inside the claim.
    // Cancel or shutdown from main must return within 2 s in every case, and
    // shutdown reaches its deadline with the download listed as unfinished
    // rather than waiting. A stall before the claim ends cancelled with no
    // file; a claimed publication still completes.
    static func stalledPublicationDoesNotBlockMain(_ root: URL) throws {
        for (phase, shutdown) in [("rename", true), ("rename", false),
                                  ("publication_claimed", true), ("publication_claimed", false)] {
            let support = root.appendingPathComponent("stalled-\(phase)-\(shutdown)")
            try fm.createDirectory(at: support, withIntermediateDirectories: false)
            let barrier = PublicationBarrier(phase)
            let store = ModelDownloadStore.forTesting(support: support, bytes: bytes,
                configuration: configuration, checkpoint: barrier.checkpoint)
            FakeModelProtocol.reset(.init(bytes: bytes))
            store.acquire(); wait { barrier.entered }
            // Reproduce a three-second stall immediately before rename.
            // A separate thread releases it even if main regresses into a
            // synchronous lock wait; the test then fails instead of hanging.
            DispatchQueue.global().asyncAfter(deadline: .now() + 3) { barrier.release.signal() }
            var time: TimeInterval = 0
            var expire: (() -> Void)?
            let lifecycle = ApplicationLifecycle(clock: .init(now: { time }, schedule: { _, body in expire = body; return {} }),
                gate: StoreShutdownGate())
            lifecycle.register(.init(name: .modelDownload, start: { _, done in store.beginShutdown(completion: done) }))
            let before = ProcessInfo.processInfo.systemUptime
            if shutdown { lifecycle.begin(.should) } else { store.cancel() }
            let elapsed = ProcessInfo.processInfo.systemUptime - before
            print("stalled publication: phase=\(phase) shutdown=\(shutdown) main_return_ms=\(elapsed * 1000)")
            fflush(stdout)
            check(elapsed < 2, "main waited beyond the global shutdown bound")
            if shutdown {
                // The worker is still stopped. Advance only the manual
                // lifecycle clock: expiry must finish without waiting for it.
                time = 2; expire?()
                check(lifecycle.result?.deadlineReached == true)
                check(lifecycle.result?.unfinished == [.modelDownload])
            }
            if phase == "publication_claimed" {
                wait { store.state == .completed }
                check(try Data(contentsOf: destination(support)) == bytes)
            } else {
                wait { store.state == .cancelled }
                check(!fm.fileExists(atPath: destination(support).path))
            }
        }
    }
}
