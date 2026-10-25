import AppKit

// Refusing key and main status is what keeps keystrokes with the client app
// while the panel is on screen.
@MainActor
final class CandidatePanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

// One candidate row. VoiceOver's pick and press reach the controller only
// through the handlers set for the current snapshot; a row without them
// reports the action as not performed.
@MainActor
final class CandidateRowView: NSTableCellView {
    let candidateLabel = NSTextField(labelWithString: "")
    let annotationLabel = NSTextField(labelWithString: "")
    var accessibilitySelectHandler: (() -> Bool)?
    var accessibilityPressHandler: (() -> Bool)?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        setAccessibilityElement(true)
        setAccessibilityRole(.row)
        candidateLabel.lineBreakMode = .byTruncatingTail
        candidateLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        annotationLabel.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        annotationLabel.textColor = .secondaryLabelColor
        annotationLabel.alignment = .right
        annotationLabel.lineBreakMode = .byTruncatingTail
        addSubview(candidateLabel)
        addSubview(annotationLabel)
        precondition(accessibilityRole() == .row)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // The annotation takes 48 to 120 points on the right; the candidate text
    // gets the rest, truncated at the tail when it does not fit.
    override func layout() {
        super.layout()
        let inset: CGFloat = 8
        let annotationWidth = annotationLabel.isHidden
            ? 0
            : min(120, max(48, annotationLabel.intrinsicContentSize.width))
        candidateLabel.frame = NSRect(
            x: inset,
            y: 0,
            width: max(0, bounds.width - inset * 2 - annotationWidth),
            height: bounds.height
        )
        annotationLabel.frame = NSRect(
            x: max(inset, bounds.width - inset - annotationWidth),
            y: 0,
            width: annotationWidth,
            height: bounds.height
        )
    }

    override func accessibilityPerformPick() -> Bool {
        guard let accessibilitySelectHandler else { return false }
        return accessibilitySelectHandler()
    }

    override func accessibilityPerformPress() -> Bool {
        guard let accessibilityPressHandler else { return false }
        return accessibilityPressHandler()
    }
}

