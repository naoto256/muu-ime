import Foundation

// Compiled three ways by session-create-policy-test. Standard must neither
// read the key nor honour it; dogfood and test read it once per call.
@main
struct ForceCreateFailureTests {
    static func main() {
        #if MUU_DOGFOOD || MUU_TEST
        let available = true
        #else
        let available = false
        #endif
        for requested in [true, false] {
            var keys: [String] = []
            let result = MuuBuildCapabilities.forceCreateFailureRequested { key in
                keys.append(key)
                return requested
            }
            precondition(result == (available && requested))
            precondition(keys == (available ? ["MuuForceCreateFailure"] : []),
                "standard must not read the failure-injection preference")
        }
        print("session creation policy passed; injection available=\(available)")
    }
}
