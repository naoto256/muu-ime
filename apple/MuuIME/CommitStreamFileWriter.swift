import Foundation
import Darwin

/// Serial, record-at-a-time IO. Numbered generations survive partial failures;
/// the lowest hole, not an in-memory progress counter, is the restart journal.
/// There is no per-record fsync contract: success means a complete framed write
/// (and exclusive rename for a fresh 0), as with the previous recorder.
final class CommitStreamFileWriter {
    static let maximumFileBytes = 8 * 1024 * 1024
    static let fileCount = 8
    static let lockName = ".commit-stream-v5.lock"
    static let temporaryName = ".commit-stream-v5-next.tmp"
    private let directory: URL
    private let checkpoint: (String) throws -> Void
    private let writePart: (Data, Int32, String) throws -> Void
    private let tailReader: ((URL) throws -> UInt8?)?
    private var tainted = false
    private var knownEnd: (device: dev_t, inode: ino_t, size: off_t)?

    init(directory: URL, checkpoint: @escaping (String) throws -> Void = { _ in },
         tailReader: ((URL) throws -> UInt8?)? = nil,
         writePart: @escaping (Data, Int32, String) throws -> Void = { data, fd, phase in
             try FileSafety.writeAll(data, to: fd, operation: phase, posixError: { _, code in POSIXError(POSIXErrorCode(rawValue: code) ?? .EIO) })
         }) {
        self.directory = directory; self.checkpoint = checkpoint
        self.tailReader = tailReader; self.writePart = writePart
    }
    func write(_ record: Data) throws {
        // Ingress has already applied this check. If it ever failed here,
        // delivery would treat `.malformed` as an ordinary IO retry.
        guard CommitStreamDelivery.isFramedRecord(record) else { throw CommitStreamDeliveryFailure.malformed }
        try prepareDirectory()
        let dir = Darwin.open(directory.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
        guard dir >= 0 else { throw openError() }
        defer { _ = Darwin.close(dir) }
        let lease = openat(dir, Self.lockName, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK, 0o600)
        guard lease >= 0 else { throw openError() }
        defer { _ = Darwin.close(lease) }
        guard try regular(lease).st_size == 0 else { throw CommitStreamDeliveryFailure.unsafePath }
        guard fchmod(lease, 0o600) == 0 else { throw posixError() }
        guard flock(lease, LOCK_EX | LOCK_NB) == 0 else { throw posixError() }
        defer { _ = flock(lease, LOCK_UN) }
        try checkpoint("locked")

        // Only the fixed private temp is disposable. A symlink, directory, or
        // oversized imposter is never followed/deleted. Lock precedes cleanup.
        try removeTemporary(in: dir)
        let zero = name(0)
        if try metadata(zero, in: dir) != nil {
            let fd = openat(dir, zero, O_RDWR | O_APPEND | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK)
            guard fd >= 0 else { throw openError() }
            defer { _ = Darwin.close(fd) }
            let info = try regular(fd)
            guard fchmod(fd, 0o600) == 0 else { throw posixError() }
            if info.st_size == 0 {
                try unlink(zero, in: dir, phase: "remove_empty")
                knownEnd = nil
            } else {
                let isKnown = knownEnd.map { $0.device == info.st_dev && $0.inode == info.st_ino && $0.size == info.st_size } ?? false
                if !tainted && !isKnown {
                    do {
                        try checkpoint("read_tail")
                        let last: UInt8?
                        if let tailReader { last = try tailReader(directory.appendingPathComponent(zero)) }
                        else {
                            var byte: UInt8 = 0
                            var n: Int
                            repeat { n = pread(fd, &byte, 1, info.st_size - 1) } while n < 0 && errno == EINTR
                            guard n == 1 else { throw POSIXError(.EIO) }
                            last = byte
                        }
                        tainted = last != 10
                    } catch {
                        // This FD was proven regular, but its tail is unknown.
                        // Retry can preserve it as an older generation instead
                        // of concatenating a line onto unproven bytes.
                        tainted = true
                        throw error
                    }
                }
                if tainted || info.st_size > off_t(Self.maximumFileBytes - record.count - 1) {
                    try rotate(in: dir)
                    knownEnd = nil
                } else {
                    // If only the LF fails, a complete record stays as the
                    // unterminated tail of an older generation, and the retry
                    // writes it again. A reader that keeps such a tail sees
                    // the record twice.
                    do {
                        try checkpoint("append_body")
                        try writePart(record, fd, "append_body")
                        try checkpoint("append_lf")
                        try writePart(Data([10]), fd, "append_lf")
                    } catch { tainted = true; knownEnd = nil; throw error }
                    knownEnd = (info.st_dev, info.st_ino, info.st_size + off_t(record.count + 1))
                    return // Complete line: cleanup cannot reverse success.
                }
            }
        }
        try createFirstLine(record, in: dir)
    }
    private func createFirstLine(_ record: Data, in dir: Int32) throws {
        try checkpoint("create_temp")
        let fd = openat(dir, Self.temporaryName, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard fd >= 0 else { throw openError(conflictIsUnsafe: true) }
        defer { _ = Darwin.close(fd) }
        let info = try regular(fd)
        guard fchmod(fd, 0o600) == 0 else { throw posixError() }
        try checkpoint("fresh_body")
        try writePart(record, fd, "fresh_body")
        try checkpoint("fresh_lf")
        try writePart(Data([10]), fd, "fresh_lf")
        try checkpoint("publish_zero")
        try rename(Self.temporaryName, to: name(0), in: dir)
        tainted = false
        knownEnd = (info.st_dev, info.st_ino, off_t(record.count + 1))
        // After this exclusive rename, the record is successful. In particular
        // close/logging/cleanup must not turn it into a retry or returned debt.
    }
    private func rotate(in dir: Int32) throws {
        while try metadata(name(0), in: dir) != nil {
            var hole: Int?
            for index in 1..<Self.fileCount {
                if try metadata(name(index), in: dir) == nil, hole == nil { hole = index }
            }
            if hole == nil {
                try unlink(name(7), in: dir, phase: "remove_oldest")
                hole = 7
            }
            // The lowest hole has a present predecessor. On any failed rename
            // the filesystem still describes the next step; no second oldest
            // deletion is possible until a new 0 has actually succeeded.
            for source in stride(from: hole! - 1, through: 0, by: -1) {
                try checkpoint("rotate_\(source)_before")
                guard try metadata(name(source), in: dir) != nil else { throw CommitStreamDeliveryFailure.unsafePath }
                try rename(name(source), to: name(source + 1), in: dir)
                try checkpoint("rotate_\(source)_after")
            }
        }
    }
    private func removeTemporary(in dir: Int32) throws {
        guard let info = try metadata(Self.temporaryName, in: dir) else { return }
        guard info.st_size >= 0, info.st_size <= off_t(CommitStreamDelivery.maximumBytes + 1) else { throw CommitStreamDeliveryFailure.unsafePath }
        try unlink(Self.temporaryName, in: dir, phase: "remove_temp")
    }
    private func regular(_ fd: Int32) throws -> stat {
        var info = stat()
        guard fstat(fd, &info) == 0 else { throw posixError() }
        guard info.st_mode & S_IFMT == S_IFREG, info.st_size >= 0 else { throw CommitStreamDeliveryFailure.unsafePath }
        return info
    }
    private func metadata(_ name: String, in dir: Int32) throws -> stat? {
        var info = stat()
        if fstatat(dir, name, &info, AT_SYMLINK_NOFOLLOW) != 0 {
            if errno == ENOENT { return nil }
            throw posixError()
        }
        guard info.st_mode & S_IFMT == S_IFREG, info.st_size >= 0 else { throw CommitStreamDeliveryFailure.unsafePath }
        return info
    }
    private func unlink(_ name: String, in dir: Int32, phase: String) throws {
        guard try metadata(name, in: dir) != nil else { return }
        try checkpoint(phase + "_before")
        guard unlinkat(dir, name, 0) == 0 else { throw posixError() }
        try checkpoint(phase + "_after")
    }
    private func rename(_ source: String, to destination: String, in dir: Int32) throws {
        guard renameatx_np(dir, source, dir, destination, UInt32(RENAME_EXCL)) == 0 else { throw openError(conflictIsUnsafe: true) }
    }
    private func name(_ index: Int) -> String { "commit-stream-v5-\(index).jsonl" }
    private func prepareDirectory() throws {
        try FileSafety.rejectSymbolicLinkAncestors(of: directory, fileManager: .default,
            nonAbsolutePathError: { CommitStreamDeliveryFailure.unsafePath }, symbolicLinkError: { _ in CommitStreamDeliveryFailure.unsafePath })
        var cursor = URL(fileURLWithPath: "/", isDirectory: true)
        for part in directory.standardizedFileURL.pathComponents.dropFirst() {
            cursor.appendPathComponent(part)
            var info = stat()
            if lstat(cursor.path, &info) == 0 {
                guard info.st_mode & S_IFMT == S_IFDIR else { throw CommitStreamDeliveryFailure.unsafePath }
            } else if errno != ENOENT { throw openError() }
        }
        try checkpoint("prepare_directory")
        // 0700 applies to the directories created here; the leaf is chmodded
        // again below, and existing ancestors keep their mode.
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let fd = Darwin.open(directory.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
        guard fd >= 0 else { throw openError() }
        defer { _ = Darwin.close(fd) }
        guard fchmod(fd, 0o700) == 0 else { throw posixError() }
    }
    private func posixError() -> Error { POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
    private func openError(conflictIsUnsafe: Bool = false) -> Error {
        if errno == ELOOP || errno == ENOTDIR || errno == EISDIR || (conflictIsUnsafe && errno == EEXIST) { return CommitStreamDeliveryFailure.unsafePath }
        return posixError()
    }
}
