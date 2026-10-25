import AppKit
import OSLog

/// The candidate list window. It never takes key, main or first-responder
/// status from the client app, so typing keeps going to the app (and through
/// IMK to the controller). It shows one page of up to eight rows: the page
/// that holds the selected candidate. Rows only report actions; the
/// controller decides through CandidateActionAuthority whether one still
/// applies. Everything here runs on main.
@MainActor
final class CandidatePanelController: NSObject {
    private static let rowHeight: CGFloat = 30
    private static let maximumVisibleRows = 8
    private static let minimumWidth: CGFloat = 220
    private static let maximumWidth: CGFloat = 480

    private let logger = Logger(
        subsystem: MuuConstants.bundleIdentifier,
        category: "candidate_surface"
    )
    private let actionHandler: (CandidateSurfaceAction) -> Void
    private let panel: CandidatePanel
    private let scrollView = NSScrollView()
    private let tableView = NSTableView()
    private var snapshot: CandidateSurfaceSnapshot?
    private var pageStart = 0
    private var actionsEnabled = false
    // The last anchor the client itself reported usably. When none of a later
    // response's anchors is usable, the panel is placed from this one before
    // falling back to the default anchor. hide() clears it so it never
    // carries over to another composition.
    private var lastValidAnchor: NSRect?
    // The selection made by the first click, so a double-click commits only
    // that row.
    private var pendingSelectionAction: CandidateSurfaceAction?

    init(actionHandler: @escaping (CandidateSurfaceAction) -> Void) {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        self.actionHandler = actionHandler
        panel = CandidatePanel(
            contentRect: .zero,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        super.init()
        configurePanel()
    }


    deinit {
        // Pending selection and snapshot need no reset; they end with self.
        // The panel outlives self until the main-thread cleanup runs.
        let panel = panel
        finishInputUIOnMain {
            panel.orderOut(nil)
            panel.close()
        }
    }

    func render(
        snapshot next: CandidateSurfaceSnapshot,
        anchors: CandidateSurfaceAnchors
    ) {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        guard CandidateSurfacePolicy.isValidSnapshot(next) else {
            hide()
            return
        }
        // While actions are disabled, only a snapshot newer than the
        // invalidated one may redraw and re-enable the rows; an older render
        // arriving late is dropped.
        if !actionsEnabled, let snapshot,
           next.generation <= snapshot.generation
        {
            return
        }

        let update = CandidateSurfacePolicy.update(
            current: snapshot,
            next: next,
            isVisible: panel.isVisible
        )
        let nextPageStart = pageStart(for: next.selectedIndex)
        let pageChanged = nextPageStart != pageStart
        let contentSize = preferredContentSize(
            for: next,
            pageStart: nextPageStart
        )
        guard let geometry = CandidateSurfacePolicy.geometry(
            anchors: anchors,
            lastValidAnchor: lastValidAnchor,
            contentSize: contentSize
        ) else {
            logger.error(
                "candidate_surface geometry=invalid count=\(next.items.count, privacy: .public)"
            )
            hide()
            return
        }
        let geometryChanged = panel.frame != geometry.frame
        guard update.reloadList
            || update.updateSelection
            || update.show
            || geometryChanged
        else {
            return
        }

        if snapshot?.generation != next.generation {
            pendingSelectionAction = nil
        }
        snapshot = next
        pageStart = nextPageStart
        actionsEnabled = true
        if update.reloadList || pageChanged {
            tableView.reloadData()
        }
        if update.reloadList || pageChanged {
            let visibleRows = visiblePageRange(in: next).count
            tableView.frame = NSRect(
                origin: .zero,
                size: NSSize(
                    width: contentSize.width,
                    height: CGFloat(visibleRows) * Self.rowHeight
                )
            )
            tableView.tableColumns.first?.width = contentSize.width
        }
        if update.updateSelection || pageChanged {
            let visibleRow = next.selectedIndex - pageStart
            tableView.selectRowIndexes(
                IndexSet(integer: visibleRow),
                byExtendingSelection: false
            )
            tableView.scrollRowToVisible(visibleRow)
            updateVisibleAccessibilitySelection(selectedIndex: next.selectedIndex)
        }

        if let cacheableAnchor = geometry.cacheableAnchor {
            lastValidAnchor = cacheableAnchor
        }
        if geometryChanged {
            panel.setFrame(geometry.frame, display: panel.isVisible)
        }
        // orderFrontRegardless shows the panel without activating Muu. The
        // precondition proves that showing it moved no focus.
        if update.show {
            let previousKeyWindow = NSApp.keyWindow
            let previousMainWindow = NSApp.mainWindow
            let previousFirstResponder = previousKeyWindow?.firstResponder
            panel.orderFrontRegardless()
            precondition(
                NSApp.keyWindow === previousKeyWindow
                    && NSApp.mainWindow === previousMainWindow
                    && NSApp.keyWindow?.firstResponder === previousFirstResponder,
                "Candidate panel must not take key, main, or client focus"
            )
        }
        logger.notice(
            "candidate_surface reload=\(update.reloadList, privacy: .public) selection=\(update.updateSelection, privacy: .public) show=\(update.show, privacy: .public) geometry_changed=\(geometryChanged, privacy: .public) composition_rects=\(anchors.compositionRects.count, privacy: .public) count=\(next.items.count, privacy: .public) selected_index=\(next.selectedIndex, privacy: .public) placed_above=\(geometry.placedAbove, privacy: .public)"
        )
    }

    func hide() {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        lastValidAnchor = nil
        pageStart = 0
        guard panel.isVisible || snapshot != nil else { return }
        panel.orderOut(nil)
        snapshot = nil
        actionsEnabled = false
        pendingSelectionAction = nil
        tableView.deselectAll(nil)
        logger.notice("candidate_surface hide=true")
    }

    // The rows stay visible but inert. The local generation advances, so
    // accessibility handlers built for the old rows are refused, and only a
    // render from a newer snapshot turns the actions back on.
    func invalidateActionsPreservingPresentation() {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        actionsEnabled = false
        pendingSelectionAction = nil
        guard let snapshot else { return }
        self.snapshot = CandidateSurfaceSnapshot(
            items: snapshot.items,
            selectedIndex: snapshot.selectedIndex,
            generation: snapshot.generation &+ 1
        )
    }

    #if MUU_TEST
    /// Advance the generation without disabling the currently visible
    /// actions.  This models a stale event arriving after a fresh snapshot
    /// was published while keeping the live presentation authoritative.
    func advanceGenerationPreservingActionsForTesting() {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        guard let snapshot else { return }
        self.snapshot = CandidateSurfaceSnapshot(
            items: snapshot.items,
            selectedIndex: snapshot.selectedIndex,
            generation: snapshot.generation &+ 1
        )
    }
    #endif

    private func configurePanel() {
        panel.isFloatingPanel = true
        panel.level = .popUpMenu
        panel.hidesOnDeactivate = false
        panel.hasShadow = true
        panel.isOpaque = false
        panel.backgroundColor = .windowBackgroundColor
        panel.collectionBehavior = [
            .canJoinAllSpaces,
            .fullScreenAuxiliary,
            .transient,
            .ignoresCycle,
        ]
        panel.isReleasedWhenClosed = false
        panel.becomesKeyOnlyIfNeeded = true
        panel.isMovable = false

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("candidate"))
        column.resizingMask = .autoresizingMask
        column.width = Self.minimumWidth
        tableView.addTableColumn(column)
        tableView.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        tableView.headerView = nil
        tableView.rowHeight = Self.rowHeight
        tableView.intercellSpacing = .zero
        tableView.backgroundColor = .clear
        tableView.selectionHighlightStyle = .regular
        tableView.allowsMultipleSelection = false
        tableView.allowsEmptySelection = false
        tableView.dataSource = self
        tableView.delegate = self
        tableView.target = self
        tableView.action = #selector(clickedCandidate)
        tableView.doubleAction = #selector(doubleClickedCandidate)
        tableView.setAccessibilityRole(.list)
        tableView.setAccessibilityLabel("Muu IME candidates")
        tableView.autoresizingMask = [.width]

        scrollView.documentView = tableView
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        scrollView.autoresizingMask = [.width, .height]
        panel.contentView = scrollView
        validatePanelContract()
    }

    // Pages are fixed blocks of eight, not a window that scrolls one row.
    private func pageStart(for selectedIndex: Int) -> Int {
        selectedIndex / Self.maximumVisibleRows * Self.maximumVisibleRows
    }

    private func visiblePageRange(
        in snapshot: CandidateSurfaceSnapshot
    ) -> Range<Int> {
        let end = min(pageStart + Self.maximumVisibleRows, snapshot.items.count)
        return pageStart..<end
    }

    private func preferredContentSize(
        for snapshot: CandidateSurfaceSnapshot,
        pageStart: Int
    ) -> NSSize {
        let candidateFont = NSFont.systemFont(ofSize: NSFont.systemFontSize)
        let annotationFont = NSFont.systemFont(ofSize: NSFont.smallSystemFontSize)
        let pageEnd = min(
            pageStart + Self.maximumVisibleRows,
            snapshot.items.count
        )
        let pageItems = snapshot.items[pageStart..<pageEnd]
        let widest = pageItems.reduce(CGFloat.zero) { width, item in
            let candidateWidth = (item.text as NSString).size(
                withAttributes: [.font: candidateFont]
            ).width
            let annotationWidth = item.annotation.map {
                ($0 as NSString).size(withAttributes: [.font: annotationFont]).width
                    + 16
            } ?? 0
            return max(width, candidateWidth + annotationWidth)
        }
        let width = min(
            Self.maximumWidth,
            max(Self.minimumWidth, ceil(widest) + 32)
        )
        return NSSize(
            width: width,
            height: CGFloat(pageItems.count) * Self.rowHeight
        )
    }

    private func updateVisibleAccessibilitySelection(selectedIndex: Int) {
        guard let snapshot else { return }
        for row in 0..<visiblePageRange(in: snapshot).count {
            let view = tableView.view(
                atColumn: 0,
                row: row,
                makeIfNecessary: false
            )
            view?.setAccessibilitySelected(pageStart + row == selectedIndex)
        }
    }

    @objc
    private func clickedCandidate() {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        _ = performMouseSelection(at: tableView.clickedRow)
    }

    // A single click only selects. `row` is relative to the visible page.
    private func performMouseSelection(at row: Int) -> Bool {
        guard actionsEnabled,
              let snapshot,
              visiblePageRange(in: snapshot).contains(pageStart + row),
              let action = CandidateSurfacePolicy.candidateAction(
                  at: pageStart + row,
                  in: snapshot,
                  commit: false
              )
        else { return false }
        pendingSelectionAction = action
        actionHandler(action)
        return true
    }

    @objc
    private func doubleClickedCandidate() {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        _ = performMouseCommit(at: tableView.clickedRow)
    }

    private func performMouseCommit(at row: Int) -> Bool {
        guard actionsEnabled,
              let snapshot,
              visiblePageRange(in: snapshot).contains(pageStart + row),
              let action = CandidateSurfacePolicy.doubleClickAction(
                  after: pendingSelectionAction,
                  at: pageStart + row,
                  in: snapshot
              )
        else { return false }
        pendingSelectionAction = nil
        actionHandler(action)
        return true
    }

    private func validatePanelContract() {
        precondition(panel.styleMask.contains(.nonactivatingPanel))
        precondition(panel.level == .popUpMenu)
        precondition(panel.hidesOnDeactivate == false)
        precondition(panel.collectionBehavior.contains(.canJoinAllSpaces))
        precondition(panel.collectionBehavior.contains(.fullScreenAuxiliary))
        precondition(panel.canBecomeKey == false)
        precondition(panel.canBecomeMain == false)
        precondition(tableView.accessibilityRole() == .list)
    }

}

extension CandidatePanelController: NSTableViewDataSource, NSTableViewDelegate {
    func numberOfRows(in tableView: NSTableView) -> Int {
        snapshot.map { visiblePageRange(in: $0).count } ?? 0
    }

    func tableView(
        _ tableView: NSTableView,
        viewFor tableColumn: NSTableColumn?,
        row: Int
    ) -> NSView? {
        guard let snapshot,
              visiblePageRange(in: snapshot).contains(pageStart + row)
        else {
            return nil
        }
        let identifier = NSUserInterfaceItemIdentifier("candidate-row")
        let view =
            tableView.makeView(withIdentifier: identifier, owner: self)
                as? CandidateRowView
            ?? CandidateRowView()
        view.identifier = identifier
        let globalIndex = pageStart + row
        let item = snapshot.items[globalIndex]
        view.candidateLabel.stringValue = item.text
        let annotation = CandidateSurfacePolicy.rowAnnotation(for: item)
        view.annotationLabel.stringValue = annotation ?? ""
        view.annotationLabel.isHidden = annotation == nil
        view.setAccessibilityLabel(CandidateSurfacePolicy.accessibilityLabel(for: item))
        view.setAccessibilitySelected(globalIndex == snapshot.selectedIndex)
        let generation = snapshot.generation
        view.accessibilitySelectHandler = { [weak self] in
            self?.performAccessibilityAction(
                row: globalIndex,
                generation: generation,
                commit: false
            ) ?? false
        }
        view.accessibilityPressHandler = { [weak self] in
            self?.performAccessibilityAction(
                row: globalIndex,
                generation: generation,
                commit: true
            ) ?? false
        }
        return view
    }

    // Each row's handlers capture the generation they were built for. Pick
    // selects; press selects and commits, built the same way as a
    // double-click on that row.
    private func performAccessibilityAction(
        row: Int,
        generation: UInt64,
        commit: Bool
    ) -> Bool {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        guard actionsEnabled,
              let snapshot,
              snapshot.generation == generation
        else {
            return false
        }
        let action: CandidateSurfaceAction?
        if commit {
            let selection = CandidateSurfacePolicy.candidateAction(
                at: row,
                in: snapshot,
                commit: false
            )
            action = CandidateSurfacePolicy.doubleClickAction(
                after: selection,
                at: row,
                in: snapshot
            )
        } else {
            action = CandidateSurfacePolicy.candidateAction(
                at: row,
                in: snapshot,
                commit: false
            )
        }
        guard let action else { return false }
        actionHandler(action)
        return true
    }

    /// Read on main, just like every other panel operation. The input
    /// controller uses actual visibility so a hidden panel cannot consume
    /// a key when Core becomes unavailable (docs/src/internals/architecture.md「失敗したとき」).
    var isVisible: Bool { panel.isVisible }

    #if MUU_TEST
    func accessibilityRowForTesting(at row: Int) -> NSView? {
        precondition(Thread.isMainThread, "Candidate surface is main-thread only")
        guard tableView.numberOfRows > row, row >= 0 else { return nil }
        return tableView.view(
            atColumn: 0,
            row: row,
            makeIfNecessary: true
        )
    }

    var isVisibleForTesting: Bool {
        isVisible
    }

    var snapshotForTesting: CandidateSurfaceSnapshot? {
        snapshot
    }

    var visibleGlobalRangeForTesting: Range<Int> {
        snapshot.map { visiblePageRange(in: $0) } ?? 0..<0
    }

    var materializedRowCountForTesting: Int {
        tableView.numberOfRows
    }

    var actionsEnabledForTesting: Bool {
        actionsEnabled
    }

    var frameForTesting: NSRect {
        panel.frame
    }

    var displayedRowCountForTesting: Int { tableView.numberOfRows }
    var selectedRowForTesting: Int { tableView.selectedRow }

    var windowNumberForTesting: Int {
        panel.windowNumber
    }


    var hasShadowForTesting: Bool {
        panel.hasShadow
    }

    var hasPendingSelectionForTesting: Bool {
        pendingSelectionAction != nil
    }

    #if MUU_TEST
    func accessibilityMetadataForTesting(at row: Int) -> (label: String, selectable: Bool, pressable: Bool)? {
        guard let view = tableView.view(
            atColumn: 0,
            row: row,
            makeIfNecessary: true
        ) as? CandidateRowView
        else { return nil }
        return (
            view.accessibilityLabel() ?? "",
            view.accessibilitySelectHandler != nil,
            view.accessibilityPressHandler != nil
        )
    }
    #endif

    func performMouseSelectionForTesting(at row: Int) -> Bool {
        performMouseSelection(at: row)
    }

    func performMouseCommitForTesting(at row: Int) -> Bool {
        performMouseCommit(at: row)
    }

    #endif
}
