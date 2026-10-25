import AppKit

enum DictionaryWindowSupport {
    static func makeWindow(
        title: String,
        contentSize: NSSize,
        minimumSize: NSSize
    ) -> NSWindow {
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: contentSize),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = title
        window.minSize = minimumSize
        return window
    }

    static func makeTextCell(_ value: String) -> NSTableCellView {
        let cell = NSTableCellView()
        let label = NSTextField(labelWithString: value)
        label.translatesAutoresizingMaskIntoConstraints = false
        label.lineBreakMode = .byTruncatingTail
        cell.addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 4),
            label.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -4),
            label.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
        ])
        return cell
    }

    static func configureTable(
        _ tableView: NSTableView,
        columns: [(identifier: String, title: String, width: CGFloat)]
    ) -> NSScrollView {
        for columnDescription in columns {
            let column = NSTableColumn(
                identifier: NSUserInterfaceItemIdentifier(columnDescription.identifier)
            )
            column.title = columnDescription.title
            column.width = columnDescription.width
            tableView.addTableColumn(column)
        }
        tableView.usesAlternatingRowBackgroundColors = true
        tableView.allowsMultipleSelection = false
        let scrollView = NSScrollView()
        scrollView.documentView = tableView
        scrollView.hasVerticalScroller = true
        scrollView.borderType = .bezelBorder
        return scrollView
    }
}
