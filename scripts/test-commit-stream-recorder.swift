import Foundation

enum MuuConstants {
    static let bundleIdentifier = "dev.muu.inputmethod.Muu.commit-stream-test"
}

@main
private enum CommitStreamRecorderTests {
    private static let maximumFileBytes = 8 * 1_024 * 1_024

    static func main() throws {
        let fileManager = FileManager.default
        let root = fileManager.homeDirectoryForCurrentUser
            .appendingPathComponent("Library", isDirectory: true)
            .appendingPathComponent("Caches", isDirectory: true)
            .appendingPathComponent("muu-commit-stream-recorder-tests", isDirectory: true)
            // Resolve only the valid fixture base. Foundation's temporary path
            // may cross /var or /tmp symlinks; intentional symlink fixtures below
            // must still exercise production rejection without being resolved.
            .resolvingSymlinksInPath()
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try fileManager.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fileManager.removeItem(at: root) }

        try assertUnreadableMetadataStopsDrainAndReturnsRaw(root: root)
        try assertDefaultOffDoesNotCreateStorage(root: root, fileManager: fileManager)
        try assertProductionConfigurationAuthorityGates(root: root, fileManager: fileManager)
        try assertProcessConfigurationDoesNotMintSessionNonces(root: root, fileManager: fileManager)
        try assertValidRecordAndOwnerOnlyPermissions(root: root, fileManager: fileManager)
        try assertOpaquePayloadContentsAreNotValidated(root: root, fileManager: fileManager)
        try assertStorageDoesNotMixWithV1Evidence(root: root, fileManager: fileManager)
        try assertMalformedRecordDropsOnlyItself(root: root, fileManager: fileManager)
        try assertOversizedRecordIsRejected(root: root, fileManager: fileManager)
        try assertInjectedWriteFailureWaitsAndRecovers(root: root, fileManager: fileManager)
        try assertPermissionDenialWaitsAndRecovers(root: root, fileManager: fileManager)
        try assertSymlinkAncestorCannotEscape(root: root, fileManager: fileManager)
        try assertRotationIsBounded(root: root, fileManager: fileManager)
        try assertRotationFailureIsFailClosed(root: root, fileManager: fileManager)
        try assertV4ArchiveSuccessAndFailure(root: root, fileManager: fileManager)
        try assertIneligibleV4NamesDoNotCreateArchives(root: root, fileManager: fileManager)
        try assertWriteFailureLogsOnceWithoutContents(root: root, fileManager: fileManager)
        try assertMaximumOpaqueRecordBoundary(root: root, fileManager: fileManager)
        try assertRestartAfterInterruptedLine(root: root, fileManager: fileManager)
        try assertStartupLineBoundaryCases(root: root, fileManager: fileManager)
        try assertUnreadableTailRotatesAndIsCheckedOnlyOnce(root: root, fileManager: fileManager)
        try assertStatusDoesNotWaitForWriter(root: root, fileManager: fileManager)
        print("commit stream v5 opaque recorder, framing, rotation and v4 archive: passed")
    }

    // Drop metadata that Swift cannot read is handed back to Core unchanged, counted as
    // one discard, and stops further draining. Guessing a weight from bytes it cannot
    // read would misreport what was lost.
    private static func assertUnreadableMetadataStopsDrainAndReturnsRaw(root: URL) throws {
        let metadata = Data("{\"schema_version\":2,\"drop_weight\":7}".utf8)
        let returned = DispatchSemaphore(value: 0)
        let recorder = CommitStreamRecorder(processConfiguration: configuration(), directoryURL: root.appendingPathComponent("metadata"), reportDropped: { bytes in
            precondition(bytes == metadata); returned.signal(); return 0
        })
        recorder.discardUnreadableMetadata(metadata)
        precondition(!recorder.canDrain && recorder.status.metadataFailed && recorder.status.discarded == 1)
        precondition(returned.wait(timeout: .now() + 180) == .success)
    }

    // Status is a short locked read. It must answer while writer IO is blocked and while
    // a failure is being published, so the About window never waits on disk. "Requested"
    // and "configured" are separate facts: requested with no configuration is the
    // failed-to-configure case. One write failure logs once, even when more records follow.
    private static func assertStatusDoesNotWaitForWriter(root: URL, fileManager: FileManager) throws {
        let disabled = CommitStreamRecorder(processConfiguration: nil, directoryURL: nil)
        precondition(!disabled.status.requested && !disabled.status.configured && !disabled.status.stopped)
        let failed = CommitStreamRecorder(processConfiguration: nil, directoryURL: nil, recordingRequested: true)
        precondition(failed.status.requested && !failed.status.configured && !failed.status.stopped)
        let directory = root.appendingPathComponent("status")
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data("old\n".utf8).write(to: file(directory: directory, index: 0))
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let clock = CommitStreamTestClock()
        let recorder = CommitStreamRecorder(processConfiguration: configuration(), directoryURL: directory,
            lastByteReader: { _ in
                entered.signal()
                precondition(release.wait(timeout: .now() + 180) == .success, "status waited for writer IO")
                return 10
            }, clock: clock.clock)
        recorder.append(Data("opaque".utf8))
        precondition(entered.wait(timeout: .now() + 180) == .success)
        for _ in 0..<1_000 {
            let status = recorder.status
            precondition(status.requested && status.configured && !status.stopped)
        }
        release.signal()
        recorder.waitUntilIdleForTesting()
        recorder.failNextWriteForTesting()
        recorder.append(Data("opaque".utf8))
        // Race the short read with the actual failure publication. Each read
        // may see either complete state; it never synchronizes with IO.
        for _ in 0..<1_000 { precondition(recorder.status.configured) }
        recorder.waitUntilIdleForTesting()
        precondition(recorder.status.retrying && !recorder.status.stopped && recorder.failureLogCount == 1)
        recorder.append(Data("discarded".utf8))
        recorder.waitUntilIdleForTesting()
        precondition(recorder.status.retrying && !recorder.status.stopped && recorder.failureLogCount == 1)
    }

    // A file that ends without LF (a crash mid-write) is never appended to; the next
    // record would be glued onto the broken line. It rotates to slot 1 and the new
    // record starts slot 0. With all eight slots full, the rotation drops only the
    // oldest slot.
    private static func assertRestartAfterInterruptedLine(root: URL, fileManager: FileManager) throws {
        for full in [false, true] {
            let directory = root.appendingPathComponent(full ? "interrupted-full" : "interrupted", isDirectory: true)
            try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
            let interrupted = Data(#"{"interrupted":"#.utf8)
            try interrupted.write(to: file(directory: directory, index: 0))
            if full {
                for index in 1..<8 {
                    try Data("old-\(index)\n".utf8).write(to: file(directory: directory, index: index))
                }
            }
            let recorder = recorder(directory: directory, fileManager: fileManager)
            let first = Data(#"{"next_record":1}"#.utf8)
            let second = Data(#"{"next_record":2}"#.utf8)
            recorder.append(first)
            recorder.waitUntilIdleForTesting()
            precondition(try! Data(contentsOf: file(directory: directory, index: 1)) == interrupted)
            precondition(try! Data(contentsOf: file(directory: directory, index: 0)) == first + Data([10]))
            recorder.append(second)
            recorder.waitUntilIdleForTesting()
            precondition(!recorder.disabledForProcessForTesting)
            precondition(try! Data(contentsOf: file(directory: directory, index: 0)) == first + Data([10]) + second + Data([10]))
            precondition(try! Data(contentsOf: file(directory: directory, index: 1)) == interrupted)
            if full {
                let names = try fileManager.contentsOfDirectory(atPath: directory.path)
                precondition(names.filter { $0.hasPrefix("commit-stream-v5-") }.count == 8)
                for index in 2..<8 {
                    precondition(try! Data(contentsOf: file(directory: directory, index: index)) == Data("old-\(index - 1)\n".utf8))
                }
                precondition(!fileManager.fileExists(atPath: file(directory: directory, index: 8).path))
            }
        }
    }

    // The two tails that are safe to append to, an empty file and one ending in LF, do
    // not rotate.
    private static func assertStartupLineBoundaryCases(root: URL, fileManager: FileManager) throws {
        for (name, existing) in [("empty", Data()), ("complete", Data("opaque previous line\n".utf8))] {
            let directory = root.appendingPathComponent("boundary-\(name)", isDirectory: true)
            try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
            try existing.write(to: file(directory: directory, index: 0))
            let recorder = recorder(directory: directory, fileManager: fileManager)
            let next = Data("opaque next line".utf8)
            recorder.append(next)
            recorder.waitUntilIdleForTesting()
            precondition(!recorder.disabledForProcessForTesting)
            precondition(try! Data(contentsOf: file(directory: directory, index: 0)) == existing + next + Data([10]))
            precondition(!fileManager.fileExists(atPath: file(directory: directory, index: 1).path))
        }
    }

    // When the last byte cannot be read, the tail is treated as unsafe: the file rotates
    // after the retry, rather than being appended to or making the recorder stop. The
    // tail is read once per process, not once per record.
    private static func assertUnreadableTailRotatesAndIsCheckedOnlyOnce(root: URL, fileManager: FileManager) throws {
        let directory = root.appendingPathComponent("unreadable-tail", isDirectory: true)
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        let existing = Data("complete but unreadable boundary\n".utf8)
        try existing.write(to: file(directory: directory, index: 0))
        var reads = 0
        let clock = CommitStreamTestClock()
        let recorder = CommitStreamRecorder(processConfiguration: configuration(), directoryURL: directory, fileManager: fileManager,
            lastByteReader: { url in
                precondition(url == file(directory: directory, index: 0))
                reads += 1
                throw POSIXError(.EACCES)
            }, clock: clock.clock)
        let record = Data("new opaque payload".utf8)
        recorder.append(record)
        recorder.append(record)
        recorder.waitUntilIdleForTesting()
        precondition(recorder.status.retrying && recorder.status.pendingCount == 2)
        clock.advance(1); recorder.waitUntilIdleForTesting()
        precondition(reads == 1)
        precondition(!recorder.disabledForProcessForTesting)
        precondition(try! Data(contentsOf: file(directory: directory, index: 1)) == existing)
        precondition(try! Data(contentsOf: file(directory: directory, index: 0)) == record + Data([10]) + record + Data([10]))
    }

    // Only regular v4 files are archived. A symlink or a directory with a v4 name is
    // left alone, the symlink target is not read or moved, and no archive directory
    // appears, across repeated starts.
    private static func assertIneligibleV4NamesDoNotCreateArchives(root: URL, fileManager: FileManager) throws {
        let directory = root.appendingPathComponent("ineligible-v4", isDirectory: true)
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        let outside = root.appendingPathComponent("synthetic-outside.jsonl")
        let bytes = Data("synthetic legacy contents".utf8)
        try bytes.write(to: outside)
        let link = directory.appendingPathComponent("commit-stream-v4-0.jsonl")
        try fileManager.createSymbolicLink(at: link, withDestinationURL: outside)
        try fileManager.createDirectory(at: directory.appendingPathComponent("commit-stream-v4-1.jsonl"), withIntermediateDirectories: false)
        for _ in 0..<3 {
            let recorder = recorder(directory: directory, fileManager: fileManager)
            recorder.waitUntilIdleForTesting()
            precondition(!fileManager.fileExists(atPath: directory.appendingPathComponent("archive").path))
            precondition(!recorder.disabledForProcessForTesting)
        }
        precondition(try! Data(contentsOf: outside) == bytes)
        precondition(try! fileManager.destinationOfSymbolicLink(atPath: link.path) == outside.path)
    }

    // A failed write logs once, with no payload, and keeps both records pending for the
    // retry. The second payload's text says "discarded" from the older fail-closed
    // behaviour; it is retained now.
    private static func assertWriteFailureLogsOnceWithoutContents(root: URL, fileManager: FileManager) throws {
        let directory = root.appendingPathComponent("one-failure-log", isDirectory: true)
        let clock = CommitStreamTestClock()
        let recorder = CommitStreamRecorder(processConfiguration: configuration(), directoryURL: directory, fileManager: fileManager, clock: clock.clock)
        recorder.failNextWriteForTesting()
        recorder.append(Data("synthetic payload must not be logged".utf8))
        recorder.append(Data("another discarded payload".utf8))
        recorder.waitUntilIdleForTesting()
        // The test counter is incremented at the single content-free log site;
        // no OSLog history or actual user record is inspected.
        precondition(recorder.failureLogCount == 1)
        precondition(!recorder.status.stopped && recorder.status.retrying && recorder.status.pendingCount == 2)
        precondition(!fileManager.fileExists(atPath: file(directory: directory, index: 0).path))
    }

    // A record exactly at the 4 MiB cap is accepted, and its LF counts against the
    // 8 MiB file budget only. Two of them therefore rotate into two files.
    private static func assertMaximumOpaqueRecordBoundary(root: URL, fileManager: FileManager) throws {
        let directory = root.appendingPathComponent("exact-record-cap", isDirectory: true)
        let recorder = recorder(directory: directory, fileManager: fileManager)
        let record = Data(repeating: 0x61, count: 4 * 1_024 * 1_024)
        recorder.append(record)
        recorder.waitUntilIdleForTesting() // File limit, independently of ingress's 4 MiB cap.
        recorder.append(record)
        recorder.waitUntilIdleForTesting()
        // The framing newline belongs to the file budget, not the opaque record
        // budget; two maximum records must rotate rather than exceed 8 MiB (docs/src/internals/storage.md「記録」).
        precondition(!recorder.disabledForProcessForTesting)
        precondition(try! Data(contentsOf: file(directory: directory, index: 0)) == record + Data([10]))
        precondition(try! Data(contentsOf: file(directory: directory, index: 1)) == record + Data([10]))
    }

    // Numbered v4 files move into one 0700 archive generation; a name without a number
    // stays. If the move fails, the v4 file stays where it was and v5 recording goes on.
    // A second start does not add or rotate archives.
    private static func assertV4ArchiveSuccessAndFailure(root: URL, fileManager: FileManager) throws {
        for fail in [false,true] {
            let dir=root.appendingPathComponent(fail ? "archive-failure" : "archive-success",isDirectory:true)
            try fileManager.createDirectory(at:dir,withIntermediateDirectories:true)
            let legacy=dir.appendingPathComponent("commit-stream-v4-0.jsonl")
            let fixture=Data("synthetic legacy bytes\n".utf8)
            try fixture.write(to:legacy)
            let ignored=dir.appendingPathComponent("commit-stream-v4-not-a-number.jsonl")
            try fixture.write(to:ignored)
            let recorder:CommitStreamRecorder
            if fail { recorder=CommitStreamRecorder(processConfiguration:configuration(),directoryURL:dir,fileManager:fileManager,moveLegacyFile:{_,_ in throw POSIXError(.EACCES)}) }
            else { recorder=CommitStreamRecorder(processConfiguration:configuration(),directoryURL:dir,fileManager:fileManager) }
            let payload=Data("opaque v5".utf8)
            recorder.append(payload);recorder.waitUntilIdleForTesting()
            precondition(!recorder.disabledForProcessForTesting)
            precondition((try! Data(contentsOf:file(directory:dir,index:0))) == payload+Data([10]))
            precondition((try! Data(contentsOf:ignored)) == fixture)
            if fail {
                precondition((try! Data(contentsOf:legacy)) == fixture)
                let family = dir.appendingPathComponent("archive/v4")
                precondition(try! fileManager.contentsOfDirectory(atPath: family.path).isEmpty)
            }
            else {
                precondition(!fileManager.fileExists(atPath:legacy.path))
                let archive=dir.appendingPathComponent("archive/v4")
                let generations=try fileManager.contentsOfDirectory(at:archive,includingPropertiesForKeys:nil)
                precondition(generations.count==1)
                precondition(permissions(generations[0],fileManager:fileManager)==0o700)
                precondition((try! Data(contentsOf:generations[0].appendingPathComponent(legacy.lastPathComponent))) == fixture)
                // Another recorder start must not overwrite or rotate archives.
                let next=CommitStreamRecorder(processConfiguration:configuration(),directoryURL:dir,fileManager:fileManager)
                next.append(payload);next.waitUntilIdleForTesting()
                precondition(try! fileManager.contentsOfDirectory(at:archive,includingPropertiesForKeys:nil).count==1)
            }
        }
    }

    // A fixed configuration, so every recorder here is "configured" without reading a
    // bundle.
    private static func configuration() -> CommitStreamProcessConfiguration {
        CommitStreamProcessConfiguration(
            processNonce: "test-process",
            sourceCommit: String(repeating: "a", count: 40),
            build: 1
        )
    }

    // The production factory builds its configuration from the bundle's build
    // provenance alone (source commit and build number), and refuses a bad value of
    // either one.
    private static func assertProductionConfigurationAuthorityGates(root: URL, fileManager: FileManager) throws {
        let bundleRoot=root.appendingPathComponent("factory-bundle",isDirectory:true)
        try fileManager.createDirectory(at:bundleRoot,withIntermediateDirectories:true)
        let info:[String:Any]=["CFBundleIdentifier":"dev.muu.factory-test","CFBundleName":"MuuFactoryTest","CFBundlePackageType":"BNDL","CFBundleVersion":"374","MuuBuildVariant":"dogfood","MuuSourceCommit":String(repeating:"a",count:40)]
        let infoData=try PropertyListSerialization.data(fromPropertyList:info,format:.xml,options:0)
        try infoData.write(to:bundleRoot.appendingPathComponent("Info.plist"))
        let bundle=Bundle(url:bundleRoot)!
        // Recording configuration uses build provenance only; it does not
        // depend on conversion resources (docs/src/internals/storage.md「記録」).
        let config=CommitStreamRecorder.productionConfigurationForTesting(bundle:bundle,processNonce:"test-process")
        precondition(config != nil)
        precondition(config == CommitStreamRecorder.productionConfigurationForTesting(bundle:bundle,processNonce:"test-process"))
        precondition(config?.schema == "muu.commit-stream.v5")
        let object=try JSONSerialization.jsonObject(with:JSONEncoder().encode(config!.makeEngineConfiguration())) as! [String:Any]
        precondition(Set(object.keys)==Set(["schema","process_nonce","source_commit","build"]))
        precondition(object["process_nonce"] as? String == "test-process")
        precondition(object["source_commit"] as? String == String(repeating:"a",count:40))
        precondition(object["build"] as? Int == 374)
        // Contract: the factory's shared decision, reached through the test
        // seam, is off unless a matching dogfood build opts in. This file is
        // compiled both ways. Standard reads no key at all, whatever the
        // preference. Dogfood reads it once; absent and false never resolve
        // storage, report nothing requested or configured, give Core no
        // stream setting, and write nothing on append. Negatives: a default-on factory, an early path
        // lookup, or a created directory. `settings[key] ?? false` mirrors
        // UserDefaults.bool for an absent key. True must then write the exact
        // opaque bytes plus LF.
        for (name, preference) in [("absent", nil), ("false", false), ("true", true)] as [(String, Bool?)] {
            // The literal is the external contract: users' defaults use this
            // name. Using the constant would let a rename pass unnoticed.
            let key = "MuuCommitStreamEnabled"
            let settings = preference.map { [key: $0] } ?? [:]
            let support = root.appendingPathComponent("factory-preference-" + name, isDirectory: true)
            var queriedKeys: [String] = []
            var directoryLookups = 0
            let recorder = CommitStreamRecorder.makeProcessRecorderForTesting(
                readBool: { key in
                    queriedKeys.append(key)
                    return settings[key] ?? false
                },
                bundle: bundle,
                applicationSupportDirectory: {
                    directoryLookups += 1
                    return support
                }
            )
            #if MUU_DOGFOOD
            let permitsRecording = true
            #else
            let permitsRecording = false
            #endif
            let enabled = permitsRecording && preference == true
            precondition(queriedKeys == (permitsRecording ? [key] : []), "standard must not read recording defaults")
            precondition(directoryLookups == (enabled ? 1 : 0))
            precondition(recorder.status.availableInBuild == permitsRecording)
            precondition(recorder.status.requested == enabled)
            precondition(recorder.status.configured == enabled)
            precondition((recorder.makeEngineConfiguration() != nil) == enabled)
            let bytes = try record(caseName: "factory-" + name)
            recorder.append(bytes)
            recorder.waitUntilIdleForTesting()
            if enabled {
                let directory = support.appendingPathComponent("MuuIME/Dogfood/CommitStream", isDirectory: true)
                precondition(try! Data(contentsOf: file(directory: directory, index: 0)) == bytes + Data([10]))
            } else {
                precondition(!fileManager.fileExists(atPath: support.path))
            }
        }
        // A plist variant that does not match the compiled build: no defaults,
        // no storage, no Core setting. In the standard compile the capability
        // check already returns; the dogfood compile is what reaches the plist
        // comparison.
        for variant in ["standard", "unknown"] {
            let dir = root.appendingPathComponent("factory-variant-" + variant, isDirectory: true)
            try fileManager.createDirectory(at: dir, withIntermediateDirectories: true)
            var mismatch = info; mismatch["MuuBuildVariant"] = variant
            try PropertyListSerialization.data(fromPropertyList: mismatch, format: .xml, options: 0)
                .write(to: dir.appendingPathComponent("Info.plist"))
            let recorder = CommitStreamRecorder.makeProcessRecorderForTesting(
                readBool: { _ in preconditionFailure("mismatched build consulted defaults") },
                bundle: Bundle(url: dir)!,
                applicationSupportDirectory: { preconditionFailure("mismatched build consulted storage") })
            precondition(!recorder.status.availableInBuild && !recorder.status.requested)
            precondition(recorder.makeEngineConfiguration() == nil)
        }
        for (name,key,bad) in [("commit","MuuSourceCommit","bad"),("build","CFBundleVersion","bad")] {
            let dir=root.appendingPathComponent("factory-invalid-"+name,isDirectory:true)
            try fileManager.createDirectory(at:dir,withIntermediateDirectories:true)
            var invalid=info;invalid[key]=bad
            try PropertyListSerialization.data(fromPropertyList:invalid,format:.xml,options:0).write(to:dir.appendingPathComponent("Info.plist"))
            precondition(CommitStreamRecorder.productionConfigurationForTesting(bundle:Bundle(url:dir)!) == nil)
        }
    }

    /// A valid record whose bytes differ per case.
    private static func record(caseName: String) throws -> Data {
        try validRecord(engineNonce: caseName)
    }

    private static func recorder(
        directory: URL,
        fileManager: FileManager
    ) -> CommitStreamRecorder {
        CommitStreamRecorder(
            processConfiguration: configuration(),
            directoryURL: directory,
            fileManager: fileManager
        )
    }

    private static func validRecord(engineNonce: String = "opaque") throws -> Data {
        // Swift has no record decoder. This fixture deliberately need not
        // satisfy Rust's schema; its bytes must be persisted unchanged (docs/src/internals/storage.md「記録」).
        try JSONSerialization.data(withJSONObject:["case":engineNonce,"unexpected":true])
    }

    // Swift checks framing only. Records that the old Swift validator rejected, and
    // bytes that are not JSON or UTF-8, are written unchanged. What it does refuse is
    // framing: an empty record, an LF or a CR, and anything over 4 MiB, each counted as
    // one discard.
    private static func assertOpaquePayloadContentsAreNotValidated(root: URL, fileManager: FileManager) throws {
        // Each former structured-validator negative is now an opaque-payload
        // preservation case. Rust tests own all schema/authority validation.
        let oldNegatives=["retired-schema-v2","retired-schema-v3","model-identity","native-scorer-required","range-count","empty-reading","empty-partition","empty-baseline-id","boolean-margin","boolean-causal-nll","range-rebind","duplicate-rank","hypothesis-cardinality","authority-type","authority-config-drift","overlay-order","overlay-schema","overlay-domain","overlay-hex","overlay-empty-iff","missing-required","process-nonce-required","segment-flags-required","segment-flags-type","dictionary-sha-mismatch","dictionary-token-count-mismatch"]
        for name in oldNegatives {
            let dir=root.appendingPathComponent("opaque-"+name,isDirectory:true)
            let recorder=recorder(directory:dir,fileManager:fileManager)
            let data=try JSONSerialization.data(withJSONObject:["schema":"unexpected","case":name,"authority":false])
            recorder.append(data);recorder.waitUntilIdleForTesting()
            precondition(!recorder.disabledForProcessForTesting,name)
            precondition((try! Data(contentsOf:file(directory:dir,index:0))) == data+Data([10]),name)
        }
        for (index,data) in [Data("not-json".utf8),Data([0xff,0xfe]),Data("{}".utf8)].enumerated() {
            let dir=root.appendingPathComponent("opaque-bytes-\(index)")
            let recorder=recorder(directory:dir,fileManager:fileManager)
            recorder.append(data);recorder.waitUntilIdleForTesting()
            precondition(!recorder.disabledForProcessForTesting)
            precondition((try! Data(contentsOf:file(directory:dir,index:0))) == data+Data([10]))
        }
        for (index,data) in [Data(),Data([10]),Data([13]),Data(repeating:0x61,count:4*1024*1024+1)].enumerated() {
            let dir=root.appendingPathComponent("framing-\(index)")
            let recorder=recorder(directory:dir,fileManager:fileManager)
            recorder.append(data);recorder.waitUntilIdleForTesting()
            precondition(!recorder.disabledForProcessForTesting && recorder.status.discarded == 1)
            precondition(!fileManager.fileExists(atPath:file(directory:dir,index:0).path))
        }
    }

    // With no configuration the recorder is off: an append creates no directory, and
    // being off is not a failure.
    private static func assertDefaultOffDoesNotCreateStorage(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("default-off", isDirectory: true)
        let recorder = CommitStreamRecorder(
            processConfiguration: nil,
            directoryURL: directory,
            fileManager: fileManager
        )
        recorder.append(try record(caseName: "disabled"))
        recorder.waitUntilIdleForTesting()
        precondition(!fileManager.fileExists(atPath: directory.path))
        precondition(
            !recorder.disabledForProcessForTesting,
            recorder.failureForTestingDescription ?? "unknown recorder failure"
        )
    }

    // The process configuration hands Core the same values on every call. Session
    // nonces are minted by Rust, so a fresh value per call here would split one
    // process's records.
    private static func assertProcessConfigurationDoesNotMintSessionNonces(root _: URL, fileManager _: FileManager) throws {
        let process=configuration(),first=process.makeEngineConfiguration(),second=process.makeEngineConfiguration()
        precondition(first.processNonce==second.processNonce)
        precondition(first==second,"Session identity belongs to Rust, not process config")
        precondition(first.sourceCommit==second.sourceCommit)
        precondition(first.build==second.build)
    }

    // The bytes plus one LF, in a 0700 directory and a 0600 file.
    private static func assertValidRecordAndOwnerOnlyPermissions(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("valid", isDirectory: true)
        let recorder = recorder(directory: directory, fileManager: fileManager)
        let expected = try record(caseName: "valid")
        recorder.append(expected)
        recorder.waitUntilIdleForTesting()
        precondition(
            !recorder.disabledForProcessForTesting,
            recorder.failureForTestingDescription ?? "unknown recorder failure"
        )

        let active = file(directory: directory, index: 0)
        let bytes = try Data(contentsOf: active)
        precondition(bytes == expected + Data([0x0a]))
        precondition(permissions(directory, fileManager: fileManager) == 0o700)
        precondition(permissions(active, fileManager: fileManager) == 0o600)
    }

    // A record with an embedded newline is discarded alone; the next record is written.
    private static func assertMalformedRecordDropsOnlyItself(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("malformed", isDirectory: true)
        let recorder = recorder(directory: directory, fileManager: fileManager)
        recorder.append(Data("two\nlines".utf8))
        let expected = try record(caseName: "valid-after-bad-framing")
        recorder.append(expected)
        recorder.waitUntilIdleForTesting()
        precondition(!recorder.disabledForProcessForTesting && recorder.status.discarded == 1)
        let saved = try Data(contentsOf: file(directory: directory, index: 0))
        precondition(saved == expected + Data([10]))
    }

    // v5 uses its own file names, so a v1 file in the same directory is neither read nor
    // changed.
    private static func assertStorageDoesNotMixWithV1Evidence(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("schema-separation", isDirectory: true)
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        let legacy = directory.appendingPathComponent("commit-stream-0.jsonl")
        let legacyBytes = Data(#"{"schema":"muu.commit-stream.v1"}\n"#.utf8)
        precondition(fileManager.createFile(atPath: legacy.path, contents: legacyBytes))

        let recorder = recorder(directory: directory, fileManager: fileManager)
        let expected = try record(caseName: "v4")
        recorder.append(expected)
        recorder.waitUntilIdleForTesting()

        let preservedLegacy = try Data(contentsOf: legacy)
        let written = try Data(contentsOf: file(directory: directory, index: 0))
        precondition(preservedLegacy == legacyBytes)
        precondition(written == expected + Data([0x0a]))
    }

    // A record the size of a whole file is refused at receipt by the 4 MiB record cap,
    // before any writing, and counts as one discard.
    private static func assertOversizedRecordIsRejected(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("record-cap", isDirectory: true)
        let recorder = recorder(directory: directory, fileManager: fileManager)
        let prefix = Data(#"{"schema":"muu.commit-stream.v5","pad":""#.utf8)
        let suffix = Data(#""}"#.utf8)
        var record = prefix
        record.append(Data(repeating: 0x61, count: maximumFileBytes - prefix.count - suffix.count))
        record.append(suffix)
        precondition(record.count == maximumFileBytes)

        recorder.append(record)
        recorder.waitUntilIdleForTesting()
        precondition(!recorder.disabledForProcessForTesting && recorder.status.discarded == 1)
        precondition(!fileManager.fileExists(atPath: file(directory: directory, index: 0).path))
    }

    // A symlinked ancestor of the recording directory disables recording for the process
    // and writes nothing at the link target.
    private static func assertSymlinkAncestorCannotEscape(
        root: URL,
        fileManager: FileManager
    ) throws {
        let outside = root.appendingPathComponent("outside", isDirectory: true)
        try fileManager.createDirectory(at: outside, withIntermediateDirectories: true)
        let link = root.appendingPathComponent("linked-parent", isDirectory: true)
        try fileManager.createSymbolicLink(at: link, withDestinationURL: outside)
        let recorder = recorder(
            directory: link.appendingPathComponent("CommitStream", isDirectory: true),
            fileManager: fileManager
        )
        recorder.append(try record(caseName: "escape"))
        recorder.waitUntilIdleForTesting()
        precondition(recorder.disabledForProcessForTesting)
        let outsideContents = try fileManager.contentsOfDirectory(atPath: outside.path)
        precondition(outsideContents.isEmpty)
    }

    // A failed write waits and keeps its records, and the next retry writes them all.
    // Nothing is discarded, despite the second record's name.
    private static func assertInjectedWriteFailureWaitsAndRecovers(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("injected-failure", isDirectory: true)
        let clock = CommitStreamTestClock()
        let recorder = CommitStreamRecorder(processConfiguration: configuration(), directoryURL: directory, fileManager: fileManager, clock: clock.clock)
        recorder.failNextWriteForTesting()
        recorder.append(try record(caseName: "simulated-disk-full"))
        recorder.append(try record(caseName: "discarded-after-failure"))
        recorder.waitUntilIdleForTesting()
        precondition(!recorder.status.stopped && recorder.status.retrying)
        precondition(!fileManager.fileExists(atPath: directory.path))
        clock.advance(1); recorder.waitUntilIdleForTesting()
        precondition(!recorder.status.retrying && recorder.status.pendingCount == 0 && recorder.status.discarded == 0)
    }

    // A directory that cannot be created yet is treated like a write failure: wait,
    // then recover once the permission is restored.
    private static func assertPermissionDenialWaitsAndRecovers(
        root: URL,
        fileManager: FileManager
    ) throws {
        let parent = root.appendingPathComponent("permission-denied", isDirectory: true)
        try fileManager.createDirectory(at: parent, withIntermediateDirectories: true)
        try fileManager.setAttributes(
            [.posixPermissions: 0o500],
            ofItemAtPath: parent.path
        )
        defer {
            try? fileManager.setAttributes(
                [.posixPermissions: 0o700],
                ofItemAtPath: parent.path
            )
        }
        let directory = parent.appendingPathComponent("CommitStream", isDirectory: true)
        let clock = CommitStreamTestClock()
        let recorder = CommitStreamRecorder(processConfiguration: configuration(), directoryURL: directory, fileManager: fileManager, clock: clock.clock)
        recorder.append(try record(caseName: "permission-denied"))
        recorder.waitUntilIdleForTesting()
        precondition(!recorder.status.stopped && recorder.status.retrying)
        precondition(!fileManager.fileExists(atPath: file(directory: directory, index: 0).path))
        try fileManager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: parent.path)
        clock.advance(1); recorder.waitUntilIdleForTesting()
        precondition(!recorder.status.retrying && recorder.status.pendingCount == 0 && recorder.status.discarded == 0)
    }

    // Rotation keeps exactly eight files: the full active file moves to slot 1, the
    // oldest content falls off, and no slot 8 is created.
    private static func assertRotationIsBounded(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("rotation", isDirectory: true)
        let recorder = recorder(directory: directory, fileManager: fileManager)
        recorder.append(try record(caseName: "seed"))
        recorder.waitUntilIdleForTesting()
        for index in 0 ..< 8 {
            let count = index == 0 ? maximumFileBytes : 1
            try Data(repeating: UInt8(index), count: count).write(
                to: file(directory: directory, index: index)
            )
        }

        let appended = try record(caseName: "rotate")
        recorder.append(appended)
        recorder.waitUntilIdleForTesting()
        precondition(!recorder.disabledForProcessForTesting)
        let contents = try fileManager.contentsOfDirectory(atPath: directory.path)
        let active = try Data(contentsOf: file(directory: directory, index: 0))
        let previous = try Data(contentsOf: file(directory: directory, index: 1))
        let oldest = try Data(contentsOf: file(directory: directory, index: 7))
        precondition(contents.filter { $0.hasPrefix("commit-stream-v5-") }.count == 8)
        precondition(active == appended + Data([0x0a]))
        precondition(previous.count == maximumFileBytes)
        precondition(oldest == Data([6]))
        precondition(!fileManager.fileExists(atPath: file(directory: directory, index: 8).path))
    }

    // When rotation hits a symlink in the last slot, recording stops for the process,
    // rather than writing through the link or removing it.
    private static func assertRotationFailureIsFailClosed(
        root: URL,
        fileManager: FileManager
    ) throws {
        let directory = root.appendingPathComponent("rotation-failure", isDirectory: true)
        let recorder = recorder(directory: directory, fileManager: fileManager)
        recorder.append(try record(caseName: "seed"))
        recorder.waitUntilIdleForTesting()
        try Data(repeating: 0, count: maximumFileBytes).write(
            to: file(directory: directory, index: 0)
        )
        let sentinel = root.appendingPathComponent("outside-sentinel")
        let sentinelBytes = Data("outside-must-survive".utf8)
        try sentinelBytes.write(to: sentinel)
        try fileManager.createSymbolicLink(
            at: file(directory: directory, index: 7),
            withDestinationURL: sentinel
        )

        recorder.append(try record(caseName: "must-fail"))
        recorder.append(try record(caseName: "must-also-drop"))
        recorder.waitUntilIdleForTesting()
        precondition(recorder.disabledForProcessForTesting)
        let preserved = try Data(contentsOf: sentinel)
        precondition(preserved == sentinelBytes)
        precondition(fileManager.fileExists(atPath: file(directory: directory, index: 7).path))
    }

    private static func file(directory: URL, index: Int) -> URL {
        directory.appendingPathComponent("commit-stream-v5-\(index).jsonl")
    }

    private static func permissions(_ url: URL, fileManager: FileManager) -> Int {
        let attributes = try! fileManager.attributesOfItem(atPath: url.path)
        return (attributes[.posixPermissions] as! NSNumber).intValue & 0o777
    }
}
