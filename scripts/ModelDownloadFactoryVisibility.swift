import Foundation

// Compiled twice by model-download-test: with MUU_TEST it must build and run;
// without it the `forTesting` member must be missing. Older Swift also reports
// that arguments on the same call have no contextual type. The checker
// confines inference cascades to that call, but the MUU_UNRELATED_DIAGNOSTIC_FIXTURE
// build adds an unrelated error that must still be rejected.
@main private enum ModelDownloadFactoryVisibility {
    @MainActor static func main() {
        _ = ModelDownloadStore.forTesting(support: nil, bytes: Data([1]), configuration: {
            URLSessionConfiguration.ephemeral
        })
        #if MUU_UNRELATED_DIAGNOSTIC_FIXTURE
        let unrelated: Int = "not an integer"
        print(unrelated)
        #endif
    }
}
