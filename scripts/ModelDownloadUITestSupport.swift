import AppKit

// Capture every request. Normally hold it; the stalled-publication case
// serves a small synthetic body. An unexpected URL still cannot go online.
private final class AboutModelProtocol: URLProtocol {
    private static let lock = NSLock()
    private static var requests = 0
    private static var payload: Data?
    static var count: Int { lock.lock(); defer { lock.unlock() }; return requests }
    static func reset(payload: Data? = nil) { lock.lock(); requests = 0; self.payload = payload; lock.unlock() }
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        Self.lock.lock(); Self.requests += 1; let payload = Self.payload; Self.lock.unlock()
        guard let payload else { return }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 200,
            httpVersion: "HTTP/1.1", headerFields: nil)!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: payload)
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() { }
}

private extension NSAlert {
    @MainActor var buttonsByPosition: [NSButton] {
        // Observe the rendered alert without relayout or key changes: a test
        // must not repair the placement or default that it is checking.
        let content = window.contentView!
        let frames = buttons.map { content.convert($0.bounds, from: $0) }
        if let first = frames.first {
            precondition(frames.allSatisfy { $0.width > 0 && abs($0.midY - first.midY) < 1 },
                "the tested alert must have one horizontal button row")
        }
        return buttons.sorted {
            content.convert($0.bounds, from: $0).minX < content.convert($1.bounds, from: $1).minX
        }
    }
}

@MainActor enum ModelDownloadUITests {
    // The real presenter against an offline store. Opening and reopening About
    // reuse one window and send no request; Enter is wired to Close and closes
    // without a request. Only the first button starts an explicit GET, which keeps
    // running after Close and is seen again on reopen. The synchronous input
    // session works while that request is held; cancel offers Retry, whose
    // actual button starts a new GET.
    static func run() throws {
        runtimeFacts()
        presentation()
        projectionCombinations()
        let fm = FileManager.default
        let root = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent("build/about-download-\(UUID())")
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: root) }
        AboutModelProtocol.reset()
        try noticeTable(root)
        try preservesOtherStatusLines(root)
        let store = ModelDownloadStore.forTesting(support: root, bytes: Data("synthetic".utf8), configuration: {
            let configuration = ModelDownloadStore.configurationForTesting()
            configuration.protocolClasses = [AboutModelProtocol.self]
            return configuration
        })
        store.start(); wait { store.cache == .missing }
        var captures = 0
        let presenter = AboutPresenter.forTesting(store: store) {
            captures += 1
            return .init(lines: [.init(label: "Core", value: "利用可"),
                .init(label: "言語モデル", value: "なし", needsAttention: true)],
                modelAttribution: AboutStatusSnapshot.attribution(for: nil), downloadRuntime: .unavailable)
        }
        presenter.show()
        let first = presenter.alertForTesting!
        precondition(AboutModelProtocol.count == 0 && captures == 1)
        precondition(first.buttonsByPosition.count == 2 && first.buttonsByPosition[1].title == "閉じる")
        precondition(first.buttonsByPosition[1].keyEquivalent == "\r" && first.buttonsByPosition[0].keyEquivalent.isEmpty)
        precondition(first.window.defaultButtonCell === first.buttonsByPosition[1].cell)
        precondition(first.buttonsByPosition.map(\.title) == ["取得", "閉じる"])
        precondition((first.accessoryView as! NSTextField).stringValue.contains("取得後、次回の Muu IME の起動から使用します。"))
        let escape = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
            windowNumber: first.window.windowNumber, context: nil, characters: "\u{1b}",
            charactersIgnoringModifiers: "\u{1b}", isARepeat: false, keyCode: 53)!
        precondition(!first.window.performKeyEquivalent(with: escape))
        precondition(presenter.alertForTesting === first && AboutModelProtocol.count == 0)
        presenter.show()
        precondition(presenter.alertForTesting === first && captures == 1 && AboutModelProtocol.count == 0)
        // Exercise the actual key-equivalent dispatch, not an alternate test
        // action: Enter reaches Close and never the acquisition action.
        let enter = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
            windowNumber: first.window.windowNumber, context: nil, characters: "\r",
            charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36)!
        precondition(first.window.performKeyEquivalent(with: enter))
        precondition(presenter.alertForTesting == nil && AboutModelProtocol.count == 0)
        presenter.show()
        presenter.alertForTesting!.buttonsByPosition[0].performClick(nil)
        wait { AboutModelProtocol.count == 1 }
        precondition(presenter.alertForTesting!.buttonsByPosition[0].title == "中止")
        presenter.alertForTesting!.buttonsByPosition[1].performClick(nil)
        precondition(presenter.alertForTesting == nil && store.onChange == nil)
        presenter.show()
        precondition(AboutModelProtocol.count == 1 && presenter.alertForTesting!.buttonsByPosition[0].title == "中止")
        // A held URLSession leaves the real synchronous input session free.
        let engine = RustEngine()
        for character in ["n", "a"] {
            let reply = engine.handle(EventRequest(key: .init(eventType: "key_down", keycode: 0, modifiers: 0,
                charactersUTF8: character, charactersIgnoringModifiers: character, isRepeat: false)))
            precondition(reply.status == 0)
        }
        let converted = engine.handle(CommandRequest(command: "convert"))
        precondition(converted.status == 0 && converted.view?.preedit.map(\.text).joined() == "な")
        let committed = engine.handle(CommandRequest(command: "commit"))
        precondition(committed.status == 0 && committed.view?.commitText == "な")
        presenter.alertForTesting!.buttonsByPosition[0].performClick(nil)
        wait { store.state == .cancelled }
        precondition(presenter.alertForTesting!.buttonsByPosition[0].title == "再試行" && AboutModelProtocol.count == 1)
        presenter.alertForTesting!.buttonsByPosition[0].performClick(nil)
        wait { AboutModelProtocol.count == 2 }
        precondition(presenter.alertForTesting!.buttonsByPosition[0].title == "中止")
        presenter.alertForTesting!.buttonsByPosition[0].performClick(nil)
        wait { store.state == .cancelled }
        precondition(presenter.alertForTesting!.buttonsByPosition[0].title == "再試行")
        presenter.alertForTesting!.buttonsByPosition[1].performClick(nil)
        // With an edge Core running, About is clean until the cancelled
        // download row is added; then the menu shows the mark.
        let provider = AboutStatusProvider(query: {
            var runtime = CoreRuntimeStatus(schemaVersion: 2, systemDictionary: .available,
                languageModel: .available, commitStreamEnabled: false)
            runtime.languageModelDetails = .init(requestedModel: .edge, actualModel: .edge,
                route: .requested, fallbackReason: nil, unavailableReason: nil)
            return .success(runtime)
        })
        provider.configure(initialization: .ready, build: .unknown)
        precondition(!provider.capture().needsAttention)
        provider.configureModelDownload(store)
        let menu = provider.capture()
        precondition(menu.needsAttention && menu.menuTitle == "⚠︎ About Muu IME")
        precondition(menu.text.contains("モデルの取得: 中止しました") && menu.text.contains("名称: kanakanji-edge-33m"))
        try cancelButtonDuringPublication(root)
        try restartButtonAfterCompletion(root)
        print("About download: open/Enter do not fetch; one nonmodal window, explicit acquire/cancel, reopen and input passed")
    }

    // Adding the download row keeps every other row and its mark as it was. A
    // healthy download adds no warning of its own, so the menu mark is an OR:
    // set only if some row, here the recorder, needs attention.
    private static func preservesOtherStatusLines(_ root: URL) throws {
        let support = root.appendingPathComponent("independent-warning")
        let file = support.appendingPathComponent("MuuIME/Models/zenz-v3.2-small/fixture.gguf")
        let bytes = Data("healthy synthetic model".utf8)
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        try bytes.write(to: file)
        let store = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: {
            let configuration = ModelDownloadStore.configurationForTesting()
            configuration.protocolClasses = [AboutModelProtocol.self]
            return configuration
        }, preferences: .forTesting(boolSetting: { $0 == "MuuConversionLmEnabled" },
            stringSetting: { $0 == "MuuConversionLmModel" ? "zenz" : nil }))
        store.start(); wait { store.cache == .valid }

        let original = AboutStatusSnapshot(lines: [
            .init(label: "Core", value: "利用可"),
            .init(label: "言語モデル", value: "zenz-v3.2-small（利用中）"),
            .init(label: "記録", value: "保存に失敗", needsAttention: true),
        ], modelAttribution: AboutStatusSnapshot.attribution(for: .zenz), downloadRuntime: .zenz)
        let combined = original.addingDownload(store)
        // A healthy download has no warning of its own. Preserve every prior
        // row and its attention fact through the real composition method.
        precondition(combined.lines.count == original.lines.count + 1)
        for (before, after) in zip(original.lines, combined.lines) {
            precondition(after.label == before.label && after.value == before.value && after.needsAttention == before.needsAttention)
        }
        precondition(combined.textWithoutDownload == original.text)
        precondition(combined.lines.last?.label == "モデルの取得" && combined.lines.last?.value == "取得済み・利用中")
        precondition(combined.lines.last?.needsAttention == false)
        precondition(combined.needsAttention && combined.menuTitle == "⚠︎ About Muu IME")
        var healthy = original
        healthy.lines.removeLast()
        precondition(!healthy.addingDownload(store).needsAttention && AboutModelProtocol.count == 0)
    }

    // The real 中止 button, pressed while the worker is stalled at `rename`,
    // must return to main within 2 s, and the attempt then ends cancelled
    // with no file.
    private static func cancelButtonDuringPublication(_ root: URL) throws {
        let support = root.appendingPathComponent("stalled-button")
        try FileManager.default.createDirectory(at: support, withIntermediateDirectories: false)
        let bytes = Data("synthetic".utf8)
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        AboutModelProtocol.reset(payload: bytes)
        let store = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: {
            let configuration = ModelDownloadStore.configurationForTesting()
            configuration.protocolClasses = [AboutModelProtocol.self]
            return configuration
        }, checkpoint: { phase in
            if phase == "rename" {
                entered.signal()
                precondition(release.wait(timeout: .now() + 10) == .success)
            }
        })
        let presenter = AboutPresenter.forTesting(store: store) {
            .init(lines: [], downloadRuntime: .unavailable)
        }
        store.start(); wait { store.cache == .missing }
        presenter.show()
        presenter.alertForTesting!.buttonsByPosition[0].performClick(nil)
        precondition(entered.wait(timeout: .now() + 2) == .success)
        DispatchQueue.global().asyncAfter(deadline: .now() + 3) { release.signal() }
        let before = ProcessInfo.processInfo.systemUptime
        presenter.alertForTesting!.buttonsByPosition[0].performClick(nil)
        let elapsed = ProcessInfo.processInfo.systemUptime - before
        precondition(elapsed < 2, "the actual Cancel button waited for publication IO")
        print("About Cancel during three-second publication stall: \(elapsed * 1000) ms")
        presenter.alertForTesting!.buttonsByPosition[1].performClick(nil)
        wait { store.state == .cancelled }
        precondition(!FileManager.default.fileExists(atPath: support.appendingPathComponent("MuuIME/Models/zenz-v3.2-small/fixture.gguf").path))
    }

    // The real buttons against a synthetic completed download. The injected
    // terminator keeps this test process alive and counts normal quit requests.
    // Return is ignored while restart is pending; Escape closes. A second click on Muu IME を再起動
    // sends no second request; and with zenz already in use there is no
    // restart button or notice at all.
    private static func restartButtonAfterCompletion(_ root: URL) throws {
        let support = root.appendingPathComponent("restart-button")
        try FileManager.default.createDirectory(at: support, withIntermediateDirectories: false)
        let bytes = Data("synthetic".utf8)
        AboutModelProtocol.reset(payload: bytes)
        let store = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: {
            let configuration = ModelDownloadStore.configurationForTesting()
            configuration.protocolClasses = [AboutModelProtocol.self]
            return configuration
        })
        var terminations = 0
        let presenter = AboutPresenter.forTesting(store: store, terminator: { terminations += 1 }) {
            .init(lines: [], downloadRuntime: .unavailable)
        }
        store.start(); wait { store.cache == .missing }
        presenter.show()
        precondition(presenter.alertForTesting!.buttonsByPosition[0].title == "取得" && terminations == 0)
        let acquiring = presenter.alertForTesting!
        acquiring.buttonsByPosition[0].performClick(nil)
        wait { store.state == .completed }
        let completed = presenter.alertForTesting!
        precondition(completed !== acquiring && !acquiring.window.isVisible && store.onChange != nil)
        // Queued actions from the replaced window cannot operate on its successor.
        acquiring.buttonsByPosition[0].performClick(nil)
        acquiring.buttonsByPosition[1].performClick(nil)
        precondition(presenter.alertForTesting === completed && terminations == 0 && store.onChange != nil)
        let startupNotice = "取得後、次回の Muu IME の起動から使用します。"
        let completedText = (completed.accessoryView as! NSTextField).stringValue
        precondition(completed.buttonsByPosition[0].title == "Muu IME を再起動" && completed.buttonsByPosition[0].isEnabled)
        precondition(AboutModelProtocol.count == 1 && terminations == 0)
        precondition((completed.accessoryView as? NSTextField)?.stringValue.contains(
            "Muu IME を終了します。次の入力から zenz を使います") == true)
        precondition(completed.buttonsByPosition.map(\.title) == ["Muu IME を再起動"])
        precondition(completed.buttonsByPosition[0].keyEquivalent.isEmpty && completed.window.defaultButtonCell == nil)
        let enter = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
            windowNumber: completed.window.windowNumber, context: nil, characters: "\r",
            charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36)!
        precondition(!completed.window.performKeyEquivalent(with: enter))
        NSApp.sendEvent(enter)
        precondition(presenter.alertForTesting === completed && terminations == 0 && AboutModelProtocol.count == 1)
        sendEscape(to: completed)
        precondition(presenter.alertForTesting == nil && terminations == 0)
        presenter.show()
        let restart = presenter.alertForTesting!.buttonsByPosition[0]
        precondition(restart.title == "Muu IME を再起動")
        restart.performClick(nil)
        precondition(terminations == 1)
        restart.performClick(nil)
        precondition(terminations == 1 && AboutModelProtocol.count == 1 && store.state == .completed)
        sendEscape(to: presenter.alertForTesting!)
        let inUse = AboutPresenter.forTesting(store: store, terminator: { terminations += 1 }) {
            .init(lines: [], downloadRuntime: .zenz)
        }
        inUse.show()
        let used = inUse.alertForTesting!
        let usedText = (used.accessoryView as! NSTextField).stringValue
        precondition(!completedText.contains(startupNotice) && !usedText.contains(startupNotice),
            "obsolete startup hint: completed=\(completedText.contains(startupNotice)), in_use=\(usedText.contains(startupNotice))")
        precondition(used.buttonsByPosition.map(\.title) == ["閉じる"])
        precondition(used.window.defaultButtonCell === used.buttonsByPosition[0].cell)
        precondition((used.accessoryView as? NSTextField)?.stringValue.contains("Muu IME を終了します。次の入力から zenz を使います") == false)
        used.buttonsByPosition[0].performClick(nil)
        precondition(inUse.alertForTesting == nil && terminations == 1 && AboutModelProtocol.count == 1)
        var enabled = false
        var model: String?
        let preferences = ModelDownloadPreferences.forTesting(
            boolSetting: { $0 == "MuuConversionLmEnabled" && enabled },
            stringSetting: { $0 == "MuuConversionLmModel" ? model : nil },
            setString: { value, _ in model = value }, setBool: { value, _ in enabled = value })
        let cachedStore = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: {
            let configuration = ModelDownloadStore.configurationForTesting()
            configuration.protocolClasses = [AboutModelProtocol.self]
            return configuration
        }, preferences: preferences)
        cachedStore.start(); wait { cachedStore.cache == .valid }
        precondition(!cachedStore.isConfiguredForZenz)
        let cached = AboutPresenter.forTesting(store: cachedStore) {
            .init(lines: [], downloadRuntime: .zenzFailed(.loadFailed, fallbackAvailable: true))
        }
        cached.show()
        precondition(cached.alertForTesting!.buttonsByPosition.map(\.title) == ["使用する", "閉じる"])
        precondition((cached.alertForTesting!.accessoryView as! NSTextField).stringValue.contains("言語モデルを読み込めませんでした"))
        cached.alertForTesting!.buttonsByPosition[0].performClick(nil)
        wait { cachedStore.state == .completed }
        precondition(cachedStore.isConfiguredForZenz && AboutModelProtocol.count == 1 && terminations == 1)
        precondition(cached.alertForTesting!.buttonsByPosition[0].title == "Muu IME を再起動")
        sendEscape(to: cached.alertForTesting!)

        // A cache that arrived after Core's startup also permits restart from
        // idle. zenz's not_found wins over a failed fallback edge model.
        let idleStore = ModelDownloadStore.forTesting(support: support, bytes: bytes, configuration: {
            let configuration = ModelDownloadStore.configurationForTesting()
            configuration.protocolClasses = [AboutModelProtocol.self]
            return configuration
        }, preferences: preferences)
        idleStore.start(); wait { idleStore.cache == .valid }
        var core = CoreRuntimeStatus(schemaVersion: 2, systemDictionary: .available,
            languageModel: .failed, commitStreamEnabled: false)
        core.languageModelDetails = .init(requestedModel: .zenz, actualModel: nil, route: .unavailable,
            fallbackReason: .notFound, unavailableReason: .loadFailed)
        let idlePresenter = AboutPresenter.forTesting(store: idleStore, terminator: { terminations += 1 }) {
            .init(lines: [], downloadRuntime: .from(core))
        }
        idlePresenter.show()
        let pending = idlePresenter.alertForTesting!
        precondition(idleStore.state == .idle && pending.buttonsByPosition.map(\.title) == ["Muu IME を再起動"])
        precondition((pending.accessoryView as! NSTextField).stringValue.contains("取得済み（再起動待ち）"))
        pending.buttonsByPosition[0].performClick(nil)
        pending.buttonsByPosition[0].performClick(nil)
        precondition(terminations == 2 && AboutModelProtocol.count == 1)
        sendEscape(to: pending)
        print("About restart: Return ignored while pending; Escape closes; explicit clicks terminate once")
    }

    private static func sendEscape(to alert: NSAlert) {
        let event = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
            windowNumber: alert.window.windowNumber, context: nil, characters: "\u{1b}",
            charactersIgnoringModifiers: "\u{1b}", isARepeat: false, keyCode: 53)!
        NSApp.sendEvent(event)
    }

    private static func wait(_ condition: () -> Bool) {
        let deadline = Date().addingTimeInterval(10)
        while !condition() && Date() < deadline { RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01)) }
        precondition(condition(), "offline About hang guard")
    }

    // Preserve the requested model's failure across both successful and failed
    // edge fallbacks. In the latter case Core's overall failed is not zenz's
    // reason: a newly arrived cache can repair not_found, but not load_failed.
    private static func runtimeFacts() {
        typealias Case = (name: String, overall: CoreRuntimeStatus.LanguageModel,
                          requested: ConversionLmModel?, actual: ConversionLmModel?,
                          route: CoreRuntimeStatus.ModelDetails.Route,
                          fallback: CoreRuntimeStatus.ModelDetails.Failure?,
                          unavailable: CoreRuntimeStatus.ModelDetails.Failure?, expected: ModelDownloadRuntime)
        let cases: [Case] = [
            ("disabled", .unloaded, nil, nil, .disabled, nil, nil, .unavailable),
            ("loading", .loading, .zenz, nil, .loading, nil, nil, .loading),
            ("zenz in use", .available, .zenz, .zenz, .requested, nil, nil, .zenz),
            ("explicit edge", .available, .edge, .edge, .requested, nil, nil, .other),
            ("explicit edge failed", .failed, .edge, nil, .unavailable, nil, .loadFailed, .unavailable),
            ("direct not found", .failed, .zenz, nil, .unavailable, nil, .notFound, .zenzFailed(.notFound, fallbackAvailable: false)),
            ("direct load failed", .failed, .zenz, nil, .unavailable, nil, .loadFailed, .zenzFailed(.loadFailed, fallbackAvailable: false)),
            ("not found, edge available", .available, .zenz, .edge, .fallback, .notFound, nil, .zenzFailed(.notFound, fallbackAvailable: true)),
            ("load failed, edge available", .available, .zenz, .edge, .fallback, .loadFailed, nil, .zenzFailed(.loadFailed, fallbackAvailable: true)),
            ("both load failed", .failed, .zenz, nil, .unavailable, .loadFailed, .loadFailed, .zenzFailed(.loadFailed, fallbackAvailable: false)),
            ("not found, edge load failed", .failed, .zenz, nil, .unavailable, .notFound, .loadFailed, .zenzFailed(.notFound, fallbackAvailable: false)),
            ("unspecified failure", .failed, nil, nil, .unavailable, nil, nil, .failed),
        ]
        precondition(ModelDownloadRuntime.from(nil) == .unknown)
        for item in cases {
            var core = CoreRuntimeStatus(schemaVersion: 2, systemDictionary: .available,
                languageModel: item.overall, commitStreamEnabled: false)
            core.languageModelDetails = .init(requestedModel: item.requested, actualModel: item.actual,
                route: item.route, fallbackReason: item.fallback, unavailableReason: item.unavailable)
            let runtime = ModelDownloadRuntime.from(core)
            precondition(runtime == item.expected, "lost startup fact for \(item.name): \(runtime)")
            precondition(runtime.hasOtherModel == (item.actual == .edge))
        }
        print("About Core facts: \(cases.count) startup routes plus unavailable query passed")
    }

    // Exercise every combination of cache, acquisition, current setting and
    // reduced Core fact. Exact rows/controls are checked on real alerts below;
    // this larger cross-product guards their coupling even for stale facts.
    private static func projectionCombinations() {
        let caches: [ModelDownloadStore.Cache] = [.checking, .missing, .unavailable(.hash), .valid]
        let states: [ModelDownloadStore.State] = [.idle, .receiving(42), .verifying,
            .failed(.http), .failed(.hash), .failed(.durability), .cancelled, .completed]
        let runtimes: [ModelDownloadRuntime] = [.unknown, .unavailable, .failed, .loading, .zenz, .other,
            .zenzFailed(.notFound, fallbackAvailable: false), .zenzFailed(.notFound, fallbackAvailable: true),
            .zenzFailed(.loadFailed, fallbackAvailable: false), .zenzFailed(.loadFailed, fallbackAvailable: true)]
        var count = 0
        for cache in caches { for state in states { for configured in [false, true] { for runtime in runtimes {
            let row = ModelDownloadPresentation.make(cache: cache, state: state, configured: configured, runtime: runtime)
            let pending = row.line.value.contains("再起動待ち")
            precondition(pending == (row.buttons == .restartOnly))
            precondition(pending == (row.action == .restart))
            precondition(pending == (row.notice == "Muu IME を終了します。次の入力から zenz を使います"))
            if pending { precondition(cache == .valid && runtime != .zenz && row.line.needsAttention) }
            if runtime == .zenz {
                precondition(row.line.value == "取得済み・利用中" && row.buttons == .closeOnly && row.notice == nil)
            }
            count += 1
        } } } }
        precondition(count == 640)
        print("About projection: \(count) cache/acquisition/setting/Core combinations agree on row, notice and controls")
    }

    // Guidance and controls agree with the literal row in each state. Restart
    // is possible for a valid idle cache as well as an explicit acquisition.
    private static func noticeTable(_ root: URL) throws {
        let acquisition = "取得後、次回の Muu IME の起動から使用します。"
        let restart = "Muu IME を終了します。次の入力から zenz を使います"
        typealias Case = (name: String, cache: ModelDownloadStore.Cache, state: ModelDownloadStore.State,
                          configured: Bool, runtime: ModelDownloadRuntime, line: String, notice: String?,
                          titles: [String], defaultIndex: Int?, escapeCloses: Bool)
        let cases: [Case] = [
            ("checking", .checking, .idle, false, .unknown, "確認中", nil, ["取得", "閉じる"], 1, false),
            ("missing", .missing, .idle, false, .unavailable, "未取得（約74 MB）", acquisition, ["取得", "閉じる"], 1, false),
            ("receiving", .missing, .receiving(42), false, .unavailable, "取得中（42%）", acquisition, ["中止", "閉じる"], 1, false),
            ("verifying", .missing, .verifying, false, .unavailable, "照合中", acquisition, ["中止", "閉じる"], 1, false),
            ("failed", .missing, .failed(.http), false, .unavailable, "サーバーから取得できませんでした", acquisition, ["再試行", "閉じる"], 1, false),
            ("cancelled", .missing, .cancelled, false, .unavailable, "中止しました", acquisition, ["再試行", "閉じる"], 1, false),
            ("unusable cache", .unavailable(.hash), .idle, false, .unavailable, "照合に失敗しました", acquisition, ["再試行", "閉じる"], 1, false),
            ("unused", .valid, .idle, false, .unavailable, "取得済み（未使用）", nil, ["使用する", "閉じる"], 1, false),
            ("completed", .valid, .completed, true, .unavailable, "完了（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("loading", .valid, .idle, true, .loading, "取得済み（読み込み中）", nil, ["取得済み", "閉じる"], 1, false),
            ("unknown", .valid, .idle, true, .unknown, "取得済み（利用状態不明）", nil, ["取得済み", "閉じる"], 1, false),
            ("in use", .valid, .idle, true, .zenz, "取得済み・利用中", nil, ["閉じる"], 0, false),
            ("completed in use", .valid, .completed, true, .zenz, "取得済み・利用中", nil, ["閉じる"], 0, false),
            ("awaiting next launch", .valid, .idle, true, .unavailable, "取得済み（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("other Core, now configured", .valid, .idle, true, .other, "取得済み（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("explicit edge", .valid, .idle, false, .other, "取得済み（未使用）", nil, ["使用する", "閉じる"], 1, false),
            ("durability failure in use", .valid, .failed(.durability), true, .zenz, "取得済み・利用中", nil, ["閉じる"], 0, false),
            ("completed other Core", .valid, .completed, true, .other, "完了（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("not found, cache arrived", .valid, .idle, true, .zenzFailed(.notFound, fallbackAvailable: false), "取得済み（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("not found, edge running", .valid, .idle, true, .zenzFailed(.notFound, fallbackAvailable: true), "取得済み（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("not found, still missing", .missing, .idle, true, .zenzFailed(.notFound, fallbackAvailable: false), "未取得（約74 MB）", acquisition, ["取得", "閉じる"], 1, false),
            ("not found, invalid", .unavailable(.hash), .idle, true, .zenzFailed(.notFound, fallbackAvailable: false), "照合に失敗しました", acquisition, ["再試行", "閉じる"], 1, false),
            ("not found, acquired", .valid, .completed, true, .zenzFailed(.notFound, fallbackAvailable: false), "完了（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("load failed, same cache", .valid, .idle, true, .zenzFailed(.loadFailed, fallbackAvailable: false), "言語モデルを読み込めませんでした", nil, ["取得済み", "閉じる"], 1, false),
            ("load failed, edge running", .valid, .idle, true, .zenzFailed(.loadFailed, fallbackAvailable: true), "言語モデルを読み込めませんでした", nil, ["取得済み", "閉じる"], 1, false),
            ("load failed, acquired", .valid, .completed, true, .zenzFailed(.loadFailed, fallbackAvailable: false), "完了（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("load failed, acquired with edge", .valid, .completed, true, .zenzFailed(.loadFailed, fallbackAvailable: true), "完了（再起動待ち）", restart, ["Muu IME を再起動"], nil, true),
            ("unspecified failure", .valid, .idle, true, .failed, "言語モデルを読み込めませんでした", nil, ["取得済み", "閉じる"], 1, false),
        ]
        let support = root.appendingPathComponent("button-table")
        try FileManager.default.createDirectory(at: support, withIntermediateDirectories: false)
        let store = ModelDownloadStore.forTesting(support: support, bytes: Data("table".utf8), configuration: {
            let configuration = ModelDownloadStore.configurationForTesting()
            configuration.protocolClasses = [AboutModelProtocol.self]
            return configuration
        })
        store.start(); wait { store.cache == .missing }
        var terminations = 0
        let presenter = AboutPresenter.forTesting(store: store, terminator: { terminations += 1 }) {
            .init(lines: [], downloadRuntime: .unavailable)
        }
        for item in cases {
            let row = ModelDownloadPresentation.make(cache: item.cache, state: item.state,
                configured: item.configured, runtime: item.runtime)
            presenter.show()
            presenter.renderForTesting(row)
            let alert = presenter.alertForTesting!
            precondition(alert.buttonsByPosition.map(\.title) == item.titles,
                "\(item.name): line=\(row.line.value), actual_buttons=\(alert.buttonsByPosition.map(\.title))")
            if ["missing", "completed", "in use"].contains(item.name) {
                let positions = alert.buttonsByPosition.map { button in
                    "\(button.title):\(alert.window.contentView!.convert(button.bounds, from: button).minX)"
                }
                print("About button positions: \(item.name) left_to_right=\(positions) default=\(String(describing: item.defaultIndex))")
            }
            precondition(row.notice == item.notice, "unexpected notice for \(item.name)")
            precondition(row.line.value == item.line, "unexpected row for \(item.name): \(row.line.value)")
            precondition((alert.accessoryView as! NSTextField).stringValue.contains("モデルの取得: " + item.line))
            precondition(store.onChange != nil)
            for (index, button) in alert.buttonsByPosition.enumerated() {
                precondition(button.keyEquivalent == (index == item.defaultIndex ? "\r" : ""))
            }
            let enter = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                windowNumber: alert.window.windowNumber, context: nil, characters: "\r",
                charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36)!
            if let index = item.defaultIndex {
                precondition(alert.window.defaultButtonCell === alert.buttonsByPosition[index].cell)
                precondition(alert.window.performKeyEquivalent(with: enter))
                precondition(presenter.alertForTesting == nil)
            } else {
                precondition(alert.window.defaultButtonCell == nil)
                precondition(!alert.window.performKeyEquivalent(with: enter))
                NSApp.sendEvent(enter)
                precondition(presenter.alertForTesting === alert)
            }
            presenter.show()
            presenter.renderForTesting(row)
            let escaped = presenter.alertForTesting!
            sendEscape(to: escaped)
            precondition((presenter.alertForTesting == nil) == item.escapeCloses, "unexpected Escape for \(item.name)")
            if !item.escapeCloses { escaped.buttonsByPosition.last!.performClick(nil) }
            precondition(presenter.alertForTesting == nil && store.onChange == nil)
            precondition(AboutModelProtocol.count == 0 && terminations == 0)
        }
        print("About state table: \(cases.count) real alerts, button titles/default/Return/Escape and guidance passed")
    }

    // The row rules as a table: button and action per state, and the mark,
    // which stays on after completion until the running Core uses zenz.
    // With zenz in use the dialog offers only Close. Missing or invalid data
    // still contributes its independent warning to the menu's OR.
    private static func presentation() {
        func row(_ cache: ModelDownloadStore.Cache, _ state: ModelDownloadStore.State = .idle,
                 _ runtime: ModelDownloadRuntime = .unavailable, configured: Bool = false) -> ModelDownloadPresentation {
            .make(cache: cache, state: state, configured: configured, runtime: runtime)
        }
        let missing = row(.missing)
        precondition(missing.line.needsAttention && missing.button == "取得" && missing.action == .acquire)
        let downloading = row(.missing, .receiving(42))
        precondition(downloading.line.value == "取得中（42%）" && downloading.line.needsAttention && downloading.action == .cancel)
        precondition(row(.missing, .verifying).button == "中止")
        precondition(row(.missing, .failed(.hash)).line.value == "照合に失敗しました")
        precondition(row(.missing, .failed(.http)).button == "再試行")
        let complete = row(.valid, .completed)
        precondition(complete.line.needsAttention && complete.action == .restart && complete.button == "Muu IME を再起動" && complete.line.value == "完了（再起動待ち）")
        precondition(row(.valid, .completed, .other, configured: true).line.needsAttention)
        for state in [ModelDownloadStore.State.idle, .receiving(42), .verifying, .failed(.hash), .cancelled] {
            let other = row(.valid, state)
            precondition(other.button != "Muu IME を再起動" && other.action != .restart)
        }
        let inUse = row(.valid, .completed, .zenz, configured: true)
        precondition(inUse.button == "取得済み" && inUse.action == .none && !inUse.line.needsAttention)
        precondition(!row(.valid, .idle, .zenz, configured: true).line.needsAttention)
        precondition(!row(.valid, .idle, .loading, configured: true).line.needsAttention)
        precondition(!row(.checking, .idle, .unknown).line.needsAttention)
        precondition(row(.valid).button == "使用する")
        precondition(row(.missing, .idle, .other).line.needsAttention)
        precondition(row(.valid, .failed(.durability), .zenz).line.needsAttention)
        let base = AboutStatusSnapshot(lines: [.init(label: "記録", value: "停止", needsAttention: true)])
        precondition(base.needsAttention && base.menuTitle == "⚠︎ About Muu IME")
        precondition(AboutStatusSnapshot.attribution(for: nil).contains("名称: zenz-v3.2-small"))
        precondition(AboutStatusSnapshot.attribution(for: .edge).contains("名称: kanakanji-edge-33m"))
    }
}
