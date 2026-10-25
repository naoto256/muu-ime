import Darwin
import Foundation

private func require(_ condition: @autoclosure () -> Bool, _ message: String) {
    guard condition() else { fatalError("user dictionary test failed: \(message)") }
}
private struct Injected: Error {}

// A protocol fixture, deliberately not a TSV codec. Opaque bytes below are
// not even UTF-8: accepting them proves persistence does not parse Core data.
private final class FixtureCore {
    let lock = NSLock()
    var revision: UInt64 = 0
    var rows: [[String: Any]] = []
    var next: [[String: Any]]?
    var events: [String] = []
    var publishStatus: Int32 = 0
    var cancelStatus: Int32 = 0
    var prepareFailure: Error?
    var readOnly = false
    var degraded = false
    var malformedView = false
    var oversizedFile = false
    var onPrepare: (() -> Void)?
    let bytes = Data([0xff, 0x00, 0x0a, 0x41])
    func viewObject(_ rows: [[String: Any]], revision: UInt64) -> [String: Any] {
        ["schema_version":1, "management_revision":revision, "access":readOnly ? "read_only":"editable",
         "reason":NSNull(), "layer_status":degraded ? "empty_degraded":"active", "entries":readOnly ? NSNull():rows,
         "entry_count":readOnly ? NSNull():rows.count, "dormant_count":readOnly ? NSNull():0]
    }
    func record(_ event: String) { lock.lock(); events.append(event); lock.unlock() }
    var bridge: UserDictionaryCoreBridge {
        .init(view: { [self] in
            lock.lock(); defer { lock.unlock() }
            return try JSONSerialization.data(withJSONObject:viewObject(rows,revision:revision))
        }, prepare: { [self] command in
            precondition(!Thread.isMainThread)
            onPrepare?()
            lock.lock(); defer { lock.unlock() }
            events.append("prepare")
            if let prepareFailure { throw prepareFailure }
            let value = try JSONSerialization.jsonObject(with:command) as! [String:Any]
            require((value["expected_revision"] as! NSNumber).uint64Value == revision,"displayed version forwarded")
            var target = rows
            if value["operation"] as? String == "remove" { target.removeAll { $0["entry_id"] as? String == value["entry_id"] as? String } }
            else {
                var row = value; row.removeValue(forKey:"operation");row.removeValue(forKey:"expected_revision")
                row["entry_id"]="opaque-new-row"; target.append(row)
            }
            next = target
            var view=viewObject(target,revision:revision+1)
            if malformedView { view["entries"]=[["entry_id":"bad"]] }
            return (oversizedFile ? Data(count: UserDictionaryStore.maxFileBytes + 1) : bytes,try JSONSerialization.data(withJSONObject:["schema_version":1,"ticket":["owner":1,"operation":1,"expected_revision":revision],
                "expected_revision":revision,"next_revision":revision+1,"view":view]))
        }, publish: { [self] _ in
            precondition(!Thread.isMainThread)
            lock.lock(); defer { lock.unlock() };events.append("publish")
            if publishStatus == 0 { rows=next!;revision += 1;next=nil }
            return publishStatus
        }, cancel: { [self] _ in lock.lock();defer { lock.unlock() };events.append("cancel");if cancelStatus == 0 { next=nil };return cancelStatus })
    }
}

// Exercise the shared classification with opaque bytes and original errors.
// Only open ENOENT is missing. Empty and exact-limit files are present. Over
// limit, a symlink, a FIFO, a directory, ENOTDIR and an ENOENT thrown by
// validation are all unreadable, and must carry the caller's own Error object.
// The last case catches a classifier that matches ENOENT from any stage.
private func assertStoredFileReadStates(_ base: URL) throws {
    let directory = base.appendingPathComponent("read-state")
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    let original = NSError(domain: "synthetic-read-error", code: 123)
    func read(_ url: URL, validationError: Error? = nil) -> FileSafety.StoredFileRead {
        FileSafety.readState(url, maxBytes: 4, validate: {
            if let validationError { throw validationError }
            try FileSafety.rejectSymbolicLinkAncestors(of: url, fileManager: .default,
                nonAbsolutePathError: { original }, symbolicLinkError: { _ in original })
        }, unsupportedFileType: { original }, fileTooLarge: { original }, posixError: { _, _ in original })
    }
    let missing = read(directory.appendingPathComponent("absent"))
    require(missing.state == .missing && missing.bytes.isEmpty && missing.error == nil, "only open ENOENT is absence")
    let file = directory.appendingPathComponent("opaque")
    for bytes in [Data(), Data([0xff, 0, 2, 3])] {
        try bytes.write(to: file)
        let result = read(file)
        require(result.state == .present && result.bytes == bytes && result.error == nil, "empty/exact-limit are present opaque bytes")
    }
    try Data(count: 5).write(to: file)
    let link = directory.appendingPathComponent("link")
    try FileManager.default.createSymbolicLink(at: link, withDestinationURL: file)
    let fifo = directory.appendingPathComponent("fifo")
    require(mkfifo(fifo.path, 0o600) == 0, "read-state FIFO fixture")
    let nonOpenENOENT = NSError(domain: NSPOSIXErrorDomain, code: Int(ENOENT))
    for result in [read(file), read(link), read(fifo), read(directory), read(file.appendingPathComponent("child"))] {
        require(result.state == .unreadable && result.bytes.isEmpty && result.error as NSError? === original,
                "bounds, symlink, nonregular and ENOTDIR preserve unreadable error")
    }
    let rejected = read(file, validationError: nonOpenENOENT)
    require(rejected.state == .unreadable && rejected.error as NSError? === nonOpenENOENT,
            "an ENOENT outside open must not become absence")
}

@main enum UserDictionaryStoreTest {
    static func wait(_ predicate: () -> Bool) {
        // This is only a hang guard, not a timing/response assertion. The IO
        // fixture notifies through completion, and busy must span that callback.
        let guardEnd=Date().addingTimeInterval(180)
        while !predicate() { require(Date() < guardEnd,"completion hang guard");RunLoop.current.run(until:Date().addingTimeInterval(0.002)) }
    }
    static func add(_ store: UserDictionaryStore, reading: String="ムウ", surface: String="Muu") -> Result<Void,Error> {
        var result: Result<Void,Error>?
        store.upsert(reading:reading,surface:surface,partOfSpeech:.noun,priority:.high,replacing:nil) { value in
            require(Thread.isMainThread,"completion on main");require(store.isBusy,"busy includes UI completion");result=value
        }
        wait { result != nil && !store.isBusy };return result!
    }
    static func main() throws {
        // A fresh process, as at the next launch: it must read only the primary
        // file (or find it missing) and ignore any recovery file beside it.
        if CommandLine.arguments.count == 5 && CommandLine.arguments[1] == "--recovery-startup" {
            let root = URL(fileURLWithPath: CommandLine.arguments[2])
            let store = UserDictionaryStore(baseDirectory: root, bundleIdentifier: CommandLine.arguments[3])
            let present = CommandLine.arguments[4] == "present"
            require(store.startupFile.state == (present ? .present : .missing), "next process reads only the primary filename")
            require(store.startupFile.bytes == (present ? FixtureCore().bytes : Data()), "recovery files are never loaded at startup")
            return
        }
        // ABI values are fixed independently of the implementation. BUSY is
        // definite only for stores whose publication contract permits it.
        let acknowledgements: [(Int32, DictionaryAcknowledgement, Bool, Bool)] = [
            (0, .success, false, false),
            (2, .refused, true, true), (3, .refused, true, true),
            (4, .refused, true, true), (6, .refused, true, true),
            (8, .revisionMismatch, true, true), (9, .busy, false, true),
            (7, .ambiguous, false, false), (42, .ambiguous, false, false),
            (-1, .ambiguous, false, false),
        ]
        for (status, expected, user, layered) in acknowledgements {
            let ack = DictionaryAcknowledgement(status)
            require(ack == expected, "ACK classification")
            require(ack.isDefinitelyUnpublished(allowBusy: false) == user, "User publication ACK")
            require(ack.isDefinitelyUnpublished(allowBusy: true) == layered, "layer publication ACK")
            require(ack.canRetryRecovery == [2,3,4,6,9].contains(status), "bounded recovery ACK")
        }
        // The editor's part-of-speech choices: all 45 have distinct, nonempty
        // labels, with one fixed spelling as an anchor. Format and codec
        // contracts are Rust's (imported_dictionary_file and workspace tests).
        precondition(MuuDictionaryPartOfSpeech.allCases.count == 45)
        let labels = MuuDictionaryPartOfSpeech.allCases.map(\.displayName)
        precondition(labels.allSatisfy { !$0.isEmpty })
        precondition(Set(labels).count == 45)
        precondition(MuuDictionaryPartOfSpeech.haGroup1Verb.displayName == "動詞ハ行四段")
        let base=URL(fileURLWithPath:FileManager.default.currentDirectoryPath).appendingPathComponent(".muu-user-dictionary-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at:base,withIntermediateDirectories:true)
        defer { try? FileManager.default.removeItem(at:base) }
        try assertStoredFileReadStates(base)
        func make(_ name: String,_ core: FixtureCore, injection: ((String)throws->Void)?=nil) throws -> UserDictionaryStore {
            let store=UserDictionaryStore(baseDirectory:base,bundleIdentifier:name,bridge:core.bridge,injection:injection)
            store.recoveryDelayForTesting = { _ in precondition(!Thread.isMainThread) }
            try store.acceptInitializedCore();return store
        }
        // The managed boundary belongs to the caller. Reject a missing or
        // non-directory base before mkdir, chmod or any directory sync. The
        // negatives: a base that does not exist (nothing above it may be
        // created) and a base that is a file (left byte for byte). Only the
        // reported checkpoints are observed, not the fsync calls themselves.
        for fileBase in [false, true] {
            let parent = base.appendingPathComponent(fileBase ? "file-boundary" : "missing-boundary")
            let boundary = parent.appendingPathComponent("ApplicationSupport")
            let original = Data("caller-owned file".utf8)
            if fileBase {
                try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
                try original.write(to: boundary)
            }
            var checkpoints: [String] = []
            do {
                try FileSafety.ensurePrivateDirectories(boundary.appendingPathComponent("bundle/data"), below: boundary,
                    unsupportedPath: { Injected() }, checkpoint: { checkpoints.append($0) },
                    posixError: { _, _ in Injected() })
                fatalError("invalid base must be rejected before side effects")
            } catch {}
            require(checkpoints.isEmpty, "invalid base must not attempt directory mutations or syncs")
            if fileBase { require(try! Data(contentsOf: boundary) == original, "caller file unchanged") }
            else { require(!FileManager.default.fileExists(atPath: parent.path), "missing ancestors above base stay absent") }
        }
        // The bundle folder and `user_dict/` end up 0700, created or found at
        // 0755, with their links synced before the file's rename. A failed
        // directory sync aborts the save and still releases Core's ticket.
        for existed in [false, true] {
            let core = FixtureCore()
            var checkpoints: [String] = []
            let store = try make("private-directories-\(existed)", core, injection: { checkpoints.append($0) })
            let leaf = store.fileURL.deletingLastPathComponent()
            let managed = leaf.deletingLastPathComponent()
            if existed {
                try FileManager.default.createDirectory(at: leaf, withIntermediateDirectories: true)
                require(chmod(managed.path, 0o755) == 0 && chmod(leaf.path, 0o755) == 0, "fixture permissions")
            }
            try add(store).get()
            for directory in [managed, leaf] {
                var info = stat()
                require(lstat(directory.path, &info) == 0 && info.st_mode & 0o777 == 0o700, "all managed directories are private")
            }
            let sync = checkpoints.firstIndex(of: "directory_parent_fsync")
            let rename = checkpoints.firstIndex(of: "before_rename")
            require(sync != nil && rename != nil && sync! < rename!, "directory links durable before file rename")
        }
        do {
            let core = FixtureCore()
            let store = try make("directory-sync-failure", core, injection: { if $0 == "directory_parent_fsync" { throw Injected() } })
            if case .success = add(store) { fatalError("directory fsync failure must abort") }
            require(!FileManager.default.fileExists(atPath: store.fileURL.path), "no publication after directory sync failure")
            require(core.events == ["prepare", "cancel"], "directory sync failure releases unpublished ticket")
        }
        // Cancellation is part of completing the edit, not fire-and-forget.
        // A failed native ACK leaves its reservation; the completion must
        // already see editing stopped, while successful publish needs no cancel.
        for status: Int32 in [0, 6, 7, 9, 42] {
            let core = FixtureCore(); core.cancelStatus = status; core.malformedView = true
            let store = try make("cancel-ack-\(status)", core)
            var completed = false, completionStopped = false
            store.upsert(reading: "むう", surface: "Muu", partOfSpeech: .noun, priority: .high, replacing: nil) { result in
                if case .success = result { fatalError("malformed prepared view must fail") }
                completionStopped = store.editingStopped
                completed = true
            }
            wait { completed && !store.isBusy }
            require(core.events == ["prepare", "cancel"], "known ticket cancelled exactly once")
            require(completionStopped == (status != 0) && store.editingStopped == (status != 0),
                    "cancel ACK must affect the outcome before completion")
        }
        do {
            let core = FixtureCore(); core.cancelStatus = 6
            let store = try make("published-no-cancel", core)
            try add(store).get()
            require(core.events == ["prepare", "publish"] && !store.editingStopped,
                    "successful publish does not cancel")
        }
        // A file over the bound fails after the ticket is known, so the store
        // cancels it; nothing reaches disk and the next save works.
        do {
            let core = FixtureCore(); core.oversizedFile = true
            let store = try make("oversized-prepared-file", core)
            if case .success = add(store) { fatalError("oversized prepared output must fail") }
            require(core.events == ["prepare", "cancel"] && core.next == nil, "known ticket cancelled on output bound failure")
            require(!FileManager.default.fileExists(atPath: store.fileURL.path), "bad prepared output never reaches disk")
            core.oversizedFile = false
            try add(store).get()
        }
        // Shutdown reports only its own work: an edit that already failed and
        // was delivered leaves the shutdown result settled, with no new Core call.
        do {
            let core = FixtureCore(); core.publishStatus = 6
            let s = try make("shutdown-after-failure", core)
            if case .success = add(s) { fatalError("ordinary edit must fail") }
            let before = core.events
            MainActor.assumeIsolated {
                var logs: [String] = []
                let lifecycle = ApplicationLifecycle(gate: StoreShutdownGate(), log: LifecycleLog(sink: { logs.append($0) }))
                lifecycle.register(.init(name: .user, start: { _, done in s.beginShutdown(completion: done) }))
                lifecycle.begin(.will); lifecycle.waitForCompletion()
                require(lifecycle.result?.results[.user] == .settled, "prior failure is not shutdown work")
                require(logs.contains { $0.contains("event=shutdown_finished ") && $0.contains("failed=0") }, "aggregate excludes prior failure")
            }
            require(core.events == before, "settled shutdown adds no edit")
        }
        do {
        // An accepted single-file transaction finishes on IO even when main
        // cannot acknowledge its screen result during application termination.
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0), drained = DispatchSemaphore(value: 0)
        let shutdownCore = FixtureCore()
        let shuttingDown = try make("shutdown", shutdownCore, injection: {
            if $0 == "before_save" { entered.signal(); require(release.wait(timeout: .now() + 180) == .success, "release accepted save") }
        })
        var lateScreens = 0
        shuttingDown.upsert(reading: "むう", surface: "Muu", partOfSpeech: .noun, priority: .high, replacing: nil) { _ in lateScreens += 1 }
        require(entered.wait(timeout: .now() + 180) == .success, "accepted transaction is on IO")
        shuttingDown.beginShutdown { result in require(!Thread.isMainThread && result == .settled, "user shutdown worker settled"); drained.signal() }
        release.signal()
        require(drained.wait(timeout: .now() + 180) == .success, "user IO cannot wait for main")
        require(shutdownCore.revision == 1 && (try! Data(contentsOf: shuttingDown.fileURL)) == shutdownCore.bytes, "accepted transaction persists and publishes")
        let eventCount = shutdownCore.events.count
        shuttingDown.upsert(reading: "あと", surface: "後", partOfSpeech: .noun, priority: .high, replacing: nil) { result in
            if case .success = result { fatalError("new operation must be closed") }
        }
        require(shutdownCore.events.count == eventCount && !shuttingDown.canEdit, "new edit cannot start")
        var tick = false; DispatchQueue.main.async { tick = true }; wait { tick }
        require(lateScreens == 0, "late main callback cannot reopen UI")

        }
        // Loading, requesting a view and closing never creates or rewrites a
        // file, even when Rust says rows are degraded or unavailable.
        for kind in ["missing","empty","legacy","invalid","degraded"] {
            let core=FixtureCore();core.readOnly=kind == "invalid" || kind == "empty";core.degraded=kind == "degraded"
            let url=base.appendingPathComponent(kind).appendingPathComponent("user_dict/user-dictionary.tsv")
            let original=kind == "empty" ? Data() : Data("# muu-user-dictionary\t1\nlegacy opaque bytes".utf8)
            if kind != "missing" { try FileManager.default.createDirectory(at:url.deletingLastPathComponent(),withIntermediateDirectories:true);try original.write(to:url) }
            var writes=0
            let store=try make(kind,core,injection:{ _ in writes += 1 })
            require(store.startupFile.state == (kind == "missing" ? .missing:.present),"absence vs present: \(kind), \(String(describing:store.loadError))")
            require(writes == 0 && core.events.isEmpty,"read-only startup has no edit/write/rename/unlink")
            if kind == "missing" { require(!FileManager.default.fileExists(atPath:url.path),"no file created") }
            else { require(try! Data(contentsOf:url) == original,"original bytes unchanged") }
        }
        // Core's file bytes are written unchanged, and the fields Swift shows are
        // not normalized (combining marks stay decomposed). The file is owner-only.
        let opaqueCore=FixtureCore();let opaque=try make("opaque",opaqueCore)
        try add(opaque,reading:"か\u{3099}",surface:"e\u{301}").get()
        require(try! Data(contentsOf:opaque.fileURL) == opaqueCore.bytes,"Core bytes saved without decoding")
        require(Array(opaque.entries[0].reading.utf8) == Array("か\u{3099}".utf8),"Swift does not normalize fields")
        require(opaqueCore.events == ["prepare","publish"],"prepare then publish once")
        var info=stat();require(lstat(opaque.fileURL.path,&info)==0 && info.st_mode & 0o777 == 0o600,"file mode 0600")
        require(lstat(opaque.fileURL.deletingLastPathComponent().path,&info)==0 && info.st_mode & 0o777 == 0o700,"directory mode 0700")

        // Unlike Imported and Postal, this store writes the file before publish.
        // A definite publish refusal must therefore restore the previous bytes
        // exactly, or remove the file when there was none, and leave Core as is.
        for status: Int32 in [2,3,4,6,8] {
            for existed in [false,true] {
                let core=FixtureCore();core.publishStatus=status
                let store=try make("reject-\(status)-\(existed)",core)
                let original=Data("exact old bytes without newline".utf8)
                if existed { try FileManager.default.createDirectory(at:store.fileURL.deletingLastPathComponent(),withIntermediateDirectories:true);try original.write(to:store.fileURL) }
                if case .success=add(store) { fatalError("rejection must fail") }
                if existed { require(try! Data(contentsOf:store.fileURL)==original,"byte-exact rollback") }
                else { require(!FileManager.default.fileExists(atPath:store.fileURL.path),"rollback to absent") }
                require(core.revision==0 && core.next==nil,"rejected publication/cancel")
            }
        }
        // A failure anywhere before the rename never reaches publish, and the
        // final cancel releases Core's prepared slot.
        for stage in ["before_save","open_temporary","write_temporary","fsync_temporary","close_temporary","before_rename"] {
            let core=FixtureCore();let store=try make(stage,core,injection:{ if $0==stage { throw Injected() } })
            if case .success=add(store) { fatalError("save injection must fail") }
            require(!FileManager.default.fileExists(atPath:store.fileURL.path),"failure before rename preserves absence")
            require(!core.events.contains("publish") && core.next==nil,"failure cancels before publication")
        }
        // A rename that committed with a warning, and that shutdown overtakes
        // before main delivers it, makes the shutdown result failed.
        do {
            let renamed = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
            let core = FixtureCore()
            let s = try make("shutdown-warning", core, injection: {
                if $0 == "after_rename" { renamed.signal(); throw Injected() }
            })
            var screens = 0
            s.upsert(reading: "むう", surface: "Muu", partOfSpeech: .noun, priority: .high, replacing: nil) { _ in screens += 1 }
            require(renamed.wait(timeout: .now() + 180) == .success, "warning rename reached IO")
            s.beginShutdown { result in require(result == .failed, "shutdown retains warning before main"); done.signal() }
            require(done.wait(timeout: .now() + 180) == .success && screens == 0 && core.revision == 1, "warning is committed without a screen callback")
        }
        let warnCore=FixtureCore();let warning=try make("warning",warnCore,injection:{ if $0=="after_rename" { throw Injected() } })
        try add(warning).get();require(warnCore.revision==1 && warning.durabilityWarning != nil,"rename warning still publishes")

        // An edit that never commits a save/restore cannot establish the
        // durability of the preceding save, even when it returns no warning.
        for failure in ["before_save", "prepare", "decode", "read", "open_temporary", "write_temporary", "fsync_temporary", "close_temporary", "before_rename"] {
            let core=FixtureCore()
            let pendingWarning=NSError(domain:"pending-durability",code:1)
            var failingStage="after_rename"
            let store=try make("retain-warning-\(failure)",core,injection:{
                if $0==failingStage { throw pendingWarning }
            })
            try add(store).get()
            require(store.durabilityWarning as NSError? === pendingWarning,"first save has pending warning")
            let original=try Data(contentsOf:store.fileURL)
            failingStage=failure
            if failure=="prepare" { core.prepareFailure=Injected() }
            if failure=="decode" { core.malformedView=true }
            let backup=store.fileURL.appendingPathExtension("saved")
            if failure=="read" {
                try FileManager.default.moveItem(at:store.fileURL,to:backup)
                try FileManager.default.createDirectory(at:store.fileURL,withIntermediateDirectories:false)
            }
            if case .success=add(store) { fatalError("second edit must fail: \(failure)") }
            require(store.durabilityWarning as NSError? === pendingWarning,"pre-commit failure preserves exact warning: \(failure)")
            require(core.revision==1 && core.next==nil,"pre-commit failure keeps prior Core version")
            if failure=="read" {
                try FileManager.default.removeItem(at:store.fileURL)
                try FileManager.default.moveItem(at:backup,to:store.fileURL)
            }
            require(try! Data(contentsOf:store.fileURL)==original,"pre-commit failure keeps prior bytes")
        }

        // The warning clears only after a write that reached its directory fsync:
        // a durable new save, or a durable restore after a refused publish.
        for restores in [false,true] {
            let core=FixtureCore();var failingStage="after_rename";var checkpoints:[String]=[]
            let store=try make("clear-warning-\(restores)",core,injection:{
                checkpoints.append($0)
                if $0==failingStage { throw Injected() }
            })
            try add(store).get();require(store.durabilityWarning != nil,"warning before durable operation")
            let original=try Data(contentsOf:store.fileURL)
            failingStage="";checkpoints=[]
            if restores { core.publishStatus=6 }
            let result=add(store)
            if restores {
                if case .success=result { fatalError("publish refusal still reports failure") }
                require(core.revision==1 && (try! Data(contentsOf:store.fileURL))==original,"durable restore keeps old state")
                require(checkpoints.contains("restore_fsync_directory"),"restore reached directory fsync")
            } else {
                try result.get();require(core.revision==2,"durable save published")
            }
            require(checkpoints.contains("fsync_directory"),"save reached directory fsync")
            require(store.durabilityWarning==nil,"successful directory fsync clears prior warning")
        }

        // Refused publication must retain the exact old bytes on disk even
        // after all bounded restore attempts fail. This regression originally
        // stopped after one restore and retained the bytes only in memory.
        do {
            let core = FixtureCore(); core.publishStatus = 6
            var attempts = 0
            let store = try make("recovery-backup", core, injection: {
                if $0 == "before_restore" { attempts += 1; throw Injected() }
            })
            let parent = store.fileURL.deletingLastPathComponent()
            try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
            let original = Data([0xff, 0, 0x41, 0x0a, 0x42])
            try original.write(to: store.fileURL)
            if case .success = add(store) { fatalError("publish refusal must fail") }
            require(attempts == 3, "restore must use three bounded attempts")
            let backups = try FileManager.default.contentsOfDirectory(at: parent, includingPropertiesForKeys: nil)
                .filter { $0.lastPathComponent.hasPrefix("user-dictionary.recovery-") && $0.pathExtension == "tsv" }
            require(backups.count == 1 && (try! Data(contentsOf: backups[0])) == original,
                    "exhausted restoration must preserve exact original bytes in a recovery file")
            require(store.editingStopped && core.next == nil, "failed restoration stops edits and cancels the ticket")
        }

        // Retry waits are observed on IO, not performed in these tests. A
        // committed restore with a directory-sync warning is not retried.
        for succeedsAt in 1...3 {
            let core = FixtureCore(); core.publishStatus = 6
            var attempts = 0, delays: [TimeInterval] = []
            let store = try make("restore-at-\(succeedsAt)", core, injection: {
                if $0 == "before_restore" {
                    attempts += 1
                    if attempts < succeedsAt { throw Injected() }
                }
            })
            store.recoveryDelayForTesting = { delay in precondition(!Thread.isMainThread); delays.append(delay) }
            let original = Data([0xff, 0, 0x42])
            try FileManager.default.createDirectory(at: store.fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            try original.write(to: store.fileURL)
            if case .success = add(store) { fatalError("refused publish remains a failed edit") }
            require(attempts == succeedsAt && delays == Array([0.1, 0.25].prefix(succeedsAt - 1)), "bounded retry schedule")
            require((try! Data(contentsOf: store.fileURL)) == original && !store.editingStopped, "successful restore keeps old bytes and permits editing")
            require(core.events == ["prepare", "publish", "cancel"], "retry never republishes or cancels twice")
        }
        // Each outcome after the retries run out: a saved backup, a failure
        // before its rename (no backup, no partial), a backup whose directory
        // sync failed (kept, but reported as not durable), and an original that
        // was missing (no file invented). Each logs exactly one finite code,
        // and the primary file keeps the new bytes in every case.
        for fault in ["none", "recovery_before_rename", "recovery_fsync_directory", "missing"] {
            let core = FixtureCore(); core.publishStatus = 6
            var checkpoints: [String] = [], logs: [String] = [], delays: [TimeInterval] = []
            let bundle = "backup-\(fault)"
            let store = try make(bundle, core, injection: {
                checkpoints.append($0)
                if $0 == "before_restore" || $0 == fault { throw Injected() }
            })
            store.recoveryDelayForTesting = { precondition(!Thread.isMainThread); delays.append($0) }
            store.recoveryLogForTesting = { logs.append($0) }
            let parent = store.fileURL.deletingLastPathComponent()
            if fault != "missing" {
                try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
                try Data([0xff, 0, 0x31]).write(to: store.fileURL)
            }
            if case .success = add(store) { fatalError("exhausted restore must fail") }
            require(delays == [0.1, 0.25] && checkpoints.filter { $0 == "before_restore" }.count == 3, "exact retry budget")
            require(store.editingStopped && core.next == nil && core.revision == 0, "stop and cancel after exhausted restore")
            require((try! Data(contentsOf: store.fileURL)) == core.bytes, "backup does not overwrite the primary new file")
            let children = try FileManager.default.contentsOfDirectory(at: parent, includingPropertiesForKeys: nil)
            require(!children.contains { $0.pathExtension == "tmp" }, "no recovery partial after any outcome")
            let backups = children.filter { $0.lastPathComponent.hasPrefix("user-dictionary.recovery-") }
            let saved = fault == "none" || fault == "recovery_fsync_directory"
            require(backups.count == (saved ? 1 : 0), "only a completed recovery rename leaves a backup")
            if saved {
                require((try! Data(contentsOf: backups[0])) == Data([0xff, 0, 0x31]), "backup retains non-UTF8 bytes")
                var info = stat()
                require(lstat(backups[0].path, &info) == 0 && info.st_mode & 0o777 == 0o600, "recovery file mode 0600")
                let name = backups[0].deletingPathExtension().lastPathComponent
                require(UUID(uuidString: String(name.dropFirst("user-dictionary.recovery-".count))) != nil, "unique UUID recovery name")
                let fsyncFile = checkpoints.firstIndex(of: "recovery_fsync_temporary")!
                let rename = checkpoints.firstIndex(of: "recovery_before_rename")!
                let fsyncDirectory = checkpoints.firstIndex(of: "recovery_fsync_directory")!
                require(fsyncFile < rename && rename < fsyncDirectory, "recovery durability checkpoint order")
                // These are checkpoints, not direct observations of fsync syscalls.
            }
            let expectedCode: String
            switch fault {
            case "none": expectedCode = "user_dictionary_recovery_saved"
            case "missing": expectedCode = "user_dictionary_recovery_original_missing"
            case "recovery_fsync_directory": expectedCode = "user_dictionary_recovery_durability_warning"
            default: expectedCode = "user_dictionary_recovery_save_failed"
            }
            require(logs == [expectedCode], "recovery logs contain only a finite result code")
            require((store.durabilityWarning != nil) == (fault == "recovery_fsync_directory"), "backup fsync warning is not reported as durable")
            if fault == "none" {
                func checkStartup(_ expected: String) throws {
                    let process = Process()
                    process.executableURL = Bundle.main.executableURL!
                    process.arguments = ["--recovery-startup", base.path, bundle, expected]
                    process.standardOutput = FileHandle.nullDevice
                    try process.run(); process.waitUntilExit()
                    require(process.terminationStatus == 0, "separate-process startup ignores recovery backup")
                }
                try checkStartup("present")
                try FileManager.default.removeItem(at: store.fileURL)
                try checkStartup("missing")
                require((try! Data(contentsOf: backups[0])) == Data([0xff, 0, 0x31]), "startup leaves backup unchanged")
            }
        }
        // Closing fences accepted retries without a worker/main rendezvous.
        do {
            let core = FixtureCore(); core.publishStatus = 6
            let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
            let store = try make("closing-recovery", core, injection: { if $0 == "before_restore" { throw Injected() } })
            var first = true, screens = 0
            store.recoveryDelayForTesting = { _ in
                if first { first = false; entered.signal(); require(release.wait(timeout: .now() + 180) == .success, "release retry on IO") }
            }
            store.upsert(reading: "むう", surface: "Muu", partOfSpeech: .noun, priority: .high, replacing: nil) { _ in screens += 1 }
            require(entered.wait(timeout: .now() + 180) == .success, "retry entered on worker")
            store.beginShutdown { result in require(!Thread.isMainThread && result == .failed, "failed restoration reports at IO fence"); done.signal() }
            release.signal()
            require(done.wait(timeout: .now() + 180) == .success && screens == 0, "closing does not need main completion")
        }

        // Restore outcomes. A restore that fails leaves disk ahead of Core, so
        // editing stops; the warning from the first rename stays. A restore that
        // commits with only an fsync warning is a recovery, so editing continues.
        // CORE_INVALID (7) may or may not have published, so no restore is guessed.
        let failedWarningCore=FixtureCore();failedWarningCore.publishStatus=6
        let failedWarning=try make("restore-failed-with-warning",failedWarningCore,injection:{
            if $0=="after_rename" || $0=="before_restore" { throw Injected() }
        })
        _=add(failedWarning)
        require(!failedWarning.canEdit && failedWarning.durabilityWarning != nil,"failed restore keeps the renamed file's warning")

        let recoverCore=FixtureCore();recoverCore.publishStatus=6
        // restore_after_rename is past the commit point: the restore already
        // landed, so a retry would only rename again. Exactly one restore.
        var warningRestores = 0
        let recover=try make("restore-warning",recoverCore,injection:{
            if $0 == "before_restore" { warningRestores += 1 }
            if $0=="restore_after_rename" { throw Injected() }
        })
        try FileManager.default.createDirectory(at:recover.fileURL.deletingLastPathComponent(),withIntermediateDirectories:true)
        try Data("old".utf8).write(to:recover.fileURL)
        _=add(recover);require(try! Data(contentsOf:recover.fileURL)==Data("old".utf8),"restore committed despite fsync warning")
        require(recover.durabilityWarning != nil && recover.canEdit,"restored warning distinguished from recovery failure")
        require(warningRestores == 1, "committed restore with durability warning must not be retried")

        let failedCore=FixtureCore();failedCore.publishStatus=6
        let failed=try make("restore-failed",failedCore,injection:{ if $0=="before_restore" { throw Injected() } })
        _=add(failed);require(!failed.canEdit,"failed restore stops edits")
        require(try! Data(contentsOf:failed.fileURL)==failedCore.bytes,"failed restore leaves new disk")
        for status: Int32 in [7,9,42,-1] {
            let invalidCore=FixtureCore();invalidCore.publishStatus=status;let invalid=try make("core-invalid-\(status)",invalidCore)
            _=add(invalid);require(!invalid.canEdit,"ambiguous User ACK stops edits without guessing rollback")
            require(try! Data(contentsOf:invalid.fileURL)==invalidCore.bytes,"ambiguous User ACK keeps renamed bytes")
        }

        // A plan Swift cannot decode is refused before the disk is touched, and
        // its ticket is still released.
        let decodeCore=FixtureCore();decodeCore.malformedView=true;let decode=try make("decode",decodeCore)
        _=add(decode);require(decodeCore.next==nil && !decodeCore.events.contains("publish"),"decode before disk releases ticket")
        require(!FileManager.default.fileExists(atPath:decode.fileURL.path),"decode failure saves nothing")

        // One operation at a time, through its main-thread completion: a second
        // edit during prepare is refused as busy rather than queued.
        let entered=DispatchSemaphore(value:0), release=DispatchSemaphore(value:0)
        let busyCore=FixtureCore();busyCore.onPrepare={ entered.signal();release.wait() }
        let busy=try make("busy",busyCore);var firstDone=false
        busy.upsert(reading:"むう",surface:"Muu",partOfSpeech:.noun,priority:.high,replacing:nil) { _ in
            require(busy.isBusy,"busy until completion returned");firstDone=true
        }
        // Notification only: the long bound protects a broken test from
        // hanging, not the responsiveness of a machine under build load.
        require(entered.wait(timeout:.now()+180) == .success,"prepare entered")
        var secondRejected=false
        busy.upsert(reading:"むう",surface:"Other",partOfSpeech:.noun,priority:.high,replacing:nil) { result in
            if case .failure(let error as UserDictionaryOperationError)=result { secondRejected=error.code=="user_dictionary_busy" }
        }
        require(secondRejected && !firstDone,"whole-operation serialization")
        release.signal();wait { firstDone && !busy.isBusy }

        // Startup reads refuse a symlinked ancestor, a FIFO (without blocking on
        // open) and a file over the size cap, all as unreadable.
        let symlink=base.appendingPathComponent("symlink");try FileManager.default.createSymbolicLink(at:symlink,withDestinationURL:base)
        let linkStore=UserDictionaryStore(baseDirectory:symlink,bundleIdentifier:"elsewhere")
        require(linkStore.startupFile.state == .unreadable,"ancestor symlink rejected")
        let special=base.appendingPathComponent("special/user_dict/user-dictionary.tsv")
        try FileManager.default.createDirectory(at:special.deletingLastPathComponent(),withIntermediateDirectories:true)
        require(Darwin.mkfifo(special.path,0o600)==0,"FIFO fixture")
        require(UserDictionaryStore(baseDirectory:base,bundleIdentifier:"special").startupFile.state == .unreadable,"nonblocking special-file rejection")
        let large=base.appendingPathComponent("large/user_dict/user-dictionary.tsv")
        try FileManager.default.createDirectory(at:large.deletingLastPathComponent(),withIntermediateDirectories:true)
        try Data(count:UserDictionaryStore.maxFileBytes+1).write(to:large)
        require(UserDictionaryStore(baseDirectory:base,bundleIdentifier:"large").startupFile.state == .unreadable,"bounded read")
        print("user dictionary transaction tests passed")
    }
}
