import AppKit

private struct Injected: Error {}
private func require(_ value: @autoclosure () throws -> Bool, _ message: String) {
    precondition(try! value(), "imported dictionary: \(message)")
}
// This timeout prevents a hung test process, not a responsiveness assertion.
// A loaded machine may take minutes; completion, not elapsed time, is tested.
private func wait(_ condition: () -> Bool) {
    let deadline = Date().addingTimeInterval(180)
    while !condition() && Date() < deadline { RunLoop.current.run(until: Date().addingTimeInterval(0.005)) }
    require(condition(), "completion did not arrive")
}
private func object(_ bytes: Data) throws -> [String: Any] {
    try JSONSerialization.jsonObject(with: bytes) as! [String: Any]
}
private func json(_ value: Any) throws -> Data { try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]) }

// Protocol fixture, NOT a file codec. The generation is not JSON and the blob
// is not UTF-8; successfully saving both fixes Swift's opaque-byte contract.
private final class FixtureCore {
    var revision: UInt64 = 0
    var dictionaries: [[String: Any]] = []
    var generation: String?
    var readOnly = false
    var degraded = false
    var degradedReason: String?
    var prepareFailure = false
    var supplyFailure = false
    var publishStatus: Int32 = 0
    var rollbackStatuses: [Int32] = []
    var finishStatus: Int32 = 0
    var cancelStatus: Int32 = 0
    var loadRequests: [[String: Any]] = []
    var events: [String] = []
    var onPublish: (() -> Void)?
    var onLoadBegin: (() -> Void)?
    var onFinish: ((String) -> Void)?
    var next: [[String: Any]] = []
    var old: [[String: Any]] = []
    var oldGeneration: String?
    var pendingTicket: [String: Any] = [:]
    var serial: UInt64 = 0
    var loading = false
    var resolved = false
    var published = false
    var blob = Data([0xff, 0x00, 0x41])
    var generationBytes = Data()
    var fixedGenerationBytes: Data?
    var needsBlob = false
    var customLoadStep: (([String: Any]) -> [String: Any])?
    var commands: [[String: Any]] = []
    func record(_ event: String) { precondition(!Thread.isMainThread); events.append(event) }
    func view(_ rows: [[String: Any]], revision: UInt64) -> [String: Any] {
        ["schema_version":1,"management_revision":revision,"access":readOnly ? "read_only":"editable",
         "layer_status":degraded || readOnly ? "empty_degraded":"active","reason":readOnly ? "bad_file" as Any:degradedReason as Any? ?? NSNull(),
         "dictionaries":readOnly ? NSNull():rows]
    }
    func row(hash: String) -> [String: Any] {
        ["metadata":["id":"fixture.a","name":"fixture","version":"1","author":"作者","license":"CC0","provenance":"自作"],
         "enabled":true,"entry_count":1,"dormant_entry_count":0,"imported_at_unix_ms":123,"source_sha256":hash,"blob_sha256":hash]
    }
    func reserve(load: Bool) {
        serial += 1; pendingTicket=["domain":"imported_dictionary","owner":1,"operation":serial,"expected_revision":revision]
        loading=load; old=dictionaries;oldGeneration=generation;next=dictionaries;published=false;resolved=false
    }
    func reply() throws -> ImportedDictionaryCoreBridge.Reply {
        var step: [String:Any] = ["schema_version":1,"ticket":pendingTicket,"request":NSNull(),"write_blob":NSNull(),"ready":NSNull(),"preview":NSNull()]
        if loading && !loadRequests.isEmpty {
            step["state"]="needs_blob";step["request"]=loadRequests.removeFirst()
        } else if needsBlob {
            step["state"]="needs_blob";step["write_blob"]=["sha256":FileSafety.sha256(blob),"byte_length":blob.count]
            step["preview"]=["metadata":row(hash:FileSafety.sha256(blob))["metadata"]!,"entry_count":1,"replaces_existing":!old.isEmpty]
        } else {
            step["state"]="ready"
            generationBytes=loading ? Data():(fixedGenerationBytes ?? Data("opaque-generation-\(serial)".utf8))
            let newHash=loading ? generation:FileSafety.sha256(generationBytes)
            step["ready"]=["expected_revision":revision,"published_revision":revision+1,"rollback_revision":revision+2,
                "generation_sha256":newHash as Any? ?? NSNull(),"previous_generation_sha256":oldGeneration as Any? ?? NSNull(),
                "current_bytes_base64":newHash.map { Data(($0+"\n").utf8).base64EncodedString() } as Any? ?? NSNull(),
                "referenced_blobs":next.compactMap{$0["blob_sha256"] as? String},"previous_referenced_blobs":old.compactMap{$0["blob_sha256"] as? String},
                "next_view":view(next,revision:revision+1),"rollback_view":view(old,revision:revision+2),"preview":NSNull()]
        }
        if let customLoadStep, loading { step=customLoadStep(step) }
        return(generationBytes,try json(step))
    }
    var bridge: ImportedDictionaryCoreBridge {
        .init(view:{ [self] in try json(view(dictionaries,revision:revision)) },
            loadBegin:{ [self] command,_ in
                record("load_begin");onLoadBegin?();commands.append(try object(command));reserve(load:true);needsBlob=false;return try reply()
            }, loadSupply:{ [self] command,_ in
                record("load_supply");commands.append(try object(command));return try reply()
            }, prepare:{ [self] command,data in
                record("prepare");let command=try object(command);commands.append(command)
                if prepareFailure { throw Injected() }
                require((command["expected_revision"] as! NSNumber).uint64Value==revision,"expected revision")
                reserve(load:false);needsBlob=false
                switch command["action"] as! String {
                case "import": blob=data;next=[row(hash:FileSafety.sha256(blob))];needsBlob=true
                case "set_enabled": if !next.isEmpty { next[0]["enabled"]=command["enabled"] }
                case "remove": next=[]
                default: preconditionFailure("fixture action")
                }
                return try reply()
            }, prepareSupply:{ [self] command,_ in
                record("prepare_supply");commands.append(try object(command));if supplyFailure {throw Injected()};needsBlob=false;return try reply()
            }, preparedBlob:{ [self] _ in record("prepared_blob");return blob },
            publish:{ [self] _ in
                record("publish");onPublish?();if publishStatus != 0 {return publishStatus}
                revision += 1;dictionaries=next;generation=loading ? generation:FileSafety.sha256(generationBytes);published=true;return 0
            // Rollback and finish check the revision as Core does, so a
            // concurrent change yields 8 from state, not from a forced status.
            }, rollback:{ [self] _ in
                record("rollback");let status=rollbackStatuses.isEmpty ? 0:rollbackStatuses.removeFirst()
                if status==0 && revision != (pendingTicket["expected_revision"] as! UInt64)+1 {return 8}
                if status==0 {revision += 1;dictionaries=old;generation=oldGeneration;published=false};return status
            }, finish:{ [self] command in
                let command=try! object(command);let kind=command["outcome"] as! String
                record("finish:\(kind)");onFinish?(kind)
                if finishStatus==0 && revision != (command["revision"] as! NSNumber).uint64Value {return 8}
                if finishStatus==0 {resolved=true};return finishStatus
            // Like Core, a published reservation is not released until finish
            // resolves it: cancel answers BUSY (9) until then.
            }, cancel:{ [self] _ in record("cancel");return cancelStatus != 0 ? cancelStatus : published && !resolved ? 9:0 },
            template:{ [self] in record("template");return blob })
    }
}
private struct Fixture {
    let core: FixtureCore
    let store: ImportedDictionaryStore
    let base: URL
    let source: URL
    init(_ label: String, core: FixtureCore = FixtureCore(), injection: ((String) throws -> Void)? = nil, fatal: @escaping () -> Void = {}, delay: @escaping (TimeInterval)->Void = { _ in }, bridgeOverride: ImportedDictionaryCoreBridge? = nil) throws {
        self.core=core
        // Foundation may retain /var in its temporary-directory URL even
        // after resolvingSymlinksInPath. Use an explicit fixture under cwd;
        // production correctly rejects the /var symbolic-link ancestor.
        base=URL(fileURLWithPath:FileManager.default.currentDirectoryPath).appendingPathComponent(".muu-imported-dictionary-\(label)-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at:base,withIntermediateDirectories:true)
        source=base.appendingPathComponent("input.muudict")
        try core.blob.write(to:source)
        store=ImportedDictionaryStore(baseDirectory:base,bundleIdentifier:"fixture",bridge:bridgeOverride ?? core.bridge,injection:injection,fatalRuntimeRecovery:fatal,recoveryDelay:delay)
    }
    func start() {
        var done=false;store.start { result in require((try? result.get()) != nil,"startup");done=true }
        wait {done && !store.isBusy}
    }
    func prepare() throws -> PreparedImportedDictionary {
        var result:Result<PreparedImportedDictionary,Error>?
        store.prepareDictionaryFile(at:source){result=$0}
        wait{result != nil};return try result!.get()
    }
    @discardableResult func install() throws -> Result<Void,Error> {
        let prepared=try prepare();var result:Result<Void,Error>?
        store.install(prepared){ require(store.isBusy,"busy through main completion");result=$0 }
        wait{result != nil && !store.isBusy};return result!
    }
    func remove() -> Result<Void,Error> {
        var result:Result<Void,Error>?
        store.remove(dictionaryID:"fixture.a"){result=$0};wait{result != nil && !store.isBusy};return result!
    }
    func clean() { try? FileManager.default.removeItem(at:base) }
}

@main enum ImportedDictionaryTest {
    // Shutdown reports only its own outcome: an edit that failed and was delivered before
    // shutdown leaves the result settled (failed=0) and adds no Core call.
    static func shutdownExcludesPreviouslyCompletedFailure() throws {
        let f = try Fixture("shutdown-after-failure"); defer { f.clean() }; f.start()
        f.core.prepareFailure = true
        if case .success = f.remove() { fatalError("ordinary edit must fail") }
        let before = f.core.events
        MainActor.assumeIsolated {
            var logs: [String] = []
            let lifecycle = ApplicationLifecycle(gate: StoreShutdownGate(), log: LifecycleLog(sink: { logs.append($0) }))
            lifecycle.register(.init(name: .imported, start: { _, done in f.store.beginShutdown(completion: done) }))
            lifecycle.begin(.will); lifecycle.waitForCompletion()
            require(lifecycle.result?.results[.imported] == .settled, "prior failure is not shutdown work")
            require(logs.contains { $0.contains("event=shutdown_finished ") && $0.contains("failed=0") }, "aggregate excludes prior failure")
        }
        require(f.core.events == before, "settled shutdown adds no edit")
    }

    // An unexpected live revision change after publication must stop recovery,
    // not overwrite that authority or collect its files. Normal edit reservations
    // prevent this; the hostile bridge fixes the defensive mismatch response.
    static func unexpectedRevisionAfterPublishStopsRecovery() throws {
        for rollback in [false, true] {
            var gc=0;var fatal=0;var delays:[TimeInterval]=[];var pointerWrites=0
            let core=FixtureCore()
            let f=try Fixture("post-publish-conflict",core:core,injection:{
                if $0=="before_gc" {gc += 1}
                if $0=="before_current_rename" {
                    pointerWrites += 1
                    // The bridge violates the retained reservation after its
                    // successful ACK; finish/rollback must reject the stale ticket.
                    require(core.events.last=="publish" && core.revision==2,"revision changes after publication ACK")
                    core.record("unexpected_revision")
                    core.revision += 1;core.dictionaries=[];core.generation=nil
                    if rollback {throw Injected()}
                }
            },fatal:{fatal += 1},delay:{delays.append($0)});defer{f.clean()}
            f.start();f.core.events.removeAll()
            let result=try f.install()
            switch result {
            case .success: preconditionFailure("post-publish conflict must fail")
            case .failure(let error): require((error as? ImportedDictionaryOperationError)?.status==8,"revision mismatch reaches Swift completion")
            }
            require(f.core.revision==3 && f.core.dictionaries.isEmpty,"unexpected live authority is not overwritten")
            require(gc==0 && delays.isEmpty && pointerWrites==1 && fatal==1,"conflict permits no GC/forward, one fatal recovery")
            require(!f.store.canEdit,"revision conflict stops editing")
            let publication=f.core.events.firstIndex(of:"publish")!
            require(f.core.events[publication+1]=="unexpected_revision","revision change follows successful publication")
            require(f.core.events.filter{$0=="rollback"}.count == (rollback ? 1:0),"no blind rollback after failed finish")
            require(FileManager.default.fileExists(atPath:f.store.fileURL.path) == !rollback,"finish conflict preserves new pointer; rollback conflict preserves absence")
            print("unexpectedRevisionAfterPublishStopsRecovery(\(rollback ? "rollback":"finish")): passed; gc=0, forward=0, fatal=1")
        }
    }

    // A renamed blob or generation whose directory fsync failed is an orphan, not a
    // durable file. Every retry against that same name must fsync both the file and the
    // directory again before it publishes. The fixed generation bytes make the retry hit
    // the same name; a fresh name each time would never reach the reuse branch.
    static func immutableRetryRequiresDurability() throws {
        for suffix in [".muudict", ".json"] {
            var selected=false;var fail=true;var fileSyncs=0;var directorySyncs=0;var gc=0
            let f=try Fixture("retry-durability",injection:{
                if $0.hasPrefix("write_immutable:") {selected=$0.hasSuffix(suffix)}
                if selected && $0=="immutable:fsync_temporary" {fileSyncs += 1}
                if selected && $0=="immutable:fsync_directory" {
                    directorySyncs += 1
                    if fail {throw Injected()}
                }
                if $0=="before_gc" {gc += 1}
            });defer{f.clean()}
            // Retry the same generation hash too, rather than accidentally
            // testing a fresh generation whose name could never be reused.
            f.core.fixedGenerationBytes=Data("same opaque generation".utf8)
            f.start();f.core.events.removeAll()
            for attempt in 1...2 {
                require((try? f.install().get())==nil,"\(suffix) retry \(attempt) cannot bypass failed fsync")
                let bytes=suffix==".muudict" ? f.core.blob:f.core.generationBytes
                let directory=suffix==".muudict" ? f.store.blobsURL:f.store.generationsURL
                let orphan=directory.appendingPathComponent(FileSafety.sha256(bytes)+suffix)
                require(try Data(contentsOf:orphan)==bytes,"\(suffix) rename left a matching orphan")
                require(fileSyncs==attempt && directorySyncs==attempt,"\(suffix) every retry syncs file and directory")
                require(f.core.revision==1 && !f.core.events.contains("publish") && gc==0,"\(suffix) failed durability remains unpublished without GC")
                require(!FileManager.default.fileExists(atPath:f.store.fileURL.path),"\(suffix) no current before durability")
            }
            fail=false
            require((try? f.install().get()) != nil,"\(suffix) retry succeeds after both fsyncs")
            require(fileSyncs==3 && directorySyncs==3 && gc==1,"\(suffix) durable retry reaches finish and GC once")
            require(f.core.events.filter{$0=="finish:new_pointer_durable"}.count==1 && f.store.durabilityWarning==nil,"\(suffix) only synced retry finishes durably")
            print("immutableRetryRequiresDurability(\(suffix)): passed")
        }
    }

    // Construction is inert; start() loads once, off main. While it loads, edits and
    // template export are refused before any IO, and repeated start() calls do nothing.
    // The final state follows Core's view: read-only, degraded (still ready), or failed
    // when the load's publish is refused.
    static func startupStatesAndGuards() throws {
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let f = try Fixture("startup-state", injection: { point in
            if point == "read:current" {
                require(!Thread.isMainThread, "startup IO is off main")
                entered.signal(); require(release.wait(timeout: .now() + 180) == .success, "release startup")
            }
        })
        defer { f.clean() }
        require(f.store.startupState == .notStarted && !f.store.canEdit, "not started")
        f.store.start()
        require(entered.wait(timeout: .now() + 180) == .success, "load entered")
        require(f.store.startupState == .loading && !f.store.canEdit && !f.store.canExportTemplate, "loading closes edits and template")
        var rejection: Result<Void, Error>?
        f.store.exportTemplate(to: f.store.rootURL.appendingPathComponent("template")) { rejection = $0 }
        require((try? rejection?.get()) == nil && rejection != nil, "template rejected before IO")
        f.store.remove(dictionaryID: "fixture.a") { rejection = $0 }
        require(f.core.events.isEmpty, "no Core edit before initial load")
        for _ in 0..<10 { f.store.start() }
        require(f.store.startupState == .loading, "duplicate start cannot finish loading")
        release.signal(); wait { !f.store.isBusy }
        require(f.store.startupState == .ready && f.store.canEdit, "ready only after release ACK")
        for (name, readOnly, degraded, status, expected) in [
            ("readonly", true, false, Int32(0), DataStartupState.readOnly),
            ("degraded", false, true, Int32(0), .ready),
            ("failed", false, false, Int32(6), .failed)
        ] {
            let c = FixtureCore(); c.readOnly = readOnly; c.degraded = degraded; c.publishStatus = status
            let item = try Fixture("startup-" + name, core: c); defer { item.clean() }
            var result: Result<Void, Error>?
            item.store.start { result = $0 }
            wait { result != nil && !item.store.isBusy }
            require(item.store.startupState == expected, "startup result " + name)
        }
    }

    // Shutdown settles on a worker even while main is blocked, and cancels exactly once.
    // A completion that main delivers later cannot reopen editing or cancel again. A
    // prepared preview is released by shutdown, and a confirmation that arrives after
    // that can neither install nor cancel it a second time.
    static func shutdownFinishesWithoutMainAndCancelsPreviewOnce() throws {
        do {
            let f = try Fixture("shutdown-screen"); defer { f.clean() }
            let ioFinished = DispatchSemaphore(value: 0), drained = DispatchSemaphore(value: 0)
            var screenCalls = 0
            f.core.onFinish = { _ in ioFinished.signal() }
            f.store.start { _ in screenCalls += 1 }
            require(ioFinished.wait(timeout: .now() + 180) == .success, "IO finished without pumping main")
            f.store.beginShutdown { result in
                require(!Thread.isMainThread && result == .settled, "worker settles shutdown")
                drained.signal()
            }
            require(drained.wait(timeout: .now() + 180) == .success, "cleanup cannot wait for main screen")
            require(f.core.events.filter { $0 == "cancel" }.count == 1, "one cancel")
            var tick = false; DispatchQueue.main.async { tick = true }; wait { tick }
            require(screenCalls == 0 && f.store.isClosing && !f.store.canEdit, "late screen cannot reopen")
            require(f.core.events.filter { $0 == "cancel" }.count == 1, "late screen cannot cancel twice")
        }
        do {
            let f = try Fixture("shutdown-preview"); defer { f.clean() }; f.start()
            let prepared = try f.prepare()
            let before = f.core.events.filter { $0 == "cancel" }.count
            let done = DispatchSemaphore(value: 0)
            f.store.beginShutdown { result in require(result == .settled, "preview cancelled"); done.signal() }
            require(done.wait(timeout: .now() + 180) == .success, "preview release")
            require(f.core.events.filter { $0 == "cancel" }.count == before + 1, "preview ticket released once")
            let calls = f.core.events.count
            f.store.install(prepared) { _ in }; f.store.cancel(prepared)
            require(f.core.events.count == calls && !f.store.canExportTemplate, "late confirmation cannot install or cancel again")
            require(!FileManager.default.fileExists(atPath: f.store.fileURL.path), "preview never saved")
        }
    }

    // A warning that shutdown overtakes before the screen saw it still makes the
    // shutdown result failed. It is reported rather than waited on: no screen callback
    // is needed for shutdown to finish.
    static func shutdownRetainsUnacknowledgedDurabilityWarning() throws {
        let finished = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
        let f = try Fixture("shutdown-warning", injection: { if $0 == "after_current_rename" { throw Injected() } }); defer { f.clean() }
        f.start(); let prepared = try f.prepare()
        f.core.onFinish = { _ in finished.signal() }
        var screens = 0
        f.store.install(prepared) { _ in screens += 1 }
        require(finished.wait(timeout: .now() + 180) == .success, "warning transaction finished on IO")
        f.store.beginShutdown { result in require(result == .failed, "undelivered durability warning remains a failure fact"); done.signal() }
        require(done.wait(timeout: .now() + 180) == .success && screens == 0, "warning cannot require screen acknowledgement")
    }

    // Shutdown during the initial load cancels that ticket once, publishes nothing and
    // counts as settled: cancelling on purpose is not an IO failure.
    static func shutdownCancelsInitialLoad() throws {
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
        let f = try Fixture("shutdown-load"); defer { f.clean() }
        f.core.onLoadBegin = { entered.signal(); require(release.wait(timeout: .now() + 180) == .success, "release initial load") }
        var screens = 0
        f.store.start { _ in screens += 1 }
        require(entered.wait(timeout: .now() + 180) == .success, "initial load is on IO")
        f.store.beginShutdown { result in require(result == .settled, "intentional initial-load cancellation is not an IO failure"); done.signal() }
        release.signal()
        require(done.wait(timeout: .now() + 180) == .success, "initial ticket settles without main")
        require(!f.core.events.contains("publish") && f.core.events.filter { $0 == "cancel" }.count == 1, "unpublished initial load cancelled once")
        require(screens == 0 && !FileManager.default.fileExists(atPath: f.store.fileURL.path), "initial cancellation has no screen or save")
    }

    // Shutdown that arrives while a confirmed edit is held just before publish cancels
    // it: no publish, one cancel, no pointer, no screen call.
    static func shutdownCancelsBeforePublication() throws {
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let f = try Fixture("shutdown-unpublished", injection: {
            if $0 == "before_publish" { entered.signal(); require(release.wait(timeout: .now() + 180) == .success, "release publication barrier") }
        }); defer { f.clean() }
        f.start()
        let prepared = try f.prepare()
        let publishes = f.core.events.filter { $0 == "publish" }.count
        let cancels = f.core.events.filter { $0 == "cancel" }.count
        var screenCalls = 0
        f.store.install(prepared) { _ in screenCalls += 1 }
        require(entered.wait(timeout: .now() + 180) == .success, "unpublished IO reached barrier")
        let done = DispatchSemaphore(value: 0)
        f.store.beginShutdown { result in require(result == .settled, "unpublished cancellation is settled"); done.signal() }
        release.signal()
        require(done.wait(timeout: .now() + 180) == .success, "cancel without main completion")
        require(f.core.events.filter { $0 == "publish" }.count == publishes, "closing never publishes a prepared edit")
        require(f.core.events.filter { $0 == "cancel" }.count == cancels + 1, "unpublished ticket cancelled once")
        require(screenCalls == 0 && !FileManager.default.fileExists(atPath: f.store.fileURL.path), "no pointer or screen after closing")
    }

    static func main() throws {
        // A supply step still has the earlier Store ticket. The native
        // helper's cancellation attempt owns its cleanup, including ACK 0.
        for status: Int32 in [6, 9, 42, 0] {
            let core = FixtureCore()
            var localCancels = 0, storeCancels = 0
            var bridge = core.bridge
            let nativeCancel = bridge.cancel
            bridge.cancel = { storeCancels += 1; return nativeCancel($0) }
            bridge.prepareSupply = { command, _ in
                let command = try object(command)
                let ticket = try json(command["ticket"]!)
                localCancels += 1
                let ack = nativeCancel(ticket)
                throw DictionaryPreparationFailure(original: ImportedDictionaryOperationError(status: 4, code: "invalid_plan"), cancellationStatus: ack)
            }
            let fixture = try Fixture("supply-local-cancel-\(status)", core: core, bridgeOverride: bridge)
            defer { fixture.clean() }; fixture.start()
            let before = storeCancels
            core.cancelStatus = status
            let result = try fixture.install()
            if case .failure(let error) = result { require((error as? ImportedDictionaryOperationError)?.code == "invalid_plan", "supply preserves original error") }
            else { fatalError("local supply must fail") }
            require(localCancels == 1 && storeCancels == before, "Imported supply must not cancel the same ticket twice")
            require(fixture.store.editingStopped == (status != 0), "only failed local cancellation stops Imported")
        }
        for status: Int32 in [6, 7, 9, 42] {
            let core = FixtureCore()
            let original = ImportedDictionaryOperationError(status: 4, code: "invalid_plan")
            var prepares = 0
            var bridge = core.bridge
            bridge.prepare = { _, _ in prepares += 1; throw DictionaryPreparationFailure(original: original, cancellationStatus: status) }
            let fixture = try Fixture("local-cancel-\(status)", core: core, bridgeOverride: bridge)
            defer { fixture.clean() }; fixture.start()
            let cancels = core.events.filter { $0 == "cancel" }.count
            var result: Result<PreparedImportedDictionary, Error>?
            fixture.store.prepareDictionaryFile(at: fixture.source) {
                require(fixture.store.editingStopped, "failed local prepare cancellation stops Imported before completion")
                result = $0
            }
            wait { result != nil && !fixture.store.isBusy }
            if case .failure(let error) = result! { require((error as? ImportedDictionaryOperationError)?.code == original.code, "original prepare error preserved") }
            else { fatalError("local prepare must fail") }
            result = nil
            fixture.store.prepareDictionaryFile(at: fixture.source) { result = $0 }
            wait { result != nil && !fixture.store.isBusy }
            require(prepares == 1 && core.events.filter { $0 == "cancel" }.count == cancels, "no second prepare or duplicate cancel")
        }
        let overflow = ImportedDictionaryOperationError(status: 6, code: "revision_overflow")
        let previous = ImportedDictionaryOperationError(status: 6, code: "imported_dictionary_operation_failed")
        require(overflow.code == "revision_overflow" && overflow.localizedDescription == previous.localizedDescription,
            "ABI 13 keeps the overflow fact with the earlier fallback presentation")
        require(DictionaryAcknowledgement(overflow.status) == .refused
            && DictionaryAcknowledgement(overflow.status).canRetryRecovery,
            "overflow remains a definite refusal with the same recovery classification")
        // Every managed directory below the base ends up 0700, whether the
        // store created it or found it at 0755, and the parent links are synced
        // before the first immutable file is renamed in. A failed directory
        // sync then aborts the install with nothing published.
        for existed in [false, true] {
            var checkpoints: [String] = []
            let f = try Fixture("private-directories-\(existed)", injection: { checkpoints.append($0) })
            defer { f.clean() }
            let directories = [f.base.appendingPathComponent("fixture"), f.store.rootURL.deletingLastPathComponent(),
                f.store.rootURL, f.store.blobsURL, f.store.generationsURL]
            if existed {
                for directory in directories {
                    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
                    require(chmod(directory.path, 0o755) == 0, "fixture permissions")
                }
            }
            f.start(); try f.install().get()
            for directory in directories {
                var info = stat()
                require(lstat(directory.path, &info) == 0 && info.st_mode & 0o777 == 0o700, "all imported ancestors private")
            }
            let sync = checkpoints.firstIndex(of: "immutable:directory_parent_fsync")
            let rename = checkpoints.firstIndex(of: "immutable:before_rename")
            require(sync != nil && rename != nil && sync! < rename!, "imported directory links durable before rename")
        }
        do {
            let f = try Fixture("directory-sync-failure", injection: { if $0 == "immutable:directory_parent_fsync" { throw Injected() } })
            defer { f.clean() }; f.start()
            let publications = f.core.events.filter { $0 == "publish" }.count
            require((try? f.install().get()) == nil, "imported directory sync failure aborts")
            require(f.core.events.filter { $0 == "publish" }.count == publications && !FileManager.default.fileExists(atPath: f.store.fileURL.path), "directory failure stays unpublished")
        }
        try shutdownExcludesPreviouslyCompletedFailure()
        try shutdownRetainsUnacknowledgedDurabilityWarning()
        try shutdownCancelsInitialLoad()
        try shutdownCancelsBeforePublication()
        try shutdownFinishesWithoutMainAndCancelsPreviewOnce()
        try startupStatesAndGuards()
        try immutableRetryRequiresDurability()
        try unexpectedRevisionAfterPublishStopsRecovery()
        // No constructor write/read, no automatic migration, and every Core
        // operation is off main. Even non-UTF8/non-JSON files remain opaque.
        do {
            var io:[String]=[];let f=try Fixture("opaque",injection:{io.append($0)});defer{f.clean()}
            require(io.isEmpty,"construction is inert");f.start()
            require(!io.contains{$0.contains("write") || $0.contains("rename")},"loading does not save")
            f.core.onPublish={ require(!FileManager.default.fileExists(atPath:f.store.fileURL.path),"ACK precedes pointer") }
            require((try? f.install().get()) != nil,"opaque install")
            require(try Data(contentsOf:f.store.blobsURL.appendingPathComponent(FileSafety.sha256(f.core.blob)+".muudict"))==f.core.blob,"opaque blob bytes")
            require(try Data(contentsOf:f.store.generationsURL.appendingPathComponent(f.core.generation!+".json"))==f.core.generationBytes,"opaque generation bytes")
            require(try Data(contentsOf:f.store.fileURL)==Data((f.core.generation!+"\n").utf8),"Core pointer bytes")
            require(f.store.dictionaries.count==1,"Core summary drives UI")
            require(f.core.events.suffix(3)==["publish","finish:new_pointer_durable","cancel"],"publication then finish then release")
        }
        // Stream one blob; if a later prepare step fails, leave an orphan but
        // keep old pointer/Core. Re-durabilize matching bytes on retry; after success
        // collect unrelated orphan names only, after finish, never unknown files.
        do {
            var writes=0;let f=try Fixture("orphan",injection:{if $0.hasPrefix("write_immutable:") {writes += 1}});defer{f.clean()};f.start()
            f.core.supplyFailure=true
            require((try? f.install().get())==nil,"late private failure")
            require(f.core.revision==1,"failed preparation unpublished")
            let blobURL=f.store.blobsURL.appendingPathComponent(FileSafety.sha256(f.core.blob)+".muudict")
            require(FileManager.default.fileExists(atPath:blobURL.path),"staged orphan survives")
            let orphan=Data("other orphan".utf8);let orphanURL=f.store.blobsURL.appendingPathComponent(FileSafety.sha256(orphan)+".muudict")
            try orphan.write(to:orphanURL);let unknown=f.store.blobsURL.appendingPathComponent("keep.txt");try orphan.write(to:unknown)
            f.core.supplyFailure=false
            f.core.onFinish={ kind in if kind=="new_pointer_durable" {require(FileManager.default.fileExists(atPath:orphanURL.path),"GC waits for finish ACK")} }
            let before=writes;require((try? f.install().get()) != nil,"retry succeeds")
            require(writes-before==2,"matching blob and generation both durabilized")
            require(!FileManager.default.fileExists(atPath:orphanURL.path),"next success collects orphan")
            require(FileManager.default.fileExists(atPath:unknown.path),"unknown file preserved")
        }
        // A same-name blob with wrong bytes is replaced, not trusted: the name
        // alone never proves the contents.
        do {
            var writes=0;let f=try Fixture("corrupt",injection:{if $0.hasPrefix("write_immutable:") {writes += 1}});defer{f.clean()};f.start()
            try FileManager.default.createDirectory(at:f.store.blobsURL,withIntermediateDirectories:true)
            let url=f.store.blobsURL.appendingPathComponent(FileSafety.sha256(f.core.blob)+".muudict");try Data("broken".utf8).write(to:url)
            require((try? f.install().get()) != nil,"corrupt orphan replaced")
            require(try Data(contentsOf:url)==f.core.blob,"corrupt replacement bytes")
            require(writes==2,"blob and generation written")
        }
        // Requests are constrained names; malformed names never cause a read
        // outside managed directories, and a bad DTO still releases its ticket.
        for bad in ["../escape",String(repeating:"A",count:64),"a/b",""] {
            var reads:[String]=[];let core=FixtureCore()
            core.loadRequests=[["request_id":1,"kind":"blob","sha256":bad]]
            let f=try Fixture("bad-name",core:core,injection:{if $0.hasPrefix("read:"){reads.append($0)}});defer{f.clean()}
            var done=false;f.store.start{require((try? $0.get())==nil,"bad name rejected");done=true};wait{done && !f.store.isBusy}
            require(reads==["read:current"],"bad hash does not open a path")
            require(core.events.last=="cancel","bad name releases load")
        }
        // An unknown request kind is refused before any read, so a supplied
        // path never reaches the filesystem.
        do {
            let core=FixtureCore();core.customLoadStep={step in var step=step;step["state"]="needs_blob";step["ready"]=NSNull();step["request"]=["request_id":1,"kind":"path","sha256":"../../outside"];return step}
            let f=try Fixture("bad-kind",core:core);defer{f.clean()};var done=false
            f.store.start{require((try? $0.get())==nil,"unknown kind rejected");done=true};wait{done && !f.store.isBusy}
            require(core.events==["load_begin","cancel"],"unknown kind never opens supplied path")
        }
        // A current rename failure restores only Core, advances revision r+2,
        // and preserves old pointer bytes without interpreting or rewriting them.
        do {
            var fail=false;let f=try Fixture("rollback",injection:{if fail && $0=="before_current_rename" {throw Injected()}});defer{f.clean()};f.start()
            require((try? f.install().get()) != nil,"baseline save");let original=try Data(contentsOf:f.store.fileURL)
            fail=true;require((try? f.remove().get())==nil,"save failure surfaced")
            require(f.core.revision==4 && f.store.view?.managementRevision==4,"rollback consumes a revision")
            require(f.store.dictionaries.count==1,"old summary restored")
            require(try Data(contentsOf:f.store.fileURL)==original,"old pointer untouched")
            require(f.core.events.contains("finish:rolled_back"),"rollback resolved")
        }
        // Two definitely unpublished rollbacks lead forward; 7/8 do not.
        do {
            var failures=1;let core=FixtureCore();var fatal=0;var delays:[TimeInterval]=[]
            let f=try Fixture("forward",core:core,injection:{if $0=="before_current_rename" && failures>0 {failures -= 1;throw Injected()}},fatal:{fatal += 1},delay:{delays.append($0)});defer{f.clean()};f.start()
            core.rollbackStatuses=[6,6];require((try? f.install().get()) != nil,"bounded forward recovery")
            require(core.events.filter{$0=="rollback"}.count==2 && fatal==0,"two rollbacks, no fatal")
            require(f.store.recoveryNotice != nil,"forward completion is visible")
            require(delays==[0.1],"first forward delay")
        }
        // A rollback answered with a conflict, CORE_INVALID or an unknown value
        // gets no forward save and no GC, and the store exits fatally once.
        for status:Int32 in [7,8,42,-1] {
            var attempts=0;var fatal=0;var gc=0;let core=FixtureCore()
            let f=try Fixture("terminal",core:core,injection:{if $0=="before_gc" {gc += 1};if $0=="before_current_rename" {attempts += 1;throw Injected()}},fatal:{fatal += 1});defer{f.clean()};f.start()
            core.rollbackStatuses=[status];require((try? f.install().get())==nil,"terminal rollback fails")
            require(attempts==1 && fatal==1 && gc==0,"no blind forward or GC after conflicting or ambiguous ACK")
            require(!f.store.canEdit,"fatal recovery stops editing")
        }
        do {
            var fatal=0;let core=FixtureCore();core.rollbackStatuses=[6,6];var delays:[TimeInterval]=[]
            let f=try Fixture("fatal-forward",core:core,injection:{if $0=="before_current_rename" {throw Injected()}},fatal:{fatal += 1},delay:{delays.append($0)});defer{f.clean()};f.start()
            require((try? f.install().get())==nil && fatal==1,"exhausted forward recovery terminates")
            require(!FileManager.default.fileExists(atPath:f.store.fileURL.path),"no false pointer commit")
            require(delays==[0.1,0.25,0.5],"bounded forward retry sequence")
        }
        // A post-rename warning means COMMITTED, never rollback/GC. A later
        // private failure cannot erase it; a fully durable save can.
        do {
            var warn=true;var gc=0;let f=try Fixture("warning",injection:{if $0=="after_current_rename" && warn {throw Injected()};if $0=="before_gc"{gc += 1}});defer{f.clean()};f.start()
            require((try? f.install().get()) != nil,"rename warning is committed")
            require(f.store.durabilityWarning != nil && gc==0,"warning preserved, no GC")
            require(!f.core.events.contains("rollback"),"no post-rename rollback")
            f.core.prepareFailure=true;require((try? f.remove().get())==nil,"private failure")
            require(f.store.durabilityWarning != nil,"private failure keeps old warning")
            f.core.prepareFailure=false;warn=false;require((try? f.remove().get()) != nil,"durable next edit")
            require(f.store.durabilityWarning==nil && gc==1,"durability success clears warning")
        }
        // Every checkpoint of either immutable write fails before publish:
        // Core keeps the old revision and no pointer appears.
        for suffix in [".muudict", ".json"] {
          for checkpoint in ["open_temporary","write_temporary","fsync_temporary","close_temporary","before_rename","fsync_directory"] {
            var selected=false
            let f=try Fixture("private-failure",injection:{
                if $0.hasPrefix("write_immutable:") {selected=$0.hasSuffix(suffix)}
                if selected && $0=="immutable:"+checkpoint {throw Injected()}
            });defer{f.clean()};f.start()
            f.core.events.removeAll()
            require((try? f.install().get())==nil,"injected private failure")
            require(f.core.revision==1 && !f.core.events.contains("publish"),"private failure leaves old Core")
            require(!FileManager.default.fileExists(atPath:f.store.fileURL.path),"private failure leaves pointer absent")
          }
        }
        // Known refusals leave the editor usable. An unknown publication ACK
        // cannot prove that Core stayed unchanged, so editing must stop.
        for status:Int32 in [2,3,4,6,7,8,9,42,-1] {
            let f=try Fixture("publication-refused");defer{f.clean()};f.start();f.core.publishStatus=status
            require((try? f.install().get())==nil,"publication refusal surfaced")
            require(f.core.revision==1 && !FileManager.default.fileExists(atPath:f.store.fileURL.path),"refusal never commits pointer")
            require(!f.core.events.contains("finish:new_pointer_durable"),"unpublished cannot finish as committed")
            require(f.store.canEdit == ![7,42,-1].contains(status),"ambiguous publication ACK stops editor")
        }
        // Finish fails after the pointer rename: Core and disk both hold the
        // new state, so rolling back would split them. Exit fatally once, no GC.
        for status:Int32 in [6,7,8] {
            var fatal=0;var gc=0;let f=try Fixture("finish-failed",injection:{if $0=="before_gc"{gc += 1}},fatal:{fatal += 1});defer{f.clean()};f.start();f.core.finishStatus=status
            require((try? f.install().get())==nil,"failed finish stops normal completion")
            require(f.core.revision==2 && FileManager.default.fileExists(atPath:f.store.fileURL.path),"finish failure keeps renamed pointer and new Core")
            require(fatal==1 && gc==0 && !f.store.canEdit && !f.core.events.contains("rollback"),"no GC or rollback after committed finish failure")
        }
        // Prepare writes nothing; cancelling a preview leaves no storage
        // directory at all, and the next edit still works.
        do {
            let f=try Fixture("cancel-preview");defer{f.clean()};f.start();let prepared=try f.prepare();var done=false
            f.store.cancel(prepared){done=true};wait{done && !f.store.isBusy}
            require(f.core.revision==1 && !FileManager.default.fileExists(atPath:f.store.rootURL.path),"cancelled preview stages nothing")
            require((try? f.install().get()) != nil,"next edit after cancellation")
        }
        // One preview confirms at most once. A second install, or a stale
        // cancel, while the first runs must not start or release anything.
        do {
            let f=try Fixture("one-confirmation");defer{f.clean()};f.start();let prepared=try f.prepare();var done=false
            f.store.install(prepared){require((try? $0.get()) != nil,"first confirmation succeeds");done=true}
            var rejected=false
            f.store.install(prepared){rejected=(try? $0.get())==nil}
            f.store.cancel(prepared) // A stale confirmation must not release live IO.
            require(rejected,"same preview cannot start a second operation")
            wait{done && !f.store.isBusy}
            require(f.core.events.filter{$0=="publish"}.count==2,"only load and one edit publish")
        }
        // A refused final cancel leaves the saved edit in place but stops
        // editing until restart; the stop is checked before any Core call.
        for status:Int32 in [6,9,7] {
            let f=try Fixture("cancel-refused");defer{f.clean()};f.start();f.core.cancelStatus=status
            require((try? f.install().get()) != nil,"saved success survives cancellation refusal")
            require(!f.store.canEdit && f.store.loadError?.localizedDescription.contains("再起動すると編集できる") == true,"cancel refusal stops editing with restart guidance")
            let prepares=f.core.events.filter{$0=="prepare"}.count
            var refused=false
            f.store.remove(dictionaryID:"fixture.a"){refused=(try? $0.get())==nil}
            require(refused && f.core.events.filter{$0=="prepare"}.count==prepares,"stopped editor sends no new Core operation")
            require(f.core.revision==2 && FileManager.default.fileExists(atPath:f.store.fileURL.path),"cleanup failure never rolls back a saved dictionary")
        }
        // Symlink ancestors and leaves are rejected without changing the target.
        do {
            let f=try Fixture("symlink");defer{f.clean()};f.start()
            try FileManager.default.createDirectory(at:f.store.rootURL,withIntermediateDirectories:true)
            let external=f.base.appendingPathComponent("outside");try FileManager.default.createDirectory(at:external,withIntermediateDirectories:true)
            try FileManager.default.createSymbolicLink(at:f.store.blobsURL,withDestinationURL:external)
            require((try? f.install().get())==nil,"symlink storage rejected")
            require(try FileManager.default.contentsOfDirectory(atPath:external.path).isEmpty,"external directory untouched")
        }
        // The window shows Rust's degradation reason and the durability
        // warning together, and adds no cause of its own (no "上限").
        do {
            _ = NSApplication.shared
            let core=FixtureCore();core.degraded=true;core.degradedReason="invalid_dictionary_id";core.dictionaries=[core.row(hash:FileSafety.sha256(core.blob))]
            let f=try Fixture("degraded-warning",core:core,injection:{if $0=="after_current_rename" {throw Injected()}});defer{f.clean()};f.start()
            var result:Result<Void,Error>?
            f.store.setEnabled(false,dictionaryID:"fixture.a"){result=$0};wait{result != nil && !f.store.isBusy}
            require((try? result!.get()) != nil,"degraded deletion/disable save")
            let controller=ImportedDictionaryWindowController(store:f.store)
            func labels(_ view:NSView)->[NSTextField]{(view as? NSTextField).map{[$0]} ?? view.subviews.flatMap(labels)}
            let fields=labels(controller.window!.contentView!)
            require(fields.contains{$0.stringValue.contains("辞書のID") && $0.stringValue.contains("まず削除または無効化") && $0.stringValue.contains("保存の耐久性") && !$0.stringValue.contains("上限") && $0.maximumNumberOfLines==0},"Rust's degradation reason and durability warning remain visible together without inventing a capacity cause")
        }
        print("imported dictionary IO protocol, recovery, streaming, orphan reuse/GC and name confinement: passed")
    }
}
