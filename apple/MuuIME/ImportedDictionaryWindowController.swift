import AppKit
import UniformTypeIdentifiers

final class ImportedDictionaryWindowController: NSWindowController,
    NSTableViewDataSource, NSTableViewDelegate
{
    static let shared = ImportedDictionaryWindowController(store: .shared)

    private let store: ImportedDictionaryStore
    private let tableView = NSTableView()
    private let importButton = NSButton(title: "辞書を取り込む…", target: nil, action: nil)
    private let templateButton = NSButton(title: "ひな形を書き出す…", target: nil, action: nil)
    private let deleteButton = NSButton(title: "削除", target: nil, action: nil)
    private let statusLabel = NSTextField(wrappingLabelWithString: "")
    private let informationLabel = NSTextField(wrappingLabelWithString: "")
    private var mutationInProgress = false
    private var observer: NSObjectProtocol?

    init(store: ImportedDictionaryStore) {
        self.store = store
        let window = DictionaryWindowSupport.makeWindow(
            title: "Muu 辞書管理",
            contentSize: NSSize(width: 760, height: 470),
            minimumSize: NSSize(width: 640, height: 380)
        )
        super.init(window: window)
        configureContent()
        observer = NotificationCenter.default.addObserver(forName: .muuImportedDictionariesDidChange, object: store, queue: .main) { [weak self] _ in self?.reload() }
        reload()
    }
    deinit { if let observer { NotificationCenter.default.removeObserver(observer) } }

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

    func numberOfRows(in tableView: NSTableView) -> Int {
        store.dictionaries.count
    }

    func tableView(
        _ tableView: NSTableView,
        viewFor tableColumn: NSTableColumn?,
        row: Int
    ) -> NSView? {
        guard store.dictionaries.indices.contains(row), let tableColumn else { return nil }
        let dictionary = store.dictionaries[row]
        if tableColumn.identifier.rawValue == "enabled" {
            let button = NSButton(checkboxWithTitle: "", target: self, action: #selector(toggle(_:)))
            button.state = dictionary.enabled ? .on : .off
            button.tag = row
            // While the layer is degraded only disabling is offered; enabling
            // could not pass the layer rules that emptied it (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
            button.isEnabled = store.canEdit && !mutationInProgress && (!store.isDegraded || dictionary.enabled)
            return button
        }
        let value: String
        switch tableColumn.identifier.rawValue {
        case "name": value = dictionary.metadata.name
        case "version": value = dictionary.metadata.version
        case "entries": value = "\(dictionary.entryCount)"
        default:
            value = dictionary.dormantEntryCount == 0
                ? "有効"
                : "休眠 \(dictionary.dormantEntryCount)件"
        }
        return DictionaryWindowSupport.makeTextCell(value)
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        updateSelection()
    }

    @objc private func importDictionary(_ sender: Any?) {
        let panel = NSOpenPanel()
        panel.title = "Muu辞書を取り込む"
        panel.prompt = "取り込む"
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        panel.allowedContentTypes = [
            UTType(filenameExtension: "muudict") ?? .data,
            .plainText,
        ]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        beginMutation()
        store.prepareDictionaryFile(at: url) { [weak self] result in
            guard let self else { return }
            switch result {
            case let .failure(error):
                endMutation()
                present(error)
            case let .success(prepared):
                finishImport(prepared)
            }
        }
    }

    private func finishImport(_ prepared: PreparedImportedDictionary) {
        precondition(Thread.isMainThread)
        let metadata = prepared.preview.metadata
        if let existing = store.dictionaries.first(where: {
            $0.metadata.id == metadata.id
        }) {
            let alert = NSAlert()
            alert.messageText = "「\(existing.metadata.name)」を置き換えますか？"
            alert.informativeText = "辞書IDが同じため、現在の内容を取り込んだ内容で置き換えます。"
            alert.addButton(withTitle: "置き換える")
            alert.addButton(withTitle: "キャンセル")
            guard alert.runModal() == .alertFirstButtonReturn else {
                store.cancel(prepared) { [weak self] in self?.endMutation() }
                return
            }
        }
        store.install(prepared) { [weak self] result in
            guard let self else { return }
            endMutation()
            switch result {
            case .success:
                if let index = store.dictionaries.firstIndex(where: {
                    $0.metadata.id == metadata.id
                }) {
                    tableView.selectRowIndexes(
                        IndexSet(integer: index),
                        byExtendingSelection: false
                    )
                }
            case let .failure(error):
                present(error)
            }
        }
    }

    @objc private func exportTemplate(_ sender: Any?) {
        let panel = NSSavePanel()
        panel.title = "Muu辞書のひな形を書き出す"
        panel.nameFieldStringValue = "example.muudict"
        panel.allowedContentTypes = [UTType(filenameExtension: "muudict") ?? .data]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        store.exportTemplate(to: url) { [weak self] result in
            if case let .failure(error) = result { self?.present(error) }
        }
    }

    @objc private func toggle(_ sender: NSButton) {
        guard store.dictionaries.indices.contains(sender.tag) else { return }
        beginMutation()
        store.setEnabled(
            sender.state == .on,
            dictionaryID: store.dictionaries[sender.tag].metadata.id
        ) { [weak self] result in
            guard let self else { return }
            endMutation()
            if case let .failure(error) = result { present(error) }
        }
    }

    @objc private func deleteDictionary(_ sender: Any?) {
        let row = tableView.selectedRow
        guard store.dictionaries.indices.contains(row) else { return }
        let dictionary = store.dictionaries[row]
        let alert = NSAlert()
        alert.messageText = "「\(dictionary.metadata.name)」を削除しますか？"
        alert.addButton(withTitle: "削除")
        alert.addButton(withTitle: "キャンセル")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        beginMutation()
        store.remove(
            dictionaryID: dictionary.metadata.id
        ) { [weak self] result in
            guard let self else { return }
            endMutation()
            if case let .failure(error) = result { present(error) }
        }
    }

    private func configureContent() {
        guard let contentView = window?.contentView else { return }
        let scrollView = DictionaryWindowSupport.configureTable(tableView, columns: [
            ("enabled", "使用", 48.0),
            ("name", "辞書名", 270.0),
            ("version", "バージョン", 100.0),
            ("entries", "件数", 80.0),
            ("status", "状態", 130.0),
        ])
        tableView.delegate = self
        tableView.dataSource = self

        importButton.target = self
        importButton.action = #selector(importDictionary(_:))
        templateButton.target = self
        templateButton.action = #selector(exportTemplate(_:))
        deleteButton.target = self
        deleteButton.action = #selector(deleteDictionary(_:))
        deleteButton.isEnabled = false
        let buttons = NSStackView(views: [importButton, templateButton, deleteButton])
        buttons.orientation = .horizontal
        buttons.spacing = 8

        statusLabel.textColor = .secondaryLabelColor
        statusLabel.maximumNumberOfLines = 0
        informationLabel.textColor = .secondaryLabelColor
        informationLabel.maximumNumberOfLines = 4
        let stack = NSStackView(views: [buttons, statusLabel, scrollView, informationLabel])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 18),
            stack.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -18),
            stack.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 18),
            stack.bottomAnchor.constraint(equalTo: contentView.bottomAnchor, constant: -18),
            scrollView.widthAnchor.constraint(equalTo: stack.widthAnchor),
            statusLabel.widthAnchor.constraint(equalTo: stack.widthAnchor),
            scrollView.heightAnchor.constraint(greaterThanOrEqualToConstant: 220),
            informationLabel.widthAnchor.constraint(equalTo: stack.widthAnchor),
        ])
    }

    private func reload() {
        tableView.reloadData()
        importButton.isEnabled = store.canEdit && !store.isDegraded && !mutationInProgress
        templateButton.isEnabled = store.canExportTemplate && !mutationInProgress
        updateSelection()
        let enabled = store.dictionaries.count { $0.enabled }
        var messages = store.startupState == .loading
            ? ["取り込み辞書を読み込んでいます…"] : ["\(store.dictionaries.count)個（使用中 \(enabled)個）"]
        if let error = store.loadError { messages.append(error.localizedDescription) }
        if store.isDegraded {
            // Display Rust's reason; a readable file can fail a runtime rule
            // (for example its ID), rather than a numeric capacity limit.
            if let reason = store.view?.reason {
                messages.append(ImportedDictionaryOperationError(status: MUU_RESULT_DECODE_ERR, code: reason).localizedDescription)
            }
            messages.append("辞書を有効にできないため、まず削除または無効化で直してください。")
        }
        if let warning = store.durabilityWarning { messages.append("保存の耐久性を確認できませんでした: \(warning.localizedDescription)") }
        if let notice = store.recoveryNotice { messages.append(notice) }
        statusLabel.stringValue = messages.joined(separator: "\n")
        statusLabel.textColor = store.loadError == nil && !store.isDegraded && store.durabilityWarning == nil ? .secondaryLabelColor : .systemRed
    }

    private func updateSelection() {
        let row = tableView.selectedRow
        guard store.dictionaries.indices.contains(row) else {
            deleteButton.isEnabled = false
            informationLabel.stringValue = ""
            return
        }
        let dictionary = store.dictionaries[row]
        deleteButton.isEnabled = store.canEdit && !mutationInProgress
        let dormant = dictionary.dormantEntryCount == 0
            ? ""
            : "\nサジェストのみ \(dictionary.dormantEntryCount)件は予測機能実装後に有効化"
        let importedAt = ISO8601DateFormatter().string(
            from: Date(
                timeIntervalSince1970: Double(dictionary.importedAtUnixMilliseconds) / 1_000
            )
        )
        informationLabel.stringValue = "作者: \(dictionary.metadata.author)　ライセンス: \(dictionary.metadata.license)\n由来: \(dictionary.metadata.provenance)\n取込: \(importedAt)　SHA-256: \(dictionary.sourceSHA256)\(dormant)"
    }

    private func present(_ error: Error) {
        let alert = NSAlert(error: error)
        alert.beginSheetModal(for: window!)
    }

    private func beginMutation() {
        mutationInProgress = true
        reload()
    }

    private func endMutation() {
        mutationInProgress = false
        reload()
    }
}
