import AppKit

// Value-based geometry rules. The caller supplies bounded observations; no
// client, window, previous application or display state is kept here.
extension CandidateSurfacePolicy {
    enum AttributeIndexSpace: String, CaseIterable { case inline, document }
    enum AttributeSelectionReason: String {
        case inlineOnly = "inline_only", documentOnly = "document_only", coincident
        case inconsistent, ambiguous, exception, exceptionFallback = "exception_fallback", singlePoint = "single_point"
        case invalidRange = "invalid_range", noUsableAnchor = "no_usable_anchor"
    }

    // Production reads only `anchors`. The other fields record why and how a
    // selection was made, for the characterization tests.
    struct AttributeSelection {
        let anchors: CandidateSurfaceAnchors?
        let index: Int?
        let space: AttributeIndexSpace?
        let reason: AttributeSelectionReason
        let observation: MuuClientLineGeometry?
        let omittedCaretEndpoint: Bool
        let queryCount: Int
    }

    /// Both spaces must be tested rather than inferred from the application.
    /// A failed/ambiguous comparison retains the previously visible inline-zero
    /// point without claiming that it is the composition's start (Discord).
    static func attributeFallbackSelection(
        markedRange: NSRange, focusedStart: Int?, caret: Int,
        screens: [CandidateScreenFrame],
        observe: (Int) -> MuuClientLineGeometry
    ) -> AttributeSelection {
        var values: [Int: MuuClientLineGeometry] = [:]
        func result(_ anchors: CandidateSurfaceAnchors?, index: Int?, space: AttributeIndexSpace?,
                    reason: AttributeSelectionReason, omittedEndpoint: Bool = false) -> AttributeSelection {
            AttributeSelection(anchors: anchors, index: index, space: space, reason: reason,
                observation: index.flatMap { values[$0] }, omittedCaretEndpoint: omittedEndpoint,
                queryCount: values.count)
        }
        guard markedRange.location != NSNotFound, markedRange.location >= 0,
              markedRange.length > 0, markedRange.length <= Int.max - markedRange.location
        else { return result(nil, index: nil, space: nil, reason: .invalidRange) }
        // Preserve the known fallback before asking a client about optional
        // indices. Only failure at inline zero itself loses that fallback.
        let initial = observe(0)
        values[0] = initial
        if initial.exception.boolValue {
            return result(nil, index: 0, space: nil, reason: .exception)
        }
        let inlineZero = attributeFallbackAnchors(initial,
            markedRangeUTF16Length: markedRange.length, screens: screens)
        func index(_ offset: Int, in space: AttributeIndexSpace) -> Int? {
            guard offset >= 0, offset <= markedRange.length, offset != NSNotFound else { return nil }
            switch space {
            case .inline: return offset
            case .document:
                let sum = markedRange.location.addingReportingOverflow(offset)
                return !sum.overflow && sum.partialValue != NSNotFound ? sum.partialValue : nil
            }
        }
        // At most six distinct indices. Overlapping hypotheses share a read.
        // All inline indices are read before any document index, so an
        // exception at an inline index ends the loop before document space
        // is tried.
        let offsets = [0] + (focusedStart.map { [$0] } ?? []) + [caret]
        for space in AttributeIndexSpace.allCases {
            for offset in offsets {
                guard let i = index(offset, in: space), values[i] == nil else { continue }
                let value = observe(i)
                values[i] = value
                // An unsupported optional index must not erase a working
                // inline-zero anchor. Stop immediately and adopt no space.
                if value.exception.boolValue {
                    return result(inlineZero, index: 0, space: nil,
                        reason: inlineZero == nil ? .noUsableAnchor : .exceptionFallback)
                }
            }
        }
        var valid: [Int: CandidateSurfaceAnchors] = [:]
        for (i, value) in values {
            valid[i] = attributeFallbackAnchors(value,
                markedRangeUTF16Length: markedRange.length, screens: screens)
        }
        struct AlignedPoints {
            let points: [(offset: Int, anchors: CandidateSurfaceAnchors)]
            let omittedEndpoint: Bool
        }
        func alignedPoints(_ space: AttributeIndexSpace) -> AlignedPoints? {
            guard let focusedStart, focusedStart >= 0, focusedStart < markedRange.length,
                  caret >= 0, caret <= markedRange.length else { return nil }
            var points: [(offset: Int, anchors: CandidateSurfaceAnchors)] = []
            var omittedEndpoint = false
            for offset in Set([0, focusedStart, caret]).sorted() {
                if let i = index(offset, in: space), let anchors = valid[i] {
                    points.append((offset, anchors))
                } else if offset == caret, caret == markedRange.length {
                    // Measured clients do not always implement this endpoint.
                    // Only that invalid endpoint may be omitted; an in-range
                    // caret or a valid point on another line must still reject.
                    omittedEndpoint = true
                } else { return nil }
            }
            // With focus on the first clause and the endpoint omitted, only
            // offset 0 remains. One point cannot confirm a space; the caller
            // requires two and reports `single_point` with inline 0, which
            // is that same point for inline clients.
            guard let first = points.first,
                  let origin = first.anchors.primary else { return nil }
            let tolerance = 1 / first.anchors.backingScale
            var maximumX = origin.minX
            for point in points {
                guard let line = point.anchors.primary,
                      point.anchors.visibleFrame == first.anchors.visibleFrame,
                      point.anchors.backingScale == first.anchors.backingScale,
                      abs(line.minY - origin.minY) <= tolerance,
                      abs(line.height - origin.height) <= tolerance,
                      line.minX + tolerance >= maximumX else { return nil }
                // Compare against the prefix maximum so small adjacent
                // reversals cannot accumulate beyond one physical pixel.
                maximumX = max(maximumX, line.minX)
            }
            return AlignedPoints(points: points, omittedEndpoint: omittedEndpoint)
        }
        let inlinePoints = alignedPoints(.inline), documentPoints = alignedPoints(.document)
        let inline = inlinePoints.flatMap { $0.points.count >= 2 ? $0 : nil }
        let document = documentPoints.flatMap { $0.points.count >= 2 ? $0 : nil }
        let space: AttributeIndexSpace
        let chosen: AlignedPoints
        let reason: AttributeSelectionReason
        switch (inline, document) {
        case let (a?, nil): (space, chosen, reason) = (.inline, a, .inlineOnly)
        case let (nil, b?): (space, chosen, reason) = (.document, b, .documentOnly)
        case let (a?, b?):
            // Equal query numbers at document offset zero cannot identify the
            // client's contract. Equivalent positions can still be used safely.
            guard a.points.count == b.points.count,
                  zip(a.points, b.points).allSatisfy({
                      $0.offset == $1.offset && $0.anchors.primary == $1.anchors.primary
                          && $0.anchors.visibleFrame == $1.anchors.visibleFrame
                          && $0.anchors.backingScale == $1.anchors.backingScale
                  }) else {
                return result(valid[0], index: 0, space: nil,
                    reason: valid[0] == nil ? .noUsableAnchor : .ambiguous)
            }
            (space, chosen, reason) = (.inline, a, .coincident)
        case (nil, nil):
            // `valid` is keyed by the numeric argument, so 0 is inline offset
            // 0 (and also document offset 0 when the composition starts there).
            // Only the inline side is asked for `single_point`. A lone
            // document point with an inconsistent inline side is reported
            // as `inconsistent`.
            return result(valid[0], index: 0, space: nil,
                reason: valid[0] == nil ? .noUsableAnchor
                    : inlinePoints?.points.count == 1 ? .singlePoint : .inconsistent)
        }
        // This should not fail once a space is accepted. focusedStart is
        // below markedRange.length, so it can never be the omitted endpoint,
        // and alignedPoints already required it to be valid.
        guard let focusedStart, let selectedIndex = index(focusedStart, in: space),
              let focus = valid[selectedIndex], let origin = chosen.points.first?.anchors
        else { return result(valid[0], index: 0, space: nil, reason: .inconsistent) }
        // Keep the first line as the exclusion region. Only the horizontal
        // anchor follows focus, after all accepted points passed the row check.
        let anchors = CandidateSurfaceAnchors(primary: focus.primary, retry: nil, selectedRange: nil,
            compositionRects: origin.compositionRects, compositionExclusionRects: origin.compositionExclusionRects,
            compositionLineMetrics: origin.compositionLineMetrics, compositionOrientations: origin.compositionOrientations,
            markedRangeUTF16Length: markedRange.length, compositionRectsComplete: false,
            defaultAnchor: origin.defaultAnchor, visibleFrame: origin.visibleFrame,
            backingScale: origin.backingScale, prefersAbove: true)
        return result(anchors, index: selectedIndex, space: space, reason: reason,
            omittedEndpoint: chosen.omittedEndpoint)
    }

    /// Normalize an attributes line without assuming which index space it uses.
    /// In the measured legacy wrapper the origin is one line below firstRect: Notes reports
    /// y=839,h=23 versus firstRect y=862. Use the top of that returned box as
    /// the line origin; do not flip screen coordinates or add document offsets.
    /// Index semantics and focus following are checked by attributeFallbackSelection.
    static func attributeFallbackAnchors(
        _ observation: MuuClientLineGeometry,
        markedRangeUTF16Length: Int,
        screens: [CandidateScreenFrame]
    ) -> CandidateSurfaceAnchors? {
        // Only horizontal geometry was confirmed. Unknown/vertical direction
        // and NSException fail closed; the badge never calls this adapter.
        // IMKTextOrientationName is a BOOL in which 1 means horizontal and 0
        // vertical (IMKInputSession.h). This is the reverse of
        // NSTextLayoutOrientation, where horizontal is 0.
        guard !observation.exception.boolValue,
              observation.returnedAttributes.boolValue,
              observation.orientation == 1,
              markedRangeUTF16Length > 0
        else { return nil }
        let raw = observation.lineRect
        guard [raw.origin.x, raw.origin.y, raw.size.width, raw.size.height]
                .allSatisfy({ $0.isFinite && ($0.isZero || $0.isNormal) }),
              raw.size.width > 0, raw.size.height > 0
        else { return nil }
        // The one-line offset was measured where both answers exist (Notes).
        // In the clients this path serves, firstRect never succeeds, so the
        // same relation is assumed there rather than observed.
        let line = NSRect(x: raw.origin.x, y: raw.origin.y + raw.size.height,
            width: raw.size.width, height: raw.size.height)
        guard CandidateSurfacePolicy.isValidAnchor(line),
              let screen = screens.first(where: {
                  CandidateSurfacePolicy.isValidAnchor($0.frame)
                      && CandidateSurfacePolicy.isValidAnchor($0.visibleFrame)
                      && $0.backingScale.isFinite && $0.backingScale > 0
                      && raw.size.width >= 1 / $0.backingScale
                      && raw.size.height >= 1 / $0.backingScale
                      && $0.frame.contains(raw) && $0.visibleFrame.contains(line)
              }),
              let exclusion = CandidateSurfacePolicy.visualExclusionEnvelope(
                  logicalRect: line, orientation: .horizontal, lineMetric: line.height)
        else { return nil }
        // This one line (with the existing visual margin) is the only known
        // exclusion region. It is not a claim of complete fragment coverage.
        // Prefer above this first line to avoid later wrapped lines. Below is
        // the fallback when above cannot fit; later-line overlap is then not
        // guaranteed to be avoided. The existing 4pt gap is unchanged.
        return CandidateSurfaceAnchors(primary: line, retry: nil, selectedRange: nil,
            compositionRects: [line], compositionExclusionRects: [exclusion],
            compositionLineMetrics: [line.height], compositionOrientations: [.horizontal],
            markedRangeUTF16Length: markedRangeUTF16Length, compositionRectsComplete: false,
            defaultAnchor: line, visibleFrame: screen.visibleFrame, backingScale: screen.backingScale,
            prefersAbove: true)
    }

}
