import AppKit
import CryptoKit

enum MuuConstants {
    static let expectedABIVersion: UInt32 = 13
    static let bundleIdentifier = "dev.muu.test.StoreABI"
    static let connectionName = "dev.muu.test.StoreABI_Connection"
}

// Store publication uses the production Swift bridges. Session requests below
// use the public C boundary directly, so they cannot drain into shared stores
// or read the running user's preferences through RustEngine's convenience API.
private final class StoreSession {
    private var handle: OpaquePointer?
    init() {
        var output = MuuBuffer(data: nil, len: 0)
        let status = muu_session_create(&handle, &output)
        defer { muu_buffer_free(output) }
        precondition(status == MUU_RESULT_OK && handle != nil)
    }
    deinit { muu_session_destroy(handle) }
    func send<T: Encodable>(_ request: T) throws -> ViewState {
        let input = try MuuEnvelope.encode(request)
        var output = MuuBuffer(data: nil, len: 0)
        let status = input.withUnsafeBytes { bytes in
            muu_session_handle_event(handle, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, &output)
        }
        defer { muu_buffer_free(output) }
        precondition(status == MUU_RESULT_OK)
        return try MuuEnvelope.decodeView(Data(bytes: output.data!, count: output.len))
    }
    func convert(_ reading: String) throws -> ViewState {
        _ = try send(EventRequest(key: .init(eventType: "key_down", keycode: 0, modifiers: 0,
            charactersUTF8: reading, charactersIgnoringModifiers: reading, isRepeat: false)))
        return try send(CommandRequest(command: "convert"))
    }
}

@main @MainActor private enum StoreABITest {
    static func main() throws {
        precondition(CommandLine.arguments.count == 5, "mode, private root, dictionary and fixture directory are required")
        let mode = CommandLine.arguments[1]
        let root = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        let dictionary = CommandLine.arguments[3]
        let fixtures = URL(fileURLWithPath: CommandLine.arguments[4], isDirectory: true)
        if mode == "factory" { try factory(root: root, dictionary: dictionary); return }
        let user = UserDictionaryStore(baseDirectory: root, bundleIdentifier: MuuConstants.bundleIdentifier,
            bridge: userBridge(interrupt: mode == "interrupt-user"))
        // Fixed literals instead of process-wide UserDefaults: model off, no
        // recorder, the public dictionary, and storage under the private root.
        let configuration = RustCore.configurationForTesting(userDictionaryFileState: user.startupFile.state,
            boolSetting: { key in precondition(key == "MuuConversionLmEnabled"); return false },
            stringSetting: { key in precondition(key == "MuuConversionLmModel"); return nil },
            support: { root }, dictionary: { dictionary }, recorder: { nil })
        let initialized = try RustCore.initialize(configuration, learning: nil, userDictionaryFile: user.startupFile.bytes).get()
        try user.acceptInitializedCore()
        precondition(!initialized.degraded.personalFrequencyInvalid)
        let status = try RustCore.runtimeStatus().get()
        precondition(status.languageModel == .unloaded && !status.commitStreamEnabled)
        let barrier = mode == "first-load" ? DispatchSemaphore(value: 0) : nil
        let postalBarrier = mode == "first-load" ? DispatchSemaphore(value: 0) : nil
        var importedBridge = RustCore.importedDictionaryBridge
        let originalLoad = importedBridge.loadBegin
        importedBridge.loadBegin = { command, file in
            precondition(!Thread.isMainThread)
            if mode == "failed" { throw CocoaError(.fileReadNoPermission) }
            if let barrier { precondition(barrier.wait(timeout: .now() + 30) == .success) }
            return try originalLoad(command, file)
        }
        let originalPublish = importedBridge.publish
        var interruptImported = false
        importedBridge.publish = { ticket in
            let status = originalPublish(ticket)
            if status == 0 && interruptImported { interrupt("imported_after_ack_before_current") }
            return status
        }
        let imported = ImportedDictionaryStore(baseDirectory: root, bundleIdentifier: MuuConstants.bundleIdentifier,
            bridge: importedBridge, recoveryDelay: { _ in preconditionFailure("unexpected recovery") })
        var postalBridge = RustCore.postalCodeBridge
        let postalLoad = postalBridge.loadBegin
        postalBridge.loadBegin = { command, file in
            precondition(!Thread.isMainThread)
            if mode == "failed" { throw CocoaError(.fileReadNoPermission) }
            if let postalBarrier { precondition(postalBarrier.wait(timeout: .now() + 30) == .success) }
            return try postalLoad(command, file)
        }
        let postalPublish = postalBridge.publish
        // The seed load also publishes, so arm the interruption only for the
        // explicit update after that initial load has completed.
        var interruptPostal = false
        var seedAcknowledged = false
        let postalRoot = root.appendingPathComponent(MuuConstants.bundleIdentifier).appendingPathComponent("postal_code/v1")
        postalBridge.publish = { ticket in
            if mode == "first-load" {
                precondition(!FileManager.default.fileExists(atPath: postalRoot.appendingPathComponent("current").path))
                precondition(!FileManager.default.fileExists(atPath: postalRoot.appendingPathComponent("generations").path))
            }
            let status = postalPublish(ticket)
            if mode == "first-load" && status == 0 {
                seedAcknowledged = true
                precondition(!FileManager.default.fileExists(atPath: postalRoot.appendingPathComponent("current").path))
                precondition(!FileManager.default.fileExists(atPath: postalRoot.appendingPathComponent("generations").path))
            }
            if status == 0 && interruptPostal { interrupt("postal_after_ack_before_generation") }
            return status
        }
        var downloads = 0
        let postal = PostalCodeStore(baseDirectory: root, bundleIdentifier: MuuConstants.bundleIdentifier,
            seedDataURL: mode == "failed" ? nil : fixtures.appendingPathComponent("seed.tsv"),
            seedManifestURL: mode == "failed" ? nil : fixtures.appendingPathComponent("seed.json"),
            bridge: postalBridge, downloadOverride: { url, completion in
                downloads += 1
                let zip = try! Data(contentsOf: fixtures.appendingPathComponent(mode == "interrupt-postal" ? "next.zip" : "update.zip"))
                let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: "HTTP/1.1",
                    headerFields: ["Last-Modified": "Thu, 01 Jan 2026 00:00:00 GMT"])!
                completion(.success((zip, response)))
            // The interrupted update gets a different retrieval time, and so a
            // different generation from the saved one it must not replace.
            }, clock: { mode == "interrupt-postal" ? 1_767_225_602 : 1_767_225_601 })
        let about = AboutStatusProvider()
        about.configure(initialization: .ready, build: .unknown, user: user, imported: imported, postal: postal)
        precondition(imported.startupState == .notStarted && postal.startupState == .notStarted)
        precondition(!imported.canEdit && !postal.canUpdate)
        var importedLoaded: Result<Void, Error>?
        imported.start { importedLoaded = $0 }
        var postalLoaded: Result<Void, Error>?
        postal.start { postalLoaded = $0 }
        precondition(imported.startupState == .loading && postal.startupState == .loading)
        precondition(!imported.canEdit && !postal.canUpdate)
        precondition(line("取り込み辞書", about).contains("読み込み中") && line("郵便番号", about).contains("読み込み中"))
        var templateRejected = false
        imported.exportTemplate(to: root.appendingPathComponent("must-not-be-created.muudict")) {
            if case .failure = $0 { templateRejected = true }
        }
        precondition(templateRejected && !FileManager.default.fileExists(atPath: root.appendingPathComponent("must-not-be-created.muudict").path))
        var updateRejected = false
        postal.updateFromJapanPost { if case .failure = $0 { updateRejected = true } }
        precondition(updateRejected && downloads == 0, "loading must neither start edits nor download")
        if mode == "first-load" {
            // The worker is held before its real load_begin. Input must work
            // against the initial empty layers without waiting on that worker.
            let view = try StoreSession().convert("1000001")
            precondition(!items(view).contains("東京都試験区旧町"))
            barrier!.signal()
            postalBarrier!.signal()
        }
        // The callback precedes ticket cancellation and the final UI idle
        // handoff. A next operation is allowed only after that real boundary.
        pump { importedLoaded != nil && postalLoaded != nil && !imported.isBusy && !postal.isUpdating }
        precondition(downloads == 0, "startup never performs an automatic fetch")

        // Each mode is one process. A test in test-store-abi.py runs several
        // in sequence on its own private root, so the "read*" modes check what
        // an earlier mode, or an interrupted one, left on disk.
        switch mode {
        case "first-load", "read-seed":
            try importedLoaded!.get(); try postalLoaded!.get()
            precondition(imported.startupState == .ready && imported.canEdit)
            precondition(postal.startupState == .ready && postal.canUpdate)
            precondition(imported.view?.dictionaries?.isEmpty == true)
            if mode == "first-load" { precondition(seedAcknowledged) }
            try checkConversion("1000001", expected: "東京都試験区旧町")
        case "readonly":
            try importedLoaded!.get(); try postalLoaded!.get()
            precondition(initialized.degraded.userDictionaryInvalid && !user.canEdit)
            precondition(imported.startupState == .readOnly && !imported.canEdit)
            // Unlike the imported store, Postal stays updatable when read-only
            // or failed: an explicit update is its repair path.
            precondition(postal.startupState == .readOnly && postal.canUpdate)
            for name in ["ユーザー辞書", "取り込み辞書", "郵便番号"] {
                precondition(line(name, about).contains("読み取り専用"))
            }
        case "failed":
            if case .success = importedLoaded! { preconditionFailure("IO failure was accepted") }
            if case .success = postalLoaded! { preconditionFailure("IO failure was accepted") }
            precondition(imported.startupState == .failed && !imported.canEdit)
            precondition(postal.startupState == .failed && postal.canUpdate)
            precondition(line("取り込み辞書", about).contains("初期読み込み失敗"))
            precondition(line("郵便番号", about).contains("初期読み込み失敗"))
            try update(postal)
            precondition(downloads == 1 && postal.startupState == .ready)
            try checkConversion("1000002", expected: "東京都試験区新町")
        case "recover-postal":
            try importedLoaded!.get(); try postalLoaded!.get()
            precondition(postal.startupState == .readOnly)
            try update(postal)
            precondition(postal.startupState == .ready && postal.canUpdate && downloads == 1)
            try checkConversion("1000002", expected: "東京都試験区新町")
        case "write":
            try importedLoaded!.get(); try postalLoaded!.get()
            try upsert(user, surface: "自作ユーザー語", replacing: nil)
            try install(imported, source: fixtures.appendingPathComponent("one.muudict"))
            try update(postal)
            precondition(downloads == 1)
            try checkSaved(user: user, imported: imported, postal: postal, fixtures: fixtures, userSurface: "自作ユーザー語")
        case "read", "read-user-interrupted":
            try importedLoaded!.get(); try postalLoaded!.get()
            try checkSaved(user: user, imported: imported, postal: postal, fixtures: fixtures,
                userSurface: mode == "read" ? "自作ユーザー語" : "中断ユーザー語")
        case "interrupt-user":
            try importedLoaded!.get(); try postalLoaded!.get()
            try upsert(user, surface: "中断ユーザー語", replacing: 0)
            preconditionFailure("the hard exit did not happen")
        case "interrupt-import":
            try importedLoaded!.get(); try postalLoaded!.get()
            interruptImported = true
            try install(imported, source: fixtures.appendingPathComponent("next.muudict"))
            preconditionFailure("the hard exit did not happen")
        case "interrupt-postal":
            try importedLoaded!.get(); try postalLoaded!.get()
            interruptPostal = true
            try update(postal)
            preconditionFailure("the hard exit did not happen")
        case "legacy-load", "legacy-disable", "legacy-read-disabled", "legacy-enable", "legacy-read-enabled":
            try importedLoaded!.get(); try postalLoaded!.get()
            precondition(imported.startupState == .ready && imported.canEdit)
            // Each edit is checked against the catalog's content first, and is
            // followed by waiting for the store to go idle, not just for the
            // callback, before the next step.
            if mode == "legacy-disable" {
                try checkLegacy(imported, fixtures: fixtures, enabled: true)
                try wait { imported.setEnabled(false, dictionaryID: "org.example.abi", completion: $0) }
                pump { !imported.isBusy }
            } else if mode == "legacy-enable" {
                try checkLegacy(imported, fixtures: fixtures, enabled: false)
                try wait { imported.setEnabled(true, dictionaryID: "org.example.abi", completion: $0) }
                pump { !imported.isBusy }
            }
            // Enabled steps must commit the catalog's original surface through a
            // real Session. Disabled steps must not offer it. A surface changed
            // during migration fails here at legacy-enable, even when its
            // metadata, count and SHA were kept.
            let enabled = ["legacy-load", "legacy-enable", "legacy-read-enabled"].contains(mode)
            try checkLegacy(imported, fixtures: fixtures, enabled: enabled)
            if enabled { try checkConversion("abu", expected: "自作取り込み語") }
            else {
                let view = try StoreSession().convert("abu")
                precondition(!items(view).contains("自作取り込み語"))
            }
        default: preconditionFailure("unhandled store ABI mode: \(mode)")
        }
        print("store ABI: \(mode) passed")
    }

    private static func userBridge(interrupt enabled: Bool) -> UserDictionaryCoreBridge {
        var bridge = RustCore.userDictionaryBridge
        let publish = bridge.publish
        bridge.publish = { ticket in
            if enabled { interrupt("user_after_rename_before_publish") }
            return publish(ticket)
        }
        return bridge
    }
    // The three stop points, each inside a store's real transaction:
    // - user_after_rename_before_publish: the new file is on disk, Core has
    //   not published it. Next process: the complete new file.
    // - imported_after_ack_before_current: Core published; blob and
    //   generation are staged; `current` is old. Next process: the complete
    //   old dictionary.
    // - postal_after_ack_before_generation: Core published; nothing written.
    //   Next process: the complete old snapshot.
    nonisolated private static func interrupt(_ phase: String) -> Never {
        print("hard-exit: \(phase)"); fflush(stdout)
        _exit(86) // No catch, defer or Store/Core cleanup runs across this edge.
    }
    private static func pump(until complete: () -> Bool) {
        let deadline = Date().addingTimeInterval(30)
        while !complete() {
            precondition(Date() < deadline, "store callback did not complete")
            RunLoop.main.run(until: Date().addingTimeInterval(0.002))
        }
    }
    private static func wait<T>(_ action: (@escaping (Result<T, Error>) -> Void) -> Void) throws -> T {
        var result: Result<T, Error>?
        action { result = $0 }
        pump { result != nil }
        return try result!.get()
    }
    private static func line(_ label: String, _ about: AboutStatusProvider) -> String {
        let matches = about.capture().lines.filter { $0.label == label }
        precondition(matches.count == 1)
        return matches[0].value
    }
    private static func items(_ view: ViewState) -> [String] { view.candidates?.items.map(\.text) ?? [] }
    private static func checkConversion(_ reading: String, expected: String) throws {
        let session = StoreSession()
        let view = try session.convert(reading)
        guard let candidate = view.candidates?.items.first(where: { $0.text == expected }) else {
            preconditionFailure("persisted layer did not reach conversion: \(reading) / \(items(view))")
        }
        _ = try session.send(CommandRequest(command: "select_candidate", arg: .candidate(id: candidate.id)))
        let committed = try session.send(CommandRequest(command: "commit"))
        precondition(committed.commitText == expected)
    }
    private static func upsert(_ user: UserDictionaryStore, surface: String, replacing: Int?) throws {
        try wait { user.upsert(reading: "あび", surface: surface, partOfSpeech: .init(rawValue: "NOUN")!,
            priority: .high, replacing: replacing, completion: $0) }
        pump { !user.isBusy }
    }
    private static func install(_ imported: ImportedDictionaryStore, source: URL) throws {
        let prepared = try wait { imported.prepareDictionaryFile(at: source, completion: $0) }
        try wait { imported.install(prepared, completion: $0) }
        pump { !imported.isBusy }
    }
    private static func update(_ postal: PostalCodeStore) throws {
        try wait { postal.updateFromJapanPost(completion: $0) }
        pump { !postal.isUpdating }
    }
    private static func checkSaved(user: UserDictionaryStore, imported: ImportedDictionaryStore, postal: PostalCodeStore,
                                   fixtures: URL, userSurface: String) throws {
        precondition(user.canEdit && user.entries.count == 1 && user.entries[0].surface == userSurface)
        precondition(imported.startupState == .ready && imported.view?.dictionaries?.count == 1)
        let entry = imported.view!.dictionaries![0]
        let sourceHash = SHA256.hash(data: try Data(contentsOf: fixtures.appendingPathComponent("one.muudict")))
            .map { String(format: "%02x", $0) }.joined()
        precondition(entry.metadata.id == "org.example.abi" && entry.entryCount == 1 && entry.enabled)
        precondition(entry.sourceSHA256 == sourceHash)
        precondition(postal.startupState == .ready && postal.view?.manifest?.entryCount == 1)
        try checkConversion("abi", expected: userSurface)
        try checkConversion("abu", expected: "自作取り込み語")
        try checkConversion("1000002", expected: "東京都試験区新町")
        let missingImported = try StoreSession().convert("abe")
        let missingPostal = try StoreSession().convert("1000003")
        precondition(!items(missingImported).contains("中断取り込み語"))
        precondition(!items(missingPostal).contains("東京都試験区次町"))
    }

    // What the legacy catalog in test-store-abi.py holds: one dictionary with
    // its six metadata fields, one entry, imported_at_unix_ms 123, and the
    // SHA-256 of one.muudict. Migration must carry all of them unchanged.
    private static func checkLegacy(_ imported: ImportedDictionaryStore, fixtures: URL, enabled: Bool) throws {
        precondition(imported.view?.dictionaries?.count == 1)
        let entry = imported.view!.dictionaries![0]
        precondition(entry.metadata == MuuDictionaryMetadata(id: "org.example.abi", name: "接続の検体", version: "1",
            author: "自作", license: "CC0", provenance: "自作のテスト"))
        precondition(entry.entryCount == 1 && entry.enabled == enabled)
        precondition(entry.importedAtUnixMilliseconds == 123)
        let sourceHash = SHA256.hash(data: try Data(contentsOf: fixtures.appendingPathComponent("one.muudict")))
            .map { String(format: "%02x", $0) }.joined()
        precondition(entry.sourceSHA256 == sourceHash)
    }

    // The real policy with injected reads: unset and false keep the model off,
    // true defaults to zenz, and an explicit edge selects edge. The recorded
    // reads must keep the production order, and nothing may be created.
    private static func factory(root: URL, dictionary: String) throws {
        for (enabled, model) in [(nil, nil), (false, nil), (true, nil), (true, "edge")] as [(Bool?, String?)] {
            var queries: [String] = []
            let configuration = RustCore.configurationForTesting(userDictionaryFileState: .missing,
                boolSetting: { key in queries.append(key); return ["MuuConversionLmEnabled": enabled].compactMapValues { $0 }[key] ?? false },
                stringSetting: { key in queries.append(key); return ["MuuConversionLmModel": model].compactMapValues { $0 }[key] },
                support: { queries.append("support"); return root },
                dictionary: { queries.append("dictionary"); return dictionary },
                recorder: { queries.append("recorder"); return nil })
            precondition(queries == ["MuuConversionLmEnabled", "MuuConversionLmModel", "support", "dictionary", "recorder"])
            precondition(configuration.dictionaryPath == dictionary && configuration.commitStream == nil)
            precondition(configuration.conversionLmFallbackDirectory == nil,
                "the app never hides unavailable zenz behind an old edge cache")
            if enabled == true {
                precondition(configuration.conversionLmModel == (model == "edge" ? .edge : .zenz))
                precondition(configuration.conversionLmDirectory?.hasPrefix(root.path) == true)
            } else {
                precondition(configuration.conversionLmModel == nil && configuration.conversionLmDirectory == nil)
                precondition(configuration.conversionLmFallbackDirectory == nil)
            }
        }
        print("store ABI: factory passed")
    }
}
