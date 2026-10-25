// What each model is called and where its files live, in one place. Loading
// (ConversionLmConfiguration gives Core the directory) and acquisition (the
// download's destination) both derive from `directory(under:)`, so a
// published download lands where Core looks. About shows the same names.
import Foundation

// The raw values are wire values: the MuuConversionLmModel preference, the
// `conversion_lm_model` field sent to Core, and the models Core reports back.
enum ConversionLmModel: String, Codable, Sendable {
    case edge, zenz
    // Both the display name and the cache directory name. Renaming a model
    // moves its cache: files already in the old directory are no longer found.
    // scripts/fetch-conversion-lm.sh writes to the same directory names.
    var name: String { self == .zenz ? "zenz-v3.2-small" : "kanakanji-edge-33m" }

    // Application Support/MuuIME/Models/<name>. Core is configured with this
    // directory for either model, downloadable or not.
    func directory(under support: URL) -> URL {
        support.appendingPathComponent("MuuIME/Models", isDirectory: true)
            .appendingPathComponent(name, isDirectory: true)
    }
}

// The app acquires exactly this artifact. This is deliberately not a model
// registry or an arbitrary-URL downloader; Core still interprets the GGUF.
//
// Every value is pinned: the URL names a fixed revision (never `main` or a
// lookup of the latest), and the file must have exactly `byteCount` bytes and
// this SHA-256. They are the same values scripts/fetch-conversion-lm.sh pins.
// The test pins them again as literals rather than reading them from here.
//
// Only zenz has a profile (the initializer is private). The edge model keeps
// its directory but gains no download path.
struct ModelDownloadProfile {
    let model: ConversionLmModel
    var name: String { model.name }
    let fileName: String
    let url: URL
    let byteCount: Int
    let sha256: String

    static let zenz = Self(model: .zenz, fileName: "ggml-model-Q5_K_M.gguf",
        url: URL(string: "https://huggingface.co/Miwa-Keita/zenz-v3.2-small-gguf/resolve/c67e03e07d215c869f591b274c1631170d3e11fe/ggml-model-Q5_K_M.gguf")!,
        byteCount: 73_871_936,
        sha256: "29c223d4c23327b80fd13ebb5ab2555057a46317997d5da391584ffbef0db673")

    // The published file, inside the same directory Core is configured with.
    func destination(under support: URL) -> URL {
        model.directory(under: support).appendingPathComponent(fileName)
    }

    private init(model: ConversionLmModel, fileName: String, url: URL, byteCount: Int, sha256: String) {
        self.model = model; self.fileName = fileName; self.url = url
        self.byteCount = byteCount; self.sha256 = sha256
    }

    // Synthetic bytes and URL, but the real zenz directory, so tests exercise
    // the production cache layout.
    #if MUU_TEST
    static func fixture(bytes: Data) -> Self {
        Self(model: .zenz, fileName: "fixture.gguf",
            url: URL(string: "https://huggingface.co/test/model")!,
            byteCount: bytes.count, sha256: FileSafety.sha256(bytes))
    }
    #endif
}
