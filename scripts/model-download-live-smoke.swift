
// The manual recipe appends this entry to the unchanged ModelDownload.swift.
// Same-file access reuses its private downloader/configuration without a test
// flag, alternate endpoint, or a production API added only for this tool.
extension ModelDownloadStore {
    nonisolated fileprivate static func liveSmokeConfiguration() -> URLSessionConfiguration {
        liveConfiguration()
    }
}

@main
private enum ModelDownloadLiveSmoke {
    private enum Failure: String, Error { case usage, destination, timeout, inspection, verification }

    static func main() {
        do { try run() }
        catch {
            let code = (error as? Failure)?.rawValue ?? (error as? ModelDownloadFailure)?.rawValue ?? "io"
            fputs("model-download-live-smoke: \(code)\n", stderr)
            exit(1)
        }
    }

    private static func destination() throws -> URL {
        guard CommandLine.arguments.count == 2, CommandLine.arguments[1].hasPrefix("/") else { throw Failure.usage }
        let raw = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
        var rawPath = raw.path
        while rawPath.hasSuffix("/") { rawPath.removeLast() }
        let prefix = "muu-model-live."
        guard raw.lastPathComponent.hasPrefix(prefix), raw.lastPathComponent.count > prefix.count else { throw Failure.destination }
        let build = URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true)
            .appendingPathComponent("build", isDirectory: true)
        var buildMetadata = stat()
        guard lstat(build.path, &buildMetadata) == 0, buildMetadata.st_mode & S_IFMT == S_IFDIR else { throw Failure.destination }
        let root = try physicalDirectory(rawPath)
        let parent = try physicalDirectory(build.path)
        guard root.deletingLastPathComponent().path == parent.path else { throw Failure.destination }
        var metadata = stat()
        guard lstat(rawPath, &metadata) == 0, metadata.st_mode & S_IFMT == S_IFDIR,
              metadata.st_uid == geteuid(), metadata.st_mode & 0o7777 == 0o700 else { throw Failure.destination }
        // Foundation standardization can turn /private/tmp back into /tmp.
        // Use checkout-local mktemp directories and the production guard itself.
        //
        // Why only build/ in this checkout: the production guard walks
        // `standardizedFileURL`, and Foundation standardizes /private/tmp
        // back to its symlink alias /tmp. A
        // destination in the OS temporary tree would therefore be refused by
        // production itself, correctly. So the destination must be an
        // existing, empty, owner-only (0700) `muu-model-live.*` directory whose
        // physical parent (realpath) is the checkout's real build/ directory,
        // and it must pass the same ancestor guard the app uses.
        try FileSafety.rejectSymbolicLinkAncestors(of: root, fileManager: .default,
            nonAbsolutePathError: { Failure.destination }, symbolicLinkError: { _ in Failure.destination })
        guard try FileManager.default.contentsOfDirectory(atPath: root.path).isEmpty else { throw Failure.destination }
        return root
    }

    private static func physicalDirectory(_ path: String) throws -> URL {
        guard let resolved = realpath(path, nil) else { throw Failure.destination }
        defer { free(resolved) }
        return URL(fileURLWithPath: String(cString: resolved), isDirectory: true)
    }

    private static func run() throws {
        // Validate before constructing any downloader. Never resolve the app's
        // Application Support location or construct the preferences-owning Store.
        let root = try destination()
        let outcome = Mutex<Result<Void, ModelDownloadFailure>?>(nil)
        let finished = DispatchSemaphore(value: 0)
        let attempt = ModelDownloadAttempt(profile: .zenz, support: root,
            configuration: ModelDownloadStore.liveSmokeConfiguration, checkpoint: { _ in }, completion: { event in
                if case .finished(let result) = event {
                    outcome.withLock { $0 = result }
                    finished.signal()
                }
            })
        attempt.start()
        // One minute past the session's 15-minute resource limit, so the
        // production timeout, not this wait, normally ends a stuck transfer.
        guard finished.wait(timeout: .now() + 16 * 60) == .success else {
            attempt.cancel()
            throw Failure.timeout
        }
        let report = try inspect(root)
        let json = try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys])
        print(String(decoding: json, as: UTF8.self))
        try outcome.withLock { result in
            guard let result else { throw Failure.inspection }
            try result.get()
        }
        guard report["received_bytes"] as? Int == ModelDownloadProfile.zenz.byteCount,
              report["sha256_matches"] as? Bool == true, report["permissions"] as? String == "0600",
              report["partials_remaining"] as? Int == 0 else { throw Failure.verification }
    }

    private static func inspect(_ root: URL) throws -> [String: Any] {
        let profile = ModelDownloadProfile.zenz
        let directory = root.appendingPathComponent("MuuIME/Models").appendingPathComponent(profile.name)
        let file = directory.appendingPathComponent(profile.fileName)
        var report: [String: Any] = ["received_bytes": NSNull(), "sha256_matches": NSNull(), "permissions": NSNull()]
        let entries = try FileManager.default.subpathsOfDirectory(atPath: root.path)
        report["partials_remaining"] = entries.filter { $0.hasSuffix(".partial") }.count
        let descriptor = open(file.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)
        if descriptor < 0 {
            guard errno == ENOENT else { throw Failure.inspection }
            return report
        }
        defer { close(descriptor) }
        var metadata = stat()
        guard fstat(descriptor, &metadata) == 0, metadata.st_mode & S_IFMT == S_IFREG else { throw Failure.inspection }
        let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: false)
        var digest = SHA256()
        var count = 0
        while let chunk = try handle.read(upToCount: 64 * 1024), !chunk.isEmpty {
            count += chunk.count
            guard count <= profile.byteCount else { throw Failure.verification }
            digest.update(data: chunk)
        }
        // The destination began empty, and production accepts a complete fixed
        // length body only. A successful final file therefore counts this run's
        // received bytes; on failure without a final file the count is unknown.
        report["received_bytes"] = count
        report["sha256_matches"] = digest.finalize().map { String(format: "%02x", $0) }.joined() == profile.sha256
        report["permissions"] = String(format: "%04o", metadata.st_mode & 0o7777)
        return report
    }
}
