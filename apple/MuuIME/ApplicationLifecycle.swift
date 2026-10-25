import Foundation
import OSLog

/// Host-owned startup progress, independent of Core's management access and
/// effective layer. A readable, degraded layer can still finish as ready;
/// an IO failure before a view exists must not be called a corrupt file.
enum DataStartupState: String, Sendable {
    case notStarted, loading, ready, readOnly, failed

    var finished: Bool { self != .notStarted && self != .loading }

    static func completed(access: String?, failed: Bool) -> Self {
        if failed { return .failed }
        return access == "read_only" ? .readOnly : .ready
    }
}

enum ShutdownWorkResult: String, Sendable { case settled, failed }
struct StoreShutdownCancellation: Error {}

/// Admission and the shutdown-only result share a short lock, so an ordinary
/// completion cannot race the start of shutdown into its result. Closing never
/// waits for IO or calls Core, UI, or a completion under this lock.
final class StoreShutdownGate {
    private let lock = NSLock()
    private var closing = false
    private var result: ShutdownWorkResult?
    var isClosing: Bool { lock.withLock { closing } }
    var shutdownResult: ShutdownWorkResult { lock.withLock { result ?? .settled } }
    func close() {
        lock.withLock {
            guard !closing else { return }
            result = nil; closing = true
        }
    }
    // Imported/Postal record from their completion lease. A lease whose
    // screen was acknowledged before closing, but whose cleanup cancel was
    // still queued, therefore still counts. User clears its pending result
    // at acknowledgement instead.
    func recordShutdownResult(_ value: ShutdownWorkResult) {
        lock.withLock {
            guard closing, result != .failed else { return }
            result = value
        }
    }
    func checkOpen() throws { if isClosing { throw StoreShutdownCancellation() } }
}

/// Owned and consumed on one store's IO queue. Normally main acknowledges its
/// screen completion first. At shutdown the queue consumes this same lease
/// without waiting for main; a late screen acknowledgement cannot cancel twice.
final class IOCompletionLease {
    private var body: (() -> Void)?
    init(_ body: @escaping () -> Void) { self.body = body }
    func finish() {
        let action = body
        body = nil
        action?()
    }
}

/// Monotonic deadlines and asynchronous scheduling share one time domain.
/// schedule never runs inline, including when its deadline has already passed.
struct ApplicationClock {
    var now: () -> TimeInterval
    var schedule: (TimeInterval, @escaping () -> Void) -> (() -> Void)
    static let live = ApplicationClock(now: { ProcessInfo.processInfo.systemUptime }, schedule: { deadline, body in
        let timer = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
        timer.setEventHandler { timer.setEventHandler(handler: nil); timer.cancel(); body() }
        timer.schedule(deadline: .now() + max(0, deadline - ProcessInfo.processInfo.systemUptime))
        timer.resume()
        return { timer.setEventHandler(handler: nil); timer.cancel() }
    })
}

/// Only armed for closing. Several short-lock state changes coalesce into one
/// asynchronous inspection; no inspection or Core call runs under those locks.
final class ShutdownProgressSignal {
    private let lock = NSLock()
    private let clock: ApplicationClock
    private var queued = false
    private var observer: (() -> Void)?
    init(clock: ApplicationClock, observer: @escaping () -> Void) {
        self.clock = clock; self.observer = observer
    }
    func pulse() {
        let shouldQueue = lock.withLock { () -> Bool in
            guard observer != nil, !queued else { return false }
            queued = true; return true
        }
        guard shouldQueue else { return }
        _ = clock.schedule(clock.now()) { [weak self] in
            guard let self else { return }
            let action = self.lock.withLock { self.queued = false; return self.observer }
            action?()
        }
    }
    func cancel() { lock.withLock { observer = nil } }
}

enum LifecycleParticipant: String, CaseIterable, Sendable { case learning, user, imported, postal, recorder, modelDownload }
enum ShutdownTrigger: String { case should, will }
enum StartupWorkerPhase: String, CaseIterable { case begin, prepared, published, finished, cancelled, failed }
struct ShutdownSummary {
    let deadlineReached: Bool
    let results: [LifecycleParticipant: ShutdownWorkResult]
    let unfinished: Set<LifecycleParticipant>
}

/// A worker-side completion ledger, independent of AppKit and main's run loop.
/// Expiry hooks may change short-lock ownership, but must never perform IO.
private final class ShutdownRun {
    let deadline: TimeInterval
    private let lock = NSLock()
    private let group = DispatchGroup()
    private var pending: Set<LifecycleParticipant>
    private var results: [LifecycleParticipant: ShutdownWorkResult] = [:]
    private var summary: ShutdownSummary?
    private var observer: ((ShutdownSummary) -> Void)?
    // Holds the summary back until `armed`, so a participant that completes
    // synchronously inside `begin` cannot end the run before the rest start.
    private var starting = true
    private var expiring = false
    private let expiry: [() -> Void]
    init(deadline: TimeInterval, participants: Set<LifecycleParticipant>, expiry: [() -> Void]) {
        self.deadline = deadline; pending = participants; self.expiry = expiry; group.enter()
    }
    var result: ShutdownSummary? { lock.withLock { summary } }
    func observe(_ action: @escaping (ShutdownSummary) -> Void) {
        let ready = lock.withLock { () -> ShutdownSummary? in
            if let summary { return summary }
            precondition(observer == nil); observer = action; return nil
        }
        if let ready { action(ready) }
    }
    func complete(_ name: LifecycleParticipant, _ result: ShutdownWorkResult) {
        lock.lock()
        guard summary == nil, pending.remove(name) != nil else { lock.unlock(); return }
        results[name] = result
        finishLocked(deadlineReached: false)
    }
    func armed() { lock.lock(); starting = false; finishLocked(deadlineReached: false) }
    func expire() {
        lock.lock()
        guard summary == nil, !expiring else { lock.unlock(); return }
        expiring = true; lock.unlock()
        // In particular, move waiting recorder records to its loss outbox
        // before permitting AppKit to exit. Active IO is never guessed lost.
        for action in expiry { action() }
        // Completions that arrived while the hooks ran are kept in `results`,
        // but the summary still reports that the deadline was reached.
        lock.lock(); expiring = false; starting = false
        finishLocked(deadlineReached: true)
    }
    private func finishLocked(deadlineReached: Bool) {
        guard summary == nil, !starting, !expiring, deadlineReached || pending.isEmpty else { lock.unlock(); return }
        let value = ShutdownSummary(deadlineReached: deadlineReached, results: results, unfinished: pending)
        summary = value; let callback = observer; observer = nil; lock.unlock()
        group.leave(); callback?(value)
    }
    // Only will-terminate waits here. A timer may already own the short-lock
    // expiry hooks; allow at most 100 ms for that final ownership transfer and
    // summary, without extending the IO deadline or changing should's reply.
    func wait(now: () -> TimeInterval,
              wait: (DispatchGroup, DispatchTime) -> DispatchTimeoutResult) -> Bool {
        if wait(group, .now() + max(0, deadline - now())) == .success { return true }
        expire()
        return wait(group, .now() + .milliseconds(100)) == .success
    }
}

/// This facade owns admission and AppKit-facing callbacks on main. A shutdown
/// never constructs a previously unused store, repeats a query, or renews its
/// deadline. Durability remains the individual stores' ordinary save contract.
@MainActor final class ApplicationLifecycle {
    static let inputGate = StoreShutdownGate()
    struct Participant {
        let name: LifecycleParticipant
        let start: (TimeInterval, @escaping (ShutdownWorkResult) -> Void) -> Void
        var expire: () -> Void = {}
    }
    private let clock: ApplicationClock
    private let gate: StoreShutdownGate
    private let log: LifecycleLog
    private var participants: [Participant] = []
    private var run: ShutdownRun?
    private var cancelTimer: (() -> Void)?
    init(clock: ApplicationClock = .live, gate: StoreShutdownGate? = nil,
         log: LifecycleLog = .shared) {
        self.clock = clock; self.gate = gate ?? Self.inputGate; self.log = log
    }
    var isClosing: Bool { run != nil }
    var result: ShutdownSummary? { run?.result }
    var deadline: TimeInterval? { run?.deadline }
    func register(_ participant: Participant) {
        precondition(run == nil && !participants.contains { $0.name == participant.name })
        participants.append(participant)
    }
    func begin(_ trigger: ShutdownTrigger) {
        log.signal(trigger)
        // The first signal fixes the one deadline; a later should or will
        // joins this run instead of starting another.
        guard run == nil else { return }
        gate.close()
        let run = ShutdownRun(deadline: clock.now() + 2, participants: Set(participants.map(\.name)), expiry: participants.map(\.expire))
        self.run = run
        log.begin()
        cancelTimer = clock.schedule(run.deadline) { run.expire() }
        for participant in participants {
            log.participant(participant.name)
            participant.start(run.deadline) { [log] result in
                log.participant(participant.name, result: result)
                run.complete(participant.name, result)
            }
        }
        run.armed()
    }
    func whenFinished(_ completion: @escaping (ShutdownSummary) -> Void) {
        precondition(run != nil)
        run!.observe { [weak self] result in
            // AppKit may run a nested termination loop from a main dispatch
            // callback. Another main dispatch block cannot reenter that queue;
            // a main RunLoop block can still deliver the terminateLater reply.
            RunLoop.main.perform(inModes: [.common]) {
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.cancelTimer?(); self.cancelTimer = nil
                    self.log.finished(result)
                    completion(result)
                }
            }
        }
    }
    /// Used only from will-terminate. No RunLoop pumping and no queue.sync:
    /// workers can signal this ledger without delivering any screen callback.
    func waitForCompletion(wait: (DispatchGroup, DispatchTime) -> DispatchTimeoutResult = { $0.wait(timeout: $1) }) {
        if let run, !run.wait(now: clock.now, wait: wait) { log.expiryWaitTimedOut() }
        if let result { log.finished(result) }
        cancelTimer?(); cancelTimer = nil
    }
}

/// Deliberately typed, low-frequency facts only. Never accepts arbitrary errors,
/// file paths, data identifiers, text, or payload hashes. Live capture is needed
/// to establish whether IMK actually delivers termination notifications.
final class LifecycleLog {
    static let shared = LifecycleLog()
    private let lock = NSLock()
    private let logger = Logger(subsystem: "dev.muu.inputmethod.Muu", category: "lifecycle")
    private let process = UUID().uuidString.lowercased()
    // Each key is logged at most once per process, and at most 64 keys in
    // all; later events are dropped without a trace.
    private var emitted: Set<String> = []
    private let sink: ((String) -> Void)?
    init(sink: ((String) -> Void)? = nil) { self.sink = sink }
    private func emit(_ key: String, _ facts: String) {
        guard lock.withLock({ emitted.count < 64 && emitted.insert(key).inserted }) else { return }
        let line = "process=\(process) pid=\(ProcessInfo.processInfo.processIdentifier) \(facts)"
        if let sink { sink(line) } else { logger.notice("\(line, privacy: .public)") }
    }
    func blasThreadLimit(_ value: String?, inherited: Bool) {
        // Only the numeric limit is public. A malformed environment value is
        // preserved for Accelerate but must not inject arbitrary log text.
        let limit = value.flatMap(Int.init).map(String.init) ?? "invalid"
        emit("blas-threads", "event=blas_threads limit=\(limit) inherited=\(inherited)")
    }
    func launched(build: String, source: String) { emit("launch", "event=launch build=\(build) source=\(source)") }
    func startup(_ data: LifecycleParticipant, _ state: DataStartupState) {
        emit("startup-\(data)-\(state)", "event=startup data=\(data.rawValue) state=\(state.rawValue) main=\(Thread.isMainThread)")
    }
    func signal(_ trigger: ShutdownTrigger) { emit("signal-\(trigger)", "event=\(trigger.rawValue)_terminate") }
    func begin() { emit("shutdown", "event=shutdown_begin budget_ms=2000") }
    func participant(_ data: LifecycleParticipant, result: ShutdownWorkResult? = nil) {
        let phase = result?.rawValue ?? "begin"
        emit("shutdown-\(data)-\(phase)", "event=shutdown data=\(data.rawValue) phase=\(phase) main=\(Thread.isMainThread)")
    }
    func startupWorker(_ data: LifecycleParticipant, phase: StartupWorkerPhase) {
        emit("startup-worker-\(data)-\(phase)", "event=startup_worker data=\(data.rawValue) phase=\(phase.rawValue) main=\(Thread.isMainThread)")
    }
    func storeWarning(_ data: LifecycleParticipant) {
        emit("warning-\(data)", "event=shutdown_warning data=\(data.rawValue) durability_warning=true")
    }
    func learningQueries(_ count: Int) { emit("queries", "event=shutdown_learning queries=\(count)") }
    func recorder(pending: Int, discarded: UInt64, reportPending: Bool) {
        emit("recorder", "event=shutdown_recorder pending=\(pending) discarded=\(discarded) report_pending=\(reportPending)")
    }
    func finished(_ result: ShutdownSummary) {
        emit("finished", "event=shutdown_finished deadline=\(result.deadlineReached) unfinished=\(result.unfinished.count) failed=\(result.results.values.filter { $0 == .failed }.count)")
    }
    func expiryWaitTimedOut() { emit("expiry-wait-timeout", "event=shutdown_expiry_wait_timeout limit_ms=100") }
    func replied() { emit("reply", "event=termination_reply") }
}
