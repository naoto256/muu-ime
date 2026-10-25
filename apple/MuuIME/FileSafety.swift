import CryptoKit
import Darwin
import Foundation

enum FileSafety {
    typealias POSIXErrorFactory = (_ operation: String, _ code: Int32) -> Error

    struct ReadResult {
        let data: Data
        let modificationDate: Date?
    }

    // Raw values are the Rust StoredFileState wire names sent to Core.
    enum StoredFileState: String { case missing, present, unreadable }
    struct StoredFileRead {
        let state: StoredFileState
        let bytes: Data
        let error: Error?
    }

    // Only an ENOENT from open means absence. Validation, fstat and read
    // failures are unreadable, preserving the caller's original error.
    // An ENOENT reported by validation is unreadable too: only the open of
    // this path proves the file is absent. Empty bytes are present.
    //
    // The three stores share this classification and keep their own
    // validation, bounds and error factories. The factory's Error is the
    // caller's type, so absence is noted where the factory is called rather
    // than matched from the thrown error.
    static func readState(
        _ url: URL, maxBytes: Int, validate: () throws -> Void,
        unsupportedFileType: () -> Error, fileTooLarge: () -> Error,
        posixError: POSIXErrorFactory
    ) -> StoredFileRead {
        var missing = false
        do {
            try validate()
            let bytes = try boundedRead(url, maxBytes: maxBytes,
                unsupportedFileType: unsupportedFileType, fileTooLarge: fileTooLarge,
                posixError: { operation, code in
                    missing = operation == "open-read" && code == ENOENT
                    return posixError(operation, code)
                })
            return StoredFileRead(state: .present, bytes: bytes, error: nil)
        } catch {
            return StoredFileRead(state: missing ? .missing : .unreadable, bytes: Data(), error: missing ? nil : error)
        }
    }

    static func boundedRead(
        _ url: URL,
        maxBytes: Int,
        unsupportedFileType: () -> Error,
        fileTooLarge: () -> Error,
        posixError: POSIXErrorFactory
    ) throws -> Data {
        try boundedReadWithMetadata(url, maxBytes: maxBytes,
            unsupportedFileType: unsupportedFileType, fileTooLarge: fileTooLarge,
            posixError: posixError).data
    }

    /// Metadata belongs to the same NOFOLLOW, regular-file descriptor as the
    /// bytes. A timestamp is only an observation, never persistence authority.
    static func boundedReadWithMetadata(
        _ url: URL,
        maxBytes: Int,
        unsupportedFileType: () -> Error,
        fileTooLarge: () -> Error,
        posixError: POSIXErrorFactory,
        modificationDate: (stat) -> Date? = { metadata in
            let seconds = Double(metadata.st_mtimespec.tv_sec)
                + Double(metadata.st_mtimespec.tv_nsec) / 1_000_000_000
            return seconds.isFinite ? Date(timeIntervalSince1970: seconds) : nil
        }
    ) throws -> ReadResult {
        let descriptor = Darwin.open(
            url.path,
            O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK
        )
        guard descriptor >= 0 else { throw posixError("open-read", errno) }
        defer { _ = Darwin.close(descriptor) }

        var metadata = stat()
        guard Darwin.fstat(descriptor, &metadata) == 0 else {
            throw posixError("fstat-read", errno)
        }
        guard metadata.st_mode & S_IFMT == S_IFREG else {
            throw unsupportedFileType()
        }
        guard metadata.st_size >= 0,
              UInt64(metadata.st_size) <= UInt64(maxBytes)
        else {
            throw fileTooLarge()
        }
        // Taken from the fstat that admitted the bytes, before reading them.
        // A failing fstat still rejects the read, so only the injected
        // conversion can yield nil here.
        let observedDate = modificationDate(metadata)

        var data = Data()
        data.reserveCapacity(Int(metadata.st_size))
        var buffer = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            let remaining = maxBytes - data.count
            let readLimit = min(buffer.count, remaining + 1)
            let result = buffer.withUnsafeMutableBytes { bytes in
                Darwin.read(descriptor, bytes.baseAddress, readLimit)
            }
            if result < 0, errno == EINTR { continue }
            guard result >= 0 else { throw posixError("read", errno) }
            if result == 0 { return ReadResult(data: data, modificationDate: observedDate) }
            guard result <= remaining else { throw fileTooLarge() }
            buffer.withUnsafeBytes { bytes in
                data.append(bytes.bindMemory(to: UInt8.self).baseAddress!, count: result)
            }
        }
    }

    static func writeAll(
        _ data: Data,
        to descriptor: Int32,
        operation: String,
        posixError: POSIXErrorFactory
    ) throws {
        try data.withUnsafeBytes { bytes in
            var written = 0
            while written < bytes.count {
                let result = Darwin.write(
                    descriptor,
                    bytes.baseAddress!.advanced(by: written),
                    bytes.count - written
                )
                if result < 0, errno == EINTR { continue }
                guard result > 0 else { throw posixError(operation, errno) }
                written += result
            }
        }
    }

    static func fsyncDirectory(
        _ directory: URL,
        openFlags: Int32 = O_RDONLY | O_CLOEXEC | O_DIRECTORY,
        posixError: POSIXErrorFactory
    ) throws {
        let descriptor = Darwin.open(directory.path, openFlags)
        guard descriptor >= 0 else { throw posixError("open-directory", errno) }
        defer { _ = Darwin.close(descriptor) }
        guard Darwin.fsync(descriptor) == 0 else {
            throw posixError("fsync-directory", errno)
        }
    }

    /// Removes only eligible direct regular-file children not named in `keeping`.
    /// Ineligible names, symlinks, directories, and other file types are retained.
    static func removeUnreferencedRegularFiles(
        in directory: URL,
        keeping: Set<String>,
        eligible: (String) -> Bool,
        posixError: POSIXErrorFactory
    ) throws {
        let descriptor = Darwin.open(
            directory.path,
            O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW
        )
        guard descriptor >= 0 else { throw posixError("open-prune-directory", errno) }
        defer { _ = Darwin.close(descriptor) }

        // Names are listed by path, but every check and unlink goes through
        // the NOFOLLOW descriptor opened above.
        let names = try FileManager.default.contentsOfDirectory(atPath: directory.path)
        var removed = false
        for name in names where !keeping.contains(name) && eligible(name) {
            var metadata = stat()
            let result = Darwin.fstatat(descriptor, name, &metadata, AT_SYMLINK_NOFOLLOW)
            if result != 0, errno == ENOENT { continue }
            guard result == 0 else { throw posixError("fstatat-prune", errno) }
            guard metadata.st_mode & S_IFMT == S_IFREG else { continue }
            guard Darwin.unlinkat(descriptor, name, 0) == 0 else {
                if errno == ENOENT { continue }
                throw posixError("unlinkat-prune", errno)
            }
            removed = true
        }
        if removed, Darwin.fsync(descriptor) != 0 {
            throw posixError("fsync-prune-directory", errno)
        }
    }

    /// Returns a warning only after the destination rename has committed.
    /// Every throw happens before the rename, so a caller that catches one
    /// knows the destination still holds its previous contents (or is absent).
    ///
    /// This primitive syncs the file and its containing directory. Managed
    /// stores call `ensurePrivateDirectories` first, with their explicit base,
    /// to make every ancestor private and its parent link durable too. Called
    /// on its own, it still creates missing parents with the default mode and
    /// makes only the containing directory 0700, without syncing those links.
    static func atomicWrite(
        _ data: Data,
        to destination: URL,
        temporaryPrefix: String,
        directoryOpenFlags: Int32 = O_RDONLY | O_CLOEXEC | O_DIRECTORY,
        beforeRename: (() throws -> Void)? = nil,
        afterRename: (() -> Error?)? = nil,
        checkpoint: ((String) throws -> Void)? = nil,
        posixError: POSIXErrorFactory
    ) throws -> Error? {
        let directory = destination.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        guard Darwin.chmod(directory.path, 0o700) == 0 else {
            throw posixError("chmod-directory", errno)
        }
        let temporary = directory.appendingPathComponent(
            "\(temporaryPrefix).\(getpid()).\(UUID().uuidString).tmp"
        )
        // Deterministic transaction tests can fail each syscall boundary
        // without relying on permission changes or the timing of disk faults.
        try checkpoint?("open_temporary")
        var descriptor = Darwin.open(temporary.path, O_WRONLY | O_CREAT | O_EXCL, 0o600)
        guard descriptor >= 0 else { throw posixError("open-temporary", errno) }
        var shouldUnlink = true
        defer {
            if descriptor >= 0 { _ = Darwin.close(descriptor) }
            if shouldUnlink { _ = Darwin.unlink(temporary.path) }
        }
        try checkpoint?("write_temporary")
        try writeAll(
            data,
            to: descriptor,
            operation: "write-temporary",
            posixError: posixError
        )
        try checkpoint?("fsync_temporary")
        guard Darwin.fsync(descriptor) == 0 else {
            throw posixError("fsync-temporary", errno)
        }
        try checkpoint?("close_temporary")
        guard Darwin.close(descriptor) == 0 else {
            throw posixError("close-temporary", errno)
        }
        descriptor = -1
        try beforeRename?()
        guard Darwin.rename(temporary.path, destination.path) == 0 else {
            throw posixError("rename", errno)
        }
        shouldUnlink = false
        if let warning = afterRename?() { return warning }
        do {
            try checkpoint?("fsync_directory")
            try fsyncDirectory(
                directory,
                openFlags: directoryOpenFlags,
                posixError: posixError
            )
            return nil
        } catch {
            return error
        }
    }

    static func rejectSymbolicLink(
        _ url: URL,
        fileManager: FileManager,
        symbolicLinkError: (String) -> Error
    ) throws {
        if (try? fileManager.destinationOfSymbolicLink(atPath: url.path)) != nil {
            throw symbolicLinkError(url.path)
        }
    }

    static func rejectSymbolicLinkAncestors(
        of url: URL,
        fileManager: FileManager,
        nonAbsolutePathError: () -> Error,
        symbolicLinkError: (String) -> Error
    ) throws {
        let components = url.standardizedFileURL.pathComponents
        guard components.first == "/" else { throw nonAbsolutePathError() }
        var current = URL(fileURLWithPath: "/", isDirectory: true)
        try rejectSymbolicLink(
            current,
            fileManager: fileManager,
            symbolicLinkError: symbolicLinkError
        )
        for component in components.dropFirst() {
            current.appendPathComponent(component)
            try rejectSymbolicLink(
                current,
                fileManager: fileManager,
                symbolicLinkError: symbolicLinkError
            )
        }
    }

    /// Creates private managed directories and persists their links before a
    /// multi-file stage can depend on them. Retries sync existing links too:
    /// an earlier mkdir may have succeeded while its parent's fsync failed.
    /// The base must already be a directory; it belongs to the caller and is
    /// never created or chmod'ed here.
    /// Transaction order, publication, rollback, and GC remain in each store.
    ///
    /// Before any side effect, `base` must exist as a directory. The check
    /// uses lstat, so a symlinked base is refused as well, and a refusal
    /// reports no checkpoint. Because `base` exists, the missing-directory
    /// walk below stops at or under it and never creates anything above it.
    /// A store whose base is missing therefore fails every save as an
    /// unsupported path; nothing here creates it (in production the base is
    /// Application Support, which the system provides).
    ///
    /// Then two passes. First, each missing directory below `base`, from the
    /// top down, is made with mode 0700 and its parent fsynced. Then every
    /// directory from `target` up to, but not including, `base` is chmodded
    /// to 0700 and fsynced together with its parent; the last parent synced
    /// is `base`, whose own link in its parent stays the caller's. The second pass runs
    /// on every call, so each save re-syncs the whole chain rather than
    /// trusting an earlier run. Each sync first reports its operation name
    /// (`create_directory`, `directory_parent_fsync`, `directory_fsync`) to
    /// `checkpoint`, and any failure throws before the caller's rename.
    /// Tests observe these checkpoints, not execution of the fsync syscall.
    static func ensurePrivateDirectories(
        _ target: URL,
        below base: URL,
        unsupportedPath: () -> Error,
        checkpoint: ((String) throws -> Void)? = nil,
        posixError: POSIXErrorFactory
    ) throws {
        func safe(_ url: URL) throws {
            try rejectSymbolicLinkAncestors(
                of: url, fileManager: .default, nonAbsolutePathError: unsupportedPath,
                symbolicLinkError: { _ in unsupportedPath() })
        }
        func sync(_ url: URL, operation: String) throws {
            try checkpoint?(operation)
            try fsyncDirectory(
                url, openFlags: O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
                posixError: { _, code in posixError(operation, code) })
        }
        guard target.path.hasPrefix(base.path + "/") else { throw unsupportedPath() }
        try safe(target)
        // Check the caller-owned boundary before any side effect. Otherwise
        // the missing-parent walk could create and sync ancestors above base.
        var baseMetadata = stat()
        guard lstat(base.path, &baseMetadata) == 0,
              baseMetadata.st_mode & S_IFMT == S_IFDIR else { throw unsupportedPath() }
        var missing: [URL] = []
        var cursor = target
        while !FileManager.default.fileExists(atPath: cursor.path) {
            missing.append(cursor)
            let parent = cursor.deletingLastPathComponent()
            guard parent != cursor else { throw unsupportedPath() }
            cursor = parent
        }
        for directory in missing.reversed() {
            try checkpoint?("create_directory")
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: false,
                attributes: [.posixPermissions: 0o700])
            try sync(directory.deletingLastPathComponent(), operation: "directory_parent_fsync")
        }
        cursor = target
        while cursor.path.hasPrefix(base.path + "/") {
            try safe(cursor)
            guard Darwin.chmod(cursor.path, 0o700) == 0 else {
                throw posixError("chmod_directory", errno)
            }
            try sync(cursor, operation: "directory_fsync")
            try sync(cursor.deletingLastPathComponent(), operation: "directory_parent_fsync")
            cursor = cursor.deletingLastPathComponent()
        }
    }

    // The same spelling as Rust muu-digest. Imported and Postal check names
    // and hashes that Core produced, and compare them as strings, so the two
    // sides must agree. What each hash covers stays with its store.
    static func hexLower<Bytes: Sequence>(_ bytes: Bytes) -> String where Bytes.Element == UInt8 {
        bytes.map { String(format: "%02x", $0) }.joined()
    }

    // Rejects uppercase rather than folding it, as Rust does: one digest has
    // one spelling.
    static func isLowercaseSHA256(_ value: String) -> Bool {
        value.utf8.count == 64
            && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }

    static func sha256(_ data: Data) -> String {
        hexLower(SHA256.hash(data: data))
    }
}
