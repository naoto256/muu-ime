import Cocoa
import InputMethodKit
import OSLog

/// Owns the client-facing mirror and UI state for one input controller.
/// It reflects Rust's bytes and UTF-16 values in order; session lifetime remains
/// with the caller, including the distinct Core and Session failure boundaries.
@MainActor
final class InputPresentation {
    private let logger = Logger(subsystem: MuuConstants.bundleIdentifier, category: "controller")
    private(set) var composedText = ""
    private(set) var currentMode = InputMode.hiragana
    private(set) var candidateSurface: CandidatePanelController?
    private(set) var modeBadge: ModeBadgeController?
    var candidateActionAuthority = CandidateActionAuthority()
    var candidateActionHandler: ((CandidateSurfaceAction) -> Void)?
    var suppressesModeBadgePresentation = false
    private var hasActiveComposition: Bool { !composedText.isEmpty }
    private var clearedInvalidCorePresentation = false
    #if MUU_TEST
    var applyEventRecorderForTesting: ((String) -> Void)?
    private(set) var lastViewForTesting: ViewState?
    #endif

    /// A non-nil scheduler marks a test fixture. As in the earlier test-only
    /// controller init, it also pins the badge to one 1024x768 screen.
    /// Production passes nil and reads NSScreen.
    init(modeBadgeScheduler: ModeBadgeScheduler? = nil) {
        candidateSurface = CandidatePanelController { [weak self] action in
            self?.candidateActionHandler?(action)
        }
        #if MUU_TEST
        if let scheduler = modeBadgeScheduler {
            modeBadge = ModeBadgeController(screenProvider: {
                [CandidateScreenFrame(frame: NSRect(x: 0, y: 0, width: 1_024, height: 768),
                    visibleFrame: NSRect(x: 0, y: 0, width: 1_024, height: 768))]
            }, scheduler: scheduler)
        } else {
            modeBadge = ModeBadgeController()
        }
        #else
        modeBadge = ModeBadgeController()
        #endif
    }

    @discardableResult
    func clearInvalidCorePresentation(
        to client: IMKTextInput, consumeFailedEvent: Bool = false,
        invalidateSession: () -> Void
    ) -> Bool {
        guard !clearedInvalidCorePresentation else { return false }
        let hadPresentation = hasActiveComposition
            || candidateSurface?.isVisible == true || modeBadge?.isVisible == true
        clearedInvalidCorePresentation = true
        #if MUU_TEST
        lastViewForTesting = nil
        #endif
        if hasActiveComposition {
            #if MUU_TEST
            applyEventRecorderForTesting?("setMarkedText")
            #endif
            client.setMarkedText("", selectionRange: NSRange(location: NSNotFound, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
            composedText = ""
        }
        hideCandidates()
        modeBadge?.hide()
        invalidateSession()
        return consumeFailedEvent || hadPresentation
    }

    /// The Session callbacks run where the old controller touched its
    /// engine: recreateSession before the candidate and badge hides, and
    /// invalidateSession at the end of the single Core-invalid clear. The
    /// presentation never keeps a session of its own.
    func apply(_ response: EngineResponse?, to client: IMKTextInput,
               recreateSession: () -> Void, invalidateSession: () -> Void) -> Bool {
        // RustEngine.handle records terminal status once. Here we only decide
        // whether the attempted key was lost or there is display to discard.
        if RustCore.isInvalid {
            return clearInvalidCorePresentation(
                to: client, consumeFailedEvent: response?.status == MUU_RESULT_CORE_INVALID,
                invalidateSession: invalidateSession)
        }
        // No response means there is no session (it was never created, or
        // was dropped). The key is consumed only while marked text is shown,
        // so it cannot leak into the app beside a composition.
        guard let response else {
            #if MUU_TEST
            lastViewForTesting = nil
            #endif
            invalidateCandidateActionsPreservingPresentation()
            return hasActiveComposition
        }
        #if MUU_TEST
        lastViewForTesting = response.view
        #endif
        // Session-level failure: only this input client is affected and the
        // core's shared data is intact, so a fresh session is enough. The core
        // check above must come first; recreating against an invalid core
        // would repeat on every key.
        if response.status == MUU_RESULT_SESSION_INVALID || response.view?.poisoned == true {
            logger.fault("Rust panic caught; ending composition and recreating engine")
            if hasActiveComposition {
                #if MUU_TEST
                applyEventRecorderForTesting?("setMarkedText")
                #endif
                client.setMarkedText(
                    "",
                    selectionRange: NSRange(location: NSNotFound, length: 0),
                    replacementRange: NSRange(location: NSNotFound, length: 0)
                )
                composedText = ""
            }
            recreateSession()
            hideCandidates()
            modeBadge?.hide()
            return true
        }
        guard response.status == MUU_RESULT_OK else {
            logger.error("Engine event failed; result=\(response.status, privacy: .public)")
            invalidateCandidateActionsPreservingPresentation()
            return hasActiveComposition
        }
        if let commit = response.recoveredCommitText {
            #if MUU_TEST
            applyEventRecorderForTesting?("insertText")
            #endif
            client.insertText(commit, replacementRange: NSRange(location: NSNotFound, length: 0))
            // insertText replaces the old marked text. No unverified ranges or
            // mode from the broken display are applied to the client.
            // Known limit: Rust may still hold a next preedit that this reply
            // could not show. Until the next response redraws it, a click or
            // deactivate sees no composition here and does not commit it.
            composedText = ""
            hideCandidates()
            modeBadge?.hide()
            return true
        }
        guard let view = response.view else {
            logger.error("Engine event failed; result=\(response.status, privacy: .public)")
            invalidateCandidateActionsPreservingPresentation()
            return hasActiveComposition
        }

        let candidateCount = view.candidates?.items.count ?? 0
        let hasCommitText = view.commitText != nil
        let focusedRunIndex = view.preedit.firstIndex(where: \.isFocused) ?? -1
        let segmentCount = view.segments?.count ?? 0
        let selectedIndex = view.candidates?.selectedIndex ?? -1
        let errorCode = view.error?.code ?? "none"
        let requestedLength = integerErrorParameter(view.error, named: "requested_length")
        let currentLength = integerErrorParameter(view.error, named: "current_length")
        let errorFocusedSegment = integerErrorParameter(view.error, named: "focused_segment")
        let errorSegmentCount = integerErrorParameter(view.error, named: "segment_count")
        logger.notice(
            "view_apply consumed=\(view.consumed, privacy: .public) caret_utf16=\(view.caretUTF16, privacy: .public) has_commit=\(hasCommitText, privacy: .public) runs=\(view.preedit.count, privacy: .public) focused_run=\(focusedRunIndex, privacy: .public) segments=\(segmentCount, privacy: .public) candidates=\(candidateCount, privacy: .public) selected_index=\(selectedIndex, privacy: .public) error_code=\(errorCode, privacy: .public) error_requested=\(requestedLength, privacy: .public) error_current=\(currentLength, privacy: .public) error_focus=\(errorFocusedSegment, privacy: .public) error_segments=\(errorSegmentCount, privacy: .public)"
        )
        // A structured error (for example a segment resize that does not
        // fit) leaves the composition as Rust kept it; only candidate actions
        // from the old panel are invalidated. With a commit the error must not
        // return early: the commit and the next preedit are applied below, and
        // only the candidate panel is withheld.
        if view.error != nil && view.commitText == nil {
            logger.error("view_error preserved current composition")
            invalidateCandidateActionsPreservingPresentation()
            return view.consumed
        }

        if let commitText = view.commitText {
            logger.notice("commit_text insert")
            #if MUU_TEST
            applyEventRecorderForTesting?("insertText")
            #endif
            client.insertText(
                commitText,
                replacementRange: NSRange(location: NSNotFound, length: 0)
            )
            composedText = ""
            hideCandidates()
            currentMode = view.mode
            #if MUU_TEST
            applyEventRecorderForTesting?("modeUpdate")
            #endif
            // A printable key received during conversion commits the selected
            // segments and starts the next preedit in the same Rust response.
            if view.preedit.isEmpty {
                showModeBadgeIfNeeded(view: view, client: client)
                return view.consumed
            }
        }

        let markedText = MarkedTextRenderer.text(view.preedit)
        if markedText.length > 0 {
            let selection = view.selection.map {
                NSRange(location: $0.start, length: $0.len)
            } ?? NSRange(location: view.caretUTF16, length: 0)
            #if MUU_TEST
            applyEventRecorderForTesting?("setMarkedText")
            #endif
            client.setMarkedText(
                markedText,
                selectionRange: selection,
                replacementRange: NSRange(location: NSNotFound, length: 0)
            )
            logger.notice(
                "marked_text update utf16_len=\(markedText.length, privacy: .public) runs=\(view.preedit.count, privacy: .public)"
            )
            composedText = view.preedit.map(\.text).joined()
        } else if hasActiveComposition {
            #if MUU_TEST
            applyEventRecorderForTesting?("setMarkedText")
            #endif
            client.setMarkedText(
                "",
                selectionRange: NSRange(location: NSNotFound, length: 0),
                replacementRange: NSRange(location: NSNotFound, length: 0)
            )
            composedText = ""
        }
        currentMode = view.mode
        #if MUU_TEST
        applyEventRecorderForTesting?("modeUpdate")
        #endif
        synchronizeCandidates(view.error == nil ? view.candidates : nil, view: view, client: client)
        showModeBadgeIfNeeded(view: view, client: client)
        return view.consumed
    }

    // The badge appears only for a reported mode transition, and never while
    // candidates or segments are on screen or while a deactivation suppresses it.
    private func showModeBadgeIfNeeded(
        view: ViewState,
        client: IMKTextInput
    ) {
        let candidatesAreActive = view.segments != nil
            || !(view.candidates?.items.isEmpty ?? true)
        guard !suppressesModeBadgePresentation, !candidatesAreActive else {
            modeBadge?.hide()
            return
        }
        guard let transition = view.compositionModeTransition else { return }
        #if MUU_TEST
        applyEventRecorderForTesting?("modeBadge")
        #endif
        modeBadge?.show(
            transition: transition,
            view: view,
            client: client
        )
    }

    private func integerErrorParameter(_ error: ViewError?, named name: String) -> Int {
        guard let value = error?.params[name], case let .number(number) = value else {
            return -1
        }
        return Int(number)
    }

    private func synchronizeCandidates(
        _ candidates: CandidateList?,
        view: ViewState,
        client: IMKTextInput
    ) {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        #if MUU_TEST
        applyEventRecorderForTesting?("candidateSync")
        #endif
        guard let candidates,
              !candidates.items.isEmpty,
              candidates.items.indices.contains(candidates.selectedIndex),
              let candidateSurface
        else {
            hideCandidates()
            return
        }

        let items = candidates.items.map {
            CandidateSurfaceItem(
                id: $0.id,
                text: $0.text,
                annotation: $0.annotation
            )
        }
        guard let snapshot = candidateActionAuthority.snapshot(
            items: items,
            selectedIndex: candidates.selectedIndex
        ) else {
            logger.error(
                "candidate_surface snapshot_valid=false count=\(items.count, privacy: .public)"
            )
            hideCandidates()
            return
        }
        let anchors = ClientGeometryReader.anchors(
            view: view,
            client: client
        )
        guard let anchors else {
            // The geometry adapter already observed markedRange. Do not query
            // the client again merely to build a diagnostic message.
            logger.error("candidate_surface geometry_complete=false")
            hideCandidates()
            return
        }
        #if MUU_TEST
        applyEventRecorderForTesting?("candidateRender")
        #endif
        candidateSurface.render(snapshot: snapshot, anchors: anchors)
    }

    // Hiding also invalidates the action authority, so a click that arrives
    // after the panel closed cannot select from it.
    func hideCandidates() {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        #if MUU_TEST
        applyEventRecorderForTesting?("candidateHide")
        #endif
        candidateSurface?.hide()
        candidateActionAuthority.invalidate()
    }

    // Keeps the panel on screen but makes its rows inert, for responses that
    // leave the composition unchanged (failures and structured errors).
    func invalidateCandidateActionsPreservingPresentation() {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        #if MUU_TEST
        applyEventRecorderForTesting?("candidateInvalidate")
        #endif
        candidateSurface?.invalidateActionsPreservingPresentation()
        candidateActionAuthority.invalidate()
    }
}

/// AppKit attributes belong to the marked-text surface, while Rust owns the
/// text, clause order, focus, and UTF-16 selection/caret.
@MainActor
enum MarkedTextRenderer {
    static func text(_ runs: [PreeditRun]) -> NSAttributedString {
        let result = NSMutableAttributedString()
        for (clauseIndex, run) in runs.enumerated() {
            result.append(
                NSAttributedString(
                    string: run.text,
                    attributes: Self.preeditAttributes(
                        // A raw preedit owns a zero-length insertion point.
                        // Painting that whole run as a focused selection makes
                        // AppKit clients suppress the caret inside marked text.
                        // Focus chrome is only meaningful for converted clauses.
                        isFocused: run.isFocused && run.isConverted,
                        clauseIndex: clauseIndex
                    )
                )
            )
        }
        return result
    }

    static func preeditAttributes(
        isFocused: Bool,
        clauseIndex: Int
    ) -> [NSAttributedString.Key: Any] {
        var attributes: [NSAttributedString.Key: Any] = [
            .underlineStyle: (
                isFocused ? NSUnderlineStyle.thick : NSUnderlineStyle.single
            ).rawValue,
            .markedClauseSegment: NSNumber(value: clauseIndex),
        ]
        if isFocused {
            attributes[.backgroundColor] =
                NSColor.selectedTextBackgroundColor.withAlphaComponent(0.28)
        }
        return attributes
    }
}
