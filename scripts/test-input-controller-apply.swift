import AppKit
import CoreFoundation
import InputMethodKit

enum MuuConstants {
    static let bundleIdentifier = "dev.muu.inputmethod.Muu.apply-smoke"
    static let expectedABIVersion: UInt32 = 13
}

/// Optional comparison artifact for the input presentation contract. Observes
/// the fake client's public effects and actual encoded FFI requests; it never
/// drives input.
/// Text is preserved as UTF-8 bytes, with no normalization for the comparison.
@MainActor
private enum InputPathContractTrace {
    static let output = ProcessInfo.processInfo.environment["MUU_INPUT_PATH_TRACE"]
    private static var rows: [[String: Any]] = []
    // A deallocated controller's address may be reused later in the fixture.
    // Weak object keys keep labels stable without extending its UI lifetime.
    private static let controllers = NSMapTable<MuuInputController, NSNumber>(
        keyOptions: [.weakMemory, .objectPointerPersonality], valueOptions: .strongMemory)
    private static var nextController = 0
    private static var clients = 0

    static func clientIdentifier() -> Int {
        clients += 1
        return clients
    }

    static func record(_ operation: String, _ fields: [String: Any]) {
        guard output != nil else { return }
        precondition(Thread.isMainThread, "input effects are observed on main")
        rows.append(["operation": operation, "facts": fields])
    }

    static func bytes(_ value: String) -> String { Data(value.utf8).base64EncodedString() }
    static func range(_ value: NSRange) -> [String: Int] {
        ["location": value.location, "length": value.length]
    }
    static func number(_ value: CGFloat) -> Any {
        value.isFinite ? Double(value) : value.isNaN ? "nan" : value > 0 ? "+infinity" : "-infinity"
    }
    static func rect(_ value: NSRect) -> [String: Any] {
        ["x": number(value.origin.x), "y": number(value.origin.y),
         "width": number(value.size.width), "height": number(value.size.height)]
    }
    static func attributes(_ value: Any?) -> [[String: Any]] {
        guard let text = value as? NSAttributedString else { return [] }
        var result: [[String: Any]] = []
        text.enumerateAttributes(in: NSRange(location: 0, length: text.length)) { attrs, span, _ in
            var item: [String: Any] = ["range": range(span), "keys": attrs.keys.map(\.rawValue).sorted()]
            item["underline"] = attrs[.underlineStyle] as? Int ?? -1
            item["clause"] = attrs[.markedClauseSegment] as? Int ?? -1
            if let color = (attrs[.backgroundColor] as? NSColor)?.usingColorSpace(.sRGB) {
                item["background_rgba"] = [color.redComponent, color.greenComponent, color.blueComponent, color.alphaComponent].map(number)
            } else { item["background_rgba"] = NSNull() }
            result.append(item)
        }
        return result
    }

    static func request(_ data: Data) {
        record("ffi_request", ["envelope_header": Array(data.prefix(3)).map(Int.init),
            "json": try! JSONSerialization.jsonObject(with: data.dropFirst(3))])
    }

    static func committedRecord(_ data: Data) {
        record("commit_stream", ["json": try! JSONSerialization.jsonObject(with: data)])
    }

    static func learning(_ data: Data, version: LearningVersion) {
        record("learning_received", ["generation": version.generation, "revision": version.revision,
            "bytes": data.base64EncodedString()])
    }

    static func applied(_ controller: MuuInputController, consumed: Bool) {
        if controllers.object(forKey: controller) == nil {
            nextController += 1
            controllers.setObject(NSNumber(value: nextController), forKey: controller)
        }
        var fields: [String: Any] = ["controller": controllers.object(forKey: controller)!.intValue, "consumed": consumed,
            "composed_utf8": bytes(controller.composedTextForTesting), "mode": controller.currentModeForTesting.rawValue,
            "candidates_visible": controller.candidateSurfaceVisibleForTesting,
            "actions_enabled": controller.candidateActionsEnabledForTesting,
            "badge_visible": controller.modeBadgeVisibleForTesting,
            "badge_text": controller.modeBadgeTextForTesting as Any? ?? NSNull()]
        if let snapshot = controller.candidateSnapshotForTesting {
            fields["candidates"] = snapshot.items.map { ["id": $0.id, "utf8": bytes($0.text),
                "annotation_utf8": $0.annotation.map(bytes) as Any? ?? NSNull()] as [String: Any] }
            fields["selected_index"] = snapshot.selectedIndex
            fields["generation"] = snapshot.generation
        } else { fields["candidates"] = NSNull() }
        if let display = controller.candidateDisplayForTesting {
            fields["panel"] = ["frame": rect(display.frame), "row_labels_utf8": display.labels.map(bytes),
                "selected_row": display.selectedRow]
        }
        if controller.modeBadgeVisibleForTesting {
            fields["badge_frame"] = controller.modeBadgeFrameForTesting.map(rect)
            fields["badge_alpha"] = controller.modeBadgeAlphaForTesting.map(number)
        }
        record("applied", fields)
    }

    static func finish() {
        guard let output else { return }
        let lines = rows.map { try! JSONSerialization.data(withJSONObject: $0, options: [.sortedKeys]) }
        var data = Data()
        for line in lines { data.append(line); data.append(10) }
        try! data.write(to: URL(fileURLWithPath: output), options: [.atomic])
        print("input-path contract trace: \(rows.count) observations")
    }
}

private enum ClientCommand: Equatable {
    case insert(String)
    case mark(String)
}

/// The scripted boundary has no Rust handle or persistence side effects.
/// The real RustEngine is covered separately by the frozen request/effect trace.
@MainActor
private final class ScriptedInputSession: InputSession {
    var isAvailable = true
    var responses: [EngineResponse] = []
    private(set) var requests: [Data] = []
    private(set) var recreations = 0
    func handle(_ request: EventRequest) -> EngineResponse { next(try! MuuEnvelope.encode(request)) }
    func handle(_ request: CommandRequest) -> EngineResponse { next(try! MuuEnvelope.encode(request)) }
    func recreate() { recreations += 1 }
    private func next(_ request: Data) -> EngineResponse {
        requests.append(request)
        precondition(!responses.isEmpty, "unexpected session operation")
        return responses.removeFirst()
    }
}

private enum PersonalFrequencyFixtureError: Error {
    case replace
    case persist
}

private enum FixtureCoreError: Error, Equatable { case installFailed(Int32); case recoveryFailed }

private final class ManualPersistenceScheduler {
    private let lock = NSLock()
    private var now: TimeInterval = 0
    private var entries: [(TimeInterval, () -> Void)] = []
    private var delays: [TimeInterval] = []
    func schedule(_ delay: TimeInterval, _ work: @escaping () -> Void) {
        lock.lock(); entries.append((now + delay, work)); delays.append(delay); lock.unlock()
    }
    func advance(_ delta: TimeInterval) {
        lock.lock(); now += delta
        let due = entries.filter { $0.0 <= now }.map { $0.1 }
        entries.removeAll { $0.0 <= now }; lock.unlock()
        due.forEach { $0() }
    }
    var scheduledDelays: [TimeInterval] { lock.lock(); defer { lock.unlock() }; return delays }
    var count: Int { lock.lock(); defer { lock.unlock() }; return entries.count }
}

@MainActor
private final class ManualModeBadgeScheduler {
    private struct Entry {
        let repeats: Bool
        let action: () -> Bool
        var isActive: Bool
    }

    private var entries: [Entry] = []
    private(set) var scheduledIntervals: [TimeInterval] = []
    var now: TimeInterval = 0
    var activeTaskCount: Int { entries.filter(\.isActive).count }

    lazy var scheduler = ModeBadgeScheduler(
        now: { [unowned self] in self.now },
        schedule: { [unowned self] interval, repeats, action in
            let index = self.entries.count
            self.scheduledIntervals.append(interval)
            self.entries.append(Entry(repeats: repeats, action: action, isActive: true))
            return ModeBadgeScheduledTask { [weak self] in
                precondition(Thread.isMainThread, "badge cancellation must run on main")
                guard let self, self.entries.indices.contains(index) else { return }
                self.entries[index].isActive = false
            }
        }
    )

    func fireNext() {
        guard let index = entries.firstIndex(where: \.isActive) else {
            preconditionFailure("manual mode badge scheduler has no active task")
        }
        let entry = entries[index]
        let shouldRepeat = entry.action()
        if !entry.repeats || !shouldRepeat {
            entries[index].isActive = false
        }
    }
}

@MainActor
private final class FakeTextClient: NSObject, @preconcurrency IMKTextInput {
    private lazy var traceIdentifier = InputPathContractTrace.clientIdentifier()
    private(set) var commands: [ClientCommand] = []
    private(set) var geometryQueries: [NSRange] = []
    private(set) var markedSelectionRange = NSRange(location: NSNotFound, length: 0)
    private(set) var markedText: NSAttributedString?
    private var documentMarkedRange = NSRange(location: NSNotFound, length: 0)
    private var documentSelectedRange = NSRange(location: 100, length: 0)
    private var firstRectOverride: ((NSRange) -> CandidateClientRectResult)?
    private(set) var attributeQueries: [Int] = []
    var attributeRectForTesting: NSRect?
    var attributeResponseForTesting: ((Int) -> (rect: NSRect, orientation: Int))?
    var attributeValuesForTesting: [AnyHashable: Any] = ["CANARY_ATTRIBUTE_KEY": "CANARY_ATTRIBUTE_BODY"]
    var geometryEventForTesting: ((String) -> Void)?

    func configureGeometry(
        markedRange: NSRange,
        selectedRange: NSRange,
        firstRect: @escaping (NSRange) -> CandidateClientRectResult
    ) {
        documentMarkedRange = markedRange
        documentSelectedRange = selectedRange
        firstRectOverride = firstRect
        geometryQueries.removeAll()
    }

    func insertText(_ string: Any!, replacementRange: NSRange) {
        InputPathContractTrace.record("client_insert", ["client": traceIdentifier,
            "utf8": InputPathContractTrace.bytes(Self.plainText(string)),
            "replacement": InputPathContractTrace.range(replacementRange)])
        commands.append(.insert(Self.plainText(string)))
        documentMarkedRange = NSRange(location: NSNotFound, length: 0)
    }

    func setMarkedText(
        _ string: Any!,
        selectionRange: NSRange,
        replacementRange: NSRange
    ) {
        InputPathContractTrace.record("client_mark", ["client": traceIdentifier,
            "utf8": InputPathContractTrace.bytes(Self.plainText(string)),
            "selection": InputPathContractTrace.range(selectionRange),
            "replacement": InputPathContractTrace.range(replacementRange),
            "attributes": InputPathContractTrace.attributes(string)])
        let text = Self.plainText(string)
        commands.append(.mark(text))
        markedSelectionRange = selectionRange
        markedText = string as? NSAttributedString
        documentMarkedRange = text.isEmpty
            ? NSRange(location: NSNotFound, length: 0)
            : NSRange(location: 100, length: (text as NSString).length)
        documentSelectedRange = NSRange(
            location: documentMarkedRange.location == NSNotFound
                ? 100
                : documentMarkedRange.location + selectionRange.location,
            length: selectionRange.length
        )
    }

    func selectedRange() -> NSRange {
        InputPathContractTrace.record("client_selected_range", ["client": traceIdentifier,
            "result": InputPathContractTrace.range(documentSelectedRange)])
        geometryEventForTesting?("selectedRange")
        return documentSelectedRange
    }

    func markedRange() -> NSRange {
        InputPathContractTrace.record("client_marked_range", ["client": traceIdentifier,
            "result": InputPathContractTrace.range(documentMarkedRange)])
        geometryEventForTesting?("markedRange")
        return documentMarkedRange
    }

    func attributedSubstring(from range: NSRange) -> NSAttributedString! {
        nil
    }

    func length() -> Int {
        0
    }

    func characterIndex(
        for point: NSPoint,
        tracking mappingMode: IMKLocationToOffsetMappingMode,
        inMarkedRange: UnsafeMutablePointer<ObjCBool>!
    ) -> Int {
        NSNotFound
    }

    func attributes(
        forCharacterIndex index: Int,
        lineHeightRectangle lineRect: UnsafeMutablePointer<NSRect>!
    ) -> [AnyHashable: Any]! {
        geometryEventForTesting?("attributes")
        attributeQueries.append(index)
        if let response = attributeResponseForTesting?(index) {
            lineRect.pointee = response.rect
            return ["IMKTextOrientation": response.orientation]
        }
        if let rect = attributeRectForTesting { lineRect.pointee = rect }
        return attributeValuesForTesting
    }

    func validAttributesForMarkedText() -> [Any]! {
        []
    }

    func overrideKeyboard(withKeyboardNamed keyboardUniqueName: String!) {}

    func selectMode(_ modeIdentifier: String!) {}

    func supportsUnicode() -> Bool {
        true
    }

    func bundleIdentifier() -> String! {
        MuuConstants.bundleIdentifier
    }

    func windowLevel() -> CGWindowLevel {
        CGWindowLevelForKey(.popUpMenuWindow)
    }

    func supportsProperty(_ property: TSMDocumentPropertyTag) -> Bool {
        false
    }

    func uniqueClientIdentifierString() -> String! {
        "apply-smoke-client"
    }

    func string(
        from range: NSRange,
        actualRange: NSRangePointer
    ) -> String! {
        actualRange.pointee = range
        return ""
    }

    func firstRect(
        forCharacterRange range: NSRange,
        actualRange: NSRangePointer
    ) -> NSRect {
        geometryEventForTesting?("firstRect")
        geometryQueries.append(range)
        if let firstRectOverride {
            let result = firstRectOverride(range)
            actualRange.pointee = result.actualRange
            InputPathContractTrace.record("client_first_rect", ["client": traceIdentifier,
                "requested": InputPathContractTrace.range(range), "actual": InputPathContractTrace.range(result.actualRange),
                "rect": InputPathContractTrace.rect(result.rect)])
            return result.rect
        }
        actualRange.pointee = range
        let offset = CGFloat(max(range.location - 100, 0))
        let rect = NSRect(x: 240 + offset * 8, y: 520, width: 8, height: 20)
        InputPathContractTrace.record("client_first_rect", ["client": traceIdentifier,
            "requested": InputPathContractTrace.range(range), "actual": InputPathContractTrace.range(range),
            "rect": InputPathContractTrace.rect(rect)])
        return rect
    }

    private static func plainText(_ value: Any?) -> String {
        if let attributed = value as? NSAttributedString {
            return attributed.string
        }
        return value as? String ?? ""
    }
}

@main
@MainActor
private enum InputControllerApplyCharacterization {
    static func main() {
        _ = NSApplication.shared
        if InputPathContractTrace.output != nil {
            NSApp.appearance = NSAppearance(named: .aqua)
            RustEngine.requestObserverForTesting = InputPathContractTrace.request
            RustEngine.recordObserverForTesting = InputPathContractTrace.committedRecord
            PersonalFrequencyStore.receiveObserverForTesting = InputPathContractTrace.learning
            MuuInputController.applyObserverForTesting = InputPathContractTrace.applied
        }
        defer { InputPathContractTrace.finish() }
        guard CommandLine.arguments.count == 3 else {
            preconditionFailure("usage: test-input-controller-apply <dictionary-path> <case>")
        }
        let dictionaryPath = CommandLine.arguments[1]
        let testCase = CommandLine.arguments[2]
        if testCase == "cleared-learning-probe" {
            let store = PersonalFrequencyStore(baseDirectory: URL(fileURLWithPath: dictionaryPath), bundleIdentifier: "test")
            precondition(store.startupData == nil && store.loadError == nil && store.persistenceStatus.fileState == .absent,
                "new process starts with absent learning after clear")
            return
        }
        switch testCase {
        case "scripted-session":
            initializeTestCore(dictionaryPath: nil)
            assertForcedCreateFailurePolicy()
            assertScriptedSessionBoundary()
            assertMainActorCleanupBoundary()
            assertRealUIOffMainTeardown()
        case "controller":
            initializeTestCore(dictionaryPath: nil)
            // IMK/AppKit objects may outlive a Swift local through autorelease.
            // Each fixture releases its clients before the next pumps the loop.
            autoreleasepool { assertControllerCharacterization() }
            autoreleasepool { assertUnicodeAndRawEnvelopeContract() }
            autoreleasepool { assertAttributeCandidateFallback() }
            autoreleasepool { assertAttributeIndexSpaceSelection() }
        case "commit-recovery":
            initializeTestCore(dictionaryPath: nil)
            assertCommitRecovery()
        case "closing-input":
            initializeTestCore(dictionaryPath: nil)
            assertClosingInputPassesThrough()
        case "lifecycle":
            initializeTestCore(dictionaryPath: nil)
            try! LifecycleTests.run()
        case "personal-frequency-store":
            initializeTestCore(dictionaryPath: nil)
            assertPersonalFrequencyPersistenceAndClear()
        case "about-status":
            initializeTestCore(dictionaryPath: nil)
            let provider = AboutStatusProvider.shared
            provider.configure(initialization: .notStarted, build: .unknown)
            let client = FakeTextClient()
            let controller = MuuInputController(testingClient: client)
            // Exercise the actual IMK menu: attention is a leading text mark,
            // and neither state uses an image. VoiceOver keeps its own label.
            let normalItem = controller.menu().items.last!
            precondition(normalItem.image == nil && normalItem.title == "About Muu IME")
            try! AboutStatusTests.run()
            assertPreparedReplyOwnership()
            provider.configure(initialization: .ready, build: .unknown)
            let item = controller.menu().items.last!
            precondition(item.image == nil)
            precondition(item.title == "⚠︎ About Muu IME")
            precondition(item.accessibilityLabel() == "About Muu IME、注意が必要です")
            provider.configure(initialization: .notStarted, build: .unknown)
        case "core-invalid":
            initializeTestCore(dictionaryPath:nil)
            assertCoreInvalidPresentation()
        case "learning-restart-first", "learning-restart-second", "learning-no-file":
            assertLearningRestart(dictionaryPath:dictionaryPath,testCase:testCase)
        case "learning-corrupt":
            assertRejectedLearningStartup(dictionaryPath:dictionaryPath,data:Data("broken".utf8))
        case "personal-frequency-mismatch":
            assertPersonalFrequencyDictionaryMismatch(dictionaryPath: dictionaryPath)
        case "native-frequency-empty", "native-frequency-nonempty", "native-frequency-legacy":
            assertPersonalFrequencyNativeV5Pair(dictionaryPath: dictionaryPath, testCase: testCase)
        case "generation-baseline-learning", "generation-baseline-user":
            assertGenerationBaselineProductionPair(dictionaryPath: dictionaryPath, testCase: testCase)
        case "commit-recovery-native":
            assertProductionGenerationE2E(dictionaryPath: dictionaryPath,
                requireBaselineHold: true, inputRomaji: "kyou", expectedSurface: "今日",
                corruptCommitReply: true)
        case "dictionary-only-recording":
            assertDictionaryOnlyRecording(dictionaryPath: dictionaryPath)
        case "commit-stream-delivery":
            assertCommitDeliveryDebtE2E(dictionaryPath: dictionaryPath)

        case "imported-completion-internal", "imported-completion-busy":
            assertImportedCompletionFailure(dictionaryPath: dictionaryPath,
                status: testCase == "imported-completion-internal" ? 6 : 9)
        default:
            preconditionFailure("unknown characterization case: \(testCase)")
        }
        print("candidate apply characterization: \(testCase) passed")
    }

    /// Initializes this process's Core once, through the same
    /// `RustCore.initialize` the app uses: the given dictionary (nil for the
    /// fast cases), no model, no recorder, an empty user dictionary and the
    /// learning store's startup bytes. RustEngine's testing initializer then
    /// sees `initializedForTesting` and does not initialize again, so every
    /// session a case creates shares this Core.
    private static func initializeTestCore(dictionaryPath: String?) {
        // These controller cases compile as standard. Use the process factory,
        // then check the actual native Core rather than just the Swift setting.
        let recorder = CommitStreamRecorder.shared
        precondition(!recorder.status.availableInBuild && recorder.makeEngineConfiguration() == nil)
        let result = try! RustCore.initialize(CoreConfiguration(dictionaryPath: dictionaryPath, conversionLmDirectory: nil,
            commitStream: recorder.makeEngineConfiguration()), learning: PersonalFrequencyStore.shared.startupData).get()
        precondition(try! RustCore.runtimeStatus().get().commitStreamEnabled == false)
        PersonalFrequencyStore.shared.acceptStartup(result)
        precondition(result.personalFrequencyVersion.revision == 0)
    }

    // The controller against the real Core and a fake client: the events it asks IMK
    // for, the input menu's items, order and targets, and how views reach the client
    // and the candidate surface, including the geometry recoveries observed in real
    // apps.
    private static func assertControllerCharacterization() {
        let client = FakeTextClient()
        let controller = MuuInputController(testingClient: client)
        let requiredEventMask: NSEvent.EventTypeMask = [
            .keyDown,
            .flagsChanged,
            .leftMouseDown,
            .rightMouseDown,
            .otherMouseDown,
        ]
        precondition(controller.recognizedEvents(client) == Int(requiredEventMask.rawValue))
        let inputMenu = controller.menu()
        precondition(inputMenu?.items.count == 7)
        precondition(inputMenu?.items.first?.title == "ユーザー辞書…")
        precondition(inputMenu?.items.first?.action == NSSelectorFromString("showUserDictionary:"))
        precondition(inputMenu?.items.first?.target === controller)
        precondition(inputMenu?.items[1].title == "辞書の管理…")
        precondition(
            inputMenu?.items[1].action == NSSelectorFromString("showImportedDictionaries:")
        )
        precondition(inputMenu?.items[1].target === controller)
        precondition(inputMenu?.items[2].title == "郵便番号データ…")
        precondition(inputMenu?.items[2].action == NSSelectorFromString("showPostalCodeData:"))
        precondition(inputMenu?.items[3].isSeparatorItem == true)
        precondition(inputMenu?.items[4].title == "変換学習データを削除…")
        precondition(
            inputMenu?.items[4].action == NSSelectorFromString("clearPersonalFrequency:")
        )
        precondition(inputMenu?.items[4].target === controller)
        precondition(inputMenu?.items.last?.target === controller)
        precondition(inputMenu?.items[5].isSeparatorItem == true)
        precondition(inputMenu?.items[6].title == "About Muu IME")
        precondition(inputMenu?.items[6].action == NSSelectorFromString("showAboutMuu:"))

        // Exact live failure shape: the client reports the marked, preferred,
        // and selected range as (1,3), but cannot provide its rectangle. The
        // adapter must recover from the marked-end caret and the surface policy
        // must produce a frame outside the synthesized fallback envelope.
        let observedGeometryView = ViewState(
            preedit: [
                PreeditRun(
                    text: "金曜日",
                    utf16Start: 0,
                    utf16Length: 3,
                    isFocused: true,
                    isConverted: true
                )
            ],
            caretUTF16: 3,
            selection: TextRange(start: 0, len: 3),
            segments: nil,
            candidates: nil,
            commitText: nil,
            mode: .hiragana,
            compositionModeTransition: nil,
            poisoned: false,
            consumed: true,
            error: nil
        )
        let observedUnavailableRects = [
            NSRect(
                x: CGFloat.leastNonzeroMagnitude,
                y: 0,
                width: CGFloat.leastNonzeroMagnitude,
                height: CGFloat.leastNonzeroMagnitude
            ),
            NSRect(
                x: 0,
                y: 423_306.045,
                width: CGFloat.leastNonzeroMagnitude,
                height: -1
            ),
        ]
        for observedUnavailableRect in observedUnavailableRects {
            let observedGeometryClient = FakeTextClient()
            observedGeometryClient.configureGeometry(
                markedRange: NSRange(location: 1, length: 3),
                selectedRange: NSRange(location: 1, length: 3)
            ) { range in
                if range == NSRange(location: 4, length: 0) {
                    return CandidateClientRectResult(
                        rect: NSRect(x: 240, y: 520, width: 0, height: 20),
                        actualRange: range
                    )
                }
                precondition(range == NSRange(location: 1, length: 3))
                return CandidateClientRectResult(
                    rect: observedUnavailableRect,
                    actualRange: NSRange(location: 0, length: 0)
                )
            }
            guard let observedAnchors = ClientGeometryReader.anchors(
                view: observedGeometryView,
                client: observedGeometryClient
            ) else {
                preconditionFailure("observed unavailable geometry must recover")
            }
            precondition(
                observedGeometryClient.geometryQueries == [
                    NSRange(location: 1, length: 3),
                    NSRange(location: 4, length: 0),
                ]
            )
            precondition(
                observedAnchors.primary
                    == NSRect(x: 240, y: 520, width: 1, height: 20)
            )
            precondition(observedAnchors.compositionRects.isEmpty)
            precondition(observedAnchors.compositionExclusionRects.isEmpty)
            precondition(!observedAnchors.compositionRectsComplete)
            guard let observedPlacement = CandidateSurfacePolicy.geometry(
                anchors: observedAnchors,
                lastValidAnchor: nil,
                contentSize: NSSize(width: 240, height: 180)
            ),
            let fallbackEnvelope = CandidateSurfacePolicy.visualExclusionEnvelope(
                logicalRect: observedPlacement.resolvedAnchor,
                orientation: .horizontal,
                lineMetric: observedPlacement.resolvedAnchor.height
            ) else {
                preconditionFailure("observed fallback geometry must place the panel")
            }
            precondition(!fallbackEnvelope.intersects(observedPlacement.frame))
            precondition(
                observedPlacement.placedAbove
                    ? observedPlacement.frame.minY - fallbackEnvelope.maxY
                        >= CandidateSurfacePolicy.panelGap
                    : fallbackEnvelope.minY - observedPlacement.frame.maxY
                        >= CandidateSurfacePolicy.panelGap
            )
        }

        let sameLocationZeroLengthClient = FakeTextClient()
        sameLocationZeroLengthClient.configureGeometry(
            markedRange: NSRange(location: 1, length: 3),
            selectedRange: NSRange(location: 1, length: 3)
        ) { range in
            if range == NSRange(location: 4, length: 0) {
                return CandidateClientRectResult(
                    rect: NSRect(x: 240, y: 520, width: 0, height: 20),
                    actualRange: range
                )
            }
            precondition(range == NSRange(location: 1, length: 3))
            return CandidateClientRectResult(
                rect: NSRect(x: 200, y: 520, width: 1, height: 20),
                actualRange: NSRange(location: 1, length: 0)
            )
        }
        guard let sameLocationZeroLengthAnchors =
            ClientGeometryReader.anchors(
                view: observedGeometryView,
                client: sameLocationZeroLengthClient
            )
        else {
            preconditionFailure(
                "same-location zero-length geometry must use the marked-end boundary"
            )
        }
        precondition(
            sameLocationZeroLengthClient.geometryQueries == [
                NSRange(location: 1, length: 3),
                NSRange(location: 4, length: 0),
            ]
        )
        precondition(
            sameLocationZeroLengthAnchors.primary
                == NSRect(x: 240, y: 520, width: 1, height: 20)
        )
        precondition(sameLocationZeroLengthAnchors.compositionRects.isEmpty)
        precondition(!sameLocationZeroLengthAnchors.compositionRectsComplete)

        let mismatchedBoundaryClient = FakeTextClient()
        mismatchedBoundaryClient.configureGeometry(
            markedRange: NSRange(location: 1, length: 3),
            selectedRange: NSRange(location: 1, length: 3)
        ) { range in
            if range == NSRange(location: 4, length: 0) {
                return CandidateClientRectResult(
                    rect: NSRect(x: 240, y: 520, width: 0, height: 20),
                    actualRange: NSRange(location: 3, length: 0)
                )
            }
            precondition(range == NSRange(location: 1, length: 3))
            return CandidateClientRectResult(
                rect: .zero,
                actualRange: NSRange(location: 0, length: 0)
            )
        }
        precondition(
            ClientGeometryReader.anchors(
                view: observedGeometryView,
                client: mismatchedBoundaryClient
            ) == nil
        )
        precondition(
            mismatchedBoundaryClient.geometryQueries == [
                NSRange(location: 1, length: 3),
                NSRange(location: 4, length: 0),
            ]
        )

        // A transition is a Rust-owned fact, while Swift owns its presentation.
        // The same panel is reused for rapid transitions and a stale expiry may
        // not dismiss the newer badge.
        let badgeClient = FakeTextClient()
        let badgeController = MuuInputController(testingClient: badgeClient)
        let literalTransition = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [
                    PreeditRun(
                        text: "n",
                        utf16Start: 0,
                        utf16Length: 1,
                        isFocused: true,
                        isConverted: false
                    )
                ],
                caretUTF16: 1,
                selection: nil,
                segments: nil,
                candidates: nil,
                commitText: nil,
                mode: .hiragana,
                compositionModeTransition: .literal,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        precondition(badgeController.applyForTesting(literalTransition, to: badgeClient))
        precondition(badgeController.modeBadgeVisibleForTesting)
        precondition(badgeController.modeBadgeTextForTesting == "A")
        guard let literalGeneration = badgeController.modeBadgeGenerationForTesting,
              let literalFrame = badgeController.modeBadgeFrameForTesting
        else {
            preconditionFailure("literal transition must display a mode badge")
        }
        let caretAnchor = NSRect(x: 248, y: 520, width: 8, height: 20)
        guard let preeditExclusion = CandidateSurfacePolicy.visualExclusionEnvelope(
            logicalRect: NSRect(x: 240, y: 520, width: 8, height: 20),
            orientation: .horizontal,
            lineMetric: 20
        ) else {
            preconditionFailure("raw preedit must have an exclusion envelope")
        }
        precondition(!literalFrame.intersects(caretAnchor))
        precondition(!literalFrame.intersects(preeditExclusion))
        precondition(
            abs(preeditExclusion.minY - literalFrame.maxY - 4) < 0.001
        )
        precondition(
            badgeClient.geometryQueries == [
                NSRange(location: 101, length: 0),
                NSRange(location: 100, length: 1),
            ]
        )

        let readingTransition = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [
                    PreeditRun(
                        text: "ん",
                        utf16Start: 0,
                        utf16Length: 1,
                        isFocused: true,
                        isConverted: false
                    )
                ],
                caretUTF16: 1,
                selection: nil,
                segments: nil,
                candidates: nil,
                commitText: nil,
                mode: .hiragana,
                compositionModeTransition: .reading,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        precondition(badgeController.applyForTesting(readingTransition, to: badgeClient))
        precondition(badgeController.modeBadgeTextForTesting == "あ")
        guard let readingGeneration = badgeController.modeBadgeGenerationForTesting else {
            preconditionFailure("reading transition must replace the mode badge")
        }
        precondition(readingGeneration > literalGeneration)
        precondition(
            ClientGeometryReader.badgeGeometry(
                view: readingTransition.view!,
                client: badgeClient,
                screens: [],
                contentSize: NSSize(width: 32, height: 28)
            ) == nil
        )
        badgeController.finishModeBadgeForTesting(generation: literalGeneration)
        precondition(badgeController.modeBadgeVisibleForTesting)
        badgeController.finishModeBadgeForTesting(generation: readingGeneration)
        precondition(!badgeController.modeBadgeVisibleForTesting)

        // Empty-composition Shift In anchors at selectedRange. A client that
        // cannot return that exact boundary hides only the badge; the mode
        // transition remains consumed and authoritative.
        let emptyTransition = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [],
                caretUTF16: 0,
                selection: nil,
                segments: nil,
                candidates: nil,
                commitText: nil,
                mode: .hiragana,
                compositionModeTransition: .literal,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        precondition(badgeController.applyForTesting(emptyTransition, to: badgeClient))
        precondition(badgeController.modeBadgeVisibleForTesting)
        precondition(badgeController.modeBadgeTextForTesting == "A")

        // The first empty-composition Shift tap observed in production returned
        // this exact zero-width caret. Placing the fixed badge four points from
        // the raw caret still overlapped the visual line; the shared candidate
        // envelope must be the exclusion authority instead.
        let observedEmptyCaret = NSRect(x: 372, y: 754, width: 1, height: 17)
        guard let observedEmptyEnvelope =
            CandidateSurfacePolicy.visualExclusionEnvelope(
                logicalRect: observedEmptyCaret,
                orientation: .horizontal,
                lineMetric: 17
            )
        else {
            preconditionFailure("observed empty caret must yield a visual envelope")
        }
        let oldCaretOnlyFrame = NSRect(x: 356.5, y: 722, width: 32, height: 28)
        precondition(oldCaretOnlyFrame.intersects(observedEmptyEnvelope))
        badgeClient.configureGeometry(
            markedRange: NSRange(location: NSNotFound, length: 0),
            selectedRange: NSRange(location: 69, length: 0)
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 372, y: 754, width: 0, height: 17),
                actualRange: range
            )
        }
        precondition(badgeController.applyForTesting(emptyTransition, to: badgeClient))
        guard let observedEmptyFrame = badgeController.modeBadgeFrameForTesting else {
            preconditionFailure("observed empty caret must display a mode badge")
        }
        precondition(observedEmptyFrame.size == NSSize(width: 32, height: 28))
        precondition(!observedEmptyFrame.intersects(observedEmptyEnvelope))
        precondition(
            abs(observedEmptyEnvelope.minY - observedEmptyFrame.maxY - 4) < 0.001
        )
        precondition(
            badgeClient.geometryQueries == [NSRange(location: 69, length: 0)]
        )

        badgeClient.configureGeometry(
            markedRange: NSRange(location: NSNotFound, length: 0),
            selectedRange: NSRange(location: 100, length: 0)
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 240, y: 520, width: 0, height: 20),
                actualRange: NSRange(location: range.location + 1, length: 0)
            )
        }
        precondition(badgeController.applyForTesting(emptyTransition, to: badgeClient))
        precondition(!badgeController.modeBadgeVisibleForTesting)
        for degenerateRect in observedUnavailableRects {
            badgeClient.configureGeometry(
                markedRange: NSRange(location: NSNotFound, length: 0),
                selectedRange: NSRange(location: 100, length: 0)
            ) { range in
                CandidateClientRectResult(
                    rect: degenerateRect,
                    actualRange: range
                )
            }
            precondition(badgeController.applyForTesting(emptyTransition, to: badgeClient))
            precondition(!badgeController.modeBadgeVisibleForTesting)
        }

        let geometryScreen = CandidateScreenFrame(
            frame: NSRect(x: 0, y: 0, width: 200, height: 200),
            visibleFrame: NSRect(x: 0, y: 0, width: 200, height: 200)
        )
        badgeClient.configureGeometry(
            markedRange: NSRange(location: NSNotFound, length: 0),
            selectedRange: NSRange(location: 100, length: 3)
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 40, y: 40, width: 0, height: 20),
                actualRange: range
            )
        }
        precondition(
            ClientGeometryReader.badgeGeometry(
                view: emptyTransition.view!,
                client: badgeClient,
                screens: [geometryScreen],
                contentSize: NSSize(width: 32, height: 28)
            ) == nil
        )
        precondition(badgeClient.geometryQueries.isEmpty)

        badgeClient.configureGeometry(
            markedRange: NSRange(location: NSNotFound, length: 0),
            selectedRange: NSRange(location: 100, length: 0)
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 40, y: 1, width: 0, height: 20),
                actualRange: range
            )
        }
        guard let aboveGeometry = ClientGeometryReader.badgeGeometry(
            view: emptyTransition.view!,
            client: badgeClient,
            screens: [geometryScreen],
            contentSize: NSSize(width: 32, height: 28)
        ) else {
            preconditionFailure("badge must flip above when below is unsafe")
        }
        precondition(aboveGeometry.placedAbove)
        precondition(!aboveGeometry.frame.intersects(aboveGeometry.anchor))
        guard let aboveEnvelope = CandidateSurfacePolicy.visualExclusionEnvelope(
            logicalRect: aboveGeometry.anchor,
            orientation: .horizontal,
            lineMetric: aboveGeometry.anchor.height
        ) else {
            preconditionFailure("empty caret must yield an exclusion envelope")
        }
        precondition(
            abs(aboveGeometry.frame.minY - aboveEnvelope.maxY - 4) < 0.001
        )

        let constrainedScreen = CandidateScreenFrame(
            frame: NSRect(x: 0, y: 0, width: 200, height: 45),
            visibleFrame: NSRect(x: 0, y: 0, width: 200, height: 45)
        )
        badgeClient.configureGeometry(
            markedRange: NSRange(location: NSNotFound, length: 0),
            selectedRange: NSRange(location: 100, length: 0)
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 40, y: 10, width: 0, height: 20),
                actualRange: range
            )
        }
        precondition(
            ClientGeometryReader.badgeGeometry(
                view: emptyTransition.view!,
                client: badgeClient,
                screens: [constrainedScreen],
                contentSize: NSSize(width: 32, height: 28)
            ) == nil
        )
        precondition(
            ClientGeometryReader.badgeGeometry(
                view: emptyTransition.view!,
                client: badgeClient,
                screens: [
                    CandidateScreenFrame(
                        frame: NSRect(x: 500, y: 500, width: 200, height: 200),
                        visibleFrame: NSRect(x: 500, y: 500, width: 200, height: 200)
                    ),
                ],
                contentSize: NSSize(width: 32, height: 28)
            ) == nil
        )

        // The production scheduler uses the main RunLoop and monotonic uptime.
        // This manual scheduler drives the same state machine without making
        // CPU scheduling latency part of the contract.
        let timedBadgeClient = FakeTextClient()
        let badgeScheduler = ManualModeBadgeScheduler()
        let timedBadgeController = MuuInputController(
            testingClient: timedBadgeClient,
            modeBadgeScheduler: badgeScheduler.scheduler
        )
        precondition(
            timedBadgeController.applyForTesting(literalTransition, to: timedBadgeClient)
        )
        precondition(timedBadgeController.modeBadgeVisibleForTesting)
        precondition(timedBadgeController.modeBadgeAlphaForTesting == 1)
        precondition(badgeScheduler.scheduledIntervals == [0.8])
        badgeScheduler.now = 0.8
        badgeScheduler.fireNext()
        precondition(timedBadgeController.modeBadgeVisibleForTesting)
        precondition(timedBadgeController.modeBadgeAlphaForTesting == 1)
        precondition(badgeScheduler.scheduledIntervals.count == 2)
        precondition(abs(badgeScheduler.scheduledIntervals[1] - (1.0 / 30.0)) < 0.000_001)
        badgeScheduler.now = 0.9
        badgeScheduler.fireNext()
        guard let fadingAlpha = timedBadgeController.modeBadgeAlphaForTesting else {
            preconditionFailure("mode badge must expose its fade before hiding")
        }
        precondition(abs(fadingAlpha - 0.5) < 0.000_001)
        badgeScheduler.now = 1.01
        badgeScheduler.fireNext()
        precondition(!timedBadgeController.modeBadgeVisibleForTesting)

        let candidateTransition = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [
                    PreeditRun(
                        text: "名",
                        utf16Start: 0,
                        utf16Length: 1,
                        isFocused: true,
                        isConverted: true
                    ),
                ],
                caretUTF16: 1,
                selection: TextRange(start: 0, len: 1),
                segments: nil,
                candidates: CandidateList(
                    items: [
                        CandidateItem(id: "mode-candidate-1", text: "名", annotation: nil),
                        CandidateItem(id: "mode-candidate-2", text: "菜", annotation: nil),
                    ],
                    selectedIndex: 0
                ),
                commitText: nil,
                mode: .hiragana,
                compositionModeTransition: .reading,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        let simultaneousClient = FakeTextClient()
        let simultaneousController = MuuInputController(testingClient: simultaneousClient)
        precondition(
            simultaneousController.applyForTesting(candidateTransition, to: simultaneousClient)
        )
        precondition(simultaneousController.candidateSurfaceVisibleForTesting)
        precondition(simultaneousController.candidateActionsEnabledForTesting)
        precondition(!simultaneousController.modeBadgeVisibleForTesting)
        precondition(
            simultaneousController.candidateSnapshotForTesting?.items.map(\.id)
                == ["mode-candidate-1", "mode-candidate-2"]
        )

        let lingeringClient = FakeTextClient()
        let lingeringController = MuuInputController(testingClient: lingeringClient)
        precondition(lingeringController.applyForTesting(literalTransition, to: lingeringClient))
        guard let lingeringGeneration = lingeringController.modeBadgeGenerationForTesting else {
            preconditionFailure("literal badge generation must exist")
        }
        let candidateWithoutTransition = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: candidateTransition.view!.preedit,
                caretUTF16: candidateTransition.view!.caretUTF16,
                selection: candidateTransition.view!.selection,
                segments: candidateTransition.view!.segments,
                candidates: candidateTransition.view!.candidates,
                commitText: nil,
                mode: .hiragana,
                compositionModeTransition: nil,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        precondition(
            lingeringController.applyForTesting(
                candidateWithoutTransition,
                to: lingeringClient
            )
        )
        precondition(lingeringController.candidateSurfaceVisibleForTesting)
        precondition(lingeringController.candidateActionsEnabledForTesting)
        precondition(!lingeringController.modeBadgeVisibleForTesting)
        precondition(lingeringController.applyForTesting(readingTransition, to: lingeringClient))
        precondition(!lingeringController.candidateSurfaceVisibleForTesting)
        precondition(lingeringController.modeBadgeVisibleForTesting)
        lingeringController.finishModeBadgeForTesting(generation: lingeringGeneration)
        precondition(lingeringController.modeBadgeVisibleForTesting)

        // Deactivation still commits the active Literal preedit, but its real
        // Literal -> Reading transition is presentation-suppressed. Both the
        // pre-commit and post-commit hides invalidate stale timer generations.
        let deactivateClient = FakeTextClient()
        let deactivateController = MuuInputController(testingClient: deactivateClient)
        precondition(deactivateController.handle(
            keyEvent(keyCode: 46, modifiers: [.shift], characters: "M"),
            client: deactivateClient
        ))
        guard let preDeactivateGeneration = deactivateController.modeBadgeGenerationForTesting
        else {
            preconditionFailure("literal chord must show a badge before deactivate")
        }
        precondition(deactivateController.modeBadgeVisibleForTesting)
        var deactivateCommands: [String] = []
        RustEngine.commandObserverForTesting = { command in
            deactivateCommands.append(command)
        }
        deactivateController.deactivateServer(deactivateClient)
        RustEngine.commandObserverForTesting = nil
        precondition(deactivateClient.commands.last == .insert("M"))
        precondition(
            deactivateCommands == ["force_commit", "clear_preceding_context"],
            "deactivate must clear context after committing the active composition"
        )
        precondition(deactivateController.composedTextForTesting.isEmpty)
        precondition(!deactivateController.modeBadgeVisibleForTesting)
        guard let postDeactivateGeneration = deactivateController.modeBadgeGenerationForTesting
        else {
            preconditionFailure("deactivate must invalidate the badge generation")
        }
        precondition(postDeactivateGeneration > preDeactivateGeneration)
        precondition(
            deactivateController.applyForTesting(emptyTransition, to: deactivateClient)
        )
        precondition(deactivateController.modeBadgeVisibleForTesting)
        deactivateController.finishModeBadgeForTesting(generation: preDeactivateGeneration)
        deactivateController.finishModeBadgeForTesting(generation: postDeactivateGeneration)
        precondition(deactivateController.modeBadgeVisibleForTesting)

        var events: [String] = []
        controller.setApplyEventRecorderForTesting {
            events.append($0)
        }
        var recreatedEngineAvailability: [Bool] = []
        RustEngine.recreateObserverForTesting = {
            recreatedEngineAvailability.append($0.isAvailable)
        }
        defer {
            RustEngine.recreateObserverForTesting = nil
        }

        let convertedPrintable = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [
                    PreeditRun(
                        text: "な",
                        utf16Start: 0,
                        utf16Length: 1,
                        isFocused: true,
                        isConverted: false
                    )
                ],
                caretUTF16: 1,
                selection: nil,
                segments: nil,
                candidates: CandidateList(
                    items: [
                        CandidateItem(id: "candidate-1", text: "名", annotation: nil),
                        CandidateItem(id: "candidate-2", text: "菜", annotation: nil),
                    ],
                    selectedIndex: 0
                ),
                commitText: "選択済み",
                mode: .hiragana,
                compositionModeTransition: nil,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        precondition(controller.applyForTesting(convertedPrintable, to: client))
        precondition(
            client.commands == [
                .insert("選択済み"),
                .mark("な"),
            ]
        )
        precondition(
            events == [
                "insertText",
                "candidateHide",
                "modeUpdate",
                "setMarkedText",
                "modeUpdate",
                "candidateSync",
                "candidateRender",
            ]
        )
        precondition(controller.composedTextForTesting == "な")
        precondition(controller.currentModeForTesting == .hiragana)
        guard let presentedSnapshot = controller.candidateSnapshotForTesting else {
            preconditionFailure("successful apply must present candidates")
        }
        precondition(presentedSnapshot.items.count == 2)
        precondition(presentedSnapshot.selectedIndex == 0)
        precondition(controller.candidateSurfaceVisibleForTesting)
        precondition(controller.candidateActionsEnabledForTesting)

        let commandCount = client.commands.count
        events.removeAll()
        precondition(controller.applyForTesting(nil, to: client))
        precondition(events == ["candidateInvalidate"])
        precondition(client.commands.count == commandCount)
        assertPresentationPreserved(controller, from: presentedSnapshot)
        precondition(!controller.candidateActionsEnabledForTesting)

        events.removeAll()
        let transportFailure = EngineResponse(
            status: Int32(MUU_RESULT_DECODE_ERR),
            view: nil
        )
        precondition(controller.applyForTesting(transportFailure, to: client))
        precondition(events == ["candidateInvalidate"])
        precondition(client.commands.count == commandCount)
        assertPresentationPreserved(controller, from: presentedSnapshot)

        events.removeAll()
        let nilView = EngineResponse(status: MUU_RESULT_OK, view: nil)
        precondition(controller.applyForTesting(nilView, to: client))
        precondition(events == ["candidateInvalidate"])
        precondition(client.commands.count == commandCount)
        assertPresentationPreserved(controller, from: presentedSnapshot)

        events.removeAll()
        let structuredError = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [],
                caretUTF16: 0,
                selection: nil,
                segments: nil,
                candidates: nil,
                commitText: nil,
                mode: .halfAlnum,
                compositionModeTransition: nil,
                poisoned: false,
                consumed: false,
                error: ViewError(code: "characterization", params: [:])
            )
        )
        precondition(!controller.applyForTesting(structuredError, to: client))
        precondition(events == ["candidateInvalidate"])
        precondition(client.commands.count == commandCount)
        assertPresentationPreserved(controller, from: presentedSnapshot)
        precondition(controller.composedTextForTesting == "な")
        precondition(controller.currentModeForTesting == .hiragana)

        events.removeAll()
        // These are separate production-branch fixtures: removing either the
        // PANIC predicate or the poisoned predicate prevents its corresponding
        // recreate observer callback. Removing the recreate call prevents both.
        let panic = EngineResponse(
            status: MUU_RESULT_SESSION_INVALID,
            view: nil
        )
        precondition(controller.applyForTesting(panic, to: client))
        precondition(events == ["setMarkedText", "candidateHide"])
        precondition(client.commands.last == .mark(""))
        precondition(controller.composedTextForTesting.isEmpty)
        precondition(controller.candidateSnapshotForTesting == nil)
        precondition(!controller.candidateSurfaceVisibleForTesting)
        precondition(!controller.candidateActionsEnabledForTesting)
        precondition(recreatedEngineAvailability == [true])

        events.removeAll()
        let activeBeforePoison = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [
                    PreeditRun(
                        text: "か",
                        utf16Start: 0,
                        utf16Length: 1,
                        isFocused: true,
                        isConverted: false
                    )
                ],
                caretUTF16: 1,
                selection: nil,
                segments: nil,
                candidates: CandidateList(
                    items: [
                        CandidateItem(
                            id: "candidate-after-panic",
                            text: "蚊",
                            annotation: nil
                        )
                    ],
                    selectedIndex: 0
                ),
                commitText: nil,
                mode: .hiragana,
                compositionModeTransition: nil,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        precondition(controller.applyForTesting(activeBeforePoison, to: client))
        precondition(controller.composedTextForTesting == "か")
        precondition(controller.candidateSnapshotForTesting != nil)
        precondition(controller.candidateSurfaceVisibleForTesting)
        precondition(controller.candidateActionsEnabledForTesting)

        let commandCountBeforePoison = client.commands.count
        events.removeAll()
        let poisoned = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [],
                caretUTF16: 0,
                selection: nil,
                segments: nil,
                candidates: nil,
                commitText: "must-not-commit",
                mode: .halfAlnum,
                compositionModeTransition: nil,
                poisoned: true,
                consumed: false,
                error: nil
            )
        )
        precondition(controller.applyForTesting(poisoned, to: client))
        precondition(events == ["setMarkedText", "candidateHide"])
        precondition(client.commands.count == commandCountBeforePoison + 1)
        precondition(client.commands.last == .mark(""))
        precondition(controller.composedTextForTesting.isEmpty)
        precondition(controller.candidateSnapshotForTesting == nil)
        precondition(!controller.candidateSurfaceVisibleForTesting)
        precondition(!controller.candidateActionsEnabledForTesting)
        precondition(recreatedEngineAvailability == [true, true])

        // Production-shape caret projection: Rust owns the preedit cursor and
        // Swift projects it into IMK's marked-text selectionRange.
        let caretClient = FakeTextClient()
        let caretController = MuuInputController(testingClient: caretClient)
        for (keyCode, character) in [
            (40, "k"), (0, "a"), (40, "k"), (34, "i"), (40, "k"), (32, "u"),
        ] {
            precondition(caretController.handle(
                keyEvent(keyCode: UInt16(keyCode), characters: character),
                client: caretClient
            ))
        }
        precondition(caretClient.commands.last == .mark("かきく"))
        precondition(caretClient.markedSelectionRange == NSRange(location: 3, length: 0))
        let rawAttributes = caretClient.markedText?.attributes(
            at: 0,
            effectiveRange: nil
        ) ?? [:]
        precondition(rawAttributes[.backgroundColor] == nil)
        precondition(
            rawAttributes[.underlineStyle] as? Int
                == NSUnderlineStyle.single.rawValue
        )

        // Removing raw-preedit selection chrome must not weaken the focused
        // converted-clause presentation.
        let convertedStyleClient = FakeTextClient()
        let convertedStyleController = MuuInputController(
            testingClient: convertedStyleClient
        )
        let convertedStyleResponse = EngineResponse(
            status: MUU_RESULT_OK,
            view: ViewState(
                preedit: [
                    PreeditRun(
                        text: "書く",
                        utf16Start: 0,
                        utf16Length: 2,
                        isFocused: true,
                        isConverted: true
                    )
                ],
                caretUTF16: 2,
                selection: TextRange(start: 0, len: 2),
                segments: nil,
                candidates: nil,
                commitText: nil,
                mode: .hiragana,
                compositionModeTransition: nil,
                poisoned: false,
                consumed: true,
                error: nil
            )
        )
        precondition(
            convertedStyleController.applyForTesting(
                convertedStyleResponse,
                to: convertedStyleClient
            )
        )
        let convertedAttributes = convertedStyleClient.markedText?.attributes(
            at: 0,
            effectiveRange: nil
        ) ?? [:]
        precondition(convertedAttributes[.backgroundColor] != nil)
        precondition(
            convertedAttributes[.underlineStyle] as? Int
                == NSUnderlineStyle.thick.rawValue
        )
        for expectedCaret in [2, 1] {
            precondition(caretController.handle(
                keyEvent(
                    keyCode: 123,
                    modifiers: [.function, .numericPad],
                    characters: "\u{f702}"
                ),
                client: caretClient
            ))
            precondition(
                caretClient.markedSelectionRange
                    == NSRange(location: expectedCaret, length: 0)
            )
        }
        for (keyCode, character) in [(1, "s"), (0, "a")] {
            precondition(caretController.handle(
                keyEvent(keyCode: UInt16(keyCode), characters: character),
                client: caretClient
            ))
        }
        precondition(caretClient.commands.last == .mark("かさきく"))
        precondition(caretClient.markedSelectionRange == NSRange(location: 2, length: 0))
        precondition(caretController.handle(
            keyEvent(keyCode: 51, characters: ""),
            client: caretClient
        ))
        precondition(caretClient.commands.last == .mark("かきく"))
        precondition(caretClient.markedSelectionRange == NSRange(location: 1, length: 0))
        precondition(caretController.handle(
            keyEvent(
                keyCode: 123,
                modifiers: [.function, .numericPad],
                characters: "\u{f702}"
            ),
            client: caretClient
        ))
        precondition(caretClient.markedSelectionRange == NSRange(location: 0, length: 0))
        precondition(caretController.handle(
            keyEvent(keyCode: 51, characters: ""),
            client: caretClient
        ))
        precondition(caretClient.commands.last == .mark("かきく"))
        precondition(caretClient.markedSelectionRange == NSRange(location: 0, length: 0))
        precondition(!caretClient.commands.contains { command in
            if case .insert = command { return true }
            return false
        })

        // Production-shape NSEvent -> Swift envelope -> Rust Session coverage.
        // Removing flagsChanged from recognizedEvents, failing to preserve its
        // keycode/modifiers, or dropping the explicit mouse commit makes this
        // sequence fail at its first affected assertion.
        let eventClient = FakeTextClient()
        let eventController = MuuInputController(testingClient: eventClient)
        precondition(eventController.handle(
            keyEvent(keyCode: 46, modifiers: [.shift], characters: "M"),
            client: eventClient
        ))
        precondition(eventController.modeBadgeVisibleForTesting)
        precondition(eventController.modeBadgeTextForTesting == "A")
        for (keyCode, character) in [(32, "u"), (32, "u")] {
            precondition(eventController.handle(
                keyEvent(keyCode: UInt16(keyCode), characters: character),
                client: eventClient
            ))
        }
        precondition(!eventController.handle(
            flagsChangedEvent(keyCode: 56, modifiers: [.maskShift]),
            client: eventClient
        ))
        precondition(eventController.handle(
            flagsChangedEvent(keyCode: 56, modifiers: []),
            client: eventClient
        ))
        precondition(eventClient.commands.last == .mark("Muu"))
        precondition(eventController.modeBadgeVisibleForTesting)
        precondition(eventController.modeBadgeTextForTesting == "あ")
        precondition(eventController.handle(
            keyEvent(keyCode: 40, characters: "k"),
            client: eventClient
        ))
        precondition(eventController.handle(
            keyEvent(keyCode: 0, characters: "a"),
            client: eventClient
        ))
        precondition(eventClient.commands.last == .mark("Muuか"))
        precondition(!eventController.handle(mouseDownEvent(), client: eventClient))
        precondition(eventClient.commands.last == .insert("Muuか"))

        let lowercaseLiteralClient = FakeTextClient()
        let lowercaseLiteralController = MuuInputController(
            testingClient: lowercaseLiteralClient
        )
        precondition(!lowercaseLiteralController.handle(
            flagsChangedEvent(keyCode: 56, modifiers: [.maskShift]),
            client: lowercaseLiteralClient
        ))
        precondition(lowercaseLiteralController.handle(
            flagsChangedEvent(keyCode: 56, modifiers: []),
            client: lowercaseLiteralClient
        ))
        precondition(lowercaseLiteralController.modeBadgeTextForTesting == "A")
        for (keyCode, character) in [(45, "n"), (35, "p"), (46, "m")] {
            precondition(lowercaseLiteralController.handle(
                keyEvent(keyCode: UInt16(keyCode), characters: character),
                client: lowercaseLiteralClient
            ))
        }
        precondition(lowercaseLiteralClient.commands.last == .mark("npm"))
        precondition(!lowercaseLiteralController.handle(
            flagsChangedEvent(keyCode: 56, modifiers: [.maskShift]),
            client: lowercaseLiteralClient
        ))
        precondition(lowercaseLiteralController.handle(
            flagsChangedEvent(keyCode: 56, modifiers: []),
            client: lowercaseLiteralClient
        ))
        precondition(lowercaseLiteralClient.commands.last == .mark("npm"))
        precondition(lowercaseLiteralController.modeBadgeTextForTesting == "あ")

        // This repeated-mark input is only marked while it is typed:
        // nothing is searched before conversion is requested, so a later main
        // turn must remain responsive.
        let runawayClient = FakeTextClient()
        let runawayController = MuuInputController(testingClient: runawayClient)
        for character in String(repeating: "-", count: 18) + "++" {
            precondition(runawayController.handle(
                keyEvent(keyCode: 0, characters: String(character)),
                client: runawayClient
            ))
        }
        precondition(
            runawayClient.commands.last == .mark(String(repeating: "ー", count: 18) + "＋＋")
        )
        var mainProbeRan = false
        let mainProbeWatchdog = DispatchWorkItem { preconditionFailure("main input callback hung") }
        DispatchQueue.main.asyncAfter(deadline: .now() + 180, execute: mainProbeWatchdog)
        DispatchQueue.main.async {
            mainProbeRan = true
            CFRunLoopStop(CFRunLoopGetMain())
        }
        while !mainProbeRan {
            RunLoop.main.run(mode: .default, before: .distantFuture)
        }
        mainProbeWatchdog.cancel()

        // Named-file Postal startup (once, off-main IO), publication,
        // main completion and Postal-only disable are characterized by
        // test-postal-code.swift and the Core Postal transaction tests.

        // Imported dictionary loading, main completion and serialized publication
        // are characterized by test-imported-dictionary.swift using the named-file ABI.

        // The real prepare/publish byte transaction serves existing and future
        // sessions; a malformed command must leave that shared Core usable.
        let userEngineA = RustEngine()
        let userEngineB = RustEngine()
        precondition(userEngineA.isAvailable && userEngineB.isAvailable)
        let userRecord = UserDictionaryRecord(reading:"むうてすと",surface:"Muu辞書",partOfSpeech:.noun,priority:.high)
        let bridge = RustCore.userDictionaryBridge
        let view = try! JSONSerialization.jsonObject(with: bridge.view()) as! [String: Any]
        let command = try! JSONSerialization.data(withJSONObject: [
            "operation": "upsert", "expected_revision": view["management_revision"]!,
            "reading": userRecord.reading, "surface": userRecord.surface,
            "part_of_speech": userRecord.partOfSpeech.rawValue, "priority": userRecord.priority.rawValue,
        ])
        let (_, planBytes) = try! bridge.prepare(command)
        let plan = try! JSONSerialization.jsonObject(with: planBytes) as! [String: Any]
        let ticket = try! JSONSerialization.data(withJSONObject: plan["ticket"]!)
        precondition(bridge.publish(ticket) == MUU_RESULT_OK)
        _ = bridge.cancel(ticket)
        let acceptedView = try! bridge.view()
        do {
            _ = try bridge.prepare(Data(#"{"operation":"not_an_edit"}"#.utf8))
            preconditionFailure("invalid edit must be rejected")
        } catch let error as UserDictionaryOperationError {
            precondition(error.status == MUU_RESULT_DECODE_ERR)
            precondition(error.code == "user_dictionary_invalid_edit")
            precondition(userEngineA.isAvailable && userEngineB.isAvailable)
            precondition(try! bridge.view() == acceptedView)
        } catch { preconditionFailure("unexpected edit error") }

        let userFailureRoot = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".muu-user-dictionary-ui-failure-\(UUID().uuidString)")
        // Managed stores now require their base to exist, as Application
        // Support does in production, so each fixture creates its own.
        try! FileManager.default.createDirectory(at: userFailureRoot, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: userFailureRoot) }
        var failureBridge = RustCore.userDictionaryBridge
        failureBridge.prepare = { _ in throw FixtureCoreError.installFailed(-77) }
        let userFailureStore = UserDictionaryStore(baseDirectory: userFailureRoot,
            bundleIdentifier: "dev.muu.user-dictionary-ui-failure", bridge: failureBridge)
        try! userFailureStore.acceptInitializedCore()
        let previousCount = userFailureStore.entries.count
        let userFailureWindow = UserDictionaryWindowController(store: userFailureStore)
        userFailureWindow.saveForTesting(reading: "むう", surface: "Muu")
        // The bound is only a hang guard; completion, not elapsed time, is
        // the assertion. Busy spans IO and the window's main completion.
        let hangGuard = Date().addingTimeInterval(180)
        while userFailureStore.isBusy {
            precondition(Date() < hangGuard)
            RunLoop.current.run(until: Date().addingTimeInterval(0.002))
        }
        precondition(userFailureStore.entries.count == previousCount)
        precondition(userFailureWindow.presentedErrorForTesting as? FixtureCoreError == .installFailed(-77))

        let userWarningRoot = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".muu-user-dictionary-ui-warning-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: userWarningRoot, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: userWarningRoot) }
        var warningOperation: String?
        var warningPublishStatus: Int32?
        var warningBridge = RustCore.userDictionaryBridge
        let warningPrepare = warningBridge.prepare
        warningBridge.prepare = { command in
            warningOperation = (try? JSONSerialization.jsonObject(with: command) as? [String: Any])?["operation"] as? String
            return try warningPrepare(command)
        }
        let warningPublish = warningBridge.publish
        warningBridge.publish = { ticket in
            let status = warningPublish(ticket)
            warningPublishStatus = status
            return status
        }
        let userWarningStore = UserDictionaryStore(baseDirectory: userWarningRoot,
            bundleIdentifier: "dev.muu.user-dictionary-ui-warning", bridge: warningBridge,
            injection: { if $0 == "after_rename" { throw FixtureCoreError.recoveryFailed } })
        try! userWarningStore.acceptInitializedCore()
        let previousWarningCount = userWarningStore.entries.count
        let userWarningWindow = UserDictionaryWindowController(store: userWarningStore)
        let warningSelectedRow = userWarningWindow.selectedRowForTesting
        userWarningWindow.saveForTesting(reading: "むう", surface: "Muu")
        while userWarningStore.isBusy {
            precondition(Date() < hangGuard)
            RunLoop.current.run(until: Date().addingTimeInterval(0.002))
        }
        // Store completion releases busy before the window's queued reload.
        // Wait for that visible result, not an elapsed runloop interval.
        while !userWarningWindow.statusTextForTesting.contains("耐久性") {
            precondition(Date() < hangGuard, "warning-save status reload did not arrive")
            RunLoop.current.run(until: Date().addingTimeInterval(0.002))
        }
        // The message carries the facts needed to see which step diverged
        // (row, operation, publish status); the assertion itself is unchanged.
        precondition(userWarningStore.entries.count == previousWarningCount + 1,
            "warning-save actual=\(userWarningStore.entries.count) previous=\(previousWarningCount) selectedRow=\(warningSelectedRow) operation=\(warningOperation ?? "none") publish=\(warningPublishStatus.map(String.init) ?? "none")")
        precondition(userWarningWindow.presentedErrorForTesting == nil)
        precondition(userWarningWindow.statusTextForTesting.contains("耐久性"))

        // Core owns the degraded content; the window must render both facts
        // when deleting one row still leaves 4,097 rows and saving warns.
        var degradedRows: [[String:Any]] = (0..<4098).map {
            ["entry_id":"degraded-\($0)","reading":"むう","surface":"語\($0)","part_of_speech":"NOUN","priority":"normal"]
        }
        var degradedRevision: UInt64=0
        func degradedView(_ rows: [[String:Any]], revision: UInt64) -> [String:Any] {
            ["schema_version":1,"management_revision":revision,"access":"editable",
             "reason":"user_dictionary_layer_invalid","layer_status":"empty_degraded",
             "entries":rows,"entry_count":rows.count,"dormant_count":0]
        }
        let degradedBridge=UserDictionaryCoreBridge(
            view: { try JSONSerialization.data(withJSONObject:degradedView(degradedRows,revision:degradedRevision)) },
            prepare: { data in
                let command=try JSONSerialization.jsonObject(with:data) as! [String:Any]
                precondition(command["operation"] as? String == "remove")
                precondition(command["entry_id"] as? String == "degraded-0")
                precondition((command["expected_revision"] as! NSNumber).uint64Value==degradedRevision)
                return (Data("opaque Core file".utf8),try JSONSerialization.data(withJSONObject:[
                    "schema_version":1,"ticket":["owner":1,"operation":1,"expected_revision":degradedRevision],
                    "expected_revision":degradedRevision,"next_revision":degradedRevision+1,
                    "view":degradedView(Array(degradedRows.dropFirst()),revision:degradedRevision+1)]))
            },
            publish: { _ in degradedRows.removeFirst();degradedRevision += 1;return 0 },
            cancel: { _ in preconditionFailure("published deletion needs no cancel") })
        let degradedRoot=URL(fileURLWithPath:FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".muu-user-dictionary-ui-degraded-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: degradedRoot, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at:degradedRoot) }
        let degradedStore=UserDictionaryStore(baseDirectory:degradedRoot,
            bundleIdentifier:"dev.muu.user-dictionary-ui-degraded",bridge:degradedBridge,
            injection:{ if $0=="after_rename" { throw FixtureCoreError.recoveryFailed } })
        try! degradedStore.acceptInitializedCore()
        let degradedWindow=UserDictionaryWindowController(store:degradedStore)
        degradedWindow.deleteForTesting(at:0)
        let deletionHangGuard=Date().addingTimeInterval(180)
        // The UI renders on the next main turn after the store releases busy.
        // Wait for that rendering, not a guessed delay after disk completion.
        while degradedStore.isBusy || !degradedWindow.statusTextForTesting.contains("4097件") {
            precondition(Date() < deletionHangGuard)
            RunLoop.current.run(until:Date().addingTimeInterval(0.002))
        }
        precondition(degradedWindow.presentedErrorForTesting==nil)
        precondition(degradedStore.isDegraded && degradedStore.durabilityWarning != nil)
        precondition(degradedWindow.statusTextForTesting.contains("4097件"))
        precondition(degradedWindow.statusTextForTesting.contains("まず削除で直してください"))
        precondition(degradedWindow.statusTextForTesting.contains("保存の耐久性を確認できませんでした"))
    }

    // Text crosses the controller as bytes. Commit and marked text are passed on
    // unchanged (no NFC; a decomposed kana stays decomposed); the caret and selection
    // are Rust's UTF-16 values, not character counts; clause and underline attributes
    // follow the focused run. Geometry queries come before the panel renders. A key
    // event is encoded with its raw modifiers, characters and repeat flag.
    private static func assertForcedCreateFailurePolicy() {
        let previous = RustEngine.forceCreateFailureSettingForTesting
        defer { RustEngine.forceCreateFailureSettingForTesting = previous }
        for requested in [true, false] {
            var keys: [String] = []
            RustEngine.forceCreateFailureSettingForTesting = { key in
                keys.append(key)
                return requested
            }
            let engine = RustEngine()
            precondition(engine.isAvailable == !requested)
            precondition(keys == ["MuuForceCreateFailure"])
        }
    }

    private static func assertCommitRecovery() {
        // A structured display error must not discard an already committed
        // string or the next preedit carried by that same response.
        let errorClient = FakeTextClient()
        let errorController = MuuInputController(testingClient: errorClient)
        let committedError = ViewState(preedit: [PreeditRun(text: "な", utf16Start: 0, utf16Length: 1,
            isFocused: true, isConverted: false)], caretUTF16: 1, selection: nil,
            segments: nil, candidates: nil, commitText: "確定😀", mode: .hiragana,
            compositionModeTransition: nil, poisoned: false, consumed: true,
            error: ViewError(code: "display_error", params: [:]))
        precondition(errorController.applyForTesting(EngineResponse(status: MUU_RESULT_OK, view: committedError), to: errorClient))
        precondition(errorClient.commands == [.insert("確定😀"), .mark("な")],
            "display error must preserve commit and next preedit exactly once")

        func envelope(_ payload: [String: Any]) -> Data {
            Data([1, 1, 0]) + (try! JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]))
        }
        let original: [String: Any] = [
            "preedit": [["text": "な😀", "utf16_start": 0, "utf16_len": 3,
                "is_focused": true, "is_converted": false]],
            "caret_utf16": 3, "selection": ["start": 1, "len": 2],
            "mode": "hiragana", "consumed": true, "commit_text": "前😀",
        ]
        // Broken display metadata only: candidates, segments or error params
        // of the wrong shape. The commit and the next preedit and selection
        // still apply, with no candidate panel.
        for (key, value): (String, Any) in [
            ("candidates", ["items": "broken", "selected_index": 0]),
            ("segments", "broken"),
            ("error", ["code": "display_error", "params": NSNull()]),
        ] {
            var payload = original
            payload[key] = value
            let response = try! EngineResponse.decodeView(envelope(payload), status: MUU_RESULT_OK)
            let client = FakeTextClient()
            let controller = MuuInputController(testingClient: client)
            precondition(response.view?.candidates == nil && response.view?.segments == nil && response.view?.error == nil)
            precondition(controller.applyForTesting(response, to: client))
            precondition(client.commands == [.insert("前😀"), .mark("な😀")])
            precondition(client.markedSelectionRange == NSRange(location: 1, length: 2))
            precondition(!controller.candidateSurfaceVisibleForTesting)
        }
        // Invalid essential fields can never create an AppKit range. Only
        // the independently decoded commit is applied, and the old panel ends.
        for (key, value): (String, Any) in [
            ("mode", "unknown"), ("caret_utf16", -1), ("caret_utf16", 4),
            ("selection", ["start": 1, "len": Int.max]),
            ("selection", ["start": -1, "len": 1]),
            ("preedit", [["text": "な", "utf16_start": 1, "utf16_len": 1,
                "is_focused": true, "is_converted": false]]),
            ("preedit", [["text": "😀", "utf16_start": 0, "utf16_len": 1,
                "is_focused": true, "is_converted": false]]),
        ] {
            var payload = original
            payload["candidates"] = "broken"
            payload[key] = value
            let response = try! EngineResponse.decodeView(envelope(payload), status: MUU_RESULT_OK)
            precondition(response.view == nil && response.recoveredCommitText == "前😀")
            let client = FakeTextClient()
            let controller = MuuInputController(testingClient: client)
            precondition(controller.applyForTesting(EngineResponse(status: MUU_RESULT_OK, view: committedError), to: client))
            let before = client.commands.count
            precondition(controller.applyForTesting(response, to: client))
            precondition(Array(client.commands.dropFirst(before)) == [.insert("前😀")])
            precondition(controller.composedTextForTesting.isEmpty && !controller.candidateSurfaceVisibleForTesting)
        }
        var broken = original
        broken["candidates"] = "broken"
        let validBrokenDisplay = envelope(broken)
        func rejects(_ data: Data, status: Int32 = MUU_RESULT_OK) {
            do {
                _ = try EngineResponse.decodeView(data, status: status)
                preconditionFailure("invalid response cannot recover a commit")
            } catch {}
        }
        // What is never rescued: any non-OK status, a short envelope, an
        // unknown format or schema, malformed JSON, a commit of the wrong type
        // and a missing commit.
        for status: Int32 in [MUU_RESULT_DECODE_ERR, MUU_RESULT_INTERNAL,
            MUU_RESULT_SESSION_INVALID, MUU_RESULT_CORE_INVALID] {
            rejects(validBrokenDisplay, status: status)
        }
        rejects(Data([1, 1]))
        rejects(Data([2, 1, 0]) + validBrokenDisplay.dropFirst(3))
        rejects(Data([1, 2, 0]) + validBrokenDisplay.dropFirst(3))
        rejects(Data([1, 1, 0]) + Data("{\"commit_text\":\"must not recover\",broken}".utf8))
        for value: Any in [NSNull(), 1, ["text"]] {
            var payload = broken
            payload["commit_text"] = value
            rejects(envelope(payload))
        }
        broken.removeValue(forKey: "commit_text")
        rejects(envelope(broken))
        // The pointer is deliberately unreadable: the advertised bound must
        // be checked before attempting any foreign-memory copy.
        let oversized = MuuBuffer(data: UnsafeMutablePointer<UInt8>(bitPattern: 1),
            len: MuuEnvelope.maximumViewBytes + 1)
        do {
            _ = try EngineResponse.decodeView(oversized, status: MUU_RESULT_OK)
            preconditionFailure("oversized view must not be copied")
        } catch MuuProtocolError.oversizedView {} catch { preconditionFailure("wrong size error") }

        // A normal no-commit error still preserves the current composition.
        let preserved = FakeTextClient()
        let preservingController = MuuInputController(testingClient: preserved)
        precondition(preservingController.applyForTesting(EngineResponse(status: MUU_RESULT_OK, view: committedError), to: preserved))
        let count = preserved.commands.count
        let noCommit = ViewState(preedit: [], caretUTF16: 0, selection: nil, segments: nil,
            candidates: nil, commitText: nil, mode: .hiragana, compositionModeTransition: nil,
            poisoned: false, consumed: true, error: ViewError(code: "display_error", params: [:]))
        precondition(preservingController.applyForTesting(EngineResponse(status: MUU_RESULT_OK, view: noCommit), to: preserved))
        precondition(preserved.commands.count == count && preservingController.composedTextForTesting == "な")

    }

    private static func assertUnicodeAndRawEnvelopeContract() {
        autoreleasepool {
            let client = FakeTextClient()
            let controller = MuuInputController(testingClient: client)
            var queryAndRenderOrder: [String] = []
            client.geometryEventForTesting = { queryAndRenderOrder.append($0) }
            controller.setApplyEventRecorderForTesting { event in
                if event == "candidateRender" { queryAndRenderOrder.append("render") }
            }
            let body = "CANARY_PREEDIT"
            let length = (body as NSString).length
            let view = ViewState(preedit: [PreeditRun(text: body, utf16Start: 0, utf16Length: length,
                isFocused: true, isConverted: true)],
                caretUTF16: length, selection: TextRange(start: 0, len: length), segments: nil,
                candidates: CandidateList(items: [CandidateItem(id: "CANARY_ID", text: "CANARY_CANDIDATE",
                    annotation: "CANARY_ANNOTATION")], selectedIndex: 0),
                commitText: "CANARY_COMMIT", mode: .hiragana, compositionModeTransition: nil, poisoned: false, consumed: true, error: nil)
            client.configureGeometry(markedRange: NSRange(location: 100, length: length),
                selectedRange: NSRange(location: 100, length: length)) { range in
                    CandidateClientRectResult(rect: NSRect(x: 100, y: 400, width: 140, height: 20), actualRange: range)
                }
            precondition(controller.applyForTesting(EngineResponse(status: MUU_RESULT_OK, view: view), to: client))
            precondition(client.commands.suffix(2) == [.insert("CANARY_COMMIT"), .mark(body)])
            precondition(controller.candidateSurfaceVisibleForTesting)
            let marked = NSRange(location: 100, length: length)
            precondition(client.geometryQueries == [marked, marked, marked])
            precondition(queryAndRenderOrder == ["markedRange", "selectedRange",
                "firstRect", "firstRect", "firstRect", "render"])
            controller.hideCandidateSurfaceForTesting()
        }

        let client = FakeTextClient()
        let controller = MuuInputController(testingClient: client)
        let decomposed = "か\u{3099}"
        let body = decomposed + "😀A"
        func response(selection: TextRange?, commit: String?) -> EngineResponse {
            EngineResponse(status: MUU_RESULT_OK, view: ViewState(preedit: [
                PreeditRun(text: decomposed, utf16Start: 0, utf16Length: 2,
                    isFocused: false, isConverted: false),
                PreeditRun(text: "😀", utf16Start: 2, utf16Length: 2,
                    isFocused: true, isConverted: true),
                PreeditRun(text: "A", utf16Start: 4, utf16Length: 1,
                    isFocused: false, isConverted: true)],
                caretUTF16: 4, selection: selection, segments: nil, candidates: nil,
                commitText: commit, mode: .hiragana, compositionModeTransition: nil, poisoned: false, consumed: true, error: nil))
        }
        precondition(controller.applyForTesting(response(selection: TextRange(start: 2, len: 2), commit: "前😀"), to: client))
        precondition(client.commands.suffix(2) == [.insert("前😀"), .mark(body)])
        precondition(Data(controller.composedTextForTesting.utf8) == Data(body.utf8), "Swift must not NFC-normalize marked bytes")
        precondition(client.markedSelectionRange == NSRange(location: 2, length: 2))
        precondition(client.markedText?.length == 5)
        precondition(client.markedText?.attribute(.markedClauseSegment, at: 2, effectiveRange: nil) as? Int == 1)
        precondition(client.markedText?.attribute(.underlineStyle, at: 2, effectiveRange: nil) as? Int == NSUnderlineStyle.thick.rawValue)
        precondition(client.markedText?.attribute(.backgroundColor, at: 0, effectiveRange: nil) == nil)
        precondition(controller.applyForTesting(response(selection: nil, commit: nil), to: client))
        precondition(client.markedSelectionRange == NSRange(location: 4, length: 0), "caret is Rust's UTF-16 value, not a character count")

        let prior = RustEngine.requestObserverForTesting
        var sent: Data?
        RustEngine.requestObserverForTesting = { data in sent = data; prior?(data) }
        defer { RustEngine.requestObserverForTesting = prior }
        let modifiers = NSEvent.ModifierFlags(rawValue: 0x20_0002)
        let event = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: modifiers,
            timestamp: 0, windowNumber: 0, context: nil, characters: "A😀",
            charactersIgnoringModifiers: "a😀", isARepeat: true, keyCode: 0)!
        _ = controller.handle(event, client: client)
        let expected = try! MuuEnvelope.encode(EventRequest(key: KeyEvent(eventType: "key_down", keycode: 0,
            modifiers: 0x20_0002, charactersUTF8: "A😀", charactersIgnoringModifiers: "a😀", isRepeat: true)))
        precondition(sent?.prefix(3) == expected.prefix(3))
        precondition(NSDictionary(dictionary: try! JSONSerialization.jsonObject(with: sent!.dropFirst(3)) as! [String: Any])
            .isEqual(to: try! JSONSerialization.jsonObject(with: expected.dropFirst(3)) as! [String: Any]))
    }

    /// Replay the observed one-line offset, then exercise the real controller
    /// and Objective-C exception boundary.
    private static func assertAttributeCandidateFallback() {
        func observation(_ rect: NSRect, orientation: Int = 1) -> MuuClientLineGeometry {
            var result = MuuClientLineGeometry()
            result.lineRect = rect
            result.returnedAttributes = true
            result.orientation = orientation
            return result
        }
        let screen = CandidateScreenFrame(frame: NSRect(x: 0, y: 0, width: 1728, height: 1117),
            visibleFrame: NSRect(x: 0, y: 0, width: 1728, height: 1084), backingScale: 2)
        let notes = observation(NSRect(x: 503, y: 839, width: 1, height: 23))
        let notesAnchors = CandidateSurfacePolicy.attributeFallbackAnchors(notes,
            markedRangeUTF16Length: 8, screens: [screen])!
        precondition(notesAnchors.primary == NSRect(x: 503, y: 862, width: 1, height: 23))
        precondition(notesAnchors.compositionRects.count == 1 && !notesAnchors.compositionRectsComplete)
        let notesPlacement = CandidateSurfacePolicy.geometry(anchors: notesAnchors,
            lastValidAnchor: nil, contentSize: NSSize(width: 200, height: 100))!
        precondition(notesAnchors.prefersAbove && notesPlacement.placedAbove)
        precondition(notesPlacement.frame.minY >= notesAnchors.compositionExclusionRects[0].maxY + 4)
        let discord = CandidateSurfacePolicy.attributeFallbackAnchors(
            observation(NSRect(x: 600, y: 27, width: 1, height: 21)),
            markedRangeUTF16Length: 6, screens: [screen])!
        precondition(discord.primary == NSRect(x: 600, y: 48, width: 1, height: 21))
        let above = CandidateSurfacePolicy.geometry(anchors: discord,
            lastValidAnchor: nil, contentSize: NSSize(width: 200, height: 216))!
        precondition(above.placedAbove && !above.frame.intersects(discord.compositionExclusionRects[0]))
        // The old below-first 220x30 panel overlapped the third line at y=86.
        let wrapped = CandidateSurfacePolicy.attributeFallbackAnchors(
            observation(NSRect(x: 659, y: 103, width: 1, height: 17)),
            markedRangeUTF16Length: 60, screens: [screen])!
        let wrappedLines = [120, 103, 86].map {
            NSRect(x: 659, y: CGFloat($0), width: 300, height: 17)
        }
        let wrappedPlacement = CandidateSurfacePolicy.geometry(anchors: wrapped,
            lastValidAnchor: nil, contentSize: NSSize(width: 220, height: 30))!
        precondition(wrappedPlacement.placedAbove)
        precondition(wrappedPlacement.frame == NSRect(x: 659, y: 158, width: 220, height: 30))
        precondition(wrappedLines.allSatisfy { !$0.intersects(wrappedPlacement.frame) })
        let top = CandidateSurfacePolicy.attributeFallbackAnchors(
            observation(NSRect(x: 659, y: 1033, width: 1, height: 17)),
            markedRangeUTF16Length: 60, screens: [screen])!
        let topPlacement = CandidateSurfacePolicy.geometry(anchors: top,
            lastValidAnchor: nil, contentSize: NSSize(width: 220, height: 30))!
        precondition(!topPlacement.placedAbove)
        precondition(topPlacement.frame == NSRect(x: 659, y: 999, width: 220, height: 30))
        // Preserve a visible, height-limited panel even when neither side fits
        // its full content; prefer the lower side if it has any space.
        let clipped = CandidateSurfacePolicy.geometry(anchors: notesAnchors,
            lastValidAnchor: nil, contentSize: NSSize(width: 220, height: 2000))!
        precondition(!clipped.placedAbove && clipped.frame.height < 2000)
        let bottom = CandidateSurfacePolicy.attributeFallbackAnchors(
            observation(NSRect(x: 659, y: 0, width: 1, height: 17)),
            markedRangeUTF16Length: 60, screens: [screen])!
        let onlyUpper = CandidateSurfacePolicy.geometry(anchors: bottom,
            lastValidAnchor: nil, contentSize: NSSize(width: 220, height: 2000))!
        precondition(onlyUpper.placedAbove && onlyUpper.frame.height < 2000)
        // A successful firstRect keeps its old below-first frame, even when
        // it too reports incomplete fragments. Preference is provenance-based.
        let firstRect = CandidateSurfaceAnchors(primary: wrapped.primary, retry: nil,
            selectedRange: nil, compositionRects: wrapped.compositionRects,
            markedRangeUTF16Length: 60, compositionRectsComplete: false,
            defaultAnchor: wrapped.defaultAnchor, visibleFrame: screen.visibleFrame, backingScale: 2)
        let firstRectPlacement = CandidateSurfacePolicy.geometry(anchors: firstRect,
            lastValidAnchor: nil, contentSize: NSSize(width: 220, height: 30))!
        precondition(!firstRect.prefersAbove && !firstRectPlacement.placedAbove)
        precondition(firstRectPlacement.frame == NSRect(x: 659, y: 69, width: 220, height: 30))
        for rect in [NSRect(x: CGFloat.nan, y: 839, width: 1, height: 23),
                     NSRect(x: 503, y: CGFloat.infinity, width: 1, height: 23),
                     NSRect(x: CGFloat.leastNonzeroMagnitude, y: 0, width: 1, height: 23),
                     NSRect(x: 503, y: 839, width: 0, height: 23),
                     NSRect(x: 503, y: 839, width: -1, height: 23),
                     NSRect(x: 503, y: 839, width: 1, height: -23),
                     NSRect(x: 503, y: 839, width: 1, height: CGFloat.leastNormalMagnitude),
                     NSRect(x: 1800, y: 839, width: 1, height: 23),
                     NSRect(x: 503, y: 1080, width: 1, height: 23),
                     NSRect(x: 1, y: CGFloat.greatestFiniteMagnitude, width: 1, height: CGFloat.greatestFiniteMagnitude)] {
            precondition(CandidateSurfacePolicy.attributeFallbackAnchors(observation(rect),
                markedRangeUTF16Length: 8, screens: [screen]) == nil)
        }
        for direction in [0, -1, 2] {
            precondition(CandidateSurfacePolicy.attributeFallbackAnchors(
                observation(notes.lineRect, orientation: direction),
                markedRangeUTF16Length: 8, screens: [screen]) == nil)
        }
        var missing = notes
        missing.returnedAttributes = false
        precondition(CandidateSurfacePolicy.attributeFallbackAnchors(missing,
            markedRangeUTF16Length: 8, screens: [screen]) == nil)
        let caught = MuuClientReadLineGeometry(MuuInputPathThrowingClientForTesting(), 0)
        precondition(caught.exception.boolValue)
        precondition(CandidateSurfacePolicy.attributeFallbackAnchors(caught,
            markedRangeUTF16Length: 8, screens: [screen]) == nil)
        precondition(CandidateSurfacePolicy.attributeFallbackAnchors(notes,
            markedRangeUTF16Length: 8, screens: []) == nil)

        let view = ViewState(preedit: [
            PreeditRun(text: "あい", utf16Start: 0, utf16Length: 2,
                isFocused: false, isConverted: true),
            PreeditRun(text: "うえ", utf16Start: 2, utf16Length: 2,
                isFocused: true, isConverted: true)],
            caretUTF16: 4, selection: TextRange(start: 2, len: 2), segments: nil,
            candidates: CandidateList(items: [CandidateItem(id: "fixture", text: "候補", annotation: nil)], selectedIndex: 0),
            commitText: nil, mode: .hiragana, compositionModeTransition: nil, poisoned: false, consumed: true, error: nil)
        guard let visible = NSScreen.screens.first?.visibleFrame else { preconditionFailure("screen required") }
        let usable = NSRect(x: visible.midX, y: visible.midY - 23, width: 1, height: 23)
        for kind in ["fallback", "fallback-top", "unwritten", "invalid", "vertical", "unknown", "fractional", "exception", "nonzero-exception", "firstRect"] {
            let fake = FakeTextClient()
            fake.attributeRectForTesting = kind == "unwritten" ? nil : kind == "invalid" ? .zero : usable
            if kind == "fallback-top" {
                fake.attributeRectForTesting = NSRect(x: visible.midX, y: visible.maxY - 69, width: 1, height: 23)
            }
            fake.attributeValuesForTesting = ["IMKTextOrientation": kind == "vertical" ? 0 : 1]
            if kind == "unknown" { fake.attributeValuesForTesting = [:] }
            if kind == "fractional" { fake.attributeValuesForTesting = ["IMKTextOrientation": 1.5] }
            fake.configureGeometry(markedRange: NSRange(location: 100, length: 4),
                selectedRange: NSRange(location: 102, length: 2)) { range in
                if kind == "firstRect" {
                    return CandidateClientRectResult(rect: NSRect(x: visible.midX, y: visible.midY, width: 80, height: 23), actualRange: range)
                }
                return CandidateClientRectResult(rect: NSRect(x: CGFloat.leastNonzeroMagnitude, y: 0,
                    width: CGFloat.leastNonzeroMagnitude, height: -1), actualRange: NSRange(location: 0, length: 0))
            }
            let client: IMKTextInput
            if kind == "exception" {
                client = MuuInputPathThrowingClientWithTargetForTesting(fake) as! IMKTextInput
            } else if kind == "nonzero-exception" {
                client = MuuInputPathThrowingClientAtIndexForTesting(fake, 2) as! IMKTextInput
            } else { client = fake }
            let controller = MuuInputController(testingClient: client)
            var order: [String] = []
            var productionAttributes = -1
            fake.geometryEventForTesting = { order.append($0) }
            controller.setApplyEventRecorderForTesting { event in
                if event == "candidateRender" {
                    productionAttributes = fake.attributeQueries.count
                    order.append("render")
                }
            }
            precondition(controller.applyForTesting(EngineResponse(status: MUU_RESULT_OK, view: view), to: client))
            precondition(controller.candidateSurfaceVisibleForTesting == (["fallback", "fallback-top", "nonzero-exception", "firstRect"].contains(kind)))
            if kind == "fallback" || kind == "fallback-top" {
                precondition(order.prefix(11) == ["markedRange", "selectedRange", "firstRect", "firstRect"]
                    + Array(repeating: "attributes", count: 6) + ["render"])
                precondition(productionAttributes == 6 && Array(fake.attributeQueries.prefix(6)) == [0, 2, 4, 100, 102, 104],
                    "both index spaces must be compared only after firstRect fails")
                // Badge geometry retains its strict firstRect path.
                let beforeBadge = fake.attributeQueries.count
                precondition(ClientGeometryReader.badgeGeometry(view: view, client: client,
                    screens: [screen], contentSize: NSSize(width: 32, height: 32)) == nil)
                precondition(fake.attributeQueries.count == beforeBadge)
            } else if kind == "nonzero-exception" {
                precondition(productionAttributes == 1 && Array(fake.attributeQueries.prefix(1)) == [0])
                precondition(order.prefix(6) == ["markedRange", "selectedRange", "firstRect", "firstRect", "attributes", "render"])
            } else if kind == "firstRect" {
                precondition(productionAttributes == 0)
                precondition(order.prefix(6) == ["markedRange", "selectedRange",
                    "firstRect", "firstRect", "firstRect", "render"])
                let actualAnchors = ClientGeometryReader.anchors(view: view, client: client)!
                precondition(!actualAnchors.prefersAbove)
                let actualPlacement = CandidateSurfacePolicy.geometry(anchors: actualAnchors,
                    lastValidAnchor: nil, contentSize: NSSize(width: 220, height: 30))!
                precondition(!actualPlacement.placedAbove)
                let scale = actualAnchors.backingScale
                let expectedBelowTop = floor((visible.midY - 27) * scale) / scale
                precondition(actualPlacement.frame == NSRect(
                    x: min(visible.midX, visible.maxX - 220), y: expectedBelowTop - 30,
                    width: 220, height: 30))
                precondition(fake.attributeQueries.isEmpty, "successful firstRect must never call attributes in normal builds")
            }
            controller.hideCandidateSurfaceForTesting()
        }
        print("attribute candidate fallback: coordinates, validation, exception/hide and firstRect priority passed")
        print("attribute fallback placement: three wrapped lines clear above; top row falls below; firstRect frame unchanged")
    }

    /// Numeric fixtures from the body-free build-481 capture. An index-space
    /// hypothesis must stand on its own evidence, without an application ID.
    private static func assertAttributeIndexSpaceSelection() {
        let screen = CandidateScreenFrame(frame: NSRect(x: 0, y: 0, width: 1728, height: 1117),
            visibleFrame: NSRect(x: 0, y: 0, width: 1728, height: 1084), backingScale: 2)
        func row(_ x: CGFloat, y: CGFloat = 103, height: CGFloat = 17) -> NSRect {
            NSRect(x: x, y: y, width: 1, height: height)
        }
        func select(_ rows: [Int: NSRect], location: Int = 12, length: Int = 6,
                    focus: Int? = 3, caret: Int = 6)
            -> (CandidateSurfacePolicy.AttributeSelection, [Int]) {
            let client = FakeTextClient()
            client.attributeResponseForTesting = { index in
                // The legacy wrapper returned a dictionary even for its
                // unwritten/zero rectangle, with no orientation attribute.
                (rows[index] ?? .zero, rows[index] == nil ? -1 : 1)
            }
            let selection = CandidateSurfacePolicy.attributeFallbackSelection(
                markedRange: NSRange(location: location, length: length), focusedStart: focus,
                caret: caret, screens: [screen]) { MuuClientReadLineGeometry(client, UInt($0)) }
            precondition(selection.queryCount == client.attributeQueries.count && selection.queryCount <= 6)
            precondition(Set(client.attributeQueries).count == client.attributeQueries.count)
            let placement = selection.anchors.flatMap {
                CandidateSurfacePolicy.geometry(anchors: $0, lastValidAnchor: nil,
                    contentSize: NSSize(width: 220, height: 240))
            }
            InputPathContractTrace.record("attribute_selection", [
                "marked": InputPathContractTrace.range(NSRange(location: location, length: length)),
                "focus": focus as Any? ?? NSNull(), "caret": caret,
                "query_indices": client.attributeQueries,
                "space": selection.space?.rawValue as Any? ?? NSNull(),
                "reason": selection.reason.rawValue, "index": selection.index as Any? ?? NSNull(),
                "omitted_endpoint": selection.omittedCaretEndpoint,
                "anchor": selection.anchors?.primary.map(InputPathContractTrace.rect) as Any? ?? NSNull(),
                "frame": placement.map { InputPathContractTrace.rect($0.frame) } as Any? ?? NSNull(),
                "placed_above": placement.map(\.placedAbove) as Any? ?? NSNull(),
            ])
            return (selection, client.attributeQueries)
        }
        // A Tauri text input supplies ordered inline points, while document
        // points are zero, and the caret exactly at the endpoint is unwritten.
        for sample in [(12, CGFloat(749), CGFloat(788)), (7, CGFloat(712), CGFloat(750))] {
            let rows = [0: row(sample.1), 3: row(sample.2)]
            let (initial, initialQueries) = select(rows, location: sample.0, focus: 0, caret: 3)
            let (moved, movedQueries) = select(rows, location: sample.0)
            precondition(initial.space == .inline && moved.space == .inline)
            precondition(initial.reason == .inlineOnly && moved.reason == .inlineOnly)
            precondition(initial.anchors?.primary?.minX == sample.1 && moved.anchors?.primary?.minX == sample.2)
            precondition(!initial.omittedCaretEndpoint && moved.omittedCaretEndpoint)
            precondition(initial.index == 0 && moved.index == 3)
            precondition(initialQueries == [0, 3, sample.0, sample.0 + 3])
            precondition(movedQueries == [0, 3, 6, sample.0, sample.0 + 3, sample.0 + 6])
            // Only the horizontal anchor follows focus. Exclusion still uses
            // the composition's first line, with the existing line expansion.
            precondition(moved.anchors?.compositionRects == initial.anchors?.compositionRects)
            precondition(moved.anchors?.compositionExclusionRects == initial.anchors?.compositionExclusionRects)
            let geometry = CandidateSurfacePolicy.geometry(anchors: moved.anchors!, lastValidAnchor: nil,
                contentSize: NSSize(width: 220, height: 240))!
            precondition(geometry.placedAbove && geometry.frame.minX == sample.2)
        }
        // Discord 60/95: inline 3 is left of inline 0; document points are
        // unwritten. Do not guess that the leftmost point is the start.
        let discordRows = [0: row(666, y: 27, height: 21), 3: row(586, y: 27, height: 21)]
        for (focus, caret) in [(0, 3), (3, 6)] {
            let (discord, _) = select(discordRows, location: 8, focus: focus, caret: caret)
            precondition(discord.space == nil && discord.reason == .inconsistent)
            precondition(discord.index == 0 && discord.anchors?.primary?.minX == 666)
        }
        // Build 484, Discord 427/462: its state changed and the document
        // hypothesis now supplies ordered points while inline still reverses.
        let discordDocumentRows = [0: row(600, y: 27, height: 21), 3: row(521, y: 27, height: 21),
            1: row(489, y: 27, height: 21), 4: row(537, y: 27, height: 21)]
        for (focus, caret, x) in [(0, 3, CGFloat(489)), (3, 6, CGFloat(537))] {
            let (discord, _) = select(discordDocumentRows, location: 1, focus: focus, caret: caret)
            precondition(discord.space == .document && discord.reason == .documentOnly)
            precondition(discord.anchors?.primary?.minX == x)
        }
        // A Tauri search field near the screen top: both interpretations
        // coincide at marked.location=0. Preserve the measured lower placement.
        let (top, topQueries) = select([0: row(40, y: 968, height: 18), 1: row(52, y: 968, height: 18)],
            location: 0, focus: 0, caret: 1)
        precondition(top.reason == .coincident && topQueries == [0, 1])
        let topGeometry = CandidateSurfacePolicy.geometry(anchors: top.anchors!, lastValidAnchor: nil,
            contentSize: NSSize(width: 220, height: 240))!
        precondition(!topGeometry.placedAbove)
        precondition(topGeometry.frame == NSRect(x: 40, y: 724, width: 220, height: 240))
        precondition(top.anchors!.primary!.minY - topGeometry.frame.maxY == 22)

        let (document, _) = select([0: row(600), 12: row(100), 15: row(140)])
        precondition(document.space == .document && document.reason == .documentOnly)
        precondition(document.index == 15 && document.anchors?.primary?.minX == 140)
        let (ambiguous, _) = select([0: row(100), 3: row(140), 12: row(500), 15: row(540)])
        precondition(ambiguous.space == nil && ambiguous.reason == .ambiguous)
        precondition(ambiguous.anchors?.primary?.minX == 100)
        let (coincident, _) = select([0: row(100), 3: row(140), 12: row(100), 15: row(140)])
        precondition(coincident.reason == .coincident && coincident.anchors?.primary?.minX == 140)

        // At scale 2 the whole sequence may reverse by at most 0.5pt, not
        // another 0.5pt at each step. The exact one-pixel boundary is inclusive.
        let (cumulativeReverse, _) = select([0: row(100), 3: row(99.5), 6: row(99)])
        precondition(cumulativeReverse.space == nil && cumulativeReverse.reason == .inconsistent)
        precondition(cumulativeReverse.index == 0 && cumulativeReverse.anchors?.primary?.minX == 100)
        let withinPixelSequences: [[CGFloat]] = [[100, 99.5, 99.5], [100, 99.75, 99.5], [100, 100.5, 100]]
        for xs in withinPixelSequences {
            let (withinPixel, _) = select([0: row(xs[0]), 3: row(xs[1]), 6: row(xs[2])])
            precondition(withinPixel.space == .inline && withinPixel.reason == .inlineOnly)
            precondition(withinPixel.index == 3 && withinPixel.anchors?.primary?.minX == xs[1])
        }

        // Only an invalid terminal caret may be omitted, never an in-range
        // point or a valid endpoint on another line / behind earlier points.
        for (rows, caret) in [([0: row(100), 3: row(140)], 4),
                              ([0: row(100), 3: row(140), 6: row(160, y: 86)], 6),
                              ([0: row(100), 3: row(140), 6: row(120)], 6)] {
            let (rejected, _) = select(rows, caret: caret)
            precondition(rejected.space == nil && rejected.reason == .inconsistent)
            precondition(!rejected.omittedCaretEndpoint && rejected.anchors?.primary?.minX == 100)
        }
        let (single, _) = select([0: row(100)], focus: 0)
        precondition(single.space == nil && single.reason == .singlePoint && single.anchors?.primary?.minX == 100)
        let (noFocus, _) = select([0: row(100), 3: row(140)], focus: nil)
        precondition(noFocus.space == nil && noFocus.anchors?.primary?.minX == 100)
        let (invalidFocus, invalidQueries) = select([0: row(100)], focus: -1, caret: 7)
        precondition(invalidFocus.space == nil && invalidQueries == [0, 12])
        let (missing, _) = select([:])
        precondition(missing.anchors == nil && missing.reason == .noUsableAnchor)

        for (throwingIndex, expectedQueries) in [(0, [0]), (3, [0, 3]), (6, [0, 3, 6]), (12, [0, 3, 6, 12])] {
            var queries: [Int] = []
            let throwing = CandidateSurfacePolicy.attributeFallbackSelection(
                markedRange: NSRange(location: 12, length: 6), focusedStart: 3, caret: 6, screens: [screen]) { index in
                    queries.append(index)
                    if index == throwingIndex { return MuuClientReadLineGeometry(MuuInputPathThrowingClientForTesting(), UInt(index)) }
                    let client = FakeTextClient()
                    client.attributeRectForTesting = row(index == 0 ? 100 : 140)
                    client.attributeValuesForTesting = ["IMKTextOrientation": 1]
                    return MuuClientReadLineGeometry(client, UInt(index))
                }
            precondition(queries == expectedQueries)
            if throwingIndex == 0 {
                precondition(throwing.anchors == nil && throwing.reason == .exception)
            } else {
                precondition(throwing.space == nil && throwing.reason == .exceptionFallback)
                precondition(throwing.index == 0 && throwing.anchors?.primary?.minX == 100)
            }
        }
        let overflow = CandidateSurfacePolicy.attributeFallbackSelection(
            markedRange: NSRange(location: Int.max - 2, length: 6), focusedStart: 3, caret: 6,
            screens: [screen]) { _ in preconditionFailure("invalid ranges must not query the client") }
        precondition(overflow.anchors == nil && overflow.reason == .invalidRange && overflow.queryCount == 0)
        print("attribute index spaces: inline and document coordinates, fixed fallback and upper-row placement passed")
        print("attribute index spaces: document/coincident/ambiguous, endpoint-only omission, invalid and exception guards passed")
        print("attribute index spaces: cumulative two-pixel reversal rejected; whole-sequence one-pixel boundary accepted")
    }

    // UI cleanup called on main runs at once; called from a background thread it is
    // delivered on main. The watchdog only guards against a hang.
    private static func assertMainActorCleanupBoundary() {
        @MainActor final class State { var count = 0 }
        let state = State()
        finishInputUIOnMain { state.count += 1 }
        precondition(state.count == 1, "main cleanup must remain immediate")
        let watchdog = DispatchWorkItem { preconditionFailure("main cleanup callback hung") }
        DispatchQueue.main.asyncAfter(deadline: .now() + 180, execute: watchdog)
        DispatchQueue.global().async {
            finishInputUIOnMain {
                precondition(Thread.isMainThread)
                state.count += 1
                CFRunLoopStop(CFRunLoopGetMain())
            }
        }
        while state.count != 2 { RunLoop.main.run(mode: .default, before: .distantFuture) }
        watchdog.cancel()
        print("UI cleanup: immediate on main; background completion delivered on main")
    }

    /// Transfer the final owners, rather than just a cleanup closure, off main.
    /// The release notification cannot wait for main: main deliberately waits
    /// here before pumping the FIFO sentinel behind all three cleanups.
    private static func assertRealUIOffMainTeardown() {
        final class Transfer: @unchecked Sendable {
            private let lock = NSLock()
            private var values: [AnyObject]
            init(_ values: [AnyObject]) { self.values = values }
            func release() { lock.lock(); values.removeAll(); lock.unlock() }
        }
        @MainActor final class Completion { var finished = false; var cancelled = false }
        let completion = Completion()
        let scheduler = ManualModeBadgeScheduler()
        weak var weakClient: FakeTextClient?
        var candidateNumber = 0
        var badgeNumber = 0
        let transfer: Transfer = autoreleasepool {
            let client = FakeTextClient()
            weakClient = client
            var authority = CandidateActionAuthority()
            let items = [CandidateSurfaceItem(id: "teardown", text: "候補", annotation: nil)]
            let snapshot = authority.snapshot(items: items, selectedIndex: 0)!
            let row = NSRect(x: 240, y: 520, width: 40, height: 20)
            let panel = CandidatePanelController { _ in }
            panel.render(snapshot: snapshot, anchors: CandidateSurfaceAnchors(primary: row,
                retry: nil, selectedRange: nil, compositionRects: [row], defaultAnchor: row,
                visibleFrame: NSRect(x: 0, y: 0, width: 1200, height: 900)))
            precondition(panel.isVisibleForTesting)
            candidateNumber = panel.windowNumberForTesting
            let badge = ModeBadgeController(screenProvider: {
                [CandidateScreenFrame(frame: NSRect(x: 0, y: 0, width: 1200, height: 900),
                    visibleFrame: NSRect(x: 0, y: 0, width: 1200, height: 900), backingScale: 2)]
            }, scheduler: scheduler.scheduler)
            let view = try! JSONDecoder().decode(ViewState.self, from: Data(
                #"{"preedit":[],"caret_utf16":0,"mode":"hiragana","consumed":true}"#.utf8))
            badge.show(transition: .literal, view: view, client: client)
            precondition(badge.isVisibleForTesting && scheduler.activeTaskCount == 1)
            badgeNumber = badge.windowNumberForTesting
            let task = ModeBadgeScheduledTask {
                precondition(Thread.isMainThread)
                completion.cancelled = true
            }
            return Transfer([panel, badge, task])
        }
        precondition(weakClient == nil, "UI and its pending work must not keep the client alive")
        func visible(_ number: Int) -> Bool {
            NSApp.windows.contains { $0.windowNumber == number && $0.isVisible }
        }
        let released = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            precondition(!Thread.isMainThread)
            transfer.release()
            released.signal()
        }
        precondition(released.wait(timeout: .now() + 180) == .success, "off-main deinit waited for main")
        precondition(visible(candidateNumber) && visible(badgeNumber))
        precondition(!completion.cancelled && scheduler.activeTaskCount == 1)
        let watchdog = DispatchWorkItem { preconditionFailure("UI teardown callback hung") }
        DispatchQueue.main.asyncAfter(deadline: .now() + 180, execute: watchdog)
        DispatchQueue.main.async {
            precondition(Thread.isMainThread)
            precondition(!visible(candidateNumber) && !visible(badgeNumber))
            precondition(completion.cancelled && scheduler.activeTaskCount == 0)
            completion.finished = true
            CFRunLoopStop(CFRunLoopGetMain())
        }
        while !completion.finished { RunLoop.main.run(mode: .default, before: .distantFuture) }
        watchdog.cancel()
        print("real UI teardown: candidate, badge and timer released off main; cleanup on main")
    }

    // Once the input gate closes at termination, keys and clicks pass through and
    // activate, deactivate, commit and mode changes do nothing: no forced commit, no
    // presentation change, no Core request.
    private static func assertClosingInputPassesThrough() {
        let session = ScriptedInputSession(), client = FakeTextClient()
        let controller = MuuInputController(testingClient: client, testingSession: session)
        let response = ViewState(preedit: [.init(text: "むう", utf16Start: 0, utf16Length: 2,
            isFocused: true, isConverted: false)], caretUTF16: 2, selection: nil,
            segments: nil, candidates: nil, commitText: nil, mode: .hiragana,
            compositionModeTransition: nil, poisoned: false, consumed: true, error: nil)
        session.responses.append(.init(status: MUU_RESULT_OK, view: response))
        let event = keyEvent(keyCode: 0, characters: "a")
        precondition(controller.handle(event, client: client))
        let effects = client.commands
        ApplicationLifecycle.inputGate.close()
        precondition(!controller.handle(event, client: client) && !controller.handle(mouseDownEvent(), client: client))
        controller.activateServer(client); controller.deactivateServer(client); controller.commitComposition(client)
        controller.setValue(InputMode.halfAlnum.inputModeIdentifier, forTag: Int(kTextServiceInputModePropertyTag), client: client)
        precondition(session.requests.count == 1 && client.commands == effects && controller.composedTextForTesting == "むう",
            "closing neither forces a commit nor changes presentation or sends new Core requests")
    }

    // The controller against a scripted session, with no Rust: the exact request bytes
    // it sends, a synchronous consumed flag, failure and session recreation, commit on
    // click, mode changes and clearing the preceding context.
    private static func assertScriptedSessionBoundary() {
        func matches(_ actual: Data?, _ expected: Data) -> Bool {
            guard let actual, actual.prefix(3) == expected.prefix(3) else { return false }
            // JSON object key order is not part of the envelope contract.
            // Compare every field while keeping the wire header exact.
            let a = try! JSONSerialization.jsonObject(with: actual.dropFirst(3))
            let b = try! JSONSerialization.jsonObject(with: expected.dropFirst(3))
            return (try! JSONSerialization.data(withJSONObject: a, options: [.sortedKeys]))
                == (try! JSONSerialization.data(withJSONObject: b, options: [.sortedKeys]))
        }
        func view(_ text: String = "", consumed: Bool = true, mode: InputMode = .hiragana,
                  commit: String? = nil) -> ViewState {
            let length = (text as NSString).length
            return ViewState(preedit: text.isEmpty ? [] : [PreeditRun(text: text, utf16Start: 0, utf16Length: length,
                isFocused: true, isConverted: false)],
                caretUTF16: length, selection: nil, segments: nil, candidates: nil,
                commitText: commit, mode: mode, compositionModeTransition: nil, poisoned: false, consumed: consumed, error: nil)
        }
        let session = ScriptedInputSession()
        let client = FakeTextClient()
        let controller = MuuInputController(testingClient: client, testingSession: session)
        let raw = "か\u{3099}😀"
        let event = keyEvent(keyCode: 0, characters: "a")
        session.responses.append(EngineResponse(status: MUU_RESULT_OK, view: view(raw, consumed: false)))
        precondition(!controller.handle(event, client: client))
        precondition(client.commands.last == .mark(raw) && controller.composedTextForTesting == raw)
        precondition(Array(client.markedText!.string.utf8) == Array(raw.utf8))
        precondition(Array(controller.composedTextForTesting.utf8) == Array(raw.utf8))
        let expected = try! MuuEnvelope.encode(EventRequest(key: KeyEvent(eventType: "key_down", keycode: 0,
            modifiers: 0, charactersUTF8: "a", charactersIgnoringModifiers: "a", isRepeat: false)))
        precondition(session.requests.count == 1 && matches(session.requests.first, expected))

        session.responses.append(EngineResponse(status: MUU_RESULT_INTERNAL, view: nil))
        precondition(controller.handle(event, client: client))
        precondition(controller.composedTextForTesting == raw && session.recreations == 0)
        session.responses.append(EngineResponse(status: MUU_RESULT_SESSION_INVALID, view: nil))
        precondition(controller.handle(event, client: client))
        precondition(controller.composedTextForTesting.isEmpty && client.commands.last == .mark(""))
        precondition(session.recreations == 1)

        session.responses.append(EngineResponse(status: MUU_RESULT_OK, view: view(raw)))
        precondition(controller.handle(event, client: client))
        session.responses.append(EngineResponse(status: MUU_RESULT_OK, view: view(commit: "確定")))
        precondition(!controller.handle(mouseDownEvent(), client: client))
        precondition(client.commands.last == .insert("確定"))
        precondition(matches(session.requests.last, try! MuuEnvelope.encode(CommandRequest(command: "force_commit"))))

        session.responses.append(EngineResponse(status: MUU_RESULT_OK, view: view(mode: .halfAlnum)))
        controller.setValue(InputMode.halfAlnum.inputModeIdentifier, forTag: Int(kTextServiceInputModePropertyTag), client: client)
        precondition(controller.currentModeForTesting == .halfAlnum)
        precondition(matches(session.requests.last, try! MuuEnvelope.encode(CommandRequest(command: "set_mode", arg: .mode(.halfAlnum)))))
        session.responses.append(EngineResponse(status: MUU_RESULT_OK, view: view()))
        controller.deactivateServer(client)
        precondition(matches(session.requests.last, try! MuuEnvelope.encode(CommandRequest(command: "clear_preceding_context"))))
        precondition(session.responses.isEmpty && session.requests.count == 7)
        session.isAvailable = false
        precondition(!controller.handle(event, client: client) && session.requests.count == 7)
        print("scripted session: raw request, synchronous consumed, failure/recreate, click commit, mode and context clear passed")
    }

    private static func assertProductionGenerationE2E(
        dictionaryPath: String,
        personalFrequency: PersonalFrequencySnapshot = .empty,
        userDictionaryFile: Data? = nil,
        expectedPersonalization: Bool = false,
        requireBaselineHold: Bool = false,
        inputRomaji: String = "honkennnituitehabettokyougiwoshiyou",
        expectedSurface: String = "本件については別途競技を使用",
        corruptCommitReply: Bool = false
    ) {
        let fileManager = FileManager.default
        // The recorder's ancestor safety policy rejects macOS /tmp -> /var
        // symlink traversal. Keep this production-shaped fixture under the
        // checked-out worktree, whose ancestors are regular directories.
        let root = URL(fileURLWithPath: fileManager.currentDirectoryPath)
            .appendingPathComponent(".muu-native-e2e-\(UUID().uuidString)", isDirectory: true)
        try! fileManager.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fileManager.removeItem(at: root) }

        let (bundle, _) = makeBundleFixture(root: root)

        let nativeConfiguration = CommitStreamRecorder.productionConfigurationForTesting(
            bundle: bundle,
            processNonce: "native-e2e-process"
        )!
        let recorderDirectory = root.appendingPathComponent("CommitStream", isDirectory: true)
        let recorder = CommitStreamRecorder(
            processConfiguration: nativeConfiguration,
            directoryURL: recorderDirectory,
            fileManager: fileManager
        )
        let previousObserver = RustEngine.recordObserverForTesting
        var delivered: [Data] = []
        RustEngine.recordObserverForTesting = { data in previousObserver?(data); delivered.append(data) }
        defer { RustEngine.recordObserverForTesting = previousObserver }
        let engine = RustEngine(
            testingCommitStream: nativeConfiguration.makeEngineConfiguration(),
            dictionaryPath: dictionaryPath,
            recorder: recorder,
            personalFrequency: personalFrequency,
            userDictionaryFile: userDictionaryFile
        )
        precondition(engine.isAvailable, "production FFI engine must create")

        for character in inputRomaji {
            _ = engine.handle(EventRequest(key: KeyEvent(
                eventType: "key_down",
                keycode: 0,
                modifiers: 0,
                charactersUTF8: String(character),
                charactersIgnoringModifiers: String(character),
                isRepeat: false
            )))
        }
        let conversionResponse = engine.handle(EventRequest(key: KeyEvent(
            eventType: "key_down",
            keycode: 49,
            modifiers: 0,
            charactersUTF8: " ",
            charactersIgnoringModifiers: " ",
            isRepeat: false
        )))
        precondition(
            conversionResponse.view?.preedit.map({ $0.text }).joined() == expectedSurface,
            "production FFI conversion did not produce the expected generation surface"
        )
        precondition(
            conversionResponse.view?.segments != nil,
            "production FFI conversion did not expose committed segments"
        )
        precondition(conversionResponse.status == MUU_RESULT_OK, "conversion must succeed")
        var commitSurface = expectedSurface
        if corruptCommitReply {
            // Choose an alternative so this fixture actually produces a
            // positive learning observation, rather than an automatic hold.
            let candidates = conversionResponse.view!.candidates!
            precondition(candidates.items.count > 1)
            let selected = engine.handle(CommandRequest(command: "select_candidate",
                arg: .candidate(id: candidates.items[1].id)))
            precondition(selected.status == MUU_RESULT_OK)
            commitSurface = selected.view!.preedit.map(\.text).joined()
            precondition(commitSurface == candidates.items[1].text && commitSurface != expectedSurface)
        }
        let previousTransform = RustEngine.viewTransformForTesting
        let previousLearningObserver = PersonalFrequencyStore.receiveObserverForTesting
        var learned: [LearningVersion] = []
        if corruptCommitReply {
            RustEngine.viewTransformForTesting = { data in
                var payload = try! JSONSerialization.jsonObject(with: data.dropFirst(3)) as! [String: Any]
                payload["candidates"] = "broken display metadata"
                return Data(data.prefix(3)) + (try! JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]))
            }
            PersonalFrequencyStore.receiveObserverForTesting = { bytes, version in
                previousLearningObserver?(bytes, version)
                learned.append(version)
            }
        }
        defer {
            RustEngine.viewTransformForTesting = previousTransform
            PersonalFrequencyStore.receiveObserverForTesting = previousLearningObserver
        }
        let commitResponse = engine.handle(EventRequest(key: KeyEvent(
            eventType: "key_down",
            keycode: corruptCommitReply ? 0 : 36,
            modifiers: 0,
            charactersUTF8: corruptCommitReply ? "a" : "\r",
            charactersIgnoringModifiers: corruptCommitReply ? "a" : "\r",
            isRepeat: false
        )))
        precondition(
            commitResponse.view?.commitText == commitSurface,
            "production FFI fixture must commit the expected surface"
        )
        precondition(commitResponse.status == MUU_RESULT_OK, "commit must succeed")
        if corruptCommitReply {
            precondition(commitResponse.view?.preedit.map(\.text).joined() == "あ")
            let client = FakeTextClient()
            let controller = MuuInputController(testingClient: client)
            precondition(controller.applyForTesting(commitResponse, to: client))
            precondition(client.commands == [.insert(commitSurface), .mark("あ")])
            if learned.count != 1 {
                let snapshot = try! RustCore.personalFrequency(after: nil).get()!
                let facts = try! JSONSerialization.jsonObject(with: snapshot.0) as! [String: Any]
                fputs("recovery learning actual=\(learned.count) revision=\(snapshot.1.revision) observations=\(facts["observation_count"]!) stopped=\(PersonalFrequencyStore.shared.persistenceStatus.stopped)\n", stderr)
            }
            precondition(learned.count == 1, "one commit must deliver one learning update")
            engine.drainPersonalFrequencySnapshot()
            precondition(learned.count == 1, "decode recovery must not repeat learning")
            RustEngine.viewTransformForTesting = previousTransform
        }
        engine.drainCommitStreamEvents(to: recorder)
        recorder.waitUntilIdleForTesting()
        let files = (try? fileManager.contentsOfDirectory(atPath: recorderDirectory.path))?
            .filter { $0.hasPrefix("commit-stream-v5-") } ?? []
        precondition(
            files.count == 1,
            "actual Rust Session must emit one v5 event; recorder=\(recorder.failureForTestingDescription ?? "none")"
        )
        let saved = fileManager.contents(atPath: recorderDirectory.appendingPathComponent(files[0]).path)!
        precondition(delivered.count == 1 && saved == delivered[0] + Data([10]), "weighted ABI -> Swift file must preserve opaque bytes")
        let event = try! JSONSerialization.jsonObject(
            with: fileManager.contents(atPath: recorderDirectory.appendingPathComponent(files[0]).path)!
        ) as! [String: Any]
        precondition(event["schema"] as? String == "muu.commit-stream.v5")
        precondition(event["native_scorer"] == nil)
        let authority = event["provenance"] as! [String:Any]
        let dictionary = authority["dictionary"] as! [String:Any]
        precondition((dictionary["payload_sha256"] as? String)?.count == 64)
        let learning = authority["learning"] as! [String:Any]
        precondition(learning["personalization_applied"] as? Bool == expectedPersonalization)
        if corruptCommitReply {
            let final = event["final"] as! [String: Any]
            precondition(final["surface"] as? String == commitSurface)
            precondition((final["segments"] as! [[String: Any]]).map { $0["surface"] as! String }.joined() == commitSurface)
            let snapshot = try! RustCore.personalFrequency(after: nil).get()!
            let facts = try! JSONSerialization.jsonObject(with: snapshot.0) as! [String: Any]
            precondition(snapshot.1.revision == 1 && facts["observation_count"] as? Int == 1)
        } else {
            assertNativeV5EventSemantics(
                event,
                expectedPersonalization: expectedPersonalization,
                requireBaselineHold: requireBaselineHold,
                expectedSurface: expectedSurface
            )
        }

    }

    // Drop accounting end to end. The first record carries the earlier debt of 3.
    // When it is discarded after its retries, Core gets back a weight of 4 (that
    // debt plus the record itself), and the next record carries 4. The cooldown
    // from the failures still applies to that next record.
    private static func assertCommitDeliveryDebtE2E(dictionaryPath: String) {
        let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath).appendingPathComponent(".muu-debt-e2e-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let clock = CommitStreamTestClock()
        let faultLock = NSLock()
        var failing = true
        let returned = DispatchSemaphore(value: 0)
        let config = CommitStreamProcessConfiguration(processNonce: "debt-e2e", sourceCommit: "test", build: 1)
        let recorder = CommitStreamRecorder(processConfiguration: config, directoryURL: root, clock: clock.clock,
            reportDropped: { data in
                precondition((try! CommitStreamDropWeight.decode(data)) == 4)
                let status = data.withUnsafeBytes { muu_core_report_commit_stream_dropped($0.bindMemory(to: UInt8.self).baseAddress, $0.count) }
                precondition(status == MUU_RESULT_OK); returned.signal(); return status
            }, checkpoint: { _ in if faultLock.withLock({ failing }) { throw POSIXError(.ENOSPC) } })
        let engine = RustEngine(testingCommitStream: config, dictionaryPath: dictionaryPath, recorder: recorder)
        let debt = Data("{\"schema_version\":1,\"drop_weight\":3}".utf8)
        precondition(debt.withUnsafeBytes { muu_core_report_commit_stream_dropped($0.bindMemory(to: UInt8.self).baseAddress, $0.count) } == MUU_RESULT_OK)
        func commit() {
            for character in "kyou" {
                _ = engine.handle(EventRequest(key: KeyEvent(eventType: "key_down", keycode: 0, modifiers: 0, charactersUTF8: String(character), charactersIgnoringModifiers: String(character), isRepeat: false)))
            }
            _ = engine.handle(EventRequest(key: KeyEvent(eventType: "key_down", keycode: 49, modifiers: 0, charactersUTF8: " ", charactersIgnoringModifiers: " ", isRepeat: false)))
            let response = engine.handle(EventRequest(key: KeyEvent(eventType: "key_down", keycode: 36, modifiers: 0, charactersUTF8: "\r", charactersIgnoringModifiers: "\r", isRepeat: false)))
            precondition(response.view?.commitText == "今日")
            engine.drainCommitStreamEvents(to: recorder)
        }
        var seen: [Data] = []
        let previous = RustEngine.recordObserverForTesting
        RustEngine.recordObserverForTesting = { seen.append($0) }
        defer { RustEngine.recordObserverForTesting = previous }
        commit(); recorder.waitUntilIdleForTesting()
        for delay in [1.0,2,4,8,16] { clock.advance(delay); recorder.waitUntilIdleForTesting() }
        precondition(returned.wait(timeout: .now() + 180) == .success)
        precondition(recorder.status.discarded == 1 && recorder.status.pendingCount == 0)
        faultLock.withLock { failing = false }
        commit() // Cooldown persists across final discard.
        precondition(recorder.status.pendingCount == 1)
        clock.advance(16); recorder.waitUntilIdleForTesting()
        precondition(seen.count == 2)
        let first = try! JSONSerialization.jsonObject(with: seen[0]) as! [String:Any]
        let second = try! JSONSerialization.jsonObject(with: seen[1]) as! [String:Any]
        precondition((first["dropped_records"] as? NSNumber)?.uint64Value == 3)
        precondition((second["dropped_records"] as? NSNumber)?.uint64Value == 4)
        let saved = try! Data(contentsOf: root.appendingPathComponent("commit-stream-v5-0.jsonl"))
        precondition(saved == seen[1] + Data([10]))
        precondition(!recorder.status.retrying && recorder.status.discarded == 1)
    }

    // A minimal bundle with the Info.plist keys the production recorder configuration
    // reads (source commit and build number), so recording can be configured without
    // the app.
    private static func makeBundleFixture(root: URL) -> (Bundle, Data) {
        let fileManager = FileManager.default
        let bundleRoot = root.appendingPathComponent("MuuIME.bundle", isDirectory: true)
        try! fileManager.createDirectory(at: bundleRoot, withIntermediateDirectories: true)
        let info: [String: Any] = [
            "CFBundleIdentifier": "dev.muu.inputmethod.Muu.native-e2e",
            "CFBundleName": "MuuNativeE2E",
            "CFBundlePackageType": "BNDL",
            "CFBundleVersion": "296",
            "MuuSourceCommit": String(repeating: "a", count: 40),
        ]
        let infoData = try! PropertyListSerialization.data(
            fromPropertyList: info,
            format: .xml,
            options: 0
        )
        try! infoData.write(to: bundleRoot.appendingPathComponent("Info.plist"), options: .atomic)
        let bundle = Bundle(url: bundleRoot)!

        return (bundle, infoData)
    }

    // A refused final cancel (INTERNAL or BUSY) after a saved import. The editor stops
    // with restart guidance and sends no new operation, the saved pointer stays, and
    // Core is not marked invalid: conversion and commit keep working while the
    // imported-edit reservation is still held.
    private static func assertImportedCompletionFailure(dictionaryPath: String, status: Int32) {
        let engine = RustEngine(testingCommitStream: nil, dictionaryPath: dictionaryPath)
        precondition(engine.isAvailable)
        let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".muu-imported-completion-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        var bridge = RustCore.importedDictionaryBridge
        let cancel = bridge.cancel
        let prepare = bridge.prepare
        var failCancel = false
        var prepares = 0
        bridge.cancel = { bytes in
            if !failCancel { return cancel(bytes) }
            // Simulate the ACK while retaining the real Core reservation.
            // 6/9 must not poison Core, even though editing needs a restart.
            RustCore.observeStatus(status)
            return status
        }
        bridge.prepare = { command, file in prepares += 1; return try prepare(command, file) }
        let store = ImportedDictionaryStore(baseDirectory: root, bundleIdentifier: "fixture", bridge: bridge)
        func wait(_ condition: () -> Bool) {
            // Minutes are a hang guard, never a responsiveness threshold.
            let deadline = Date().addingTimeInterval(180)
            while !condition() && Date() < deadline { RunLoop.current.run(until: Date().addingTimeInterval(0.005)) }
            precondition(condition(), "imported completion did not arrive")
        }
        var loaded = false
        store.start { result in
            if case let .failure(error) = result { preconditionFailure("fixture load: \(error)") }
            loaded = true
        }
        wait { loaded && !store.isBusy }
        let source = root.appendingPathComponent("fixture.muudict")
        try! bridge.template().write(to: source)
        var prepared: PreparedImportedDictionary?
        store.prepareDictionaryFile(at: source) { prepared = try! $0.get() }
        wait { prepared != nil }
        failCancel = true
        var installed = false
        store.install(prepared!) { precondition((try? $0.get()) != nil); installed = true }
        wait { installed && !store.isBusy }
        precondition(!store.canEdit && !RustCore.isInvalid)
        precondition(store.loadError?.localizedDescription.contains("再起動すると編集できる") == true)
        let count = prepares
        var refused = false
        store.remove(dictionaryID: store.dictionaries[0].metadata.id) { refused = (try? $0.get()) == nil }
        precondition(refused && prepares == count, "stopped editor must not create a BUSY operation")
        precondition(FileManager.default.fileExists(atPath: store.fileURL.path), "saved pointer remains")
        for character in "kyou" {
            precondition(engine.handle(EventRequest(key: KeyEvent(eventType: "key_down", keycode: 0,
                modifiers: 0, charactersUTF8: String(character), charactersIgnoringModifiers: String(character), isRepeat: false))).status == 0)
        }
        let converted = engine.handle(CommandRequest(command: "convert"))
        precondition(converted.status == 0 && converted.view?.preedit.map(\.text).joined() == "今日")
        precondition(engine.handle(CommandRequest(command: "commit")).status == 0,
            "conversion and commit remain usable with an unreleased imported-edit ticket")
    }

    // Recording works with only the ordinary dictionary file copied in: no model, no
    // other resource next to it.
    private static func assertDictionaryOnlyRecording(dictionaryPath: String) {
        let fileManager = FileManager.default
        let root = URL(fileURLWithPath: fileManager.currentDirectoryPath)
            .appendingPathComponent(".muu-dictionary-recording-\(UUID().uuidString)", isDirectory: true)
        try! fileManager.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fileManager.removeItem(at: root) }
        let (bundle, _) = makeBundleFixture(root: root)
        precondition(CommitStreamRecorder.productionConfigurationForTesting(bundle: bundle) != nil)
        // An ordinary dictionary is sufficient (docs/src/internals/storage.md「記録」). Copy only that file so
        // this fixture cannot accidentally rely on an adjacent extra resource.
        let dictionary = root.appendingPathComponent("mozc-oss.muudict")
        try! fileManager.copyItem(at: URL(fileURLWithPath: dictionaryPath), to: dictionary)
        assertProductionGenerationE2E(dictionaryPath: dictionary.path,
            requireBaselineHold: true, inputRomaji: "kyou", expectedSurface: "今日")
    }

    private static func assertNativeV5EventSemantics(
        _ event: [String: Any],
        expectedPersonalization: Bool,
        requireBaselineHold: Bool,
        expectedSurface: String
    ) {
        precondition(event["schema"] as? String == "muu.commit-stream.v5")
        let initial=event["initial"] as! [String:Any], final=event["final"] as! [String:Any]
        let generated=initial["generation_segments"] as! [[String:Any]], selected=initial["selected_segments"] as! [[String:Any]]
        let segments=final["segments"] as! [[String:Any]]
        precondition(generated.count==selected.count)
        precondition(segments.count==selected.count)
        precondition(final["surface"] as? String == expectedSurface)
        precondition(segments.map{$0["surface"] as! String}.joined() == expectedSurface)
        for index in segments.indices {
            let generationRange=generated[index]["reading_range"] as! [String:Int]
            precondition(generationRange == selected[index]["reading_range"] as! [String:Int])
            precondition(generationRange == segments[index]["reading_range"] as! [String:Int])
            precondition(selected[index]["candidate_id"] as! String == segments[index]["candidate_id"] as! String)
            for segment in [generated[index],selected[index],segments[index]] {
                precondition(!(segment["candidate_id"] as! String).isEmpty)
                let path=segment["path"] as! [String:Any]
                precondition(path["index"] as? Int == 0)
                precondition(!(path["nodes"] as! [[String:Any]]).isEmpty)
            }
        }
        let report=initial["selection_report"] as! [String:Any]
        precondition(report["rules_version"] as? Int == 3)
        precondition(report["format_version"] as? Int == 3)
        let runs=report["runs"] as! [[String:Any]]
        for run in runs {
            let hypotheses=run["hypotheses"] as! [[String:Any]]
            for (rank,hypothesis) in hypotheses.enumerated() {
                precondition(hypothesis["rank"] as? Int == rank)
                precondition(hypothesis["score"] != nil)
                precondition(hypothesis["learning_bias"] != nil)
                precondition(hypothesis["adjusted_score"] != nil)
                let path = hypothesis["path"] as! [String:Any]
                precondition(path["nodes"] is [[String:Any]])
            }
            let before=run["baseline_segments"] as! [[String:Any]],after=run["applied_segments"] as! [[String:Any]]
            precondition(before.count==after.count)
            precondition(run["switched"] is Bool)
            precondition(run["context_chars"] as? Int ?? 41 <= 40)
            precondition(run["skipped"] != nil)
            precondition(run["excluded_reading_identity_ranks"] is [Int])
            precondition(run["reading_identity_vetoed_rank"] != nil)
            for segment in before+after { precondition(segment["nodes"] == nil);precondition(segment["origin_layers"] == nil) }
        }
        precondition(event["native_scorer"] == nil)
        precondition(event["causal"] == nil)
        precondition(event["late_result"] == nil)
        let timing=event["timing"] as! [String:Any]
        precondition(timing["conversion_to_result_us"] as? Int ?? 0 > 0)
        precondition(timing["conversion_to_commit_us"] as! Int >= timing["conversion_to_result_us"] as! Int)
        let beforeBytes=try! JSONSerialization.data(withJSONObject:generated,options:[.sortedKeys])
        let afterBytes=try! JSONSerialization.data(withJSONObject:selected,options:[.sortedKeys])
        if requireBaselineHold {
            // These fixtures have no LM; generation is the selector's input.
            // Learning changes costs, not the authority recorded at this seam (docs/src/internals/conversion.md「言語モデルによる選び直し」).
            precondition(beforeBytes == afterBytes,"generation baseline is recorded directly")
        }
    }

    private static func assertPersonalFrequencyNativeV5Pair(dictionaryPath: String, testCase: String) {
        let empty = PersonalFrequencySnapshot.empty
        let nonempty = PersonalFrequencySnapshot(
            schemaVersion: PersonalFrequencySnapshot.currentSchemaVersion,
            dictionaryPayloadSHA256: "4f5b9a05d483eb13a5685e2cab5b78dacf9e335248b015a3e103918b0a1724a0",
            dictionaryTokenCount: 2_038_107,
            observationCount: 1,
            entries: [PersonalFrequencyEntry(dictionaryTokenID: 883_303, count: 1)]
        )
        if testCase == "native-frequency-empty" {
            assertProductionGenerationE2E(
                dictionaryPath: dictionaryPath,
                personalFrequency: empty,
                expectedPersonalization: false
            )
            return
        }
        if testCase == "native-frequency-nonempty" {
            assertProductionGenerationE2E(
                dictionaryPath: dictionaryPath,
                personalFrequency: nonempty,
                expectedPersonalization: true
            )
            return
        }
        // Invalid startup bytes degrade once; they never recreate sessions.
        let legacy = PersonalFrequencySnapshot(schemaVersion:1,dictionaryPayloadSHA256:"11067f611988c88c691e51a547678d5df90e2a924f08480f99044c355995b459",dictionaryTokenCount:2_038_033,observationCount:1,entries:[PersonalFrequencyEntry(dictionaryTokenID:883_303,count:1)])
        assertRejectedLearningStartup(dictionaryPath:dictionaryPath,snapshot:legacy)
    }

    // Learning bytes that Core rejects at startup (corrupt, or bound to another
    // dictionary). Core starts with empty learning, the old file is moved aside
    // unchanged rather than deleted, no session is recreated, and the next save writes
    // the empty state, which a fresh store then loads cleanly.
    private static func assertRejectedLearningStartup(dictionaryPath:String,snapshot:PersonalFrequencySnapshot) {
        assertRejectedLearningStartup(dictionaryPath:dictionaryPath,data:try! JSONEncoder().encode(snapshot))
    }

    private static func assertRejectedLearningStartup(dictionaryPath:String,data:Data) {
        let base = URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true).appendingPathComponent("target/swift-learning-fixtures/invalid-startup-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at:base) }
        let url = base.appendingPathComponent("test/personal_frequency/personal-frequency-v1.json")
        try! FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
        let oldData = data
        try! oldData.write(to:url)
        let clock = ManualPersistenceScheduler()
        let store = PersonalFrequencyStore(baseDirectory:base,bundleIdentifier:"test",scheduler:clock.schedule)
        precondition(store.startupData == oldData)
        let result = try! RustCore.initialize(CoreConfiguration(dictionaryPath:dictionaryPath,conversionLmDirectory:nil,commitStream:nil),learning:store.startupData).get()
        precondition(result.degraded.personalFrequencyInvalid)
        store.acceptStartup(result)
        let received = try! RustCore.personalFrequency(after:nil).get()!
        let current = try! JSONSerialization.jsonObject(with:received.0) as! [String:Any]
        precondition(current["observation_count"] as? Int == 0)
        precondition((current["entries"] as? [Any])?.isEmpty == true)
        precondition(store.receivedVersion == result.personalFrequencyVersion)
        precondition(store.savedVersion == nil)
        precondition(!store.persistenceStopped)
        precondition(!FileManager.default.fileExists(atPath:url.path))
        let backups = try! FileManager.default.contentsOfDirectory(at:url.deletingLastPathComponent(),includingPropertiesForKeys:nil)
        precondition(backups.count == 1)
        precondition(try! Data(contentsOf:backups[0]) == oldData)
        var recreates = 0
        RustEngine.recreateObserverForTesting = { _ in recreates += 1 }
        let engine = RustEngine()
        precondition(engine.isAvailable)
        precondition(recreates == 0)
        RustEngine.recreateObserverForTesting = nil
        clock.advance(0.250); try! store.flushForTesting()
        precondition(store.savedVersion == result.personalFrequencyVersion)
        let reloaded = PersonalFrequencyStore(baseDirectory:base,bundleIdentifier:"test")
        precondition(reloaded.loadError == nil)
        let persisted = try! JSONSerialization.jsonObject(with:reloaded.startupData!) as! [String:Any]
        precondition(persisted["observation_count"] as? Int == 0)
        precondition((persisted["entries"] as? [Any])?.isEmpty == true)
    }

    // The production record path for the two inputs that change the generation
    // baseline: applied learning, and a user dictionary entry. Only the first is
    // expected to count as personalization.
    private static func assertGenerationBaselineProductionPair(dictionaryPath: String, testCase: String) {
        let applied = PersonalFrequencySnapshot(
            schemaVersion: PersonalFrequencySnapshot.currentSchemaVersion,
            dictionaryPayloadSHA256: "4f5b9a05d483eb13a5685e2cab5b78dacf9e335248b015a3e103918b0a1724a0",
            dictionaryTokenCount: 2_038_107,
            observationCount: 1,
            entries: [PersonalFrequencyEntry(dictionaryTokenID: 883_303, count: 1)]
        )
        if testCase == "generation-baseline-learning" {
            assertProductionGenerationE2E(
                dictionaryPath: dictionaryPath,
                personalFrequency: applied,
                expectedPersonalization: true,
                requireBaselineHold: true,
                inputRomaji: "honkennnituitehabettokyougiwoshiyou",
                expectedSurface: "本件については別途競技を使用"
            )
            return
        }
        // Fixed self-authored bytes, with the real file checksum. Swift does
        // not render a second copy of the persistence codec for test setup.
        let baselineUserFile = try! Data(contentsOf: URL(fileURLWithPath:
            "test-support/abi13-user-dictionary.tsv"))
        assertProductionGenerationE2E(
            dictionaryPath: dictionaryPath,
            personalFrequency: .empty,
            userDictionaryFile: baselineUserFile,
            expectedPersonalization: false,
            requireBaselineHold: true,
            inputRomaji: "kyougishiyou",
            expectedSurface: "競技しよう"
        )
    }

    // withPreparedPlan for all three stores. With a known ticket, a local
    // failure cancels exactly once with that ticket, and a success cancels
    // nothing. An oversized length (with no storage behind it) is rejected by
    // the bound before any copy, and a reply without a ticket fails closed:
    // neither runs the body or cancels, and each logs only its failure code.
    private static func assertPreparedReplyOwnership() {
        enum LocalFailure: Error { case invalidPayload }
        for (kind, oversizedCode) in [("user", "user_dictionary_output_too_large"),
            ("imported", "catalog_too_large"), ("postal", "invalid_plan")] {
            let failureCode = kind == "user" ? "user_dictionary_preparation_unreadable" :
                kind == "imported" ? "imported_dictionary_preparation_unreadable" : "postal_code_preparation_unreadable"
            let plan = kind == "user"
                ? Data(#"{"ticket":{"owner":1,"operation":2,"expected_revision":3}}"#.utf8)
                : Data(#"{"ticket":{"domain":"fixture","owner":1,"operation":2,"expected_revision":3}}"#.utf8)
            var cancelled: [Data] = [], failures: [String] = []
            var bodyCalls = 0
            RustCore.preparationFailureForTesting = { failures.append($0) }
            defer { RustCore.preparationFailureForTesting = nil }
            // Once the ticket is known, any local payload validation/copy
            // failure cancels exactly once before the reply reaches a store.
            do {
                try RustCore.preparedPlanForTesting(kind: kind, plan: plan, cancel: { cancelled.append($0); return 0 }) {
                    bodyCalls += 1; throw LocalFailure.invalidPayload
                }
                preconditionFailure("local failure must propagate")
            } catch let failure as DictionaryPreparationFailure {
                precondition(failure.cancellationStatus == 0)
                guard case LocalFailure.invalidPayload = failure.original else { preconditionFailure("unexpected local failure") }
            } catch { preconditionFailure("missing local cancellation fact") }
            precondition(bodyCalls == 1 && cancelled.count == 1 && failures.isEmpty)
            // A failed cancel still preserves the original local failure,
            // but exposes its code and ACK without the opaque ticket.
            var cancellationFailures: [(String, Int32)] = []
            RustCore.cancellationFailureForTesting = { cancellationFailures.append(($0, $1)) }
            for status: Int32 in [0, 6, 7, 9, 42] {
                do {
                    try RustCore.preparedPlanForTesting(kind: kind, plan: plan, cancel: { _ in status }) {
                        throw LocalFailure.invalidPayload
                    }
                    preconditionFailure("local failure must remain an error")
                } catch let failure as DictionaryPreparationFailure {
                    precondition(failure.cancellationStatus == status)
                    guard case LocalFailure.invalidPayload = failure.original else { preconditionFailure("original failure was replaced") }
                } catch { preconditionFailure("original failure was replaced") }
            }
            precondition(cancellationFailures.map { $0.0 } == Array(repeating: "dictionary_cancel_failed", count: 4))
            precondition(cancellationFailures.map { $0.1 } == [6, 7, 9, 42])
            RustCore.cancellationFailureForTesting = nil
            let ticket = try! JSONSerialization.jsonObject(with: cancelled[0]) as! [String: Any]
            precondition((ticket["operation"] as? NSNumber)?.intValue == 2)
            cancelled = []; bodyCalls = 0
            try! RustCore.preparedPlanForTesting(kind: kind, plan: plan, cancel: { cancelled.append($0); return 0 }) { bodyCalls += 1 }
            precondition(bodyCalls == 1 && cancelled.isEmpty && failures.isEmpty, "successful reply transfers ownership")
            bodyCalls = 0
            // A forged oversized length with NULL storage proves the bound
            // runs before copy: the error must be the typed size rejection,
            // rather than the copy helper's invalid/empty-buffer error.
            do {
                try RustCore.preparedPlanForTesting(kind: kind, plan: Data(), claimedLength: Int.max,
                    cancel: { cancelled.append($0); return 0 }) { bodyCalls += 1 }
                preconditionFailure("oversized reply accepted")
            } catch {
                let code = (error as? UserDictionaryOperationError)?.code ??
                    (error as? ImportedDictionaryOperationError)?.code ?? (error as? PostalCodeOperationError)?.code
                precondition(code == oversizedCode)
            }
            precondition(bodyCalls == 0 && cancelled.isEmpty && failures == [failureCode])
            failures = []
            // Missing authority is a Core contract bug. Fail closed and log
            // only a code; do not invent a cancel ticket to unlock the slot.
            do {
                try RustCore.preparedPlanForTesting(kind: kind, plan: Data("{}".utf8),
                    cancel: { cancelled.append($0); return 0 }) { bodyCalls += 1 }
                preconditionFailure("missing ticket accepted")
            } catch {}
            precondition(bodyCalls == 0 && cancelled.isEmpty && failures == [failureCode])
        }
        // Exercise the real prepared-plan helper through a Store's prepare
        // closure. A failed native cancellation must reach the Store even
        // though its own ticket variable has not been assigned yet.
        let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent("target/local-prepare-cancel-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        for status: Int32 in [0, 6, 7, 9, 42] {
            var prepares = 0, nativeCancels = 0, storeCancels = 0
            let original = UserDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "user_dictionary_output_too_large")
            var bridge = UserDictionaryCoreBridge.unavailable
            bridge.view = { Data(#"{"schema_version":1,"management_revision":0,"access":"editable","layer_status":"active","entries":[],"entry_count":0,"dormant_count":0}"#.utf8) }
            bridge.prepare = { _ in
                prepares += 1
                try RustCore.preparedPlanForTesting(kind: "user",
                    plan: Data(#"{"ticket":{"owner":1,"operation":2,"expected_revision":0}}"#.utf8),
                    cancel: { _ in nativeCancels += 1; return status }) { throw original }
                preconditionFailure("payload validation must fail")
            }
            bridge.cancel = { _ in storeCancels += 1; return status }
            let store = UserDictionaryStore(baseDirectory: root, bundleIdentifier: "ack-\(status)", bridge: bridge)
            try! store.acceptInitializedCore()
            func edit(checkOriginal: Bool = false) {
                var done = false
                store.upsert(reading: "むう", surface: "Muu", partOfSpeech: .noun, priority: .high, replacing: nil) { result in
                    if checkOriginal {
                        precondition(store.editingStopped == (status != 0), "local cancellation fact reaches the Store before its screen completion")
                        if case .failure(let error) = result {
                            precondition((error as? UserDictionaryOperationError)?.code == original.code, "original payload error reaches the screen")
                        } else { preconditionFailure("local payload validation must fail") }
                    }
                    done = true
                }
                let deadline = Date().addingTimeInterval(180)
                while !done || store.isBusy {
                    precondition(Date() < deadline, "local prepare completion hang guard")
                    RunLoop.current.run(until: Date().addingTimeInterval(0.01))
                }
            }
            edit(checkOriginal: true)
            precondition(store.editingStopped == (status != 0), "local prepare cancellation failure must stop before completion")
            edit()
            precondition(prepares == (status == 0 ? 2 : 1) && nativeCancels == prepares && storeCancels == 0,
                "failed local cancellation prevents the next prepare without double cancellation")
        }
        print("prepared replies: bounded copy, known-ticket cancellation and ticketless fail-closed passed")
    }

    private static func assertPersonalFrequencyPersistenceAndClear() {
        let base = URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true).appendingPathComponent("target/swift-learning-fixtures/persistence-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: base) }
        let v1 = LearningVersion(generation: 0, revision: 1)
        let v2 = LearningVersion(generation: 0, revision: 2)
        let cleared = LearningVersion(generation: 1, revision: 0)
        let bytes1 = Data("opaque-v1-with-future-fields".utf8)
        let bytes2 = Data("opaque-v2".utf8)
        // Learning's managed directories become 0700 whether created or found
        // at 0755, and the caller's base keeps its own mode (0751 here).
        for existed in [false, true] {
            let directory = base.appendingPathComponent("private-directories-\(existed)")
            try! FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            precondition(chmod(directory.path, 0o751) == 0)
            let managed = directory.appendingPathComponent("test")
            let leaf = managed.appendingPathComponent("personal_frequency")
            if existed {
                try! FileManager.default.createDirectory(at: leaf, withIntermediateDirectories: true)
                precondition(chmod(managed.path, 0o755) == 0 && chmod(leaf.path, 0o755) == 0)
            }
            let scheduler = ManualPersistenceScheduler()
            let store = PersonalFrequencyStore(baseDirectory: directory, bundleIdentifier: "test", scheduler: scheduler.schedule)
            store.receive(bytes1, version: v1)
            scheduler.advance(1); try! store.flushForTesting()
            precondition(store.savedVersion == v1)
            for path in [managed, leaf] {
                var info = stat()
                precondition(lstat(path.path, &info) == 0 && info.st_mode & 0o777 == 0o700, "all learning directories private")
            }
            var info = stat()
            precondition(lstat(directory.path, &info) == 0 && info.st_mode & 0o777 == 0o751, "caller base permissions unchanged")
        }
        func file(_ directory: String) -> URL {
            base.appendingPathComponent(directory).appendingPathComponent("test/personal_frequency/personal-frequency-v1.json")
        }
        func disk(_ directory: String) -> Data? { try? Data(contentsOf: file(directory)) }
        func seed(_ directory: String, _ data: Data) {
            try! FileManager.default.createDirectory(at:file(directory).deletingLastPathComponent(),withIntermediateDirectories:true)
            try! data.write(to:file(directory))
        }
        // A missing dictionary cannot establish the saved file's authority.
        // Preserve the bytes and both baselines; even an elapsed save deadline
        // or a query must not quarantine or overwrite the original file.
        seed("unverifiable", bytes1)
        let unverifiableClock = ManualPersistenceScheduler()
        var unverifiableWrites = 0
        var unverifiableQuarantines = 0
        let unverifiable = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("unverifiable"),bundleIdentifier:"test",scheduler:unverifiableClock.schedule,persistenceOverride:{_,_ in
            unverifiableWrites += 1
            return nil
        },quarantineOverride:{_ in
            unverifiableQuarantines += 1
        },coreClear:{.success(cleared)})
        let unverifiableResult = try! JSONDecoder().decode(CoreInitializationResult.self,from:Data(#"{"status":"degraded","degraded":{"dictionary_kana_only":false,"personal_frequency_invalid":false,"personal_frequency_unverifiable":true,"user_dictionary_invalid":false,"language_model_unavailable":true},"personal_frequency_version":{"generation":0,"revision":0}}"#.utf8))
        unverifiable.acceptStartup(unverifiableResult)
        precondition(unverifiable.persistenceStopped)
        precondition(unverifiable.receivedVersion == nil && unverifiable.savedVersion == nil)
        unverifiable.poll()
        unverifiable.receive(bytes2,version:v1)
        unverifiableClock.advance(60); try! unverifiable.flushForTesting()
        precondition(disk("unverifiable") == bytes1)
        precondition(unverifiableQuarantines == 0 && unverifiableWrites == 0)
        precondition(try! FileManager.default.contentsOfDirectory(atPath:file("unverifiable").deletingLastPathComponent().path).count == 1)
        guard case .cleared = unverifiable.clear() else { preconditionFailure("explicit clear resumes persistence") }
        precondition(!unverifiable.persistenceStopped)
        precondition(disk("unverifiable") == nil)

        // All default IO paths reject an ancestor link, not only a leaf link.
        // The destination contains only synthetic opaque bytes. Nothing may
        // read, write, quarantine, or clear through the linked directory.
        let actual = base.appendingPathComponent("symlink-actual", isDirectory: true)
        let linked = base.appendingPathComponent("symlink-parent", isDirectory: true)
        try! FileManager.default.createDirectory(at: actual, withIntermediateDirectories: true)
        let actualFile = actual.appendingPathComponent("test/personal_frequency/personal-frequency-v1.json")
        try! FileManager.default.createDirectory(at: actualFile.deletingLastPathComponent(), withIntermediateDirectories: true)
        try! bytes1.write(to: actualFile)
        try! FileManager.default.createSymbolicLink(atPath: linked.path, withDestinationPath: actual.path)
        let unsafeClock = ManualPersistenceScheduler()
        var unsafeClearCalls = 0
        let unsafe = PersonalFrequencyStore(baseDirectory: linked, bundleIdentifier: "test", scheduler: unsafeClock.schedule,
            coreClear: { unsafeClearCalls += 1; return .success(cleared) })
        precondition(unsafe.startupData == Data() && unsafe.loadError != nil, "unsafe is not missing")
        unsafe.receive(bytes2, version: v1)
        unsafeClock.advance(1); try! unsafe.flushForTesting()
        precondition(unsafe.savedVersion == nil && unsafe.durabilityWarning != nil)
        precondition(try! Data(contentsOf: actualFile) == bytes1)
        switch unsafe.clear() {
        case .cleared, .clearedWithDurabilityWarning: preconditionFailure("clear must reject the ancestor link")
        case .precommitFailure: break
        case .committedWithRuntimeWarning: preconditionFailure("Core.clear must not run after disk rejection")
        }
        precondition(unsafeClearCalls == 0, "disk rejection must precede Core.clear")
        let rejected = try! JSONDecoder().decode(CoreInitializationResult.self, from: Data(#"{"status":"degraded","degraded":{"dictionary_kana_only":false,"personal_frequency_invalid":true,"personal_frequency_unverifiable":false,"user_dictionary_invalid":false,"language_model_unavailable":true},"personal_frequency_version":{"generation":0,"revision":0}}"#.utf8))
        unsafe.acceptStartup(rejected)
        precondition(unsafe.persistenceStopped && unsafe.loadError != nil)
        precondition(try! Data(contentsOf: actualFile) == bytes1)
        precondition(try! FileManager.default.contentsOfDirectory(atPath: actualFile.deletingLastPathComponent().path) == [actualFile.lastPathComponent])

        let clock = ManualPersistenceScheduler()
        try! FileManager.default.createDirectory(at: base.appendingPathComponent("basic"), withIntermediateDirectories: true)
        let store = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("basic"),bundleIdentifier:"test",scheduler:clock.schedule,coreClear:{.success(cleared)})
        precondition(store.startupData == nil)
        precondition(store.receivedVersion == nil && store.savedVersion == nil)
        store.receive(bytes1,version:v1)
        precondition(store.receivedVersion == v1)
        precondition(store.savedVersion == nil)
        precondition(store.persistenceStatus.pending && store.persistenceStatus.saveIssue == nil)
        precondition(clock.count == 1)
        for _ in 0..<48 { store.receive(bytes1,version:v1) }
        precondition(clock.count == 1,"same version cannot postpone the save")
        clock.advance(0.249); try! store.flushForTesting()
        precondition(disk("basic") == nil)
        clock.advance(0.001); try! store.flushForTesting()
        precondition(disk("basic") == bytes1)
        precondition(store.savedVersion == v1)
        precondition(store.persistenceStatus.lastSavedAt != nil && store.persistenceStatus.saveIssue == nil)
        precondition(store.receivedVersion == v1)
        precondition(clock.count == 0)
        let restart = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("basic"),bundleIdentifier:"test")
        precondition(restart.startupData == bytes1,"the store does not interpret the learning bytes")
        precondition(restart.loadError == nil)
        guard case .cleared = store.clear() else { preconditionFailure("clear success") }
        precondition(MuuInputController.personalFrequencyClearNotice(for:.cleared) == nil)
        precondition(store.receivedVersion == cleared && store.savedVersion == cleared)
        precondition(disk("basic") == nil)
        precondition(!store.persistenceStopped)
        precondition(store.persistenceStatus.lastSavedAt == nil && store.persistenceStatus.fileState == .cleared)
        let emptyRestart = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("basic"),bundleIdentifier:"test")
        precondition(emptyRestart.startupData == nil)
        precondition(emptyRestart.loadError == nil)
        store.receive(bytes2,version:v2)
        clock.advance(1); try! store.flushForTesting()
        precondition(disk("basic") == nil,"a stale generation cannot restore cleared bytes")

        // New versions restart the 250ms window; equal versions do not.
        // Only the latest bytes are written when the final deadline arrives.
        let debounceClock = ManualPersistenceScheduler()
        var debounceWrites = 0
        let debounce = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("debounce"),bundleIdentifier:"test",scheduler:debounceClock.schedule,persistenceOverride:{data,url in
            debounceWrites += 1
            try FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
            try data.write(to:url)
            return nil
        })
        debounce.receive(bytes1,version:v1)
        debounceClock.advance(0.200); try! debounce.flushForTesting()
        precondition(debounceWrites == 0)
        debounce.receive(bytes2,version:v2)
        debounceClock.advance(0.050); try! debounce.flushForTesting()
        precondition(debounceWrites == 0,"newer version moves the deadline past the old reservation")
        debounce.receive(bytes2,version:v2)
        debounceClock.advance(0.199); try! debounce.flushForTesting()
        precondition(debounceWrites == 0)
        debounceClock.advance(0.001); try! debounce.flushForTesting()
        precondition(debounceWrites == 1,"equal versions did not extend the new deadline")
        precondition(disk("debounce") == bytes2)
        precondition(debounce.savedVersion == v2 && debounce.receivedVersion == v2)

        let retryClock = ManualPersistenceScheduler()
        var attempts = 0
        let retry = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("retry"),bundleIdentifier:"test",scheduler:retryClock.schedule,persistenceOverride:{data,url in
            attempts += 1
            if attempts == 1 { throw PersonalFrequencyFixtureError.persist }
            try FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
            try data.write(to:url)
            return nil
        })
        retry.receive(bytes1,version:v1)
        retryClock.advance(0.250); try! retry.flushForTesting()
        precondition(attempts == 1)
        precondition(retry.savedVersion == nil)
        precondition(retry.receivedVersion == v1)
        precondition(retry.durabilityWarning != nil)
        precondition(retry.persistenceStatus.saveIssue == .writeFailed && retry.persistenceStatus.lastSavedAt == nil)
        precondition(disk("retry") == nil)
        precondition(retryClock.count == 1)
        retry.receive(bytes1,version:v1)
        precondition(retryClock.count == 1)
        retryClock.advance(0.500); try! retry.flushForTesting()
        precondition(attempts == 2)
        precondition(retry.savedVersion == v1 && retry.receivedVersion == v1)
        precondition(disk("retry") == bytes1)
        precondition(retry.durabilityWarning == nil)
        precondition(retry.persistenceStatus.saveIssue == nil && retry.persistenceStatus.lastSavedAt != nil)

        // Failed writes keep their bytes, but retry at a bounded exponential
        // interval. A newer version resets it without extending equal versions.
        let backoffClock = ManualPersistenceScheduler()
        var backoffAttempts = 0
        var failBackoff = true
        let backoff = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("backoff"),bundleIdentifier:"test",scheduler:backoffClock.schedule,persistenceOverride:{data,url in
            backoffAttempts += 1
            if failBackoff { throw PersonalFrequencyFixtureError.persist }
            try FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
            try data.write(to:url)
            return nil
        })
        backoff.receive(bytes1,version:v1)
        for delay in [0.250, 0.500, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0] {
            precondition(backoffClock.scheduledDelays.last == delay)
            let before = backoffAttempts
            backoffClock.advance(delay - 0.001); try! backoff.flushForTesting()
            precondition(backoffAttempts == before)
            backoffClock.advance(0.001); try! backoff.flushForTesting()
            precondition(backoffAttempts == before + 1)
            precondition(backoff.savedVersion == nil && backoff.receivedVersion == v1)
            precondition(disk("backoff") == nil)
        }
        backoff.receive(bytes2,version:v2)
        precondition(backoffClock.scheduledDelays.last == 0.250)
        let reservations = backoffClock.scheduledDelays.count
        backoff.receive(bytes2,version:v2)
        precondition(backoffClock.scheduledDelays.count == reservations)
        failBackoff = false
        backoffClock.advance(0.250); try! backoff.flushForTesting()
        precondition(backoff.savedVersion == v2 && disk("backoff") == bytes2)
        let completedAttempts = backoffAttempts
        backoffClock.advance(60); try! backoff.flushForTesting()
        precondition(backoffAttempts == completedAttempts, "successful save stops retries")

        let overlapClock = ManualPersistenceScheduler()
        let started = DispatchSemaphore(value:0), release = DispatchSemaphore(value:0)
        var overlapWrites = 0
        let overlap = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("overlap"),bundleIdentifier:"test",scheduler:overlapClock.schedule,persistenceOverride:{data,url in
            overlapWrites += 1
            if overlapWrites == 1 { started.signal(); precondition(release.wait(timeout:.now()+5) == .success) }
            try FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
            try data.write(to:url)
            return nil
        })
        overlap.receive(bytes1,version:v1)
        overlapClock.advance(0.250)
        precondition(started.wait(timeout:.now()+5) == .success)
        overlap.receive(bytes2,version:v2)
        precondition(overlap.receivedVersion == v2)
        precondition(overlap.savedVersion == nil)
        release.signal(); try! overlap.flushForTesting()
        precondition(overlap.savedVersion == v1,"v1 completion cannot claim that v2 was saved")
        precondition(overlap.receivedVersion == v2)
        precondition(disk("overlap") == bytes1)
        overlapClock.advance(0.250); try! overlap.flushForTesting()
        precondition(overlap.savedVersion == v2)
        precondition(disk("overlap") == bytes2)
        precondition(overlapWrites == 2)

        // v1 can fail after v2 has reserved its deadline. Retrying v1 must
        // neither postpone v2 nor put older bytes back after v2 succeeds.
        let failedOverlapClock = ManualPersistenceScheduler()
        let failedStarted = DispatchSemaphore(value:0), failedRelease = DispatchSemaphore(value:0)
        var failedOverlapWrites = 0
        var failedOverlapBytes: [Data] = []
        let failedOverlap = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("failed-overlap"),bundleIdentifier:"test",scheduler:failedOverlapClock.schedule,persistenceOverride:{data,url in
            failedOverlapWrites += 1
            failedOverlapBytes.append(data)
            if failedOverlapWrites == 1 {
                failedStarted.signal()
                precondition(failedRelease.wait(timeout:.now()+5) == .success)
                throw PersonalFrequencyFixtureError.persist
            }
            try FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
            try data.write(to:url)
            return nil
        })
        failedOverlap.receive(bytes1,version:v1)
        failedOverlapClock.advance(0.250)
        precondition(failedStarted.wait(timeout:.now()+5) == .success)
        failedOverlap.receive(bytes2,version:v2)
        // Advance while v1 is blocked: its completion must not restart the
        // clock, even with part of v2's 250ms already elapsed.
        failedOverlapClock.advance(0.100)
        failedRelease.signal(); try! failedOverlap.flushForTesting()
        precondition(failedOverlap.receivedVersion == v2 && failedOverlap.savedVersion == nil)
        precondition(failedOverlap.durabilityWarning != nil)
        precondition(disk("failed-overlap") == nil)
        precondition(failedOverlapClock.scheduledDelays == [0.250, 0.250])
        precondition(failedOverlapClock.count == 1)
        failedOverlapClock.advance(0.149); try! failedOverlap.flushForTesting()
        precondition(failedOverlapWrites == 1, "v2 must wait until its original deadline")
        failedOverlapClock.advance(0.001); try! failedOverlap.flushForTesting()
        precondition(failedOverlapWrites == 2, "v1 failure must not extend v2's 250ms deadline")
        precondition(failedOverlap.savedVersion == v2 && failedOverlap.receivedVersion == v2)
        precondition(failedOverlapBytes == [bytes1, bytes2])
        precondition(disk("failed-overlap") == bytes2)
        precondition(failedOverlap.durabilityWarning == nil)
        precondition(failedOverlapClock.count == 0)
        failedOverlapClock.advance(60); try! failedOverlap.flushForTesting()
        precondition(failedOverlapWrites == 2, "an old retry cannot write after v2 succeeds")
        precondition(disk("failed-overlap") == bytes2 && failedOverlap.savedVersion == v2)

        // Retrying cannot repair a path/type or payload contract violation.
        // Keep known bytes and cursors, but do not arm another timer.
        for error in [PersonalFrequencyStoreError.unsupportedFileType, .fileTooLarge, .io(operation: "open-directory", code: ELOOP)] {
            let scheduler = ManualPersistenceScheduler()
            var attempts = 0
            let store = PersonalFrequencyStore(baseDirectory: base.appendingPathComponent("permanent-\(UUID().uuidString)"),
                bundleIdentifier: "test", scheduler: scheduler.schedule,
                persistenceOverride: { _, _ in attempts += 1; throw error })
            store.receive(bytes1, version: v1)
            scheduler.advance(1); try! store.flushForTesting()
            precondition(store.persistenceStopped, "permanent learning save failure must stop instead of retrying")
            precondition(attempts == 1 && scheduler.count == 0 && store.persistenceStatus.pending,
                "permanent failure retains pending without another timer")
            precondition(store.receivedVersion == v1 && store.savedVersion == nil, "failed save cannot advance the saved cursor")
        }

        // The other side of the boundary: EACCES and EIO, even from a directory
        // sync, keep the ordinary retry and save the same bytes next time.
        for code in [EACCES, EIO] {
            let scheduler = ManualPersistenceScheduler()
            var attempts = 0, savedBytes: Data?
            let store = PersonalFrequencyStore(baseDirectory: base, bundleIdentifier: "typed-io-\(code)", scheduler: scheduler.schedule,
                persistenceOverride: { data, _ in
                    attempts += 1
                    if attempts == 1 { throw PersonalFrequencyStoreError.io(operation: "directory_fsync", code: code) }
                    savedBytes = data; return nil
                })
            store.receive(bytes1, version: v1); scheduler.advance(0.25); try! store.flushForTesting()
            precondition(!store.persistenceStopped && scheduler.count == 1 && store.pendingDataForTesting == bytes1)
            scheduler.advance(1); try! store.flushForTesting()
            precondition(attempts == 2 && savedBytes == bytes1 && store.savedVersion == v1 && !store.persistenceStatus.pending)
        }
        // A newer version arriving while the failing write runs: the stop keeps
        // those newer bytes, its armed timer becomes a no-op, and only a
        // successful clear resumes saving.
        do {
            let scheduler = ManualPersistenceScheduler()
            let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
            var attempts = 0, queries = 0, permanentlyFail = true
            let store = PersonalFrequencyStore(baseDirectory: base, bundleIdentifier: "permanent-overlap", scheduler: scheduler.schedule,
                persistenceOverride: { _, _ in
                    attempts += 1
                    if permanentlyFail {
                        entered.signal(); precondition(release.wait(timeout: .now() + 180) == .success)
                        throw PersonalFrequencyStoreError.unsupportedFileType
                    }
                    return nil
                }, coreClear: { .success(cleared) }, query: { _ in queries += 1; return .success(nil) })
            store.receive(bytes1, version: v1); scheduler.advance(0.25)
            precondition(entered.wait(timeout: .now() + 180) == .success)
            store.receive(bytes2, version: v2); release.signal(); try! store.flushForTesting()
            precondition(store.persistenceStopped && store.pendingDataForTesting == bytes2 && store.receivedVersion == v2 && store.savedVersion == nil,
                "permanent failure retains the newer pending bytes and old saved cursor")
            scheduler.advance(10); try! store.flushForTesting()
            precondition(attempts == 1, "already scheduled newer callback becomes a no-op after permanent failure")
            // Clear is the existing way to reset persistence authority.
            guard case .cleared = store.clear() else { preconditionFailure("clear stopped learning") }
            precondition(!store.persistenceStopped && store.pendingDataForTesting == nil && store.savedVersion == cleared)
            permanentlyFail = false
            store.receive(bytes1, version: LearningVersion(generation: 1, revision: 1))
            scheduler.advance(1); try! store.flushForTesting()
            precondition(attempts == 2 && store.savedVersion == LearningVersion(generation: 1, revision: 1))
            precondition(queries == 0)
        }
        do {
            let scheduler = ManualPersistenceScheduler()
            var attempts = 0, queries = 0
            // Shutdown after a stop: no Core query, no retry, and failed
            // because known bytes are unsaved.
            let store = PersonalFrequencyStore(baseDirectory: base, bundleIdentifier: "permanent-shutdown", scheduler: scheduler.schedule,
                persistenceOverride: { _, _ in attempts += 1; throw PersonalFrequencyStoreError.fileTooLarge },
                query: { _ in queries += 1; return .success(nil) })
            store.acceptStartup(CoreInitializationResult(degraded: .init(personalFrequencyInvalid: false,
                personalFrequencyUnverifiable: false, userDictionaryInvalid: false), personalFrequencyVersion: .init(generation: 0, revision: 0)))
            store.receive(bytes1, version: v1); scheduler.advance(1); try! store.flushForTesting()
            let done = DispatchSemaphore(value: 0)
            store.beginShutdown(deadline: 2, now: { 0 }) { result in precondition(result == .failed); done.signal() }
            precondition(done.wait(timeout: .now() + 180) == .success && attempts == 1 && queries == 0,
                "shutdown reports known unsaved data without a query or unsafe retry")
        }
        do {
            let scheduler = ManualPersistenceScheduler()
            // The real writer refuses over-limit bytes before touching disk.
            let name = "oversized-save"
            let directory = base.appendingPathComponent(name)
            try! FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let store = PersonalFrequencyStore(baseDirectory: directory, bundleIdentifier: "test", scheduler: scheduler.schedule)
            let oversized = Data(count: 512 * 1024 + 1)
            store.receive(oversized, version: v1); scheduler.advance(1); try! store.flushForTesting()
            precondition(store.persistenceStatus.stopReason == .invalidSave && store.pendingDataForTesting == oversized)
            precondition(store.savedVersion == nil && !FileManager.default.fileExists(atPath: file(name).path), "invalid-size bytes never reach disk")
        }
        do {
            // The directory is swapped for a link after the rename. The bytes
            // already landed in the real directory; the NOFOLLOW sync fails,
            // which is a durability warning, not a stop, and nothing is
            // written through the link.
            let name = "nofollow-directory"
            seed(name, bytes1)
            let parent = file(name).deletingLastPathComponent()
            let moved = parent.deletingLastPathComponent().appendingPathComponent("saved-directory")
            let outside = base.appendingPathComponent("nofollow-outside")
            try! FileManager.default.createDirectory(at: outside, withIntermediateDirectories: true)
            let store = PersonalFrequencyStore(baseDirectory: base.appendingPathComponent(name), bundleIdentifier: "test", scheduler: { _, work in work() })
            PersonalFrequencyStore.persistenceCheckpointForTesting = { stage in
                if stage == "fsync_directory" {
                    try FileManager.default.moveItem(at: parent, to: moved)
                    try FileManager.default.createSymbolicLink(at: parent, withDestinationURL: outside)
                }
            }
            store.receive(bytes2, version: v2); try! store.flushForTesting()
            PersonalFrequencyStore.persistenceCheckpointForTesting = nil
            precondition(store.savedVersion == v2 && !store.persistenceStopped && store.persistenceStatus.saveIssue == .durabilityWarning)
            guard case .io(let operation, let code) = store.durabilityWarning as? PersonalFrequencyStoreError else { preconditionFailure("NOFOLLOW directory open must fail after the path is replaced") }
            precondition(operation == "open-directory" && [ELOOP, ENOTDIR].contains(code))
            precondition((try! Data(contentsOf: moved.appendingPathComponent(file(name).lastPathComponent))) == bytes2)
            precondition((try! FileManager.default.contentsOfDirectory(atPath: outside.path)).isEmpty, "no file written through the replacement link")
        }

        // A real C-ABI session event queued on main cannot run between
        // the persisted-file deletion and the synchronous core clear.
        var queuedSession: OpaquePointer?
        var queuedSessionError = MuuBuffer(data:nil,len:0)
        precondition(muu_session_create(&queuedSession,&queuedSessionError) == MUU_RESULT_OK)
        muu_buffer_free(queuedSessionError)
        precondition(queuedSession != nil)
        defer { muu_session_destroy(queuedSession) }
        var queuedSessionEventCompleted = false
        let raceClock = ManualPersistenceScheduler()
        let raceStarted = DispatchSemaphore(value:0), raceRelease = DispatchSemaphore(value:0)
        var order: [String] = []
        var clearCalls = 0
        let race = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("race"),bundleIdentifier:"test",scheduler:raceClock.schedule,persistenceOverride:{data,url in
            raceStarted.signal(); precondition(raceRelease.wait(timeout:.now()+5) == .success)
            try FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
            try data.write(to:url)
            order.append("write")
            return nil
        },deletionOverride:{url in
            precondition(try! Data(contentsOf:url) == bytes1)
            try FileManager.default.removeItem(at:url)
            order.append("delete")
            DispatchQueue.main.async {
                let bytes = try! MuuEnvelope.encode(CommandRequest(command:"cancel"))
                var output = MuuBuffer(data:nil,len:0)
                let status = bytes.withUnsafeBytes { buffer in
                    muu_session_handle_event(queuedSession,buffer.bindMemory(to:UInt8.self).baseAddress,buffer.count,&output)
                }
                muu_buffer_free(output)
                precondition(status == MUU_RESULT_OK)
                queuedSessionEventCompleted = true
            }
        },coreClear:{
            precondition(Thread.isMainThread)
            precondition(!queuedSessionEventCompleted,"main cannot yield between file deletion and core clear")
            precondition(order == ["write","delete"],"the serial queue finishes old writes before deletion")
            precondition(disk("race") == nil)
            clearCalls += 1; order.append("core")
            return .success(cleared)
        })
        race.receive(bytes1,version:v1); raceClock.advance(0.250)
        precondition(raceStarted.wait(timeout:.now()+5) == .success)
        PersonalFrequencyStore.clearWillWaitForTesting = { raceRelease.signal() }
        guard case .cleared = race.clear() else { preconditionFailure("serialized clear") }
        PersonalFrequencyStore.clearWillWaitForTesting = nil
        precondition(clearCalls == 1)
        precondition(order == ["write","delete","core"])
        precondition(!queuedSessionEventCompleted)
        // A zero-timeout poll need not service the main dispatch source.
        // Wait for completion; the deadline is only a hang guard.
        let queuedSessionEventDeadline = DispatchTime.now() + .seconds(5)
        while !queuedSessionEventCompleted && DispatchTime.now() < queuedSessionEventDeadline {
            CFRunLoopRunInMode(CFRunLoopMode.defaultMode, 0.005, false)
        }
        precondition(queuedSessionEventCompleted,"the reserved session event runs after clear returns")
        precondition(race.receivedVersion == cleared && race.savedVersion == cleared)
        precondition(race.persistenceStatus.lastSavedAt == nil && race.persistenceStatus.fileState == .cleared,
            "old-epoch completion must not restore a cleared save timestamp")
        raceClock.advance(1); try! race.flushForTesting()
        precondition(disk("race") == nil)
        precondition(PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("race"),bundleIdentifier:"test").startupData == nil)

        // "Delete all learning" includes rejected startup backups, without
        // following similarly named symlinks or deleting unrelated files.
        for current in [false, true] {
            let name = "clear-rejected-\(current)"
            let parent = file(name).deletingLastPathComponent()
            try! FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
            if current { try! bytes1.write(to: file(name)) }
            let backups = ["personal-frequency-rejected-one.json", "personal-frequency-rejected-two.json"]
                .map { parent.appendingPathComponent($0) }
            for url in backups { try! Data("self-written rejected bytes".utf8).write(to: url) }
            let unrelated = parent.appendingPathComponent("other.json")
            try! bytes2.write(to: unrelated)
            let link = parent.appendingPathComponent("personal-frequency-rejected-link.json")
            try! FileManager.default.createSymbolicLink(at: link, withDestinationURL: unrelated)
            let directory = parent.appendingPathComponent("personal-frequency-rejected-directory.json")
            try! FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
            var calls = 0
            let store = PersonalFrequencyStore(baseDirectory: base.appendingPathComponent(name), bundleIdentifier: "test", coreClear: { calls += 1; return .success(cleared) })
            guard case .cleared = store.clear() else { preconditionFailure("clear rejected backups") }
            precondition(calls == 1 && !FileManager.default.fileExists(atPath: file(name).path))
            precondition(backups.allSatisfy { !FileManager.default.fileExists(atPath: $0.path) }, "clear must also delete rejected startup backups")
            precondition((try! Data(contentsOf: unrelated)) == bytes2 && (try! FileManager.default.destinationOfSymbolicLink(atPath: link.path)) == unrelated.path)
            precondition(FileManager.default.fileExists(atPath: directory.path), "unrelated and non-regular children survive clear")
            let child = Process()
            child.executableURL = Bundle.main.executableURL!
            child.arguments = [base.appendingPathComponent(name).path, "cleared-learning-probe"]
            var environment = ProcessInfo.processInfo.environment
            environment.removeValue(forKey: "MUU_INPUT_PATH_TRACE")
            child.environment = environment
            child.standardOutput = FileHandle.nullDevice
            try! child.run(); child.waitUntilExit()
            precondition(child.terminationStatus == 0, "next-process learning startup after clear")
        }

        // Test checkpoints observe branch/order, not fsync syscalls. A
        // failure while removing backups keeps current bytes and Core intact.
        for fault in ["enumerate_clear_directory", "first_rejected", "second_rejected", "unlink_current", "fsync_clear_directory", "core_and_fsync"] {
            let name = "clear-fault-\(fault)"
            seed(name, bytes1)
            let parent = file(name).deletingLastPathComponent()
            for suffix in ["a", "b"] { try! bytes1.write(to: parent.appendingPathComponent("personal-frequency-rejected-\(suffix).json")) }
            var calls = 0, unlinkCount = 0, syncCount = 0
            let scheduler = ManualPersistenceScheduler()
            let store = PersonalFrequencyStore(baseDirectory: base.appendingPathComponent(name), bundleIdentifier: "test",
                scheduler: scheduler.schedule, coreClear: {
                    calls += 1
                    return fault == "core_and_fsync" ? .failure(PersonalFrequencyFixtureError.replace) : .success(cleared)
                })
            store.receive(bytes2, version: v2)
            PersonalFrequencyStore.deletionCheckpointForTesting = { stage in
                if stage == "unlink_rejected" {
                    unlinkCount += 1
                    if (fault == "first_rejected" && unlinkCount == 1) || (fault == "second_rejected" && unlinkCount == 2) { throw PersonalFrequencyFixtureError.persist }
                }
                if stage == "fsync_clear_directory" { syncCount += 1 }
                if stage == fault || (fault == "core_and_fsync" && stage == "fsync_clear_directory") { throw PersonalFrequencyFixtureError.persist }
            }
            let result = store.clear()
            PersonalFrequencyStore.deletionCheckpointForTesting = nil
            if ["fsync_clear_directory", "core_and_fsync"].contains(fault) {
                precondition(calls == 1 && disk(name) == nil && !store.persistenceStatus.pending)
                precondition(store.persistenceStatus.fileState == .cleared && store.persistenceStatus.lastSavedAt == nil)
                precondition(store.persistenceStatus.saveIssue == .clearDurabilityWarning, "post-unlink fsync warning stays a committed clear")
                if fault == "core_and_fsync" {
                    guard case .committedWithRuntimeWarning = result else { preconditionFailure("runtime rejection after clear") }
                    precondition(store.persistenceStopped && store.persistenceStatus.stopReason == .clearRejected)
                } else {
                    guard case .clearedWithDurabilityWarning = result else { preconditionFailure("clear with durability warning") }
                    precondition(!store.persistenceStopped && store.savedVersion == cleared)
                    let notice = MuuInputController.personalFrequencyClearNotice(for: result)
                    precondition(notice?.kind == .clearedWithDurabilityWarning && notice!.informativeText.contains("耐久性"))
                }
                scheduler.advance(1); try! store.flushForTesting()
                precondition(disk(name) == nil, "old callbacks cannot resurrect cleared bytes")
                if fault != "core_and_fsync" {
                    store.receive(bytes2, version: LearningVersion(generation: 1, revision: 1))
                    scheduler.advance(1); try! store.flushForTesting()
                    precondition(disk(name) == bytes2 && store.persistenceStatus.saveIssue == nil, "new generation can save normally")
                }
            } else {
                guard case .precommitFailure = result else { preconditionFailure("precommit clear failure") }
                precondition(calls == 0 && disk(name) == bytes1 && store.persistenceStatus.pending && !store.persistenceStopped)
                precondition(syncCount == (["second_rejected", "unlink_current"].contains(fault) ? 1 : 0), "partial unlink is synced before failure returns")
            }
        }

        seed("disk-failure",bytes1)
        var rejectedClearCalls = 0
        let diskFailure = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("disk-failure"),bundleIdentifier:"test",deletionOverride:{_ in throw PersonalFrequencyFixtureError.persist},coreClear:{ rejectedClearCalls += 1; return .success(cleared) })
        let failure = diskFailure.clear()
        guard case .precommitFailure = failure else { preconditionFailure("disk deletion failure") }
        precondition(rejectedClearCalls == 0)
        precondition(disk("disk-failure") == bytes1)
        precondition(!diskFailure.persistenceStopped)
        precondition(diskFailure.persistenceStatus.lastSavedAt != nil, "failed unlink preserves the old file timestamp")
        let failureNotice = MuuInputController.personalFrequencyClearNotice(for:failure)
        precondition(failureNotice?.kind == .precommitFailure)
        precondition(failureNotice?.messageText == "変換学習データを削除できませんでした")
        precondition(failureNotice?.informativeText == "現在の学習状態は変更されていません。")

        seed("core-failure",bytes1)
        let stopClock = ManualPersistenceScheduler()
        var stoppedWrites = 0
        let coreFailure = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("core-failure"),bundleIdentifier:"test",scheduler:stopClock.schedule,persistenceOverride:{_,_ in stoppedWrites += 1; return nil},coreClear:{.failure(PersonalFrequencyFixtureError.replace)})
        coreFailure.receive(bytes2,version:v2)
        let warning = coreFailure.clear()
        guard case .committedWithRuntimeWarning = warning else { preconditionFailure("core rejection after disk deletion") }
        precondition(coreFailure.persistenceStopped)
        precondition(coreFailure.persistenceStatus.stopReason == .clearRejected && coreFailure.persistenceStatus.lastSavedAt == nil)
        precondition(disk("core-failure") == nil)
        stopClock.advance(1); try! coreFailure.flushForTesting()
        precondition(stoppedWrites == 0)
        coreFailure.receive(bytes1,version:LearningVersion(generation:2,revision:1))
        stopClock.advance(1); try! coreFailure.flushForTesting()
        precondition(stoppedWrites == 0)
        precondition(disk("core-failure") == nil)
        let warningNotice = MuuInputController.personalFrequencyClearNotice(for:warning)
        precondition(warningNotice?.kind == .committedWithRuntimeWarning)
        precondition(warningNotice?.messageText == "変換学習データを削除しました")
        precondition(warningNotice?.informativeText == "保存済みの学習データは削除しましたが、動作中の変換には反映できませんでした。入力メソッドを再起動すると、空の学習から始まります。")

        seed("old-retry",bytes1)
        let oldRetryClock = ManualPersistenceScheduler()
        var oldAttempts = 0
        let oldRetry = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("old-retry"),bundleIdentifier:"test",scheduler:oldRetryClock.schedule,persistenceOverride:{_,_ in oldAttempts += 1; throw PersonalFrequencyFixtureError.persist},coreClear:{.success(cleared)})
        oldRetry.receive(bytes2,version:v2)
        oldRetryClock.advance(0.250); try! oldRetry.flushForTesting()
        precondition(oldAttempts == 1 && oldRetryClock.count == 1)
        guard case .cleared = oldRetry.clear() else { preconditionFailure("clear old retry") }
        oldRetryClock.advance(1); try! oldRetry.flushForTesting()
        precondition(oldAttempts == 1)
        precondition(disk("old-retry") == nil)
        precondition(oldRetry.savedVersion == cleared)

        seed("quarantine-failure",bytes1)
        let quarantineClock = ManualPersistenceScheduler()
        var quarantineWrites = 0
        let quarantine = PersonalFrequencyStore(baseDirectory:base.appendingPathComponent("quarantine-failure"),bundleIdentifier:"test",scheduler:quarantineClock.schedule,persistenceOverride:{data,url in quarantineWrites += 1; try data.write(to:url); return nil},quarantineOverride:{_ in throw PersonalFrequencyFixtureError.persist},coreClear:{.success(cleared)})
        let rejectedStartup = try! JSONDecoder().decode(CoreInitializationResult.self,from:Data(#"{"status":"degraded","degraded":{"dictionary_kana_only":true,"personal_frequency_invalid":true,"personal_frequency_unverifiable":false,"user_dictionary_invalid":false,"language_model_unavailable":true},"personal_frequency_version":{"generation":0,"revision":0}}"#.utf8))
        quarantine.acceptStartup(rejectedStartup)
        precondition(quarantine.persistenceStopped)
        precondition(quarantine.persistenceStatus.stopReason == .quarantineFailed)
        precondition(quarantine.receivedVersion == nil)
        precondition(quarantine.loadError != nil)
        quarantine.receive(bytes2,version:v2); quarantineClock.advance(1); try! quarantine.flushForTesting()
        precondition(quarantineWrites == 0)
        precondition(disk("quarantine-failure") == bytes1)
        guard case .cleared = quarantine.clear() else { preconditionFailure("clear resumes quarantined persistence") }
        precondition(!quarantine.persistenceStopped)
        precondition(quarantine.receivedVersion == cleared && quarantine.savedVersion == cleared)
        precondition(disk("quarantine-failure") == nil)
        quarantine.receive(bytes2,version:LearningVersion(generation:1,revision:1))
        quarantineClock.advance(0.250); try! quarantine.flushForTesting()
        precondition(quarantineWrites == 1)
        precondition(disk("quarantine-failure") == bytes2)
    }

    private static var incompatiblePersonalFrequencySnapshot: PersonalFrequencySnapshot {
        return PersonalFrequencySnapshot(
            schemaVersion: PersonalFrequencySnapshot.currentSchemaVersion,
            dictionaryPayloadSHA256: String(repeating: "a", count: 64),
            dictionaryTokenCount: 10,
            observationCount: 1,
            entries: [PersonalFrequencyEntry(dictionaryTokenID: 3, count: 2)]
        )
    }

    private static func assertPersonalFrequencyDictionaryMismatch(dictionaryPath:String) {
        assertRejectedLearningStartup(dictionaryPath:dictionaryPath,snapshot:incompatiblePersonalFrequencySnapshot)
    }


    // After CORE_INVALID every controller clears what it shows (marked text, candidates,
    // badge) once and then passes keys through. A controller with nothing shown passes
    // the first key; one with a display consumes one key to clear it. No session is
    // recreated, and a new engine is unavailable.
    private static func assertCoreInvalidPresentation() {
        let clientA = FakeTextClient(), clientB = FakeTextClient()
        for client in [clientA,clientB] {
            client.configureGeometry(markedRange:NSRange(location:1,length:1),selectedRange:NSRange(location:1,length:1)) { range in
                CandidateClientRectResult(rect:NSRect(x:240,y:520,width:20,height:20),actualRange:range)
            }
        }
        let clock = ManualModeBadgeScheduler()
        let a = MuuInputController(testingClient:clientA,modeBadgeScheduler:clock.scheduler)
        let b = MuuInputController(testingClient:clientB,modeBadgeScheduler:clock.scheduler)
        let clientEmpty = FakeTextClient(), clientFlag = FakeTextClient(), clientLost = FakeTextClient()
        let empty = MuuInputController(testingClient:clientEmpty,modeBadgeScheduler:clock.scheduler)
        let flag = MuuInputController(testingClient:clientFlag,modeBadgeScheduler:clock.scheduler)
        let lost = MuuInputController(testingClient:clientLost,modeBadgeScheduler:clock.scheduler)
        func view(_ candidates:CandidateList?, transition:CompositionModeTransition?) -> EngineResponse {
            EngineResponse(status:MUU_RESULT_OK,view:ViewState(preedit:[PreeditRun(text:"か",utf16Start:0,utf16Length:1,isFocused:true,isConverted:candidates != nil)],caretUTF16:1,selection:nil,segments:nil,candidates:candidates,commitText:nil,mode:.hiragana,compositionModeTransition:transition,poisoned:false,consumed:true,error:nil))
        }
        precondition(a.applyForTesting(view(CandidateList(items:[CandidateItem(id:"one",text:"か",annotation:nil)],selectedIndex:0),transition:nil),to:clientA))
        precondition(b.applyForTesting(view(nil,transition:.literal),to:clientB))
        precondition(!a.composedTextForTesting.isEmpty && !b.composedTextForTesting.isEmpty)
        precondition(a.candidateSnapshotForTesting != nil)
        precondition(a.candidateSurfaceVisibleForTesting)
        precondition(b.modeBadgeVisibleForTesting)
        precondition(flag.applyForTesting(view(nil,transition:nil),to:clientFlag))
        let countA = clientA.commands.count, countB = clientB.commands.count
        var recreates = 0
        RustEngine.recreateObserverForTesting = { _ in recreates += 1 }
        defer { RustEngine.recreateObserverForTesting = nil }
        // Production observes status in RustEngine.handle; a synthetic
        // response must model that boundary explicitly.
        RustCore.observeStatus(MUU_RESULT_CORE_INVALID)
        precondition(!empty.handle(keyEvent(keyCode:0,characters:"a"),client:clientEmpty),
                     "global flag with no display passes the first key through")
        precondition(clientEmpty.commands.isEmpty)
        precondition(!empty.handle(keyEvent(keyCode:0,characters:"a"),client:clientEmpty))
        precondition(flag.handle(keyEvent(keyCode:0,characters:"a"),client:clientFlag),
                     "global flag with display discards it and consumes one key")
        precondition(clientFlag.commands.last == .mark(""))
        precondition(!flag.handle(keyEvent(keyCode:0,characters:"a"),client:clientFlag))
        precondition(lost.applyForTesting(EngineResponse(status:MUU_RESULT_CORE_INVALID,view:nil),to:clientLost),
                     "an attempted event returning 7 is consumed even with no display")
        precondition(clientLost.commands.isEmpty)
        precondition(a.applyForTesting(EngineResponse(status:MUU_RESULT_CORE_INVALID,view:nil),to:clientA))
        precondition(RustCore.isInvalid)
        precondition(clientA.commands.count == countA+1 && clientA.commands.last == .mark(""))
        precondition(a.composedTextForTesting.isEmpty && a.candidateSnapshotForTesting == nil)
        precondition(!a.candidateSurfaceVisibleForTesting && !a.modeBadgeVisibleForTesting)
        precondition(!a.handle(keyEvent(keyCode:0,characters:"a"),client:clientA))
        a.activateServer(clientA); a.deactivateServer(clientA); a.commitComposition(clientA)
        precondition(clientA.commands.count == countA+1)
        b.activateServer(clientB)
        precondition(clientB.commands.count == countB+1 && clientB.commands.last == .mark(""))
        precondition(b.composedTextForTesting.isEmpty && !b.modeBadgeVisibleForTesting)
        precondition(!b.candidateSurfaceVisibleForTesting && b.candidateSnapshotForTesting == nil)
        precondition(!b.handle(keyEvent(keyCode:0,characters:"a"),client:clientB))
        b.activateServer(clientB); b.deactivateServer(clientB); b.commitComposition(clientB)
        precondition(clientB.commands.count == countB+1)
        precondition(recreates == 0)
        let newEngine = RustEngine()
        precondition(!newEngine.isAvailable)
        // The shared ACK passes the command bytes once, returns the C result
        // unchanged, and observes each result exactly once.
        var observed: [Int32] = []
        RustCore.statusObserverForTesting = { observed.append($0) }
        defer { RustCore.statusObserverForTesting = nil }
        for expected: Int32 in [0, 4, 7, -1] {
            var calls = 0
            let result = RustCore.acknowledgeForTesting({ pointer, count in
                calls += 1
                precondition(count == 2 && pointer?[0] == 3 && pointer?[1] == 5)
                return expected
            }, Data([3, 5]))
            precondition(result == expected && calls == 1)
        }
        precondition(observed == [0, 4, 7, -1], "each ACK observes its result once")
    }

    // Learning across process restarts on one directory. Both processes load a
    // file bound to the current dictionary and add one observation. Startup alone
    // never rewrites it. With no file, learning starts at zero. No session exists
    // before startup data is accepted, and saves wait for the scheduler.
    private static func assertLearningRestart(dictionaryPath:String,testCase:String) {
        let base = URL(fileURLWithPath:ProcessInfo.processInfo.environment["MUU_TEST_LEARNING_RESTART_DIR"]!)
        try! FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        let url = base.appendingPathComponent("test/personal_frequency/personal-frequency-v1.json")
        let seed = PersonalFrequencySnapshot(schemaVersion:1,dictionaryPayloadSHA256:"4f5b9a05d483eb13a5685e2cab5b78dacf9e335248b015a3e103918b0a1724a0",dictionaryTokenCount:2_038_107,observationCount:1,entries:[PersonalFrequencyEntry(dictionaryTokenID:883_303,count:1)])
        if testCase == "learning-restart-first" {
            try! FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true)
            try! JSONEncoder().encode(seed).write(to:url)
        }
        if testCase == "learning-no-file" { try? FileManager.default.removeItem(at:url) }
        let clock = ManualPersistenceScheduler()
        let store = PersonalFrequencyStore(baseDirectory:base,bundleIdentifier:"test",scheduler:clock.schedule)
        let loaded = store.startupData
        precondition((loaded == nil) == (testCase == "learning-no-file"))
        let early = RustEngine()
        precondition(!early.isAvailable,"no Session can observe before startup data is accepted")
        let result = try! RustCore.initialize(CoreConfiguration(dictionaryPath:dictionaryPath,conversionLmDirectory:nil,commitStream:nil),learning:loaded).get()
        precondition(!result.degraded.personalFrequencyInvalid)
        store.acceptStartup(result)
        if let loaded {
            precondition(try! Data(contentsOf:url) == loaded, "accepted startup never rewrites learning")
        }
        precondition(store.receivedVersion == result.personalFrequencyVersion && store.savedVersion == result.personalFrequencyVersion)
        let accepted = try! RustCore.personalFrequency(after:nil).get()!
        let model = try! JSONSerialization.jsonObject(with:accepted.0) as! [String:Any]
        precondition(model["observation_count"] as? Int == (testCase == "learning-restart-first" ? 1 : testCase == "learning-restart-second" ? 2 : 0))
        precondition(clock.count == 0)
        let engine = RustEngine()
        precondition(engine.isAvailable)
        for text in "kyou" { _ = engine.handle(EventRequest(key:KeyEvent(eventType:"key_down",keycode:0,modifiers:0,charactersUTF8:String(text),charactersIgnoringModifiers:String(text),isRepeat:false))) }
        let converted = engine.handle(EventRequest(key:KeyEvent(eventType:"key_down",keycode:49,modifiers:0,charactersUTF8:" ",charactersIgnoringModifiers:" ",isRepeat:false)))
        let items = converted.view!.candidates!.items
        precondition(items.count > 1)
        _ = engine.handle(CommandRequest(command:"select_candidate",arg:.candidate(id:items[1].id)))
        _ = engine.handle(CommandRequest(command:"commit"))
        engine.drainPersonalFrequencySnapshot(to:store)
        precondition(store.receivedVersion! > result.personalFrequencyVersion)
        precondition(store.savedVersion == result.personalFrequencyVersion)
        precondition(clock.count == 1)
        clock.advance(0.250); try! store.flushForTesting()
        precondition(store.savedVersion == store.receivedVersion)
        let saved = try! JSONSerialization.jsonObject(with:Data(contentsOf:url)) as! [String:Any]
        precondition(saved["observation_count"] as? Int == (model["observation_count"] as! Int)+1)
    }

    private static func keyEvent(
        keyCode: UInt16,
        modifiers: NSEvent.ModifierFlags = [],
        characters: String
    ) -> NSEvent {
        guard let event = NSEvent.keyEvent(
            with: .keyDown,
            location: .zero,
            modifierFlags: modifiers,
            timestamp: 0,
            windowNumber: 0,
            context: nil,
            characters: characters,
            charactersIgnoringModifiers: characters,
            isARepeat: false,
            keyCode: keyCode
        ) else {
            preconditionFailure("key event fixture")
        }
        return event
    }

    private static func flagsChangedEvent(
        keyCode: CGKeyCode,
        modifiers: CGEventFlags
    ) -> NSEvent {
        guard let event = CGEvent(
            keyboardEventSource: nil,
            virtualKey: keyCode,
            keyDown: modifiers.contains(.maskShift)
        ) else {
            preconditionFailure("CG flags event fixture")
        }
        event.type = .flagsChanged
        event.flags = modifiers
        guard let appKitEvent = NSEvent(cgEvent: event) else {
            preconditionFailure("AppKit flags event fixture")
        }
        return appKitEvent
    }

    private static func mouseDownEvent() -> NSEvent {
        guard let event = NSEvent.mouseEvent(
            with: .leftMouseDown,
            location: .zero,
            modifierFlags: [],
            timestamp: 0,
            windowNumber: 0,
            context: nil,
            eventNumber: 1,
            clickCount: 1,
            pressure: 1
        ) else {
            preconditionFailure("mouse event fixture")
        }
        return event
    }

    private static func assertPresentationPreserved(
        _ controller: MuuInputController,
        from snapshot: CandidateSurfaceSnapshot
    ) {
        guard let current = controller.candidateSnapshotForTesting else {
            preconditionFailure("error must preserve the candidate presentation")
        }
        precondition(current.items == snapshot.items)
        precondition(current.selectedIndex == snapshot.selectedIndex)
        precondition(current.generation > snapshot.generation)
        precondition(controller.candidateSurfaceVisibleForTesting)
    }
}
