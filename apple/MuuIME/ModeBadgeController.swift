import AppKit
import InputMethodKit

@MainActor
private final class ModeBadgePanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

/// The small "あ" / "A" badge shown near the text when Rust reports a
/// composition mode transition. It holds for 0.8 s, then fades over 0.2 s.
/// Like the candidate panel it never takes focus, and it also ignores the
/// mouse. Every show or hide advances `presentationGeneration`; a timer from
/// an earlier presentation sees the mismatch and does nothing.
@MainActor
final class ModeBadgeController {
    private static let contentSize = NSSize(width: 32, height: 28)
    private static let holdDuration: TimeInterval = 0.8
    private static let fadeDuration: TimeInterval = 0.2
    private static let fadeInterval: TimeInterval = 1.0 / 30.0

    private let panel: ModeBadgePanel
    private let label = NSTextField(labelWithString: "")
    private let screenProvider: () -> [CandidateScreenFrame]
    private let scheduler: ModeBadgeScheduler
    private var holdTask: ModeBadgeScheduledTask?
    private var fadeTask: ModeBadgeScheduledTask?
    private var presentationGeneration: UInt64 = 0

    init(
        screenProvider: @escaping () -> [CandidateScreenFrame] = {
            NSScreen.screens.map {
                CandidateScreenFrame(
                    frame: $0.frame,
                    visibleFrame: $0.visibleFrame,
                    backingScale: $0.backingScaleFactor
                )
            }
        },
        // Optional because a default argument is evaluated outside the main
        // actor; `.mainRunLoop` there is a Swift 6 error. Resolved in the body.
        scheduler: ModeBadgeScheduler? = nil
    ) {
        precondition(Thread.isMainThread, "Mode badge is main-thread only")
        self.screenProvider = screenProvider
        self.scheduler = scheduler ?? .mainRunLoop
        panel = ModeBadgePanel(
            contentRect: NSRect(origin: .zero, size: Self.contentSize),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        configurePanel()
    }

    deinit {
        // Timer callbacks hold this owner weakly. Transfer only cancellation
        // tokens and the panel, never self or an IMK client, to main cleanup.
        let hold = holdTask
        let fade = fadeTask
        let panel = panel
        finishInputUIOnMain {
            hold?.cancel()
            fade?.cancel()
            panel.orderOut(nil)
            panel.close()
        }
    }

    func show(
        transition: CompositionModeTransition,
        view: ViewState,
        client: IMKTextInput
    ) {
        precondition(Thread.isMainThread, "Mode badge is main-thread only")
        // No usable geometry hides the badge rather than guessing a spot.
        guard let geometry = ClientGeometryReader.badgeGeometry(
            view: view,
            client: client,
            screens: screenProvider(),
            contentSize: Self.contentSize
        ) else {
            hide()
            return
        }

        presentationGeneration &+= 1
        let generation = presentationGeneration
        invalidateTimers()
        label.stringValue = transition == .literal ? "A" : "あ"
        panel.alphaValue = 1
        panel.setFrame(geometry.frame, display: panel.isVisible)

        let previousKeyWindow = NSApp.keyWindow
        let previousMainWindow = NSApp.mainWindow
        let previousFirstResponder = previousKeyWindow?.firstResponder
        panel.orderFrontRegardless()
        precondition(
            NSApp.keyWindow === previousKeyWindow
                && NSApp.mainWindow === previousMainWindow
                && NSApp.keyWindow?.firstResponder === previousFirstResponder,
            "Mode badge must not take key, main, or client focus"
        )

        holdTask = scheduler.schedule(Self.holdDuration, false) { [weak self] in
            guard let self else { return false }
            self.beginFadeIfCurrent(generation: generation)
            return false
        }
    }

    func hide() {
        precondition(Thread.isMainThread, "Mode badge is main-thread only")
        presentationGeneration &+= 1
        invalidateTimers()
        panel.orderOut(nil)
        panel.alphaValue = 1
    }

    private func configurePanel() {
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.ignoresMouseEvents = true
        panel.level = .popUpMenu
        panel.collectionBehavior = [.transient, .ignoresCycle]

        let background = NSVisualEffectView(frame: panel.contentView?.bounds ?? .zero)
        background.autoresizingMask = [.width, .height]
        background.material = .popover
        background.blendingMode = .behindWindow
        background.state = .active
        background.wantsLayer = true
        background.layer?.cornerRadius = 7

        label.frame = background.bounds
        label.autoresizingMask = [.width, .height]
        label.alignment = .center
        label.font = .systemFont(ofSize: 16, weight: .semibold)
        label.textColor = .labelColor
        background.addSubview(label)
        panel.contentView = background
    }

    private func beginFadeIfCurrent(generation: UInt64) {
        precondition(Thread.isMainThread, "Mode badge is main-thread only")
        guard generation == presentationGeneration else { return }
        holdTask = nil
        // Alpha follows elapsed time from the scheduler's clock, not the tick
        // count, so a late tick does not stretch the fade.
        let startedAt = scheduler.now()
        fadeTask = scheduler.schedule(Self.fadeInterval, true) { [weak self] in
            guard let self, generation == self.presentationGeneration else { return false }
            let elapsed = self.scheduler.now() - startedAt
            let progress = min(max(elapsed / Self.fadeDuration, 0), 1)
            self.panel.alphaValue = 1 - progress
            if progress >= 1 {
                self.fadeTask = nil
                self.finishPresentationIfCurrent(generation: generation)
                return false
            }
            return true
        }
    }

    // Alpha is reset after ordering out, so the next show starts opaque.
    private func finishPresentationIfCurrent(generation: UInt64) {
        guard generation == presentationGeneration else { return }
        panel.orderOut(nil)
        panel.alphaValue = 1
    }

    private func invalidateTimers() {
        holdTask?.cancel()
        holdTask = nil
        fadeTask?.cancel()
        fadeTask = nil
    }

    /// Read on main, just like every other panel operation. The input
    /// controller uses actual visibility so a hidden panel cannot consume
    /// a key when Core becomes unavailable (docs/src/internals/architecture.md「失敗したとき」).
    var isVisible: Bool { panel.isVisible }

    #if MUU_TEST
    var isVisibleForTesting: Bool { isVisible }
    var displayedTextForTesting: String { label.stringValue }
    var frameForTesting: NSRect { panel.frame }
    var windowNumberForTesting: Int { panel.windowNumber }
    var alphaValueForTesting: CGFloat { panel.alphaValue }
    var generationForTesting: UInt64 { presentationGeneration }

    func finishPresentationForTesting(generation: UInt64) {
        finishPresentationIfCurrent(generation: generation)
    }
    #endif
}
