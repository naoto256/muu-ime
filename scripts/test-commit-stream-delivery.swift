import Foundation

// The delivery queue is driven by hand: a manual clock whose due tasks run
// only on `advance`/`run`, and executors that stand in for the writer and
// return queues, so every interleaving is chosen by the test rather than by
// the scheduler. `run` refuses to spin forever.
private final class ManualClock {
    final class Task {
        let at: Double; let body: () -> Void; var cancelled = false
        init(_ at: Double, _ body: @escaping () -> Void) { self.at = at; self.body = body }
    }
    var now: Double = 0
    var tasks: [Task] = []
    var history: [Task] = []
    var clock: CommitStreamClock {
        .init(now: { self.now }, schedule: { at, body in
            let task = Task(at, body); self.tasks.append(task); self.history.append(task)
            return { task.cancelled = true }
        })
    }
    func run() {
        var limit = 0
        while let i = tasks.firstIndex(where: { !$0.cancelled && $0.at <= now }) {
            let task = tasks.remove(at: i); task.body()
            limit += 1; precondition(limit < 10000, "scheduler spinning")
        }
    }
    func advance(_ seconds: Double) { now += seconds; run() }
}
private final class Executor {
    var tasks: [() -> Void] = []
    func add(_ task: @escaping () -> Void) { tasks.append(task) }
    func run() { while !tasks.isEmpty { tasks.removeFirst()() } }
}

@main
private enum CommitStreamDeliveryTests {
    static func main() throws {
        try weights()
        retryAndOnce()
        boundsAndAge()
        activeBounds()
        unsafeStopReturnsEveryWeightOnce()
        reporterFailures()
        ageWhileIOIsBlocked()
        rawMetadataDoesNotReplaceInFlightDebt()
        unreadableMetadataBecomesUnreportableAfterRetryLimit()
        queuedUnreadableMetadataBecomesUnreportableAfterBatchRetryLimit()
        shutdown()
        commonDeadlineAndUnknownActiveIO()
        print("commit delivery: weights, FIFO, ingress bounds, retries, stale callbacks, debt and shutdown passed")
    }
    // Drop weights round-trip exactly up to u64::MAX (no float), a weight of
    // zero or another schema is refused, and adding weights saturates.
    static func weights() throws {
        for n: UInt64 in [1, (1 << 53) + 1, .max] {
            let decoded = try CommitStreamDropWeight.decode(JSONEncoder().encode(CommitStreamDropWeight(n)))
            precondition(decoded == n)
        }
        for data in [Data(), Data("{\"schema_version\":2,\"drop_weight\":1}".utf8), Data("{\"schema_version\":1,\"drop_weight\":0}".utf8)] {
            do { _ = try CommitStreamDropWeight.decode(data); preconditionFailure() } catch {}
        }
        precondition(commitStreamAdd(.max, 1) == .max)
    }
    static func retryAndOnce() {
        let clock = ManualClock(), writer = Executor(), returns = Executor()
        var weights: [UInt64] = [], writes: [Data] = [], fail = true
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: returns.add) { data in
            weights.append(try! CommitStreamDropWeight.decode(data)); return 0
        }
        let delivery = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { data in
            writes.append(data); if fail { throw CommitStreamDeliveryFailure.io }
        }
        delivery.append(Data([65]), weight: 4)
        delivery.append(Data([66]), weight: 1)
        clock.run(); let old = clock.history.map(\.body); writer.run()
        precondition(writes == [Data([65])] && delivery.status.retrying)
        fail = false; clock.advance(1); writer.run(); clock.run(); writer.run(); clock.run(); returns.run()
        precondition(writes == [Data([65]), Data([65]), Data([66])])
        precondition(weights.isEmpty && delivery.status.count == 0 && !delivery.status.retrying)
        old.forEach { $0() }; writer.run(); returns.run()
        precondition(writes.count == 3 && weights.isEmpty)

        fail = true; writes = []
        delivery.append(Data([67]), weight: 9)
        clock.run(); writer.run()
        for wait in [1.0, 2, 4, 8, 16] { clock.advance(wait); writer.run() }
        clock.run(); returns.run()
        precondition(writes.count == 6 && weights == [9] && delivery.status.discarded == 1)
        let finishedCallbacks = clock.history.map(\.body)
        delivery.append(Data([68]), weight: 1)
        finishedCallbacks.forEach { $0() }
        clock.run(); writer.run()
        precondition(writes.count == 6, "cooldown must survive record discard")
        clock.advance(15); writer.run(); precondition(writes.count == 6)
        fail = false; clock.advance(1); writer.run()
        precondition(writes.count == 7 && delivery.status.discarded == 1 && !delivery.status.retrying)
    }
    static func boundsAndAge() {
        let clock = ManualClock(), writer = Executor(), returns = Executor()
        var total: UInt64 = 0, writes = 0
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: returns.add) { data in
            total = commitStreamAdd(total, try! CommitStreamDropWeight.decode(data)); return 0
        }
        let delivery = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { _ in writes += 1 }
        for _ in 0..<256 { delivery.append(Data([65]), weight: 1) }
        delivery.append(Data([66]), weight: 12)
        precondition(delivery.status.count == 256 && delivery.status.bytes == 256 && delivery.status.discarded == 1)
        clock.run(); precondition(writer.tasks.count == 1 && returns.tasks.count == 1)
        returns.run(); precondition(total == 12)
        // Expire all before queued IO starts: no wall-clock sleeping.
        clock.now = 61
        delivery.beginShutdown(); clock.advance(2); writer.run(); clock.run(); returns.run()
        precondition(writes == 0 && delivery.status.count == 0 && delivery.status.discarded == 257)

        let d2 = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { _ in }
        d2.append(Data(repeating: 65, count: CommitStreamDelivery.maximumBytes), weight: 1)
        d2.append(Data([66]), weight: 10)
        precondition(d2.status.count == 1 && d2.status.bytes == CommitStreamDelivery.maximumBytes && d2.status.discarded == 1)
        let d3 = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { _ in }
        d3.append(Data([65, 10]), weight: 3); d3.append(Data([65]), weight: 1)
        precondition(d3.status.count == 1 && d3.status.discarded == 1 && !d3.status.stopped)
    }
    static func activeBounds() {
        let clock = ManualClock(), returns = Executor()
        let queue = DispatchQueue(label: "test.blocked-writer")
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        var returned: UInt64 = 0
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: returns.add) { data in
            returned += try! CommitStreamDropWeight.decode(data); return 0
        }
        let delivery = CommitStreamDelivery(clock: clock.clock, execute: { queue.async(execute: DispatchWorkItem(block: $0)) }, reporter: reporter) { _ in
            entered.signal(); precondition(release.wait(timeout: .now() + 30) == .success)
        }
        delivery.append(Data(repeating: 65, count: CommitStreamDelivery.maximumBytes), weight: 1)
        clock.run(); precondition(entered.wait(timeout: .now() + 30) == .success)
        for _ in 0..<1000 { delivery.append(Data([66]), weight: 8) }
        precondition(delivery.status.count == 1 && delivery.status.bytes == CommitStreamDelivery.maximumBytes && delivery.status.discarded == 1000)
        clock.run(); returns.run(); precondition(returned == 8000)
        release.signal(); queue.sync {}
        precondition(delivery.status.count == 0 && delivery.status.bytes == 0)
    }
    static func unsafeStopReturnsEveryWeightOnce() {
        let clock = ManualClock(), writer = Executor(), returns = Executor()
        var sent: [UInt64] = [], writes = 0
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: returns.add) { data in
            sent.append(try! CommitStreamDropWeight.decode(data)); return 0
        }
        let delivery = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { _ in writes += 1; throw CommitStreamDeliveryFailure.unsafePath }
        delivery.append(Data([65]),weight:4); delivery.append(Data([66]),weight:5)
        clock.run(); writer.run(); clock.run(); returns.run()
        precondition(delivery.status.stopped && delivery.status.discarded == 2 && sent == [9])
        let stale = clock.history.map(\.body)
        delivery.append(Data([67]),weight:7)
        stale.forEach { $0() }; clock.run(); writer.run(); returns.run()
        precondition(writes == 1 && delivery.status.count == 0 && delivery.status.discarded == 3 && sent == [9,7])
    }
    static func reporterFailures() {
        for failure: Int32 in [2, 6, 7, 3, 99] {
            let clock = ManualClock(), executor = Executor()
            var results: [UInt64] = [], status = failure
            let reporter = CommitStreamLossReporter(clock: clock.clock, execute: executor.add) { data in
                results.append(try! CommitStreamDropWeight.decode(data)); return status
            }
            reporter.accept(4); reporter.accept(5); clock.run()
            let stale = clock.history.map(\.body)
            reporter.accept(3); executor.run()
            status = 0; clock.advance(1); executor.run(); clock.run(); executor.run()
            if failure == 2 || failure == 6 {
                precondition(results == [9, 9, 3] && !reporter.status.pending)
            } else {
                precondition(results == [9] && reporter.status.unreportable && reporter.status.pending)
            }
            stale.forEach { $0() }; executor.run()
            precondition(results.count == ((failure == 2 || failure == 6) ? 3 : 1))
        }
        let clock = ManualClock(), executor = Executor()
        var attempts = 0, failEncode = true
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: executor.add, encode: { weight in
            if failEncode { throw CommitStreamDeliveryFailure.io }
            return try JSONEncoder().encode(CommitStreamDropWeight(weight))
        }) { _ in attempts += 1; return 0 }
        reporter.accept(4); clock.run(); executor.run(); precondition(attempts == 0)
        failEncode = false; clock.advance(1); executor.run(); precondition(attempts == 1 && !reporter.status.pending)
        var failures = 0
        let exhausted = CommitStreamLossReporter(clock: clock.clock, execute: executor.add) { _ in failures += 1; return 6 }
        exhausted.accept(1); clock.run(); executor.run()
        for wait in [1.0, 2, 4, 8, 16] { clock.advance(wait); executor.run() }
        clock.advance(100); executor.run(); precondition(failures == 6 && exhausted.status.pending)
        exhausted.receipt(); clock.run(); executor.run(); precondition(failures == 7)
    }
    static func ageWhileIOIsBlocked() {
        let clock = ManualClock(), writer = Executor(), returns = Executor()
        var returned: [UInt64] = []
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: returns.add) { data in
            returned.append(try! CommitStreamDropWeight.decode(data)); return 0
        }
        var delivery: CommitStreamDelivery!
        delivery = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { _ in
            clock.advance(60)
            precondition(delivery.status.count == 1 && delivery.status.discarded == 2, "only active OS IO is exempt from age")
        }
        delivery.append(Data([65]), weight: 4); delivery.append(Data([66]), weight: 7); delivery.append(Data([67]), weight: 8)
        clock.run(); writer.run(); clock.run(); returns.run(); clock.run(); returns.run()
        precondition(delivery.status.count == 0 && returned.reduce(0,+) == 15)
    }
    static func rawMetadataDoesNotReplaceInFlightDebt() {
        let clock = ManualClock(), executor = Executor()
        var sent: [Data] = []
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: executor.add) { data in sent.append(data); return 0 }
        reporter.accept(7); clock.run()
        let metadata = Data("opaque-metadata".utf8)
        reporter.acceptUnreadable(metadata) // Arrives after scheduling, before sending.
        executor.run(); clock.run(); executor.run()
        precondition(sent.count == 2 && (try! CommitStreamDropWeight.decode(sent[0])) == 7 && sent[1] == metadata)
        precondition(!reporter.status.pending)
    }
    static func unreadableMetadataBecomesUnreportableAfterRetryLimit() {
        let clock = ManualClock(), executor = Executor()
        var attempts = 0
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: executor.add) { _ in attempts += 1; return 2 }
        reporter.acceptUnreadable(Data("incompatible-metadata".utf8)); clock.run(); executor.run()
        for delay in [1.0,2,4,8,16] { clock.advance(delay); executor.run() }
        precondition(attempts == 6 && reporter.status.pending && reporter.status.unreportable, "incompatible metadata cannot remain merely waiting after the retry limit")
        reporter.receipt(); clock.advance(100); executor.run(); precondition(attempts == 6)
    }
    static func queuedUnreadableMetadataBecomesUnreportableAfterBatchRetryLimit() {
        for failure: Int32 in [2, 6] {
            let clock = ManualClock(), executor = Executor()
            var sent: [Data] = []
            let reporter = CommitStreamLossReporter(clock: clock.clock, execute: executor.add) { data in sent.append(data); return failure }
            reporter.accept(7); clock.run() // Hold the normal batch in-flight before its Core call.
            reporter.acceptUnreadable(Data("incompatible-metadata".utf8))
            executor.run()
            for delay in [1.0,2,4,8,16] {
                precondition(reporter.status.pending && !reporter.status.unreportable)
                clock.advance(delay); executor.run()
            }
            precondition(sent.count == 6 && sent.allSatisfy { (try? CommitStreamDropWeight.decode($0)) == 7 })
            precondition(reporter.status.pending && reporter.status.unreportable, "queued incompatible metadata cannot remain merely waiting behind an exhausted normal batch")
            reporter.receipt(); clock.advance(100); executor.run(); precondition(sent.count == 6)
        }
    }
    static func shutdown() {
        let clock = ManualClock(), writer = Executor(), returns = Executor()
        var weights: [UInt64] = [], writes = 0
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: returns.add) { data in
            weights.append(try! CommitStreamDropWeight.decode(data)); return 0
        }
        let delivery = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { _ in
            writes += 1
            // Simulate deadline expiring while OS IO has started. Only queued
            // records can be discarded; this successful record stays successful.
            clock.advance(2)
        }
        delivery.append(Data([65]), weight: 4); delivery.append(Data([66]), weight: 7)
        delivery.beginShutdown(); clock.run(); writer.run(); clock.run(); returns.run()
        precondition(writes == 1 && delivery.status.count == 0 && delivery.status.discarded == 1)
        precondition(weights.isEmpty && reporter.status.pending, "deadline leaves debt pending rather than guessing a report")
        let old = clock.history.map(\.body); old.forEach { $0() }; writer.run()
        precondition(writes == 1)
        delivery.append(Data([67]), weight: 2)
        precondition(delivery.status.discarded == 2)
    }
    static func commonDeadlineAndUnknownActiveIO() {
        let clock = ManualClock(), writer = Executor(), returns = Executor()
        var sent: [UInt64] = [], writes = 0
        let reporter = CommitStreamLossReporter(clock: clock.clock, execute: returns.add) { bytes in
            sent.append(try! CommitStreamDropWeight.decode(bytes)); return 0
        }
        let delivery = CommitStreamDelivery(clock: clock.clock, execute: writer.add, reporter: reporter) { _ in writes += 1 }
        delivery.append(Data([65]), weight: 7); clock.run() // pump is queued, not started
        delivery.beginShutdown(deadline: 1)
        delivery.beginShutdown(deadline: 20) // never extends the first deadline
        clock.now = 1
        // App expiry runs before the independent timer. It must transfer the
        // waiting record once before allowing process exit.
        delivery.expireShutdown(); reporter.expireShutdown()
        precondition(delivery.status.count == 0 && delivery.status.discarded == 1 && reporter.status.pending)
        precondition(reporter.shutdownSettled && sent.isEmpty)
        clock.history.forEach { $0.body() }; writer.run(); returns.run()
        precondition(writes == 0 && sent.isEmpty && delivery.status.discarded == 1)

        let activeClock = ManualClock(), activeWriter = Executor()
        let activeReporter = CommitStreamLossReporter(clock: activeClock.clock, report: { _ in preconditionFailure("no lost active record") })
        var active: CommitStreamDelivery!
        active = CommitStreamDelivery(clock: activeClock.clock, execute: activeWriter.add, reporter: activeReporter) { _ in
            activeClock.now = 2; active.expireShutdown(); activeReporter.expireShutdown()
            precondition(active.status.count == 1 && active.status.discarded == 0,
                "started IO remains unknown rather than discarded")
        }
        active.append(Data([65]), weight: 9); active.beginShutdown(deadline: 2)
        activeClock.run(); activeWriter.run(); activeClock.run()
        precondition(active.status.count == 0 && active.status.discarded == 0 && !activeReporter.status.pending)
    }

}
