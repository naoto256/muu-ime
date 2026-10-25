import AppKit

// Namespace for the panel's pure decisions (geometry, actions, updates), kept
// free of AppKit state so the policy tests can run them without a window.
enum CandidateSurfacePolicy {}

struct CandidateSurfaceItem: Equatable {
    let id: String
    let text: String
    let annotation: String?
}

// One rendered list. `generation` changes when the candidate identities change
// or actions are invalidated; an action is tied to the generation it came from.
struct CandidateSurfaceSnapshot: Equatable {
    let items: [CandidateSurfaceItem]
    let selectedIndex: Int
    let generation: UInt64

    var selectedID: String? {
        items.indices.contains(selectedIndex) ? items[selectedIndex].id : nil
    }
}

enum CandidateSurfaceAction: Equatable {
    case select(id: String, generation: UInt64)
    case selectAndCommit(id: String, generation: UInt64)
}

struct CandidateSurfaceUpdate: Equatable {
    let reloadList: Bool
    let updateSelection: Bool
    let show: Bool
}

// Everything the geometry policy needs for one placement, in screen
// coordinates. The four composition arrays run in parallel, one entry per
// line fragment of the marked text; the policy refuses anchors whose arrays
// disagree. `compositionRectsComplete` is false when only part of the
// composition's geometry is known (a caret or the first line).
struct CandidateSurfaceAnchors: Equatable {
    let primary: NSRect?
    let retry: NSRect?
    let selectedRange: NSRect?
    let compositionRects: [NSRect]
    let compositionExclusionRects: [NSRect]
    let compositionLineMetrics: [CGFloat]
    let compositionOrientations: [CandidateTextOrientation]
    let markedRangeUTF16Length: Int
    let compositionRectsComplete: Bool
    // Only the attributes fallback knows just the first line of a possibly
    // wrapped composition. Other geometry retains the usual below-first order.
    let prefersAbove: Bool
    let defaultAnchor: NSRect
    let visibleFrame: NSRect
    let backingScale: CGFloat

    init(
        primary: NSRect?,
        retry: NSRect?,
        selectedRange: NSRect?,
        compositionRects: [NSRect],
        compositionExclusionRects: [NSRect],
        compositionLineMetrics: [CGFloat],
        compositionOrientations: [CandidateTextOrientation],
        markedRangeUTF16Length: Int = 0,
        compositionRectsComplete: Bool = true,
        defaultAnchor: NSRect,
        visibleFrame: NSRect,
        backingScale: CGFloat = 1,
        prefersAbove: Bool = false
    ) {
        self.primary = primary
        self.retry = retry
        self.selectedRange = selectedRange
        self.compositionRects = compositionRects
        self.compositionExclusionRects = compositionExclusionRects
        self.compositionLineMetrics = compositionLineMetrics
        self.compositionOrientations = compositionOrientations
        self.markedRangeUTF16Length = markedRangeUTF16Length
        self.compositionRectsComplete = compositionRectsComplete
        self.prefersAbove = prefersAbove
        self.defaultAnchor = defaultAnchor
        self.visibleFrame = visibleFrame
        self.backingScale = backingScale
    }

}

enum CandidateTextOrientation: String, Equatable {
    case horizontal
    case vertical
}

struct CandidateLogicalFragment: Equatable {
    let rect: NSRect
    let actualRange: NSRange
    let orientation: CandidateTextOrientation
    let lineMetric: CGFloat
}

struct CandidateClientRectResult {
    let rect: NSRect
    let actualRange: NSRange
}

struct CandidateClientGeometry {
    let primary: NSRect?
    let retry: NSRect?
    let selectedRange: NSRect?
    let compositionFragments: [CandidateLogicalFragment]
    let compositionExclusionRects: [NSRect]
    let compositionRectsComplete: Bool

    var compositionRects: [NSRect] {
        compositionFragments.map(\.rect)
    }

    var compositionLineMetrics: [CGFloat] {
        compositionFragments.map(\.lineMetric)
    }

    var compositionOrientations: [CandidateTextOrientation] {
        compositionFragments.map(\.orientation)
    }
}

enum CandidateClientGeometryResolution {
    case complete(CandidateClientGeometry)
    case failure
}

// `cacheableAnchor` is nil when the frame rests only on the cached or the
// default anchor; the panel keeps its earlier cached anchor in that case.
struct CandidatePanelGeometry: Equatable {
    let frame: NSRect
    let resolvedAnchor: NSRect
    let cacheableAnchor: NSRect?
    let placedAbove: Bool
}

struct CandidateScreenFrame: Equatable {
    let frame: NSRect
    let visibleFrame: NSRect
    let backingScale: CGFloat

    init(
        frame: NSRect,
        visibleFrame: NSRect,
        backingScale: CGFloat = 1
    ) {
        self.frame = frame
        self.visibleFrame = visibleFrame
        self.backingScale = backingScale
    }
}

enum CandidateCommitAction: Equatable {
    case commit
    case preserveComposition
}
