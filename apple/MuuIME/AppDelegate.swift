import AppKit
import InputMethodKit
import OSLog

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate {
    private let logger = Logger(subsystem: MuuConstants.bundleIdentifier, category: "lifecycle")
    private var server: IMKServer?
    private let lifecycle = ApplicationLifecycle()
    private var terminationReplyRegistered = false
    private var terminationReplied = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        LifecycleLog.shared.launched(build: Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "unknown",
            source: Bundle.main.object(forInfoDictionaryKey: "MuuSourceCommit") as? String ?? "unknown")
        // Local, read-only inspection runs off main and never gates Core or
        // input. Only About's explicit action may create a network task.
        let modelDownload = ModelDownloadStore.shared
        AboutStatusProvider.shared.configureModelDownload(modelDownload)
        // Registered in every launch mode, ABI mismatch included, because an
        // attempt does not depend on Core. On termination it joins the same
        // 2 s lifecycle deadline as the stores: the attempt is cancelled and
        // reports only after its worker has released the partial, FD and
        // lock, so a rename is either done or not done, never cut off without
        // being accounted for. It does not extend the deadline or wait on main.
        lifecycle.register(.init(name: .modelDownload, start: { _, done in
            modelDownload.beginShutdown(completion: done)
        }))
        modelDownload.start()
        // Launch order is load-bearing (docs/src/internals/architecture.md「起動と終了」):
        //
        // 1. Initialize the core. Until this returns, session creation and
        //    dictionary operations fail at once instead of waiting.
        // 2. Take the learning baseline and the user dictionary view, then
        //    start the Postal and Imported loads. Constructing either of those
        //    stores does no IO and sends nothing, so earlier access cannot
        //    lose a load that Core would refuse before it is ready.
        // 3. Create the IMKServer last. It is what lets macOS create input
        //    controllers; a controller created earlier would get no session and
        //    stay in pass-through.
        //
        // If initialization fails the server still starts, so every controller
        // passes keys through instead of the input method disappearing.
        let actualABI = muu_abi_version()
        if actualABI == MuuConstants.expectedABIVersion {
            // Learning, user and recorder take part in shutdown even when
            // initialization fails below. Postal and Imported are created and
            // registered only after it succeeds.
            let learningStore = PersonalFrequencyStore.shared
            let userDictionary = UserDictionaryStore.shared
            let recorder = CommitStreamRecorder.shared
            lifecycle.register(.init(name: .learning, start: { deadline, done in
                learningStore.beginShutdown(deadline: deadline, now: { ProcessInfo.processInfo.systemUptime }) { result in
                    LifecycleLog.shared.learningQueries(learningStore.shutdownQueryCount)
                    if learningStore.persistenceStatus.saveIssue == .durabilityWarning { LifecycleLog.shared.storeWarning(.learning) }
                    done(result)
                }
            }))
            lifecycle.register(.init(name: .user, start: { _, done in userDictionary.beginShutdown(completion: done) }))
            lifecycle.register(.init(name: .recorder, start: { deadline, done in
                recorder.beginShutdown(deadline: deadline) { result in
                    let state = recorder.status
                    LifecycleLog.shared.recorder(pending: state.pendingCount, discarded: state.discarded, reportPending: state.reportPending)
                    done(result)
                }
            }, expire: {
                recorder.expireShutdown()
                let state = recorder.status
                LifecycleLog.shared.recorder(pending: state.pendingCount, discarded: state.discarded, reportPending: state.reportPending)
            }))
            switch RustCore.initialize(RustCore.defaultConfiguration(userDictionaryFileState: userDictionary.startupFile.state), learning: learningStore.startupData, userDictionaryFile: userDictionary.startupFile.bytes) {
            case .success(let result):
                learningStore.acceptStartup(result)
                do { try userDictionary.acceptInitializedCore() }
                catch { logger.error("User dictionary view unavailable; code=user_dictionary_view_failed") }
                let postal = PostalCodeStore.shared
                lifecycle.register(.init(name: .postal, start: { _, done in postal.beginShutdown(completion: done) }))
                postal.start()
                let imported = ImportedDictionaryStore.shared
                lifecycle.register(.init(name: .imported, start: { _, done in imported.beginShutdown(completion: done) }))
                imported.start()
                AboutStatusProvider.shared.configure(initialization: .ready, build: .init(bundle: .main),
                    learning: learningStore, user: userDictionary, imported: imported, postal: postal,
                    recorder: CommitStreamRecorder.shared)
            case .failure:
                AboutStatusProvider.shared.configure(initialization: .failed, build: .init(bundle: .main),
                    learning: learningStore, user: userDictionary, recorder: CommitStreamRecorder.shared)
                logger.fault("Core initialization failed; category=initialization")
            }
        } else {
            AboutStatusProvider.shared.configure(initialization: .abiMismatch, build: .init(bundle: .main))
            logger.fault(
                "Rust ABI mismatch; expected=\(MuuConstants.expectedABIVersion, privacy: .public) actual=\(actualABI, privacy: .public)"
            )
        }
        server = IMKServer(
            name: MuuConstants.connectionName,
            bundleIdentifier: MuuConstants.bundleIdentifier
        )
        logger.notice(
            "IMKServer started; abi=\(actualABI, privacy: .public) core=\(String(cString: muu_core_version()), privacy: .public)"
        )
    }

    // A repeated request joins the run already in progress and gets
    // terminateLater again; the one reply is sent when that run finishes.
    //
    // About requests normal termination, just like logout. Keep sudden
    // termination disabled in Info.plist so AppKit cannot skip this callback
    // and the final learning flush. The release check also rejects opting
    // into automatic termination; shutdown remains explicitly owned by this
    // bounded lifecycle.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        lifecycle.begin(.should)
        if !terminationReplyRegistered {
            terminationReplyRegistered = true
            lifecycle.whenFinished { [weak self] _ in
                guard let self, !self.terminationReplied else { return }
                self.terminationReplied = true
                LifecycleLog.shared.replied()
                sender.reply(toApplicationShouldTerminate: true)
            }
        }
        return .terminateLater
    }

    func applicationWillTerminate(_ notification: Notification) {
        lifecycle.begin(.will)
        // May be the only notification. The IO ledger never waits for main;
        // should+will share this same deadline rather than another two seconds.
        lifecycle.waitForCompletion()
    }
}

enum MuuConstants {
    // `just ffi-abi-contract-check` matches this exact line against the Rust
    // ABI version. Every Swift comparison goes through this constant.
    static let expectedABIVersion: UInt32 = 13
    static let bundleIdentifier: String = {
        guard let identifier = Bundle.main.bundleIdentifier else {
            preconditionFailure("MuuIME must run from a bundle with CFBundleIdentifier")
        }
        return identifier
    }()
    static let connectionName = "\(bundleIdentifier)_Connection"
}
