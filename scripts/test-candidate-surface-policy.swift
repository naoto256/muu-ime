import AppKit

enum MuuConstants {
    static let bundleIdentifier = "dev.muu.inputmethod.Muu.policy-smoke"
}

private final class VerticalTextClientProbe: NSObject {
    private(set) var queriedIndices: [UInt] = []

    @objc(drawsVerticallyForCharacterAtIndex:)
    func drawsVertically(forCharacterAt index: UInt) -> Bool {
        queriedIndices.append(index)
        return true
    }
}

// The candidate panel's rules, tested on the real AppKit panel and the pure
// policy functions, with scripted clients instead of an IMK host. Each group
// is one function below; a failed precondition names the broken rule.
@main
@MainActor
private enum CandidateSurfacePolicyTests {
    static func main() {
        _ = NSApplication.shared
        testPanelLifecycleFocusMouseAccessibility()
        testModelsUpdatesAndActionAuthority()
        testClientRangesAndAdapter()
        testGeometryPlacementAndCommitPolicy()
        print("candidate surface policy: passed")
    }

    // The real panel: show/hide lifecycle, no stale anchor carried into the
    // next composition, focus and mouse behaviour, and the VoiceOver actions
    // it reports.
    private static func testPanelLifecycleFocusMouseAccessibility() {
        var accessibilityActions: [CandidateSurfaceAction] = []
        let appKitSurface = CandidatePanelController {
            accessibilityActions.append($0)
        }
        appKitSurface.hide()
        precondition(
            appKitSurface.hasShadowForTesting,
            "candidate panel keeps its standard shadow"
        )

        let first = candidateFixture()
        let smokeKeyWindow = NSApp.keyWindow
        let smokeMainWindow = NSApp.mainWindow
        let smokeFirstResponder = smokeKeyWindow?.firstResponder
        let firstCompositionRects = [
            NSRect(x: 80, y: 390, width: 300, height: 30)
        ]
        let firstAnchors = CandidateSurfaceAnchors(
            primary: NSRect(x: 100, y: 400, width: 20, height: 20),
            retry: nil,
            selectedRange: nil,
            compositionRects: firstCompositionRects,
            defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
            visibleFrame: NSRect(x: 0, y: 0, width: 800, height: 600)
        )
        appKitSurface.render(
            snapshot: first,
            anchors: firstAnchors
        )
        precondition(NSApp.keyWindow === smokeKeyWindow)
        precondition(NSApp.mainWindow === smokeMainWindow)
        precondition(NSApp.keyWindow?.firstResponder === smokeFirstResponder)
        precondition(
            firstAnchors.compositionExclusionRects.allSatisfy {
                !$0.intersects(appKitSurface.frameForTesting)
            }
        )
        let firstAccessibilityRow =
            appKitSurface.accessibilityRowForTesting(at: 0)
        precondition(firstAccessibilityRow?.accessibilityPerformPick() == true)
        precondition(
            accessibilityActions.last == .select(id: "a", generation: 7)
        )
        precondition(firstAccessibilityRow?.accessibilityPerformPress() == true)
        precondition(
            accessibilityActions.last
                == .selectAndCommit(id: "a", generation: 7)
        )

        let selectionOnly = CandidateSurfaceSnapshot(
            items: first.items,
            selectedIndex: 1,
            generation: first.generation
        )
        let stableFrame = appKitSurface.frameForTesting
        appKitSurface.render(snapshot: selectionOnly, anchors: firstAnchors)
        precondition(appKitSurface.frameForTesting == stableFrame)

        let movedCompositionRects = [
            NSRect(x: 80, y: 300, width: 300, height: 30)
        ]
        let movedAnchors = CandidateSurfaceAnchors(
                primary: NSRect(x: 100, y: 310, width: 20, height: 20),
                retry: nil,
                selectedRange: nil,
                compositionRects: movedCompositionRects,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: NSRect(x: 0, y: 0, width: 800, height: 600)
            )
        appKitSurface.render(
            snapshot: selectionOnly,
            anchors: movedAnchors
        )
        precondition(appKitSurface.frameForTesting != stableFrame)
        precondition(
            movedAnchors.compositionExclusionRects.allSatisfy {
                !$0.intersects(appKitSurface.frameForTesting)
            }
        )
        precondition(NSApp.keyWindow === smokeKeyWindow)
        precondition(NSApp.mainWindow === smokeMainWindow)
        precondition(NSApp.keyWindow?.firstResponder === smokeFirstResponder)

        var pagedActions: [CandidateSurfaceAction] = []
        let pagedSurface = CandidatePanelController {
            pagedActions.append($0)
        }
        let pagedItems = (0..<4_096).map { index in
            CandidateSurfaceItem(
                id: "page-\(index)",
                text: "候補\(index)",
                annotation: nil
            )
        }
        let lastPage = CandidateSurfaceSnapshot(
            items: pagedItems,
            selectedIndex: 4_095,
            generation: 1
        )
        pagedSurface.render(snapshot: lastPage, anchors: firstAnchors)
        precondition(pagedSurface.visibleGlobalRangeForTesting == 4_088..<4_096)
        precondition(pagedSurface.materializedRowCountForTesting == 8)
        precondition(pagedSurface.performMouseSelectionForTesting(at: 7))
        precondition(
            pagedActions.last == .select(id: "page-4095", generation: 1)
        )
        let previousPage = CandidateSurfaceSnapshot(
            items: pagedItems,
            selectedIndex: 4_087,
            generation: 1
        )
        pagedSurface.render(snapshot: previousPage, anchors: firstAnchors)
        precondition(pagedSurface.visibleGlobalRangeForTesting == 4_080..<4_088)
        precondition(pagedSurface.materializedRowCountForTesting == 8)
        let firstVisibleAccessibilityRow =
            pagedSurface.accessibilityRowForTesting(at: 0)
        precondition(
            firstVisibleAccessibilityRow?.accessibilityPerformPick() == true
        )
        precondition(
            pagedActions.last == .select(id: "page-4080", generation: 1)
        )
        pagedSurface.hide()

        let hiddenSurface = CandidatePanelController { _ in }
        hiddenSurface.render(snapshot: first, anchors: firstAnchors)
        hiddenSurface.hide()
        hiddenSurface.render(
            snapshot: CandidateSurfaceSnapshot(
                items: first.items,
                selectedIndex: first.selectedIndex,
                generation: first.generation + 1
            ),
            anchors: CandidateSurfaceAnchors(
                primary: nil,
                retry: nil,
                selectedRange: nil,
                defaultAnchor: .zero,
                visibleFrame: firstAnchors.visibleFrame
            )
        )
        precondition(
            !hiddenSurface.isVisibleForTesting,
            "a hidden composition must not donate its cached anchor to the next composition"
        )

        let appKitReplacement = CandidateSurfaceSnapshot(
            items: [
                CandidateSurfaceItem(id: "a", text: "候補A", annotation: nil),
                CandidateSurfaceItem(id: "c", text: "候補C", annotation: nil),
            ],
            selectedIndex: 0,
            generation: 8
        )
        appKitSurface.render(
            snapshot: appKitReplacement,
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 100, y: 400, width: 20, height: 20),
                retry: nil,
                selectedRange: nil,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: NSRect(x: 0, y: 0, width: 800, height: 600)
            )
        )
        let actionCountAfterReplacement = accessibilityActions.count
        precondition(firstAccessibilityRow?.accessibilityPerformPick() == false)
        precondition(accessibilityActions.count == actionCountAfterReplacement)
        let replacementAccessibilityRow =
            appKitSurface.accessibilityRowForTesting(at: 0)
        precondition(
            replacementAccessibilityRow?.accessibilityPerformPick() == true
        )
        precondition(
            accessibilityActions.last == .select(id: "a", generation: 8)
        )
        appKitSurface.hide()
        let actionCountAfterHide = accessibilityActions.count
        precondition(
            replacementAccessibilityRow?.accessibilityPerformPress() == false
        )
        precondition(accessibilityActions.count == actionCountAfterHide)

        var teardownActions: [CandidateSurfaceAction] = []
        var teardownSurface: CandidatePanelController? =
            CandidatePanelController {
                teardownActions.append($0)
            }
        teardownSurface?.render(
            snapshot: first,
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 100, y: 400, width: 20, height: 20),
                retry: nil,
                selectedRange: nil,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: NSRect(x: 0, y: 0, width: 800, height: 600)
            )
        )
        let teardownAccessibilityRow =
            teardownSurface?.accessibilityRowForTesting(at: 0)
        teardownSurface = nil
        precondition(
            teardownAccessibilityRow?.accessibilityPerformPick() == false
        )
        precondition(teardownActions.isEmpty)
    }

    /// Each sentinel follows real main-queue callbacks. The first valid retry
    /// must succeed before introducing superseded tokens, so those later
    /// renders cannot accidentally mask a reservation left by the rejection.


    // Snapshot validity (unique ids, valid selection), the minimal update for
    // each change (reload the list, move the selection, show), and which
    // candidate actions a click may emit: every action names its generation,
    // and a double click whose first click was in an older generation does
    // not commit.
    private static func testModelsUpdatesAndActionAuthority() {
        let first = candidateFixture()
        let selectedSecond = CandidateSurfaceSnapshot(
            items: first.items,
            selectedIndex: 1,
            generation: 7
        )
        precondition(CandidateSurfacePolicy.isValidSnapshot(first))
        precondition(
            !CandidateSurfacePolicy.isValidSnapshot(
                CandidateSurfaceSnapshot(
                    items: [
                        CandidateSurfaceItem(
                            id: "duplicate",
                            text: "候補A",
                            annotation: nil
                        ),
                        CandidateSurfaceItem(
                            id: "duplicate",
                            text: "候補B",
                            annotation: nil
                        ),
                    ],
                    selectedIndex: 0,
                    generation: 7
                )
            )
        )
        precondition(
            CandidateSurfacePolicy.update(
                current: nil,
                next: first,
                isVisible: false
            ) == CandidateSurfaceUpdate(
                reloadList: true,
                updateSelection: true,
                show: true
            )
        )
        precondition(
            CandidateSurfacePolicy.update(
                current: first,
                next: selectedSecond,
                isVisible: true
            ) == CandidateSurfaceUpdate(
                reloadList: false,
                updateSelection: true,
                show: false
            )
        )
        precondition(
            CandidateSurfacePolicy.update(
                current: selectedSecond,
                next: selectedSecond,
                isVisible: true
            ) == CandidateSurfaceUpdate(
                reloadList: false,
                updateSelection: false,
                show: false
            )
        )
        precondition(
            CandidateSurfacePolicy.candidateID(at: 1, in: first) == "b"
        )
        precondition(
            CandidateSurfacePolicy.candidateID(at: 2, in: first) == nil
        )
        let singleClickAction = CandidateSurfacePolicy.candidateAction(
            at: 1,
            in: selectedSecond,
            commit: false
        )
        precondition(singleClickAction == .select(id: "b", generation: 7))
        precondition(
            CandidateSurfacePolicy.doubleClickAction(
                after: singleClickAction,
                at: 1,
                in: selectedSecond
            ) == .selectAndCommit(id: "b", generation: 7)
        )
        let nextGeneration = CandidateSurfaceSnapshot(
            items: selectedSecond.items,
            selectedIndex: selectedSecond.selectedIndex,
            generation: 8
        )
        precondition(
            CandidateSurfacePolicy.update(
                current: selectedSecond,
                next: nextGeneration,
                isVisible: true
            ) == CandidateSurfaceUpdate(
                reloadList: true,
                updateSelection: true,
                show: false
            )
        )
        precondition(
            CandidateSurfacePolicy.candidateAction(
                at: 1,
                in: nextGeneration,
                commit: true
            ) == .selectAndCommit(id: "b", generation: 8)
        )
        precondition(
            CandidateSurfacePolicy.doubleClickAction(
                after: singleClickAction,
                at: 1,
                in: nextGeneration
            ) == nil
        )
        precondition(
            CandidateSurfacePolicy.doubleClickAction(
                after: singleClickAction,
                at: 0,
                in: selectedSecond
            ) == nil
        )

        var authority = CandidateActionAuthority()
        let authorityFirst = authority.snapshot(
            items: first.items,
            selectedIndex: 0
        )
        precondition(authorityFirst?.generation == 1)
        let authorityOldAction = authorityFirst.flatMap {
            CandidateSurfacePolicy.candidateAction(
                at: 1,
                in: $0,
                commit: false
            )
        }
        precondition(authorityOldAction.map(authority.accepts) == true)

        let replacementItems = [
            CandidateSurfaceItem(id: "b", text: "候補B", annotation: "名詞"),
            CandidateSurfaceItem(id: "c", text: "候補C", annotation: nil),
        ]
        let authorityReplacement = authority.snapshot(
            items: replacementItems,
            selectedIndex: 0
        )
        precondition(authorityReplacement?.generation == 2)
        precondition(authorityOldAction.map(authority.accepts) == false)
        precondition(
            authorityOldAction.flatMap { oldAction in
                authorityReplacement.flatMap {
                    CandidateSurfacePolicy.doubleClickAction(
                        after: oldAction,
                        at: 0,
                        in: $0
                    )
                }
            } == nil
        )
        let replacementAction = authorityReplacement.flatMap {
            CandidateSurfacePolicy.candidateAction(
                at: 0,
                in: $0,
                commit: false
            )
        }
        precondition(replacementAction.map(authority.accepts) == true)

        authority.invalidate()
        precondition(authority.generation == 3)
        precondition(replacementAction.map(authority.accepts) == false)
        let sameIDsNewComposition = authority.snapshot(
            items: replacementItems,
            selectedIndex: 0
        )
        precondition(sameIDsNewComposition?.generation == 4)
        precondition(replacementAction.map(authority.accepts) == false)
        let genuineNewAction = sameIDsNewComposition.flatMap {
            CandidateSurfacePolicy.candidateAction(
                at: 0,
                in: $0,
                commit: false
            )
        }
        precondition(genuineNewAction.map(authority.accepts) == true)
        precondition(
            authority.accepts(.select(id: "unknown", generation: 4)) == false
        )

        do {
            var failureAuthority = CandidateActionAuthority()
            guard let failureSnapshot = failureAuthority.snapshot(
                items: first.items,
                selectedIndex: 0
            ) else {
                preconditionFailure("failure lifecycle needs a valid snapshot")
            }
            var failureActions: [CandidateSurfaceAction] = []
            let failureSurface = CandidatePanelController { action in
                guard failureAuthority.accepts(action) else { return }
                failureActions.append(action)
            }
            failureSurface.render(
                snapshot: failureSnapshot,
                anchors: CandidateSurfaceAnchors(
                    primary: NSRect(x: 100, y: 400, width: 20, height: 20),
                    retry: nil,
                    selectedRange: nil,
                    defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                    visibleFrame: NSRect(x: 0, y: 0, width: 800, height: 600)
                )
            )
            let queuedMouseAction = CandidateSurfacePolicy.candidateAction(
                at: 0,
                in: failureSnapshot,
                commit: false
            )
            let queuedAccessibilityRow =
                failureSurface.accessibilityRowForTesting(at: 0)
            precondition(
                failureSurface.performMouseSelectionForTesting(at: 0)
            )
            precondition(
                failureActions.last
                    == .select(id: "a", generation: failureSnapshot.generation)
            )
            precondition(failureSurface.hasPendingSelectionForTesting)

            // The surface contract is common to failure exits: keep the
            // visible snapshot and invalidate only actions. Response routing
            // belongs to the input presentation tests.
            failureSurface.invalidateActionsPreservingPresentation()
            failureAuthority.invalidate()
            let actionCountBeforeStaleDelivery = failureActions.count
            let invalidatedSnapshot = failureSurface.snapshotForTesting
            precondition(!failureSurface.actionsEnabledForTesting)
            precondition(!failureSurface.hasPendingSelectionForTesting)
            precondition(invalidatedSnapshot?.items == failureSnapshot.items)
            precondition(
                invalidatedSnapshot?.selectedIndex
                    == failureSnapshot.selectedIndex
            )
            precondition(
                invalidatedSnapshot?.generation
                    != failureSnapshot.generation
            )
            precondition(
                queuedMouseAction.map(failureAuthority.accepts) == false
            )
            precondition(
                failureSurface.performMouseSelectionForTesting(at: 0) == false
            )
            precondition(
                failureSurface.performMouseCommitForTesting(at: 0) == false
            )
            precondition(
                queuedMouseAction.flatMap {
                    CandidateSurfacePolicy.doubleClickAction(
                        after: $0,
                        at: 0,
                        in: failureSnapshot
                    )
                }.map(failureAuthority.accepts) == false
            )
            precondition(failureSurface.isVisibleForTesting)
            precondition(
                queuedAccessibilityRow?.accessibilityPerformPick() == false
            )
            precondition(
                queuedAccessibilityRow?.accessibilityPerformPress() == false
            )
            precondition(failureActions.count == actionCountBeforeStaleDelivery)
            if let invalidatedSnapshot {
                failureSurface.render(
                    snapshot: invalidatedSnapshot,
                    anchors: CandidateSurfaceAnchors(
                        primary: NSRect(
                            x: 100,
                            y: 400,
                            width: 20,
                            height: 20
                        ),
                        retry: nil,
                        selectedRange: nil,
                        defaultAnchor: NSRect(
                            x: 10,
                            y: 10,
                            width: 10,
                            height: 10
                        ),
                        visibleFrame: NSRect(
                            x: 0,
                            y: 0,
                            width: 800,
                            height: 600
                        )
                    )
                )
            }
            precondition(!failureSurface.actionsEnabledForTesting)
            precondition(failureActions.count == actionCountBeforeStaleDelivery)

            guard let healedSnapshot = failureAuthority.snapshot(
                items: first.items,
                selectedIndex: 0
            ) else {
                preconditionFailure("normal view must rematerialize candidates")
            }
            precondition(
                healedSnapshot.generation != failureSnapshot.generation
            )
            failureSurface.render(
                snapshot: healedSnapshot,
                anchors: CandidateSurfaceAnchors(
                    primary: NSRect(x: 100, y: 400, width: 20, height: 20),
                    retry: nil,
                    selectedRange: nil,
                    defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                    visibleFrame: NSRect(x: 0, y: 0, width: 800, height: 600)
                )
            )
            precondition(failureSurface.actionsEnabledForTesting)
            let healedAccessibilityRow =
                failureSurface.accessibilityRowForTesting(at: 0)
            precondition(
                healedAccessibilityRow?.accessibilityPerformPick() == true
            )
            precondition(
                failureActions.last
                    == .select(id: "a", generation: healedSnapshot.generation)
            )
            precondition(
                failureSurface.performMouseSelectionForTesting(at: 0)
            )
            precondition(failureSurface.hasPendingSelectionForTesting)
            precondition(
                failureSurface.performMouseCommitForTesting(at: 0)
            )
            precondition(!failureSurface.hasPendingSelectionForTesting)
            precondition(
                failureActions.last
                    == .selectAndCommit(
                        id: "a",
                        generation: healedSnapshot.generation
                    )
            )
            precondition(
                healedAccessibilityRow?.accessibilityPerformPress() == true
            )
            failureSurface.hide()
        }

        precondition(
            CandidateSurfacePolicy.accessibilityLabel(for: first.items[0])
                == "候補A"
        )
        precondition(
            CandidateSurfacePolicy.accessibilityLabel(for: first.items[1])
                == "候補B, 名詞"
        )
    }

    // Reading geometry from the client: which ranges are queried, which
    // firstRect answers are accepted (complete, contiguous, matching the
    // requested range), and that anything inconsistent fails closed rather
    // than placing the panel on a guess.
    private static func testClientRangesAndAdapter() {
        let verticalProbe = VerticalTextClientProbe()
        precondition(
            ClientGeometryReader.textOrientation(
                client: verticalProbe,
                at: 42
            ) == .vertical
        )
        precondition(verticalProbe.queriedIndices == [42])
        precondition(
            ClientGeometryReader.textOrientation(
                client: verticalProbe,
                at: -1
            ) == .horizontal
        )
        precondition(verticalProbe.queriedIndices == [42])
        precondition(
            ClientGeometryReader.textOrientation(
                client: NSObject(),
                at: 42
            ) == .horizontal
        )

        let visibleFrame = NSRect(x: 0, y: 0, width: 800, height: 600)
        let resolvedClientGeometryResolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 100, length: 6),
            preferredRange: NSRange(location: 102, length: 1),
            selectedRange: NSRange(location: 200, length: 0)
        ) { range in
            switch (range.location, range.length) {
            case (102, 1):
                return CandidateClientRectResult(
                    rect: NSRect(x: 120.5, y: 400.5, width: 10, height: 20),
                    actualRange: NSRange(location: 100, length: 3)
                )
            case (100, 3):
                return CandidateClientRectResult(
                    rect: NSRect(x: 100.5, y: 400.5, width: 80, height: 20),
                    actualRange: NSRange(location: 100, length: 3)
                )
            case (200, 0):
                return CandidateClientRectResult(
                    rect: NSRect(x: 200.5, y: 300.5, width: 1, height: 20),
                    actualRange: NSRange(location: 200, length: 0)
                )
            case (100, 6):
                return CandidateClientRectResult(
                    rect: NSRect(x: 100.5, y: 400.5, width: 80, height: 20),
                    actualRange: NSRange(location: 100, length: 3)
                )
            case (103, 3):
                return CandidateClientRectResult(
                    rect: NSRect(x: 20.5, y: 380.5, width: 90, height: 20),
                    actualRange: NSRange(location: 103, length: 3)
                )
            default:
                preconditionFailure("unexpected IMK range query \(range)")
            }
        }
        guard case let .complete(resolvedClientGeometry) =
            resolvedClientGeometryResolution
        else {
            preconditionFailure("complete logical rectangles must resolve")
        }
        precondition(
            resolvedClientGeometry.primary
                == NSRect(x: 120.5, y: 400.5, width: 10, height: 20)
        )
        precondition(
            resolvedClientGeometry.retry
                == NSRect(x: 100.5, y: 400.5, width: 80, height: 20)
        )
        precondition(
            resolvedClientGeometry.selectedRange
                == NSRect(x: 200.5, y: 300.5, width: 1, height: 20)
        )
        precondition(
            resolvedClientGeometry.compositionRects == [
                NSRect(x: 100.5, y: 400.5, width: 80, height: 20),
                NSRect(x: 20.5, y: 380.5, width: 90, height: 20),
            ]
        )
        precondition(
            resolvedClientGeometry.compositionFragments.map(\.actualRange) == [
                NSRange(location: 100, length: 3),
                NSRange(location: 103, length: 3),
            ]
        )
        precondition(
            resolvedClientGeometry.compositionLineMetrics == [20, 20]
        )
        precondition(
            resolvedClientGeometry.compositionOrientations
                == [.horizontal, .horizontal]
        )
        precondition(
            resolvedClientGeometry.compositionExclusionRects == [
                NSRect(x: 80.5, y: 380.5, width: 120, height: 60),
                NSRect(x: 0.5, y: 360.5, width: 130, height: 60),
            ]
        )
        var verticalOrientationQueries: [Int] = []
        let verticalGeometryResolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 2),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0),
            orientationAt: { index in
                verticalOrientationQueries.append(index)
                return .vertical
            }
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 500, y: 300, width: -18, height: 72),
                actualRange: range
            )
        }
        guard case let .complete(verticalGeometry) =
            verticalGeometryResolution
        else {
            preconditionFailure("vertical fragment must resolve")
        }
        precondition(verticalOrientationQueries == [10])
        precondition(verticalGeometry.compositionLineMetrics == [18])
        precondition(verticalGeometry.compositionOrientations == [.vertical])
        precondition(
            verticalGeometry.compositionRects
                == [NSRect(x: 482, y: 300, width: 18, height: 72)]
        )
        precondition(
            verticalGeometry.compositionExclusionRects
                == [NSRect(x: 464, y: 282, width: 54, height: 108)]
        )
        let verticalPlacement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: verticalGeometry.compositionRects[0],
                retry: nil,
                selectedRange: nil,
                compositionRects: verticalGeometry.compositionRects,
                compositionExclusionRects:
                    verticalGeometry.compositionExclusionRects,
                compositionLineMetrics:
                    verticalGeometry.compositionLineMetrics,
                compositionOrientations:
                    verticalGeometry.compositionOrientations,
                defaultAnchor: NSRect(x: 10, y: 10, width: 1, height: 20),
                visibleFrame: visibleFrame,
                backingScale: 2
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 200, height: 100)
        )
        precondition(
            verticalPlacement?.frame
                == NSRect(x: 482, y: 178, width: 200, height: 100)
        )

        var sameLocationFallbackQueries: [NSRange] = []
        let sameLocationFallback = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { range in
            sameLocationFallbackQueries.append(range)
            if range == NSRange(location: 14, length: 0) {
                return CandidateClientRectResult(
                    rect: NSRect(x: 40, y: 100, width: 0, height: 20),
                    actualRange: range
                )
            }
            precondition(range == NSRange(location: 10, length: 4))
            return CandidateClientRectResult(
                rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                actualRange: NSRange(location: range.location, length: 0)
            )
        }
        guard case let .complete(sameLocationFallbackGeometry) = sameLocationFallback
        else {
            preconditionFailure(
                "an initial same-location zero-length range must use its caret boundary"
            )
        }
        precondition(
            sameLocationFallbackQueries == [
                NSRange(location: 10, length: 4),
                NSRange(location: 14, length: 0),
            ]
        )
        precondition(
            sameLocationFallbackGeometry.primary
                == NSRect(x: 40, y: 100, width: 1, height: 20)
        )
        precondition(sameLocationFallbackGeometry.compositionFragments.isEmpty)
        precondition(!sameLocationFallbackGeometry.compositionRectsComplete)

        var boundaryFallbackQueries: [NSRange] = []
        let boundaryFallback = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { range in
            boundaryFallbackQueries.append(range)
            if range == NSRange(location: 10, length: 4) {
                return CandidateClientRectResult(
                    rect: .zero,
                    actualRange: NSRange(location: 0, length: 0)
                )
            }
            precondition(range == NSRange(location: 14, length: 0))
            return CandidateClientRectResult(
                rect: NSRect(x: 40, y: 100, width: 0, height: 20),
                actualRange: range
            )
        }
        guard case let .complete(boundaryFallbackGeometry) = boundaryFallback else {
            preconditionFailure("an unavailable marked range must use its caret boundary")
        }
        precondition(
            boundaryFallbackQueries == [
                NSRange(location: 10, length: 4),
                NSRange(location: 14, length: 0),
            ]
        )
        precondition(
            boundaryFallbackGeometry.primary
                == NSRect(x: 40, y: 100, width: 1, height: 20)
        )
        precondition(boundaryFallbackGeometry.compositionFragments.isEmpty)
        precondition(boundaryFallbackGeometry.compositionExclusionRects.isEmpty)
        precondition(!boundaryFallbackGeometry.compositionRectsComplete)

        for observedUnavailableRect in [
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
        ] {
            var observedQueries: [NSRange] = []
            let observedFallback = CandidateSurfacePolicy.clientGeometry(
                markedRange: NSRange(location: 10, length: 4),
                preferredRange: nil,
                selectedRange: NSRange(location: NSNotFound, length: 0)
            ) { range in
                observedQueries.append(range)
                if range == NSRange(location: 10, length: 4) {
                    return CandidateClientRectResult(
                        rect: observedUnavailableRect,
                        actualRange: NSRange(location: 0, length: 0)
                    )
                }
                precondition(range == NSRange(location: 14, length: 0))
                return CandidateClientRectResult(
                    rect: NSRect(x: 40, y: 100, width: 0, height: 20),
                    actualRange: range
                )
            }
            guard case .complete = observedFallback else {
                preconditionFailure(
                    "observed location-mismatched sentinel geometry must recover"
                )
            }
            precondition(
                observedQueries == [
                    NSRange(location: 10, length: 4),
                    NSRange(location: 14, length: 0),
                ]
            )
        }

        let invalidBoundaryFallback = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { range in
            CandidateClientRectResult(
                rect: .zero,
                actualRange: NSRange(location: 0, length: 0)
            )
        }
        guard case .failure = invalidBoundaryFallback else {
            preconditionFailure("an unavailable boundary must still fail closed")
        }

        var mismatchedBoundaryQueries: [NSRange] = []
        let mismatchedBoundaryFallback = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { range in
            mismatchedBoundaryQueries.append(range)
            if range == NSRange(location: 10, length: 4) {
                return CandidateClientRectResult(
                    rect: .zero,
                    actualRange: NSRange(location: 0, length: 0)
                )
            }
            precondition(range == NSRange(location: 14, length: 0))
            return CandidateClientRectResult(
                rect: NSRect(x: 40, y: 100, width: 0, height: 20),
                actualRange: NSRange(location: 13, length: 0)
            )
        }
        precondition(
            mismatchedBoundaryQueries == [
                NSRange(location: 10, length: 4),
                NSRange(location: 14, length: 0),
            ]
        )
        guard case .failure = mismatchedBoundaryFallback else {
            preconditionFailure(
                "a usable caret rectangle for another location must fail closed"
            )
        }

        let missingActualRange = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { _ in
            CandidateClientRectResult(
                rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                actualRange: NSRange(location: NSNotFound, length: 0)
            )
        }
        guard case .failure = missingActualRange else {
            preconditionFailure("NSNotFound actualRange must fail closed")
        }

        let gap = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { _ in
            CandidateClientRectResult(
                rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                actualRange: NSRange(location: 11, length: 3)
            )
        }
        guard case .failure = gap else {
            preconditionFailure("gapped actualRange must fail closed")
        }

        let disjoint = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { _ in
            CandidateClientRectResult(
                rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                actualRange: NSRange(location: 9, length: 1)
            )
        }
        guard case .failure = disjoint else {
            preconditionFailure("disjoint actualRange must fail closed")
        }

        let overshoot = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { _ in
            CandidateClientRectResult(
                rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                actualRange: NSRange(location: 10, length: 5)
            )
        }
        guard case .failure = overshoot else {
            preconditionFailure("overshooting actualRange must fail closed")
        }

        var overflowQueried = false
        let overflow = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: Int.max - 1, length: 2),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { _ in
            overflowQueried = true
            return CandidateClientRectResult(
                rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                actualRange: NSRange(location: 10, length: 1)
            )
        }
        precondition(!overflowQueried)
        guard case .failure = overflow else {
            preconditionFailure("overflowing markedRange must fail before querying")
        }

        for invalidMarkedRange in [
            NSRange(location: -1, length: 4),
            NSRange(location: 10, length: -1),
        ] {
            var invalidMarkedQueried = false
            var invalidOrientationQueried = false
            let invalidMarked = CandidateSurfacePolicy.clientGeometry(
                markedRange: invalidMarkedRange,
                preferredRange: nil,
                selectedRange: NSRange(location: NSNotFound, length: 0),
                orientationAt: { _ in
                    invalidOrientationQueried = true
                    return .vertical
                }
            ) { _ in
                invalidMarkedQueried = true
                return CandidateClientRectResult(
                    rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                    actualRange: NSRange(location: 10, length: 1)
                )
            }
            precondition(!invalidMarkedQueried)
            precondition(!invalidOrientationQueried)
            guard case .failure = invalidMarked else {
                preconditionFailure("negative markedRange must fail before querying")
            }
        }

        for invalidActualRange in [
            NSRange(location: -1, length: 2),
            NSRange(location: 12, length: -1),
            NSRange(location: Int.max - 1, length: 2),
        ] {
            var actualRangeQueries = 0
            let invalidActual = CandidateSurfacePolicy.clientGeometry(
                markedRange: NSRange(location: 10, length: 4),
                preferredRange: nil,
                selectedRange: NSRange(location: NSNotFound, length: 0)
            ) { range in
                actualRangeQueries += 1
                if range.location == 10 {
                    return CandidateClientRectResult(
                        rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                        actualRange: NSRange(location: 10, length: 2)
                    )
                }
                return CandidateClientRectResult(
                    rect: NSRect(x: 30, y: 100, width: 20, height: 20),
                    actualRange: invalidActualRange
                )
            }
            precondition(actualRangeQueries == 2)
            guard case .failure = invalidActual else {
                preconditionFailure(
                    "negative or overflowing actualRange must discard the prefix"
                )
            }
        }

        let partialInvalid = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 10, length: 4),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { range in
            if range.location == 10 {
                return CandidateClientRectResult(
                    rect: NSRect(x: 10, y: 100, width: 20, height: 20),
                    actualRange: NSRange(location: 10, length: 2)
                )
            }
            return CandidateClientRectResult(
                rect: NSRect(x: 30, y: 100, width: 20, height: 0),
                actualRange: NSRange(location: 12, length: 2)
            )
        }
        guard case .failure = partialInvalid else {
            preconditionFailure("an invalid later rect must discard the complete prefix")
        }

        let invalidVerticalMetric = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 20, length: 2),
            preferredRange: nil,
            selectedRange: NSRange(location: NSNotFound, length: 0),
            orientationAt: { _ in .vertical }
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 30, y: 100, width: 0, height: 40),
                actualRange: range
            )
        }
        guard case .failure = invalidVerticalMetric else {
            preconditionFailure("vertical fragments require a positive width metric")
        }

        var safeFallbackQueries: [NSRange] = []
        let safeFallbackResolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 800, length: 2),
            preferredRange: NSRange(location: -1, length: 0),
            selectedRange: NSRange(location: 900, length: -1)
        ) { range in
            safeFallbackQueries.append(range)
            guard range == NSRange(location: 800, length: 2) else {
                preconditionFailure("invalid preferred/selected ranges must not be queried")
            }
            return CandidateClientRectResult(
                rect: NSRect(x: 720, y: 220, width: 40, height: 20),
                actualRange: range
            )
        }
        guard case let .complete(safeFallbackGeometry) = safeFallbackResolution else {
            preconditionFailure("invalid optional anchors must use composition fallback")
        }
        precondition(safeFallbackQueries == [NSRange(location: 800, length: 2)])
        precondition(safeFallbackGeometry.primary == nil)
        precondition(safeFallbackGeometry.selectedRange == nil)
        let safeFallbackPlacement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: safeFallbackGeometry.primary,
                retry: safeFallbackGeometry.retry,
                selectedRange: safeFallbackGeometry.selectedRange,
                compositionRects: safeFallbackGeometry.compositionRects,
                defaultAnchor: NSRect(x: 10, y: 10, width: 1, height: 20),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: NSRect(x: 40, y: 40, width: 1, height: 20),
            contentSize: NSSize(width: 200, height: 100)
        )
        precondition(
            safeFallbackPlacement?.resolvedAnchor
                == NSRect(x: 720, y: 220, width: 40, height: 20)
        )

        let caretGeometryResolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 300, length: 4),
            preferredRange: NSRange(location: 302, length: 0),
            selectedRange: NSRange(location: 400, length: 0)
        ) { range in
            switch (range.location, range.length) {
            case (302, 0):
                return CandidateClientRectResult(
                    rect: NSRect(x: 321.5, y: 222.5, width: 0, height: 20),
                    actualRange: range
                )
            case (400, 0):
                return CandidateClientRectResult(
                    rect: NSRect(x: 401.5, y: 122.5, width: 0, height: 18),
                    actualRange: range
                )
            case (300, 4):
                return CandidateClientRectResult(
                    rect: NSRect(x: 100, y: 200, width: 100, height: 20),
                    actualRange: range
                )
            default:
                preconditionFailure("unexpected caret range query \(range)")
            }
        }
        guard case let .complete(caretGeometry) = caretGeometryResolution else {
            preconditionFailure("zero-width caret anchors are valid")
        }
        precondition(
            caretGeometry.primary
                == NSRect(x: 321.5, y: 222.5, width: 1, height: 20)
        )
        precondition(
            caretGeometry.selectedRange
                == NSRect(x: 401.5, y: 122.5, width: 1, height: 18)
        )
        let caretPlacement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: caretGeometry.primary,
                retry: nil,
                selectedRange: caretGeometry.selectedRange,
                compositionRects: caretGeometry.compositionRects,
                defaultAnchor: NSRect(x: 10, y: 10, width: 1, height: 20),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: NSRect(x: 50, y: 50, width: 1, height: 20),
            contentSize: NSSize(width: 200, height: 100)
        )
        precondition(caretPlacement?.resolvedAnchor == caretGeometry.primary)

        let selectedCaretResolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 500, length: 4),
            preferredRange: NSRange(location: 502, length: 0),
            selectedRange: NSRange(location: 600, length: 0)
        ) { range in
            switch (range.location, range.length) {
            case (502, 0):
                return CandidateClientRectResult(
                    rect: NSRect(x: 500, y: 300, width: 0, height: 0),
                    actualRange: range
                )
            case (600, 0):
                return CandidateClientRectResult(
                    rect: NSRect(x: 610, y: 310, width: 0, height: 18),
                    actualRange: range
                )
            case (500, 4):
                return CandidateClientRectResult(
                    rect: NSRect(x: 400, y: 300, width: 100, height: 20),
                    actualRange: range
                )
            default:
                preconditionFailure("unexpected selected caret query \(range)")
            }
        }
        guard case let .complete(selectedCaretGeometry) = selectedCaretResolution else {
            preconditionFailure("selected caret fallback must resolve")
        }
        let selectedCaretPlacement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: selectedCaretGeometry.primary,
                retry: nil,
                selectedRange: selectedCaretGeometry.selectedRange,
                compositionRects: selectedCaretGeometry.compositionRects,
                defaultAnchor: NSRect(x: 10, y: 10, width: 1, height: 20),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: NSRect(x: 50, y: 50, width: 1, height: 20),
            contentSize: NSSize(width: 200, height: 100)
        )
        precondition(
            selectedCaretPlacement?.resolvedAnchor
                == NSRect(x: 610, y: 310, width: 1, height: 18)
        )

        let standardizedGeometryResolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 700, length: 2),
            preferredRange: NSRange(location: 702, length: 0),
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { range in
            switch (range.location, range.length) {
            case (702, 0):
                return CandidateClientRectResult(
                    rect: NSRect(x: 710, y: 330, width: 0, height: -20),
                    actualRange: range
                )
            case (700, 2):
                return CandidateClientRectResult(
                    rect: NSRect(x: 720, y: 300, width: -20, height: 20),
                    actualRange: range
                )
            default:
                preconditionFailure("unexpected negative-size query \(range)")
            }
        }
        guard case let .complete(standardizedGeometry) =
            standardizedGeometryResolution
        else {
            preconditionFailure("SDK negative-size rectangles must standardize")
        }
        precondition(
            standardizedGeometry.primary
                == NSRect(x: 710, y: 310, width: 1, height: 20)
        )
        precondition(
            standardizedGeometry.compositionRects
                == [NSRect(x: 700, y: 300, width: 20, height: 20)]
        )
    }

    // Turning accepted geometry into a panel position: below or above the
    // composition without covering it, clamped to the visible frame. Last,
    // a candidate commit goes through only for an OK status with a view and
    // no structured error; anything else keeps the composition.
    private static func testGeometryPlacementAndCommitPolicy() {
        let visibleFrame = NSRect(x: 0, y: 0, width: 800, height: 600)
        let oldCase1Resolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: NSRange(location: 1_000, length: 9),
            preferredRange: NSRange(location: 1_000, length: 9),
            selectedRange: NSRange(location: NSNotFound, length: 0)
        ) { range in
            CandidateClientRectResult(
                rect: NSRect(x: 1_001, y: 825, width: 72, height: 18),
                actualRange: range
            )
        }
        guard case let .complete(oldCase1Geometry) = oldCase1Resolution
        else {
            preconditionFailure("old case1 logical fragment must resolve")
        }
        precondition(oldCase1Geometry.compositionLineMetrics == [18])
        precondition(
            oldCase1Geometry.compositionExclusionRects
                == [NSRect(x: 983, y: 807, width: 108, height: 54)]
        )
        let oldCase1Placement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: oldCase1Geometry.primary,
                retry: oldCase1Geometry.retry,
                selectedRange: oldCase1Geometry.selectedRange,
                compositionRects: oldCase1Geometry.compositionRects,
                compositionExclusionRects:
                    oldCase1Geometry.compositionExclusionRects,
                compositionLineMetrics:
                    oldCase1Geometry.compositionLineMetrics,
                compositionOrientations:
                    oldCase1Geometry.compositionOrientations,
                markedRangeUTF16Length: 9,
                defaultAnchor: NSRect(
                    x: 24,
                    y: 572,
                    width: 1,
                    height: 20
                ),
                visibleFrame: NSRect(
                    x: 0,
                    y: 90,
                    width: 1_728,
                    height: 964
                ),
                backingScale: 2
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 220, height: 240)
        )
        precondition(
            oldCase1Placement?.frame
                == NSRect(x: 1_001, y: 563, width: 220, height: 240)
        )
        precondition(oldCase1Placement?.frame.maxY == 803)
        precondition(oldCase1Placement?.frame.origin.y != 581)
        precondition(
            oldCase1Geometry.compositionExclusionRects[0].minY
                - (oldCase1Placement?.frame.maxY ?? .infinity)
                >= CandidateSurfacePolicy.panelGap
        )
        let mismatchedEnvelopePlacement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: oldCase1Geometry.primary,
                retry: nil,
                selectedRange: nil,
                compositionRects: oldCase1Geometry.compositionRects,
                compositionExclusionRects:
                    oldCase1Geometry.compositionRects,
                compositionLineMetrics:
                    oldCase1Geometry.compositionLineMetrics,
                compositionOrientations:
                    oldCase1Geometry.compositionOrientations,
                defaultAnchor: NSRect(x: 24, y: 572, width: 1, height: 20),
                visibleFrame: NSRect(
                    x: 0,
                    y: 90,
                    width: 1_728,
                    height: 964
                )
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 220, height: 240)
        )
        precondition(
            mismatchedEnvelopePlacement == nil,
            "raw logical rectangles must never become placement exclusion authority"
        )

        let belowComposition = [
            NSRect(x: 90, y: 390, width: 300, height: 30)
        ]
        let below = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 100, y: 400, width: 20, height: 20),
                retry: nil,
                selectedRange: nil,
                compositionRects: belowComposition,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240, height: 180)
        )
        precondition(below?.frame == NSRect(x: 100, y: 176, width: 240, height: 180))
        precondition(below?.placedAbove == false)
        precondition(
            belowComposition.allSatisfy {
                guard let frame = below?.frame else { return false }
                return !$0.intersects(frame)
            }
        )

        let aboveComposition = [
            NSRect(x: 650, y: 16, width: 120, height: 24)
        ]
        let above = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 700, y: 20, width: 20, height: 20),
                retry: nil,
                selectedRange: nil,
                compositionRects: aboveComposition,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240, height: 180)
        )
        precondition(above?.frame == NSRect(x: 560, y: 68, width: 240, height: 180))
        precondition(above?.placedAbove == true)
        precondition(
            aboveComposition.allSatisfy {
                guard let frame = above?.frame else { return false }
                return !$0.intersects(frame)
            }
        )

        let multilineComposition = [
            NSRect(x: 100, y: 420, width: 280, height: 20),
            NSRect(x: 20, y: 380, width: 180, height: 20),
        ]
        let multiline = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: multilineComposition[0],
                retry: nil,
                selectedRange: nil,
                compositionRects: multilineComposition,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240, height: 180)
        )
        precondition(
            multiline?.frame == NSRect(x: 100, y: 176, width: 240, height: 180)
        )
        precondition(
            multilineComposition.allSatisfy {
                guard let frame = multiline?.frame else { return false }
                return !$0.intersects(frame)
            }
        )

        let tightComposition = [
            NSRect(x: 50, y: 250, width: 500, height: 100)
        ]
        let tight = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 100, y: 280, width: 20, height: 20),
                retry: nil,
                selectedRange: nil,
                compositionRects: tightComposition,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240, height: 500)
        )
        precondition(tight?.frame == NSRect(x: 100, y: 0, width: 240, height: 146))
        precondition(tight?.placedAbove == false)
        precondition(
            tightComposition.allSatisfy {
                guard let frame = tight?.frame else { return false }
                return !$0.intersects(frame)
            }
        )

        let noSafePlacement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 100, y: 280, width: 20, height: 20),
                retry: nil,
                selectedRange: nil,
                compositionRects: [
                    NSRect(x: 1, y: 1, width: 799, height: 599)
                ],
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240, height: 180)
        )
        precondition(noSafePlacement == nil)

        let invalid = NSRect.zero
        let fallback = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: invalid,
                retry: invalid,
                selectedRange: NSRect(x: 40, y: 300, width: 10, height: 20),
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: NSRect(x: 60, y: 320, width: 10, height: 20),
            contentSize: NSSize(width: 240, height: 180)
        )
        precondition(fallback?.resolvedAnchor == NSRect(x: 40, y: 300, width: 10, height: 20))
        precondition(fallback?.cacheableAnchor == NSRect(x: 40, y: 300, width: 10, height: 20))

        let retryFallback = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect.zero,
                retry: NSRect(x: 80, y: 360, width: 10, height: 20),
                selectedRange: nil,
                defaultAnchor: NSRect(x: 10, y: 10, width: 10, height: 10),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240, height: 180)
        )
        precondition(
            retryFallback?.resolvedAnchor
                == NSRect(x: 80, y: 360, width: 10, height: 20)
        )

        let defaultFallback = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 900, y: 900, width: 10, height: 20),
                retry: nil,
                selectedRange: nil,
                defaultAnchor: NSRect(x: 10, y: 300, width: 10, height: 20),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240, height: 180)
        )
        precondition(
            defaultFallback?.resolvedAnchor
                == NSRect(x: 10, y: 300, width: 10, height: 20)
        )

        let screens = [
            CandidateScreenFrame(
                frame: NSRect(x: -1920, y: 0, width: 1920, height: 1080),
                visibleFrame: NSRect(
                    x: -1920,
                    y: 24,
                    width: 1920,
                    height: 1056
                ),
                backingScale: 2
            ),
            CandidateScreenFrame(
                frame: NSRect(x: 0, y: 0, width: 1440, height: 900),
                visibleFrame: NSRect(x: 0, y: 24, width: 1440, height: 852)
            ),
        ]
        precondition(
            CandidateSurfacePolicy.visibleFrame(
                for: NSRect(x: -100, y: 400, width: 10, height: 20),
                screens: screens,
                mainVisibleFrame: screens[1].visibleFrame
            ) == screens[0].visibleFrame
        )
        precondition(
            CandidateSurfacePolicy.selectedScreen(
                for: NSRect(x: -100, y: 400, width: 10, height: 20),
                screens: screens,
                mainScreen: screens[1]
            ) == screens[0]
        )
        precondition(
            CandidateSurfacePolicy.visibleFrame(
                for: NSRect(x: 2500, y: 400, width: 10, height: 20),
                screens: screens,
                mainVisibleFrame: screens[0].visibleFrame
            ) == screens[1].visibleFrame
        )
        precondition(
            CandidateSurfacePolicy.visibleFrame(
                for: nil,
                screens: screens,
                mainVisibleFrame: screens[1].visibleFrame
            ) == screens[1].visibleFrame
        )

        let fractionalVisibleFrame = NSRect(
            x: -1920,
            y: 23.5,
            width: 1920,
            height: 1056.5
        )
        let fractionalComposition = [
            NSRect(x: -101.5, y: 100.5, width: 81, height: 19.5)
        ]
        let fractional = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(
                    x: -100.5,
                    y: 101.5,
                    width: 1,
                    height: 18.5
                ),
                retry: nil,
                selectedRange: nil,
                compositionRects: fractionalComposition,
                defaultAnchor: NSRect(
                    x: -1896,
                    y: 400,
                    width: 1,
                    height: 20
                ),
                visibleFrame: fractionalVisibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 240.5, height: 180.5)
        )
        precondition(
            fractional?.frame
                == NSRect(
                    x: -240.5,
                    y: 144,
                    width: 240.5,
                    height: 180.5
                )
        )
        precondition(
            fractional.map { fractionalVisibleFrame.contains($0.frame) }
                == true
        )
        precondition(
            fractionalComposition.allSatisfy {
                guard let frame = fractional?.frame else { return false }
                return !$0.intersects(frame)
            }
        )
        let pixelLogicalRect = NSRect(
            x: 100.3,
            y: 100.3,
            width: 80.2,
            height: 19.4
        )
        let pixelEnvelope =
            CandidateSurfacePolicy.visualExclusionEnvelope(
                logicalRect: pixelLogicalRect,
                orientation: .horizontal,
                lineMetric: 19.4
            )!
        func pixelAlignedPlacement(scale: CGFloat) -> CandidatePanelGeometry? {
            CandidateSurfacePolicy.geometry(
                anchors: CandidateSurfaceAnchors(
                    primary: pixelLogicalRect,
                    retry: nil,
                    selectedRange: nil,
                    compositionRects: [pixelLogicalRect],
                    compositionExclusionRects: [pixelEnvelope],
                    compositionLineMetrics: [19.4],
                    compositionOrientations: [.horizontal],
                    defaultAnchor: NSRect(
                        x: 10,
                        y: 300,
                        width: 1,
                        height: 20
                    ),
                    visibleFrame: visibleFrame,
                    backingScale: scale
                ),
                lastValidAnchor: nil,
                contentSize: NSSize(width: 200, height: 50)
            )
        }
        let pixelAligned1x = pixelAlignedPlacement(scale: 1)
        let pixelAligned2x = pixelAlignedPlacement(scale: 2)
        precondition(pixelAligned1x?.frame.maxY == 76)
        precondition(pixelAligned2x?.frame.maxY == 76.5)
        precondition(
            floor(pixelEnvelope.minY) - (pixelAligned1x?.frame.maxY ?? .infinity)
                >= CandidateSurfacePolicy.panelGap
        )
        precondition(
            floor(pixelEnvelope.minY * 2) / 2
                - (pixelAligned2x?.frame.maxY ?? .infinity)
                >= CandidateSurfacePolicy.panelGap
        )

        let clampedOversize = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: NSRect(x: 10, y: 300, width: 10, height: 20),
                retry: nil,
                selectedRange: nil,
                defaultAnchor: NSRect(x: 10, y: 300, width: 10, height: 20),
                visibleFrame: visibleFrame
            ),
            lastValidAnchor: nil,
            contentSize: NSSize(width: 2_000, height: 2_000)
        )
        precondition(
            clampedOversize?.frame
                == NSRect(x: 0, y: 0, width: 800, height: 276)
        )

        precondition(
            CandidateSurfacePolicy.candidateCommitAction(
                statusOK: false,
                hasView: true,
                hasStructuredError: false
            ) == .preserveComposition
        )
        precondition(
            CandidateSurfacePolicy.candidateCommitAction(
                statusOK: true,
                hasView: false,
                hasStructuredError: false
            ) == .preserveComposition
        )
        precondition(
            CandidateSurfacePolicy.candidateCommitAction(
                statusOK: true,
                hasView: true,
                hasStructuredError: true
            ) == .preserveComposition
        )
        precondition(
            CandidateSurfacePolicy.candidateCommitAction(
                statusOK: true,
                hasView: true,
                hasStructuredError: false
            ) == .commit
        )
    }

    private static func candidateFixture() -> CandidateSurfaceSnapshot {
        CandidateSurfaceSnapshot(
            items: [
                CandidateSurfaceItem(id: "a", text: "候補A", annotation: nil),
                CandidateSurfaceItem(id: "b", text: "候補B", annotation: "名詞"),
            ],
            selectedIndex: 0,
            generation: 7
        )
    }
}
