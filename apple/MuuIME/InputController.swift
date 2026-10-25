import AppKit
import Darwin
import InputMethodKit
import OSLog

// In the Swift 5 language mode `@MainActor` is checked at compile time
// only. Nothing traps if IMK calls an override off main; the candidate
// surface and badge keep their main-thread preconditions for that reason.
// The controller has no deinit: the panel and badge owners release their
// own windows and timers (finishInputUIOnMain).
@objc(MuuInputController)
@MainActor
final class MuuInputController: IMKInputController {
    private let logger = Logger(subsystem: MuuConstants.bundleIdentifier, category: "controller")
    // Assigned right after super.init, because its client provider
    // captures self weakly. The read-only getters tolerate nil; the
    // IMK entry points assume it is set.
    private var coordinator: InputSessionCoordinator!
    private var presentation: InputPresentation { coordinator.presentation }
    private var composedText: String { coordinator?.presentation.composedText ?? "" }
    private var candidateSurface: CandidatePanelController? { coordinator?.presentation.candidateSurface }
    private var modeBadge: ModeBadgeController? { coordinator?.presentation.modeBadge }
    private var candidateActionAuthority: CandidateActionAuthority {
        get { presentation.candidateActionAuthority }
        set { presentation.candidateActionAuthority = newValue }
    }
    private var currentMode: InputMode { coordinator?.presentation.currentMode ?? .hiragana }
    #if MUU_TEST
    private var applyEventRecorderForTesting: ((String) -> Void)? {
        get { coordinator?.presentation.applyEventRecorderForTesting }
        set { coordinator?.presentation.applyEventRecorderForTesting = newValue }
    }
    var candidateActionAttemptsForTesting: Int { coordinator.candidateActionAttemptsForTesting }
    private var testingClientOverride: IMKTextInput?
    var lastViewForTesting: ViewState? { coordinator?.presentation.lastViewForTesting }
    static var applyObserverForTesting: ((MuuInputController, Bool) -> Void)?
    #endif

    override init!(server: IMKServer!, delegate: Any!, client inputClient: Any!) {
        super.init(server: server, delegate: delegate, client: inputClient)
        coordinator = InputSessionCoordinator(presentation: InputPresentation(), makeSession: { RustEngine() }) { [weak self] in
            #if MUU_TEST
            if let client = self?.testingClientOverride { return client }
            #endif
            return self?.client() as? IMKTextInput
        }
        if !coordinator.isAvailable {
            logger.error("Controller is in pass-through mode because engine creation failed")
        }
    }

    #if MUU_TEST
    init(testingClient: IMKTextInput, modeBadgeScheduler: ModeBadgeScheduler? = nil,
         dictionaryPath: String? = nil, testingSession: InputSession? = nil) {
        super.init()
        testingClientOverride = testingClient
        // In order: a scripted fake, a real engine on a fixture dictionary,
        // or the real engine as in production. All three meet the
        // coordinator through the same InputSession seam.
        coordinator = InputSessionCoordinator(presentation: InputPresentation(modeBadgeScheduler: modeBadgeScheduler ?? .mainRunLoop), makeSession: {
            if let testingSession { return testingSession }
            if let dictionaryPath {
                return RustEngine(testingCommitStream: nil, dictionaryPath: dictionaryPath)
            }
            return RustEngine()
        }) { [weak self] in self?.testingClientOverride ?? (self?.client() as? IMKTextInput) }
    }
    #endif

    override func recognizedEvents(_ sender: Any!) -> Int {
        // flagsChanged is required for a modifier-only Shift tap. Once the
        // mask is extended beyond keyDown, IMK no longer supplies its default
        // outside-click commit, so the mouse events must stay paired with the
        // explicit commit path in handle(_:client:).
        let mask: NSEvent.EventTypeMask = [
            .keyDown,
            .flagsChanged,
            .leftMouseDown,
            .rightMouseDown,
            .otherMouseDown,
        ]
        logger.notice(
            "recognizedEvents mask=\(mask.rawValue, privacy: .public)"
        )
        return Int(mask.rawValue)
    }

    // The selectors stay on this ObjC class and keep their names. They
    // are no longer private, because InputMenuActions builds the
    // #selector references.
    override func menu() -> NSMenu! { InputMenuActions.menu(target: self) }
    @objc func showUserDictionary(_ sender: Any?) { InputMenuActions.showUserDictionary(sender) }
    @objc func showImportedDictionaries(_ sender: Any?) { InputMenuActions.showImportedDictionaries(sender) }
    @objc func showPostalCodeData(_ sender: Any?) { InputMenuActions.showPostalCodeData(sender) }
    @objc func clearPersonalFrequency(_ sender: Any?) { InputMenuActions.clearPersonalFrequency(sender) }
    @objc func showAboutMuu(_ sender: Any?) { InputMenuActions.showAboutMuu(sender) }
    #if MUU_TEST
    static func personalFrequencyClearNotice(for result: PersonalFrequencyClearResult) -> PersonalFrequencyClearNotice? {
        InputMenuActions.personalFrequencyClearNotice(for: result)
    }
    #endif

    override func handle(_ event: NSEvent!, client sender: Any!) -> Bool {
        coordinator.handle(event, client: sender)
    }
    override func activateServer(_ sender: Any!) { coordinator.activateServer(sender) }
    override func deactivateServer(_ sender: Any!) { coordinator.deactivateServer(sender) }
    override func commitComposition(_ sender: Any!) { coordinator.commitComposition(sender) }
    #if MUU_TEST
    private func apply(_ response: EngineResponse?, to client: IMKTextInput) -> Bool {
        coordinator.apply(response, to: client)
    }
    private func hideCandidates() { presentation.hideCandidates() }
    #endif

    override func composedString(_ sender: Any!) -> Any! {
        composedText
    }

    override func value(forTag tag: Int, client sender: Any!) -> Any! {
        if tag == Int(kTextServiceInputModePropertyTag) {
            return currentMode.inputModeIdentifier
        }
        return super.value(forTag: tag, client: sender)
    }

    override func setValue(_ value: Any!, forTag tag: Int, client sender: Any!) {
        guard tag == Int(kTextServiceInputModePropertyTag),
              let identifier = value as? String,
              let mode = InputMode(inputModeIdentifier: identifier)
        else {
            super.setValue(value, forTag: tag, client: sender)
            return
        }
        // A mode chosen by the system (the input menu's mode items) goes to
        // Rust as set_mode, and the reply's view is applied like any key's.
        guard let client = sender as? IMKTextInput else { return }
        coordinator.setMode(mode, to: client)
    }

    #if MUU_TEST
    func setApplyEventRecorderForTesting(
        _ recorder: ((String) -> Void)?
    ) {
        applyEventRecorderForTesting = recorder
    }

    func applyForTesting(
        _ response: EngineResponse?,
        to client: IMKTextInput
    ) -> Bool {
        let consumed = apply(response, to: client)
        Self.applyObserverForTesting?(self, consumed)
        return consumed
    }

    var candidateSnapshotForTesting: CandidateSurfaceSnapshot? {
        candidateSurface?.snapshotForTesting
    }

    var candidateSurfaceVisibleForTesting: Bool {
        candidateSurface?.isVisibleForTesting == true
    }

    var candidateDisplayForTesting: (frame: NSRect, labels: [String], selectedRow: Int)? {
        guard let surface = candidateSurface, surface.isVisibleForTesting else { return nil }
        let labels = (0..<surface.displayedRowCountForTesting).map {
            surface.accessibilityMetadataForTesting(at: $0)?.label ?? ""
        }
        return (surface.frameForTesting, labels, surface.selectedRowForTesting)
    }

    var candidateActionsEnabledForTesting: Bool {
        candidateSurface?.actionsEnabledForTesting == true
    }

    var candidateActionAttemptsCountForTesting: Int {
        candidateActionAttemptsForTesting
    }

    /// Render a production candidate snapshot through the same authority and
    /// panel path used by `synchronizeCandidates`.  This is test-only access
    /// for the WindowServer matrix; it does not duplicate selection logic.
    func renderCandidateSnapshotForTesting(
        _ snapshot: CandidateSurfaceSnapshot,
        anchors: CandidateSurfaceAnchors
    ) {
        guard let authoritySnapshot = candidateActionAuthority.snapshot(
            items: snapshot.items,
            selectedIndex: snapshot.selectedIndex
        ) else {
            hideCandidates()
            return
        }
        candidateSurface?.render(snapshot: authoritySnapshot, anchors: anchors)
    }

    func invalidateCandidateSurfaceForTesting() {
        presentation.invalidateCandidateActionsPreservingPresentation()
    }

    func advanceCandidateGenerationForTesting() {
        candidateSurface?.advanceGenerationPreservingActionsForTesting()
    }

    func hideCandidateSurfaceForTesting() {
        hideCandidates()
    }

    func candidateAccessibilityRowForTesting(at row: Int) -> (label: String, selectable: Bool, pressable: Bool)? {
        candidateSurface?.accessibilityMetadataForTesting(at: row)
    }

    var currentModeForTesting: InputMode {
        currentMode
    }

    var composedTextForTesting: String {
        composedText
    }

    var modeBadgeVisibleForTesting: Bool {
        modeBadge?.isVisibleForTesting == true
    }

    var modeBadgeTextForTesting: String? {
        modeBadge?.displayedTextForTesting
    }

    var modeBadgeFrameForTesting: NSRect? {
        modeBadge?.frameForTesting
    }

    var modeBadgeAlphaForTesting: CGFloat? {
        modeBadge?.alphaValueForTesting
    }

    var modeBadgeGenerationForTesting: UInt64? {
        modeBadge?.generationForTesting
    }

    func finishModeBadgeForTesting(generation: UInt64) {
        modeBadge?.finishPresentationForTesting(generation: generation)
    }
    #endif
}
