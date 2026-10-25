import Foundation

/// UI teardown carries resources, never the owner or an IMK client. Main-thread
/// release stays synchronous; another thread never waits for the main queue.
///
/// Called from deinit, which is nonisolated even in a @MainActor class.
/// Off main, a window stays ordered in until that main turn runs.
func finishInputUIOnMain(_ cleanup: @escaping @MainActor () -> Void) {
    if Thread.isMainThread {
        MainActor.assumeIsolated { cleanup() }
    } else {
        DispatchQueue.main.async { cleanup() }
    }
}
