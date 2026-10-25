import AppKit
import InputMethodKit

// Owns synchronous IMK queries; placement rules consume their values.
@MainActor
enum ClientGeometryReader {
    static func anchors(
        view: ViewState,
        client: IMKTextInput
    ) -> CandidateSurfaceAnchors? {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        let markedRange = client.markedRange()
        // The panel prefers the focused clause (the segment being converted),
        // else the caret. Both are Rust UTF-16 offsets into the preedit,
        // moved into document positions by the client's marked range and
        // checked so the sum cannot overflow or leave the marked text.
        var preferredRange: NSRange?
        let focusedRun = view.preedit.first(where: \.isFocused)
        if let focusedRun,
           markedRange.location != NSNotFound,
           focusedRun.utf16Start >= 0,
           focusedRun.utf16Length >= 0,
           focusedRun.utf16Start <= markedRange.length,
           focusedRun.utf16Length <= markedRange.length - focusedRun.utf16Start,
           focusedRun.utf16Start <= Int.max - markedRange.location
        {
            preferredRange = NSRange(
                location: markedRange.location + focusedRun.utf16Start,
                length: focusedRun.utf16Length
            )
        } else if markedRange.location != NSNotFound,
                  view.caretUTF16 >= 0,
                  view.caretUTF16 <= markedRange.length,
                  view.caretUTF16 <= Int.max - markedRange.location
        {
            preferredRange = NSRange(
                location: markedRange.location + view.caretUTF16,
                length: 0
            )
        }

        let selectedRange = client.selectedRange()
        let clientGeometryResolution = CandidateSurfacePolicy.clientGeometry(
            markedRange: markedRange,
            preferredRange: preferredRange,
            selectedRange: selectedRange,
            orientationAt: { index in
                let orientation = textOrientation(client: client as AnyObject, at: index)
                return orientation
            }
        ) { range in
            var actualRange = NSRange(location: NSNotFound, length: 0)
            return CandidateClientRectResult(
                rect: client.firstRect(
                    forCharacterRange: range,
                    actualRange: &actualRange
                ),
                actualRange: actualRange
            )
        }
        let clientGeometry: CandidateClientGeometry
        switch clientGeometryResolution {
        case let .complete(geometry):
            clientGeometry = geometry
        case .failure:
            // The document-range firstRect path keeps priority. Some legacy
            // clients leave it unwritten but implement the inline attributes
            // query. Read that only after the complete firstRect path failed.
            // This ordinary fallback is kept so that the panel still appears
            // in clients whose firstRect fails.
            // attributeFallbackSelection repeats this check, so its
            // `invalid_range` result cannot arise from this call; an unusable
            // marked range simply hides the panel here.
            guard markedRange.location != NSNotFound,
                  markedRange.location >= 0,
                  markedRange.length > 0,
                  markedRange.length <= Int.max - markedRange.location
            else { return nil }
            let screens = NSScreen.screens.map {
                CandidateScreenFrame(frame: $0.frame, visibleFrame: $0.visibleFrame,
                    backingScale: $0.backingScaleFactor)
            }
            // Query both interpretations only on this failed firstRect path.
            // Every call stays inside the same Objective-C exception boundary.
            let selection = CandidateSurfacePolicy.attributeFallbackSelection(markedRange: markedRange,
                focusedStart: focusedRun?.utf16Start, caret: view.caretUTF16, screens: screens) {
                    MuuClientReadLineGeometry(client, UInt($0))
                }
            return selection.anchors
        }

        let referenceRect = [
            clientGeometry.primary,
            clientGeometry.retry,
            clientGeometry.selectedRange,
            clientGeometry.compositionRects.first,
        ]
            .compactMap { $0 }
            .first(where: CandidateSurfacePolicy.isValidAnchor)
        let screens = NSScreen.screens.map {
            CandidateScreenFrame(
                frame: $0.frame,
                visibleFrame: $0.visibleFrame,
                backingScale: $0.backingScaleFactor
            )
        }
        let mainScreen = NSScreen.main.map {
            CandidateScreenFrame(
                frame: $0.frame,
                visibleFrame: $0.visibleFrame,
                backingScale: $0.backingScaleFactor
            )
        }
        let selectedScreen =
            CandidateSurfacePolicy.selectedScreen(
                for: referenceRect,
                screens: screens,
                mainScreen: mainScreen
            )
            ?? CandidateScreenFrame(
                frame: NSRect(x: 0, y: 0, width: 1024, height: 768),
                visibleFrame: NSRect(x: 0, y: 0, width: 1024, height: 768)
            )
        let visibleFrame = selectedScreen.visibleFrame
        // Last resort when no client anchor is usable: near the left edge,
        // halfway up the chosen screen.
        let defaultAnchor = NSRect(
            x: visibleFrame.minX + 24,
            y: visibleFrame.midY,
            width: 1,
            height: 20
        )
        return CandidateSurfaceAnchors(
            primary: clientGeometry.primary,
            retry: clientGeometry.retry,
            selectedRange: clientGeometry.selectedRange,
            compositionRects: clientGeometry.compositionRects,
            compositionExclusionRects:
                clientGeometry.compositionExclusionRects,
            compositionLineMetrics: clientGeometry.compositionLineMetrics,
            compositionOrientations: clientGeometry.compositionOrientations,
            markedRangeUTF16Length: markedRange.length,
            compositionRectsComplete: clientGeometry.compositionRectsComplete,
            defaultAnchor: defaultAnchor,
            visibleFrame: visibleFrame,
            backingScale: selectedScreen.backingScale
        )
    }

    /// Badge geometry remains strict: exact caret and complete fragments,
    /// with neither attributes nor candidate last-valid/default fallbacks.
    static func badgeGeometry(view: ViewState, client: IMKTextInput,
                              screens: [CandidateScreenFrame], contentSize: NSSize) -> ModeBadgeGeometry? {
        BadgePlacement.geometry(view: view, screens: screens, contentSize: contentSize,
            readSelectedRange: { client.selectedRange() }, readMarkedRange: { client.markedRange() },
            readOrientation: { textOrientation(client: client as AnyObject, at: $0) }, readRect: { range in
                var actual = NSRange(location: NSNotFound, length: 0)
                let rect = client.firstRect(forCharacterRange: range, actualRange: &actual)
                return CandidateClientRectResult(rect: rect, actualRange: actual)
            })
    }

    // drawsVerticallyForCharacterAtIndex: is optional and not declared on
    // IMKTextInput, so it is looked up and called through its IMP. A client
    // without it, or a negative index, counts as horizontal.
    static func textOrientation(
        client: AnyObject,
        at characterIndex: Int
    ) -> CandidateTextOrientation {
        guard characterIndex >= 0 else { return .horizontal }
        let selector = NSSelectorFromString(
            "drawsVerticallyForCharacterAtIndex:"
        )
        guard client.responds(to: selector),
              let method = client.method(for: selector)
        else {
            return .horizontal
        }
        typealias Implementation =
            @convention(c) (AnyObject, Selector, UInt) -> ObjCBool
        let implementation = unsafeBitCast(method, to: Implementation.self)
        return implementation(
            client,
            selector,
            UInt(characterIndex)
        ).boolValue ? .vertical : .horizontal
    }
}
