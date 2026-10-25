import Darwin
import Foundation
import AppKit

private func json(_ object: Any) -> Data {
    try! JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
}
private func object(_ bytes: Data) -> [String: Any] {
    try! JSONSerialization.jsonObject(with: bytes) as! [String: Any]
}
private let seedTSV = Data("opaque seed payload\0\u{fffd}".utf8)
private let seedManifest = Data("opaque seed metadata, never decoded by Swift".utf8)

/// A named-file Core double. It deliberately returns non-TSV/non-JSON file
/// bytes, proving that the Store only implements transport and IO. The real
/// codec, layers, revisions and conflicts are covered by Rust's Postal tests.
private final class FixtureCore {
    var view: [String: Any] = [
        "schema_version": 1, "management_revision": 0, "access": "editable",
        "layer_status": "empty", "reason": NSNull(), "manifest": NSNull(), "generation": NSNull(),
    ]
    var old: [String: Any] = [:]
    var next: [String: Any] = [:]
    var rollbackView: [String: Any] = [:]
    var disabledView: [String: Any] = [:]
    var ticket: [String: Any] = [:]
    var calls: [String: Int] = [:]
    var statuses: [String: [Int32]] = [:]
    var lastFinish: String?
    var published = false
    var loadingSeed = false
    var loadGeneration: String?
    var operation: UInt64 = 0
    var nextReason: String?
    var began: (() -> Void)?
    var didPublish: (() -> Void)?
    var didFinish: ((String) -> Void)?
    var revision: UInt64 { (view["management_revision"] as! NSNumber).uint64Value }
    var bridge: PostalCodeCoreBridge {
        PostalCodeCoreBridge(
            view: { json(self.view) }, loadBegin: { self.loadBegin($0, $1) },
            loadSupply: { self.loadSupply($0, $1) }, prepare: { self.prepare($0, $1) },
            publish: { self.ack("publish", $0) }, rollback: { self.ack("rollback", $0) },
            disable: { self.ack("disable", $0) }, finish: { self.ack("finish", $0) },
            cancel: { self.ack("cancel", $0) })
    }
    private func count(_ operation: String) { calls[operation, default: 0] += 1 }
    private func status(_ operation: String) -> Int32 {
        count(operation)
        return statuses[operation]?.isEmpty == false ? statuses[operation]!.removeFirst() : 0
    }
    private func reserve() {
        operation += 1
        old = view
        ticket = [
            "domain": "postal_code", "owner": 1, "operation": operation,
            "expected_revision": revision,
        ]
    }
    private func metadata() -> [String: Any] {
        [
            "schema_version": 1, "source_url": "https://source.invalid/",
            "source_page_url": "https://page.invalid/", "source_last_modified": "test",
            "retrieved_at": "2026-10-16T00:00:00Z", "zip_sha256": String(repeating: "1", count: 64),
            "csv_sha256": String(repeating: "2", count: 64),
            "snapshot_sha256": FileSafety.sha256(seedTSV), "entry_count": 1,
        ]
    }
    private func request(_ kind: String, id: Int) -> PostalCodeCoreBridge.Reply {
        (
            Data(), Data(),
            json([
                "schema_version": 1, "state": "needs_file", "ticket": ticket,
                "request": [
                    "request_id": id, "kind": kind,
                    "generation": loadGeneration as Any? ?? NSNull(),
                ], "ready": NSNull(),
            ])
        )
    }
    private func ready(
        _ kind: String, tsv: Data, manifest: Data, generation: String?, error: String? = nil
    ) -> PostalCodeCoreBridge.Reply {
        next = old
        next["management_revision"] = revision + 1
        next["generation"] = generation as Any? ?? NSNull()
        next["manifest"] = error == nil ? metadata() : NSNull()
        next["access"] = error == nil ? "editable" : "read_only"
        next["reason"] = (error ?? nextReason) as Any? ?? NSNull()
        next["layer_status"] =
            error == nil ? (nextReason == nil ? "available" : "empty_degraded") : "empty"
        rollbackView = old
        rollbackView["management_revision"] = revision + 2
        disabledView = rollbackView
        disabledView["reason"] = "runtime_unavailable"
        disabledView["layer_status"] = "empty_degraded"
        let p: [String: Any] = [
            "kind": kind, "published_revision": revision + 1, "recovery_revision": revision + 2,
            "generation": generation as Any? ?? NSNull(), "previous_generation": old["generation"]!,
            "current_bytes_base64": generation.map { Data(($0 + "\n").utf8).base64EncodedString() }
                as Any? ?? NSNull(), "next_view": next, "rollback_view": rollbackView,
            "disabled_view": disabledView,
        ]
        return (
            tsv, manifest,
            json([
                "schema_version": 1, "state": "ready", "ticket": ticket, "request": NSNull(),
                "ready": p,
            ])
        )
    }
    private func loadBegin(_ command: Data, _ data: Data) -> PostalCodeCoreBridge.Reply {
        count("load_begin")
        began?()
        reserve()
        let c = object(command)
        if c["current_state"] as? String == "missing" {
            loadingSeed = true
            loadGeneration = nil
            return request("seed_manifest", id: 1)
        }
        if c["current_state"] as? String == "present",
            let s = String(data: data, encoding: .utf8)?.trimmingCharacters(
                in: .whitespacesAndNewlines), s.count == 64,
            s.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) })
        {
            loadingSeed = false
            loadGeneration = s
            return request("generation_manifest", id: 1)
        }
        return ready(
            "loaded", tsv: Data(), manifest: Data(), generation: nil, error: "invalid_current")
    }
    private func loadSupply(_ command: Data, _ data: Data) -> PostalCodeCoreBridge.Reply {
        count("load_supply")
        let c = object(command)
        guard c["file_state"] as? String == "present" else {
            return ready(
                "loaded", tsv: Data(), manifest: Data(), generation: loadGeneration,
                error: "postal_file_missing")
        }
        if (c["request_id"] as! NSNumber).intValue == 1 {
            return request(loadingSeed ? "seed_tsv" : "generation_tsv", id: 2)
        }
        return ready(
            loadingSeed ? "install_seed" : "loaded", tsv: loadingSeed ? seedTSV : Data(),
            manifest: loadingSeed ? seedManifest : Data(),
            generation: loadingSeed ? FileSafety.sha256(seedManifest) : loadGeneration)
    }
    private func prepare(_ command: Data, _ zip: Data) -> PostalCodeCoreBridge.Reply {
        count("prepare")
        reserve()
        let seconds = (object(command)["retrieved_unix_seconds"] as! NSNumber).int64Value
        let m = Data("opaque new metadata \(seconds)".utf8)
        return ready(
            "update", tsv: Data("opaque new payload".utf8), manifest: m,
            generation: FileSafety.sha256(m))
    }
    private func ack(_ operation: String, _ bytes: Data) -> Int32 {
        let s = status(operation)
        if s != 0 { return s }
        switch operation {
        case "publish":
            view = next
            published = true
            didPublish?()
        case "rollback":
            view = rollbackView
            published = false
        case "disable":
            view = disabledView
            published = false
        case "finish":
            lastFinish = object(bytes)["outcome"] as? String
            didFinish?(lastFinish!)
            published = false
        default: break
        }
        return 0
    }
}

@main private enum PostalCodeTests {
    static func main() throws {
        // A load supply fails locally while the store holds the first reply's
        // ticket. The helper's one cancel is the only cancel, for every ACK
        // including 0, the original error reaches the screen, and only a
        // failed ACK stops updates.
        for status: Int32 in [6, 9, 42, 0] {
            let base = try temporary(); defer { try? FileManager.default.removeItem(at: base) }
            let core = FixtureCore()
            var localCancels = 0, storeCancels = 0
            var bridge = core.bridge
            let nativeCancel = bridge.cancel
            bridge.cancel = { storeCancels += 1; return nativeCancel($0) }
            bridge.loadSupply = { command, _ in
                let command = object(command)
                localCancels += 1
                let ack = nativeCancel(json(command["ticket"]!))
                throw DictionaryPreparationFailure(original: PostalCodeOperationError(status: 4, code: "invalid_plan"), cancellationStatus: ack)
            }
            core.statuses["cancel"] = [status]
            let value = try store(base, core, bridgeOverride: bridge)
            let result = run(value, start: true)
            if case .failure(let error) = result { precondition((error as? PostalCodeOperationError)?.code == "invalid_plan") }
            else { preconditionFailure("local supply must fail") }
            precondition(localCancels == 1 && storeCancels == 0 && core.calls["cancel"] == 1,
                "Postal load supply must not cancel the same ticket twice")
            precondition(value.updatesStopped == (status != 0))
        }
        for status: Int32 in [6, 7, 9, 42] {
            let base = try temporary(); defer { try? FileManager.default.removeItem(at: base) }
            let core = FixtureCore()
            let original = PostalCodeOperationError(status: 4, code: "invalid_plan")
            var prepares = 0
            var bridge = core.bridge
            bridge.prepare = { _, _ in prepares += 1; throw DictionaryPreparationFailure(original: original, cancellationStatus: status) }
            let value = try store(base, core, bridgeOverride: bridge)
            precondition(succeeded(run(value, start: true)))
            let cancels = core.calls["cancel", default: 0]
            let result = run(value, start: false, probe: { _ in precondition(value.updatesStopped, "local cancellation stops Postal before completion") })
            if case .failure(let error) = result { precondition((error as? PostalCodeOperationError)?.code == original.code) }
            else { preconditionFailure("local prepare must fail") }
            precondition(value.updatesStopped, "failed local prepare cancellation stops Postal")
            _ = run(value, start: false)
            precondition(prepares == 1 && core.calls["cancel", default: 0] == cancels, "no second prepare or duplicate cancel")
        }
        let overflow = PostalCodeOperationError(status: 6, code: "revision_overflow")
        let previous = PostalCodeOperationError(status: 6, code: "core_operation_failed")
        precondition(overflow.code == "revision_overflow" && overflow.localizedDescription == previous.localizedDescription)
        precondition(DictionaryAcknowledgement(overflow.status) == .refused
            && DictionaryAcknowledgement(overflow.status).canRetryRecovery)
        try unavailablePublicationIsARefusalWithoutStoppingTheStore()
        try shutdownExcludesPreviouslyCompletedFailure()
        try shutdownRetainsUnacknowledgedSeedWarning()
        try shutdownCancelsUnpublishedSeed()
        try shutdownFinishesWithoutMainAndIgnoresLateDownload()
        try startupIsExplicitOnceAndOffMainWithoutADeadlineJudgingResponsiveness()
        try opaqueSeedACKPrecedesDiskAndRestartDoesNotWrite()
        try missingEmptyUnreadableAndUnsafePathsNeverBecomeSeedFallbacks()
        try everyPreCommitFailureRollsBackAndPreservesDisk()
        try pointerWarningIsNotRolledBackAndSurvivesNextPrivateFailure()
        try recoveryOrderIsRollbackTwoForwardOneDisableTwo()
        try conflictsAndFinishFailuresNeverForwardOrCollect()
        try orphanReuseMustSyncBothFilesAndDirectories()
        try corruptFixedSeedGenerationExplainsFailureAndExplicitUpdateRecovers()
        try garbageCollectionLeavesUnknownAndSymlinkGenerations()
        try cleanupFailureStopsOnlyNewUpdates()
        try localDecodeAndPublicationRefusalLeaveDiskUntouched()
        try finalDisableFailuresAreFatalOnce()
        try completionKeepsBusyAndDurableNewSaveClearsForwardNotice()
        try degradedExistingLoadNeverWritesAndExplicitUpdateCanRepair()
        try updateFailureWindowReportsTheActualLayer()
        print("postal-code tests passed (21 groups; opaque bytes, ACK/recovery/IO/GC/UI)")
    }
    // The window's failure text follows the layer that remains. A download
    // failure and a refused publish leave the layer available, so the text
    // says it was kept (the download case also names the cause). A failed save
    // whose rollback is refused ends in disable: the layer is empty, the text
    // asks for a restart, and it must not claim the data was kept. The three
    // extra publish cases make the final cancel fail (INTERNAL, CORE_INVALID,
    // BUSY): updates then stop, and the reloaded text asks for a restart
    // and no longer claims the data was kept.
    private static func updateFailureWindowReportsTheActualLayer() throws {
        _ = NSApplication.shared
        let failures: [(String, Int32?)] = [
            ("disabled", nil), ("download", nil), ("publish", nil),
            ("publish", 6), ("publish", 7), ("publish", 9),
        ]
        for (failure, cancelStatus) in failures {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let core = FixtureCore()
            var armed = false
            var download: PostalCodeStore.Download?
            if failure == "download" {
                download = { _, completion in completion(.failure(PostalCodeStoreError.downloadFailed)) }
            }
            let s = try store(base, core, injection: { point in
                if armed && failure == "disabled" && point == "before_current_rename" {
                    throw PostalCodeStoreError.downloadFailed
                }
            }, download: download)
            precondition(succeeded(run(s, start: true)))
            armed = true
            if failure == "publish" { core.statuses["publish"] = [6] }
            if let cancelStatus { core.statuses["cancel"] = [cancelStatus] }
            if failure == "disabled" { core.statuses["rollback"] = [6, 6] }
            let window = PostalCodeWindowController(store: s)
            window.startUpdateForTesting()
            let deadline = Date().addingTimeInterval(180)
            while s.isUpdating || window.presentedErrorForTesting == nil {
                precondition(Date() < deadline, "Postal window completion hung")
                RunLoop.current.run(until: Date().addingTimeInterval(0.005))
            }
            let text = window.statusTextForTesting
            if cancelStatus != nil {
                precondition(s.updatesStopped && !s.canUpdate)
                precondition(!text.contains("維持しています"), "failed final cancel must not claim the layer was preserved")
                precondition(text.contains("再起動"), "final cancel failure needs restart guidance")
            } else if failure == "disabled" {
                precondition(s.view?.layerStatus == "empty_degraded" && core.calls["disable"] == 1)
                precondition(!text.contains("維持しています"), "disabled Postal window must not claim the layer was preserved")
                precondition(text.contains("一時停止") && text.contains("再起動"))
            } else {
                precondition(s.view?.layerStatus == "available" && text.contains("維持しています"))
                if failure == "download" { precondition(text.contains("取得できません")) }
            }
        }
    }

    // The stub's ACK and a real bridge's CORE_INVALID differ only in whether the
    // store stops: the stub's refusal leaves updates running, the 7 stops them.
    // Both make exactly one publish call, create no current file and request
    // neither rollback nor disable, so neither failure authorizes a recovery write.
    static func unavailablePublicationIsARefusalWithoutStoppingTheStore() throws {
        for useUnavailable in [true, false] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let core = FixtureCore()
            var bridge = core.bridge
            var calls = 0
            bridge.publish = { ticket in
                calls += 1
                return useUnavailable ? PostalCodeCoreBridge.unavailable.publish(ticket) : 7
            }
            let value = try store(base, core, bridgeOverride: bridge)
            precondition(!succeeded(run(value, start: true)))
            fputs("unavailable publication: stub=\(useUnavailable) stopped=\(value.updatesStopped) calls=\(calls)\n", stderr)
            precondition(calls == 1 && value.updatesStopped == !useUnavailable,
                         "only actual CORE_INVALID is ambiguous; the unsent stub is a refusal")
            precondition(!FileManager.default.fileExists(atPath: value.currentURL.path))
            precondition(core.calls["rollback"] == nil && core.calls["disable"] == nil,
                         "neither failed publication authorizes recovery writes")
        }
    }

    // Fixtures live under the working directory's target/ rather than $TMPDIR, so the
    // store's symlink and path-safety checks see a real path with no /var -> /private/var alias.
    private static func temporary() throws -> URL {
        let p = URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true)
            .appendingPathComponent(
                "target/swift-postal-fixtures/\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: p, withIntermediateDirectories: true)
        return p
    }
    // The clock is fixed, and the double derives the generation name from it, so every
    // update through one store prepares the same generation. Tests rely on that to reach
    // the "existing generation" (orphan reuse) branch on a second update.
    private static func store(
        _ base: URL, _ core: FixtureCore, seed: Bool = true,
        injection: ((String) throws -> Void)? = nil, fatal: @escaping () -> Void = {},
        seconds: Int64 = 2, bridgeOverride: PostalCodeCoreBridge? = nil, download: PostalCodeStore.Download? = nil
    ) throws -> PostalCodeStore {
        let t = base.appendingPathComponent("seed.tsv")
        let m = base.appendingPathComponent("seed.json")
        if seed {
            try seedTSV.write(to: t)
            try seedManifest.write(to: m)
        }
        return PostalCodeStore(
            baseDirectory: base, bundleIdentifier: "fixture", seedDataURL: seed ? t : nil,
            seedManifestURL: seed ? m : nil, bridge: bridgeOverride ?? core.bridge,
            injection: injection,
            fatalRuntimeRecovery: fatal,
            downloadOverride: download ?? { url, completion in
                completion(
                    .success(
                        (
                            Data("self-written ZIP stand-in for the Core double".utf8),
                            HTTPURLResponse(
                                url: url, statusCode: 200, httpVersion: nil,
                                headerFields: ["Content-Type": "application/zip"])!
                        )))
            }, clock: { seconds })
    }
    @discardableResult private static func run(
        _ store: PostalCodeStore, start: Bool = false,
        probe: @escaping (Result<Void, Error>) -> Void = { _ in }
    )
        -> Result<Void, Error>
    {
        var result: Result<Void, Error>?
        if start {
            store.start {
                result = $0
                probe($0)
            }
        } else {
            store.updateFromJapanPost {
                result = $0
                probe($0)
            }
        }
        // Minutes are solely a hang guard. No elapsed-time threshold decides
        // responsiveness or correctness; completion and released busy do.
        let deadline = Date().addingTimeInterval(180)
        while result == nil || store.isUpdating {
            precondition(Date() < deadline, "Postal completion hung")
            RunLoop.current.run(until: Date().addingTimeInterval(0.005))
        }
        return result!
    }
    private static func succeeded(_ result: Result<Void, Error>) -> Bool {
        (try? result.get()) != nil
    }
    // Regular files only, found with lstat: a symlink is never followed, so comparing two
    // trees also proves nothing was written through one.
    private static func tree(_ root: URL) -> [String: Data] {
        var result: [String: Data] = [:]
        guard let e = FileManager.default.enumerator(at: root, includingPropertiesForKeys: nil)
        else { return result }
        for case let url as URL in e {
            var st = stat()
            if lstat(url.path, &st) == 0, st.st_mode & S_IFMT == S_IFREG {
                result[String(url.path.dropFirst(root.path.count))] = try! Data(contentsOf: url)
            }
        }
        return result
    }
    // The injection points that mean the store is about to change disk. A test that
    // counts these proves a path is read-only without hooking the filesystem.
    private static func isWrite(_ point: String) -> Bool {
        point.hasPrefix("write_") || point.hasPrefix("close_") || point.hasPrefix("create_")
            || point.contains("rename") || point.contains("fsync") || point == "gc"
    }
    // Shutdown reports only its own outcome. An update that failed and was delivered
    // before shutdown must not turn the shutdown result into failed, and shutdown makes
    // no further Core calls. The UI error from that update stays.
    static func shutdownExcludesPreviouslyCompletedFailure() throws {
        let base = try temporary(); defer { try? FileManager.default.removeItem(at: base) }
        let core = FixtureCore()
        var armed = false
        let s = try store(base, core, injection: {
            if armed && $0 == "before_current_rename" { throw PostalCodeStoreError.downloadFailed }
        })
        precondition(succeeded(run(s, start: true)))
        armed = true
        precondition(!succeeded(run(s)), "ordinary update must fail")
        let error = s.loadError, before = core.calls
        precondition(error != nil)
        MainActor.assumeIsolated {
            var logs: [String] = []
            let lifecycle = ApplicationLifecycle(gate: StoreShutdownGate(), log: LifecycleLog(sink: { logs.append($0) }))
            lifecycle.register(.init(name: .postal, start: { _, done in s.beginShutdown(completion: done) }))
            lifecycle.begin(.will); lifecycle.waitForCompletion()
            precondition(lifecycle.result?.results[.postal] == .settled)
            precondition(logs.contains { $0.contains("event=shutdown_finished ") && $0.contains("failed=0") })
        }
        precondition(core.calls == before && s.loadError != nil, "prior UI fact remains without contaminating shutdown")
    }

    // A seed install whose pointer rename left a durability warning, and that shutdown
    // overtakes before the main-thread delivery, still counts as failed: the warning was
    // never shown. The new pointer is kept, and no screen callback runs after shutdown.
    static func shutdownRetainsUnacknowledgedSeedWarning() throws {
        let base = try temporary(); defer { try? FileManager.default.removeItem(at: base) }
        let core = FixtureCore(), finished = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
        core.didFinish = { _ in finished.signal() }
        let s = try store(base, core, injection: { if $0 == "after_current_rename" { throw CocoaError(.fileWriteUnknown) } })
        var screens = 0
        s.start { _ in screens += 1 }
        precondition(finished.wait(timeout: .now() + 180) == .success)
        s.beginShutdown { result in precondition(result == .failed); done.signal() }
        precondition(done.wait(timeout: .now() + 180) == .success && screens == 0)
        precondition(FileManager.default.fileExists(atPath: s.currentURL.path) && core.lastFinish == "new_pointer_warning")
    }

    // Shutdown during the startup load, before publication, cancels the reservation
    // and never publishes or writes the seed. The negative is a seed that lands on disk
    // because shutdown let the stopped load continue.
    static func shutdownCancelsUnpublishedSeed() throws {
        let base = try temporary(); defer { try? FileManager.default.removeItem(at: base) }
        let core = FixtureCore(), entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
        core.began = { entered.signal(); precondition(release.wait(timeout: .now() + 180) == .success) }
        let s = try store(base, core)
        var screens = 0
        s.start { _ in screens += 1 }
        precondition(entered.wait(timeout: .now() + 180) == .success)
        s.beginShutdown { result in precondition(result == .settled); done.signal() }
        release.signal()
        precondition(done.wait(timeout: .now() + 180) == .success)
        precondition(core.calls["publish", default: 0] == 0 && core.calls["cancel"] == 1 && screens == 0)
        precondition(!FileManager.default.fileExists(atPath: s.currentURL.path), "unpublished seed never saves")
    }

    // Shutdown completes on a background thread, so it cannot deadlock with a blocked
    // main thread, and cancels exactly once. The second half: a download that answers
    // after shutdown must not reach prepare or a screen callback.
    static func shutdownFinishesWithoutMainAndIgnoresLateDownload() throws {
        let base = try temporary(); defer { try? FileManager.default.removeItem(at: base) }
        let core = FixtureCore(), finished = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
        core.didFinish = { _ in finished.signal() }
        let s = try store(base, core)
        var screenCalls = 0
        s.start { _ in screenCalls += 1 }
        precondition(finished.wait(timeout: .now() + 180) == .success)
        s.beginShutdown { result in precondition(!Thread.isMainThread && result == .settled); done.signal() }
        precondition(done.wait(timeout: .now() + 180) == .success)
        precondition(core.calls["cancel"] == 1 && !s.canUpdate)
        var tick = false; DispatchQueue.main.async { tick = true }
        let end = Date().addingTimeInterval(180)
        while !tick { precondition(Date() < end); RunLoop.current.run(until: Date().addingTimeInterval(0.005)) }
        precondition(screenCalls == 0 && core.calls["cancel"] == 1)
        let lateBase = try temporary(); defer { try? FileManager.default.removeItem(at: lateBase) }
        let lateCore = FixtureCore()
        var late: ((Result<(Data, HTTPURLResponse), Error>) -> Void)?
        let lateStore = try store(lateBase, lateCore, download: { _, completion in late = completion })
        precondition(succeeded(run(lateStore, start: true)))
        var updateCallbacks = 0
        lateStore.updateFromJapanPost { _ in updateCallbacks += 1 }
        precondition(late != nil)
        let closed = DispatchSemaphore(value: 0)
        lateStore.beginShutdown { _ in closed.signal() }
        precondition(closed.wait(timeout: .now() + 180) == .success)
        let prepares = lateCore.calls["prepare", default: 0]
        late?(.failure(CocoaError(.fileReadUnknown)))
        precondition(lateCore.calls["prepare", default: 0] == prepares && updateCallbacks == 0,
            "late network result cannot prepare or deliver a screen result")
    }

    // Constructing the store does nothing; start() begins exactly one load off main, and
    // repeated start() calls while it runs are ignored. An update during loading is
    // refused. Responsiveness is shown by main staying free while Core is held, not by
    // measuring time.
    static func startupIsExplicitOnceAndOffMainWithoutADeadlineJudgingResponsiveness() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let core = FixtureCore()
        let entered = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        core.began = {
            precondition(!Thread.isMainThread)
            entered.signal()
            precondition(release.wait(timeout: .now() + 180) == .success)
        }
        let s = try store(base, core)
        precondition(core.calls.isEmpty && s.startupState == .notStarted && !s.canUpdate)
        var completed = false
        s.start { _ in
            precondition(Thread.isMainThread)
            completed = true
        }
        precondition(entered.wait(timeout: .now() + 180) == .success)
        precondition(s.startupState == .loading && !s.canUpdate)
        var refused = false
        s.updateFromJapanPost { if case .failure = $0 { refused = true } }
        precondition(refused)
        // Main remains free while the load is intentionally stopped in Core.
        for _ in 0..<100 { s.start() }
        precondition(!completed)
        release.signal()
        let deadline = Date().addingTimeInterval(180)
        while !completed || s.isUpdating {
            precondition(Date() < deadline)
            RunLoop.current.run(until: Date().addingTimeInterval(0.005))
        }
        precondition(core.calls["load_begin"] == 1 && s.startupState == .ready && s.canUpdate)
    }
    // Ordering of a seed install: Core publishes before the pointer exists, and finish
    // "new_pointer_durable" comes after it. The seed bytes are stored unchanged. A second
    // launch loads that generation with no write at all.
    static func opaqueSeedACKPrecedesDiskAndRestartDoesNotWrite() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let c = FixtureCore()
        let s = try store(base, c)
        c.didPublish = { precondition(!FileManager.default.fileExists(atPath: s.currentURL.path)) }
        c.didFinish = { outcome in
            precondition(outcome == "new_pointer_durable")
            precondition(FileManager.default.fileExists(atPath: s.currentURL.path))
        }
        precondition(succeeded(run(s, start: true)))
        let before = tree(s.rootURL)
        let gen = String(decoding: try Data(contentsOf: s.currentURL), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        precondition(
            try! Data(
                contentsOf: s.generationsURL.appendingPathComponent(gen).appendingPathComponent(
                    "postal-code.tsv")) == seedTSV)
        let secondCore = FixtureCore()
        var points: [String] = []
        let second = try store(base, secondCore, injection: { points.append($0) })
        precondition(succeeded(run(second, start: true)))
        precondition(tree(s.rootURL) == before)
        precondition(!points.contains(where: isWrite))
        precondition(secondCore.lastFinish == "loaded")
    }
    // Only a missing pointer installs the seed. An empty, invalid or symlinked pointer is
    // an error with no writes, and Core is never asked for seed files (load_supply 0).
    // With no bundled seed there is one supply call, which reports the file missing. A
    // symlinked pointer or base directory is never written through, even by an update.
    static func missingEmptyUnreadableAndUnsafePathsNeverBecomeSeedFallbacks() throws {
        for kind in ["empty", "bad", "symlink", "missing_seed"] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let c = FixtureCore()
            var writes = 0
            let s = try store(
                base, c, seed: kind != "missing_seed", injection: { if isWrite($0) { writes += 1 } }
            )
            if kind != "missing_seed" {
                try FileManager.default.createDirectory(
                    at: s.rootURL, withIntermediateDirectories: true)
                if kind == "symlink" {
                    let outside = base.appendingPathComponent("outside")
                    try Data("protected".utf8).write(to: outside)
                    try FileManager.default.createSymbolicLink(
                        at: s.currentURL, withDestinationURL: outside)
                } else {
                    try (kind == "empty" ? Data() : Data("bad".utf8)).write(to: s.currentURL)
                }
            }
            run(s, start: true)
            precondition(writes == 0)
            precondition(s.loadError != nil)
            precondition(c.calls["load_supply", default: 0] == (kind == "missing_seed" ? 1 : 0))
            if kind == "symlink" {
                precondition(!succeeded(run(s)))
                precondition(
                    try! Data(contentsOf: base.appendingPathComponent("outside"))
                        == Data("protected".utf8))
            }
        }
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let real = base.appendingPathComponent("real")
        try FileManager.default.createDirectory(at: real, withIntermediateDirectories: false)
        let alias = base.appendingPathComponent("alias")
        try FileManager.default.createSymbolicLink(at: alias, withDestinationURL: real)
        let c = FixtureCore()
        let s = try store(alias, c)
        run(s, start: true)
        precondition(s.loadError != nil)
        precondition(tree(real).keys.allSatisfy { !$0.contains("postal_code") })
    }
    // Each IO checkpoint before the pointer rename, when it fails, gives exactly one
    // rollback and finish "rolled_back", and leaves the old pointer byte for byte. A
    // checkpoint that the loop does not reach would fail the `seen` check, so the list
    // cannot go stale silently.
    static func everyPreCommitFailureRollsBackAndPreservesDisk() throws {
        for point in [
            "create_directory", "directory_fsync", "write_tsv", "tsv_fsync", "close_tsv",
            "write_manifest", "manifest_fsync", "close_manifest", "staging_fsync",
            "before_generation_rename", "after_generation_rename", "generations_fsync",
            "before_current_rename", "current_open_temporary", "current_write_temporary",
            "current_fsync_temporary", "current_close_temporary",
        ] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let c = FixtureCore()
            var armed = false
            var seen = false
            let s = try store(
                base, c,
                injection: { p in
                    if armed && p == point {
                        seen = true
                        throw PostalCodeStoreError.downloadFailed
                    }
                })
            precondition(succeeded(run(s, start: true)))
            let current = try Data(contentsOf: s.currentURL)
            armed = true
            // Existing directories skip mkdir; remove the empty generations
            // parent only for this checkpoint while preserving old current.
            if point == "create_directory" {
                try FileManager.default.removeItem(at: s.generationsURL)
            }
            precondition(!succeeded(run(s)))
            precondition(seen, point)
            precondition(try! Data(contentsOf: s.currentURL) == current)
            precondition(c.calls["rollback"] == 1)
            precondition(c.lastFinish == "rolled_back")
            precondition(!s.isUpdating)
        }
    }
    // A failure after the pointer rename (the directory fsync) is a warning: the update
    // stands, nothing is rolled back, and finish says "new_pointer_warning". The warning
    // then survives a later update that fails before the pointer (publish refused). Only
    // the next durable save clears it. This update reuses the generation, so the
    // before_generation_rename injection is not reached; the publish status is what fails it.
    static func pointerWarningIsNotRolledBackAndSurvivesNextPrivateFailure() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let c = FixtureCore()
        var stage = 0
        let s = try store(
            base, c,
            injection: { p in
                if stage == 1 && p == "current_fsync_directory" {
                    throw PostalCodeStoreError.downloadFailed
                }
                if stage == 2 && p == "before_generation_rename" {
                    throw PostalCodeStoreError.invalidSnapshot
                }
            })
        precondition(succeeded(run(s, start: true)))
        stage = 1
        c.nextReason = "invalid_postal_code_layer"
        precondition(succeeded(run(s)))
        precondition(s.durabilityWarning != nil)
        precondition(s.statusMessages.count == 2)
        precondition(c.calls["rollback", default: 0] == 0)
        precondition(c.lastFinish == "new_pointer_warning")
        stage = 2
        c.nextReason = nil
        // Fail before the pointer even when this update reuses its generation.
        c.statuses["publish"] = [6]
        precondition(!succeeded(run(s)))
        precondition(s.durabilityWarning != nil)
        stage = 0
        precondition(succeeded(run(s)))
        precondition(s.durabilityWarning == nil)
    }
    // After a pre-pointer failure with two refused rollbacks (status 6, INTERNAL), the
    // store makes exactly one forward save. If that succeeds, the update stands with a
    // recovery notice. If it fails, disable is tried, and the second attempt succeeds:
    // the layer is disabled with no fatal exit.
    static func recoveryOrderIsRollbackTwoForwardOneDisableTwo() throws {
        for disabled in [false, true] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let c = FixtureCore()
            var armed = false
            var failures = 0
            var fatals = 0
            let s = try store(
                base, c,
                injection: { p in
                    if armed && p == "before_current_rename" {
                        failures += 1
                        if disabled || failures == 1 { throw PostalCodeStoreError.downloadFailed }
                    }
                }, fatal: { fatals += 1 })
            precondition(succeeded(run(s, start: true)))
            armed = true
            c.statuses["rollback"] = [6, 6]
            if disabled { c.statuses["disable"] = [6, 0] }
            let result = run(s)
            precondition(c.calls["rollback"] == 2)
            precondition(failures == 2)
            precondition(fatals == 0)
            if disabled {
                precondition(!succeeded(result))
                precondition(c.calls["disable"] == 2)
                precondition(c.lastFinish == "disabled")
                precondition(s.loadError != nil)
            } else {
                precondition(succeeded(result))
                precondition(c.calls["disable", default: 0] == 0)
                precondition(s.recoveryNotice != nil)
            }
        }
    }
    // A REVISION_MISMATCH (8) on rollback, or a failed finish, stops recovery: no forward
    // save (the generation is written once), no GC, one fatal exit, and no further updates.
    static func conflictsAndFinishFailuresNeverForwardOrCollect() throws {
        for operation in ["rollback", "finish"] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let c = FixtureCore()
            var armed = false
            var gc = 0
            var saved = 0
            var fatals = 0
            let s = try store(
                base, c,
                injection: { p in
                    if armed {
                        if p == "gc" { gc += 1 }
                        if p == "before_generation_rename" { saved += 1 }
                        if operation == "rollback" && p == "before_current_rename" {
                            throw PostalCodeStoreError.downloadFailed
                        }
                    }
                }, fatal: { fatals += 1 })
            precondition(succeeded(run(s, start: true)))
            armed = true
            c.statuses[operation] = [8]
            precondition(!succeeded(run(s)))
            precondition(saved == 1)
            precondition(gc == 0)
            precondition(fatals == 1)
            precondition(!s.canUpdate)
        }
    }
    // A generation left by a failed update (renamed in, then the parent fsync failed) is
    // reused by the next update, and every fsync on that reused generation is required: a
    // failure at any of the four rolls back without GC. A fresh store must fsync all four
    // again rather than trust that the earlier process did.
    static func orphanReuseMustSyncBothFilesAndDirectories() throws {
        for point in [
            "existing_tsv_fsync", "existing_manifest_fsync", "existing_generation_fsync",
            "existing_generations_fsync",
        ] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let c = FixtureCore()
            var armed = false
            var first = true
            var points: [String] = []
            let s = try store(
                base, c,
                injection: { p in
                    if armed {
                        points.append(p)
                        if first && p == "generations_fsync" {
                            first = false
                            throw PostalCodeStoreError.downloadFailed
                        }
                        if !first && p == point { throw PostalCodeStoreError.downloadFailed }
                    }
                })
            precondition(succeeded(run(s, start: true)))
            armed = true
            precondition(!succeeded(run(s)))
            points = []
            precondition(!succeeded(run(s)))
            precondition(points.contains(point))
            precondition(!points.contains("gc"))
            precondition(c.lastFinish == "rolled_back")
            // A fresh host/Core retry on the identical orphan must re-fsync it.
            let nextCore = FixtureCore()
            var synced: [String] = []
            let next = try store(base, nextCore, injection: { synced.append($0) })
            precondition(succeeded(run(next, start: true)))
            synced = []
            precondition(succeeded(run(next)))
            precondition(
                synced.contains("existing_tsv_fsync") && synced.contains("existing_manifest_fsync")
                    && synced.contains("existing_generation_fsync")
                    && synced.contains("existing_generations_fsync"))
            precondition(nextCore.lastFinish == "new_pointer_durable")
        }
    }
    // A same-name seed generation with a corrupt manifest is never replaced at startup,
    // not even on a second launch: no writes, a visible message, no pointer. An explicit
    // update writes a different generation and recovers.
    static func corruptFixedSeedGenerationExplainsFailureAndExplicitUpdateRecovers() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let root = base.appendingPathComponent(
            "fixture/postal_code/v1/generations/" + FileSafety.sha256(seedManifest))
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try seedTSV.write(to: root.appendingPathComponent("postal-code.tsv"))
        try Data("interrupted".utf8).write(to: root.appendingPathComponent("manifest.json"))
        for _ in 0..<2 {
            let c = FixtureCore()
            var writes = 0
            let s = try store(base, c, injection: { if isWrite($0) { writes += 1 } })
            precondition(!succeeded(run(s, start: true)))
            precondition(writes == 0)
            precondition(!s.statusMessages.isEmpty)
            precondition(!FileManager.default.fileExists(atPath: s.currentURL.path))
        }
        let c = FixtureCore()
        let s = try store(base, c)
        run(s, start: true)
        precondition(succeeded(run(s)))
        precondition(s.loadError == nil)
        precondition(
            try! Data(contentsOf: s.currentURL)
                != Data((FileSafety.sha256(seedManifest) + "\n").utf8))
    }
    // GC removes only hash-named directories that hold exactly the expected files. A
    // directory with other contents, and a symlink with its target, are left alone.
    static func garbageCollectionLeavesUnknownAndSymlinkGenerations() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let c = FixtureCore()
        let s = try store(base, c)
        precondition(succeeded(run(s, start: true)))
        let unknown = s.generationsURL.appendingPathComponent(String(repeating: "a", count: 64))
        try FileManager.default.createDirectory(at: unknown, withIntermediateDirectories: false)
        try Data("keep".utf8).write(to: unknown.appendingPathComponent("unknown"))
        let external = base.appendingPathComponent("external")
        try FileManager.default.createDirectory(at: external, withIntermediateDirectories: false)
        try Data("keep".utf8).write(to: external.appendingPathComponent("postal-code.tsv"))
        let link = s.generationsURL.appendingPathComponent(String(repeating: "b", count: 64))
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: external)
        precondition(succeeded(run(s)))
        precondition(FileManager.default.fileExists(atPath: unknown.path))
        precondition(
            try! Data(contentsOf: external.appendingPathComponent("postal-code.tsv"))
                == Data("keep".utf8))
        precondition((try? FileManager.default.destinationOfSymbolicLink(atPath: link.path)) != nil)
    }
    // If the final cancel fails (INTERNAL or BUSY), the update already succeeded and its
    // data stays live. Only new updates stop, and the stop is checked before prepare. The
    // message asks for a restart.
    static func cleanupFailureStopsOnlyNewUpdates() throws {
        for status: Int32 in [6, 9] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let c = FixtureCore()
            let s = try store(base, c)
            precondition(succeeded(run(s, start: true)))
            c.statuses["cancel"] = [status]
            precondition(succeeded(run(s)))
            precondition(!s.canUpdate)
            precondition(s.statusMessages.joined().contains("再起動"))
            let count = c.calls["prepare"]!
            precondition(!succeeded(run(s)))
            precondition(c.calls["prepare"] == count)
            precondition(c.view["layer_status"] as? String == "available")
        }
    }
    // A plan Swift cannot decode, and a publish refused for a moved revision, both end
    // before any disk change. In the conflict case the store adopts Core's current view
    // (revision 99) instead of keeping its stale one. The cancel count proves the
    // reservation was released.
    static func localDecodeAndPublicationRefusalLeaveDiskUntouched() throws {
        // A publish ACK of CORE_INVALID or an unknown value (42, -1) leaves the
        // disk as it was and stops updates, with no rollback attempted.
        for status: Int32 in [7, 42, -1] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let core = FixtureCore()
            let s = try store(base, core)
            precondition(succeeded(run(s, start: true)))
            let before = tree(s.rootURL)
            core.statuses["publish"] = [status]
            precondition(!succeeded(run(s)))
            precondition(tree(s.rootURL) == before)
            precondition(!s.canUpdate, "ambiguous publication ACK stops updates")
            precondition(core.calls["finish"] == 1 && core.calls["rollback"] == nil)
        }
        for mode in ["decode", "publish"] {
            let base = try temporary()
            defer { try? FileManager.default.removeItem(at: base) }
            let core = FixtureCore()
            var armed = false
            var bridge = core.bridge
            let original = bridge.prepare
            bridge.prepare = { command, bytes in
                let reply = try original(command, bytes)
                guard armed else { return reply }
                if mode == "decode" {
                    var p = object(reply.plan)
                    var ready = p["ready"] as! [String: Any]
                    var view = ready["next_view"] as! [String: Any]
                    view.removeValue(forKey: "layer_status")
                    ready["next_view"] = view
                    p["ready"] = ready
                    return (reply.tsv, reply.manifest, json(p))
                }
                core.view["management_revision"] = 99
                core.statuses["publish"] = [8]
                return reply
            }
            let s = try store(base, core, bridgeOverride: bridge)
            precondition(succeeded(run(s, start: true)))
            let before = tree(s.rootURL)
            armed = true
            precondition(!succeeded(run(s)))
            precondition(tree(s.rootURL) == before)
            precondition(core.calls["finish"] == 1)
            precondition(core.calls["cancel"] == 2)
            precondition(core.calls["publish"] == (mode == "decode" ? 1 : 2))
            if mode == "publish" { precondition(s.view?.managementRevision == 99) }
        }
    }
    // When rollback, the forward save and disable all fail, the process exit is requested
    // exactly once, and the old pointer remains as the authority for the next launch.
    static func finalDisableFailuresAreFatalOnce() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let core = FixtureCore()
        var armed = false
        var attempts = 0
        var fatals = 0
        let s = try store(
            base, core,
            injection: { point in
                if armed && point == "before_current_rename" {
                    attempts += 1
                    throw PostalCodeStoreError.downloadFailed
                }
            }, fatal: { fatals += 1 })
        precondition(succeeded(run(s, start: true)))
        let before = try Data(contentsOf: s.currentURL)
        armed = true
        core.statuses["rollback"] = [6, 6]
        core.statuses["disable"] = [6, 6]
        precondition(!succeeded(run(s)))
        precondition(attempts == 2)
        precondition(core.calls["rollback"] == 2 && core.calls["disable"] == 2 && fatals == 1)
        precondition(try! Data(contentsOf: s.currentURL) == before)
        precondition(!s.canUpdate)
    }
    // The store stays busy while the completion callback runs, so an update started from
    // inside it is rejected and does not prepare. The forward-save notice from that
    // recovery is cleared by the next durable save.
    static func completionKeepsBusyAndDurableNewSaveClearsForwardNotice() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let core = FixtureCore()
        var armed = false
        var attempts = 0
        let s = try store(
            base, core,
            injection: { point in
                if armed && point == "before_current_rename" {
                    attempts += 1
                    if attempts == 1 { throw PostalCodeStoreError.downloadFailed }
                }
            })
        precondition(succeeded(run(s, start: true)))
        armed = true
        core.statuses["rollback"] = [6, 6]
        var nestedRejected = false
        precondition(
            succeeded(
                run(
                    s,
                    probe: { _ in
                        precondition(s.isUpdating && !s.canUpdate)
                        s.updateFromJapanPost { nestedRejected = !succeeded($0) }
                    })))
        precondition(nestedRejected)
        precondition(core.calls["prepare"] == 1)
        precondition(s.recoveryNotice != nil)
        precondition(succeeded(run(s)))
        precondition(s.recoveryNotice == nil)
    }
    // Loading an existing generation that Core marks degraded writes nothing and leaves
    // updates allowed. An explicit update can then repair it.
    static func degradedExistingLoadNeverWritesAndExplicitUpdateCanRepair() throws {
        let base = try temporary()
        defer { try? FileManager.default.removeItem(at: base) }
        let initial = FixtureCore()
        let first = try store(base, initial)
        precondition(succeeded(run(first, start: true)))
        let core = FixtureCore()
        core.nextReason = "invalid_postal_code_layer"
        var writes = 0
        let s = try store(base, core, injection: { if isWrite($0) { writes += 1 } })
        precondition(succeeded(run(s, start: true)))
        precondition(writes == 0)
        precondition(s.loadError != nil && s.canUpdate)
        precondition(s.view?.layerStatus == "empty_degraded")
        core.nextReason = nil
        precondition(succeeded(run(s)))
        precondition(s.loadError == nil)
    }

}
