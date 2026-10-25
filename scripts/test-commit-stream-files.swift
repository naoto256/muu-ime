import Foundation
import Darwin

@main
private enum CommitStreamFileTests {
    static let fm = FileManager.default
    // The fixtures live under ~/Library/Caches rather than the system temp
    // directory, whose /var alias FileSafety refuses as a symlinked ancestor.
    // They are removed on exit; a precondition failure leaves them behind.
    static func main() throws {
        let root = fm.homeDirectoryForCurrentUser.appendingPathComponent("Library/Caches/muu-commit-file-tests").resolvingSymlinksInPath().appendingPathComponent(UUID().uuidString)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: root) }
        try framing(root)
        try everyRotationFailure(root)
        try sparseAndRepeatedFailures(root)
        try partialWrites(root)
        try safetyAndLock(root)
        print("commit files: framing, resumable rotation, repeated/restarted failures, partial writes, safety and lock passed")
    }
    static func path(_ directory: URL, _ index: Int) -> URL { directory.appendingPathComponent("commit-stream-v5-\(index).jsonl") }
    static func data(_ url: URL) throws -> Data { try Data(contentsOf: url) }
    static func setup(_ root: URL, _ name: String, indices: [Int] = Array(0..<8)) throws -> URL {
        let directory = root.appendingPathComponent(name)
        try fm.createDirectory(at: directory, withIntermediateDirectories: true)
        for i in indices { try Data("old\(i)".utf8).write(to: path(directory,i)) }
        try Data("sentinel".utf8).write(to: directory.appendingPathComponent("keep.jsonl"))
        return directory
    }
    static func expectFailure(_ action: () throws -> Void) {
        do { try action(); preconditionFailure("expected injected failure") } catch {}
    }
    // Each record is written as its bytes plus LF, bytes unchanged. A current
    // file ending mid-line is rotated away before the next record, so a record
    // never continues someone else's partial line; the rotation point is the
    // first record that would not fit the size limit.
    static func framing(_ root: URL) throws {
        for (name, initial, rotated) in [("empty",Data(),false),("lf",Data("old\n".utf8),false),("partial",Data("{\"interrupted\":".utf8),true)] {
            let dir = try setup(root,name,indices: [])
            try initial.write(to:path(dir,0))
            let writer = CommitStreamFileWriter(directory:dir)
            try writer.write(Data("{\"next\":1}".utf8)); try writer.write(Data([255,65]))
            let expected = (rotated ? Data() : initial) + Data("{\"next\":1}\n".utf8) + Data([255,65,10])
            let bytes = try data(path(dir,0)); precondition(bytes == expected)
            if rotated { let old = try data(path(dir,1)); precondition(old == initial) }
            else { precondition(!fm.fileExists(atPath:path(dir,1).path)) }
        }
        let dir = try setup(root,"boundary",indices:[])
        try Data(repeating:65,count:CommitStreamFileWriter.maximumFileBytes - 3).appendingLF.write(to:path(dir,0))
        let writer = CommitStreamFileWriter(directory:dir)
        try writer.write(Data([66])); precondition(!fm.fileExists(atPath:path(dir,1).path))
        try writer.write(Data([67])); let old = try data(path(dir,1)); precondition(old.count == CommitStreamFileWriter.maximumFileBytes)
    }
    // A failure injected at every rotation step, with and without a restart:
    // the next write still lands in a fresh file 0, files 1-7 are the old 0-6
    // in order, and an unrelated file in the directory is untouched.
    static func everyRotationFailure(_ root: URL) throws {
        var phases = ["remove_oldest_before","remove_oldest_after","create_temp","fresh_body","fresh_lf","publish_zero"]
        for i in stride(from:6,through:0,by:-1) { phases += ["rotate_\(i)_before","rotate_\(i)_after"] }
        for (test,phase) in phases.enumerated() {
            for restart in [false,true] {
                let dir = try setup(root,"failure-\(test)-\(restart)")
                var injected = false
                var writer = CommitStreamFileWriter(directory:dir,checkpoint:{ at in
                    if at == phase && !injected { injected = true; throw POSIXError(.EIO) }
                })
                expectFailure { try writer.write(Data([65])) }
                precondition(injected)
                if restart { writer = CommitStreamFileWriter(directory:dir) }
                try writer.write(Data([66]))
                let fresh = try data(path(dir,0)); precondition(fresh == Data([66,10]))
                for i in 0..<7 { let old = try data(path(dir,i+1)); precondition(old == Data("old\(i)".utf8)) }
                let sentinel = try data(dir.appendingPathComponent("keep.jsonl")); precondition(sentinel == Data("sentinel".utf8))
            }
        }
    }
    // Every combination of missing older files, with the first write failing
    // repeatedly: at most eight files (plus one temp) ever exist, the
    // surviving ones keep their contents, and a later writer recovers.
    static func sparseAndRepeatedFailures(_ root: URL) throws {
        for mask in 0..<128 {
            let indices = [0] + (1..<8).filter { mask & (1 << ($0-1)) != 0 }
            let dir = try setup(root,"sparse-\(mask)",indices:indices)
            var failures = 0
            var writer = CommitStreamFileWriter(directory:dir,checkpoint:{ phase in
                if phase == "fresh_body" { failures += 1; throw POSIXError(.ENOSPC) }
            })
            let attempts = mask == 127 ? 100 : 10
            for _ in 0..<attempts { expectFailure { try writer.write(Data([65])) } }
            precondition(failures == attempts)
            let names = try fm.contentsOfDirectory(atPath:dir.path).filter { $0.hasPrefix("commit-stream-v5-") }
            let surviving = try names.sorted().map { try data(dir.appendingPathComponent($0)) }
            let expectedIndices = indices.count == 8 ? Array(indices.dropLast()) : indices
            precondition(surviving == expectedIndices.map { Data("old\($0)".utf8) })
            let all = try fm.contentsOfDirectory(atPath:dir.path)
            precondition(names.count + all.filter { $0 == CommitStreamFileWriter.temporaryName }.count <= 8)
            writer = CommitStreamFileWriter(directory:dir)
            try writer.write(Data([66]))
        }
    }
    // A write that fails after part of the body, or after the body but before
    // its LF, leaves an incomplete line; the next record goes to a fresh file
    // 0 and the incomplete bytes are kept as an older file. A temp file that
    // cannot be removed blocks writing without creating file 0.
    static func partialWrites(_ root: URL) throws {
        for fresh in [false,true] {
            let dir = try setup(root,"partial-write-\(fresh)",indices:[])
            if !fresh { try Data("first\n".utf8).write(to:path(dir,0)) }
            var fail = true
            let writer = CommitStreamFileWriter(directory:dir,writePart:{ bytes,fd,phase in
                if fail && phase.hasSuffix("body") {
                    try FileSafety.writeAll(Data(bytes.prefix(2)),to:fd,operation:phase,posixError:{ _,_ in POSIXError(.EIO) })
                    fail = false; throw POSIXError(.EIO)
                }
                try FileSafety.writeAll(bytes,to:fd,operation:phase,posixError:{ _,_ in POSIXError(.EIO) })
            })
            expectFailure { try writer.write(Data("body".utf8)) }
            try writer.write(Data("second".utf8))
            let bytes = try data(path(dir,0)); precondition(bytes == Data("second\n".utf8))
            if !fresh { let old = try data(path(dir,1)); precondition(old == Data("first\nbo".utf8)) }
        }
        // A whole body without its LF is still an incomplete line. The next
        // record must go to fresh 0, preserving those bytes as an older file.
        for fresh in [false,true] {
            let dir = try setup(root,"missing-lf-\(fresh)",indices:[])
            if !fresh { try Data("prior\n".utf8).write(to:path(dir,0)) }
            var fail = true
            let writer = CommitStreamFileWriter(directory:dir,checkpoint:{ phase in
                if fail && phase == (fresh ? "fresh_lf" : "append_lf") { fail = false; throw POSIXError(.EIO) }
            })
            expectFailure { try writer.write(Data("body".utf8)) }
            try writer.write(Data("next".utf8))
            let saved = try data(path(dir,0)); precondition(saved == Data("next\n".utf8))
            if !fresh { let old = try data(path(dir,1)); precondition(old == Data("prior\nbody".utf8)) }
        }
        let dir = try setup(root,"temp-remove",indices:[])
        try Data([65]).write(to:dir.appendingPathComponent(CommitStreamFileWriter.temporaryName))
        let writer = CommitStreamFileWriter(directory:dir,checkpoint:{ phase in if phase == "remove_temp_before" { throw POSIXError(.EIO) } })
        for _ in 0..<10 { expectFailure { try writer.write(Data([66])) } }
        let names = try fm.contentsOfDirectory(atPath:dir.path)
        precondition(names.filter { $0.hasSuffix(".tmp") }.count == 1 && !fm.fileExists(atPath:path(dir,0).path))
        try CommitStreamFileWriter(directory:dir).write(Data([67]))
    }
    // A symlink at the current, temp or lock name stops with unsafePath and the
    // link's target is never written. Another holder of the lock blocks
    // writing until it releases it.
    static func safetyAndLock(_ root: URL) throws {
        let sentinel = root.appendingPathComponent("outside"); try Data("outside".utf8).write(to:sentinel)
        for name in ["commit-stream-v5-0.jsonl",CommitStreamFileWriter.temporaryName,CommitStreamFileWriter.lockName] {
            let dir = try setup(root,UUID().uuidString,indices:[])
            try fm.createSymbolicLink(at:dir.appendingPathComponent(name),withDestinationURL:sentinel)
            do { try CommitStreamFileWriter(directory:dir).write(Data([65])); preconditionFailure() }
            catch CommitStreamDeliveryFailure.unsafePath {} // Safe stop, not an IO retry.
            let bytes = try data(sentinel); precondition(bytes == Data("outside".utf8))
        }
        let dir = try setup(root,"lock",indices:[])
        let lockFile = dir.appendingPathComponent(CommitStreamFileWriter.lockName)
        let fd = Darwin.open(lockFile.path,O_RDWR|O_CREAT|O_NOFOLLOW,0o600)
        precondition(fd >= 0 && flock(fd,LOCK_EX|LOCK_NB) == 0)
        let writer = CommitStreamFileWriter(directory:dir)
        expectFailure { try writer.write(Data([65])) }
        precondition(!fm.fileExists(atPath:path(dir,0).path))
        _ = flock(fd,LOCK_UN); _ = Darwin.close(fd)
        try writer.write(Data([66]))
        let bytes = try data(path(dir,0)); precondition(bytes == Data([66,10]))
    }
}
private extension Data { var appendingLF: Data { self + Data([10]) } }
