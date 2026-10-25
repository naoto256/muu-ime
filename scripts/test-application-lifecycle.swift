import AppKit

/// An isolated AppKit process, without IMKServer, user data, windows, or network.
/// General terminateLater delivery is distinct from IMK's unverified delivery.
//
// Two modes, one process each. "early": the only participant finishes before
// the deadline. "deadline": it never finishes, and the scripted clock fires
// the expiry at t=2. Either way AppKit gets exactly one reply to one
// should-terminate, the shared deadline is 2 s, and only the deadline mode
// reports deadlineReached.
@MainActor private final class TerminationFixture: NSObject, NSApplicationDelegate {
    let mode: String
    var time: TimeInterval = 0
    var expiry: (() -> Void)?
    var lifecycle: ApplicationLifecycle!
    var shouldCalls = 0
    var replies = 0
    init(mode: String) { self.mode = mode; super.init() }
    func applicationDidFinishLaunching(_ notification: Notification) {
        let clock = ApplicationClock(now: { self.time }, schedule: { _, work in self.expiry = work; return {} })
        lifecycle = ApplicationLifecycle(clock: clock, gate: StoreShutdownGate(), log: LifecycleLog(sink: { _ in }))
        lifecycle.register(.init(name: .user, start: { _, done in
            if self.mode == "early" { DispatchQueue.global().async { done(.settled) } }
        }))
        DispatchQueue.main.async { NSApp.terminate(nil) }
    }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        shouldCalls += 1
        lifecycle.begin(.should)
        lifecycle.whenFinished { _ in
            self.replies += 1; precondition(self.replies == 1)
            sender.reply(toApplicationShouldTerminate: true)
        }
        if mode == "deadline" {
            RunLoop.main.perform(inModes: [.common]) {
                MainActor.assumeIsolated { self.time = 2; self.expiry?() }
            }
        }
        return .terminateLater
    }
    func applicationWillTerminate(_ notification: Notification) {
        lifecycle.begin(.will); lifecycle.waitForCompletion()
        precondition(shouldCalls == 1 && replies == 1 && lifecycle.deadline == 2)
        precondition(lifecycle.result?.deadlineReached == (mode == "deadline"))
        print("AppKit termination: \(mode) passed")
        fflush(stdout)
    }
}

@main @MainActor private enum ApplicationLifecycleTest {
    static func main() {
        // The BLAS thread-limit fact is logged once per log, from its first
        // report, and an environment value cannot inject a second log line.
        for (value, inherited, expected) in [("2", false, "2"), ("1", true, "1"), ("8", true, "8"), ("invalid\ntext", true, "invalid")] {
            var lines: [String] = []
            let log = LifecycleLog(sink: { lines.append($0) })
            log.blasThreadLimit(value, inherited: inherited)
            log.blasThreadLimit("3", inherited: true)
            precondition(lines.count == 1 && lines[0].contains("limit=\(expected) inherited=\(inherited)"))
            precondition(!lines[0].contains("\n"), "environment must not inject log lines")
        }
        precondition(CommandLine.arguments.count == 2)
        let mode = CommandLine.arguments[1]
        precondition(mode == "early" || mode == "deadline")
        let app = NSApplication.shared
        let delegate = TerminationFixture(mode: mode)
        app.delegate = delegate
        withExtendedLifetime(delegate) { app.run() }
        preconditionFailure("AppKit should terminate the isolated process")
    }
}
