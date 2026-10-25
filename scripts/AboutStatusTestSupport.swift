import AppKit

@MainActor
enum AboutStatusTests {
    nonisolated private static let build = AboutBuildInfo(version: "0.1", build: "123", commit: String(repeating: "a", count: 40))
    nonisolated private static let healthy = AboutManagedDataStatus(started: true, access: "editable", layer: "active", count: 0)
    nonisolated private static let saved = PersonalFrequencyPersistenceStatus(initialized: true, stopped: false,
        stopReason: nil, fileState: .absent, lastSavedAt: nil, pending: false, saveIssue: nil, queryFailed: false)
    nonisolated private static let recording = CommitStreamRecorderStatus(requested: true, configured: true, stopped: false)

    static func run() throws {
        unavailableAcknowledgements()
        failureEnvelopes()
        try ModelDownloadUITests.run()
        try modelSelectionAndFallback()
        try recorderRetryingAfterDiscard()
        try matrix()
        try metadataAndSaveFacts()
        print("About: attention matrix, fresh snapshot, single alert, same-FD metadata and save outcomes passed")
    }

    // All eleven ACK closures of the three unavailable stubs return
    // CREATE_FAILED. A stub that sends nothing must report a refusal, never
    // an ambiguous value such as CORE_INVALID that would stop its store.
    private static func unavailableAcknowledgements() {
        let command = Data()
        let user = UserDictionaryCoreBridge.unavailable
        let imported = ImportedDictionaryCoreBridge.unavailable
        let postal = PostalCodeCoreBridge.unavailable
        let values = [user.publish(command), user.cancel(command), imported.publish(command), imported.rollback(command),
            imported.finish(command), imported.cancel(command), postal.publish(command), postal.rollback(command),
            postal.disable(command), postal.finish(command), postal.cancel(command)]
        precondition(values == Array(repeating: 3, count: 11), "unsent stub ACKs are CREATE_FAILED refusals")
    }

    // The shared failure decoder for each kind: empty, non-UTF-8, non-object
    // and wrongly typed bodies, a null pointer and an advertised length one past
    // the bound (with an invalid pointer, so a read would crash) all give the
    // fallback code. A body exactly at the bound is still decoded, and only
    // Imported keeps `line` and `field`.
    private static func failureEnvelopes() {
        typealias Kind = RustCore.FailureKindForTesting
        let cases: [(Kind, Int, String)] = [
            (.user, 32 * 1024 * 1024, "user_dictionary_operation_failed"),
            (.imported, 32 * 1024 * 1024, "imported_dictionary_operation_failed"),
            (.postal, 1024 * 1024, "core_operation_failed"),
            (.core, 16 * 1024 * 1024, "core_operation_failed"),
        ]
        func facts(_ error: Error) -> (Int32, String, Int?, String?) {
            if let e = error as? UserDictionaryOperationError { return (e.status, e.code, nil, nil) }
            if let e = error as? ImportedDictionaryOperationError { return (e.status, e.code, e.line, e.field) }
            if let e = error as? PostalCodeOperationError { return (e.status, e.code, nil, nil) }
            let e = error as! CoreOperationError
            return (e.status, e.code, nil, nil)
        }
        for (kind, limit, fallback) in cases {
            func check(_ output: MuuBuffer, _ expected: String, line: Int? = nil, field: String? = nil) {
                let actual = facts(RustCore.failureForTesting(kind, status: 6, output: output))
                precondition(actual.0 == 6 && actual.1 == expected && actual.2 == line && actual.3 == field)
            }
            func bytes(_ data: Data, _ expected: String, line: Int? = nil, field: String? = nil) {
                data.withUnsafeBytes {
                    check(MuuBuffer(data: UnsafeMutablePointer(mutating: $0.bindMemory(to: UInt8.self).baseAddress), len: $0.count),
                          expected, line: line, field: field)
                }
            }
            for data in [Data(), Data([0xff]), Data("[]".utf8), Data("{}".utf8), Data("{\"code\":1}".utf8)] {
                bytes(data, fallback)
            }
            check(MuuBuffer(data: nil, len: 1), fallback)
            // An invalid pointer is intentional: an oversized envelope must be
            // rejected before any read or allocation from the advertised size.
            check(MuuBuffer(data: UnsafeMutablePointer<UInt8>(bitPattern: 1), len: limit + 1), fallback)
            bytes(Data("{\"code\":\"fixture_reason\",\"params\":null}".utf8), "fixture_reason")
            bytes(Data("{\"code\":\"fixture_reason\",\"params\":{\"line\":7,\"field\":\"surface\"}}".utf8),
                  "fixture_reason", line: kind == .imported ? 7 : nil, field: kind == .imported ? "surface" : nil)
            bytes(Data("{\"code\":\"fixture_reason\",\"params\":{\"line\":\"bad\"}}".utf8), kind == .imported ? fallback : "fixture_reason")
            var atLimit = Data("{\"code\":\"at_limit\"}".utf8)
            atLimit.append(Data(repeating: 32, count: limit - atLimit.count))
            bytes(atLimit, "at_limit")
        }
    }

    // Model selection and how About reports it. Neither the default (zenz) nor an
    // explicit edge carries a fallback directory. If Core still reports running on a
    // fallback (the C ABI keeps that route for explicit callers), it is shown
    // with its reason but is not an attention state; no usable model at all is. A
    // schema-2 status decodes the model details, and a schema-2 status without them is
    // unsupported rather than read as "no details".
    private static func modelSelectionAndFallback() throws {
        let support = URL(fileURLWithPath: "/fixture/support", isDirectory: true)
        let disabled = ConversionLmConfiguration.resolve(enabled: false, modelSetting: nil, support: support)
        precondition(disabled.directory == nil && disabled.model == nil && disabled.fallbackDirectory == nil)
        let zenz = ConversionLmConfiguration.resolve(enabled: true, modelSetting: nil, support: support)
        // Full literal paths, not built from the profile. Configuration and both
        // download destinations must name the same zenz directory. An unknown
        // setting resolves to zenz, no support directory configures nothing,
        // and edge keeps its own directory.
        let zenzDirectory = "/fixture/support/MuuIME/Models/zenz-v3.2-small"
        precondition(zenz.model == .zenz && zenz.directory == zenzDirectory)
        precondition(ModelDownloadProfile.zenz.destination(under: support).path
            == zenzDirectory + "/ggml-model-Q5_K_M.gguf")
        precondition(ModelDownloadProfile.fixture(bytes: Data([1])).destination(under: support).path
            == zenzDirectory + "/fixture.gguf")
        precondition(ConversionLmConfiguration.resolve(enabled: true, modelSetting: "unknown", support: support).directory == zenzDirectory)
        precondition(ConversionLmConfiguration.resolve(enabled: true, modelSetting: nil, support: nil).directory == nil)
        precondition(zenz.fallbackDirectory == nil)
        let edge = ConversionLmConfiguration.resolve(enabled: true, modelSetting: "edge", support: support)
        precondition(edge.model == .edge && edge.fallbackDirectory == nil)
        precondition(edge.directory == "/fixture/support/MuuIME/Models/kanakanji-edge-33m")
        let config = CoreConfiguration(dictionaryPath: nil, conversionLmDirectory: zenz.directory, commitStream: nil,
            conversionLmModel: zenz.model, conversionLmFallbackDirectory: zenz.fallbackDirectory)
        let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(config)) as! [String: Any]
        precondition(json["conversion_lm_model"] as? String == "zenz")
        precondition(json["conversion_lm_fallback_dir"] as? String == zenz.fallbackDirectory)
        for failure in [CoreRuntimeStatus.ModelDetails.Failure.notFound, .loadFailed] {
            var loaded = runtime()
            loaded.languageModelDetails = .init(requestedModel: .zenz, actualModel: .edge, route: .fallback,
                fallbackReason: failure, unavailableReason: nil)
            let fallback = snapshot(runtime: .success(loaded))
            precondition(fallback.text.contains("kanakanji-edge-33m") && fallback.text.contains(failure.description))
            precondition(fallback.text.contains("代わりのモデルを使用") && !fallback.needsAttention)
            precondition(fallback.text.hasSuffix(AboutStatusSnapshot.attribution))
            loaded = runtime(.failed)
            loaded.languageModelDetails = .init(requestedModel: .zenz, actualModel: nil, route: .unavailable,
                fallbackReason: failure, unavailableReason: .notFound)
            let missing = snapshot(runtime: .success(loaded))
            precondition(missing.needsAttention && missing.text.contains("zenz-v3.2-small")
                && missing.text.contains("kanakanji-edge-33m") && missing.text.contains("名称: zenz-v3.2-small"))
        }
        var loaded = runtime()
        loaded.languageModelDetails = .init(requestedModel: .zenz, actualModel: .zenz, route: .requested,
            fallbackReason: nil, unavailableReason: nil)
        let active = snapshot(runtime: .success(loaded))
        precondition(active.text.contains("読み込み済み（zenz-v3.2-small）") && !active.needsAttention)
        precondition(active.text.contains("ライセンス: Apache-2.0（モデルカードの宣言）。tokenizer は ku-nlp/gpt2-small-japanese-char 由来（CC BY-SA 4.0）"))
        let wire = Data("""
        {"schema_version":2,"system_dictionary":"available","language_model":"available",
         "commit_stream_enabled":false,"language_model_details":{"requested_model":"zenz","actual_model":"edge",
         "route":"fallback","fallback_reason":"load_failed","unavailable_reason":null}}
        """.utf8)
        let decoded = try JSONDecoder().decode(CoreRuntimeStatus.self, from: wire)
        precondition(decoded.isSupported && decoded.languageModelDetails?.actualModel == .edge
            && decoded.languageModelDetails?.fallbackReason == .loadFailed)
        let missing = CoreRuntimeStatus(schemaVersion: 2, systemDictionary: .available, languageModel: .available, commitStreamEnabled: false)
        precondition(!missing.isSupported && runtime().isSupported)
    }

    nonisolated private static func runtime(_ model: CoreRuntimeStatus.LanguageModel = .available,
                                dictionary: CoreRuntimeStatus.SystemDictionary = .available) -> CoreRuntimeStatus {
        .init(schemaVersion: 1, systemDictionary: dictionary, languageModel: model, commitStreamEnabled: true)
    }

    // Every input defaults to healthy, so each matrix case changes one fact and the
    // attention result can be blamed on that fact alone.
    private static func snapshot(runtime: Result<CoreRuntimeStatus, CoreOperationError>? = .success(runtime()),
                                 initialization: AboutStatusProvider.Initialization = .ready,
                                 managed: AboutManagedDataStatus = healthy,
                                 learning: PersonalFrequencyPersistenceStatus? = saved,
                                 recorder: CommitStreamRecorderStatus? = recording,
                                 coreInvalid: Bool = false) -> AboutStatusSnapshot {
        AboutStatusProvider.snapshot(initialization: initialization, build: build, runtime: runtime,
            coreInvalid: coreInvalid, user: managed, imported: managed, postal: managed,
            learning: learning, recorder: recorder, formatDate: { "utc:\(Int($0.timeIntervalSince1970))" })
    }

    // The attention matrix: which facts raise the menu warning and which do not.
    // Transient states (loading, busy, retry cooldown, closing) and a failed status
    // query never do. Known faults do, and an unknown Core status must not hide a known
    // store fault. Store reasons can carry file text, so they are never shown (the
    // CANARY check). The last part: capture() queries Core afresh on every call and not
    // before configure(), and the real runtime status with no Core is kana-only and
    // unloaded.
    private static func matrix() throws {
        let normal = snapshot()
        precondition(!normal.needsAttention && normal.menuTitle == "About Muu IME")
        precondition(normal.lines.map(\.label) == ["バージョン", "コミット", "Core", "システム辞書", "言語モデル",
            "ユーザー辞書", "取り込み辞書", "郵便番号", "学習", "学習の最終保存", "学習の保存状態", "記録"])
        let attribution = """
        ■ 言語モデル（別途取得）
        名称: kanakanji-edge-33m
        ライセンス: 重み Apache-2.0、tokenizer と学習データ CC BY-SA 4.0 ほか
        詳細: アプリに同梱の NOTICE（THIRD_PARTY_LICENSES）
        """
        precondition(normal.text.contains(build.commit) && AboutStatusSnapshot.attribution == attribution)
        precondition(normal.text.hasSuffix("\n\n" + attribution) && !normal.text.contains("\n\n\n"))
        let withoutModel = snapshot(runtime: .success(runtime(.unloaded)))
        precondition(withoutModel.lines.first(where: { $0.label == "言語モデル" })?.value == "なし")
        precondition(withoutModel.text.hasSuffix("\n\n" + AboutStatusSnapshot.attribution(for: nil)))
        let alert = normal.makeAlert()
        precondition(alert.messageText == "About Muu IME" && alert.informativeText == normal.text)
        precondition(alert.buttons.count == 1 && alert.buttons[0].title == "閉じる")
        for state in [CoreRuntimeStatus.LanguageModel.unloaded, .failed] {
            precondition(snapshot(runtime: .success(runtime(state))).needsAttention)
        }
        precondition(!snapshot(runtime: .success(runtime(.loading))).needsAttention)
        precondition(snapshot(runtime: .success(runtime(dictionary: .kanaOnly))).needsAttention)
        precondition(snapshot(initialization: .failed).needsAttention)
        precondition(snapshot(initialization: .abiMismatch).needsAttention)
        precondition(snapshot(coreInvalid: true).needsAttention)
        for code in [MUU_RESULT_CREATE_FAILED, MUU_RESULT_CORE_INVALID] {
            precondition(snapshot(runtime: .failure(.init(status: code, code: "fixture"))).needsAttention)
        }
        let unknown: Result<CoreRuntimeStatus, CoreOperationError> = .failure(.init(status: MUU_RESULT_INTERNAL, code: "fixture"))
        precondition(!snapshot(runtime: unknown).needsAttention, "a failed query is not degradation")
        let unknownBuild = AboutStatusProvider.snapshot(initialization: .ready, build: .unknown,
            runtime: .success(runtime()), coreInvalid: false, user: healthy, imported: healthy,
            postal: healthy, learning: saved, recorder: recording)
        precondition(!unknownBuild.needsAttention && unknownBuild.text.contains("ビルド 不明"))
        precondition(!snapshot(runtime: nil, initialization: .notStarted, learning: nil, recorder: nil).needsAttention)
        var loadingData = healthy; loadingData.busy = true; loadingData.startupState = .loading
        let loadingSnapshot = snapshot(managed: loadingData)
        precondition(!loadingSnapshot.needsAttention && loadingSnapshot.lines.first(where: { $0.label == "取り込み辞書" })?.value == "読み込み中")
        for state in [DataStartupState.readOnly, .failed] {
            var bad = healthy; bad.startupState = state
            if state == .readOnly { bad.access = "read_only" }
            precondition(snapshot(managed: bad).needsAttention)
        }
        var busy = healthy; busy.busy = true
        precondition(!snapshot(managed: busy).needsAttention)
        precondition(!snapshot(managed: .init()).needsAttention)
        var privateReason = healthy
        privateReason.access = "read_only"; privateReason.reason = "CANARY_PRIVATE_FILE_TEXT"
        precondition(!snapshot(managed: privateReason).text.contains("CANARY_"))
        for update: (inout AboutManagedDataStatus) -> Void in [
            { $0.access = "read_only" },
            { $0.layer = "empty_degraded" }, { $0.layer = "disabled" },
            { $0.stopped = true }, { $0.durabilityWarning = true }, { $0.initialLoadFailed = true },
        ] {
            var fault = healthy; update(&fault)
            precondition(snapshot(managed: fault).needsAttention)
            precondition(snapshot(runtime: unknown, managed: fault).needsAttention,
                "unknown Core facts must not erase a known store failure")
        }
        let pending = PersonalFrequencyPersistenceStatus(initialized: true, stopped: false,
            stopReason: nil, fileState: .present, lastSavedAt: nil, pending: true, saveIssue: nil, queryFailed: false)
        precondition(!snapshot(learning: pending).needsAttention)
        precondition(snapshot(learning: pending).text.contains("時刻不明"))
        let mtime = PersonalFrequencyPersistenceStatus(initialized: true, stopped: false,
            stopReason: nil, fileState: .present, lastSavedAt: .fileModification(Date(timeIntervalSince1970: 100)),
            pending: false, saveIssue: nil, queryFailed: false)
        precondition(snapshot(learning: mtime).text.contains("utc:100（ファイル更新時刻）"))
        for issue in [PersonalFrequencyPersistenceStatus.SaveIssue.writeFailed, .durabilityWarning] {
            let warning = PersonalFrequencyPersistenceStatus(initialized: true, stopped: false,
                stopReason: nil, fileState: .present, lastSavedAt: .completedSave(Date(timeIntervalSince1970: 50)),
                pending: true, saveIssue: issue, queryFailed: false)
            precondition(snapshot(learning: warning).needsAttention)
            precondition(snapshot(learning: warning).text.contains("utc:50（最終保存）"))
        }
        for reason in [PersonalFrequencyPersistenceStatus.StopReason.unverifiable, .quarantineFailed, .clearRejected] {
            let stopped = PersonalFrequencyPersistenceStatus(initialized: true, stopped: true,
                stopReason: reason, fileState: .unknown, lastSavedAt: nil, pending: false, saveIssue: nil, queryFailed: false)
            precondition(snapshot(learning: stopped).needsAttention)
        }
        precondition(!snapshot(recorder: .init(requested: false, configured: false, stopped: false)).needsAttention)
        // The distribution line is its own text and raises no attention, but it
        // must not hide another line's warning in the menu title.
        let distribution = CommitStreamRecorderStatus(requested: false, configured: false, stopped: false, availableInBuild: false)
        let distributionSnapshot = snapshot(recorder: distribution)
        precondition(distributionSnapshot.text.contains("記録: 無効（配布ビルド）") && !distributionSnapshot.needsAttention)
        let distributionWarning = snapshot(runtime: .success(runtime(dictionary: .kanaOnly)), recorder: distribution)
        precondition(distributionWarning.text.contains("記録: 無効（配布ビルド）") && distributionWarning.needsAttention)
        precondition(distributionWarning.menuTitle == "⚠︎ About Muu IME", "build-disabled recording cannot erase another warning")
        precondition(snapshot(recorder: .init(requested: true, configured: false, stopped: false)).needsAttention)
        precondition(snapshot(recorder: .init(requested: true, configured: true, stopped: true)).needsAttention)
        for (status, text, attention) in [
            (CommitStreamRecorderStatus(requested: true, configured: true, stopped: false, pendingCount: 2), "書き込み有効（保存待ち）", false),
            (.init(requested: true, configured: true, stopped: false, pendingCount: 1, retrying: true), "再試行待ち", true),
            (.init(requested: true, configured: true, stopped: false, retrying: true), "書き込み有効", false),
            (.init(requested: true, configured: true, stopped: false, discarded: 2), "書き込み有効（この起動で破棄2件）", true),
            (.init(requested: true, configured: true, stopped: false, reportPending: true), "欠損件数の報告待ち", true),
            (.init(requested: true, configured: true, stopped: false, reportUnreportable: true), "欠損件数を報告できません", true),
            (.init(requested: true, configured: true, stopped: true), "安全でない保存先", true),
            (.init(requested: true, configured: true, stopped: false, closing: true), "終了処理中", false)
        ] {
            let value = snapshot(recorder: status)
            precondition(value.text.contains(text) && value.needsAttention == attention)
        }
        var calls = 0
        var model = CoreRuntimeStatus.LanguageModel.loading
        let provider = AboutStatusProvider(query: { calls += 1; return .success(runtime(model)) })
        _ = provider.capture()
        precondition(calls == 0, "status must not initiate Core or data stores")
        provider.configure(initialization: .ready, build: build)
        let loading = provider.capture()
        model = .available
        let available = provider.capture()
        model = .failed
        let failed = provider.capture()
        precondition(calls == 3 && !loading.needsAttention && !available.needsAttention && failed.needsAttention)
        precondition(loading.text.contains("読み込み中") && !loading.text.contains("読み込み失敗"))
        precondition(available.text.contains("読み込み済み") && failed.text.contains("読み込み失敗"))
        guard case .success(let actual) = RustCore.runtimeStatus() else { preconditionFailure("actual runtime status") }
        precondition(actual.schemaVersion == 2 && actual.systemDictionary == .kanaOnly && actual.languageModel == .unloaded)
    }

    // Retry state over a record's life. While a record waits, About shows "再試行待ち".
    // After the backoff (1, 2, 4, 8, 16 s) the record is discarded; the cooldown is
    // still there for later records, but with nothing waiting, About shows the discard
    // and the pending drop report instead. An hour later nothing has been re-attempted
    // and the text is unchanged.
    private static func recorderRetryingAfterDiscard() throws {
        let root = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/muu-about-status-tests", isDirectory: true)
            .resolvingSymlinksInPath().appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let clock = CommitStreamTestClock()
        var attempts = 0
        let recorder = CommitStreamRecorder(processConfiguration: .init(
            processNonce: "about-retry", sourceCommit: build.commit, build: 123),
            directoryURL: root, clock: clock.clock, checkpoint: { _ in
                attempts += 1
                throw CommitStreamDeliveryFailure.io
            })
        recorder.append(Data("{}".utf8))
        recorder.waitUntilIdleForTesting()
        precondition(attempts == 1 && recorder.status.pendingCount == 1 && recorder.status.retrying)
        let waiting = snapshot(recorder: recorder.status)
        precondition(waiting.lines.last?.value == "再試行待ち" && waiting.lines.last?.needsAttention == true)
        precondition(waiting.menuTitle == "⚠︎ About Muu IME")
        for delay in [1.0,2,4,8,16] { clock.advance(delay); recorder.waitUntilIdleForTesting() }
        precondition(attempts == 6 && recorder.status.pendingCount == 0 && recorder.status.discarded == 1)
        precondition(recorder.status.retrying, "cooldown remains available to later records")
        let discarded = snapshot(recorder: recorder.status)
        precondition(discarded.lines.last?.value == "書き込み有効（この起動で破棄1件）・欠損件数の報告待ち",
            "cooldown alone is not a record waiting to retry")
        precondition(discarded.lines.last?.needsAttention == true && discarded.menuTitle == "⚠︎ About Muu IME")
        precondition(discarded.text.contains("記録: 書き込み有効（この起動で破棄1件）・欠損件数の報告待ち"))
        clock.advance(3_600); recorder.waitUntilIdleForTesting()
        precondition(attempts == 6 && recorder.status.pendingCount == 0 && recorder.status.retrying)
        precondition(snapshot(recorder: recorder.status).text == discarded.text)
    }

    // Two contracts. The bounded read takes bytes and mtime from the same open file
    // descriptor, so replacing the path between them cannot mix two files. The learning
    // save status: a failed Core query is "unknown", not a fault, and cannot clear a
    // durability warning; only a later complete save does. A write that fails before the
    // rename keeps the older save time. Clearing resets the status. At startup the
    // file's mtime is shown, marked as a file time, and a failed clear keeps it.
    private static func metadataAndSaveFacts() throws {
        // Foundation can reintroduce /var aliases for temporaryDirectory;
        // use the same explicit cache fixture base as the persistence tests.
        let root = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/muu-about-status-tests", isDirectory: true)
            .resolvingSymlinksInPath().appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let file = root.appendingPathComponent("metadata")
        let original = Data("old bytes".utf8), replacement = Data("new bytes".utf8)
        let date = Date(timeIntervalSince1970: 1_000)
        try original.write(to: file)
        try FileManager.default.setAttributes([.modificationDate: date], ofItemAtPath: file.path)
        let read = try FileSafety.boundedReadWithMetadata(file, maxBytes: 100,
            unsupportedFileType: { CocoaError(.fileReadCorruptFile) },
            fileTooLarge: { CocoaError(.fileReadTooLarge) }, posixError: { _, _ in CocoaError(.fileReadUnknown) },
            modificationDate: { metadata in
                // Replace the pathname after fstat. Both bytes and timestamp
                // must still describe the opened old inode, never the new path.
                try! replacement.write(to: file, options: .atomic)
                return Date(timeIntervalSince1970: Double(metadata.st_mtimespec.tv_sec))
            })
        precondition(read.data == original && read.modificationDate == date)
        let noDate = try FileSafety.boundedReadWithMetadata(file, maxBytes: 100,
            unsupportedFileType: { CocoaError(.fileReadCorruptFile) },
            fileTooLarge: { CocoaError(.fileReadTooLarge) }, posixError: { _, _ in CocoaError(.fileReadUnknown) },
            modificationDate: { _ in nil })
        precondition(noDate.data == replacement && noDate.modificationDate == nil)

        for reason: PersonalFrequencyPersistenceStatus.StopReason in [.unsafeSave, .invalidSave] {
            let status = PersonalFrequencyPersistenceStatus(initialized: true, stopped: true, stopReason: reason,
                fileState: .present, lastSavedAt: nil, pending: true, saveIssue: .writeFailed, queryFailed: false)
            let display = snapshot(learning: status)
            precondition(display.needsAttention && display.text.contains("保存失敗・保存停止"))
            precondition(!display.text.contains("再試行待ち"), "permanent failure cannot promise another retry")
        }
        for stopped in [false, true] {
            let cleared = PersonalFrequencyPersistenceStatus(initialized: true, stopped: stopped,
                stopReason: stopped ? .clearRejected : nil, fileState: .cleared, lastSavedAt: nil,
                pending: false, saveIssue: .clearDurabilityWarning, queryFailed: false)
            let display = snapshot(learning: cleared)
            precondition(display.needsAttention && display.text.contains("消去済み・耐久性の警告あり"))
            precondition(!display.text.contains("再試行待ち"), "cleared learning must not promise a retry")
            if stopped { precondition(display.text.contains("消去の反映失敗・保存停止")) }
        }

        let scheduler = SaveScheduler()
        var attempts = 0
        var queryFails = true
        var saveTime: TimeInterval = 2_000
        let store = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "fixture",
            scheduler: scheduler.schedule, persistenceOverride: { _, _ in
                attempts += 1
                if attempts == 2 { throw CocoaError(.fileWriteUnknown) }
                return attempts == 1 ? CocoaError(.fileWriteUnknown) : nil
            }, deletionOverride: { _ in }, coreClear: { .success(.init(generation: 1, revision: 0)) },
            query: { _ in queryFails ? .failure(CocoaError(.fileReadUnknown)) : .success(nil) },
            clock: { Date(timeIntervalSince1970: saveTime) })
        let initial = CoreInitializationResult(degraded: .init(personalFrequencyInvalid: false,
            personalFrequencyUnverifiable: false, userDictionaryInvalid: false),
            personalFrequencyVersion: .init(generation: 0, revision: 0))
        store.acceptStartup(initial)
        precondition(store.persistenceStatus.lastSavedAt == nil)
        store.poll()
        precondition(store.persistenceStatus.queryFailed && store.persistenceStatus.saveIssue == nil)
        let unknown = snapshot(learning: store.persistenceStatus)
        precondition(!unknown.needsAttention && unknown.text.contains("状態不明（学習の照会に失敗）"))
        queryFails = false; store.poll()
        precondition(!store.persistenceStatus.queryFailed)
        store.receive(original, version: .init(generation: 0, revision: 1))
        precondition(store.persistenceStatus.pending && store.persistenceStatus.saveIssue == nil)
        scheduler.fire(); try store.flushForTesting()
        precondition(store.persistenceStatus.lastSavedAt == .completedSave(Date(timeIntervalSince1970: 2_000)))
        precondition(store.persistenceStatus.saveIssue == .durabilityWarning)
        queryFails = true; store.poll()
        precondition(snapshot(learning: store.persistenceStatus).needsAttention)
        precondition(store.persistenceStatus.queryFailed && store.persistenceStatus.saveIssue == .durabilityWarning)
        queryFails = false; store.poll()
        precondition(!store.persistenceStatus.queryFailed && store.persistenceStatus.saveIssue == .durabilityWarning,
            "a successful Core query cannot establish disk durability")
        store.receive(replacement, version: .init(generation: 0, revision: 2))
        saveTime = 3_000
        scheduler.fire(); try store.flushForTesting()
        precondition(store.persistenceStatus.saveIssue == .writeFailed)
        precondition(store.persistenceStatus.lastSavedAt == .completedSave(Date(timeIntervalSince1970: 2_000)),
            "a pre-rename failure cannot claim a newer save time")
        saveTime = 4_000
        scheduler.fire(); try store.flushForTesting()
        precondition(attempts == 3 && store.persistenceStatus.saveIssue == nil && !store.persistenceStatus.pending)
        precondition(store.persistenceStatus.lastSavedAt == .completedSave(Date(timeIntervalSince1970: 4_000)))
        queryFails = true; store.poll()
        precondition(store.persistenceStatus.queryFailed)
        guard case .cleared = store.clear() else { preconditionFailure("clear") }
        precondition(store.persistenceStatus.fileState == .cleared && store.persistenceStatus.lastSavedAt == nil)
        precondition(store.persistenceStatus.saveIssue == nil && !store.persistenceStatus.stopped && !store.persistenceStatus.queryFailed)

        // Observe startup mtime without changing or re-encoding opaque bytes.
        let disk = root.appendingPathComponent("mtime/personal_frequency/personal-frequency-v1.json")
        try FileManager.default.createDirectory(at: disk.deletingLastPathComponent(), withIntermediateDirectories: true)
        try original.write(to: disk)
        try FileManager.default.setAttributes([.modificationDate: date], ofItemAtPath: disk.path)
        let startup = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "mtime")
        precondition(startup.startupData == original && startup.persistenceStatus.lastSavedAt == .fileModification(date))
        precondition(startup.persistenceStatus.fileState == .present && startup.loadError == nil)
        // Startup classification, as in FileSafety.readState: only ENOENT from
        // the open is absence. Here the whole bundle directory is missing,
        // which is still that ENOENT: absent, nil bytes, no error.
        let absent = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "missing")
        precondition(absent.persistenceStatus.fileState == .absent && absent.startupData == nil && absent.loadError == nil)
        // Any other open failure is unreadable, not absent. The state stays
        // unknown, the error is kept, and the empty Data makes Core reject the
        // file rather than adopt empty learning as already saved. The blocking
        // file itself is left untouched. A check that treats every failed
        // existence test as absence (the earlier fileExists) fails here.
        // A regular file in the parent path gives deterministic ENOTDIR,
        // including in privileged test environments where chmod 000 could read.
        let blockedParent = root.appendingPathComponent("not-a-directory")
        try original.write(to: blockedParent)
        let unreadable = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "not-a-directory")
        precondition(unreadable.persistenceStatus.fileState == .unknown && unreadable.startupData == Data()
            && unreadable.loadError != nil,
            "non-ENOENT startup failure: state=\(unreadable.persistenceStatus.fileState), bytes=\(String(describing: unreadable.startupData?.count))")
        guard let error = unreadable.loadError as? PersonalFrequencyStoreError,
              case .io(let operation, let code) = error else { preconditionFailure("expected POSIX read error") }
        precondition(operation == "open-read" && code == ENOTDIR)
        precondition(try! Data(contentsOf: blockedParent) == original)

        startup.acceptStartup(initial)
        precondition(try! Data(contentsOf: disk) == original)
        let failure = PersonalFrequencyStore(baseDirectory: root, bundleIdentifier: "mtime",
            deletionOverride: { _ in throw CocoaError(.fileWriteUnknown) })
        guard case .precommitFailure = failure.clear() else { preconditionFailure("failed delete") }
        precondition(failure.persistenceStatus.lastSavedAt == .fileModification(date))
    }
}

// Holds scheduled saves until a test fires them one at a time, so each save
// attempt happens at a point the test chooses.
private final class SaveScheduler {
    private let lock = NSLock()
    private var jobs: [() -> Void] = []
    func schedule(_ delay: TimeInterval, _ job: @escaping () -> Void) {
        lock.lock(); jobs.append(job); lock.unlock()
    }
    func fire() {
        lock.lock(); precondition(!jobs.isEmpty); let job = jobs.removeFirst(); lock.unlock()
        job()
    }
}
