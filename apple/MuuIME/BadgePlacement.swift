import AppKit

struct ModeBadgeGeometry: Equatable {
    let anchor: NSRect
    let frame: NSRect
    let placedAbove: Bool
}

/// Badges require an exact caret and complete firstRect fragments. They never
/// borrow candidate fallback anchors or the incomplete attributes line geometry.
/// The read closures keep the old order and number of client queries:
/// selectedRange only without preedit, markedRange only with it, then the
/// caret rect and, with preedit, the fragment walk.
enum BadgePlacement {
    static func geometry(
        view: ViewState,
        screens: [CandidateScreenFrame],
        contentSize: NSSize,
        readSelectedRange: () -> NSRange,
        readMarkedRange: () -> NSRange,
        readOrientation: (Int) -> CandidateTextOrientation,
        readRect: (NSRange) -> CandidateClientRectResult
    ) -> ModeBadgeGeometry? {
        let requestedRange: NSRange
        let markedRange: NSRange?
        if view.preedit.isEmpty {
            let selectedRange = readSelectedRange()
            guard isValidDocumentRange(selectedRange),
                  selectedRange.length == 0
            else {
                return nil
            }
            requestedRange = selectedRange
            markedRange = nil
        } else {
            let clientMarkedRange = readMarkedRange()
            guard isValidDocumentRange(clientMarkedRange),
                  view.caretUTF16 >= 0,
                  view.caretUTF16 <= clientMarkedRange.length,
                  view.caretUTF16 <= Int.max - clientMarkedRange.location
            else {
                return nil
            }
            // Rust's caret is relative to the preedit; the client's marked
            // range says where the preedit starts in the document.
            requestedRange = NSRange(
                location: clientMarkedRange.location + view.caretUTF16,
                length: 0
            )
            markedRange = clientMarkedRange
        }

        let first = readRect(requestedRange)
        let actualRange = first.actualRange
        let rawRect = first.rect
        guard actualRange == requestedRange,
              let anchor = normalizedBoundaryRect(rawRect),
              contentSize.width.isFinite,
              contentSize.height.isFinite,
              contentSize.width > 0,
              contentSize.height > 0,
              let screen = containingScreen(for: anchor, screens: screens)
        else {
            return nil
        }

        let compositionRects: [NSRect]
        let compositionExclusionRects: [NSRect]
        let compositionLineMetrics: [CGFloat]
        let compositionOrientations: [CandidateTextOrientation]
        if let markedRange {
            let resolution = CandidateSurfacePolicy.clientGeometry(
                markedRange: markedRange,
                preferredRange: nil,
                selectedRange: NSRange(location: NSNotFound, length: 0),
                orientationAt: readOrientation
            ) { readRect($0) }
            guard case let .complete(compositionGeometry) = resolution,
                  compositionGeometry.compositionRectsComplete,
                  !compositionGeometry.compositionExclusionRects.isEmpty
            else {
                return nil
            }
            compositionRects = compositionGeometry.compositionRects
            compositionExclusionRects = compositionGeometry.compositionExclusionRects
            compositionLineMetrics = compositionGeometry.compositionLineMetrics
            compositionOrientations = compositionGeometry.compositionOrientations
        } else {
            compositionRects = []
            compositionExclusionRects = []
            compositionLineMetrics = []
            compositionOrientations = []
        }

        guard let placement = CandidateSurfacePolicy.geometry(
            anchors: CandidateSurfaceAnchors(
                primary: anchor,
                retry: nil,
                selectedRange: nil,
                compositionRects: compositionRects,
                compositionExclusionRects: compositionExclusionRects,
                compositionLineMetrics: compositionLineMetrics,
                compositionOrientations: compositionOrientations,
                markedRangeUTF16Length: markedRange?.length ?? 0,
                compositionRectsComplete: true,
                defaultAnchor: anchor,
                visibleFrame: screen.visibleFrame,
                backingScale: screen.backingScale
            ),
            lastValidAnchor: nil,
            contentSize: contentSize
        ),
              // A badge is never clipped: if the full size does not fit, it is
              // not shown.
              placement.frame.size == contentSize
        else {
            return nil
        }
        return ModeBadgeGeometry(
            anchor: anchor,
            frame: placement.frame,
            placedAbove: placement.placedAbove
        )
    }

    // A caret rect at least one point tall. A zero-width caret is widened to
    // one point; a sliver under half a point wide is refused.
    private static func normalizedBoundaryRect(_ rect: NSRect) -> NSRect? {
        guard rect.origin.x.isFinite,
              rect.origin.y.isFinite,
              rect.width.isFinite,
              rect.height.isFinite,
              rect.height >= 1,
              (rect.width == 0 || rect.width >= 0.5)
        else {
            return nil
        }
        var normalized = rect.standardized
        guard normalized.height > 0 else { return nil }
        if normalized.width == 0 {
            normalized.size.width = 1
        }
        return normalized.width > 0 ? normalized : nil
    }

    // Unlike the candidate panel, there is no nearest-screen or main-screen
    // fallback: the caret must be on a screen that shows it.
    private static func containingScreen(
        for anchor: NSRect,
        screens: [CandidateScreenFrame]
    ) -> CandidateScreenFrame? {
        let center = NSPoint(x: anchor.midX, y: anchor.midY)
        return screens.first {
            isValidFrame($0.frame)
                && isValidFrame($0.visibleFrame)
                && $0.backingScale.isFinite
                && $0.backingScale > 0
                && $0.frame.contains(center)
                && $0.visibleFrame.intersects(anchor)
        }
    }

    private static func isValidDocumentRange(_ range: NSRange) -> Bool {
        guard range.location >= 0,
              range.length >= 0,
              range.location != NSNotFound
        else {
            return false
        }
        return !range.location.addingReportingOverflow(range.length).overflow
    }

    private static func isValidFrame(_ rect: NSRect) -> Bool {
        rect.origin.x.isFinite
            && rect.origin.y.isFinite
            && rect.width.isFinite
            && rect.height.isFinite
            && rect.width > 0
            && rect.height > 0
    }
}

