import Foundation
import Darwin

// Kept as the recorder's source-compatible name; app shutdown uses the same
// monotonic clock and absolute deadline, never a second two-second window.
typealias CommitStreamClock = ApplicationClock

func commitStreamAdd(_ lhs: UInt64, _ rhs: UInt64) -> UInt64 {
    let (sum, overflow) = lhs.addingReportingOverflow(rhs)
    return overflow ? .max : sum
}

/// This is ABI metadata, never the opaque record's JSON. UInt64 decoding keeps
/// values above 2^53 exact. Raw mismatched metadata can be returned unchanged.
struct CommitStreamDropWeight: Codable, Equatable {
    let schemaVersion: UInt32
    let dropWeight: UInt64
    enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", dropWeight = "drop_weight" }
    init(_ weight: UInt64) { schemaVersion = 1; dropWeight = weight }
    static func decode(_ bytes: Data) throws -> UInt64 {
        guard !bytes.isEmpty, bytes.count <= 1024 else { throw CommitStreamDeliveryFailure.malformed }
        let value = try JSONDecoder().decode(Self.self, from: bytes)
        guard value.schemaVersion == 1, value.dropWeight > 0 else { throw CommitStreamDeliveryFailure.malformed }
        return value.dropWeight
    }
}

enum CommitStreamDeliveryFailure: Error { case malformed, unsafePath, io }

/// A small ACK-only outbox. It never holds record bytes, calls Core under a
/// lock, or retries an outcome that might already have accepted the debt.
final class CommitStreamLossReporter {
    struct Status { let pending: Bool; let unreportable: Bool }
    private let lock = NSLock()
    private let clock: CommitStreamClock
    private let execute: (@escaping () -> Void) -> Void
    private let encode: (UInt64) throws -> Data
    private var report: ((Data) -> Int32)?
    private var pending: UInt64 = 0
    private var inFlight: UInt64 = 0
    private var raw: Data?
    private var inFlightRaw: Data?
    private var flightToken: UUID?
    private var attempts = 0
    private var exhausted = false
    private var stopped = false
    private var running = false
    private var nextAttempt: TimeInterval = 0
    private var closingDeadline: TimeInterval?
    private var reservation: UUID?
    private var cancelTimer: (() -> Void)?
    private var progress: ShutdownProgressSignal?
    func observeShutdown(_ progress: ShutdownProgressSignal) { lock.withLock { self.progress = progress } }
    var shutdownSettled: Bool {
        lock.withLock {
            !running && ((pending == 0 && inFlight == 0 && raw == nil && inFlightRaw == nil) || stopped || exhausted || (closingDeadline.map { clock.now() >= $0 } ?? false))
        }
    }

    init(clock: CommitStreamClock = .live, execute: ((@escaping () -> Void) -> Void)? = nil,
         encode: @escaping (UInt64) throws -> Data = { try JSONEncoder().encode(CommitStreamDropWeight($0)) },
         report: ((Data) -> Int32)?) {
        self.clock = clock; self.encode = encode; self.report = report
        let queue = DispatchQueue(label: "dev.muu.inputmethod.Muu.commit-loss", qos: .utility)
        self.execute = execute ?? { queue.async(execute: DispatchWorkItem(block: $0)) }
    }
    /// The Rust boundary installs this once before draining. Recorder-only
    /// fixtures can inject an ACK function without linking the Core. Until it
    /// is connected, debt stays pending instead of being mistaken for refusal.
    func connect(_ report: @escaping (Data) -> Int32) {
        lock.withLock { if self.report == nil { self.report = report }; armLocked() }
    }
    var status: Status { lock.withLock { .init(pending: pending > 0 || inFlight > 0 || raw != nil || inFlightRaw != nil, unreportable: stopped) } }
    func accept(_ weight: UInt64) {
        lock.withLock {
            pending = commitStreamAdd(pending, weight)
            reactivateLocked()
        }
    }
    // Any new receipt may wake an exhausted aggregate, even a retained record.
    func receipt() { lock.withLock { reactivateLocked() } }
    func acceptUnreadable(_ metadata: Data) {
        lock.withLock {
            // A second packet cannot arrive: the recorder stops draining at
            // the first. Empty or oversized metadata goes straight to
            // unreportable, with no retries.
            guard raw == nil, inFlightRaw == nil, metadata.count <= 1024, !metadata.isEmpty else { stopped = true; return }
            raw = metadata
            reactivateLocked()
        }
    }
    func beginShutdown(deadline: TimeInterval) {
        lock.withLock {
            closingDeadline = min(closingDeadline ?? deadline, deadline)
            cancelTimer?(); cancelTimer = nil; reservation = nil
            armLocked()
        }
    }
    func expireShutdown() {
        lock.withLock {
            guard let deadline = closingDeadline, clock.now() >= deadline else { return }
            cancelTimer?(); cancelTimer = nil; reservation = nil
            // A queued Core call has not started; an already running call has
            // cleared flightToken and remains an unknown outcome until return.
            if flightToken != nil { flightToken = nil; running = false }
            progress?.pulse()
        }
    }
    private func reactivateLocked() {
        if exhausted { exhausted = false; attempts = 0 }
        armLocked()
    }
    private func armLocked() {
        defer { progress?.pulse() }
        guard report != nil, !stopped, !exhausted, !running, reservation == nil,
              pending > 0 || inFlight > 0 || raw != nil || inFlightRaw != nil else { return }
        if let deadline = closingDeadline, clock.now() >= deadline { return }
        let token = UUID(); reservation = token
        let when = max(clock.now(), nextAttempt)
        if let deadline = closingDeadline, when >= deadline { reservation = nil; return }
        cancelTimer = clock.schedule(when) { [weak self] in self?.wake(token) }
    }
    private func wake(_ token: UUID) {
        let start = lock.withLock { () -> Bool in
            defer { progress?.pulse() }
            guard reservation == token, !stopped else { return false }
            reservation = nil; cancelTimer = nil
            if let deadline = closingDeadline, clock.now() >= deadline { return false }
            running = true
            flightToken = token
            if inFlight == 0 && inFlightRaw == nil {
                if let metadata = raw { inFlightRaw = metadata; raw = nil }
                else { inFlight = pending; pending = 0 }
            }
            return true
        }
        guard start else { return }
        execute { [weak self] in self?.send(token) }
    }
    private func send(_ token: UUID) {
        let batch = lock.withLock { () -> (UInt64, Data?)? in
            defer { progress?.pulse() }
            guard flightToken == token else { return nil }
            flightToken = nil
            if let deadline = closingDeadline, clock.now() >= deadline { running = false; return nil }
            return (inFlight, inFlightRaw)
        }
        guard let batch else { return }
        let status: Int32
        guard let report = lock.withLock({ self.report }) else { finish(MUU_RESULT_INTERNAL); return }
        do { status = report(try batch.1 ?? encode(batch.0)) }
        catch { finish(MUU_RESULT_INTERNAL); return } // No Core call: known unaccepted.
        finish(status)
    }
    private func finish(_ status: Int32) {
        lock.withLock {
            running = false
            if status == MUU_RESULT_OK {
                inFlight = 0; inFlightRaw = nil; attempts = 0; nextAttempt = 0
            } else if status == MUU_RESULT_BAD_ARG || status == MUU_RESULT_INTERNAL {
                attempts += 1
                nextAttempt = clock.now() + pow(2, Double(min(attempts - 1, 4)))
                if attempts >= 6 {
                    exhausted = true
                    // Unreadable metadata also stopped the consumer, even
                    // when queued behind this normal batch. There will be no
                    // next receipt to wake it; retain the packet, mark it
                    // unreportable, and require a process restart.
                    if inFlightRaw != nil || raw != nil { stopped = true }
                }
            } else {
                // CORE_INVALID and unknown outcomes may have accepted debt.
                // Retain only the fact and do not infer that resending is safe.
                stopped = true
            }
            armLocked()
        }
    }
}

/// Bounded ownership starts at receipt, before any asynchronous work is
/// enqueued. The one pump and one timer capture only this owner/token, never
/// a payload. Pending includes the active IO, so blocked IO cannot grow memory.
final class CommitStreamDelivery {
    // The same bounds as Rust's queue (docs/src/internals/storage.md「記録」, docs/src/internals/storage.md「記録」). Lower ones here would
    // refuse records Rust has already handed over.
    static let maximumBytes = 4 * 1024 * 1024
    static let maximumRecords = 256
    static func isFramedRecord(_ data: Data) -> Bool {
        guard !data.isEmpty, data.count <= maximumBytes else { return false }
        // Receipt runs on the key path. memchr validates opaque framing in
        // bounded linear time without Swift byte-iterator overhead or a copy.
        return data.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return false }
            return memchr(base, 10, raw.count) == nil && memchr(base, 13, raw.count) == nil
        }
    }
    struct Status {
        let count: Int; let bytes: Int; let retrying: Bool; let stopped: Bool
        let discarded: UInt64; let closing: Bool
    }
    private struct Record {
        let id: UUID; let bytes: Data; let weight: UInt64; let received: TimeInterval
        var attempts = 0
    }
    private let lock = NSLock()
    private let clock: CommitStreamClock
    private let execute: (@escaping () -> Void) -> Void
    private let write: (Data) throws -> Void
    private let reporter: CommitStreamLossReporter
    private let event: (String) -> Void
    private var records: [Record] = []
    private var bytes = 0
    private var discarded: UInt64 = 0
    private var stopped = false
    private var closed = false
    private var shutdownDeadline: TimeInterval?
    private var active: UUID?
    private var reserved: UUID?
    private var cancelTimer: (() -> Void)?
    private var queuedPump: UUID?
    private var failureLevel = 0
    private var nextAttempt: TimeInterval = 0
    private var progress: ShutdownProgressSignal?
    func observeShutdown(_ progress: ShutdownProgressSignal) { lock.withLock { self.progress = progress } }

    #if MUU_TEST
    private var idleWaiters: [DispatchSemaphore] = []
    func waitUntilIdleForTesting() {
        let done = DispatchSemaphore(value: 0)
        lock.withLock {
            idleWaiters.append(done)
            notifyIdleLocked()
        }
        precondition(done.wait(timeout: .now() + 180) == .success, "recorder IO hung")
    }
    private func notifyIdleLocked() {
        if records.isEmpty || stopped || (active == nil && queuedPump == nil && failureLevel > 0 && nextAttempt > clock.now()) {
            idleWaiters.forEach { $0.signal() }; idleWaiters.removeAll()
        }
    }
    #endif
    init(clock: CommitStreamClock = .live, execute: ((@escaping () -> Void) -> Void)? = nil,
         reporter: CommitStreamLossReporter, event: @escaping (String) -> Void = { _ in },
         write: @escaping (Data) throws -> Void) {
        self.clock = clock; self.reporter = reporter; self.write = write; self.event = event
        let queue = DispatchQueue(label: "dev.muu.inputmethod.Muu.commit-delivery", qos: .utility)
        self.execute = execute ?? { queue.async(execute: DispatchWorkItem(block: $0)) }
    }
    // `retrying` is the carried cooldown, not "a record is waiting". It
    // stays set after a final discard until a later write succeeds, even
    // with nothing pending.
    var status: Status {
        lock.withLock { .init(count: records.count, bytes: bytes, retrying: failureLevel > 0,
                             stopped: stopped, discarded: discarded, closing: closed) }
    }
    func append(_ data: Data, weight: UInt64) {
        // No JSON/UTF8/schema interpretation; framing is the entire contract.
        let valid = Self.isFramedRecord(data)
        lock.withLock {
            reporter.receipt()
            guard valid, !stopped, !closed, records.count < Self.maximumRecords,
                  data.count <= Self.maximumBytes - bytes else {
                discardLocked(weight); return
            }
            records.append(.init(id: UUID(), bytes: data, weight: weight, received: clock.now()))
            bytes += data.count
            armLocked()
        }
    }
    func discardUnreadable(_ metadata: Data) {
        lock.withLock {
            discarded = commitStreamAdd(discarded, 1)
            reporter.acceptUnreadable(metadata)
        }
    }
    func beginShutdown() { beginShutdown(deadline: clock.now() + 2) }
    func beginShutdown(deadline: TimeInterval) {
        lock.withLock {
            closed = true; shutdownDeadline = min(shutdownDeadline ?? deadline, deadline)
            cancelLocked(); armLocked()
            reporter.beginShutdown(deadline: shutdownDeadline!)
        }
    }
    func expireShutdown() {
        lock.withLock {
            guard let deadline = shutdownDeadline, clock.now() >= deadline else { return }
            cancelLocked(); expireLocked()
            #if MUU_TEST
            notifyIdleLocked()
            #endif
            progress?.pulse()
        }
    }
    private func expireLocked() {
        // Only records whose OS IO has started remain unknown at the boundary.
        // The active record is always records[0]: it was the head when IO
        // started and nothing is inserted before it.
        let keep = active == nil ? 0 : 1
        while records.count > keep {
            let record = records.removeLast(); bytes -= record.bytes.count
            discardLocked(record.weight)
        }
        queuedPump = nil
    }
    private func discardLocked(_ weight: UInt64) {
        discarded = commitStreamAdd(discarded, 1)
        // Transfer ownership once, under the same short lock as final discard.
        // accept schedules only; no IO/Core or UI call is made here.
        reporter.accept(weight)
        progress?.pulse()
    }
    private func removeLocked(discard: Bool) {
        let record = records.removeFirst(); bytes -= record.bytes.count
        if discard { discardLocked(record.weight) }
    }
    private func cancelLocked() { cancelTimer?(); cancelTimer = nil; reserved = nil }
    private func armLocked() {
        defer { progress?.pulse() }
        #if MUU_TEST
        defer { notifyIdleLocked() }
        #endif
        guard reserved == nil else { return }
        if active != nil || queuedPump != nil {
            // The timer is independent of the writer, including queue age.
            // Only an OS call that has actually started is exempt from expiry.
            let expiry = records.first(where: { $0.id != active }).map { $0.received + 60 } ?? .infinity
            let when = min(expiry, shutdownDeadline ?? .infinity)
            if when.isFinite {
                let token = UUID(); reserved = token
                cancelTimer = clock.schedule(when) { [weak self] in self?.wake(token) }
            }
            return
        }
        guard !records.isEmpty, !stopped else { return }
        let now = clock.now()
        let expiry = records[0].received + 60
        let when = min(max(now, nextAttempt), expiry, shutdownDeadline ?? .infinity)
        let token = UUID(); reserved = token
        cancelTimer = clock.schedule(when) { [weak self] in self?.wake(token) }
    }
    private func wake(_ token: UUID) {
        let start = lock.withLock { () -> Bool in
            defer { progress?.pulse() }
            #if MUU_TEST
            defer { notifyIdleLocked() }
            #endif
            guard reserved == token else { return false }
            reserved = nil; cancelTimer = nil
            let now = clock.now()
            if let deadline = shutdownDeadline, now >= deadline {
                expireLocked()
                return false
            }
            let oldHead = records.first?.id
            for index in records.indices.reversed() where records[index].id != active && now >= records[index].received + 60 {
                let expired = records.remove(at: index); bytes -= expired.bytes.count
                discardLocked(expired.weight)
            }
            if oldHead != records.first?.id { queuedPump = nil }
            guard active == nil, queuedPump == nil, !stopped else { armLocked(); return false }
            guard !records.isEmpty else { return false }
            if now < nextAttempt { armLocked(); return false }
            queuedPump = token
            armLocked()
            return true
        }
        guard start else { return }
        execute { [weak self] in self?.perform(token) }
    }
    private func perform(_ token: UUID) {
        let record = lock.withLock { () -> Record? in
            defer { progress?.pulse() }
            #if MUU_TEST
            defer { notifyIdleLocked() }
            #endif
            guard queuedPump == token else { return nil }
            queuedPump = nil
            if let deadline = shutdownDeadline, clock.now() >= deadline { return nil }
            while let head = records.first, clock.now() >= head.received + 60 { removeLocked(discard: true) }
            guard let head = records.first, !stopped else { return nil }
            active = head.id
            cancelLocked(); armLocked()
            return head
        }
        guard let record else { return }
        let result: Result<Void, Error>
        do { try write(record.bytes); result = .success(()) }
        catch { result = .failure(error) }
        var phase: String?
        lock.withLock {
            guard active == record.id else { return }
            active = nil
            cancelLocked()
            switch result {
            case .success:
                removeLocked(discard: false)
                if failureLevel > 0 { phase = "resumed" }
                failureLevel = 0; nextAttempt = 0
            case .failure(let error):
                if case CommitStreamDeliveryFailure.unsafePath = error {
                    stopped = true
                    while !records.isEmpty { removeLocked(discard: true) }
                    phase = "unsafe_path"
                } else {
                    if failureLevel == 0 { phase = "retrying" }
                    failureLevel = min(failureLevel + 1, 5)
                    nextAttempt = clock.now() + pow(2, Double(failureLevel - 1))
                    // records[0] is still this record; see expireLocked().
                    records[0].attempts += 1
                    if records[0].attempts >= 6 || clock.now() >= record.received + 60 || (shutdownDeadline.map { clock.now() >= $0 } ?? false) {
                        removeLocked(discard: true)
                        phase = "discarded"
                    }
                }
            }
            armLocked()
        }
        if let phase { event(phase) }
    }
}
