import AppKit
import Darwin

// Process-entry policy, before AppKit/Core/IO starts. Respect a launcher-supplied
// value; Accelerate reads this environment once. Never change it after launch.
let blasThreadLimitInherited = getenv("VECLIB_MAXIMUM_THREADS") != nil
if !blasThreadLimitInherited { setenv("VECLIB_MAXIMUM_THREADS", "2", 0) }
LifecycleLog.shared.blasThreadLimit(
    getenv("VECLIB_MAXIMUM_THREADS").map { String(cString: $0) }, inherited: blasThreadLimitInherited)

let application = NSApplication.shared
// The executable entry runs on main; keep the manual delegate lifetime while
// making the actor boundary explicit (no @main or alternate launch path).
let applicationDelegate = MainActor.assumeIsolated { AppDelegate() }
application.delegate = applicationDelegate

withExtendedLifetime(applicationDelegate) {
    application.run()
}
