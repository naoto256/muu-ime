import Foundation
import OSLog

struct EngineResponse {
    let status: Int32
    let view: ViewState?
    // A valid commit whose display fields could not be decoded safely. This
    // belongs to this response only; it is neither queued nor put on the wire.
    let recoveredCommitText: String?

    init(status: Int32, view: ViewState?, recoveredCommitText: String? = nil) {
        self.status = status
        self.view = view
        self.recoveredCommitText = recoveredCommitText
    }

    static func decodeView(_ buffer: MuuBuffer, status: Int32) throws -> EngineResponse {
        // Check before copying foreign memory, including a non-null pointer
        // that advertises an impossible length. The caller still frees it.
        guard buffer.len >= 0, buffer.len <= MuuEnvelope.maximumViewBytes else {
            throw MuuProtocolError.oversizedView
        }
        guard let pointer = buffer.data else {
            guard buffer.len == 0 else { throw MuuProtocolError.shortEnvelope }
            return EngineResponse(status: status, view: nil)
        }
        return try decodeView(Data(bytes: pointer, count: buffer.len), status: status)
    }

    static func decodeView(_ data: Data, status: Int32) throws -> EngineResponse {
        let decoded = try MuuEnvelope.decodeViewResponse(data, recoverCommit: status == MUU_RESULT_OK)
        switch decoded {
        case .complete(let view):
            return EngineResponse(status: status, view: view)
        case .recovered(let view):
            Logger(subsystem: "dev.muu.inputmethod.Muu", category: "RustEngine")
                .error("view_decode_commit_recovered")
            return EngineResponse(status: status, view: view)
        case .commitOnly(let text):
            Logger(subsystem: "dev.muu.inputmethod.Muu", category: "RustEngine")
                .error("view_decode_commit_recovered")
            return EngineResponse(status: status, view: nil, recoveredCommitText: text)
        }
    }
}

/// Mirrors the Rust ordering: generation first, then revision. Every cursor in
/// `PersonalFrequencyStore` moves only forward in this order, so the first
/// state after a clear, `(g + 1, 0)`, supersedes anything from generation `g`.
struct LearningVersion: Codable, Comparable {
    let generation: UInt64
    let revision: UInt64
    static func < (lhs: Self, rhs: Self) -> Bool {
        lhs.generation == rhs.generation
            ? lhs.revision < rhs.revision : lhs.generation < rhs.generation
    }
}

struct CoreInitializationResult: Decodable {
    struct Degradation: Decodable {
        let personalFrequencyInvalid: Bool
        let personalFrequencyUnverifiable: Bool
        let userDictionaryInvalid: Bool
        enum CodingKeys: String, CodingKey {
            case personalFrequencyInvalid = "personal_frequency_invalid"
            case personalFrequencyUnverifiable = "personal_frequency_unverifiable"
            case userDictionaryInvalid = "user_dictionary_invalid"
        }
    }
    let degraded: Degradation
    let personalFrequencyVersion: LearningVersion
    enum CodingKeys: String, CodingKey {
        case degraded
        case personalFrequencyVersion = "personal_frequency_version"
    }
}

/// Host configuration only. Core decides presence, decoding and fallback;
/// no file-presence guess here can hide a failed request from About or v5.
struct ConversionLmConfiguration {
    let model: ConversionLmModel?
    let directory: String?
    let fallbackDirectory: String?
    // Off, or without Application Support, no model is configured and Core
    // reports the route as `disabled`. An unset or unknown model setting means
    // zenz. Neither model gets a fallback directory: a missing zenz is
    // reported as no model rather than silently replaced by an edge cache.
    static func resolve(enabled: Bool, modelSetting: String?, support: URL?) -> Self {
        guard enabled, let support else { return .init(model: nil, directory: nil, fallbackDirectory: nil) }
        let model = modelSetting.flatMap(ConversionLmModel.init(rawValue:)) ?? .zenz
        return .init(model: model, directory: model.directory(under: support).path,
            // Distribution acquires only zenz. An unavailable zenz must remain
            // visible as model-less, even if an old edge cache is present.
            // The additive C ABI's explicit fallback support stays available.
            fallbackDirectory: nil)
    }
}

struct CoreConfiguration: Encodable {
    let dictionaryPath: String?
    let conversionLmDirectory: String?
    let conversionLmModel: ConversionLmModel?
    let conversionLmFallbackDirectory: String?
    let commitStream: CommitStreamEngineConfiguration?
    let userDictionaryFileState: String
    init(dictionaryPath: String?, conversionLmDirectory: String?, commitStream: CommitStreamEngineConfiguration?,
         userDictionaryFileState: String = "missing", conversionLmModel: ConversionLmModel? = nil,
         conversionLmFallbackDirectory: String? = nil) {
        self.dictionaryPath = dictionaryPath; self.conversionLmDirectory = conversionLmDirectory
        self.conversionLmModel = conversionLmModel; self.conversionLmFallbackDirectory = conversionLmFallbackDirectory
        self.commitStream = commitStream; self.userDictionaryFileState = userDictionaryFileState
    }
    enum CodingKeys: String, CodingKey {
        case dictionaryPath = "dictionary_path", conversionLmDirectory = "conversion_lm_dir"
        case conversionLmModel = "conversion_lm_model", conversionLmFallbackDirectory = "conversion_lm_fallback_dir"
        case commitStream = "commit_stream", userDictionaryFileState = "user_dictionary_file_state"
    }
}

struct CoreOperationError: Error {
    let status: Int32
    let code: String
}

struct CoreRuntimeStatus: Decodable, Sendable {
    enum SystemDictionary: String, Decodable, Sendable { case available, kanaOnly = "kana_only" }
    enum LanguageModel: String, Decodable, Sendable { case unloaded, loading, available, failed }
    let schemaVersion: Int
    let systemDictionary: SystemDictionary
    let languageModel: LanguageModel
    let commitStreamEnabled: Bool
    struct ModelDetails: Decodable, Sendable {
        enum Route: String, Decodable, Sendable { case disabled, loading, requested, fallback, unavailable }
        enum Failure: String, Decodable, Sendable {
            case notFound = "not_found", loadFailed = "load_failed"
            var description: String { self == .notFound ? "見つかりません" : "読み込み失敗" }
        }
        let requestedModel: ConversionLmModel?
        let actualModel: ConversionLmModel?
        let route: Route
        let fallbackReason: Failure?
        let unavailableReason: Failure?
        enum CodingKeys: String, CodingKey {
            case requestedModel = "requested_model", actualModel = "actual_model", route
            case fallbackReason = "fallback_reason", unavailableReason = "unavailable_reason"
        }
    }
    var languageModelDetails: ModelDetails? = nil
    var isSupported: Bool { schemaVersion == 1 || (schemaVersion == 2 && languageModelDetails != nil) }
    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version", systemDictionary = "system_dictionary"
        case languageModel = "language_model", commitStreamEnabled = "commit_stream_enabled"
        case languageModelDetails = "language_model_details"
    }
}

/// This flag is process-wide. A background dictionary load or edit can detect a
/// terminal error before an input controller next becomes active.
///
/// Once set it is never cleared: the Rust core cannot be rebuilt inside the
/// process, so recreating sessions would fail forever. Controllers read it to
/// clear their presentation once and then pass keys through
/// (docs/src/internals/architecture.md「失敗したとき」). The lock exists because those store
/// operations run off the main thread while controllers read the flag on main.
enum RustCore {
    private static let lock = NSLock()
    private static var invalid = false
    private static let logger = Logger(subsystem: "dev.muu.inputmethod.Muu", category: "Core")
    #if MUU_TEST
        static var initializedForTesting = false
        static var preparationFailureForTesting: ((String) -> Void)?
        static var cancellationFailureForTesting: ((String, Int32) -> Void)?
        static var statusObserverForTesting: ((Int32) -> Void)?
    #endif
    static var isInvalid: Bool {
        lock.lock()
        defer { lock.unlock() }
        return invalid
    }
    static func observeStatus(_ status: Int32) {
        #if MUU_TEST
        statusObserverForTesting?(status)
        #endif
        guard status == MUU_RESULT_CORE_INVALID else { return }
        lock.lock()
        let first = !invalid
        invalid = true
        lock.unlock()
        if first { logger.fault("Conversion unavailable; result=\(status, privacy: .public)") }
    }

    static func runtimeStatus() -> Result<CoreRuntimeStatus, CoreOperationError> {
        var output = MuuBuffer(data: nil, len: 0)
        let status = muu_core_runtime_status(&output)
        defer { muu_buffer_free(output) }
        // The menu build calls this, so opening the input menu can be the
        // first Swift observation of CORE_INVALID. The process flag it sets
        // then drives the input entry points as usual.
        observeStatus(status)
        guard status == MUU_RESULT_OK else {
            return .failure(CoreOperationError(status: status, code: "runtime_status_unavailable"))
        }
        guard let bytes = output.data, output.len > 0, output.len <= 16 * 1024,
              let report = try? JSONDecoder().decode(CoreRuntimeStatus.self,
                from: Data(bytes: bytes, count: output.len)), report.isSupported
        else { return .failure(CoreOperationError(status: MUU_RESULT_INTERNAL, code: "runtime_status_invalid")) }
        return .success(report)
    }
    /// Call once at launch, before any session or store operation: both fail
    /// immediately, without waiting, until this has returned. `learning` is
    /// the saved file's bytes, or nil when there is no file. An unreadable or
    /// empty file must be passed as empty `Data`, not nil, so that Rust
    /// reports it as invalid instead of accepting "no file" as a clean start.
    static func initialize(_ configuration: CoreConfiguration, learning: Data?, userDictionaryFile: Data = Data()) -> Result<
        CoreInitializationResult, Error
    > {
        guard muu_abi_version() == MuuConstants.expectedABIVersion else {
            return .failure(
                CoreOperationError(status: Int32(MUU_RESULT_CREATE_FAILED), code: "abi_mismatch"))
        }
        do {
            let config = try JSONEncoder().encode(configuration)
            let bytes = learning ?? Data()
            var output = MuuBuffer(data: nil, len: 0)
            let status = config.withUnsafeBytes { configBuffer in
                bytes.withUnsafeBytes { learningBuffer in
                    userDictionaryFile.withUnsafeBytes { userBuffer in
                        muu_core_initialize_with_user_dictionary_file(
                            configBuffer.bindMemory(to: UInt8.self).baseAddress, configBuffer.count,
                            learningBuffer.bindMemory(to: UInt8.self).baseAddress, learningBuffer.count,
                            learning != nil, userBuffer.bindMemory(to: UInt8.self).baseAddress, userBuffer.count, &output)
                    }
                }
            }
            defer { muu_buffer_free(output) }
            observeStatus(status)
            guard status == MUU_RESULT_OK else { return .failure(error(status, output)) }
            let result = try JSONDecoder().decode(CoreInitializationResult.self, from: copy(output))
            if result.degraded.userDictionaryInvalid {
                logger.error("Startup user dictionary rejected; code=user_dictionary_invalid")
            }
            #if MUU_TEST
            initializedForTesting = true
            #endif
            return .success(result)
        } catch { return .failure(error) }
    }
    static var userDictionaryBridge: UserDictionaryCoreBridge {
        UserDictionaryCoreBridge(view: {
            var out = MuuBuffer(data: nil, len: 0)
            let status = muu_core_user_dictionary_view(&out)
            defer { muu_buffer_free(out) }
            observeStatus(status)
            guard status == MUU_RESULT_OK else { throw userDictionaryError(status, out) }
            guard out.len <= UserDictionaryStore.maxViewBytes else { throw UserDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "user_dictionary_output_too_large") }
            return try copy(out)
        }, prepare: { command in
            var file = MuuBuffer(data: nil, len: 0)
            var plan = MuuBuffer(data: nil, len: 0)
            let status = command.withUnsafeBytes { bytes in
                muu_core_user_dictionary_prepare(bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, &file, &plan)
            }
            // Both outputs are freed regardless of status, including decode
            // or bound failures after a successful prepare (docs/src/internals/storage.md「辞書の編集は 1 つの取引にする」).
            defer { muu_buffer_free(file); muu_buffer_free(plan) }
            observeStatus(status)
            guard status == MUU_RESULT_OK else { throw userDictionaryError(status, plan) }
            return try withPreparedPlan(plan, format: .user, cancel: { ticket in
                    acknowledge(muu_core_user_dictionary_cancel, ticket)
                }) { planBytes in
                    guard file.len <= UserDictionaryStore.maxFileBytes else {
                        throw UserDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "user_dictionary_output_too_large")
                    }
                    return (try copy(file), planBytes)
                }
        }, publish: { ticket in
            acknowledge(muu_core_user_dictionary_publish, ticket)
        }, cancel: { ticket in
            cancelAcknowledgement(acknowledge(muu_core_user_dictionary_cancel, ticket))
        })
    }
    private struct CodeFailure: Decodable { let code: String }
    private struct ImportedFailure: Decodable {
        struct Params: Decodable { let line: Int?; let field: String? }
        let code: String
        let params: Params?
    }

    // The caller owns and frees the foreign buffer. Check its length before
    // dereferencing it; malformed envelopes retain each bridge's fallback.
    //
    // One decoder for the four failure envelopes (User, Imported, Postal and
    // the Core). Each passes its store's existing bound as `limit`. An empty,
    // null, oversized or undecodable body yields the bridge's fallback code,
    // and nothing is read or allocated from an advertised length past the
    // bound. Imported also takes `params.line` and `params.field`; a
    // malformed `params` makes the whole envelope fall back.
    private static func decodeFailure<Payload: Decodable, Failure: Error>(
        _ output: MuuBuffer, limit: Int, payload: Payload.Type,
        make: (Payload?) -> Failure
    ) -> Failure {
        guard output.len > 0, output.len <= limit, let bytes = output.data else { return make(nil) }
        return make(try? JSONDecoder().decode(payload, from: Data(bytes: bytes, count: output.len)))
    }

    #if MUU_TEST
    enum FailureKindForTesting { case user, imported, postal, core }
    static func failureForTesting(_ kind: FailureKindForTesting, status: Int32, output: MuuBuffer) -> Error {
        switch kind {
        case .user: return userDictionaryError(status, output)
        case .imported: return importedError(status, output)
        case .postal: return postalError(status, output)
        case .core: return error(status, output)
        }
    }
    static func acknowledgeForTesting(_ function: (UnsafePointer<UInt8>?, Int) -> Int32, _ command: Data) -> Int32 {
        acknowledge(function, command)
    }
    #endif
    private static func userDictionaryError(_ status: Int32, _ output: MuuBuffer) -> Error {
        decodeFailure(output, limit: UserDictionaryStore.maxViewBytes, payload: CodeFailure.self) {
            UserDictionaryOperationError(status: status, code: $0?.code ?? "user_dictionary_operation_failed")
        }
    }
    static var importedDictionaryBridge: ImportedDictionaryCoreBridge {
        ImportedDictionaryCoreBridge(view: {
            try importedRead(muu_core_imported_dictionary_view, limit: ImportedDictionaryStore.maxCatalogBytes)
        }, loadBegin: { command, file in
            try importedStep(muu_core_imported_dictionary_load_begin, command: command, file: file)
        }, loadSupply: { command, file in
            try importedStep(muu_core_imported_dictionary_load_supply, command: command, file: file)
        }, prepare: { command, file in
            try importedStep(muu_core_imported_dictionary_prepare, command: command, file: file)
        }, prepareSupply: { command, file in
            try importedStep(muu_core_imported_dictionary_prepare_supply, command: command, file: file)
        }, preparedBlob: { command in
            var output = MuuBuffer(data: nil, len: 0)
            let status = command.withUnsafeBytes { bytes in
                muu_core_imported_dictionary_prepared_blob(bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, &output)
            }
            defer { muu_buffer_free(output) }
            observeStatus(status)
            guard status == MUU_RESULT_OK else { throw importedError(status, output) }
            guard output.len <= ImportedDictionaryStore.maxSourceBytes else { throw ImportedDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "source_too_large") }
            return try copy(output)
        }, publish: { acknowledge(muu_core_imported_dictionary_publish, $0) },
           rollback: { acknowledge(muu_core_imported_dictionary_rollback, $0) },
           finish: { acknowledge(muu_core_imported_dictionary_finish, $0) },
           cancel: { cancelAcknowledgement(acknowledge(muu_core_imported_dictionary_cancel, $0)) },
           template: { try importedRead(muu_core_imported_dictionary_template, limit: ImportedDictionaryStore.maxSourceBytes) })
    }
    // Per-store bound, oversize error, log code and ticket decoder for
    // withPreparedPlan. The bounds are the stores' own limits on the plan.
    private struct PreparationFormat {
        let limit: Int
        let oversized: Error
        let failureCode: String
        let ticket: (Data) throws -> Data
        static let user = Self(limit: UserDictionaryStore.maxViewBytes,
            oversized: UserDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "user_dictionary_output_too_large"),
            failureCode: "user_dictionary_preparation_unreadable", ticket: UserDictionaryEditPlan.ticketBytes)
        static let imported = Self(limit: ImportedDictionaryStore.maxCatalogBytes,
            oversized: ImportedDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "catalog_too_large"),
            failureCode: "imported_dictionary_preparation_unreadable", ticket: { try ImportedDictionaryStep.ticket(from: $0).bytes })
        static let postal = Self(limit: PostalCodeStore.maxPlanBytes,
            oversized: PostalCodeOperationError(status: MUU_RESULT_INTERNAL, code: "invalid_plan"),
            failureCode: "postal_code_preparation_unreadable", ticket: { try PostalCodeStep.ticket(from: $0).bytes })
    }
    /// Takes ownership of a prepared edit reply. The plan is bounded, copied
    /// and its ticket extracted first; everything after that, the caller's
    /// `body` included, runs under the cancel, so any later local failure
    /// requests cancellation exactly once before the store sees the reply.
    /// The attempt and its ACK, OK or not, accompany the original error: the
    /// store learns it no longer owns that cancel, and stops only when the ACK
    /// failed. A successful `body` hands the ticket to the store, which then
    /// owns the cancel.
    ///
    /// A failure before the ticket is known cannot cancel: cancel needs that
    /// ticket. Rust's contract is that every successful preparation carries
    /// one within these bounds, so this is a contract violation. It fails
    /// closed and leaves the slot reserved until the process restarts, rather
    /// than adding an ABI call that releases a reservation without its
    /// ticket for a case the contract excludes.
    private static func withPreparedPlan<T>(_ plan: MuuBuffer, format: PreparationFormat,
        cancel: (Data) -> Int32, body: (Data) throws -> T) throws -> T {
        let planBytes: Data
        let ticket: Data
        do {
            // Keep this bound before copy/decode: an oversized foreign buffer
            // must not cause an unbounded allocation just to find its ticket.
            guard plan.len <= format.limit else { throw format.oversized }
            planBytes = try copy(plan)
            ticket = try format.ticket(planBytes)
        } catch {
            // Rust guarantees a ticket in every successful preparation. If
            // that contract breaks, fail closed without inventing authority.
            // The reservation may remain until restart; never log its bytes.
            logger.error("Edit reply rejected; code=\(format.failureCode, privacy: .public)")
            #if MUU_TEST
            preparationFailureForTesting?(format.failureCode)
            #endif
            throw error
        }
        do { return try body(planBytes) }
        // Even ACK 0 carries the attempt: a multi-step store may still own
        // this same ticket from an earlier reply and must not cancel it twice.
        catch {
            let status = cancelAcknowledgement(cancel(ticket))
            throw DictionaryPreparationFailure(original: error, cancellationStatus: status)
        }
    }

    #if MUU_TEST
    static func preparedPlanForTesting(kind: String, plan: Data, claimedLength: Int? = nil,
        cancel: (Data) -> Int32, body: () throws -> Void) throws {
        let format: PreparationFormat
        switch kind {
        case "user": format = .user
        case "imported": format = .imported
        case "postal": format = .postal
        default: preconditionFailure("unknown preparation fixture")
        }
        try plan.withUnsafeBytes { bytes in
            let buffer = MuuBuffer(data: claimedLength == nil ? UnsafeMutablePointer(mutating: bytes.bindMemory(to: UInt8.self).baseAddress) : nil,
                len: claimedLength ?? bytes.count)
            try withPreparedPlan(buffer, format: format, cancel: cancel) { _ in try body() }
        }
    }
    #endif

    private static func importedRead(_ function: (UnsafeMutablePointer<MuuBuffer>?) -> Int32, limit: Int) throws -> Data {
        var output = MuuBuffer(data: nil, len: 0)
        let status = function(&output)
        defer { muu_buffer_free(output) }
        observeStatus(status)
        guard status == MUU_RESULT_OK else { throw importedError(status, output) }
        guard output.len <= limit else { throw ImportedDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "catalog_too_large") }
        return try copy(output)
    }
    private static func importedStep(_ function: (UnsafePointer<UInt8>?, Int, UnsafePointer<UInt8>?, Int,
        UnsafeMutablePointer<MuuBuffer>?, UnsafeMutablePointer<MuuBuffer>?) -> Int32,
        command: Data, file: Data) throws -> ImportedDictionaryCoreBridge.Reply {
        var generation = MuuBuffer(data: nil, len: 0)
        var plan = MuuBuffer(data: nil, len: 0)
        let status = command.withUnsafeBytes { commandBytes in
            file.withUnsafeBytes { fileBytes in
                function(commandBytes.bindMemory(to: UInt8.self).baseAddress, commandBytes.count,
                    fileBytes.bindMemory(to: UInt8.self).baseAddress, fileBytes.count, &generation, &plan)
            }
        }
        // Free BOTH outputs on every path. Copy/decode are private: on local
        // failure cancel the reservation before the store can start disk IO.
        defer { muu_buffer_free(generation); muu_buffer_free(plan) }
        observeStatus(status)
        guard status == MUU_RESULT_OK else { throw importedError(status, plan) }
        return try withPreparedPlan(plan, format: .imported, cancel: { acknowledge(muu_core_imported_dictionary_cancel, $0) }) { planBytes in
                _ = try ImportedDictionaryStep.decode(planBytes)
                guard generation.len <= ImportedDictionaryStore.maxCatalogBytes else { throw ImportedDictionaryOperationError(status: MUU_RESULT_INTERNAL, code: "catalog_too_large") }
                // A request/blob stage has no generation yet. The plan remains
                // nonempty; the shared copy helper rejects empty results.
                return (generation.len == 0 ? Data() : try copy(generation), planBytes)
            }
    }
    // Every native cancel result passes here once: from a store's cancel
    // closure, or from withPreparedPlan's local-failure path, never both for
    // one ticket. A failure is logged as a fixed code and the numeric status
    // only; the ticket and plan are opaque and never logged.
    private static func cancelAcknowledgement(_ status: Int32) -> Int32 {
        if status != MUU_RESULT_OK {
            logger.error("Dictionary cancel failed; code=dictionary_cancel_failed status=\(status, privacy: .public)")
            #if MUU_TEST
            cancellationFailureForTesting?("dictionary_cancel_failed", status)
            #endif
        }
        return status
    }
    // The one ACK call for all three stores: pass the borrowed command, observe
    // the result once (so CORE_INVALID marks the Core invalid), return it.
    private static func acknowledge(_ function: (UnsafePointer<UInt8>?, Int) -> Int32, _ command: Data) -> Int32 {
        let status = command.withUnsafeBytes { function($0.bindMemory(to: UInt8.self).baseAddress, $0.count) }
        observeStatus(status)
        return status
    }
    private static func importedError(_ status: Int32, _ output: MuuBuffer) -> Error {
        decodeFailure(output, limit: ImportedDictionaryStore.maxCatalogBytes, payload: ImportedFailure.self) {
            ImportedDictionaryOperationError(status: status, code: $0?.code ?? "imported_dictionary_operation_failed",
                line: $0?.params?.line, field: $0?.params?.field)
        }
    }
    static var postalCodeBridge: PostalCodeCoreBridge {
        PostalCodeCoreBridge(view: {
            var out = MuuBuffer(data: nil, len: 0)
            let status = muu_core_postal_code_view(&out)
            defer { muu_buffer_free(out) }; observeStatus(status)
            guard status == MUU_RESULT_OK else { throw postalError(status, out) }
            guard out.len <= PostalCodeStore.maxPlanBytes else { throw PostalCodeOperationError(status: MUU_RESULT_INTERNAL, code: "invalid_plan") }
            return try copy(out)
        }, loadBegin: { c, b in try postalStep(muu_core_postal_code_load_begin, command: c, file: b) },
           loadSupply: { c, b in try postalStep(muu_core_postal_code_load_supply, command: c, file: b) },
           prepare: { c, b in try postalStep(muu_core_postal_code_prepare, command: c, file: b) },
           publish: { acknowledge(muu_core_postal_code_publish, $0) },
           rollback: { acknowledge(muu_core_postal_code_rollback, $0) },
           disable: { acknowledge(muu_core_postal_code_disable, $0) },
           finish: { acknowledge(muu_core_postal_code_finish, $0) },
           cancel: { cancelAcknowledgement(acknowledge(muu_core_postal_code_cancel, $0)) })
    }
    private static func postalStep(_ function: (UnsafePointer<UInt8>?, Int, UnsafePointer<UInt8>?, Int, UnsafeMutablePointer<MuuBuffer>?, UnsafeMutablePointer<MuuBuffer>?, UnsafeMutablePointer<MuuBuffer>?) -> Int32, command: Data, file: Data) throws -> PostalCodeCoreBridge.Reply {
        var tsv = MuuBuffer(data: nil, len: 0); var manifest = MuuBuffer(data: nil, len: 0); var plan = MuuBuffer(data: nil, len: 0)
        let status = command.withUnsafeBytes { c in file.withUnsafeBytes { b in
            function(c.bindMemory(to: UInt8.self).baseAddress, c.count, b.bindMemory(to: UInt8.self).baseAddress, b.count, &tsv, &manifest, &plan)
        } }
        // Free all three on every outcome, including an empty step or a failed
        // copy. Get the ticket before decoding the remainder, so private local
        // validation failure can release Core's reservation before any save.
        defer { muu_buffer_free(tsv); muu_buffer_free(manifest); muu_buffer_free(plan) }
        observeStatus(status)
        guard status == MUU_RESULT_OK else { throw postalError(status, plan) }
        return try withPreparedPlan(plan, format: .postal, cancel: { acknowledge(muu_core_postal_code_cancel, $0) }) { planData in
                _ = try PostalCodeStep.decode(planData)
                guard tsv.len <= PostalCodeStore.maxSnapshotBytes, manifest.len <= PostalCodeStore.maxManifestBytes else { throw PostalCodeOperationError(status: MUU_RESULT_INTERNAL, code: "file_too_large") }
                return (tsv.len == 0 ? Data() : try copy(tsv), manifest.len == 0 ? Data() : try copy(manifest), planData)
            }
    }
    private static func postalError(_ status: Int32, _ output: MuuBuffer) -> Error {
        decodeFailure(output, limit: PostalCodeStore.maxPlanBytes, payload: CodeFailure.self) {
            PostalCodeOperationError(status: status, code: $0?.code ?? "core_operation_failed")
        }
    }

    static func clearPersonalFrequency() -> Result<LearningVersion, Error> {
        var output = MuuBuffer(data: nil, len: 0)
        let status = muu_core_clear_personal_frequency(&output)
        defer { muu_buffer_free(output) }
        observeStatus(status)
        guard status == MUU_RESULT_OK else { return .failure(error(status, output)) }
        do {
            return .success(try JSONDecoder().decode(LearningVersion.self, from: copy(output)))
        } catch { return .failure(error) }
    }
    static func personalFrequency(after received: LearningVersion?) -> Result<
        (Data, LearningVersion)?, Error
    > {
        do {
            let bytes = try JSONEncoder().encode(received)
            var data = MuuBuffer(data: nil, len: 0)
            var version = MuuBuffer(data: nil, len: 0)
            let status = bytes.withUnsafeBytes { buffer in
                muu_core_personal_frequency_snapshot(
                    buffer.bindMemory(to: UInt8.self).baseAddress, buffer.count, &data, &version)
            }
            defer {
                muu_buffer_free(data)
                muu_buffer_free(version)
            }
            observeStatus(status)
            guard status == MUU_RESULT_OK else {
                return .failure(CoreOperationError(status: status, code: "learning_query_failed"))
            }
            // Rust fills both buffers or neither; one without the other
            // fails in `copy` below.
            guard data.data != nil || version.data != nil else { return .success(nil) }
            let copiedData = try copy(data)
            let copiedVersion = try JSONDecoder().decode(LearningVersion.self, from: copy(version))
            return .success((copiedData, copiedVersion))
        } catch { return .failure(error) }
    }
    // An empty buffer is an error here. Outputs that may legitimately be
    // empty check `len` before calling this.
    private static func copy(_ output: MuuBuffer) throws -> Data {
        guard let bytes = output.data, output.len > 0 else {
            throw CoreOperationError(status: Int32(MUU_RESULT_INTERNAL), code: "empty_result")
        }
        return Data(bytes: bytes, count: output.len)
    }
    private static func error(_ status: Int32, _ output: MuuBuffer) -> Error {
        decodeFailure(output, limit: 16 * 1024 * 1024, payload: CodeFailure.self) {
            CoreOperationError(status: status, code: $0?.code ?? "core_operation_failed")
        }
    }
    /// Evaluated once at launch. The dictionary path, the language-model
    /// directory and whether recording is enabled identify the core for the
    /// rest of the process; changing the corresponding preference takes effect
    /// at the next launch.
    static func defaultConfiguration(userDictionaryFileState: FileSafety.StoredFileState) -> CoreConfiguration {
        configuration(userDictionaryFileState: userDictionaryFileState,
            boolSetting: { UserDefaults.standard.bool(forKey: $0) },
            stringSetting: { UserDefaults.standard.string(forKey: $0) },
            support: { FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first },
            dictionary: { Bundle.main.url(forResource: "mozc-oss", withExtension: "muudict")?.path },
            recorder: { CommitStreamRecorder.shared.makeEngineConfiguration() })
    }

    // Keep both production and the isolated startup test on one policy. Lazy
    // getters preserve the order of preference/resource reads at launch.
    //
    // Each read is a closure called at the point where the old inline body
    // made it: Enabled, Model, Application Support, then the bundled
    // dictionary, then CommitStreamRecorder.shared. So the product reads the
    // same sources in the same order as before, and touches the recorder
    // singleton at the same moment. No read is skipped today (Application
    // Support is read even when the model is off); calling the closures here,
    // rather than taking their values, keeps any later short-circuit in this
    // body in effect for every source. The store test asserts this order.
    private static func configuration(userDictionaryFileState: FileSafety.StoredFileState,
        boolSetting: (String) -> Bool, stringSetting: (String) -> String?, support: () -> URL?,
        dictionary: () -> String?, recorder: () -> CommitStreamEngineConfiguration?) -> CoreConfiguration {
        // Keep the existing opt-in gate: an unset Enabled preference is false.
        // Only the new model preference defaults to zenz. No preferences are written.
        let lm = ConversionLmConfiguration.resolve(
            enabled: boolSetting("MuuConversionLmEnabled"),
            modelSetting: stringSetting("MuuConversionLmModel"), support: support())
        return CoreConfiguration(
            dictionaryPath: dictionary(),
            conversionLmDirectory: lm.directory,
            commitStream: recorder(),
            userDictionaryFileState: userDictionaryFileState.rawValue,
            conversionLmModel: lm.model, conversionLmFallbackDirectory: lm.fallbackDirectory)
    }

    // The injection entry exists only in MUU_TEST builds. A Release build
    // cannot build a configuration from substituted preferences, paths or
    // recorder, so production keeps the single defaultConfiguration path. The
    // release check refuses MUU_TEST in the build settings and any
    // `ForTesting` symbol in the executable.
    #if MUU_TEST
    static func configurationForTesting(userDictionaryFileState: FileSafety.StoredFileState,
        boolSetting: (String) -> Bool, stringSetting: (String) -> String?, support: () -> URL?,
        dictionary: () -> String?, recorder: () -> CommitStreamEngineConfiguration?) -> CoreConfiguration {
        configuration(userDictionaryFileState: userDictionaryFileState, boolSetting: boolSetting,
            stringSetting: stringSetting, support: support, dictionary: dictionary, recorder: recorder)
    }
    #endif

}

final class RustEngine {
    private let logger = Logger(subsystem: "dev.muu.inputmethod.Muu", category: "RustEngine")
    private var engineHandle: OpaquePointer?
    #if MUU_TEST
        static var recreateObserverForTesting: ((RustEngine) -> Void)?
        static var commandObserverForTesting: ((String) -> Void)?
        static var requestObserverForTesting: ((Data) -> Void)?
        static var recordObserverForTesting: ((Data) -> Void)?
        static var viewTransformForTesting: ((Data) -> Data)?
        static var forceCreateFailureSettingForTesting: ((String) -> Bool)?
        private var eventRecorderForTesting: CommitStreamRecorder?
    #endif
    var isAvailable: Bool { engineHandle != nil && !RustCore.isInvalid }

    init() { createSession() }
    deinit { destroy() }

    #if MUU_TEST
        convenience init(
            testingCommitStream: CommitStreamEngineConfiguration?, dictionaryPath: String,
            recorder: CommitStreamRecorder? = nil,
            personalFrequency: PersonalFrequencySnapshot = .empty,
            userDictionaryFile: Data? = nil,
            personalFrequencyStore: PersonalFrequencyStore = .shared
        ) {
            if !RustCore.initializedForTesting {
                let result = RustCore.initialize(
                    CoreConfiguration(
                        dictionaryPath: dictionaryPath, conversionLmDirectory: nil,
                        commitStream: testingCommitStream,
                        userDictionaryFileState: userDictionaryFile == nil ? "missing" : "present"),
                    learning: try? JSONEncoder().encode(personalFrequency),
                    userDictionaryFile: userDictionaryFile ?? Data())
                if case .success(let initialized) = result {
                    precondition(userDictionaryFile == nil || !initialized.degraded.userDictionaryInvalid,
                        "The synthetic startup file must be valid")
                    personalFrequencyStore.acceptStartup(initialized)
                }
            }
            self.init()
            eventRecorderForTesting = recorder
        }
    #endif

    func handle<T: Encodable>(_ request: T) -> EngineResponse {
        #if MUU_TEST
            if let command = request as? CommandRequest {
                Self.commandObserverForTesting?(command.command)
            }
        #endif
        guard !RustCore.isInvalid else {
            return EngineResponse(status: Int32(MUU_RESULT_CORE_INVALID), view: nil)
        }
        guard let engineHandle else {
            return EngineResponse(status: Int32(MUU_RESULT_NULL_HANDLE), view: nil)
        }
        // Records and learning belong to the core, not to this session. They
        // are collected here only because an event is the one thing that can
        // have produced them, and because this runs on main as the learning
        // store requires. The two early returns above skip this; every
        // status from the call below, failures included, still drains.
        defer {
            #if MUU_TEST
                drainCommitStreamEvents(to: eventRecorderForTesting ?? .shared)
            #else
                drainCommitStreamEvents()
            #endif
            drainPersonalFrequencySnapshot()
        }
        do {
            let bytes = try MuuEnvelope.encode(request)
            #if MUU_TEST
                Self.requestObserverForTesting?(bytes)
            #endif
            var output = MuuBuffer(data: nil, len: 0)
            let status = bytes.withUnsafeBytes { buffer in
                muu_session_handle_event(
                    engineHandle, buffer.bindMemory(to: UInt8.self).baseAddress, buffer.count,
                    &output)
            }
            defer { muu_buffer_free(output) }
            RustCore.observeStatus(status)
            #if MUU_TEST
                if let transform = Self.viewTransformForTesting, let pointer = output.data,
                    output.len >= 0, output.len <= MuuEnvelope.maximumViewBytes {
                    return try EngineResponse.decodeView(
                        transform(Data(bytes: pointer, count: output.len)), status: status)
                }
            #endif
            return try EngineResponse.decodeView(output, status: status)
        } catch {
            logger.error("FFI envelope failure; category=protocol")
            return EngineResponse(status: Int32(MUU_RESULT_DECODE_ERR), view: nil)
        }
    }
    func drainCommitStreamEvents(to recorder: CommitStreamRecorder = .shared) {
        guard !RustCore.isInvalid, recorder.canDrain else { return }
        recorder.connectCore { request in
            let status = request.withUnsafeBytes { raw in
                muu_core_report_commit_stream_dropped(raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
            }
            RustCore.observeStatus(status)
            return status
        }
        while recorder.canDrain {
            var output = MuuBuffer(data: nil, len: 0), metadata = MuuBuffer(data: nil, len: 0)
            let status = muu_core_take_commit_stream_event_with_weight(&output, &metadata)
            defer { muu_buffer_free(output); muu_buffer_free(metadata) }
            RustCore.observeStatus(status)
            guard status == MUU_RESULT_OK else { return }
            guard let bytes = output.data, output.len > 0 else { return }
            // Oversized or missing metadata becomes empty Data. The reporter
            // then marks the debt unreportable at once, with no retries.
            let weightBytes = metadata.len <= 1024 ? metadata.data.map { Data(bytes: $0, count: metadata.len) } ?? Data() : Data()
            let weight: UInt64
            do { weight = try CommitStreamDropWeight.decode(weightBytes) }
            catch {
                // Ownership has left Core. Return the exact small metadata,
                // never guess weight=1 or decode the record to recover debt.
                recorder.discardUnreadableMetadata(weightBytes)
                return
            }
            let data = Data(bytes: bytes, count: output.len)
            #if MUU_TEST
                Self.recordObserverForTesting?(data)
            #endif
            recorder.append(data, weight: weight)
        }
    }
    func drainPersonalFrequencySnapshot(to store: PersonalFrequencyStore = .shared) {
        guard !RustCore.isInvalid else { return }
        store.poll()
    }
    /// Recovery from a session-level failure only. With the core invalid a new
    /// session cannot be created, so the handle stays nil and keys pass
    /// through.
    func recreate() {
        destroy()
        guard !RustCore.isInvalid else { return }
        createSession()
        #if MUU_TEST
            Self.recreateObserverForTesting?(self)
        #endif
    }
    // `MuuForceCreateFailure` exercises the pass-through path (docs/src/internals/build-and-test.md「インストール」), but only
    // in dogfood and test builds. A standard build never reads it: any
    // same-user process could otherwise switch Muu to silent pass-through
    // with `defaults write`.
    private static func forceCreateFailureRequested() -> Bool {
        #if MUU_TEST
            if let setting = forceCreateFailureSettingForTesting {
                return MuuBuildCapabilities.forceCreateFailureRequested(setting: setting)
            }
        #endif
        return MuuBuildCapabilities.forceCreateFailureRequested {
            UserDefaults.standard.bool(forKey: $0)
        }
    }

    private func createSession() {
        guard !RustCore.isInvalid, !Self.forceCreateFailureRequested(),
            muu_abi_version() == MuuConstants.expectedABIVersion
        else { return }
        var error = MuuBuffer(data: nil, len: 0)
        let status = muu_session_create(&engineHandle, &error)
        defer { muu_buffer_free(error) }
        RustCore.observeStatus(status)
        if status != MUU_RESULT_OK {
            logger.error("Session create failed; result=\(status, privacy: .public)")
        }
    }
    private func destroy() {
        if let handle = engineHandle {
            muu_session_destroy(handle)
            engineHandle = nil
        }
    }
}
