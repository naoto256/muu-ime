import Foundation

// The gate: the 95th percentile of key events and of conversions must each
// stay strictly under 8 ms. Each sample is one full Swift round trip
// (encode, C ABI call, Rust, decode), as the input controller pays it.
private let handleEventBudgetMicroseconds: UInt64 = 8_000
private let warmupIterations =
    ProcessInfo.processInfo.environment["MUU_LATENCY_WARMUP"].flatMap(Int.init) ?? 20
private let measuredIterations =
    ProcessInfo.processInfo.environment["MUU_LATENCY_MEASURED"].flatMap(Int.init) ?? 200

@main
private enum HandleEventMeasurement {
    static func main() throws {
        guard CommandLine.arguments.count == 2 else {
            throw MeasurementError.usage
        }
        let configuration = try JSONEncoder().encode([
            "dictionary_path": CommandLine.arguments[1],
            "user_dictionary_file_state": "missing",
        ])
        // Initialize resources once, outside the timed Session round trips.
        // A separate process, no LM and no learning file keep this gate fixed.
        var report = MuuBuffer(data: nil, len: 0)
        let initializeStatus = configuration.withUnsafeBytes { bytes in
            muu_core_initialize_with_user_dictionary_file(bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count, nil, 0, false, nil, 0, &report)
        }
        muu_buffer_free(report)
        guard initializeStatus == MUU_RESULT_OK else {
            throw MeasurementError.engineCreationFailed(initializeStatus)
        }
        var engine: OpaquePointer?
        var errorBuffer = MuuBuffer(data: nil, len: 0)
        let createStatus = muu_session_create(&engine, &errorBuffer)
        muu_buffer_free(errorBuffer)
        guard createStatus == MUU_RESULT_OK, let engine else {
            throw MeasurementError.engineCreationFailed(createStatus)
        }
        defer { muu_session_destroy(engine) }
        guard warmupIterations >= 0, measuredIterations > 0 else {
            throw MeasurementError.usage
        }

        for _ in 0..<warmupIterations {
            try runComposition(engine: engine)
        }

        var keySamples: [UInt64] = []
        var conversionSamples: [UInt64] = []
        keySamples.reserveCapacity(measuredIterations * 7)
        conversionSamples.reserveCapacity(measuredIterations)
        for _ in 0..<measuredIterations {
            try runComposition(
                engine: engine,
                keySamples: &keySamples,
                conversionSamples: &conversionSamples
            )
        }

        let keyP95 = percentile95(keySamples)
        let conversionP95 = percentile95(conversionSamples)
        try writeRawSamples(
            keySamples: keySamples,
            conversionSamples: conversionSamples,
            keyP95: keyP95,
            conversionP95: conversionP95
        )
        print(
            "handle_event swift_key_p95_us=\(keyP95) "
                + "swift_conversion_p95_us=\(conversionP95) "
                + "budget_us=\(handleEventBudgetMicroseconds)"
        )
        guard keyP95 < handleEventBudgetMicroseconds,
              conversionP95 < handleEventBudgetMicroseconds
        else {
            throw MeasurementError.budgetExceeded
        }
    }

    // One fixed composition: cancel, type "nihongo" (seven key samples), Space
    // (one conversion sample), commit. The commit never selects a candidate
    // explicitly, so nothing is learned and every iteration sees the same
    // state.
    private static func runComposition(
        engine: OpaquePointer,
        keySamples: inout [UInt64],
        conversionSamples: inout [UInt64]
    ) throws {
        _ = try roundtrip(
            engine: engine,
            request: CommandRequest(command: "cancel")
        )
        for character in "nihongo" {
            let (_, elapsed) = try roundtrip(
                engine: engine,
                request: EventRequest(
                    key: KeyEvent(
                        eventType: "key_down",
                        keycode: 0,
                        modifiers: 0,
                        charactersUTF8: String(character),
                        charactersIgnoringModifiers: String(character),
                        isRepeat: false
                    )
                )
            )
            keySamples.append(elapsed)
        }
        let (converted, conversionElapsed) = try roundtrip(
            engine: engine,
            request: EventRequest(
                key: KeyEvent(
                    eventType: "key_down",
                    keycode: 49,
                    modifiers: 0,
                    charactersUTF8: " ",
                    charactersIgnoringModifiers: " ",
                    isRepeat: false
                )
            )
        )
        guard converted.candidates != nil else {
            throw MeasurementError.missingCandidates
        }
        conversionSamples.append(conversionElapsed)
        _ = try roundtrip(
            engine: engine,
            request: CommandRequest(command: "commit")
        )
    }

    private static func runComposition(engine: OpaquePointer) throws {
        var ignoredKeys: [UInt64] = []
        var ignoredConversions: [UInt64] = []
        try runComposition(
            engine: engine,
            keySamples: &ignoredKeys,
            conversionSamples: &ignoredConversions
        )
    }

    private static func roundtrip<Request: Encodable>(
        engine: OpaquePointer,
        request: Request
    ) throws -> (ViewState, UInt64) {
        let started = DispatchTime.now().uptimeNanoseconds
        let requestData = try MuuEnvelope.encode(request)
        var output = MuuBuffer(data: nil, len: 0)
        let status = requestData.withUnsafeBytes { rawBuffer in
            muu_session_handle_event(
                engine,
                rawBuffer.bindMemory(to: UInt8.self).baseAddress,
                rawBuffer.count,
                &output
            )
        }
        defer { muu_buffer_free(output) }
        guard status == MUU_RESULT_OK, let data = output.data else {
            throw MeasurementError.handleFailed(status)
        }
        let view = try MuuEnvelope.decodeView(Data(bytes: data, count: output.len))
        let elapsed = (DispatchTime.now().uptimeNanoseconds - started) / 1_000
        return (view, elapsed)
    }

    // Nearest-rank p95: element ceil(0.95 n) - 1 of the sorted samples. The
    // caller guarantees at least one measured iteration.
    private static func percentile95(_ samples: [UInt64]) -> UInt64 {
        let sorted = samples.sorted()
        let index = max(0, (sorted.count * 95 + 99) / 100 - 1)
        return sorted[index]
    }

    /// Writes a versioned, row-oriented timing authority when requested by
    /// the measurement harness.  The first row freezes the warmup/sample
    /// contract; every measured key/conversion value is retained in order and
    /// the final row is independently recomputable from those values.
    private static func writeRawSamples(
        keySamples: [UInt64],
        conversionSamples: [UInt64],
        keyP95: UInt64,
        conversionP95: UInt64
    ) throws {
        guard let rawPath = ProcessInfo.processInfo.environment["MUU_TIMING_RAW_PATH"] else {
            return
        }
        let warmup = ProcessInfo.processInfo.environment["MUU_LATENCY_WARMUP"].flatMap(Int.init) ?? 20
        let measured = ProcessInfo.processInfo.environment["MUU_LATENCY_MEASURED"].flatMap(Int.init) ?? 200
        var lines: [String] = []
        lines.append(try jsonLine([
            "schema": "muu.handle-event-timing.v1",
            "kind": "metadata",
            "warmup_iterations": warmup,
            "measured_iterations": measured,
            "key_values": keySamples.count,
            "conversion_values": conversionSamples.count,
            "p95_rule": "sorted[(N*95+99)/100-1]"
        ]))
        for (index, micros) in keySamples.enumerated() {
            lines.append(try jsonLine([
                "schema": "muu.handle-event-timing.v1",
                "kind": "sample",
                "phase": "key",
                "index": index,
                "microseconds": micros
            ]))
        }
        for (index, micros) in conversionSamples.enumerated() {
            lines.append(try jsonLine([
                "schema": "muu.handle-event-timing.v1",
                "kind": "sample",
                "phase": "conversion",
                "index": index,
                "microseconds": micros
            ]))
        }
        lines.append(try jsonLine([
            "schema": "muu.handle-event-timing.v1",
            "kind": "summary",
            "key_p95_microseconds": keyP95,
            "conversion_p95_microseconds": conversionP95,
            "key_values": keySamples.count,
            "conversion_values": conversionSamples.count
        ]))
        let data = Data((lines.joined(separator: "\n") + "\n").utf8)
        try data.write(to: URL(fileURLWithPath: rawPath), options: .atomic)
    }

    private static func jsonLine(_ value: [String: Any]) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        return String(decoding: data, as: UTF8.self)
    }
}

private enum MeasurementError: Error {
    case usage
    case engineCreationFailed(Int32)
    case handleFailed(Int32)
    case missingCandidates
    case budgetExceeded
}
