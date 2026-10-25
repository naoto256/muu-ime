import Foundation

enum MuuConstants {
    static let bundleIdentifier = "dev.muu.inputmethod.Muu.factory-visibility-test"
}

// The same consumer must compile with MUU_TEST and fail without it. It never
// reads preferences or names a real storage directory, even in the positive run.
// The positive run is also a check: the directory closure traps, so a disabled
// factory that looked up its path would fail here. The recipe accepts only the
// "has no member" error and lost type context within that call for the standard
// build, so an unrelated compile error cannot pass for a hidden entry.
@main enum RecorderFactoryVisibility {
    static func main() {
        // Negative fixture for the recipe: a second, unrelated error beside the
        // expected one must make the diagnostic check fail.
        #if !MUU_TEST && MUU_UNRELATED_DIAGNOSTIC_FIXTURE
        let unrelated: Int = "unexpected unrelated type error"
        _ = unrelated
        #endif
        let recorder = CommitStreamRecorder.makeProcessRecorderForTesting(
            readBool: { _ in false },
            bundle: .main,
            applicationSupportDirectory: { preconditionFailure("disabled factory read a path") }
        )
        precondition(!recorder.status.requested && !recorder.status.configured)
        print("recorder factory visibility: test entry available")
    }
}
