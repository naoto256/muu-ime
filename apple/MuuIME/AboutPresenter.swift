import AppKit

// What this process's Core reports, reduced to what the download row needs.
// It comes from the Core status at About's opening, never from the files.
enum ModelDownloadRuntime: Sendable, Equatable {
    case unknown, unavailable, failed, loading, zenz, other
    case zenzFailed(CoreRuntimeStatus.ModelDetails.Failure, fallbackAvailable: Bool)

    var zenzFailure: CoreRuntimeStatus.ModelDetails.Failure? {
        if case .zenzFailed(let reason, _) = self { return reason }
        return nil
    }

    var hasOtherModel: Bool {
        if case .zenzFailed(_, let fallbackAvailable) = self { return fallbackAvailable }
        return self == .other
    }

    // zenz's own startup reason is read from `fallbackReason` first. When
    // Core fell back, `fallbackReason` is why zenz failed and
    // `unavailableReason` is why the fallback (edge) failed; only without a
    // fallback is `unavailableReason` zenz's. Reading them the other way
    // round would turn "zenz not_found, then edge load_failed" into a zenz
    // load_failed, and the overall `failed` state would hide the not_found
    // too. That reason decides whether a restart can help (see `acquired`).
    static func from(_ runtime: CoreRuntimeStatus?) -> Self {
        guard let runtime else { return .unknown }
        let details = runtime.languageModelDetails
        if runtime.languageModel == .available && details?.actualModel == .zenz { return .zenz }
        if details?.requestedModel == .zenz, let reason = details?.fallbackReason ?? details?.unavailableReason {
            // fallbackReason belongs to zenz; unavailableReason then belongs
            // to edge. Overall Core failure must not hide zenz's not_found.
            return .zenzFailed(reason, fallbackAvailable: runtime.languageModel == .available && details?.actualModel == .edge)
        }
        switch runtime.languageModel {
        case .unloaded: return .unavailable
        case .failed: return details?.requestedModel == .edge ? .unavailable : .failed
        case .loading: return .loading
        case .available: return .other
        }
    }
}

/// The acquisition row: its text, operation button, guidance and warning mark.
///
/// Guidance: at most one notice per state, chosen with the row. The
/// acquisition notice (取得後、次回の起動から使用) appears only while acquiring
/// is still the next step: the cache is missing or unusable, or a download is
/// in progress, has failed or was cancelled. A completed download not yet in
/// use gets only the restart notice. An idle valid cache also gets that notice
/// when configured for the next launch. Unused caches, startup loading or
/// checking, and a model already in use get no acquisition notice.
///
/// Buttons: 取得 while missing; 中止 while receiving or verifying; 再試行
/// after a failure or a cancel; 使用する for a valid cache that is not yet the
/// configured model; Muu IME を再起動 when the same projection says restart is
/// pending. Restart stands alone without a default; while zenz is in use only
/// Close remains. A startup load_failed stays a failure until an explicit
/// acquisition completes in this process. A not_found can use a valid cache
/// that arrived after startup. Other phases retain Close as default, with a
/// disabled 取得 during startup checking or 取得済み for other acquired states.
/// 使用する goes through `acquire`, which finds the valid
/// cache under the lock and only writes the settings, with no request.
///
/// The mark is on while the cache is missing or unusable, or the running Core
/// has no usable model, and it stays on after a completed download until a
/// Core actually runs zenz: completion says nothing about this process, which
/// keeps its startup model until restart. A durability failure (bytes in
/// place, durability not confirmed) and a hash or length mismatch (the server
/// sent other bytes) are marked even when the running Core is fine.
struct ModelDownloadPresentation: Sendable {
    enum Action: Sendable, Equatable { case none, acquire, cancel, restart }
    /// The button set required for each acquisition state.
    /// - operationAndClose (before and during acquisition, failures, unused
    ///   cache): the operation and 閉じる, with 閉じる as the default, so
    ///   Return closes and never starts a download.
    /// - restartOnly (whenever the projection says a restart is pending; see
    ///   `acquired`): only Muu IME を再起動,
    ///   and no default button, so Return cannot end the process; Return and
    ///   keypad Enter are swallowed (see handleKey). There is no 閉じる, but
    ///   Escape still closes the window without restarting.
    /// - closeOnly (zenz in use, or no download store): only 閉じる, default.
    enum Buttons: Sendable, Equatable {
        case operationAndClose, restartOnly, closeOnly

        // Indexes follow the add order in AboutStatusSnapshot: Close is added
        // first when present, so it is buttons[0] and the operation is [1].
        // With restartOnly the restart button is the only one. Callers use
        // these instead of first/last, which no longer name a role.
        @MainActor func operationButton(in alert: NSAlert) -> NSButton? {
            switch self {
            case .operationAndClose: return alert.buttons[1]
            case .restartOnly: return alert.buttons[0]
            case .closeOnly: return nil
            }
        }

        @MainActor func closeButton(in alert: NSAlert) -> NSButton? {
            self == .restartOnly ? nil : alert.buttons[0]
        }

        // Clears every key equivalent first, so a button kept from NSAlert's
        // own setup can never hold Return, then assigns the default if any.
        @MainActor func configureKeyboard(_ alert: NSAlert) {
            // Materialize AppKit's window before overriding its initial keys.
            let window = alert.window
            for button in alert.buttons { button.keyEquivalent = "" }
            if let close = closeButton(in: alert) {
                close.keyEquivalent = "\r"
                window.defaultButtonCell = close.cell as? NSButtonCell
            } else {
                window.defaultButtonCell = nil
            }
        }
    }
    let line: AboutStatusLine
    let button: String
    let action: Action
    let notice: String?
    let buttons: Buttons
    static let acquisitionNotice = "取得後、次回の Muu IME の起動から使用します。"
    static let restartNotice = "Muu IME を終了します。次の入力から zenz を使います"

    @MainActor static func make(store: ModelDownloadStore, runtime: ModelDownloadRuntime) -> Self {
        make(cache: store.cache, state: store.state, configured: store.isConfiguredForZenz, runtime: runtime)
    }

    static func make(cache: ModelDownloadStore.Cache, state: ModelDownloadStore.State,
                     configured: Bool, runtime: ModelDownloadRuntime) -> Self {
        let missing = cache == .missing
        let unavailable: Bool
        if case .unavailable = cache { unavailable = true } else { unavailable = false }
        let attention = missing || unavailable || runtime == .unavailable || runtime == .failed || runtime.zenzFailure != nil
        func presentation(_ value: String, _ button: String = "取得", _ action: Action = .acquire,
                          attention override: Bool? = nil,
                          notice: String? = ModelDownloadPresentation.acquisitionNotice,
                          buttons: Buttons = .operationAndClose) -> Self {
            .init(line: .init(label: "モデルの取得", value: value, needsAttention: override ?? attention),
                button: button, action: action, notice: notice, buttons: buttons)
        }
        // The active model is a Core fact even if the on-disk cache or its
        // acquisition changes. Retain independent durability/validation marks.
        if runtime == .zenz {
            let failed: Bool
            if case .failed(let failure) = state {
                failed = failure == .durability || failure == .hash || failure == .length
            } else { failed = false }
            return presentation("取得済み・利用中", "取得済み", .none, attention: attention || failed, notice: nil, buttons: .closeOnly)
        }
        // The one projection for a valid cache: row, operation, notice and
        // button set all follow from it, so the window can never show a
        // restart row with other buttons. The criterion is whether
        // restarting Muu can be expected to make zenz run.
        // - Yes, so restart is offered: the zenz cache is configured and this
        //   Core is not running it because zenz was not there at startup
        //   (not_found), no model was enabled at startup, Core runs another
        //   model, or a download completed in this process.
        // - No: zenz itself failed to load at startup (load_failed, or a
        //   failure with no details). A restart would read the same file and
        //   fail again, so the row reports the failure. `completed` is the
        //   exception: an explicit acquisition in this process has just
        //   verified and published the file, so a restart can now succeed.
        // - Not yet known: while the model is loading or Core's state cannot
        //   be read, nothing is offered.
        // An unconfigured cache is never enabled implicitly; it offers 使用する.
        func acquired(completed: Bool) -> Self {
            if !completed && (runtime.zenzFailure == .loadFailed || runtime == .failed) {
                return presentation("言語モデルを読み込めませんでした", configured ? "取得済み" : "使用する",
                    configured ? .none : .acquire, attention: true, notice: nil)
            }
            if !completed && !configured { return presentation("取得済み（未使用）", "使用する", notice: nil) }
            if !completed && (runtime == .loading || runtime == .unknown) {
                return presentation(runtime == .loading ? "取得済み（読み込み中）" : "取得済み（利用状態不明）",
                    "取得済み", .none, notice: nil)
            }
            return presentation(completed ? "完了（再起動待ち）" : "取得済み（再起動待ち）",
                "Muu IME を再起動", .restart, attention: true, notice: restartNotice, buttons: .restartOnly)
        }
        switch state {
        case .receiving(let percent): return presentation("取得中（\(percent)%）", "中止", .cancel)
        case .verifying: return presentation("照合中", "中止", .cancel)
        case .failed(let failure):
            return presentation(failure.message, "再試行", attention: attention || failure == .durability || failure == .hash || failure == .length)
        case .cancelled: return presentation("中止しました", "再試行")
        case .idle, .completed:
            // Row, operation, notice and keyboard layout all come from this
            // projection, including a cache acquired outside this process.
            switch cache {
            case .checking: return presentation("確認中", "取得", .none, attention: false, notice: nil)
            case .missing: return presentation("未取得（約74 MB）")
            case .unavailable(let failure): return presentation(failure.message, "再試行")
            case .valid: return acquired(completed: state == .completed)
            }
        }
    }
}

// One visible nonmodal About at a time. State changes update its acquisition
// row; only changes to the button set replace the NSAlert, whose public API
// cannot remove buttons. Core facts remain the opening-time snapshot.
// Return closes before acquisition and while in use. Pending restart has no
// default button; Escape closes it without starting another process.
//
// Nonmodal because the row must update in place while a download runs;
// `runModal()` would end at the first button press. Only one window exists,
// so a process-wide attempt is never split across two views.
//
// When the button set changes, a new NSAlert replaces the old one at the same
// top-left position. The old window's delegate is cleared before it closes,
// so its close does not tear down the new window's subscription. Button
// actions check that the sender is a button of the current alert, and
// windowWillClose checks the window, so a click or close that reaches the
// old window can never act as an operation of the new one.
@MainActor
final class AboutPresenter: NSObject, NSWindowDelegate {
    static let shared = AboutPresenter(capture: { AboutStatusProvider.shared.capture() },
        store: { AboutStatusProvider.shared.downloadStore }, terminate: { NSApp.terminate(nil) })
    private let capture: () -> AboutStatusSnapshot
    private let store: () -> ModelDownloadStore?
    private let terminate: () -> Void
    private var restartRequested = false
    private var alert: NSAlert?
    private var snapshot: AboutStatusSnapshot?
    private var downloadLine: NSTextField?
    private var observedStore: ModelDownloadStore?
    private var buttonLayout: ModelDownloadPresentation.Buttons?
    private var keyMonitor: Any?

    private init(capture: @escaping () -> AboutStatusSnapshot, store: @escaping () -> ModelDownloadStore?,
                 terminate: @escaping () -> Void) {
        self.capture = capture; self.store = store; self.terminate = terminate
    }

    func show() {
        if let alert { alert.window.makeKeyAndOrderFront(nil); return }
        let snapshot = capture()
        self.snapshot = snapshot
        observedStore = store()
        observedStore?.onChange = { [weak self] in self?.refreshDownload() }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            return self.handleKey(event)
        }
        render(observedStore.map { ModelDownloadPresentation.make(store: $0, runtime: snapshot.downloadRuntime) })
    }

    private func refreshDownload() {
        guard let store = observedStore, let snapshot else { return }
        render(ModelDownloadPresentation.make(store: store, runtime: snapshot.downloadRuntime))
    }

    private func render(_ state: ModelDownloadPresentation?) {
        guard let snapshot else { return }
        let layout = state?.buttons ?? .closeOnly
        let replacing = alert == nil || layout != buttonLayout
        let previous = replacing ? alert : nil
        if replacing {
            let next = snapshot.makeAlert(presentation: state)
            alert = next
            buttonLayout = layout
            next.window.delegate = self
            next.window.isReleasedWhenClosed = false
            if let operation = layout.operationButton(in: next) {
                operation.target = self
                operation.action = #selector(performModelAction(_:))
            }
            if let close = layout.closeButton(in: next) {
                close.target = self
                close.action = #selector(closeAbout(_:))
            }
            downloadLine = nil
            if state != nil {
                let line = NSTextField(wrappingLabelWithString: "")
                line.frame = NSRect(x: 0, y: 0, width: 420, height: 110)
                line.isSelectable = true
                next.accessoryView = line
                downloadLine = line
            }
        }
        guard let alert else { return }
        alert.informativeText = snapshot.textWithoutDownload
        if let state {
            let mark = state.line.needsAttention ? " ⚠︎" : ""
            var text = "\(state.line.label): \(state.line.value)\(mark)"
            if let notice = state.notice { text += "\n" + notice }
            // The same published terms as the zenz attribution in AboutStatus.
            if snapshot.downloadRuntime.hasOtherModel {
                text += "\n取得対象: \(ConversionLmModel.zenz.name)\nライセンス: Apache-2.0（モデルカードの宣言）。tokenizer は ku-nlp/gpt2-small-japanese-char 由来（CC BY-SA 4.0）"
            }
            downloadLine?.stringValue = text
            if let line = downloadLine {
                let bounds = (text as NSString).boundingRect(with: NSSize(width: 420, height: CGFloat.greatestFiniteMagnitude),
                    options: [.usesLineFragmentOrigin, .usesFontLeading],
                    attributes: [.font: line.font ?? NSFont.systemFont(ofSize: NSFont.systemFontSize)])
                line.frame.size.height = ceil(bounds.height) + 8
            }
            if let operation = layout.operationButton(in: alert) {
                operation.title = state.button
                operation.isEnabled = state.action != .none
            }
        }
        alert.layout()
        layout.configureKeyboard(alert)
        if replacing {
            let frame = previous?.window.frame
            // Closing the replaced window must not unsubscribe the new one.
            previous?.window.delegate = nil
            previous?.window.close()
            if let frame {
                alert.window.setFrameTopLeftPoint(NSPoint(x: frame.minX, y: frame.maxY))
            } else {
                alert.window.center()
            }
            NSApp.activate(ignoringOtherApps: true)
            alert.window.makeKeyAndOrderFront(nil)
        }
    }

    // A local key monitor, installed while About is open and removed when it
    // closes. It acts only on the restart-only window, and only on unmodified
    // keys aimed at it: Return and keypad Enter are consumed, Escape closes.
    // No button carries Escape (configureKeyboard clears every key
    // equivalent), so without this the window would have no keyboard close.
    private func handleKey(_ event: NSEvent) -> NSEvent? {
        guard buttonLayout == .restartOnly, let alert, event.window === alert.window,
              event.modifierFlags.intersection([.command, .option, .control, .shift]).isEmpty else { return event }
        switch event.keyCode {
        case 36, 76: return nil // Return and keypad Enter must never restart.
        case 53: alert.window.close(); return nil
        default: return event
        }
    }

    @objc private func performModelAction(_ sender: Any?) {
        guard let alert, let buttonLayout, let button = sender as? NSButton,
              button === buttonLayout.operationButton(in: alert),
              let store = observedStore, let snapshot else { return }
        switch ModelDownloadPresentation.make(store: store, runtime: snapshot.downloadRuntime).action {
        case .acquire: store.acquire()
        case .cancel: store.cancel()
        case .restart:
            // One normal termination request, even if another click arrives
            // while ApplicationLifecycle is flushing. macOS relaunches on input.
            //
            // `NSApp.terminate` takes the normal path: applicationShouldTerminate
            // runs ApplicationLifecycle, so learning, the dictionary stores, the
            // recorder and this download store all settle within the usual 2 s
            // deadline before the process exits. An `exit()` would skip that.
            // Nothing is force-committed: once the input gate closes, the
            // controllers leave any marked text as it is (see
            // InputSessionCoordinator). The button does not relaunch Muu; it
            // relies on macOS starting the input method again for the next
            // input, which then reads the settings acquisition already wrote.
            guard !restartRequested else { return }
            restartRequested = true
            terminate()
        case .none: break
        }
    }

    @objc private func closeAbout(_ sender: Any?) {
        guard let alert, let buttonLayout, let button = sender as? NSButton,
              button === buttonLayout.closeButton(in: alert) else { return }
        alert.window.close()
    }
    func windowWillClose(_ notification: Notification) {
        guard let window = notification.object as? NSWindow, window === alert?.window else { return }
        observedStore?.onChange = nil
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil; buttonLayout = nil
        observedStore = nil; downloadLine = nil; snapshot = nil; alert = nil
        // Closing is not cancellation. The explicitly started single attempt
        // belongs to the process store, and reopening observes it again.
    }

    #if MUU_TEST
    static func forTesting(store: ModelDownloadStore, terminator: @escaping () -> Void = {},
                           capture: @escaping () -> AboutStatusSnapshot) -> AboutPresenter {
        AboutPresenter(capture: capture, store: { store }, terminate: terminator)
    }
    var alertForTesting: NSAlert? { alert }
    func renderForTesting(_ state: ModelDownloadPresentation) { render(state) }
    #endif
}
