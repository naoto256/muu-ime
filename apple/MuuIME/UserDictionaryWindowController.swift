import AppKit

final class UserDictionaryWindowController: NSWindowController,
    NSTableViewDataSource, NSTableViewDelegate
{
    static let shared = UserDictionaryWindowController(store: .shared)

    private let store: UserDictionaryStore
    private let tableView = NSTableView()
    private let readingField = NSTextField()
    private let surfaceField = NSTextField()
    private let partOfSpeechPopup = NSPopUpButton()
    private let priorityPopup = NSPopUpButton()
    private let saveButton = NSButton(title: "追加", target: nil, action: nil)
    private let deleteButton = NSButton(title: "削除", target: nil, action: nil)
    private let statusLabel = NSTextField(labelWithString: "")
    #if MUU_TEST
    private(set) var presentedErrorForTesting: Error?
    var statusTextForTesting: String { statusLabel.stringValue }
    var selectedRowForTesting: Int { tableView.selectedRow }
    #endif

    init(store: UserDictionaryStore) {
        self.store = store
        let window = DictionaryWindowSupport.makeWindow(
            title: "Muu ユーザー辞書",
            contentSize: NSSize(width: 680, height: 440),
            minimumSize: NSSize(width: 560, height: 360)
        )
        super.init(window: window)
        configureContent()
        reload()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func show() {
        precondition(Thread.isMainThread)
        reload()
        NSApp.activate(ignoringOtherApps: true)
        window?.center()
        showWindow(nil)
        window?.makeKeyAndOrderFront(nil)
    }

    #if MUU_TEST
    func saveForTesting(
        reading: String,
        surface: String,
        partOfSpeech: MuuDictionaryPartOfSpeech = .noun,
        priority: UserDictionaryPriority = .normal
    ) {
        readingField.stringValue = reading
        surfaceField.stringValue = surface
        partOfSpeechPopup.selectItem(withTitle: partOfSpeech.displayName)
        priorityPopup.selectItem(withTitle: priority.displayName)
        saveEntry(nil)
    }
    func deleteForTesting(at index: Int) {
        tableView.selectRowIndexes(IndexSet(integer: index), byExtendingSelection: false)
        deleteEntry(nil)
    }
    #endif

    func numberOfRows(in tableView: NSTableView) -> Int {
        store.entries.count
    }

    func tableView(
        _ tableView: NSTableView,
        viewFor tableColumn: NSTableColumn?,
        row: Int
    ) -> NSView? {
        guard store.entries.indices.contains(row), let tableColumn else { return nil }
        let entry = store.entries[row]
        let value: String
        switch tableColumn.identifier.rawValue {
        case "reading": value = entry.reading
        case "surface": value = entry.surface
        case "partOfSpeech": value = entry.partOfSpeech.displayName
        default: value = entry.priority.displayName
        }
        return DictionaryWindowSupport.makeTextCell(value)
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        let row = tableView.selectedRow
        guard store.entries.indices.contains(row) else {
            deleteButton.isEnabled = false
            saveButton.title = "追加"
            return
        }
        let entry = store.entries[row]
        readingField.stringValue = entry.reading
        surfaceField.stringValue = entry.surface
        partOfSpeechPopup.selectItem(withTitle: entry.partOfSpeech.displayName)
        priorityPopup.selectItem(withTitle: entry.priority.displayName)
        deleteButton.isEnabled = store.canEdit
        saveButton.title = "更新"
    }

    // The popups are filled in allCases order, so a selected index maps
    // straight back to its case. The row index is resolved to an entry ID by
    // the store against the view that is displayed now.
    @objc private func saveEntry(_ sender: Any?) {
        let priority = UserDictionaryPriority.allCases[priorityPopup.indexOfSelectedItem]
        let pos = MuuDictionaryPartOfSpeech.allCases[partOfSpeechPopup.indexOfSelectedItem]
        store.upsert(reading: readingField.stringValue, surface: surfaceField.stringValue,
            partOfSpeech: pos, priority: priority, replacing: tableView.selectedRow >= 0 ? tableView.selectedRow : nil) {
                [weak self] result in self?.finished(result, focusReading: true)
        }
        reload()
    }

    @objc private func deleteEntry(_ sender: Any?) {
        store.remove(at: tableView.selectedRow) { [weak self] result in self?.finished(result, focusReading: false) }
        reload()
    }

    private func finished(_ result: Result<Void, Error>, focusReading: Bool) {
        // The store remains busy until this UI completion returns. Reenable
        // controls on the next main turn, after that ownership is released.
        switch result {
        case .success:
            readingField.stringValue = ""; surfaceField.stringValue = ""
            tableView.deselectAll(nil)
            if focusReading { window?.makeFirstResponder(readingField) }
        case .failure(let error): present(error)
        }
        reload()
        DispatchQueue.main.async { [weak self] in self?.reload() }
    }

    @objc private func revealFile(_ sender: Any?) {
        NSWorkspace.shared.activateFileViewerSelecting([store.fileURL])
    }

    private func configureContent() {
        guard let contentView = window?.contentView else { return }
        let readingLabel = NSTextField(labelWithString: "よみ")
        let surfaceLabel = NSTextField(labelWithString: "単語")
        let partOfSpeechLabel = NSTextField(labelWithString: "品詞")
        let priorityLabel = NSTextField(labelWithString: "優先度")
        readingField.placeholderString = "ひらがな"
        surfaceField.placeholderString = "登録する単語"
        for partOfSpeech in MuuDictionaryPartOfSpeech.allCases {
            partOfSpeechPopup.addItem(withTitle: partOfSpeech.displayName)
        }
        partOfSpeechPopup.selectItem(withTitle: MuuDictionaryPartOfSpeech.noun.displayName)
        for priority in UserDictionaryPriority.allCases {
            priorityPopup.addItem(withTitle: priority.displayName)
        }
        priorityPopup.selectItem(withTitle: UserDictionaryPriority.normal.displayName)

        saveButton.target = self
        saveButton.action = #selector(saveEntry(_:))
        saveButton.keyEquivalent = "\r"
        deleteButton.target = self
        deleteButton.action = #selector(deleteEntry(_:))
        deleteButton.isEnabled = false
        let revealButton = NSButton(title: "辞書ファイルを表示", target: self, action: #selector(revealFile(_:)))

        let form = NSGridView(views: [
            [readingLabel, readingField],
            [surfaceLabel, surfaceField],
            [partOfSpeechLabel, partOfSpeechPopup],
            [priorityLabel, priorityPopup],
        ])
        form.column(at: 0).xPlacement = .trailing
        form.column(at: 1).width = 460
        form.rowSpacing = 8
        form.columnSpacing = 12

        let scrollView = DictionaryWindowSupport.configureTable(tableView, columns: [
            ("reading", "よみ", 190.0),
            ("surface", "単語", 350.0),
            ("partOfSpeech", "品詞", 130.0),
            ("priority", "優先度", 90.0),
        ])
        tableView.delegate = self
        tableView.dataSource = self

        statusLabel.textColor = .secondaryLabelColor
        statusLabel.maximumNumberOfLines = 0
        statusLabel.lineBreakMode = .byWordWrapping
        let buttonRow = NSStackView(views: [saveButton, deleteButton, revealButton])
        buttonRow.orientation = .horizontal
        buttonRow.spacing = 8
        let stack = NSStackView(views: [form, buttonRow, statusLabel, scrollView])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 12
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 18),
            stack.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -18),
            stack.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 18),
            stack.bottomAnchor.constraint(equalTo: contentView.bottomAnchor, constant: -18),
            form.widthAnchor.constraint(equalTo: stack.widthAnchor),
            statusLabel.widthAnchor.constraint(equalTo: stack.widthAnchor),
            scrollView.widthAnchor.constraint(equalTo: stack.widthAnchor),
            scrollView.heightAnchor.constraint(greaterThanOrEqualToConstant: 180),
        ])
    }

    private func reload() {
        tableView.reloadData()
        let writable = store.canEdit
        readingField.isEnabled = writable
        surfaceField.isEnabled = writable
        partOfSpeechPopup.isEnabled = writable
        priorityPopup.isEnabled = writable
        saveButton.isEnabled = writable && !store.isDegraded
        deleteButton.isEnabled = writable && tableView.selectedRow >= 0
        if let error = store.loadError {
            statusLabel.stringValue = error.localizedDescription
            statusLabel.textColor = .systemRed
        } else if store.isBusy {
            statusLabel.stringValue = "保存中…"
            statusLabel.textColor = .secondaryLabelColor
        } else if store.isDegraded || store.durabilityWarning != nil {
            // Degradation and durability describe independent facts. A delete
            // can preserve an empty layer while its saved bytes need a warning.
            var details: [String] = []
            if store.isDegraded { details.append("辞書が上限を超えているため、まず削除で直してください。") }
            if let warning = store.durabilityWarning {
                details.append("保存の耐久性を確認できませんでした: \(warning.localizedDescription)")
            }
            statusLabel.stringValue = "\(store.view?.entryCount ?? 0)件 — \(details.joined(separator: "\n"))"
            statusLabel.textColor = .systemOrange
        } else {
            let dormantCount = store.view?.dormantCount ?? 0
            statusLabel.stringValue = dormantCount == 0
                ? "\(store.view?.entryCount ?? 0)件"
                : "\(store.view?.entryCount ?? 0)件 — サジェストのみ \(dormantCount)件は予測機能実装後に有効化"
            statusLabel.textColor = .secondaryLabelColor
        }
    }

    private func present(_ error: Error) {
        #if MUU_TEST
        presentedErrorForTesting = error
        #else
        let alert = NSAlert(error: error)
        alert.beginSheetModal(for: window!)
        #endif
    }
}
