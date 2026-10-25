import Foundation

/// Manual reservations and semaphore barriers assert ordering, not elapsed
/// responsiveness. Long wait bounds below detect hangs on a loaded host.
private final class LifecycleSaveScheduler {
    private let lock = NSLock()
    private var jobs: [() -> Void] = []
    var count: Int { lock.withLock { jobs.count } }
    func schedule(_ delay: TimeInterval, _ work: @escaping () -> Void) {
        lock.withLock { jobs.append(work) }
    }
    func fireAll() {
        let batch = lock.withLock { let batch = jobs; jobs = []; return batch }
        for work in batch { work() }
    }
}

@MainActor enum LifecycleTests {
    static func run() throws {
        let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent("target/swift-lifecycle-fixtures/\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        try learningFinalReceipt(root)
        try learningActiveWrite(root)
        try learningFailureAndDeadline(root)
        try learningStoppedAndCleared(root)
        processShutdown()
        willWaitsForConcurrentExpiry()
        try recorderShutdown(root)
    }
    private static func initial(_ generation: UInt64 = 0, unverifiable: Bool = false,
                                invalid: Bool = false) -> CoreInitializationResult {
        .init(degraded: .init(personalFrequencyInvalid: invalid,
            personalFrequencyUnverifiable: unverifiable, userDictionaryInvalid: false),
            personalFrequencyVersion: .init(generation: generation, revision: 0))
    }
    private static func wait(_ semaphore: DispatchSemaphore) {
        precondition(semaphore.wait(timeout: .now() + 180) == .success, "shutdown hang guard")
    }
    private static func close(_ store: PersonalFrequencyStore, deadline: TimeInterval = 2,
                              now: @escaping () -> TimeInterval = { 0 }) -> ShutdownWorkResult {
        let done = DispatchSemaphore(value: 0)
        var result: ShutdownWorkResult?
        store.beginShutdown(deadline: deadline, now: now) { result = $0; done.signal() }
        wait(done)
        return result!
    }
    // Shutdown asks Core once for the newest learning, writes it off main, and
    // reports settled; a second shutdown repeats nothing, and clear is closed.
    private static func learningFinalReceipt(_ root: URL) throws {
        let scheduler = LifecycleSaveScheduler()
        var writes: [Data] = [], queries = 0
        let r1 = LearningVersion(generation: 0, revision: 1), r2 = LearningVersion(generation: 0, revision: 2)
        let store = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "final-receipt",
            scheduler: scheduler.schedule, persistenceOverride: { data, _ in
                precondition(!Thread.isMainThread); writes.append(data); return nil
            }, query: { cursor in
                precondition(!Thread.isMainThread && cursor == r1)
                queries += 1; return .success((Data([0xff, 2]), r2))
            })
        store.acceptStartup(initial())
        store.receive(Data([0xff, 1]), version: r1)
        precondition(scheduler.count == 1)
        precondition(close(store) == .settled)
        precondition(close(store) == .settled && queries == 1 && store.shutdownQueryCount == 1)
        precondition(writes == [Data([0xff, 2])] && store.savedVersion == r2)
        store.receive(Data([0xff, 3]), version: .init(generation: 0, revision: 3)); store.poll()
        scheduler.fireAll(); try store.flushForTesting()
        precondition(queries == 1 && writes.count == 1 && store.receivedVersion == r2,
            "ordinary receipt and stale reservations are closed")
        guard case .precommitFailure = store.clear() else { preconditionFailure("clear is closed") }
    }
    // A shutdown that arrives while a save is running waits for it, then
    // queries and writes the newer version, all without main pumping callbacks;
    // a duplicate shutdown request settles too.
    private static func learningActiveWrite(_ root: URL) throws {
        let scheduler = LifecycleSaveScheduler(), entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        var writes: [Data] = [], events: [String] = []
        let store = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "active-write",
            scheduler: scheduler.schedule, persistenceOverride: { data, _ in
                writes.append(data); events.append("write-\(data[0])")
                if data == Data([1]) { entered.signal(); wait(release) }
                return nil
            }, query: { cursor in
                precondition(cursor == .init(generation: 0, revision: 1))
                events.append("query"); return .success((Data([2]), .init(generation: 0, revision: 2)))
            })
        store.acceptStartup(initial()); store.receive(Data([1]), version: .init(generation: 0, revision: 1))
        scheduler.fireAll(); wait(entered)
        let done = DispatchSemaphore(value: 0)
        store.beginShutdown(deadline: 2, now: { 0 }) { result in precondition(result == .settled); done.signal() }
        let duplicate = DispatchSemaphore(value: 0)
        store.beginShutdown(deadline: 99, now: { 0 }) { result in precondition(result == .settled); duplicate.signal() }
        // Main deliberately does not pump any screen callback.
        release.signal(); wait(done); wait(duplicate)
        precondition(writes == [Data([1]), Data([2])] && events == ["write-1", "query", "write-2"])
        precondition(store.savedVersion == .init(generation: 0, revision: 2) && !store.persistenceStatus.pending)
        scheduler.fireAll(); try store.flushForTesting(); precondition(writes.count == 2)
    }
    // Failed queries or writes end as failed with the status kept (still pending,
    // last good save recorded); an already received version is still written;
    // nothing new is queried or written once the deadline has passed.
    private static func learningFailureAndDeadline(_ root: URL) throws {
        for queryFails in [true, false] {
            let scheduler = LifecycleSaveScheduler()
            var writes = 0, time: TimeInterval = 2_000
            let store = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "failure-\(queryFails)",
                scheduler: scheduler.schedule, persistenceOverride: { _, _ in
                    writes += 1
                    if writes == 2 { throw CocoaError(.fileWriteUnknown) }
                    return nil
                }, query: { _ in queryFails ? .failure(CocoaError(.fileReadUnknown)) : .success(nil) },
                clock: { Date(timeIntervalSince1970: time) })
            store.acceptStartup(initial()); store.receive(Data([1]), version: .init(generation: 0, revision: 1))
            scheduler.fireAll(); try store.flushForTesting()
            time = 3_000
            store.receive(Data([2]), version: .init(generation: 0, revision: 2))
            precondition(close(store) == .failed)
            precondition(store.persistenceStatus.queryFailed == queryFails && store.persistenceStatus.saveIssue == .writeFailed)
            precondition(store.persistenceStatus.lastSavedAt == .completedSave(Date(timeIntervalSince1970: 2_000)))
            precondition(store.persistenceStatus.pending && store.savedVersion == .init(generation: 0, revision: 1))
            scheduler.fireAll(); try store.flushForTesting(); precondition(writes == 2 && scheduler.count == 0)
        }
        // A failed last query still saves an already received correct pending.
        var knownWrites = 0
        let known = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "known-pending",
            scheduler: { _, _ in }, persistenceOverride: { _, _ in knownWrites += 1; return nil },
            query: { _ in .failure(CocoaError(.fileReadUnknown)) })
        known.acceptStartup(initial()); known.receive(Data([7]), version: .init(generation: 0, revision: 7))
        precondition(close(known) == .failed && knownWrites == 1 && !known.persistenceStatus.pending)

        let warning = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "durability-warning",
            scheduler: { _, _ in }, persistenceOverride: { _, _ in CocoaError(.fileWriteUnknown) }, query: { _ in .success(nil) })
        warning.acceptStartup(initial()); warning.receive(Data([3]), version: .init(generation: 0, revision: 3))
        precondition(close(warning) == .failed && warning.savedVersion == .init(generation: 0, revision: 3))
        precondition(warning.persistenceStatus.saveIssue == .durabilityWarning && !warning.persistenceStatus.pending,
            "rename succeeded but shutdown must retain the durability warning")

        for expiredDuringQuery in [false, true] {
            var monotonic: TimeInterval = expiredDuringQuery ? 0 : 2
            var queries = 0, writes = 0
            let store = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "deadline-\(expiredDuringQuery)",
                scheduler: { _, _ in }, persistenceOverride: { _, _ in writes += 1; return nil },
                query: { _ in queries += 1; monotonic = 2; return .success((Data([9]), .init(generation: 0, revision: 9))) })
            store.acceptStartup(initial()); store.receive(Data([1]), version: .init(generation: 0, revision: 1))
            precondition(close(store, now: { monotonic }) == .failed)
            precondition(queries == (expiredDuringQuery ? 1 : 0) && writes == 0, "no new query/write after deadline")
        }
    }
    // A stopped store does no IO at shutdown, and a cleared store saves the
    // cleared generation rather than old learning.
    private static func learningStoppedAndCleared(_ root: URL) throws {
        for reason in ["uninitialized", "unverifiable", "quarantine", "clear-rejected"] {
            var queries = 0, writes = 0, deletes = 0
            let store = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: reason,
                scheduler: { _, _ in }, persistenceOverride: { _, _ in writes += 1; return nil },
                deletionOverride: { _ in deletes += 1 }, quarantineOverride: { _ in throw CocoaError(.fileWriteUnknown) },
                coreClear: { .failure(CocoaError(.fileReadUnknown)) }, query: { _ in queries += 1; return .success(nil) })
            if reason != "uninitialized" { store.acceptStartup(initial(unverifiable: reason == "unverifiable", invalid: reason == "quarantine")) }
            if reason == "clear-rejected" { _ = store.clear() }
            let beforeDeletes = deletes
            precondition(close(store) == .settled && queries == 0 && writes == 0)
            _ = store.clear(); precondition(deletes == beforeDeletes)
        }
        let scheduler = LifecycleSaveScheduler()
        var cursors: [LearningVersion?] = [], writes: [Data] = []
        let cleared = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "cleared",
            scheduler: scheduler.schedule, persistenceOverride: { data, _ in writes.append(data); return nil },
            deletionOverride: { _ in }, coreClear: { .success(.init(generation: 1, revision: 0)) },
            query: { cursor in cursors.append(cursor); return .success(nil) })
        cleared.acceptStartup(initial()); cleared.receive(Data([1]), version: .init(generation: 0, revision: 1))
        guard case .cleared = cleared.clear() else { preconditionFailure("clear") }
        cleared.receive(Data([0]), version: .init(generation: 0, revision: 99))
        precondition(close(cleared) == .settled)
        scheduler.fireAll(); try cleared.flushForTesting()
        precondition(cursors == [.init(generation: 1, revision: 0)] && writes.isEmpty && cleared.persistenceStatus.fileState == .cleared)
    }
}

private final class LifecycleManualClock {
    final class Job {
        let at: TimeInterval; let body: () -> Void; var cancelled = false
        init(_ at: TimeInterval, _ body: @escaping () -> Void) { self.at = at; self.body = body }
    }
    var time: TimeInterval = 0
    var jobs: [Job] = []
    var history: [Job] = []
    var clock: ApplicationClock {
        .init(now: { self.time }, schedule: { at, body in
            let job = Job(at, body); self.jobs.append(job); self.history.append(job)
            return { job.cancelled = true }
        })
    }
    func advance(_ delta: TimeInterval) {
        time += delta
        while let index = jobs.firstIndex(where: { !$0.cancelled && $0.at <= time }) {
            jobs.remove(at: index).body()
        }
    }
}

extension LifecycleTests {
    // will-terminate waits for an expiry hook already running on another
    // thread, but only for a bounded grace: a hook that finishes in time
    // yields shutdown_finished, one that does not yields exactly one
    // shutdown_expiry_wait_timeout and no result.
    private static func willWaitsForConcurrentExpiry() {
        for finishes in [true, false] {
            let clock = LifecycleManualClock(), state = NSLock()
            var logs: [String] = [], hookFinished = false
            let lifecycle = ApplicationLifecycle(clock: clock.clock, gate: StoreShutdownGate(),
                log: LifecycleLog(sink: { logs.append($0) }))
            let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
            let releaseRequested = DispatchSemaphore(value: 0), timerFinished = DispatchSemaphore(value: 0)
            lifecycle.register(.init(name: .recorder, start: { _, _ in }, expire: {
                entered.signal()
                precondition(release.wait(timeout: .now() + 180) == .success, "expiry hook hang guard")
                state.withLock { hookFinished = true }
            }))
            lifecycle.begin(.will)
            clock.time = 10
            let timer = clock.history[0].body
            DispatchQueue.global().async { timer(); timerFinished.signal() }
            wait(entered)
            if finishes {
                // A separate worker releases the hook only after wait reaches
                // its second stage. No sleep decides whether the race passed.
                DispatchQueue.global().async {
                    precondition(releaseRequested.wait(timeout: .now() + 180) == .success)
                    release.signal()
                }
            }
            var waits = 0
            lifecycle.waitForCompletion { group, deadline in
                waits += 1
                if waits == 1 { return group.wait(timeout: deadline) }
                precondition(waits == 2 && deadline <= .now() + .milliseconds(100), "expiry grace is bounded")
                if !finishes { return .timedOut }
                releaseRequested.signal()
                // The production bound was checked above. A long hang guard
                // keeps a heavily loaded test host from deciding correctness.
                return group.wait(timeout: .now() + 180)
            }
            precondition(waits == 2)
            if finishes {
                precondition(lifecycle.result != nil && state.withLock { hookFinished })
                precondition(logs.contains { $0.contains("event=shutdown_finished ") })
                precondition(!logs.contains { $0.contains("event=shutdown_expiry_wait_timeout ") })
            } else {
                precondition(lifecycle.result == nil && !state.withLock { hookFinished })
                precondition(logs.filter { $0.contains("event=shutdown_expiry_wait_timeout ") }.count == 1)
                precondition(!logs.contains { $0.contains("event=shutdown_finished ") })
                release.signal()
            }
            wait(timerFinished)
            lifecycle.waitForCompletion()
            precondition(logs.filter { $0.contains("event=shutdown_finished ") }.count == 1)
        }
    }

    // Without a termination notification nothing starts. After one, every
    // participant gets the same 2 s deadline, expiry marks the unfinished ones
    // once, a late completion cannot rewrite the result, AppKit gets one
    // reply, and the log stays bounded and free of paths and text.
    static func processShutdown() {
        // No notification: no store is touched and input remains open.
        let clock = LifecycleManualClock(), gate = StoreShutdownGate()
        var logs: [String] = [], starts = 0, expires = 0
        var callbacks: [(ShutdownWorkResult) -> Void] = []
        let lifecycle = ApplicationLifecycle(clock: clock.clock, gate: gate, log: LifecycleLog(sink: { logs.append($0) }))
        for name: LifecycleParticipant in [.learning, .recorder] {
            lifecycle.register(.init(name: name, start: { deadline, done in
                precondition(deadline == 2); starts += 1; callbacks.append(done)
            }, expire: { expires += 1 }))
        }
        precondition(!lifecycle.isClosing && !gate.isClosing && starts == 0)
        lifecycle.begin(.should)
        var replies = 0
        lifecycle.whenFinished { result in
            precondition(result.deadlineReached && result.unfinished.count == 2)
            replies += 1
        }
        clock.advance(1); lifecycle.begin(.should); lifecycle.begin(.will)
        precondition(lifecycle.deadline == 2 && lifecycle.result == nil && starts == 2 && gate.isClosing)
        clock.advance(1)
        precondition(expires == 2 && lifecycle.result?.deadlineReached == true)
        // Simulate will-only's bounded wait without pumping main.
        lifecycle.waitForCompletion()
        callbacks.forEach { $0(.settled) }; clock.history.forEach { $0.body() }
        precondition(lifecycle.result?.unfinished.count == 2 && expires == 2, "late completion cannot rewrite the terminal result")
        let guardEnd = Date().addingTimeInterval(180)
        while replies == 0 { precondition(Date() < guardEnd); RunLoop.current.run(until: Date().addingTimeInterval(0.002)) }
        precondition(replies == 1 && logs.count <= 64)
        var bounded: [String] = []
        let log = LifecycleLog(sink: { bounded.append($0) })
        for name in LifecycleParticipant.allCases {
            for phase in StartupWorkerPhase.allCases { log.startupWorker(name, phase: phase) }
            for state: DataStartupState in [.notStarted, .loading, .ready, .readOnly, .failed] { log.startup(name, state) }
            log.participant(name); log.participant(name, result: .settled); log.participant(name, result: .failed)
        }
        precondition(bounded.count == 64 && bounded.allSatisfy { !$0.contains("path=") && !$0.contains("text=") })

        let earlyClock = LifecycleManualClock(), earlyGate = StoreShutdownGate()
        let early = ApplicationLifecycle(clock: earlyClock.clock, gate: earlyGate, log: LifecycleLog(sink: { _ in }))
        var finish: ((ShutdownWorkResult) -> Void)?
        early.register(.init(name: .user, start: { _, done in finish = done }))
        early.begin(.will)
        let worker = DispatchSemaphore(value: 0)
        DispatchQueue.global().async { finish?(.failed); worker.signal() }
        wait(worker); early.waitForCompletion()
        precondition(early.result?.deadlineReached == false && early.result?.results[.user] == .failed)
        early.begin(.should); earlyClock.advance(10)
        precondition(early.result?.deadlineReached == false, "a later signal cannot renew an already settled shutdown")
    }
}

extension LifecycleTests {
    // A drained recorder settles with nothing pending. A blocked write fails at
    // the deadline, keeps its record and debt for reporting, and the deadline
    // never starts a new report.
    private static func recorderShutdown(_ root: URL) throws {
        let clock = CommitStreamTestClock()
        let recorder = CommitStreamRecorder(processConfiguration: .init(processNonce: "shutdown-fixture", sourceCommit: "fixture", build: 1),
            directoryURL: root.appendingPathComponent("records"), clock: clock.clock,
            reportDropped: { _ in preconditionFailure("success must return no debt") })
        recorder.append(Data("{\"fixture\":1}".utf8), weight: 7)
        let done = DispatchSemaphore(value: 0)
        recorder.beginShutdown(deadline: 2) { result in precondition(result == .settled); done.signal() }
        wait(done)
        precondition(recorder.status.pendingCount == 0 && recorder.status.discarded == 0)

        let blockedClock = CommitStreamTestClock(), entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let blocked = CommitStreamRecorder(processConfiguration: .init(processNonce: "blocked-fixture", sourceCommit: "fixture", build: 1),
            directoryURL: root.appendingPathComponent("blocked-records"), clock: blockedClock.clock,
            reportDropped: { _ in preconditionFailure("deadline cannot begin a new report") }, checkpoint: { phase in
                if phase == "locked" { entered.signal(); wait(release) }
            })
        blocked.append(Data("{\"fixture\":1}".utf8), weight: 4)
        wait(entered)
        blocked.append(Data("{\"fixture\":2}".utf8), weight: 7)
        let ended = DispatchSemaphore(value: 0)
        blocked.beginShutdown(deadline: 2) { result in precondition(result == .failed); ended.signal() }
        blockedClock.advance(2); blocked.expireShutdown()
        precondition(blocked.status.pendingCount == 1 && blocked.status.discarded == 1 && blocked.status.reportPending,
            "active IO is unknown; waiting record moved once to loss outbox")
        release.signal(); wait(ended)
        precondition(blocked.status.pendingCount == 0 && blocked.status.discarded == 1 && blocked.status.reportPending)
        blocked.expireShutdown(); precondition(blocked.status.discarded == 1)
    }
}
