import Cocoa
import OSLog

// What the user is told after "delete learning". A precommit failure leaves
// current learning unchanged (some backups may be gone). A committed clear
// with a runtime warning emptied the saved file, but the running Core still
// has the old learning until the IME restarts. A durability warning means the
// clear is complete but its directory sync could not be confirmed. Logs carry
// only these fixed codes, never error text, file names or contents.
enum PersonalFrequencyClearNoticeKind: Equatable {
    case precommitFailure
    case committedWithRuntimeWarning
    case clearedWithDurabilityWarning

    var alertStyle: NSAlert.Style {
        switch self {
        case .precommitFailure:
            return .critical
        case .committedWithRuntimeWarning, .clearedWithDurabilityWarning:
            return .warning
        }
    }
}

struct PersonalFrequencyClearNotice: Equatable {
    let kind: PersonalFrequencyClearNoticeKind
    let messageText: String
    let informativeText: String
}

/// UI-only menu routing. Storage still owns its existing serialized clear.
@MainActor
enum InputMenuActions {
    private static let logger = Logger(subsystem: MuuConstants.bundleIdentifier, category: "controller")

    // Built each time IMK asks, so the About title's leading mark and its
    // accessibility label come from a fresh status capture.
    static func menu(target: MuuInputController) -> NSMenu {
        let menu = NSMenu(title: "Muu IME")
        let item = NSMenuItem(
            title: "ユーザー辞書…",
            action: #selector(MuuInputController.showUserDictionary(_:)),
            keyEquivalent: ""
        )
        item.target = target
        menu.addItem(item)
        let importedItem = NSMenuItem(
            title: "辞書の管理…",
            action: #selector(MuuInputController.showImportedDictionaries(_:)),
            keyEquivalent: ""
        )
        importedItem.target = target
        menu.addItem(importedItem)
        let postalItem = NSMenuItem(
            title: "郵便番号データ…",
            action: #selector(MuuInputController.showPostalCodeData(_:)),
            keyEquivalent: ""
        )
        postalItem.target = target
        menu.addItem(postalItem)
        menu.addItem(.separator())
        let clearLearningItem = NSMenuItem(
            title: "変換学習データを削除…",
            action: #selector(MuuInputController.clearPersonalFrequency(_:)),
            keyEquivalent: ""
        )
        clearLearningItem.target = target
        menu.addItem(clearLearningItem)
        menu.addItem(.separator())
        let status = AboutStatusProvider.shared.capture()
        let aboutItem = NSMenuItem(title: status.menuTitle,
            action: #selector(MuuInputController.showAboutMuu(_:)), keyEquivalent: "")
        aboutItem.target = target
        aboutItem.setAccessibilityLabel(status.accessibilityLabel)
        menu.addItem(aboutItem)
        return menu
    }

    static func showAboutMuu(_ sender: Any?) {
        // Opening only observes local facts. Downloads require an explicit
        // click. Return closes except while restart is pending, when it does
        // nothing; Escape can close that one-button About.
        AboutPresenter.shared.show()
    }

    static func showUserDictionary(_ sender: Any?) {
        UserDictionaryWindowController.shared.show()
    }

    static func showImportedDictionaries(_ sender: Any?) {
        ImportedDictionaryWindowController.shared.show()
    }

    static func showPostalCodeData(_ sender: Any?) {
        PostalCodeWindowController.shared.show()
    }

    static func clearPersonalFrequency(_ sender: Any?) {
        let confirmation = NSAlert()
        confirmation.messageText = "変換学習データを削除しますか？"
        confirmation.informativeText = "学習による頻度調整をすべて消し、辞書本来の変換順へ戻します。"
        confirmation.alertStyle = .warning
        confirmation.addButton(withTitle: "削除")
        confirmation.addButton(withTitle: "キャンセル")
        guard confirmation.runModal() == .alertFirstButtonReturn else { return }

        // The alert below needs the result of clearing learning, so this call
        // waits synchronously for the learning IO queue to finish.
        let result = PersonalFrequencyStore.shared.clear()
        switch result {
        case .cleared:
            return
        case .precommitFailure:
            logger.error("Personal frequency clear failed; code=learning_clear_failed")
        case .committedWithRuntimeWarning:
            logger.error("Personal frequency clear committed; code=learning_clear_runtime_failed")
        case .clearedWithDurabilityWarning:
            logger.error("Personal frequency clear committed; code=learning_clear_durability_warning")
        }
        guard let notice = Self.personalFrequencyClearNotice(for: result) else { return }
        let alert = NSAlert()
        alert.messageText = notice.messageText
        alert.informativeText = notice.informativeText
        alert.alertStyle = notice.kind.alertStyle
        alert.runModal()
    }

    static func personalFrequencyClearNotice(
        for result: PersonalFrequencyClearResult
    ) -> PersonalFrequencyClearNotice? {
        switch result {
        case .cleared:
            return nil
        case .precommitFailure:
            return PersonalFrequencyClearNotice(
                kind: .precommitFailure,
                messageText: "変換学習データを削除できませんでした",
                informativeText: "現在の学習状態は変更されていません。"
            )
        case .clearedWithDurabilityWarning:
            return PersonalFrequencyClearNotice(
                kind: .clearedWithDurabilityWarning,
                messageText: "変換学習データを削除しました",
                informativeText: "消去は完了しましたが、耐久性を確認できませんでした。"
            )
        case .committedWithRuntimeWarning:
            return PersonalFrequencyClearNotice(
                kind: .committedWithRuntimeWarning,
                messageText: "変換学習データを削除しました",
                informativeText: "保存済みの学習データは削除しましたが、動作中の変換には反映できませんでした。入力メソッドを再起動すると、空の学習から始まります。"
            )
        }
    }
}
