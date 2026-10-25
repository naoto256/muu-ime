import AppKit

extension CandidateSurfacePolicy {
    static let panelGap: CGFloat = 4

    /// The panel frame for one render, or nil to hide. The anchor is the first
    /// usable one of: the preferred-range rect, its retry, the selected-range
    /// rect, the first composition rect, the last valid anchor, the default.
    /// The panel goes below the composition when it fits, otherwise above,
    /// otherwise on the side with more room, clipped to it (`prefersAbove`
    /// reverses the first choice; see below). It never overlaps
    /// any composition line (each widened by one line height) and keeps
    /// `panelGap` from them. Inconsistent anchors, an impossible frame or no
    /// room at all give nil rather than a panel over the text.
    static func geometry(
        anchors: CandidateSurfaceAnchors,
        lastValidAnchor: NSRect?,
        contentSize: NSSize
    ) -> CandidatePanelGeometry? {
        guard isValidFrame(anchors.visibleFrame),
              anchors.backingScale.isFinite,
              anchors.backingScale > 0,
              contentSize.width.isFinite,
              contentSize.height.isFinite,
              contentSize.width > 0,
              contentSize.height > 0
        else {
            return nil
        }

        let isUsableAnchor: (NSRect) -> Bool = {
            isValidAnchor($0) && anchors.visibleFrame.intersects($0)
        }
        guard anchors.compositionRects.count
                == anchors.compositionExclusionRects.count,
              anchors.compositionRects.count
                == anchors.compositionLineMetrics.count,
              anchors.compositionRects.count
                == anchors.compositionOrientations.count,
              anchors.compositionLineMetrics.allSatisfy({
                  $0.isFinite && $0 > 0
              }),
              anchors.compositionExclusionRects.allSatisfy(isValidAnchor),
              anchors.compositionRects.indices.allSatisfy({
                  let rect = anchors.compositionRects[$0]
                  let orientation = anchors.compositionOrientations[$0]
                  let metric = anchors.compositionLineMetrics[$0]
                  return lineMetric(for: rect, orientation: orientation)
                          == metric
                      && visualExclusionEnvelope(
                          logicalRect: rect,
                          orientation: orientation,
                          lineMetric: metric
                      ) == anchors.compositionExclusionRects[$0]
              })
        else {
            return nil
        }
        let validCompositionRects = anchors.compositionRects.filter(isValidAnchor)
        // Only an anchor from this response may be cached for later; the
        // cached and default anchors never re-cache themselves.
        let currentValidAnchor = [
            anchors.primary,
            anchors.retry,
            anchors.selectedRange,
            validCompositionRects.first,
        ].compactMap { $0 }.first(where: isUsableAnchor)
        let resolvedAnchor =
            [
                anchors.primary,
                anchors.retry,
                anchors.selectedRange,
                validCompositionRects.first,
                lastValidAnchor,
            ]
            .compactMap { $0 }
            .first(where: isUsableAnchor)
            ?? anchors.defaultAnchor
        guard isUsableAnchor(resolvedAnchor) else { return nil }

        let visible = anchors.visibleFrame
        // With no composition rects (only the end-of-marked-text caret was
        // available), the anchor's own line stands in for the composition.
        let exclusionRects: [NSRect]
        if anchors.compositionExclusionRects.isEmpty {
            guard let fallbackMetric = lineMetric(
                for: resolvedAnchor,
                orientation: .horizontal
            ),
                  let fallbackEnvelope = visualExclusionEnvelope(
                      logicalRect: resolvedAnchor,
                      orientation: .horizontal,
                      lineMetric: fallbackMetric
                  )
            else {
                return nil
            }
            exclusionRects = [
                outwardPixelAligned(
                    fallbackEnvelope,
                    scale: anchors.backingScale
                )
            ]
        } else {
            exclusionRects = anchors.compositionExclusionRects.map {
                outwardPixelAligned($0, scale: anchors.backingScale)
            }
        }
        guard let exclusionMinY = exclusionRects.map(\.minY).min(),
              let exclusionMaxY = exclusionRects.map(\.maxY).max()
        else {
            return nil
        }
        let belowTop = pixelFloor(
            min(exclusionMinY - panelGap, visible.maxY),
            scale: anchors.backingScale
        )
        let aboveBottom = pixelCeil(
            max(exclusionMaxY + panelGap, visible.minY),
            scale: anchors.backingScale
        )
        let belowCapacity = max(0, belowTop - visible.minY)
        let aboveCapacity = max(0, visible.maxY - aboveBottom)

        let placedAbove: Bool
        let availableHeight: CGFloat
        if anchors.prefersAbove {
            // The attributes fallback knows only the first horizontal line.
            // Prefer above so the panel cannot cover later wrapped lines.
            // If the full panel does not fit above, use below (with the usual
            // height limit); overlap with unknown later lines is then possible.
            // When below has no space at all, keep the usable clipped upper
            // panel rather than hide solely because its full height cannot fit.
            placedAbove = contentSize.height <= aboveCapacity || belowCapacity == 0
            availableHeight = placedAbove ? aboveCapacity : belowCapacity
        } else if contentSize.height <= belowCapacity {
            placedAbove = false
            availableHeight = belowCapacity
        } else if contentSize.height <= aboveCapacity {
            placedAbove = true
            availableHeight = aboveCapacity
        } else if belowCapacity >= aboveCapacity {
            placedAbove = false
            availableHeight = belowCapacity
        } else {
            placedAbove = true
            availableHeight = aboveCapacity
        }
        guard availableHeight > 0 else { return nil }

        let panelSize = NSSize(
            width: min(contentSize.width, visible.width),
            height: min(contentSize.height, availableHeight)
        )
        let maximumX = visible.maxX - panelSize.width
        let x = clamp(resolvedAnchor.minX, minimum: visible.minX, maximum: maximumX)
        let y =
            placedAbove
            ? aboveBottom
            : belowTop - panelSize.height
        let frame = NSRect(origin: NSPoint(x: x, y: y), size: panelSize)
        guard visible.contains(frame),
              exclusionRects.allSatisfy({ !$0.intersects(frame) }),
              exclusionRects.allSatisfy({
                  placedAbove
                      ? frame.minY - $0.maxY >= panelGap
                      : $0.minY - frame.maxY >= panelGap
              })
        else {
            return nil
        }

        return CandidatePanelGeometry(
            frame: frame,
            resolvedAnchor: resolvedAnchor,
            cacheableAnchor: currentValidAnchor,
            placedAbove: placedAbove
        )
    }

    /// Asks the client for the geometry of the marked text. The marked range is
    /// walked with firstRect one line fragment at a time; every fragment must
    /// start where the last ended and stay inside the range, or the result is
    /// a failure. A client that answers the first fragment with an empty
    /// range (it has no rect for marked text) is asked once for the caret at
    /// the marked end instead, and that caret becomes the only anchor. A
    /// client that returns a different actual range for the preferred range
    /// is asked again for that actual range (the retry anchor).
    static func clientGeometry(
        markedRange: NSRange,
        preferredRange: NSRange?,
        selectedRange: NSRange,
        orientationAt: (Int) -> CandidateTextOrientation = { _ in .horizontal },
        firstRect: (NSRange) -> CandidateClientRectResult
    ) -> CandidateClientGeometryResolution {
        let compositionFragments: [CandidateLogicalFragment]
        let compositionRectsComplete: Bool
        let fragmentFallbackAnchor: NSRect?
        switch logicalFragments(
            in: markedRange,
            orientationAt: orientationAt,
            firstRect: firstRect
        ) {
        case let .complete(fragments):
            compositionFragments = fragments
            compositionRectsComplete = true
            fragmentFallbackAnchor = nil
        case .unavailable:
            guard let markedEnd = documentRangeEnd(markedRange) else {
                return .failure
            }
            let boundaryRange = NSRange(location: markedEnd, length: 0)
            let result = firstRect(boundaryRange)
            guard result.actualRange == boundaryRange,
                  markedRange.length > 0,
                  let anchor = normalizedClientRect(
                      result.rect,
                      boundary: true
                  )
            else {
                return .failure
            }
            compositionFragments = []
            compositionRectsComplete = false
            fragmentFallbackAnchor = anchor
        case .failure:
            return .failure
        }
        let compositionExclusionRects = compositionFragments.compactMap {
            visualExclusionEnvelope(
                logicalRect: $0.rect,
                orientation: $0.orientation,
                lineMetric: $0.lineMetric
            )
        }
        guard compositionExclusionRects.count == compositionFragments.count
        else {
            return .failure
        }

        let primaryResult: CandidateClientRectResult?
        let selectedRect: NSRect?
        if fragmentFallbackAnchor != nil {
            primaryResult = nil
            selectedRect = nil
        } else {
            primaryResult = preferredRange.flatMap {
                range -> CandidateClientRectResult? in
                guard documentRangeEnd(range) != nil else { return nil }
                let result = firstRect(range)
                return normalizedClientRect(
                    result.rect,
                    boundary: range.length == 0
                ).map {
                    CandidateClientRectResult(
                        rect: $0,
                        actualRange: result.actualRange
                    )
                }
            }
            selectedRect =
                documentRangeEnd(selectedRange) != nil
                ? normalizedClientRect(
                    firstRect(selectedRange).rect,
                    boundary: selectedRange.length == 0
                )
                : nil
        }
        let retry: NSRect?
        if let preferredRange,
           let primaryResult,
           documentRangeEnd(primaryResult.actualRange) != nil,
           primaryResult.actualRange != preferredRange
        {
            let result = firstRect(primaryResult.actualRange)
            retry = normalizedClientRect(
                result.rect,
                boundary: primaryResult.actualRange.length == 0
            )
        } else {
            retry = nil
        }
        return .complete(
            CandidateClientGeometry(
                primary: primaryResult?.rect ?? fragmentFallbackAnchor,
                retry: retry,
                selectedRange: selectedRect,
                compositionFragments: compositionFragments,
                compositionExclusionRects: compositionExclusionRects,
                compositionRectsComplete: compositionRectsComplete
            )
        )
    }

    static func visibleFrame(
        for referenceAnchor: NSRect?,
        screens: [CandidateScreenFrame],
        mainVisibleFrame: NSRect?
    ) -> NSRect? {
        selectedScreen(
            for: referenceAnchor,
            screens: screens,
            mainScreen: mainVisibleFrame.map {
                CandidateScreenFrame(frame: $0, visibleFrame: $0)
            }
        )?.visibleFrame
    }

    // The screen containing the anchor's centre, else the nearest screen,
    // else the main screen. Screens with an unusable frame or scale are
    // skipped.
    static func selectedScreen(
        for referenceAnchor: NSRect?,
        screens: [CandidateScreenFrame],
        mainScreen: CandidateScreenFrame?
    ) -> CandidateScreenFrame? {
        let usableScreens = screens.filter {
            isValidFrame($0.frame)
                && isValidFrame($0.visibleFrame)
                && $0.backingScale.isFinite
                && $0.backingScale > 0
        }
        guard let referenceAnchor, isValidAnchor(referenceAnchor) else {
            return mainScreen.flatMap {
                isValidFrame($0.frame)
                    && isValidFrame($0.visibleFrame)
                    && $0.backingScale.isFinite
                    && $0.backingScale > 0
                    ? $0 : nil
            }
        }
        let anchorCenter = NSPoint(
            x: referenceAnchor.midX,
            y: referenceAnchor.midY
        )
        if let containing = usableScreens.first(where: {
            $0.frame.contains(anchorCenter)
        }) {
            return containing
        }
        if let nearest = usableScreens.min(by: {
            squaredDistance(from: referenceAnchor, to: $0.frame)
                < squaredDistance(from: referenceAnchor, to: $1.frame)
        }) {
            return nearest
        }
        return mainScreen.flatMap {
            isValidFrame($0.frame)
                && isValidFrame($0.visibleFrame)
                && $0.backingScale.isFinite
                && $0.backingScale > 0
                ? $0 : nil
        }
    }

    // Finite, non-empty and not the zero rect, which clients return when they
    // have no geometry.
    static func isValidAnchor(_ rect: NSRect) -> Bool {
        rect.origin.x.isFinite
            && rect.origin.y.isFinite
            && rect.size.width.isFinite
            && rect.size.height.isFinite
            && rect.width > 0
            && rect.height > 0
            && rect != .zero
    }

    private enum LogicalFragmentResolution {
        case complete([CandidateLogicalFragment])
        case unavailable
        case failure
    }

    private static func logicalFragments(
        in markedRange: NSRange,
        orientationAt: (Int) -> CandidateTextOrientation,
        firstRect: (NSRange) -> CandidateClientRectResult
    ) -> LogicalFragmentResolution {
        guard let markedEnd = documentRangeEnd(markedRange)
        else {
            return .failure
        }
        guard markedRange.length > 0 else {
            return .complete([])
        }

        var cursor = markedRange.location
        var fragments: [CandidateLogicalFragment] = []
        while cursor < markedEnd {
            let requested = NSRange(
                location: cursor,
                length: markedEnd - cursor
            )
            let result = firstRect(requested)
            let normalizedRect = normalizedClientRect(
                result.rect,
                boundary: false
            )
            let actualEnd = documentRangeEnd(result.actualRange)
            if fragments.isEmpty,
               actualEnd != nil,
               result.actualRange.length == 0
            {
                return .unavailable
            }
            guard let actualEnd,
                  result.actualRange.location == requested.location,
                  result.actualRange.length > 0,
                  actualEnd > cursor,
                  actualEnd <= markedEnd,
                  let rect = normalizedRect
            else {
                return .failure
            }
            let orientation = orientationAt(result.actualRange.location)
            guard let metric = lineMetric(
                for: rect,
                orientation: orientation
            ) else {
                return .failure
            }
            fragments.append(
                CandidateLogicalFragment(
                    rect: rect,
                    actualRange: result.actualRange,
                    orientation: orientation,
                    lineMetric: metric
                )
            )
            cursor = min(actualEnd, markedEnd)
        }
        return cursor == markedEnd ? .complete(fragments) : .failure
    }

    // The line's thickness across the text direction: height for horizontal
    // text, width for vertical.
    static func lineMetric(
        for rect: NSRect,
        orientation: CandidateTextOrientation
    ) -> CGFloat? {
        let metric =
            orientation == .horizontal
            ? abs(rect.height)
            : abs(rect.width)
        return metric.isFinite && metric > 0 ? metric : nil
    }

    // The area the panel must keep clear of: the fragment rect widened by one
    // line metric on every side, so the panel stays a full line away from
    // the text.
    static func visualExclusionEnvelope(
        logicalRect: NSRect,
        orientation: CandidateTextOrientation,
        lineMetric metric: CGFloat
    ) -> NSRect? {
        guard logicalRect.origin.x.isFinite,
              logicalRect.origin.y.isFinite,
              logicalRect.width.isFinite,
              logicalRect.height.isFinite,
              metric.isFinite,
              metric > 0
        else {
            return nil
        }
        let rect = logicalRect.standardized
        guard lineMetric(
            for: rect,
            orientation: orientation
        ) != nil else {
            return nil
        }
        let expanded = rect.insetBy(dx: -metric, dy: -metric)
        return isValidAnchor(expanded) ? expanded : nil
    }

    // A caret (`boundary`) may come back zero-width and is widened to one
    // point. A text rect at the exact origin is treated as the client's "no
    // geometry" answer, while a caret there is accepted.
    private static func normalizedClientRect(
        _ rect: NSRect,
        boundary: Bool
    ) -> NSRect? {
        guard rect.origin.x.isFinite,
              rect.origin.y.isFinite,
              rect.size.width.isFinite,
              rect.size.height.isFinite
        else {
            return nil
        }
        var normalized = rect.standardized
        guard normalized.height > 0 else { return nil }
        if boundary, normalized.width == 0 {
            normalized.size.width = 1
        }
        guard normalized.width > 0 else { return nil }
        if !boundary, normalized.origin == .zero {
            return nil
        }
        return normalized
    }

    // nil for NSNotFound, a negative part or an end that overflows, so an
    // invalid range is never sent back to the client.
    private static func documentRangeEnd(_ range: NSRange) -> Int? {
        guard range.location >= 0,
              range.length >= 0,
              range.location != NSNotFound
        else {
            return nil
        }
        let (end, overflow) = range.location.addingReportingOverflow(range.length)
        return overflow ? nil : end
    }

    private static func isValidFrame(_ rect: NSRect) -> Bool {
        rect.origin.x.isFinite
            && rect.origin.y.isFinite
            && rect.size.width.isFinite
            && rect.size.height.isFinite
            && rect.width > 0
            && rect.height > 0
    }

    private static func clamp(
        _ value: CGFloat,
        minimum: CGFloat,
        maximum: CGFloat
    ) -> CGFloat {
        guard maximum >= minimum else { return minimum }
        return min(max(value, minimum), maximum)
    }

    // Rounded outward to device pixels, so the clear area never shrinks.
    private static func outwardPixelAligned(
        _ rect: NSRect,
        scale: CGFloat
    ) -> NSRect {
        let minX = pixelFloor(rect.minX, scale: scale)
        let minY = pixelFloor(rect.minY, scale: scale)
        let maxX = pixelCeil(rect.maxX, scale: scale)
        let maxY = pixelCeil(rect.maxY, scale: scale)
        return NSRect(
            x: minX,
            y: minY,
            width: maxX - minX,
            height: maxY - minY
        )
    }

    private static func pixelFloor(
        _ value: CGFloat,
        scale: CGFloat
    ) -> CGFloat {
        floor(value * scale) / scale
    }

    private static func pixelCeil(
        _ value: CGFloat,
        scale: CGFloat
    ) -> CGFloat {
        ceil(value * scale) / scale
    }

    private static func squaredDistance(
        from anchor: NSRect,
        to frame: NSRect
    ) -> CGFloat {
        let point = NSPoint(x: anchor.midX, y: anchor.midY)
        let dx = max(frame.minX - point.x, 0, point.x - frame.maxX)
        let dy = max(frame.minY - point.y, 0, point.y - frame.maxY)
        return dx * dx + dy * dy
    }
}

#if MUU_TEST
extension CandidateSurfaceAnchors {
    init(
        primary: NSRect?,
        retry: NSRect?,
        selectedRange: NSRect?,
        compositionRects: [NSRect] = [],
        markedRangeUTF16Length: Int = 0,
        compositionRectsComplete: Bool = true,
        defaultAnchor: NSRect,
        visibleFrame: NSRect,
        backingScale: CGFloat = 1
    ) {
        let defaultOrientations = Array(
            repeating: CandidateTextOrientation.horizontal,
            count: compositionRects.count
        )
        let metrics = zip(compositionRects, defaultOrientations).map {
            CandidateSurfacePolicy.lineMetric(
                for: $0.0,
                orientation: $0.1
            ) ?? .nan
        }
        let exclusionRects =
            zip(
                compositionRects,
                zip(defaultOrientations, metrics)
            ).compactMap {
                CandidateSurfacePolicy.visualExclusionEnvelope(
                    logicalRect: $0.0,
                    orientation: $0.1.0,
                    lineMetric: $0.1.1
                )
            }
        self.init(
            primary: primary,
            retry: retry,
            selectedRange: selectedRange,
            compositionRects: compositionRects,
            compositionExclusionRects: exclusionRects,
            compositionLineMetrics: metrics,
            compositionOrientations: defaultOrientations,
            markedRangeUTF16Length: markedRangeUTF16Length,
            compositionRectsComplete: compositionRectsComplete,
            defaultAnchor: defaultAnchor,
            visibleFrame: visibleFrame,
            backingScale: backingScale
        )
    }
}
#endif
