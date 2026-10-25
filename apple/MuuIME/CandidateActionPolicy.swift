import AppKit

/// Decides whether a candidate action may still act. Each snapshot carries a
/// generation, which advances when the candidate identities change and on
/// every invalidation. An action is accepted only with the current generation
/// and an id from the current list, so a click on a panel that has since been
/// replaced or hidden does nothing.
struct CandidateActionAuthority {
    private(set) var generation: UInt64 = 0
    private var items: [CandidateSurfaceItem] = []
    private var candidateIDs: Set<String> = []

    // A list that only moves its selection keeps the generation, so a click
    // in progress on the same rows still counts. An invalid list (empty,
    // selection out of range, duplicate ids) invalidates and returns nil.
    mutating func snapshot(
        items nextItems: [CandidateSurfaceItem],
        selectedIndex: Int
    ) -> CandidateSurfaceSnapshot? {
        let identityChanged = items != nextItems
        let nextGeneration = identityChanged ? generation &+ 1 : generation
        let snapshot = CandidateSurfaceSnapshot(
            items: nextItems,
            selectedIndex: selectedIndex,
            generation: nextGeneration
        )
        guard CandidateSurfacePolicy.isValidSnapshot(snapshot) else {
            invalidate()
            return nil
        }
        if identityChanged {
            generation = nextGeneration
            items = nextItems
        }
        candidateIDs = Set(nextItems.map(\.id))
        return snapshot
    }

    mutating func invalidate() {
        generation &+= 1
        items.removeAll(keepingCapacity: true)
        candidateIDs.removeAll(keepingCapacity: true)
    }

    func accepts(_ action: CandidateSurfaceAction) -> Bool {
        let id: String
        let actionGeneration: UInt64
        switch action {
        case let .select(candidateID, generation):
            id = candidateID
            actionGeneration = generation
        case let .selectAndCommit(candidateID, generation):
            id = candidateID
            actionGeneration = generation
        }
        return actionGeneration == self.generation && candidateIDs.contains(id)
    }
}

extension CandidateSurfacePolicy {
    static func isValidSnapshot(_ snapshot: CandidateSurfaceSnapshot) -> Bool {
        !snapshot.items.isEmpty
            && snapshot.items.indices.contains(snapshot.selectedIndex)
            && Set(snapshot.items.map(\.id)).count == snapshot.items.count
    }

    // Reload the rows when the list or its generation changed; move the
    // selection highlight when that or the selected index changed.
    static func update(
        current: CandidateSurfaceSnapshot?,
        next: CandidateSurfaceSnapshot,
        isVisible: Bool
    ) -> CandidateSurfaceUpdate {
        let reloadList =
            current?.items != next.items
                || current?.generation != next.generation
        return CandidateSurfaceUpdate(
            reloadList: reloadList,
            updateSelection:
                reloadList || current?.selectedIndex != next.selectedIndex,
            show: !isVisible
        )
    }

    static func candidateID(
        at row: Int,
        in snapshot: CandidateSurfaceSnapshot
    ) -> String? {
        snapshot.items.indices.contains(row) ? snapshot.items[row].id : nil
    }

    static func candidateAction(
        at row: Int,
        in snapshot: CandidateSurfaceSnapshot,
        commit: Bool
    ) -> CandidateSurfaceAction? {
        guard let id = candidateID(at: row, in: snapshot) else { return nil }
        return commit
            ? .selectAndCommit(id: id, generation: snapshot.generation)
            : .select(id: id, generation: snapshot.generation)
    }

    // A double-click commits only the row that its first click selected, in
    // the same generation. A double-click that lands on another row, or after
    // the list changed, does nothing.
    static func doubleClickAction(
        after selectionAction: CandidateSurfaceAction?,
        at row: Int,
        in snapshot: CandidateSurfaceSnapshot
    ) -> CandidateSurfaceAction? {
        guard case let .select(selectedID, selectedGeneration) = selectionAction,
              selectedGeneration == snapshot.generation,
              candidateID(at: row, in: snapshot) == selectedID
        else {
            return nil
        }
        return .selectAndCommit(
            id: selectedID,
            generation: selectedGeneration
        )
    }

    static func accessibilityLabel(for item: CandidateSurfaceItem) -> String {
        guard let annotation = item.annotation, !annotation.isEmpty else {
            return item.text
        }
        return "\(item.text), \(annotation)"
    }

    static func rowAnnotation(for item: CandidateSurfaceItem) -> String? {
        guard let annotation = item.annotation, !annotation.isEmpty else {
            return nil
        }
        return annotation
    }

    // Commit after a select only when the select fully succeeded. Otherwise
    // the composition stays, so a failed select cannot commit the old choice.
    static func candidateCommitAction(
        statusOK: Bool,
        hasView: Bool,
        hasStructuredError: Bool
    ) -> CandidateCommitAction {
        guard statusOK, hasView, !hasStructuredError else {
            return .preserveComposition
        }
        return .commit
    }
}
