import AppKit

final class PostalCodeWindowController: NSWindowController {
    static let shared = PostalCodeWindowController(store: .shared)

    private let store: PostalCodeStore
    private let statusLabel = NSTextField(labelWithString: "")
    private let sourceLabel = NSTextField(labelWithString: "")
    private let retrievedLabel = NSTextField(labelWithString: "")
    private let countLabel = NSTextField(labelWithString: "")
    private let hashLabel = NSTextField(labelWithString: "")
    private let updateButton = NSButton(title: "郵便番号データを更新…", target: nil, action: nil)

    #if MUU_TEST
    var statusTextForTesting: String { statusLabel.stringValue }
    private(set) var presentedErrorForTesting: Error?
    func startUpdateForTesting() { startUpdate() }
    #endif

    init(store: PostalCodeStore) {
        self.store = store
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 300),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Muu 郵便番号データ"
        super.init(window: window)
        configureContent()
        store.startupStateDidChange = { [weak self] in self?.reload() }
        store.updatesDidStop = { [weak self] in self?.reload() }
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

    @objc private func update(_ sender: Any?) {
        let alert = NSAlert()
        alert.messageText = "郵便番号データを更新しますか？"
        alert.informativeText = "日本郵便の公式サイトへ接続し、最新データを取得します。自動更新は行いません。"
        alert.addButton(withTitle: "更新")
        alert.addButton(withTitle: "キャンセル")
        guard let window else { return }
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn else { return }
            self?.startUpdate()
        }
    }

    @objc private func revealData(_ sender: Any?) {
        NSWorkspace.shared.open(store.dataDirectoryURL)
    }

    @objc private func openSource(_ sender: Any?) {
        guard let url = URL(string: PostalCodeStore.sourcePageURL) else { return }
        NSWorkspace.shared.open(url)
    }

    private func startUpdate() {
        statusLabel.stringValue = "日本郵便から取得しています…"
        statusLabel.textColor = .secondaryLabelColor
        updateButton.isEnabled = false
        store.updateFromJapanPost { [weak self] result in
            guard let self else { return }
            self.reload()
            if case let .failure(error) = result {
                // Recovery has been adopted here. Final cancel runs later;
                // updatesDidStop reloads if it then invalidates this result.
                // A disabled layer keeps its restart explanation.
                //
                // The text comes from the store's facts, not from the
                // failure alone. First its status messages, in order: the
                // failure (or the layer's degraded reason), a durability
                // warning, a recovery notice. A failure delivered by an
                // update has set the first by the time this runs. An update
                // refused before it starts (busy, or editing stopped)
                // changes no state and may leave none; the error's own
                // description is used then.
                // "現在の変換辞書を維持しています" is added only
                // when editing has not stopped and the layer is still
                // `available`: a download, validation or publish failure
                // that changed nothing. After a disable the layer is empty
                // and the message asks for a restart; after a fatal stop,
                // or with no usable layer, nothing was kept, so the claim
                // is left out.
                //
                // This text can still be replaced. The store's final cancel
                // runs on its IO queue after this completion, and its result
                // reaches main in a later turn. If that cancel fails, the
                // store stops updates and records `completion_failed`, and
                // `updatesDidStop` reloads the window: the text becomes the
                // store's messages, which now lead with the restart notice in
                // place of this failure, and the kept-data line is gone.
                let messages = self.store.statusMessages
                self.statusLabel.stringValue = messages.isEmpty
                    ? error.localizedDescription : messages.joined(separator: "\n")
                if !self.store.updatesStopped && self.store.view?.layerStatus == "available" {
                    self.statusLabel.stringValue += "\n現在の変換辞書を維持しています。"
                }
                self.statusLabel.textColor = .systemRed
                #if MUU_TEST
                self.presentedErrorForTesting = error
                #else
                let alert = NSAlert(error: error)
                if let window = self.window {
                    alert.beginSheetModal(for: window)
                }
                #endif
            }
        }
    }

    private func configureContent() {
        guard let contentView = window?.contentView else { return }
        let title = NSTextField(labelWithString: "郵便番号から町域住所へ変換するためのデータです。")
        title.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .medium)

        let sourceButton = NSButton(title: "日本郵便の配布ページを開く", target: self, action: #selector(openSource(_:)))
        sourceButton.bezelStyle = .inline
        let revealButton = NSButton(title: "データフォルダを表示", target: self, action: #selector(revealData(_:)))
        updateButton.target = self
        updateButton.action = #selector(update(_:))

        hashLabel.lineBreakMode = .byTruncatingMiddle
        hashLabel.toolTip = "検証済み辞書スナップショットの SHA-256"
        // A degraded-data reason, durability warning and recovery notice can coexist.
        statusLabel.maximumNumberOfLines = 0
        let grid = NSGridView(views: [
            [NSTextField(labelWithString: "配布版"), sourceLabel],
            [NSTextField(labelWithString: "取得日時"), retrievedLabel],
            [NSTextField(labelWithString: "収録件数"), countLabel],
            [NSTextField(labelWithString: "SHA-256"), hashLabel],
        ])
        grid.column(at: 0).xPlacement = .trailing
        grid.column(at: 1).width = 410
        grid.rowSpacing = 7
        grid.columnSpacing = 12

        let buttons = NSStackView(views: [updateButton, sourceButton, revealButton])
        buttons.orientation = .horizontal
        buttons.spacing = 8
        let stack = NSStackView(views: [title, grid, buttons, statusLabel])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 20),
            stack.bottomAnchor.constraint(lessThanOrEqualTo: contentView.bottomAnchor, constant: -20),
            grid.widthAnchor.constraint(equalTo: stack.widthAnchor),
        ])
    }

    private func reload() {
        updateButton.isEnabled = store.canUpdate
        if store.startupState == .loading {
            statusLabel.stringValue = "郵便番号データを読み込んでいます…"
            statusLabel.textColor = .secondaryLabelColor
            return
        }
        guard let manifest = store.manifest else {
            sourceLabel.stringValue = "—"
            retrievedLabel.stringValue = "—"
            countLabel.stringValue = "0件"
            hashLabel.stringValue = "—"
            statusLabel.stringValue = store.statusMessages.isEmpty ? "郵便番号データがありません。" : store.statusMessages.joined(separator: "\n")
            statusLabel.textColor = .systemRed
            return
        }
        sourceLabel.stringValue = manifest.sourceLastModified
        retrievedLabel.stringValue = manifest.retrievedAt
        countLabel.stringValue = "\(manifest.entryCount)件"
        hashLabel.stringValue = manifest.snapshotSHA256
        if store.isUpdating {
            statusLabel.stringValue = "日本郵便から取得しています…"
            statusLabel.textColor = .secondaryLabelColor
        } else if !store.statusMessages.isEmpty {
            statusLabel.stringValue = store.statusMessages.joined(separator: "\n")
            statusLabel.textColor = store.loadError == nil ? .systemOrange : .systemRed
        } else {
            statusLabel.stringValue = "検証済みデータを使用しています。更新はこの画面から明示的に実行した場合だけ行います。"
            statusLabel.textColor = .secondaryLabelColor
        }
    }
}
