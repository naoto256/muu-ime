import AppKit

struct AboutBuildInfo: Sendable {
    let version: String
    let build: String
    let commit: String
    static let unknown = Self(version: "不明", build: "不明", commit: "不明")

    init(version: String, build: String, commit: String) {
        self.version = version; self.build = build; self.commit = commit
    }

    init(bundle: Bundle) {
        version = bundle.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "不明"
        build = bundle.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "不明"
        commit = bundle.object(forInfoDictionaryKey: "MuuSourceCommit") as? String ?? "不明"
    }
}

/// A display projection of already-owned management views and IO outcomes.
/// It carries no entries, file bytes, paths or arbitrary error descriptions.
struct AboutManagedDataStatus: Sendable {
    var started = false
    var busy = false
    var access: String?
    var layer: String?
    var reason: String?
    var count: Int?
    var stopped = false
    var durabilityWarning = false
    var initialLoadFailed = false
    var startupState: DataStartupState?
}

struct AboutStatusLine: Sendable {
    let label: String
    let value: String
    var needsAttention = false
}

struct AboutStatusSnapshot: Sendable {
    var lines: [AboutStatusLine]
    var modelAttribution = Self.attribution
    var downloadRuntime: ModelDownloadRuntime = .unknown
    var needsAttention: Bool { lines.contains(where: \.needsAttention) }
    // The input menu is shown by TextInputMenuAgent, which copies each item
    // and drops `NSMenuItem.image`; a template icon never appeared on a real
    // device. So the mark is text, placed first so it stays
    // visible at the start of the item. It follows the same rule as before:
    // `needsAttention`, the OR of the rows. VoiceOver gets the attention from
    // `accessibilityLabel` instead of reading the glyph.
    var menuTitle: String { needsAttention ? "⚠︎ About Muu IME" : "About Muu IME" }
    var accessibilityLabel: String { needsAttention ? "About Muu IME、注意が必要です" : "About Muu IME" }
    var text: String { lines.map { "\($0.label): \($0.value)" }.joined(separator: "\n") + "\n\n" + modelAttribution }
    var textWithoutDownload: String {
        lines.filter { $0.label != "モデルの取得" }.map { "\($0.label): \($0.value)" }.joined(separator: "\n") + "\n\n" + modelAttribution
    }
    @MainActor func addingDownload(_ store: ModelDownloadStore) -> Self {
        var snapshot = self
        snapshot.lines.removeAll { $0.label == "モデルの取得" }
        snapshot.lines.append(ModelDownloadPresentation.make(store: store, runtime: downloadRuntime).line)
        return snapshot
    }

    // The model is not bundled. Attribution is still visible before download;
    // the detailed provenance and license links ship in THIRD_PARTY_LICENSES.
    // The attribution block has four fixed lines. "ほか" stands for what
    // only NOTICE spells out: llm-jp-corpus-v3 under ODC-BY and the added control tokens
    // under Apache-2.0. The blank line before the heading comes from `text`.
    static let attribution = """
    ■ 言語モデル（別途取得）
    名称: \(ConversionLmModel.edge.name)
    ライセンス: 重み Apache-2.0、tokenizer と学習データ CC BY-SA 4.0 ほか
    詳細: アプリに同梱の NOTICE（THIRD_PARTY_LICENSES）
    """

    static func attribution(for model: ConversionLmModel?) -> String {
        if model == .edge { return attribution }
        // Also identify the acquisition target before a model is installed.
        // The runtime row above, not this license block, says what is in use.
        //
        // The text states the published terms only: the model card's Apache-2.0
        // declaration and the tokenizer's origin (ku-nlp's CC BY-SA 4.0
        // tokenizer). Whether the weights and training data carry other terms
        // upstream is not something Muu can establish from public material.
        // That lineage is the zenz publisher's responsibility, so it is
        // neither claimed nor ruled out here.
        return """
        ■ 言語モデル（別途取得）
        名称: \(ConversionLmModel.zenz.name)
        ライセンス: Apache-2.0（モデルカードの宣言）。tokenizer は ku-nlp/gpt2-small-japanese-char 由来（CC BY-SA 4.0）
        詳細: アプリに同梱の NOTICE（THIRD_PARTY_LICENSES）
        """
    }

    @MainActor func makeAlert(presentation: ModelDownloadPresentation? = nil) -> NSAlert {
        let alert = NSAlert()
        alert.messageText = "About Muu IME"
        alert.informativeText = text
        let layout = presentation?.buttons ?? .closeOnly
        // NSAlert initially makes the first button the default and puts it
        // rightmost in a horizontal row. Add Close first so that initial
        // default and the final one configureKeyboard assigns are the same
        // button, which keeps NSAlert's left/right placement stable: the row
        // reads operation | Close.
        // Pending restart has no Close and configureKeyboard clears its only
        // button's default explicitly.
        if layout != .restartOnly { alert.addButton(withTitle: "閉じる") }
        if let presentation, layout != .closeOnly { alert.addButton(withTitle: presentation.button) }
        layout.configureKeyboard(alert)
        return alert
    }
}

/// The menu never instantiates a data-store singleton. Launch registers the
/// stores it already created, in its existing order. Each opening captures a
/// fresh memory-only snapshot; there is no polling or notification bus.
@MainActor
final class AboutStatusProvider {
    private(set) var downloadStore: ModelDownloadStore?
    func configureModelDownload(_ store: ModelDownloadStore) { downloadStore = store }
    enum Initialization { case notStarted, ready, failed, abiMismatch }
    static let shared = AboutStatusProvider()
    private var initialization = Initialization.notStarted
    private var build = AboutBuildInfo.unknown
    private var learning: PersonalFrequencyStore?
    private var user: UserDictionaryStore?
    private var imported: ImportedDictionaryStore?
    private var postal: PostalCodeStore?
    private var recorder: CommitStreamRecorder?
    private let query: () -> Result<CoreRuntimeStatus, CoreOperationError>

    init(query: @escaping () -> Result<CoreRuntimeStatus, CoreOperationError> = RustCore.runtimeStatus) {
        self.query = query
    }

    func configure(initialization: Initialization, build: AboutBuildInfo,
                   learning: PersonalFrequencyStore? = nil, user: UserDictionaryStore? = nil,
                   imported: ImportedDictionaryStore? = nil, postal: PostalCodeStore? = nil,
                   recorder: CommitStreamRecorder? = nil) {
        self.initialization = initialization; self.build = build
        self.learning = learning; self.user = user; self.imported = imported
        self.postal = postal; self.recorder = recorder
    }

    func capture() -> AboutStatusSnapshot {
        // Only a ready Core is queried, so CREATE_FAILED from the query
        // itself does not occur here; the snapshot handles it for tests.
        let runtime = initialization == .ready ? query() : nil
        let userStatus = user.map { store in
            AboutManagedDataStatus(started: true, busy: store.isBusy,
                access: store.view?.access, layer: store.view?.layerStatus, reason: store.view?.reason,
                count: store.view?.entryCount, stopped: store.editingStopped,
                durabilityWarning: store.durabilityWarning != nil)
        } ?? .init()
        let importedStatus = imported.map { store in
            AboutManagedDataStatus(started: store.hasStarted, busy: store.isBusy,
                access: store.view?.access, layer: store.view?.layerStatus, reason: store.view?.reason,
                count: store.view?.dictionaries?.count, stopped: store.editingStopped,
                durabilityWarning: store.durabilityWarning != nil, startupState: store.startupState)
        } ?? .init()
        let postalStatus = postal.map { store in
            AboutManagedDataStatus(started: store.hasStarted, busy: store.isUpdating,
                access: store.view?.access, layer: store.view?.layerStatus, reason: store.view?.reason,
                stopped: store.updatesStopped, durabilityWarning: store.durabilityWarning != nil,
                initialLoadFailed: store.initialLoadFailed, startupState: store.startupState)
        } ?? .init()
        let snapshot = Self.snapshot(initialization: initialization, build: build, runtime: runtime,
            coreInvalid: RustCore.isInvalid, user: userStatus, imported: importedStatus,
            postal: postalStatus, learning: learning?.persistenceStatus, recorder: recorder?.status)
        return downloadStore.map { snapshot.addingDownload($0) } ?? snapshot
    }

    /// Only finite facts determine attention. Failure to query a fact is
    /// unknown, not evidence that the data itself has degraded.
    static func snapshot(initialization: Initialization, build: AboutBuildInfo,
                         runtime: Result<CoreRuntimeStatus, CoreOperationError>?, coreInvalid: Bool,
                         user: AboutManagedDataStatus, imported: AboutManagedDataStatus,
                         postal: AboutManagedDataStatus, learning: PersonalFrequencyPersistenceStatus?,
                         recorder: CommitStreamRecorderStatus?, formatDate: ((Date) -> String)? = nil) -> AboutStatusSnapshot {
        let renderDate = formatDate ?? { Self.formatDate($0) }
        var lines = [AboutStatusLine(label: "バージョン", value: "\(build.version)（ビルド \(build.build)）"),
                     AboutStatusLine(label: "コミット", value: build.commit)]
        let current: CoreRuntimeStatus?
        if case .success(let value) = runtime { current = value } else { current = nil }
        let queryCode: Int32?
        if case .failure(let error) = runtime { queryCode = error.status } else { queryCode = nil }
        if coreInvalid || queryCode == MUU_RESULT_CORE_INVALID {
            lines.append(.init(label: "Core", value: "利用できません（再起動が必要）", needsAttention: true))
        } else if initialization == .failed || initialization == .abiMismatch || queryCode == MUU_RESULT_CREATE_FAILED {
            lines.append(.init(label: "Core", value: "初期化失敗", needsAttention: true))
        } else {
            lines.append(.init(label: "Core", value: current != nil ? "利用可" : initialization == .notStarted ? "未開始" : "状態不明"))
        }
        lines.append(.init(label: "システム辞書", value: current.map {
            $0.systemDictionary == .available ? "利用可" : "かなだけで動作"
        } ?? "状態不明", needsAttention: current?.systemDictionary == .kanaOnly))
        var model: String
        switch current?.languageModel {
        case .available: model = "読み込み済み"
        case .loading: model = "読み込み中"
        case .unloaded: model = "なし"
        case .failed: model = "読み込み失敗"
        case nil: model = "状態不明"
        }
        if let details = current?.languageModelDetails {
            if let actual = details.actualModel { model += "（\(actual.name)）" }
            else if current?.languageModel == .loading, let requested = details.requestedModel {
                model += "（\(requested.name)）"
            }
            if let reason = details.fallbackReason {
                model += "・\(details.requestedModel?.name ?? "モデル")が\(reason.description)"
                if details.actualModel != nil { model += "、代わりのモデルを使用" }
            }
            if let reason = details.unavailableReason {
                let failed = details.fallbackReason == nil ? details.requestedModel : .edge
                model += "・\(failed?.name ?? "モデル")が\(reason.description)"
            }
        }
        // Unloaded is the steady state when no model is set up, so such an
        // install keeps the attention mark. Loading does not count.
        lines.append(.init(label: "言語モデル", value: model,
            needsAttention: current?.languageModel == .unloaded || current?.languageModel == .failed))
        lines.append(managedLine("ユーザー辞書", user, empty: "未登録"))
        lines.append(managedLine("取り込み辞書", imported, empty: "未登録（無効の辞書は異常ではありません）"))
        lines.append(managedLine("郵便番号", postal, empty: "データなし"))
        if let learning {
            let availability: String
            switch learning.stopReason {
            case .unverifiable: availability = "未検証・保存停止"
            case .quarantineFailed: availability = "退避失敗・保存停止"
            case .clearRejected: availability = "消去の反映失敗・保存停止"
            case .unsafeSave: availability = "安全でない保存先・保存停止"
            case .invalidSave: availability = "保存内容の不正・保存停止"
            case nil: availability = learning.stopped ? "保存停止" : learning.initialized ? "利用可" : "未開始"
            }
            lines.append(.init(label: "学習", value: availability, needsAttention: learning.stopped))
            let saved: String
            switch learning.lastSavedAt {
            case .fileModification(let date): saved = "\(renderDate(date))（ファイル更新時刻）"
            case .completedSave(let date): saved = "\(renderDate(date))（最終保存）"
            case nil:
                switch learning.fileState {
                case .absent: saved = "保存ファイルなし"
                case .cleared: saved = "消去済み・保存ファイルなし"
                case .present, .unknown: saved = "時刻不明"
                }
            }
            lines.append(.init(label: "学習の最終保存", value: saved))
            var persistence: String
            switch learning.saveIssue {
            // A stopped store never retries, so it must not say it is waiting.
            case .writeFailed: persistence = learning.stopped ? "保存失敗・保存停止" : "保存失敗・再試行待ち"
            case .durabilityWarning: persistence = "保存済み・耐久性の警告あり"
            case .clearDurabilityWarning: persistence = "消去済み・耐久性の警告あり"
            case nil: persistence = learning.stopped ? "保存停止" : learning.pending ? "保存待ち" : learning.initialized ? "保存済み" : "未開始"
            }
            if learning.queryFailed {
                let unknown = "状態不明（学習の照会に失敗）"
                persistence = learning.saveIssue == nil && !learning.pending && !learning.stopped
                    ? unknown : persistence + "・" + unknown
            }
            lines.append(.init(label: "学習の保存状態", value: persistence, needsAttention: learning.saveIssue != nil))
        } else {
            lines += [.init(label: "学習", value: "未開始"), .init(label: "学習の最終保存", value: "時刻不明"),
                      .init(label: "学習の保存状態", value: "未開始")]
        }
        let recording: AboutStatusLine
        if let recorder {
            // The distribution state is expected, so it adds no attention.
            // Other lines can still raise the menu's warning.
            if !recorder.availableInBuild { recording = .init(label: "記録", value: "無効（配布ビルド）") }
            else if !recorder.requested { recording = .init(label: "記録", value: "無効（設定）") }
            else if !recorder.configured { recording = .init(label: "記録", value: "設定失敗・停止", needsAttention: true) }
            else {
                var value: String
                var attention = false
                // retrying also carries cooldown for later records; an empty
                // queue has no current record waiting for another attempt.
                if recorder.metadataFailed { value = "配達情報の不一致により停止（再起動が必要）"; attention = true }
                else if recorder.stopped { value = "安全でない保存先のため停止（再起動が必要）"; attention = true }
                else if recorder.retrying && recorder.pendingCount > 0 { value = "再試行待ち"; attention = true }
                else if recorder.closing { value = "終了処理中" }
                else if current?.commitStreamEnabled == true { value = recorder.pendingCount > 0 ? "書き込み有効（保存待ち）" : "書き込み有効" }
                else if current != nil { value = "Core の設定で無効"; attention = true }
                else { value = "状態不明（書き込み準備済み）" }
                // Actual records discarded here, not their carried loss weight:
                // adding weights would repeatedly count the same older losses.
                if recorder.discarded > 0 { value += "（この起動で破棄\(recorder.discarded)件）"; attention = true }
                if recorder.reportUnreportable { value += "・欠損件数を報告できません"; attention = true }
                else if recorder.reportPending { value += "・欠損件数の報告待ち"; attention = true }
                recording = .init(label: "記録", value: value, needsAttention: attention)
            }
        } else { recording = .init(label: "記録", value: "未開始") }
        lines.append(recording)
        let attribution: String
        if let details = current?.languageModelDetails {
            attribution = AboutStatusSnapshot.attribution(for: details.actualModel)
        } else {
            // Schema 1 came only from edge. An unknown query cannot identify
            // a model; do not infer one from paths or the current preference.
            attribution = current?.languageModel == .available
                ? AboutStatusSnapshot.attribution : AboutStatusSnapshot.attribution(for: nil)
        }
        return .init(lines: lines, modelAttribution: attribution, downloadRuntime: .from(current))
    }

    private static func managedLine(_ label: String, _ status: AboutManagedDataStatus, empty: String) -> AboutStatusLine {
        var parts: [String] = []
        var attention = false
        if status.startupState == .loading { parts.append("読み込み中") }
        else if status.busy { parts.append("処理中") }
        if status.stopped { parts.append("編集停止（再起動が必要）"); attention = true }
        if status.access == "read_only" { parts.append("読み取り専用"); attention = true }
        // No current Rust view emits `disabled`. Postal's disabled_view
        // reports `empty_degraded`, so that branch is defensive only.
        if status.layer == "empty_degraded" || status.layer == "disabled" {
            parts.append("層は空（縮退）"); attention = true
        }
        if status.initialLoadFailed || status.startupState == .failed { parts.append("初期読み込み失敗"); attention = true }
        if status.durabilityWarning { parts.append("保存の耐久性に警告あり"); attention = true }
        if parts.isEmpty {
            if !status.started { parts.append("未開始") }
            // Any other access value shows as unknown. Imported's view says
            // `loading` until its catalog is initialized. The store is busy
            // during that load, so the line reads "処理中" instead.
            else if status.access == "editable", let layer = status.layer {
                parts.append(status.count == 0 || layer == "empty" ? empty : "利用可")
            } else { parts.append("状態不明") }
        }
        return .init(label: label, value: parts.joined(separator: "・"), needsAttention: attention)
    }

    private static func formatDate(_ date: Date) -> String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "ja_JP")
        formatter.dateFormat = "yyyy-MM-dd HH:mm:ss ZZZZZ"
        return formatter.string(from: date)
    }
}
