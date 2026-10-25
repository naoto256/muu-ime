/// Build-time capabilities cannot be enabled by a user's preferences.
/// The bundle variant is checked separately when constructing a recorder.
///
/// Why a compilation condition and not a preference: recording writes the
/// converted text a user types to a plain file. Any process running as the
/// same user can change Muu's defaults with `defaults write`, so a
/// preference-only switch would let it start that log and read it without
/// the Input Monitoring (TCC) permission or any prompt. Only a build made
/// with `MUU_DOGFOOD` has the path at all.
enum MuuBuildCapabilities {
    #if MUU_DOGFOOD
    static let isDogfood = true
    static let variant = "dogfood"
    #else
    static let isDogfood = false
    static let variant = "standard"
    #endif
    // Same reasoning as recording: a preference cannot grant what the build
    // lacks. Here the effect is availability, turning input into pass-through.
    static func forceCreateFailureRequested(setting: (String) -> Bool) -> Bool {
        #if MUU_DOGFOOD || MUU_TEST
        setting("MuuForceCreateFailure")
        #else
        // Distribution builds do not even read this development preference.
        false
        #endif
    }
}
