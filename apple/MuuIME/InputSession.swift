import Foundation

/// The interchangeable Rust boundary stays synchronous and carries the original
/// status/view. It neither interprets text nor adds a persistence drain.
@MainActor
protocol InputSession: AnyObject {
    var isAvailable: Bool { get }
    func handle(_ request: EventRequest) -> EngineResponse
    func handle(_ request: CommandRequest) -> EngineResponse
    /// Replaces only the Rust session. Candidate generations live in the
    /// presentation and are not reset by this call.
    func recreate()
}

// The real engine and injected fakes share this protocol; Core ownership,
// handle's recorder/learning defer, and teardown remain in RustEngine.
//
// Production only ever makes a RustEngine, but the seam stays: controller
// tests inject a scripted session with no Rust handle or persistence, so the
// coordinator and presentation can be driven response by response (closing
// passthrough, commit and mark order) without a Core. Only the forwarding
// object went away; the seam was never in it.
extension RustEngine: InputSession {}
