import Foundation

/// A cancellation token for one scheduled badge timer. Cancelling is
/// idempotent; the closure runs at most once.
@MainActor
final class ModeBadgeScheduledTask {
    private var cancellation: (() -> Void)?

    init(cancellation: @escaping () -> Void) {
        self.cancellation = cancellation
    }

    func cancel() {
        cancellation?()
        cancellation = nil
    }

    deinit {
        // Off main, the timer may still fire before this runs. Its action
        // holds the badge weakly and checks the badge generation.
        let cancellation = cancellation
        finishInputUIOnMain { cancellation?() }
    }
}

/// The badge's clock and timers, injectable so tests can step time by hand.
/// `action` returns whether a repeating timer should keep running.
@MainActor
struct ModeBadgeScheduler {
    let now: () -> TimeInterval
    let schedule: (
        _ interval: TimeInterval,
        _ repeats: Bool,
        _ action: @escaping () -> Bool
    ) -> ModeBadgeScheduledTask

    static let mainRunLoop = Self(
        now: { ProcessInfo.processInfo.systemUptime },
        schedule: { interval, repeats, action in
            let timer = Timer(timeInterval: interval, repeats: repeats) { timer in
                if !action() {
                    timer.invalidate()
                }
            }
            // Common modes, so the fade keeps running while a menu or a drag
            // holds the run loop in a tracking mode.
            RunLoop.main.add(timer, forMode: .common)
            return ModeBadgeScheduledTask {
                timer.invalidate()
            }
        }
    )
}

