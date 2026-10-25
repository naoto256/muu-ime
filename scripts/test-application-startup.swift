import AppKit

@main @MainActor private enum ApplicationStartupTest {
    static func main() {
        precondition(CommandLine.arguments.count == 2)
        let mode = CommandLine.arguments[1]
        precondition(["abi-mismatch", "initialization-failure", "degraded-success", "normal-success"].contains(mode))
        StartupFixture.mode = mode
        let delegate = AppDelegate()
        delegate.applicationDidFinishLaunching(Notification(name: NSApplication.didFinishLaunchingNotification))

        let events = StartupFixture.events
        func once(_ event: String) { precondition(events.filter { $0 == event }.count == 1, event) }
        func before(_ first: String, _ second: String) {
            guard let a = events.firstIndex(of: first), let b = events.firstIndex(of: second) else {
                preconditionFailure("missing startup boundary: \(first) / \(second)")
            }
            precondition(a < b, "\(first) must precede \(second)")
        }
        // Every mode: the ABI is read once, About is told once, and the server
        // is created once and last. Until the server exists macOS cannot create
        // controllers, so a controller can never see a half-started Core.
        once("abi"); once("about"); once("server")
        // The model download store exists in every mode, ABI mismatch included,
        // is handed to About before it starts, and starts before the server.
        once("model.construct"); once("model.about"); once("model.start")
        before("model.about", "model.start"); before("model.start", "server")
        precondition(events.last == "server", "the server exposes controllers, so it must come last")
        switch mode {
        // About must get the matching state and exactly the stores that exist:
        // none after an ABI mismatch, the three constructed before
        // initialization after a failure, all five after success, with the
        // degraded flag passed through to the learning store.
        case "abi-mismatch":
            precondition(events.filter { !$0.hasPrefix("model.") } == ["abi", "about", "server"],
                "ABI failure must not construct data stores or initialize Core")
            precondition(StartupFixture.about == .abiMismatch && StartupFixture.aboutStores.isEmpty)
        case "initialization-failure":
            precondition(StartupFixture.about == .failed)
            precondition(StartupFixture.aboutStores == ["learning", "user", "recorder"])
            precondition(StartupFixture.acceptedResult == nil)
        default:
            precondition(StartupFixture.acceptedResult == .init(degraded: mode == "degraded-success"))
            precondition(StartupFixture.about == .ready)
            precondition(StartupFixture.aboutStores == ["learning", "user", "recorder", "postal", "imported"])
        }

        // The ordering contract of docs/src/internals/architecture.md「起動と終了」: both startup
        // inputs are read before Core initializes. A failure starts no
        // overlay and accepts nothing. On success, learning and the user view
        // accept the result before Postal and Imported are constructed and
        // explicitly started, and both start before About. Unrelated
        // orderings, and how often a property is read, are left free.
        if mode != "abi-mismatch" {
            once("initialize"); once("configuration")
            for store in ["learning", "user", "recorder"] {
                once("\(store).construct"); before("\(store).construct", "initialize")
            }
            before("abi", "initialize"); before("user.read", "initialize")
            before("learning.read", "initialize"); before("initialize", "about")
            if mode == "initialization-failure" {
                precondition(!events.contains { !$0.hasPrefix("model.") && ($0.hasSuffix(".start") || $0.hasSuffix(".accept")) })
                // These stores do not exist on failure, so neither can enter
                // load/prepare/publish. User's bytes were needed before init.
                precondition(!events.contains("postal.construct") && !events.contains("imported.construct"))
            } else {
                for accepted in ["learning.accept", "user.accept"] {
                    once(accepted); before("initialize", accepted)
                    before(accepted, "postal.start"); before(accepted, "imported.start")
                }
                for store in ["postal", "imported"] {
                    once("\(store).construct"); once("\(store).start")
                    before("initialize", "\(store).construct"); before("\(store).start", "about")
                }
            }
        }

        // A repeated termination notification must run each registered
        // shutdown exactly once, and only for stores that were started. The
        // model download participant is registered in every mode, so it is
        // expected on top of the started stores.
        let launchEvents = StartupFixture.events
        let notification = Notification(name: NSApplication.willTerminateNotification)
        delegate.applicationWillTerminate(notification)
        delegate.applicationWillTerminate(notification)
        let shutdown = Array(StartupFixture.events.dropFirst(launchEvents.count))
        let expected: Set<String>
        if mode == "abi-mismatch" { expected = [] }
        else if mode == "initialization-failure" { expected = ["learning.shutdown", "user.shutdown", "recorder.shutdown"] }
        else { expected = ["learning.shutdown", "user.shutdown", "recorder.shutdown", "postal.shutdown", "imported.shutdown"] }
        precondition(Set(shutdown) == expected.union(["model.shutdown"]) && shutdown.count == expected.count + 1,
            "the actual delegate registers exactly the started stores, once")
        print("application startup: \(mode) passed")
    }
}
