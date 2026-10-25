import Foundation

// A manual clock for the recorder tests. Work scheduled for a time already
// reached runs at once on a global queue; later work waits until `advance`
// passes its time and then runs synchronously on the caller, unless it was
// cancelled. No test depends on wall-clock time.
final class CommitStreamTestClock {
    private let lock = NSLock()
    private var time: Double = 0
    private var tasks: [(Double, DispatchWorkItem)] = []
    var clock: CommitStreamClock {
        .init(now: { self.lock.withLock { self.time } }, schedule: { at, body in
            let work = DispatchWorkItem(block: body)
            self.lock.withLock {
                if at <= self.time { DispatchQueue.global(qos: .utility).async(execute: work) }
                else { self.tasks.append((at,work)) }
            }
            return { work.cancel() }
        })
    }
    func advance(_ seconds: Double) {
        let due = lock.withLock { () -> [DispatchWorkItem] in
            time += seconds
            let ready = tasks.filter { $0.0 <= time }.map { $0.1 }
            tasks.removeAll { $0.0 <= time }
            return ready
        }
        due.forEach { if !$0.isCancelled { $0.perform() } }
    }
}
