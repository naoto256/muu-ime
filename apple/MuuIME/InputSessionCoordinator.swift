import Cocoa
import InputMethodKit
import OSLog

/// Owns one synchronous input session and its presentation. Rust decides key
/// meaning; the coordinator preserves failure and commit boundaries without
/// keeping the client supplied to an IMK callback.
@MainActor
final class InputSessionCoordinator {
    private let logger = Logger(subsystem: MuuConstants.bundleIdentifier, category: "controller")
    private var engine: InputSession?
    let presentation: InputPresentation
    private let clientProvider: () -> IMKTextInput?
    private var composedText: String { presentation.composedText }
    private var hasActiveComposition: Bool { !composedText.isEmpty }
    private var modeBadge: ModeBadgeController? { presentation.modeBadge }
    private var candidateActionAuthority: CandidateActionAuthority { presentation.candidateActionAuthority }
    private var suppressesModeBadgePresentation: Bool {
        get { presentation.suppressesModeBadgePresentation }
        set { presentation.suppressesModeBadgePresentation = newValue }
    }
    var isAvailable: Bool { engine?.isAvailable == true }
    #if MUU_TEST
    private(set) var candidateActionAttemptsForTesting = 0
    #endif

    init(presentation: InputPresentation, makeSession: () -> InputSession,
         clientProvider: @escaping () -> IMKTextInput?) {
        self.presentation = presentation
        self.clientProvider = clientProvider
        // The surface reaches this coordinator weakly. A candidate action
        // takes its client from clientProvider at the time of the action,
        // never from the callback that showed the panel.
        presentation.candidateActionHandler = { [weak self] action in
            _ = self?.handleCandidateSurfaceAction(action)
        }
        if !ApplicationLifecycle.inputGate.isClosing { engine = makeSession() }
    }


    func setMode(_ mode: InputMode, to client: IMKTextInput) {
        guard !ApplicationLifecycle.inputGate.isClosing else { return }
        _ = apply(engine?.handle(CommandRequest(command: "set_mode", arg: .mode(mode))), to: client)
    }

    func handle(_ event: NSEvent!, client sender: Any!) -> Bool {
        guard !ApplicationLifecycle.inputGate.isClosing else { return false }
        logSelector("handleEvent:client:")
        guard let event, let client = sender as? IMKTextInput else { return false }
        if RustCore.isInvalid { return clearInvalidCorePresentation(to: client) }

        // A click anywhere commits the composition and is passed on, so it
        // still reaches the app. recognizedEvents asks for these because IMK
        // stops doing this commit itself once the mask goes beyond keyDown.
        if event.type == .leftMouseDown || event.type == .rightMouseDown || event.type == .otherMouseDown {
            logger.notice("mouse_event type=\(self.eventTypeName(event.type), privacy: .public)")
            if hasActiveComposition {
                commitComposition(client)
            }
            return false
        }

        let modifierRaw = UInt64(event.modifierFlags.rawValue)
        let modifierMask = UInt64(NSEvent.ModifierFlags.deviceIndependentFlagsMask.rawValue)
        let deviceIndependentModifiers = modifierRaw & modifierMask
        let carriesText = event.type == .keyDown || event.type == .keyUp
        let isRepeat = carriesText ? event.isARepeat : false
        let characters = carriesText ? event.characters : nil
        let charactersIgnoringModifiers = carriesText ? event.charactersIgnoringModifiers : nil
        logger.notice(
            "key_event type=\(self.eventTypeName(event.type), privacy: .public) keycode=\(event.keyCode, privacy: .public) modifiers_raw=\(modifierRaw, privacy: .public) modifiers_mask=\(modifierMask, privacy: .public) modifiers_device_independent=\(deviceIndependentModifiers, privacy: .public) repeat=\(isRepeat, privacy: .public)"
        )
        // Swift binds no key to candidate selection, number keys included.
        // Every key goes through Rust's keymap, which picks a candidate by
        // number while converted and otherwise types the digit. Modified,
        // keypad and decorated input therefore keep Rust's ordering and its
        // decision on whether the key is consumed.

        guard engine?.isAvailable == true else {
            logger.error("Key passed through; category=engine_unavailable")
            modeBadge?.hide()
            return false
        }

        #if MUU_TEST
            if UserDefaults.standard.bool(forKey: "MuuPanicOnNextEvent") {
                UserDefaults.standard.set(false, forKey: "MuuPanicOnNextEvent")
                return apply(
                    engine?.handle(CommandRequest(command: "panic_test")),
                    to: client
                )
            }
        #endif

        // The modifiers go to Rust raw, device-dependent bits included; only
        // the log line above splits out the device-independent part.
        let request = EventRequest(
            key: KeyEvent(
                eventType: eventTypeName(event.type),
                keycode: event.keyCode,
                modifiers: UInt64(event.modifierFlags.rawValue),
                charactersUTF8: characters,
                charactersIgnoringModifiers: charactersIgnoringModifiers,
                isRepeat: isRepeat
            )
        )
        let response = engine?.handle(request)
        let consumed = apply(response, to: client)
        return consumed
    }

    func activateServer(_ sender: Any!) {
        guard !ApplicationLifecycle.inputGate.isClosing else { return }
        logSelector("activateServer:")
        if RustCore.isInvalid, let client = sender as? IMKTextInput { _ = clearInvalidCorePresentation(to: client) }
    }

    func deactivateServer(_ sender: Any!) {
        // While closing this skips the usual hides and force_commit too.
        // Marked text and panels stay as they are until the process ends.
        guard !ApplicationLifecycle.inputGate.isClosing else { return }
        logSelector("deactivateServer:")
        if RustCore.isInvalid {
            if let client = sender as? IMKTextInput { _ = clearInvalidCorePresentation(to: client) }
            return
        }
        hideCandidates()
        suppressesModeBadgePresentation = true
        modeBadge?.hide()
        defer {
            // force_commit may report a real Literal -> Reading transition.
            // Preserve that Rust fact while preventing a deactivated
            // controller from presenting feedback for it.
            _ = engine?.handle(CommandRequest(command: "clear_preceding_context"))
            modeBadge?.hide()
            suppressesModeBadgePresentation = false
        }
        if hasActiveComposition, let client = sender as? IMKTextInput {
            commitComposition(client)
        }
    }

    func commitComposition(_ sender: Any!) {
        guard !ApplicationLifecycle.inputGate.isClosing else { return }
        logSelector("commitComposition:")
        if RustCore.isInvalid {
            if let client = sender as? IMKTextInput { _ = clearInvalidCorePresentation(to: client) }
            return
        }
        guard hasActiveComposition, let client = sender as? IMKTextInput else { return }
        let response = engine?.handle(CommandRequest(command: "force_commit"))
        _ = apply(response, to: client)
    }

    /// An event that itself failed is consumed. Learning of failure through
    /// the process flag consumes a key only if this client has display to
    /// discard; an empty client must pass that first key through (docs/src/internals/architecture.md「失敗したとき」).
    ///
    /// Runs once per controller, from whichever entry point first sees the
    /// invalid core. Unconfirmed text is discarded, not committed: the session
    /// that held it can no longer be asked. The engine is dropped because its
    /// session can never be used again, and no replacement is created: that
    /// cannot succeed while the core is invalid (docs/src/internals/architecture.md「失敗したとき」).
    @discardableResult
    private func clearInvalidCorePresentation(
        to client: IMKTextInput, consumeFailedEvent: Bool = false
    ) -> Bool {
        presentation.clearInvalidCorePresentation(to: client, consumeFailedEvent: consumeFailedEvent) {
            self.engine = nil
        }
    }

    /// The two closures keep the Session lifetime with the coordinator:
    /// recreateSession serves SESSION_INVALID/poisoned, and invalidateSession
    /// serves an invalid Core, after which no replacement is made.
    func apply(_ response: EngineResponse?, to client: IMKTextInput) -> Bool {
        guard !ApplicationLifecycle.inputGate.isClosing else { return false }
        return presentation.apply(response, to: client,
            recreateSession: { self.engine?.recreate() },
            invalidateSession: { self.engine = nil })
    }

    // Mouse events return before this is called, so the default branch is
    // not reached by the event types recognizedEvents requests.
    private func eventTypeName(_ type: NSEvent.EventType) -> String {
        switch type {
        case .keyDown: "key_down"
        case .keyUp: "key_up"
        case .flagsChanged: "flags_changed"
        default: "key_down"
        }
    }

    /// A click or key on the candidate panel. The authority refuses an action
    /// from a panel generation that is no longer current. Select-and-commit
    /// commits only when the select succeeded with a view and no structured
    /// error; otherwise the select's view is applied and nothing is committed.
    @discardableResult
    func handleCandidateSurfaceAction(_ action: CandidateSurfaceAction) -> Bool {
        guard !ApplicationLifecycle.inputGate.isClosing else { return false }
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        #if MUU_TEST
        candidateActionAttemptsForTesting += 1
        #endif
        let id: String
        let shouldCommit: Bool
        switch action {
        case let .select(candidateID, _):
            id = candidateID
            shouldCommit = false
        case let .selectAndCommit(candidateID, _):
            id = candidateID
            shouldCommit = true
        }
        guard candidateActionAuthority.accepts(action),
              let inputClient = clientProvider()
        else {
            logger.notice(
                "candidate_surface_action accepted=false commit=\(shouldCommit, privacy: .public)"
            )
            return false
        }

        guard let selected = engine?.handle(
            CommandRequest(
                command: "select_candidate",
                arg: .candidate(id: id)
            )
        ) else { return false }
        logger.notice(
            "candidate_surface_action accepted=true commit=\(shouldCommit, privacy: .public) status_ok=\(selected.status == MUU_RESULT_OK, privacy: .public) has_view=\(selected.view != nil, privacy: .public) has_error=\(selected.view?.error != nil, privacy: .public)"
        )
        guard shouldCommit else {
            return apply(selected, to: inputClient)
        }

        let commitAction = CandidateSurfacePolicy.candidateCommitAction(
            statusOK: selected.status == MUU_RESULT_OK,
            hasView: selected.view != nil,
            hasStructuredError: selected.view?.error != nil
        )
        guard commitAction == .commit else {
            _ = apply(selected, to: inputClient)
            return false
        }
        return apply(engine?.handle(CommandRequest(command: "commit")), to: inputClient)
    }

    func logSelector(_ selector: StaticString) {
        var threadID: UInt64 = 0
        pthread_threadid_np(nil, &threadID)
        logger.notice(
            "selector=\(selector, privacy: .public) thread_id=\(threadID, privacy: .public) is_main=\(Thread.isMainThread, privacy: .public)"
        )
    }

    private func hideCandidates() { presentation.hideCandidates() }

}
