// Audits an installed Muu against the artifact that was reviewed, through
// public interfaces only (Info.plist, codesign, TIS, ps, lsof). Subcommands:
// - signature: print the codesign identity, flags and CDHash of an app;
// - verify-artifact: a built app matches its reviewed commit, build number,
//   variant, executable SHA-256 and code signature, and passes strict codesign;
// - audit fresh-prelaunch|runtime: the installed bundle, its two public TIS
//   modes, their icons and (at runtime) the one running process all match the
//   reviewed artifact and the committed icon assets;
// - snapshot: record the raw facts without judging them;
// - ui-consumers / verify-login-generation: capture the system UI processes
//   that cache input-source icons, and prove they restarted across a login.
// Output is JSON on stdout; any failure exits 1 with the reason on stderr.

import AppKit
import Carbon
import CryptoKit
import Foundation

private struct ExpectedMode {
    let identifier: String
    let suffix: String
    let glyph: String
    let localizedName: String
    let isDefault: Bool
}

private let expectedModes = [
    ExpectedMode(
        identifier: "com.apple.inputmethod.Japanese.Hiragana",
        suffix: "Hiragana",
        glyph: "む",
        localizedName: "Muu IME ひらがな",
        isDefault: true
    ),
    ExpectedMode(
        identifier: "com.apple.inputmethod.Japanese.Roman",
        suffix: "Roman",
        glyph: "M",
        localizedName: "Muu IME 英数",
        isDefault: false
    ),
]

// Modes older builds registered. After migration they may still be listed
// by TIS, but must be neither enabled nor selectable.
private let removedPublicModes = [
    (
        identifier: "com.apple.inputmethod.Japanese.Katakana",
        suffix: "Katakana"
    ),
    (
        identifier: "com.apple.inputmethod.Japanese.HalfWidthKana",
        suffix: "HalfWidthKana"
    ),
    (
        identifier: "com.apple.inputmethod.Japanese.FullWidthRoman",
        suffix: "FullWidthRoman"
    ),
]

private var expectedAssetNames: [String] {
    expectedModes.flatMap { mode in
        [
            "MuuMode\(mode.suffix).tiff",
            "MuuMode\(mode.suffix)Selected.tiff",
            "MuuMode\(mode.suffix)Button.tiff",
        ]
    } + [
        "MuuInputMethod.tiff",
        "MuuIME.icns",
    ]
}

private let expectedJapaneseInfoPlistStrings = [
    "CFBundleDisplayName": "Muu IME",
    "CFBundleName": "Muu IME",
    "com.apple.inputmethod.Japanese.Hiragana": "Muu IME ひらがな",
    "com.apple.inputmethod.Japanese.Roman": "Muu IME 英数",
]

private enum AuditFailure: Error, CustomStringConvertible {
    case failed(String)
    case usage

    var description: String {
        switch self {
        case let .failed(message):
            message
        case .usage:
            """
            usage:
              audit-installed-mode-icons --self-test
              audit-installed-mode-icons signature <app>
              audit-installed-mode-icons snapshot <bundle-id> <installed-app>
              audit-installed-mode-icons ui-consumers <success-json> <partial-json> <error>
              audit-installed-mode-icons verify-login-generation <pre-login-ui-consumers-json> <success-json> <partial-json> <error>
              audit-installed-mode-icons verify-artifact <app> <expected-commit> <expected-build-number> <expected-build-variant> <expected-executable-sha256> <expected-codesign-json>
              audit-installed-mode-icons audit <fresh-prelaunch|runtime> <bundle-id> <installed-app> <source-assets> <expected-commit> <expected-build-number> <expected-build-variant> <expected-executable-sha256> <expected-codesign-json>
            """
        }
    }
}

private enum AuditMode: String, Codable {
    case freshPrelaunch = "fresh-prelaunch"
    case runtime
}

private struct ModeObservation {
    let inputModeID: String
    let inputSourceID: String
    let localizedName: String
    let iconURL: URL?
    let iconRefAvailable: Bool
    let enabled: Bool
    let selectCapable: Bool
}

private struct CurrentSourceReport: Codable {
    let inputSourceID: String?
    let inputModeID: String?
    let bundleIdentifier: String?
    let localizedName: String?
}

private struct ProcessReport: Codable {
    let processIdentifier: Int32
    let executablePath: String?
    let executableSHA256: String?
    let executableDeviceID: UInt64?
    let executableInode: UInt64?
    let mappedExecutablePath: String?
    let mappedExecutableInode: UInt64?
}

private struct ExpectedUIConsumer {
    let name: String
    let executablePath: String
}

// System processes that show input-source icons and cache them. A new icon
// is only trustworthy on screen after each of these has been relaunched, which
// a logout/login does.
private let expectedUIConsumers = [
    ExpectedUIConsumer(
        name: "System Settings",
        executablePath: "/System/Applications/System Settings.app/Contents/MacOS/System Settings"
    ),
    ExpectedUIConsumer(
        name: "TextInputMenuAgent",
        executablePath:
            "/System/Library/CoreServices/TextInputMenuAgent.app/Contents/MacOS/TextInputMenuAgent"
    ),
    ExpectedUIConsumer(
        name: "SystemUIServer",
        executablePath:
            "/System/Library/CoreServices/SystemUIServer.app/Contents/MacOS/SystemUIServer"
    ),
    ExpectedUIConsumer(
        name: "ControlCenter",
        executablePath:
            "/System/Library/CoreServices/ControlCenter.app/Contents/MacOS/ControlCenter"
    ),
]

private var expectedUIConsumerNames: [String] {
    expectedUIConsumers.map(\.name)
}

private struct UIConsumerReport: Codable, Equatable {
    let name: String
    let processIdentifier: Int32
    let launchTime: String
    let launchTimeEpochSeconds: TimeInterval
    let executablePath: String
}

private struct UILoginGenerationReport: Codable {
    let before: [UIConsumerReport]
    let after: [UIConsumerReport]
}

private struct UIConsumerEvidenceRow: Codable, Equatable {
    let filteredRowIndex: Int
    let name: String
    let fieldCount: Int
    let processIdentifier: Int32?
    let launchTime: String?
    let launchTimeEpochSeconds: TimeInterval?
    let executablePath: String?
    let observedPathSHA256: String?
    let issue: String?
}

private struct UIConsumerPartialEvidence: Codable, Equatable {
    let collectorExitStatus: Int32
    let collectorOutputWasUTF8: Bool
    let expectedConsumerNames: [String]
    let rows: [UIConsumerEvidenceRow]
}

private struct UIConsumerProcessCollection {
    let exitStatus: Int32
    let output: String?
}

private struct UIConsumerCapturePaths {
    let success: URL
    let partial: URL
    let error: URL
}

private struct CodeSignatureReport: Codable, Equatable {
    let identifier: String
    let flags: String
    let cdHash: String
    let signatureIdentity: String
}

private struct ModeReport: Codable {
    let index: Int
    let inputModeID: String
    let inputSourceID: String
    let localizedName: String
    let iconFile: String
    let iconSHA256: String
    let iconRefAvailable: Bool
    let enabled: Bool
    let selectCapable: Bool
}

private struct AuditReport: Codable {
    let auditMode: AuditMode
    let installedAppPath: String
    let bundleIdentifier: String
    let bundleVersion: String
    let shortVersion: String
    let sourceCommit: String
    let buildVariant: String
    let compiledPlistSHA256: String
    let expectedExecutablePath: String
    let expectedExecutableSHA256: String
    let expectedCodeSignature: CodeSignatureReport
    let installedExecutablePath: String
    let installedExecutableSHA256: String
    let installedExecutableDeviceID: UInt64
    let installedExecutableInode: UInt64
    let installedCodeSignature: CodeSignatureReport
    let assetSHA256: [String: String]
    let orderedModeIDs: [String]
    let defaultModeID: String
    let modes: [ModeReport]
    let currentSource: CurrentSourceReport
    let processCount: Int
    let processes: [ProcessReport]
    let environmentBlockers: [String]
}

private struct ArtifactAuthorityReport: Codable {
    let appPath: String
    let bundleIdentifier: String
    let sourceCommit: String
    let buildNumber: String
    let buildVariant: String
    let executablePath: String
    let executableSHA256: String
    let codeSignature: CodeSignatureReport
}

private struct RawModeReport: Codable {
    let inputModeID: String
    let inputSourceID: String
    let localizedName: String
    let iconURLPath: String?
    let iconURLReachable: Bool
    let iconSHA256: String?
    let iconRefAvailable: Bool
    let enabled: Bool
    let selectCapable: Bool
}

private struct RawSnapshot: Codable {
    let installedAppPath: String
    let bundleIdentifier: String?
    let bundleVersion: String?
    let shortVersion: String?
    let sourceCommit: String?
    let buildVariant: String?
    let compiledPlistSHA256: String
    let installedExecutablePath: String?
    let installedExecutableSHA256: String?
    let installedExecutableDeviceID: UInt64?
    let installedExecutableInode: UInt64?
    let installedCodeSignature: CodeSignatureReport?
    let modes: [RawModeReport]
    let currentSource: CurrentSourceReport
    let processCount: Int
    let processes: [ProcessReport]
}

private func require(_ condition: @autoclosure () -> Bool, _ message: String) throws {
    guard condition() else {
        throw AuditFailure.failed(message)
    }
}

private func dictionary(_ value: Any?, named name: String) throws -> [String: Any] {
    guard let value = value as? [String: Any] else {
        throw AuditFailure.failed("\(name) is not a dictionary")
    }
    return value
}

private func sha256(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

private func loadPlist(at url: URL) throws -> [String: Any] {
    try dictionary(
        PropertyListSerialization.propertyList(
            from: Data(contentsOf: url),
            options: [],
            format: nil
        ),
        named: url.lastPathComponent
    )
}

private func requireJapaneseLocalization(
    installed: URL,
    source: URL
) throws {
    let strings = try loadPlist(at: installed)
    let sourceStrings = try loadPlist(at: source)
    try require(
        strings as NSDictionary == expectedJapaneseInfoPlistStrings as NSDictionary
            && sourceStrings as NSDictionary
                == expectedJapaneseInfoPlistStrings as NSDictionary,
        "installed Japanese display-name metadata changed"
    )
}

private struct FileIdentity {
    let path: String
    let sha256: String
    let deviceID: UInt64
    let inode: UInt64
}

private func canonicalPath(_ url: URL) -> String {
    url.resolvingSymlinksInPath().standardizedFileURL.path
}

private func fileIdentity(at url: URL) throws -> FileIdentity {
    let path = canonicalPath(url)
    let attributes = try FileManager.default.attributesOfItem(atPath: path)
    guard let deviceID = (attributes[.systemNumber] as? NSNumber)?.uint64Value,
          let inode = (attributes[.systemFileNumber] as? NSNumber)?.uint64Value
    else {
        throw AuditFailure.failed("file identity is unavailable: \(path)")
    }
    return FileIdentity(
        path: path,
        sha256: sha256(try Data(contentsOf: URL(fileURLWithPath: path))),
        deviceID: deviceID,
        inode: inode
    )
}

private func parseCodeSignature(_ output: String) throws -> CodeSignatureReport {
    let lines = output.split(whereSeparator: \.isNewline).map(String.init)
    guard let identifier = lines
        .first(where: { $0.hasPrefix("Identifier=") })?
        .dropFirst("Identifier=".count),
        let codeDirectory = lines.first(where: { $0.hasPrefix("CodeDirectory ") }),
        let flags = codeDirectory
        .split(separator: " ")
        .first(where: { $0.hasPrefix("flags=") })?
        .dropFirst("flags=".count),
        let cdHash = lines
        .first(where: { $0.hasPrefix("CDHash=") })?
        .dropFirst("CDHash=".count)
    else {
        throw AuditFailure.failed("codesign evidence is incomplete")
    }
    let authorities = lines
        .filter { $0.hasPrefix("Authority=") }
        .map { String($0.dropFirst("Authority=".count)) }
    let signature = lines
        .first(where: { $0.hasPrefix("Signature=") })?
        .dropFirst("Signature=".count)
    let signatureIdentity = authorities.isEmpty
        ? signature.map(String.init)
        : authorities.joined(separator: " > ")
    guard let signatureIdentity, !signatureIdentity.isEmpty else {
        throw AuditFailure.failed("codesign identity is unavailable")
    }
    return CodeSignatureReport(
        identifier: String(identifier),
        flags: String(flags),
        cdHash: String(cdHash),
        signatureIdentity: signatureIdentity
    )
}

// `launch` starts the configured process. The self-test passes a launcher
// that swaps in a fake tool with large output; production uses `run()`.
// stdout and stderr share one pipe here, so draining that one handle before
// waiting is enough.
private func codeSignatureReport(
    app: URL, launch: (Process) throws -> Void = { try $0.run() }
) throws -> CodeSignatureReport {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
    process.arguments = ["-d", "--verbose=4", app.path]
    let output = Pipe()
    process.standardOutput = output
    process.standardError = output
    try launch(process)
    // Drain while the child can still be writing: waiting first can fill
    // the pipe and leave both processes waiting for each other.
    let data = output.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        throw AuditFailure.failed("codesign inspection failed")
    }
    return try parseCodeSignature(String(decoding: data, as: UTF8.self))
}

private func verifyStrictCodeSignature(
    app: URL, launch: (Process) throws -> Void = { try $0.run() }
) throws {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
    process.arguments = ["--verify", "--deep", "--strict", "--verbose=2", app.path]
    // Only the exit status matters. Both streams go to the null device, so
    // no pipe exists that could fill while this waits.
    process.standardOutput = FileHandle.nullDevice
    process.standardError = FileHandle.nullDevice
    try launch(process)
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        throw AuditFailure.failed("strict/deep code-sign verification failed")
    }
}

// The reviewed authority is the tuple (commit, build number, variant,
// executable SHA-256, code signature) recorded when the build was reviewed.
// Each must match the artifact exactly; the signature must also use the
// hardened runtime and name the bundle's own identifier.
private func validateArtifactAuthority(
    app: URL,
    expectedCommit: String,
    expectedBuildNumber: String,
    expectedBuildVariant: String,
    expectedExecutableSHA256: String,
    expectedCodeSignature: CodeSignatureReport,
    actualCodeSignature: CodeSignatureReport
) throws -> ArtifactAuthorityReport {
    let contents = app.appendingPathComponent("Contents", isDirectory: true)
    let plist = try loadPlist(at: contents.appendingPathComponent("Info.plist"))
    guard let bundleIdentifier = plist["CFBundleIdentifier"] as? String,
          let sourceCommit = plist["MuuSourceCommit"] as? String,
          let buildNumber = plist["CFBundleVersion"] as? String,
          let buildVariant = plist["MuuBuildVariant"] as? String,
          let executableName = plist["CFBundleExecutable"] as? String
    else {
        throw AuditFailure.failed("artifact build provenance is incomplete")
    }
    try require(sourceCommit == expectedCommit, "artifact source commit changed")
    try require(buildNumber == expectedBuildNumber, "artifact build number changed")
    try require(
        buildVariant == expectedBuildVariant,
        "artifact build variant changed"
    )
    // Both reviewed variants are known, but only the one the caller expected
    // passes the check above; dogfood is never accepted in standard's place.
    try require(
        ["standard", "dogfood"].contains(buildVariant),
        "artifact build variant is unknown"
    )
    let executableURL = contents
        .appendingPathComponent("MacOS", isDirectory: true)
        .appendingPathComponent(executableName)
    let executable = try fileIdentity(at: executableURL)
    try require(
        expectedExecutableSHA256.count == 64
            && expectedExecutableSHA256.allSatisfy(\.isHexDigit),
        "reviewed executable SHA-256 is malformed"
    )
    try require(
        executable.sha256 == expectedExecutableSHA256,
        "artifact executable differs from reviewed SHA-256"
    )
    try require(
        expectedCodeSignature.identifier == bundleIdentifier,
        "reviewed code-sign identifier differs from artifact bundle identifier"
    )
    try require(
        expectedCodeSignature.flags.contains("runtime"),
        "reviewed code-sign flags do not include hardened runtime"
    )
    try require(
        actualCodeSignature == expectedCodeSignature,
        "artifact code-sign evidence differs from reviewed authority"
    )
    return ArtifactAuthorityReport(
        appPath: canonicalPath(app),
        bundleIdentifier: bundleIdentifier,
        sourceCommit: sourceCommit,
        buildNumber: buildNumber,
        buildVariant: buildVariant,
        executablePath: executable.path,
        executableSHA256: executable.sha256,
        codeSignature: actualCodeSignature
    )
}

private struct LsofTextMapping {
    let path: String
    let inode: UInt64?
}

// The executable files a process has mapped, from `lsof -F0`, so the audit can
// tell which inode is actually running. Any lsof failure yields no mappings,
// which the caller then rejects. Drain stdout before waiting for exit;
// stderr is unused and goes to the null device.
private func lsofTextMappings(
    processIdentifier: Int32, launch: (Process) throws -> Void = { try $0.run() }
) -> [LsofTextMapping] {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
    process.arguments = [
        "-a",
        "-p",
        String(processIdentifier),
        "-d",
        "txt",
        "-F0fin",
    ]
    let standardOutput = Pipe()
    process.standardOutput = standardOutput
    process.standardError = FileHandle.nullDevice
    let output: Data
    do {
        try launch(process)
        output = standardOutput.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
    } catch {
        return []
    }
    guard process.terminationStatus == 0 else {
        return []
    }

    var mappings: [LsofTextMapping] = []
    var readingTextRecord = false
    var currentInode: UInt64?
    var currentPath: String?
    func appendCurrent() {
        guard readingTextRecord, let currentPath else {
            return
        }
        mappings.append(
            LsofTextMapping(
                path: canonicalPath(URL(fileURLWithPath: currentPath)),
                inode: currentInode
            )
        )
    }
    for fieldData in output.split(separator: 0) {
        let field = String(decoding: fieldData, as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if field == "ftxt" {
            appendCurrent()
            readingTextRecord = true
            currentInode = nil
            currentPath = nil
        } else if readingTextRecord, field.first == "i" {
            currentInode = UInt64(field.dropFirst())
        } else if readingTextRecord, field.first == "n" {
            currentPath = String(field.dropFirst())
        }
    }
    appendCurrent()
    return mappings
}

// The installed-bundle contract: the reviewed authority as above; the fixed
// Info.plist mode dictionaries, order, default and labels; the Japanese
// localization; the eight icon assets byte-equal to the committed ones; the
// public TIS view (both modes present, enabled and selectable, icons inside
// the bundle and byte-equal; removed modes inactive); and for `runtime`
// exactly one running MuuIME whose mapped executable is the installed inode
// with the reviewed hash (`fresh-prelaunch` requires none running).
private func validateInstalledContract(
    app: URL,
    bundleIdentifier: String,
    sourceAssets: URL,
    expectedCommit: String,
    expectedBuildNumber: String,
    expectedBuildVariant: String,
    expectedExecutableSHA256: String,
    expectedCodeSignature: CodeSignatureReport,
    installedCodeSignature: CodeSignatureReport,
    auditMode: AuditMode,
    observations: [ModeObservation],
    currentSource: CurrentSourceReport,
    processes: [ProcessReport]
) throws -> AuditReport {
    let contents = app.appendingPathComponent("Contents", isDirectory: true)
    let resources = contents.appendingPathComponent("Resources", isDirectory: true)
    let infoURL = contents.appendingPathComponent("Info.plist")
    let infoData = try Data(contentsOf: infoURL)
    let plist = try loadPlist(at: infoURL)
    try requireJapaneseLocalization(
        installed: resources.appendingPathComponent("ja.lproj/InfoPlist.strings"),
        source: sourceAssets
            .deletingLastPathComponent()
            .appendingPathComponent("ja.lproj/InfoPlist.strings")
    )
    try require(
        plist["CFBundleIdentifier"] as? String == bundleIdentifier,
        "installed CFBundleIdentifier changed"
    )
    guard let bundleVersion = plist["CFBundleVersion"] as? String,
          let shortVersion = plist["CFBundleShortVersionString"] as? String,
          let sourceCommit = plist["MuuSourceCommit"] as? String,
          let buildVariant = plist["MuuBuildVariant"] as? String,
          let executableName = plist["CFBundleExecutable"] as? String
    else {
        throw AuditFailure.failed("installed build provenance is incomplete")
    }
    try require(
        bundleVersion == expectedBuildNumber,
        "installed CFBundleVersion does not match the reviewed build number"
    )
    try require(
        sourceCommit == expectedCommit,
        "installed source commit does not match the reviewed commit"
    )
    try require(
        buildVariant == expectedBuildVariant,
        "installed build variant does not match the reviewed artifact"
    )
    try require(
        Int(bundleVersion).map { $0 > 0 } == true,
        "installed CFBundleVersion is not a positive monotonic build number"
    )
    let executableURL = contents
        .appendingPathComponent("MacOS", isDirectory: true)
        .appendingPathComponent(executableName)
    let installedExecutable = try fileIdentity(at: executableURL)
    try require(
        expectedExecutableSHA256.count == 64
            && expectedExecutableSHA256.allSatisfy { $0.isHexDigit },
        "reviewed Release executable SHA-256 is malformed"
    )
    try require(
        installedExecutable.sha256 == expectedExecutableSHA256,
        "installed executable does not match the reviewed Release SHA-256"
    )
    try require(
        expectedCodeSignature.identifier == bundleIdentifier,
        "reviewed code-sign identifier does not match the bundle identifier"
    )
    try require(
        expectedCodeSignature.flags.contains("runtime"),
        "reviewed code-sign flags do not include hardened runtime"
    )
    try require(
        expectedCodeSignature.cdHash.count >= 40
            && expectedCodeSignature.cdHash.allSatisfy { $0.isHexDigit },
        "reviewed code-sign CDHash is malformed"
    )
    try require(
        installedCodeSignature == expectedCodeSignature,
        "installed code-sign identity, flags, or CDHash differs from the reviewed artifact"
    )

    switch auditMode {
    case .freshPrelaunch:
        try require(
            processes.isEmpty,
            "fresh-prelaunch audit requires zero running MuuIME processes"
        )
    case .runtime:
        try require(
            processes.count == 1,
            "runtime audit requires exactly one running MuuIME process"
        )
        let process = processes[0]
        try require(
            process.executablePath == installedExecutable.path,
            "running NSRunningApplication executable path differs from the installed executable"
        )
        try require(
            process.executableSHA256 == expectedExecutableSHA256
                && process.executableSHA256 == installedExecutable.sha256,
            "running executable SHA-256 differs from the reviewed installed executable"
        )
        try require(
            process.executableDeviceID == installedExecutable.deviceID
                && process.executableInode == installedExecutable.inode,
            "running executable path identity differs from the installed executable"
        )
        try require(
            process.mappedExecutablePath == installedExecutable.path,
            "running PID text mapping path differs from the installed executable"
        )
        try require(
            process.mappedExecutableInode == installedExecutable.inode,
            "running PID text mapping inode differs from the installed executable"
        )
    }

    let sourceAssetSet = try Set(
        FileManager.default.contentsOfDirectory(atPath: sourceAssets.path)
    )
    try require(
        sourceAssetSet == Set(expectedAssetNames),
        "reviewed source icon directory does not contain exactly 8 authoritative assets"
    )
    var assetSHA256: [String: String] = [:]
    for asset in expectedAssetNames {
        let sourceData = try Data(contentsOf: sourceAssets.appendingPathComponent(asset))
        let installedData = try Data(contentsOf: resources.appendingPathComponent(asset))
        try require(
            sourceData == installedData,
            "\(asset) differs between reviewed source and installed bundle"
        )
        assetSHA256[asset] = sha256(installedData)
    }
    for mode in expectedModes {
        let menu = try Data(
            contentsOf: resources.appendingPathComponent("MuuMode\(mode.suffix).tiff")
        )
        let alternate = try Data(
            contentsOf: resources.appendingPathComponent(
                "MuuMode\(mode.suffix)Selected.tiff"
            )
        )
        let palette = try Data(
            contentsOf: resources.appendingPathComponent(
                "MuuMode\(mode.suffix)Button.tiff"
            )
        )
        try require(
            menu == alternate && alternate == palette,
            "\(mode.suffix) installed mode surfaces are not one template mask"
        )
    }
    try require(
        plist["TISIconIsTemplate"] == nil,
        "installed component must not declare top-level TISIconIsTemplate"
    )

    let component = try dictionary(plist["ComponentInputModeDict"], named: "ComponentInputModeDict")
    let modeDefinitions = try dictionary(
        component["tsInputModeListKey"],
        named: "tsInputModeListKey"
    )
    let orderedModeIDs = component["tsVisibleInputModeOrderedArrayKey"] as? [String]
    try require(
        orderedModeIDs == expectedModes.map(\.identifier),
        "installed mode order changed"
    )
    try require(
        Set(modeDefinitions.keys) == Set(expectedModes.map(\.identifier)),
        "installed fixed mode dictionary keys changed"
    )
    let defaultModes = try expectedModes.filter { mode in
        let definition = try dictionary(modeDefinitions[mode.identifier], named: mode.identifier)
        let expectedKeys: Set<String> = [
            "TISIconIsTemplate",
            "TISIconLabels",
            "TISInputSourceID",
            "TISIntendedLanguage",
            "tsInputModeAlternateMenuIconFileKey",
            "tsInputModeDefaultStateKey",
            "tsInputModeIsVisibleKey",
            "tsInputModeMenuIconFileKey",
            "tsInputModePaletteIconFileKey",
            "tsInputModePrimaryInScriptKey",
            "tsInputModeScriptKey",
        ]
        try require(
            Set(definition.keys) == expectedKeys,
            "\(mode.suffix) installed metadata keys changed"
        )
        let actual = definition["tsInputModeDefaultStateKey"] as? Bool
        try require(actual == mode.isDefault, "\(mode.suffix) default state changed")
        try require(
            definition["TISInputSourceID"] as? String == "\(bundleIdentifier).\(mode.suffix)",
            "\(mode.suffix) installed TISInputSourceID mapping changed"
        )
        try require(
            definition["TISIconIsTemplate"] as? Bool == true,
            "\(mode.suffix) installed template authority changed"
        )
        let labels = try dictionary(
            definition["TISIconLabels"],
            named: "\(mode.suffix) installed TISIconLabels"
        )
        try require(
            Set(labels.keys) == Set(["Primary"])
                && labels["Primary"] as? String == mode.glyph,
            "\(mode.suffix) installed Primary label changed"
        )
        try require(
            definition["tsInputModeMenuIconFileKey"] as? String
                == "MuuMode\(mode.suffix).tiff",
            "\(mode.suffix) installed mode icon mapping changed"
        )
        try require(
            definition["tsInputModeAlternateMenuIconFileKey"] as? String
                == "MuuMode\(mode.suffix)Selected.tiff",
            "\(mode.suffix) installed alternate icon mapping changed"
        )
        try require(
            definition["tsInputModePaletteIconFileKey"] as? String
                == "MuuMode\(mode.suffix)Button.tiff",
            "\(mode.suffix) installed palette icon mapping changed"
        )
        try require(
            definition["TISIntendedLanguage"] as? String == "ja"
                && definition["tsInputModeIsVisibleKey"] as? Bool == true
                && definition["tsInputModePrimaryInScriptKey"] as? Bool == mode.isDefault
                && definition["tsInputModeScriptKey"] as? String == "smJapanese",
            "\(mode.suffix) installed public mode metadata changed"
        )
        return actual == true
    }
    try require(defaultModes.count == 1, "installed bundle must declare one default mode")

    var observationByMode: [String: ModeObservation] = [:]
    for observation in observations {
        try require(
            observationByMode[observation.inputModeID] == nil,
            "public TIS enumeration returned duplicate mode IDs"
        )
        observationByMode[observation.inputModeID] = observation
    }
    let expectedModeIDs = Set(expectedModes.map(\.identifier))
    let removedModeIDs = Set(removedPublicModes.map(\.identifier))
    try require(
        expectedModeIDs.isSubset(of: Set(observationByMode.keys)),
        "public TIS enumeration did not expose both supported modes"
    )
    try require(
        Set(observationByMode.keys).subtracting(expectedModeIDs.union(removedModeIDs)).isEmpty,
        "public TIS enumeration returned an unknown Muu mode"
    )

    var reports: [ModeReport] = []
    var blockers: [String] = []
    for removed in removedPublicModes {
        guard let observation = observationByMode[removed.identifier] else {
            continue
        }
        try require(
            observation.inputSourceID == "\(bundleIdentifier).\(removed.suffix)",
            "\(removed.suffix) legacy TISInputSourceID changed"
        )
        try require(
            !observation.enabled && !observation.selectCapable,
            "\(removed.suffix) legacy public source is still active after two-mode migration"
        )
    }
    for (index, mode) in expectedModes.enumerated() {
        guard let observation = observationByMode[mode.identifier] else {
            throw AuditFailure.failed("public TIS mode is missing: \(mode.identifier)")
        }
        try require(
            observation.inputSourceID == "\(bundleIdentifier).\(mode.suffix)",
            "\(mode.suffix) public TISInputSourceID changed"
        )
        try require(
            observation.localizedName == mode.localizedName,
            "\(mode.suffix) public localized name changed"
        )
        guard let iconURL = observation.iconURL else {
            throw AuditFailure.failed(
                "\(mode.suffix) public TISPropertyIconImageURL is absent"
                    + (observation.iconRefAvailable ? " (IconRef only)" : "")
            )
        }
        try require(
            (try? iconURL.checkResourceIsReachable()) == true,
            "\(mode.suffix) public icon URL is not reachable"
        )
        let installedResourceRoot = resources
            .resolvingSymlinksInPath()
            .standardizedFileURL
            .path
        let publicIconPath = iconURL
            .resolvingSymlinksInPath()
            .standardizedFileURL
            .path
        try require(
            publicIconPath.hasPrefix(installedResourceRoot + "/"),
            "\(mode.suffix) public icon URL is outside the installed bundle"
        )
        let expectedIconURL = resources.appendingPathComponent("MuuMode\(mode.suffix).tiff")
        let publicIconData = try Data(contentsOf: iconURL)
        let installedIconData = try Data(contentsOf: expectedIconURL)
        try require(
            publicIconData == installedIconData,
            "\(mode.suffix) public icon bytes differ from the installed authoritative asset"
        )
        if !observation.enabled || !observation.selectCapable {
            blockers.append(
                "\(mode.suffix): enabled=\(observation.enabled) selectCapable=\(observation.selectCapable)"
            )
        }
        reports.append(
            ModeReport(
                index: index,
                inputModeID: observation.inputModeID,
                inputSourceID: observation.inputSourceID,
                localizedName: observation.localizedName,
                iconFile: iconURL.lastPathComponent,
                iconSHA256: sha256(publicIconData),
                iconRefAvailable: observation.iconRefAvailable,
                enabled: observation.enabled,
                selectCapable: observation.selectCapable
            )
        )
    }

    return AuditReport(
        auditMode: auditMode,
        installedAppPath: app.resolvingSymlinksInPath().standardizedFileURL.path,
        bundleIdentifier: bundleIdentifier,
        bundleVersion: bundleVersion,
        shortVersion: shortVersion,
        sourceCommit: sourceCommit,
        buildVariant: buildVariant,
        compiledPlistSHA256: sha256(infoData),
        expectedExecutablePath: installedExecutable.path,
        expectedExecutableSHA256: expectedExecutableSHA256,
        expectedCodeSignature: expectedCodeSignature,
        installedExecutablePath: installedExecutable.path,
        installedExecutableSHA256: installedExecutable.sha256,
        installedExecutableDeviceID: installedExecutable.deviceID,
        installedExecutableInode: installedExecutable.inode,
        installedCodeSignature: installedCodeSignature,
        assetSHA256: assetSHA256,
        orderedModeIDs: orderedModeIDs ?? [],
        defaultModeID: defaultModes[0].identifier,
        modes: reports,
        currentSource: currentSource,
        processCount: processes.count,
        processes: processes,
        environmentBlockers: blockers
    )
}

private func tisObject(_ source: TISInputSource, _ key: CFString) -> AnyObject? {
    guard let pointer = TISGetInputSourceProperty(source, key) else {
        return nil
    }
    return Unmanaged<AnyObject>.fromOpaque(pointer).takeUnretainedValue()
}

private func tisString(_ source: TISInputSource, _ key: CFString) -> String? {
    tisObject(source, key) as? String
}

private func tisBool(_ source: TISInputSource, _ key: CFString) -> Bool {
    (tisObject(source, key) as? NSNumber)?.boolValue ?? false
}

private func observations(bundleIdentifier: String) throws -> [ModeObservation] {
    let filter = [kTISPropertyBundleID as String: bundleIdentifier] as CFDictionary
    guard let sourceList = TISCreateInputSourceList(filter, true) else {
        throw AuditFailure.failed("public TIS enumeration failed")
    }
    let sources = sourceList.takeRetainedValue() as! [TISInputSource]
    return sources.compactMap { source in
        guard let modeID = tisString(source, kTISPropertyInputModeID),
              let sourceID = tisString(source, kTISPropertyInputSourceID),
              let localizedName = tisString(source, kTISPropertyLocalizedName)
        else {
            return nil
        }
        let iconURL = tisObject(source, kTISPropertyIconImageURL) as? URL
        return ModeObservation(
            inputModeID: modeID,
            inputSourceID: sourceID,
            localizedName: localizedName,
            iconURL: iconURL,
            iconRefAvailable: TISGetInputSourceProperty(source, kTISPropertyIconRef) != nil,
            enabled: tisBool(source, kTISPropertyInputSourceIsEnabled),
            selectCapable: tisBool(source, kTISPropertyInputSourceIsSelectCapable)
        )
    }
}

private func currentSourceReport() -> CurrentSourceReport {
    guard let current = TISCopyCurrentKeyboardInputSource()?.takeRetainedValue() else {
        return CurrentSourceReport(
            inputSourceID: nil,
            inputModeID: nil,
            bundleIdentifier: nil,
            localizedName: nil
        )
    }
    return CurrentSourceReport(
        inputSourceID: tisString(current, kTISPropertyInputSourceID),
        inputModeID: tisString(current, kTISPropertyInputModeID),
        bundleIdentifier: tisString(current, kTISPropertyBundleID),
        localizedName: tisString(current, kTISPropertyLocalizedName)
    )
}

private func processReports(bundleIdentifier: String) -> [ProcessReport] {
    NSRunningApplication
        .runningApplications(withBundleIdentifier: bundleIdentifier)
        .map { application in
            let executableURL = application.executableURL
            let identity = executableURL.flatMap { try? fileIdentity(at: $0) }
            let mappings = lsofTextMappings(
                processIdentifier: application.processIdentifier
            )
            let executablePath = executableURL.map(canonicalPath)
            let mapping = executablePath.flatMap { path in
                mappings.first { $0.path == path }
            } ?? mappings.first
            return ProcessReport(
                processIdentifier: application.processIdentifier,
                executablePath: executablePath,
                executableSHA256: identity?.sha256,
                executableDeviceID: identity?.deviceID,
                executableInode: identity?.inode,
                mappedExecutablePath: mapping?.path,
                mappedExecutableInode: mapping?.inode
            )
        }
        .sorted { $0.processIdentifier < $1.processIdentifier }
}

private func validateUIConsumerSnapshot(
    _ reports: [UIConsumerReport],
    phase: String
) throws -> [UIConsumerReport] {
    let expectedNames = Set(expectedUIConsumerNames)
    let byName = Dictionary(grouping: reports, by: \.name)
    let counts = expectedUIConsumerNames
        .map { "\($0)=\(byName[$0]?.count ?? 0)" }
        .joined(separator: ",")
    try require(
        Set(byName.keys) == expectedNames
            && byName.values.allSatisfy { $0.count == 1 },
        "\(phase) UI consumer snapshot must contain each expected process exactly once"
            + " (\(counts))"
    )
    let expectedByName = Dictionary(
        uniqueKeysWithValues: expectedUIConsumers.map { ($0.name, $0) }
    )
    for report in reports {
        guard let expected = expectedByName[report.name] else {
            throw AuditFailure.failed(
                "\(phase) UI consumer snapshot contains an unknown process"
            )
        }
        try require(
            report.processIdentifier > 0,
            "\(phase) \(report.name) PID is not positive"
        )
        try require(
            report.launchTimeEpochSeconds.isFinite
                && report.launchTimeEpochSeconds > 0,
            "\(phase) \(report.name) start time is invalid"
        )
        try require(
            report.executablePath == canonicalPath(
                URL(fileURLWithPath: expected.executablePath)
            ),
            "\(phase) \(report.name) executable path is not authoritative"
        )
    }
    return reports.sorted { $0.name < $1.name }
}

private func uiConsumerStartTimeFormatters() -> (DateFormatter, ISO8601DateFormatter) {
    let inputFormatter = DateFormatter()
    inputFormatter.locale = Locale(identifier: "en_US_POSIX")
    inputFormatter.calendar = Calendar(identifier: .gregorian)
    inputFormatter.timeZone = TimeZone.current
    inputFormatter.dateFormat = "EEE MMM d HH:mm:ss yyyy"
    inputFormatter.isLenient = false
    return (inputFormatter, ISO8601DateFormatter())
}

private func filteredUIConsumerEvidence(
    _ collection: UIConsumerProcessCollection
) -> UIConsumerPartialEvidence {
    let expectedNameSet = Set(expectedUIConsumerNames)
    let expectedByName = Dictionary(
        uniqueKeysWithValues: expectedUIConsumers.map { ($0.name, $0) }
    )
    let (inputFormatter, outputFormatter) = uiConsumerStartTimeFormatters()
    var rows: [UIConsumerEvidenceRow] = []
    for rawLine in (collection.output ?? "")
        .split(whereSeparator: \.isNewline)
    {
        let fields = rawLine.split(
            maxSplits: 6,
            omittingEmptySubsequences: true,
            whereSeparator: \.isWhitespace
        )
        let line = String(rawLine)
        let candidatePath = fields.last.map(String.init)?
            .trimmingCharacters(in: .whitespaces)
        let candidateName = candidatePath.map {
            URL(fileURLWithPath: $0).lastPathComponent
        }
        let matchedName = candidateName.flatMap {
            expectedNameSet.contains($0) ? $0 : nil
        } ?? expectedUIConsumers.first(where: {
            line.contains($0.executablePath)
        })?.name
        guard let name = matchedName,
              let expected = expectedByName[name]
        else {
            continue
        }
        var processIdentifier: Int32?
        var launchTime: String?
        var launchTimeEpochSeconds: TimeInterval?
        var authoritativePath: String?
        var observedPathSHA256: String?
        var issue: String?
        if fields.count != 7 {
            issue = "malformed_row"
        } else {
            let executablePath = String(fields[6])
                .trimmingCharacters(in: .whitespaces)
            observedPathSHA256 = sha256(Data(executablePath.utf8))
            if let parsedPID = Int32(fields[0]), parsedPID > 0 {
                processIdentifier = parsedPID
            } else {
                issue = "invalid_pid"
            }
            let startText = fields[1 ... 5].map(String.init).joined(separator: " ")
            if let startDate = inputFormatter.date(from: startText),
               startDate.timeIntervalSince1970.isFinite,
               startDate.timeIntervalSince1970 > 0
            {
                launchTime = outputFormatter.string(from: startDate)
                launchTimeEpochSeconds = startDate.timeIntervalSince1970
            } else if issue == nil {
                issue = "invalid_start_time"
            }
            if canonicalPath(URL(fileURLWithPath: executablePath))
                == canonicalPath(URL(fileURLWithPath: expected.executablePath))
            {
                authoritativePath = canonicalPath(
                    URL(fileURLWithPath: expected.executablePath)
                )
            } else if issue == nil {
                issue = "wrong_executable_path"
            }
        }
        rows.append(
            UIConsumerEvidenceRow(
                filteredRowIndex: rows.count + 1,
                name: name,
                fieldCount: fields.count,
                processIdentifier: processIdentifier,
                launchTime: launchTime,
                launchTimeEpochSeconds: launchTimeEpochSeconds,
                executablePath: authoritativePath,
                observedPathSHA256: observedPathSHA256,
                issue: issue
            )
        )
    }
    return UIConsumerPartialEvidence(
        collectorExitStatus: collection.exitStatus,
        collectorOutputWasUTF8: collection.output != nil,
        expectedConsumerNames: expectedUIConsumerNames,
        rows: rows
    )
}

private func parseUIConsumerEvidence(
    _ evidence: UIConsumerPartialEvidence
) throws -> [UIConsumerReport] {
    try require(
        evidence.collectorExitStatus == 0,
        "public process collector failed"
    )
    try require(
        evidence.collectorOutputWasUTF8,
        "public process collector output is not UTF-8"
    )
    var reports: [UIConsumerReport] = []
    for row in evidence.rows {
        try require(
            row.issue == nil,
            "\(row.name) public process evidence is invalid (\(row.issue ?? "unknown"))"
        )
        guard let processIdentifier = row.processIdentifier,
              let launchTime = row.launchTime,
              let launchTimeEpochSeconds = row.launchTimeEpochSeconds,
              let executablePath = row.executablePath
        else {
            throw AuditFailure.failed(
                "\(row.name) public process evidence is incomplete"
            )
        }
        reports.append(
            UIConsumerReport(
                name: row.name,
                processIdentifier: processIdentifier,
                launchTime: launchTime,
                launchTimeEpochSeconds: launchTimeEpochSeconds,
                executablePath: executablePath
            )
        )
    }
    return try validateUIConsumerSnapshot(reports, phase: "collected")
}

private func parseUIConsumerProcessTable(_ output: String) throws -> [UIConsumerReport] {
    try parseUIConsumerEvidence(
        filteredUIConsumerEvidence(
            UIConsumerProcessCollection(exitStatus: 0, output: output)
        )
    )
}

private func publicProcessTableCollection(
    run: (Process) -> UIConsumerProcessCollection = runPublicProcessTable
) -> UIConsumerProcessCollection {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/bin/ps")
    process.arguments = ["-ww", "-axo", "pid=,lstart=,comm="]
    var environment = ProcessInfo.processInfo.environment
    environment["LC_ALL"] = "C"
    environment["LANG"] = "C"
    process.environment = environment
    return run(process)
}

private func runPublicProcessTable(_ process: Process) -> UIConsumerProcessCollection {
    let standardOutput = Pipe()
    process.standardOutput = standardOutput
    // Only stdout is consumed. A separate, unread stderr pipe could fill
    // while stdout waits for EOF, so discard stderr instead.
    process.standardError = FileHandle.nullDevice
    do {
        try process.run()
    } catch {
        return UIConsumerProcessCollection(exitStatus: -1, output: "")
    }
    let outputData = standardOutput.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    return UIConsumerProcessCollection(
        exitStatus: process.terminationStatus,
        output: String(data: outputData, encoding: .utf8)
    )
}

private func removeIfPresent(_ url: URL) throws {
    if FileManager.default.fileExists(atPath: url.path) {
        try FileManager.default.removeItem(at: url)
    }
}

private func atomicWrite(
    _ data: Data,
    to url: URL,
    write: (Data, URL, Data.WritingOptions) throws -> Void = {
        try $0.write(to: $1, options: $2)
    }
) throws {
    try write(data, url, .atomic)
}

private func privacySafeUIConsumerError(_ error: Error) -> String {
    if let failure = error as? AuditFailure {
        return failure.description
    }
    return "UI consumer tooling failed"
}

// Success leaves only the result file. Failure leaves the partial evidence
// (written first) and an error file holding the audit's own message or a
// generic one, never a raw tool error.
private func captureUIConsumerOutput<Output: Encodable>(
    paths: UIConsumerCapturePaths,
    collection: UIConsumerProcessCollection,
    transform: ([UIConsumerReport]) throws -> Output
) throws {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    try removeIfPresent(paths.success)
    try removeIfPresent(paths.partial)
    try removeIfPresent(paths.error)
    let evidence = filteredUIConsumerEvidence(collection)
    try atomicWrite(try encoder.encode(evidence), to: paths.partial)
    do {
        let reports = try parseUIConsumerEvidence(evidence)
        let output = try transform(reports)
        try atomicWrite(try encoder.encode(output), to: paths.success)
        try removeIfPresent(paths.partial)
        try removeIfPresent(paths.error)
    } catch {
        let safeError = privacySafeUIConsumerError(error)
        try atomicWrite(Data((safeError + "\n").utf8), to: paths.error)
        try removeIfPresent(paths.success)
        throw error
    }
}

// Every expected UI consumer must have a new PID and a later start time
// after the login than before it, from the same executable path.
private func validateUILoginGeneration(
    before: [UIConsumerReport],
    after: [UIConsumerReport]
) throws -> UILoginGenerationReport {
    let validatedBefore = try validateUIConsumerSnapshot(before, phase: "pre-login")
    let validatedAfter = try validateUIConsumerSnapshot(after, phase: "post-login")
    let beforeByName = Dictionary(grouping: validatedBefore, by: \.name)
    let afterByName = Dictionary(grouping: validatedAfter, by: \.name)
    for name in expectedUIConsumerNames {
        guard let old = beforeByName[name]?.first,
              let new = afterByName[name]?.first
        else {
            throw AuditFailure.failed("missing UI consumer generation for \(name)")
        }
        try require(
            old.processIdentifier != new.processIdentifier,
            "\(name) PID generation did not advance after login"
        )
        try require(
            new.launchTimeEpochSeconds > old.launchTimeEpochSeconds,
            "\(name) launch time did not advance after login"
        )
        try require(
            old.executablePath == new.executablePath,
            "\(name) executable path changed across login"
        )
    }
    return UILoginGenerationReport(before: before, after: after)
}

private func rawSnapshot(
    app: URL,
    bundleIdentifier: String,
    observations: [ModeObservation],
    currentSource: CurrentSourceReport,
    processes: [ProcessReport]
) throws -> RawSnapshot {
    let contents = app.appendingPathComponent("Contents", isDirectory: true)
    let infoURL = contents.appendingPathComponent("Info.plist")
    let infoData = try Data(contentsOf: infoURL)
    let plist = try loadPlist(at: infoURL)
    let executableIdentity: FileIdentity?
    if let executable = plist["CFBundleExecutable"] as? String,
       let identity = try? fileIdentity(
           at: contents
               .appendingPathComponent("MacOS", isDirectory: true)
               .appendingPathComponent(executable)
       )
    {
        executableIdentity = identity
    } else {
        executableIdentity = nil
    }
    let modes = observations
        .map { observation -> RawModeReport in
            let reachable = observation.iconURL.flatMap {
                try? $0.checkResourceIsReachable()
            } ?? false
            let iconData = observation.iconURL.flatMap { try? Data(contentsOf: $0) }
            return RawModeReport(
                inputModeID: observation.inputModeID,
                inputSourceID: observation.inputSourceID,
                localizedName: observation.localizedName,
                iconURLPath: observation.iconURL?
                    .resolvingSymlinksInPath()
                    .standardizedFileURL
                    .path,
                iconURLReachable: reachable,
                iconSHA256: iconData.map(sha256),
                iconRefAvailable: observation.iconRefAvailable,
                enabled: observation.enabled,
                selectCapable: observation.selectCapable
            )
        }
        .sorted { $0.inputModeID < $1.inputModeID }
    return RawSnapshot(
        installedAppPath: app.resolvingSymlinksInPath().standardizedFileURL.path,
        bundleIdentifier: plist["CFBundleIdentifier"] as? String,
        bundleVersion: plist["CFBundleVersion"] as? String,
        shortVersion: plist["CFBundleShortVersionString"] as? String,
        sourceCommit: plist["MuuSourceCommit"] as? String,
        buildVariant: plist["MuuBuildVariant"] as? String,
        compiledPlistSHA256: sha256(infoData),
        installedExecutablePath: executableIdentity?.path,
        installedExecutableSHA256: executableIdentity?.sha256,
        installedExecutableDeviceID: executableIdentity?.deviceID,
        installedExecutableInode: executableIdentity?.inode,
        installedCodeSignature: try? codeSignatureReport(app: app),
        modes: modes,
        currentSource: currentSource,
        processCount: processes.count,
        processes: processes
    )
}

private func writeFixturePlist(
    app: URL,
    bundleIdentifier: String,
    bundleVersion: String = "1",
    sourceCommit: String = String(repeating: "a", count: 40),
    buildVariant: String = "standard",
    orderedModes: [ExpectedMode] = expectedModes,
    defaultSuffix: String = "Hiragana",
    omitModeMappingFor suffixToOmit: String? = nil,
    omitLabelsFor suffixWithoutLabels: String? = nil,
    templateOverride: (suffix: String, value: Bool)? = nil,
    topLevelTemplate: Bool? = nil
) throws {
    var definitions: [String: Any] = [:]
    for mode in expectedModes {
        var definition: [String: Any] = [
            "TISIconIsTemplate": true,
            "TISIconLabels": ["Primary": mode.glyph],
            "TISInputSourceID": "\(bundleIdentifier).\(mode.suffix)",
            "TISIntendedLanguage": "ja",
            "tsInputModeAlternateMenuIconFileKey": "MuuMode\(mode.suffix)Selected.tiff",
            "tsInputModeDefaultStateKey": mode.suffix == defaultSuffix,
            "tsInputModeIsVisibleKey": true,
            "tsInputModeMenuIconFileKey": "MuuMode\(mode.suffix).tiff",
            "tsInputModePaletteIconFileKey": "MuuMode\(mode.suffix)Button.tiff",
            "tsInputModePrimaryInScriptKey": mode.suffix == defaultSuffix,
            "tsInputModeScriptKey": "smJapanese",
        ]
        if mode.suffix == suffixToOmit {
            definition.removeValue(forKey: "tsInputModeMenuIconFileKey")
        }
        if mode.suffix == suffixWithoutLabels {
            definition.removeValue(forKey: "TISIconLabels")
        }
        if let templateOverride, mode.suffix == templateOverride.suffix {
            definition["TISIconIsTemplate"] = templateOverride.value
        }
        definitions[mode.identifier] = definition
    }
    var plist: [String: Any] = [
        "CFBundleIdentifier": bundleIdentifier,
        "CFBundleVersion": bundleVersion,
        "CFBundleShortVersionString": "0.1.0",
        "CFBundleExecutable": "MuuIME",
        "MuuSourceCommit": sourceCommit,
        "MuuBuildVariant": buildVariant,
        "ComponentInputModeDict": [
            "tsInputModeListKey": definitions,
            "tsVisibleInputModeOrderedArrayKey": orderedModes.map(\.identifier),
        ],
    ]
    if let topLevelTemplate {
        plist["TISIconIsTemplate"] = topLevelTemplate
    }
    let contents = app.appendingPathComponent("Contents", isDirectory: true)
    try PropertyListSerialization.data(
        fromPropertyList: plist,
        format: .binary,
        options: 0
    ).write(to: contents.appendingPathComponent("Info.plist"))
}

private func requireFailure(_ message: String, _ operation: () throws -> Void) throws {
    do {
        try operation()
    } catch {
        return
    }
    throw AuditFailure.failed(message)
}

// Observe the configured Process through an injected runner: /bin/ps, the
// public pid/lstart/comm columns, C locale, one launch and its returned result.
// The real ps is never run here.
private func validateUIConsumerCollectorContract() throws {
    var observed: [Process] = []
    let collection = publicProcessTableCollection(run: { process in
        observed.append(process)
        return UIConsumerProcessCollection(exitStatus: 17, output: "fixture output")
    })
    try require(observed.count == 1, "collector must invoke its runner once")
    let process = observed[0]
    try require(
        process.executableURL?.path == "/bin/ps"
            && process.arguments == ["-ww", "-axo", "pid=,lstart=,comm="],
        "collector must request only public PID, start time and executable names"
    )
    try require(
        process.environment?["LC_ALL"] == "C" && process.environment?["LANG"] == "C",
        "collector dates must use a stable locale"
    )
    try require(
        collection.exitStatus == 17 && collection.output == "fixture output",
        "collector must pass through its runner result"
    )
}

// The tool helpers against a fake tool that writes 128 KiB to stdout
// and to stderr, well past the pipe buffer, before the line the helper needs.
// Waiting first, or draining stdout while a separate stderr pipe fills,
// can block the child. The 30 s guard terminates it, so a regression fails
// instead of hanging. Each helper must still find the facts written last.
private func validateToolOutputDraining() throws {
    let manager = FileManager.default
    let root = manager.temporaryDirectory.appendingPathComponent("muu-tool-output-\(UUID().uuidString)")
    try manager.createDirectory(at: root, withIntermediateDirectories: true,
                                attributes: [.posixPermissions: 0o700])
    defer { try? manager.removeItem(at: root) }
    let tool = root.appendingPathComponent("fake-tool")
    let padding = String(repeating: "x", count: 128 * 1024)
    let script = """
    #!/bin/bash
    printf '%s\\n' '\(padding)'
    printf '%s\\n' '\(padding)' >&2
    case "$1" in
    signature)
      printf '%s\\n' 'Identifier=dev.muu.fixture' 'CodeDirectory v=1 flags=0x0(none)' 'CDHash=0000000000000000000000000000000000000000' 'Signature=adhoc'
      ;;
    mappings) printf '\\0ftxt\\0i123\\0n/synthetic/MuuIME\\0' ;;
    processes) printf '%s\\n' '12345 Wed Oct 21 00:00:00 2026 /synthetic/MuuIME' ;;
    esac
    """
    try Data(script.utf8).write(to: tool)
    try manager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: tool.path)
    var guards: [DispatchWorkItem] = []
    var launched: [(String, Process)] = []
    defer { guards.forEach { $0.cancel() } }
    func monitor(_ mode: String, _ process: Process) {
        launched.append((mode, process))
        // Only private fixture processes are terminated by this hang guard.
        let guardTask = DispatchWorkItem { [weak process] in
            if let process, process.isRunning { process.terminate() }
        }
        guards.append(guardTask)
        DispatchQueue.global().asyncAfter(deadline: .now() + 30, execute: guardTask)
    }
    func launcher(_ mode: String) -> (Process) throws -> Void {
        { process in
            process.executableURL = tool
            process.arguments = [mode]
            try process.run()
            monitor(mode, process)
        }
    }
    let app = root.appendingPathComponent("Fixture.app")
    var failures: [String] = []
    do {
        let report = try codeSignatureReport(app: app, launch: launcher("signature"))
        try require(report.identifier == "dev.muu.fixture"
                    && report.signatureIdentity == "adhoc"
                    && report.cdHash == String(repeating: "0", count: 40),
                    "signature at the end of large output was lost")
    } catch { failures.append("signature") }
    do { try verifyStrictCodeSignature(app: app, launch: launcher("strict")) }
    catch { failures.append("strict") }
    let mappings = lsofTextMappings(processIdentifier: 1, launch: launcher("mappings"))
    if mappings.count != 1 || mappings.first?.inode != 123
        || mappings.first?.path != "/synthetic/MuuIME" {
        failures.append("mappings")
    }
    let tableProcess = Process()
    tableProcess.executableURL = tool
    tableProcess.arguments = ["processes"]
    monitor("processes", tableProcess)
    let table = runPublicProcessTable(tableProcess)
    if table.exitStatus != 0
        || table.output?.hasSuffix("12345 Wed Oct 21 00:00:00 2026 /synthetic/MuuIME\n") != true {
        failures.append("processes")
    }
    for (mode, process) in launched {
        print("large-output fixture \(mode): status=\(process.terminationStatus)")
    }
    try require(failures.isEmpty, "large fake tool output failed: \(failures.joined(separator: ", "))")
}

private func selfTest() throws {
    try validateToolOutputDraining()
    let manager = FileManager.default
    let root = manager.temporaryDirectory.appendingPathComponent(
        "muu-installed-icon-audit-\(UUID().uuidString)",
        isDirectory: true
    )
    defer { try? manager.removeItem(at: root) }
    let app = root.appendingPathComponent("MuuIME.app", isDirectory: true)
    let contents = app.appendingPathComponent("Contents", isDirectory: true)
    let resources = contents.appendingPathComponent("Resources", isDirectory: true)
    let macOS = contents.appendingPathComponent("MacOS", isDirectory: true)
    let sourceAssets = root.appendingPathComponent("source-assets", isDirectory: true)
    try manager.createDirectory(at: resources, withIntermediateDirectories: true)
    try manager.createDirectory(at: macOS, withIntermediateDirectories: true)
    try manager.createDirectory(at: sourceAssets, withIntermediateDirectories: true)
    let fixtureExecutableURL = macOS.appendingPathComponent("MuuIME")
    try Data("fixture executable".utf8).write(to: fixtureExecutableURL)
    let fixtureExecutable = try fileIdentity(at: fixtureExecutableURL)
    let bundleIdentifier = "dev.muu.inputmethod.Muu"
    let fixtureCodeSignature = CodeSignatureReport(
        identifier: bundleIdentifier,
        flags: "0x10002(adhoc,runtime)",
        cdHash: String(repeating: "c", count: 40),
        signatureIdentity: "adhoc"
    )
    let parsedFixtureCodeSignature = try parseCodeSignature(
        """
        Executable=/tmp/MuuIME
        Identifier=\(bundleIdentifier)
        CodeDirectory v=20500 size=1 flags=0x10002(adhoc,runtime) hashes=1+1 location=embedded
        CDHash=\(String(repeating: "c", count: 40))
        Signature=adhoc
        """
    )
    try require(
        parsedFixtureCodeSignature == fixtureCodeSignature,
        "valid code-sign evidence was not parsed deterministically"
    )
    try requireFailure("incomplete code-sign evidence was accepted") {
        _ = try parseCodeSignature(
            """
            Identifier=\(bundleIdentifier)
            CodeDirectory v=20500 size=1 flags=0x10002(adhoc,runtime)
            Signature=adhoc
            """
        )
    }
    try writeFixturePlist(app: app, bundleIdentifier: bundleIdentifier)

    var valid: [ModeObservation] = []
    for mode in expectedModes {
        let iconData = Data("fixture MuuMode\(mode.suffix) template mask".utf8)
        for name in [
            "MuuMode\(mode.suffix).tiff",
            "MuuMode\(mode.suffix)Selected.tiff",
            "MuuMode\(mode.suffix)Button.tiff",
        ] {
            try iconData.write(to: resources.appendingPathComponent(name))
            try iconData.write(to: sourceAssets.appendingPathComponent(name))
        }
        let publicIconURL = resources.appendingPathComponent("MuuMode\(mode.suffix).tiff")
        valid.append(
            ModeObservation(
                inputModeID: mode.identifier,
                inputSourceID: "\(bundleIdentifier).\(mode.suffix)",
                localizedName: mode.localizedName,
                iconURL: publicIconURL,
                iconRefAvailable: true,
                enabled: mode.isDefault,
                selectCapable: mode.isDefault
            )
        )
    }
    for name in ["MuuInputMethod.tiff", "MuuIME.icns"] {
        let data = Data("fixture \(name)".utf8)
        try data.write(to: resources.appendingPathComponent(name))
        try data.write(to: sourceAssets.appendingPathComponent(name))
    }
    let fixtureLocalization = try PropertyListSerialization.data(
        fromPropertyList: expectedJapaneseInfoPlistStrings,
        format: .xml,
        options: 0
    )
    let installedLocalizationDirectory = resources.appendingPathComponent(
        "ja.lproj",
        isDirectory: true
    )
    let sourceLocalizationDirectory = sourceAssets
        .deletingLastPathComponent()
        .appendingPathComponent("ja.lproj", isDirectory: true)
    try FileManager.default.createDirectory(
        at: installedLocalizationDirectory,
        withIntermediateDirectories: true
    )
    try FileManager.default.createDirectory(
        at: sourceLocalizationDirectory,
        withIntermediateDirectories: true
    )
    try fixtureLocalization.write(
        to: installedLocalizationDirectory.appendingPathComponent("InfoPlist.strings")
    )
    try fixtureLocalization.write(
        to: sourceLocalizationDirectory.appendingPathComponent("InfoPlist.strings")
    )
    let fixtureCurrentSource = CurrentSourceReport(
        inputSourceID: "\(bundleIdentifier).Hiragana",
        inputModeID: expectedModes[0].identifier,
        bundleIdentifier: bundleIdentifier,
        localizedName: expectedModes[0].localizedName
    )
    let fixtureProcesses = [
        ProcessReport(
            processIdentifier: 42,
            executablePath: fixtureExecutable.path,
            executableSHA256: fixtureExecutable.sha256,
            executableDeviceID: fixtureExecutable.deviceID,
            executableInode: fixtureExecutable.inode,
            mappedExecutablePath: fixtureExecutable.path,
            mappedExecutableInode: fixtureExecutable.inode
        ),
    ]
    let artifactAuthority = try validateArtifactAuthority(
        app: app,
        expectedCommit: String(repeating: "a", count: 40),
        expectedBuildNumber: "1",
        expectedBuildVariant: "standard",
        expectedExecutableSHA256: fixtureExecutable.sha256,
        expectedCodeSignature: fixtureCodeSignature,
        actualCodeSignature: fixtureCodeSignature
    )
    try require(
        artifactAuthority.buildVariant == "standard"
            && artifactAuthority.executableSHA256 == fixtureExecutable.sha256,
        "valid artifact authority was not reported"
    )
    try writeFixturePlist(app: app, bundleIdentifier: bundleIdentifier, buildVariant: "dogfood")
    let dogfoodAuthority = try validateArtifactAuthority(
        app: app, expectedCommit: String(repeating: "a", count: 40),
        expectedBuildNumber: "1", expectedBuildVariant: "dogfood",
        expectedExecutableSHA256: fixtureExecutable.sha256,
        expectedCodeSignature: fixtureCodeSignature, actualCodeSignature: fixtureCodeSignature)
    try require(dogfoodAuthority.buildVariant == "dogfood", "explicit dogfood artifact was rejected")
    try requireFailure("dogfood artifact was accepted as distribution") {
        _ = try validateArtifactAuthority(
            app: app, expectedCommit: String(repeating: "a", count: 40),
            expectedBuildNumber: "1", expectedBuildVariant: "standard",
            expectedExecutableSHA256: fixtureExecutable.sha256,
            expectedCodeSignature: fixtureCodeSignature, actualCodeSignature: fixtureCodeSignature)
    }
    try writeFixturePlist(app: app, bundleIdentifier: bundleIdentifier,
        buildVariant: "unexpected-variant")
    try requireFailure("unapproved artifact variant was accepted") {
        _ = try validateArtifactAuthority(
            app: app, expectedCommit: String(repeating: "a", count: 40),
            expectedBuildNumber: "1", expectedBuildVariant: "unexpected-variant",
            expectedExecutableSHA256: fixtureExecutable.sha256,
            expectedCodeSignature: fixtureCodeSignature, actualCodeSignature: fixtureCodeSignature)
    }
    try requireFailure("unapproved artifact accepted as standard") {
        _ = try validateArtifactAuthority(
            app: app, expectedCommit: String(repeating: "a", count: 40),
            expectedBuildNumber: "1", expectedBuildVariant: "standard",
            expectedExecutableSHA256: fixtureExecutable.sha256,
            expectedCodeSignature: fixtureCodeSignature, actualCodeSignature: fixtureCodeSignature)
    }
    try writeFixturePlist(app: app, bundleIdentifier: bundleIdentifier)
    try requireFailure("wrong artifact variant fixture was accepted") {
        _ = try validateArtifactAuthority(
            app: app,
            expectedCommit: String(repeating: "a", count: 40),
            expectedBuildNumber: "1",
            expectedBuildVariant: "unexpected-variant",
            expectedExecutableSHA256: fixtureExecutable.sha256,
            expectedCodeSignature: fixtureCodeSignature,
            actualCodeSignature: fixtureCodeSignature
        )
    }
    try requireFailure("wrong artifact executable SHA fixture was accepted") {
        _ = try validateArtifactAuthority(
            app: app,
            expectedCommit: String(repeating: "a", count: 40),
            expectedBuildNumber: "1",
            expectedBuildVariant: "standard",
            expectedExecutableSHA256: String(repeating: "0", count: 64),
            expectedCodeSignature: fixtureCodeSignature,
            actualCodeSignature: fixtureCodeSignature
        )
    }
    let wrongArtifactSignature = CodeSignatureReport(
        identifier: fixtureCodeSignature.identifier,
        flags: fixtureCodeSignature.flags,
        cdHash: String(repeating: "d", count: 40),
        signatureIdentity: fixtureCodeSignature.signatureIdentity
    )
    try requireFailure("wrong artifact code-sign fixture was accepted") {
        _ = try validateArtifactAuthority(
            app: app,
            expectedCommit: String(repeating: "a", count: 40),
            expectedBuildNumber: "1",
            expectedBuildVariant: "standard",
            expectedExecutableSHA256: fixtureExecutable.sha256,
            expectedCodeSignature: fixtureCodeSignature,
            actualCodeSignature: wrongArtifactSignature
        )
    }
    func validateFixture(
        _ observations: [ModeObservation],
        expectedCommit: String = String(repeating: "a", count: 40),
        expectedBuildNumber: String = "1",
        expectedBuildVariant: String = "standard",
        expectedExecutableSHA256: String? = nil,
        expectedCodeSignature: CodeSignatureReport? = nil,
        installedCodeSignature: CodeSignatureReport? = nil,
        auditMode: AuditMode = .runtime,
        processes: [ProcessReport]? = nil,
        assets: URL? = nil
    ) throws -> AuditReport {
        try validateInstalledContract(
            app: app,
            bundleIdentifier: bundleIdentifier,
            sourceAssets: assets ?? sourceAssets,
            expectedCommit: expectedCommit,
            expectedBuildNumber: expectedBuildNumber,
            expectedBuildVariant: expectedBuildVariant,
            expectedExecutableSHA256: expectedExecutableSHA256
                ?? fixtureExecutable.sha256,
            expectedCodeSignature: expectedCodeSignature ?? fixtureCodeSignature,
            installedCodeSignature: installedCodeSignature ?? fixtureCodeSignature,
            auditMode: auditMode,
            observations: observations,
            currentSource: fixtureCurrentSource,
            processes: processes ?? fixtureProcesses
        )
    }
    let report = try validateFixture(valid)
    try require(report.modes.count == 2, "valid public TIS fixture was not accepted")
    try require(
        report.environmentBlockers.count == 1,
        "disabled non-default modes were not separated as environment blockers"
    )
    try require(
        report.auditMode == .runtime
            && report.processCount == 1
            && report.expectedExecutableSHA256 == fixtureExecutable.sha256
            && report.installedExecutableSHA256 == fixtureExecutable.sha256
            && report.expectedCodeSignature == fixtureCodeSignature
            && report.installedCodeSignature == fixtureCodeSignature
            && report.installedExecutableInode == fixtureExecutable.inode
            && report.processes[0].mappedExecutableInode == fixtureExecutable.inode,
        "valid runtime executable authority was not reported"
    )
    let prelaunch = try validateFixture(
        valid,
        auditMode: .freshPrelaunch,
        processes: []
    )
    try require(
        prelaunch.auditMode == .freshPrelaunch && prelaunch.processCount == 0,
        "fresh-prelaunch zero-process fixture was not accepted"
    )
    try requireFailure("runtime zero-process fixture was accepted") {
        _ = try validateFixture(valid, processes: [])
    }
    try requireFailure("fresh-prelaunch running-process fixture was accepted") {
        _ = try validateFixture(
            valid,
            auditMode: .freshPrelaunch,
            processes: fixtureProcesses
        )
    }
    try requireFailure("multiple runtime process fixture was accepted") {
        _ = try validateFixture(
            valid,
            processes: fixtureProcesses + fixtureProcesses
        )
    }
    try requireFailure("wrong reviewed executable SHA-256 fixture was accepted") {
        _ = try validateFixture(
            valid,
            expectedExecutableSHA256: String(repeating: "0", count: 64)
        )
    }
    try requireFailure("wrong installed build variant fixture was accepted") {
        _ = try validateFixture(
            valid,
            expectedBuildVariant: "unexpected-variant"
        )
    }
    let wrongSignature = CodeSignatureReport(
        identifier: bundleIdentifier,
        flags: fixtureCodeSignature.flags,
        cdHash: String(repeating: "d", count: 40),
        signatureIdentity: fixtureCodeSignature.signatureIdentity
    )
    try requireFailure("wrong installed code-sign evidence fixture was accepted") {
        _ = try validateFixture(valid, installedCodeSignature: wrongSignature)
    }
    let missingRuntime = CodeSignatureReport(
        identifier: bundleIdentifier,
        flags: "0x2(adhoc)",
        cdHash: fixtureCodeSignature.cdHash,
        signatureIdentity: fixtureCodeSignature.signatureIdentity
    )
    try requireFailure("reviewed code-sign evidence without runtime was accepted") {
        _ = try validateFixture(
            valid,
            expectedCodeSignature: missingRuntime,
            installedCodeSignature: missingRuntime
        )
    }
    let originalExecutableData = try Data(contentsOf: fixtureExecutableURL)
    try Data("old executable fixture".utf8).write(to: fixtureExecutableURL)
    try requireFailure("old installed executable bytes fixture was accepted") {
        _ = try validateFixture(valid)
    }
    try originalExecutableData.write(to: fixtureExecutableURL)
    let wrongPathProcess = ProcessReport(
        processIdentifier: 42,
        executablePath: root.appendingPathComponent("WrongMuuIME").path,
        executableSHA256: fixtureExecutable.sha256,
        executableDeviceID: fixtureExecutable.deviceID,
        executableInode: fixtureExecutable.inode,
        mappedExecutablePath: fixtureExecutable.path,
        mappedExecutableInode: fixtureExecutable.inode
    )
    try requireFailure("wrong runtime executable path fixture was accepted") {
        _ = try validateFixture(valid, processes: [wrongPathProcess])
    }
    let wrongHashProcess = ProcessReport(
        processIdentifier: 42,
        executablePath: fixtureExecutable.path,
        executableSHA256: String(repeating: "f", count: 64),
        executableDeviceID: fixtureExecutable.deviceID,
        executableInode: fixtureExecutable.inode,
        mappedExecutablePath: fixtureExecutable.path,
        mappedExecutableInode: fixtureExecutable.inode
    )
    try requireFailure("wrong runtime executable hash fixture was accepted") {
        _ = try validateFixture(valid, processes: [wrongHashProcess])
    }
    let wrongInodeProcess = ProcessReport(
        processIdentifier: 42,
        executablePath: fixtureExecutable.path,
        executableSHA256: fixtureExecutable.sha256,
        executableDeviceID: fixtureExecutable.deviceID,
        executableInode: fixtureExecutable.inode + 1,
        mappedExecutablePath: fixtureExecutable.path,
        mappedExecutableInode: fixtureExecutable.inode + 1
    )
    try requireFailure("wrong runtime executable inode fixture was accepted") {
        _ = try validateFixture(valid, processes: [wrongInodeProcess])
    }
    let wrongMappingProcess = ProcessReport(
        processIdentifier: 42,
        executablePath: fixtureExecutable.path,
        executableSHA256: fixtureExecutable.sha256,
        executableDeviceID: fixtureExecutable.deviceID,
        executableInode: fixtureExecutable.inode,
        mappedExecutablePath: root.appendingPathComponent("OldMuuIME").path,
        mappedExecutableInode: fixtureExecutable.inode
    )
    try requireFailure("wrong PID text mapping fixture was accepted") {
        _ = try validateFixture(valid, processes: [wrongMappingProcess])
    }
    let wrongMappingInodeProcess = ProcessReport(
        processIdentifier: 42,
        executablePath: fixtureExecutable.path,
        executableSHA256: fixtureExecutable.sha256,
        executableDeviceID: fixtureExecutable.deviceID,
        executableInode: fixtureExecutable.inode,
        mappedExecutablePath: fixtureExecutable.path,
        mappedExecutableInode: fixtureExecutable.inode + 1
    )
    try requireFailure("wrong PID text mapping inode fixture was accepted") {
        _ = try validateFixture(valid, processes: [wrongMappingInodeProcess])
    }

    try requireFailure("missing public TIS mode fixture was accepted") {
        _ = try validateFixture(Array(valid.dropLast()))
    }
    var wrongID = valid
    wrongID[0] = ModeObservation(
        inputModeID: wrongID[0].inputModeID,
        inputSourceID: "\(bundleIdentifier).Wrong",
        localizedName: wrongID[0].localizedName,
        iconURL: wrongID[0].iconURL,
        iconRefAvailable: wrongID[0].iconRefAvailable,
        enabled: true,
        selectCapable: true
    )
    try requireFailure("changed public TISInputSourceID fixture was accepted") {
        _ = try validateFixture(wrongID)
    }
    var changedName = valid
    changedName[0] = ModeObservation(
        inputModeID: changedName[0].inputModeID,
        inputSourceID: changedName[0].inputSourceID,
        localizedName: "changed",
        iconURL: changedName[0].iconURL,
        iconRefAvailable: changedName[0].iconRefAvailable,
        enabled: true,
        selectCapable: true
    )
    try requireFailure("changed localized-name fixture was accepted") {
        _ = try validateFixture(changedName)
    }
    let activeLegacyMode = ModeObservation(
        inputModeID: removedPublicModes[0].identifier,
        inputSourceID: "\(bundleIdentifier).\(removedPublicModes[0].suffix)",
        localizedName: "legacy",
        iconURL: nil,
        iconRefAvailable: false,
        enabled: true,
        selectCapable: true
    )
    try requireFailure("active legacy five-mode source fixture was accepted") {
        _ = try validateFixture(valid + [activeLegacyMode])
    }
    let inertLegacyMode = ModeObservation(
        inputModeID: activeLegacyMode.inputModeID,
        inputSourceID: activeLegacyMode.inputSourceID,
        localizedName: activeLegacyMode.localizedName,
        iconURL: nil,
        iconRefAvailable: false,
        enabled: false,
        selectCapable: false
    )
    _ = try validateFixture(valid + [inertLegacyMode])
    var unreachable = valid
    unreachable[0] = ModeObservation(
        inputModeID: unreachable[0].inputModeID,
        inputSourceID: unreachable[0].inputSourceID,
        localizedName: unreachable[0].localizedName,
        iconURL: root.appendingPathComponent("missing.tiff"),
        iconRefAvailable: true,
        enabled: true,
        selectCapable: true
    )
    try requireFailure("unreachable public icon fixture was accepted") {
        _ = try validateFixture(unreachable)
    }
    let outsideIconURL = root.appendingPathComponent("outside.tiff")
    try Data(contentsOf: valid[0].iconURL!).write(to: outsideIconURL)
    var outsideBundle = valid
    outsideBundle[0] = ModeObservation(
        inputModeID: outsideBundle[0].inputModeID,
        inputSourceID: outsideBundle[0].inputSourceID,
        localizedName: outsideBundle[0].localizedName,
        iconURL: outsideIconURL,
        iconRefAvailable: true,
        enabled: true,
        selectCapable: true
    )
    try requireFailure("bundle-external public icon URL fixture was accepted") {
        _ = try validateFixture(outsideBundle)
    }
    let hiraganaIcon = resources.appendingPathComponent("MuuModeHiragana.tiff")
    let originalHiragana = try Data(contentsOf: hiraganaIcon)
    try Data("drift".utf8).write(to: hiraganaIcon)
    try requireFailure("reviewed source/installed icon byte-drift fixture was accepted") {
        _ = try validateFixture(valid)
    }
    try originalHiragana.write(to: hiraganaIcon)
    try requireFailure("old installed build-number fixture was accepted") {
        _ = try validateFixture(valid, expectedBuildNumber: "2")
    }
    try requireFailure("old installed source-commit fixture was accepted") {
        _ = try validateFixture(
            valid,
            expectedCommit: String(repeating: "b", count: 40)
        )
    }

    try writeFixturePlist(
        app: app,
        bundleIdentifier: bundleIdentifier,
        orderedModes: Array(expectedModes.reversed())
    )
    try requireFailure("changed installed mode-order fixture was accepted") {
        _ = try validateFixture(valid)
    }
    try writeFixturePlist(
        app: app,
        bundleIdentifier: bundleIdentifier,
        defaultSuffix: "Roman"
    )
    try requireFailure("changed installed default-mode fixture was accepted") {
        _ = try validateFixture(valid)
    }
    try writeFixturePlist(
        app: app,
        bundleIdentifier: bundleIdentifier,
        omitModeMappingFor: "Roman"
    )
    try requireFailure("missing installed mode-icon mapping fixture was accepted") {
        _ = try validateFixture(valid)
    }
    try writeFixturePlist(
        app: app,
        bundleIdentifier: bundleIdentifier,
        omitLabelsFor: "Hiragana"
    )
    try requireFailure("missing installed Primary label fixture was accepted") {
        _ = try validateFixture(valid)
    }
    try writeFixturePlist(
        app: app,
        bundleIdentifier: bundleIdentifier,
        templateOverride: (suffix: "Hiragana", value: false)
    )
    try requireFailure("disabled installed mode-template fixture was accepted") {
        _ = try validateFixture(valid)
    }
    try writeFixturePlist(
        app: app,
        bundleIdentifier: bundleIdentifier,
        topLevelTemplate: true
    )
    try requireFailure("top-level component-template fixture was accepted") {
        _ = try validateFixture(valid)
    }

    var legacyPlist = try loadPlist(at: contents.appendingPathComponent("Info.plist"))
    legacyPlist.removeValue(forKey: "MuuSourceCommit")
    try PropertyListSerialization.data(
        fromPropertyList: legacyPlist,
        format: .binary,
        options: 0
    ).write(to: contents.appendingPathComponent("Info.plist"))
    let legacySnapshot = try rawSnapshot(
        app: app,
        bundleIdentifier: bundleIdentifier,
        observations: valid,
        currentSource: fixtureCurrentSource,
        processes: fixtureProcesses
    )
    try require(
        legacySnapshot.sourceCommit == nil,
        "legacy pre-upgrade snapshot must tolerate missing source provenance"
    )

    try validateUIConsumerCollectorContract()
    let writeBytes = Data("collector evidence".utf8)
    let writeURL = root.appendingPathComponent("write-request.json")
    // The evidence writer must request one .atomic write of these bytes
    // to this URL.
    // It does not test atomicity against power loss.
    var writeRequests: [(Data, URL, Data.WritingOptions)] = []
    try atomicWrite(writeBytes, to: writeURL, write: { bytes, url, options in
        writeRequests.append((bytes, url, options))
    })
    try require(writeRequests.count == 1, "evidence writer must make one write request")
    try require(
        writeRequests[0].0 == writeBytes && writeRequests[0].1 == writeURL
            && writeRequests[0].2 == .atomic,
        "evidence writer must forward bytes and destination with atomic replacement"
    )
    let uiConsumerPath = Dictionary(
        uniqueKeysWithValues: expectedUIConsumers.map {
            ($0.name, canonicalPath(URL(fileURLWithPath: $0.executablePath)))
        }
    )
    func publicProcessFixtureLine(
        pid: Int32,
        second: Int,
        path: String
    ) -> String {
        String(
            format: "%5d Wed Jul 29 00:00:%02d 2026     %@",
            pid,
            second,
            path
        )
    }
    let publicProcessFixture = expectedUIConsumers.enumerated()
        .map { index, expected in
            publicProcessFixtureLine(
                pid: Int32(100 + index),
                second: index,
                path: expected.executablePath
            )
        }
        .joined(separator: "\n")
    let parsedPublicConsumers = try parseUIConsumerProcessTable(
        publicProcessFixture
    )
    try require(
        parsedPublicConsumers.count == expectedUIConsumers.count,
        "public process fixture did not retain UI consumers omitted by nil NSWorkspace launchDate"
    )
    try requireFailure("missing public UI consumer fixture was accepted") {
        _ = try parseUIConsumerProcessTable(
            expectedUIConsumers.dropLast().enumerated()
                .map { index, expected in
                    publicProcessFixtureLine(
                        pid: Int32(100 + index),
                        second: index,
                        path: expected.executablePath
                    )
                }
                .joined(separator: "\n")
        )
    }
    try requireFailure("duplicate public UI consumer fixture was accepted") {
        _ = try parseUIConsumerProcessTable(
            publicProcessFixture
                + "\n"
                + publicProcessFixtureLine(
                    pid: 999,
                    second: 9,
                    path: expectedUIConsumers[0].executablePath
                )
        )
    }
    try requireFailure("wrong public UI consumer path fixture was accepted") {
        _ = try parseUIConsumerProcessTable(
            publicProcessFixture.replacingOccurrences(
                of: expectedUIConsumers[0].executablePath,
                with: "/tmp/System Settings"
            )
        )
    }
    try requireFailure("unparseable public UI consumer time fixture was accepted") {
        _ = try parseUIConsumerProcessTable(
            publicProcessFixture.replacingOccurrences(
                of: "Wed Jul 29 00:00:00 2026",
                with: "Wed Jul 29 99:99:99 2026"
            )
        )
    }
    try requireFailure("nonpositive public UI consumer PID fixture was accepted") {
        _ = try parseUIConsumerProcessTable(
            publicProcessFixture.replacingOccurrences(
                of: publicProcessFixtureLine(
                    pid: 100,
                    second: 0,
                    path: expectedUIConsumers[0].executablePath
                ),
                with: publicProcessFixtureLine(
                    pid: 0,
                    second: 0,
                    path: expectedUIConsumers[0].executablePath
                )
            )
        )
    }
    try requireFailure("malformed public UI consumer row fixture was accepted") {
        _ = try parseUIConsumerProcessTable(
            publicProcessFixture.replacingOccurrences(
                of: "  100 Wed Jul 29 00:00:00 2026",
                with: "invalid"
            )
        )
    }
    let unrelatedProcessFixture =
        "99999 Wed Jul 29 00:00:09 2026 /Users/private/UNRELATED_SECRET --token"
    func capturePaths(_ name: String) -> UIConsumerCapturePaths {
        UIConsumerCapturePaths(
            success: root.appendingPathComponent("\(name)-success.json"),
            partial: root.appendingPathComponent("\(name)-partial.json"),
            error: root.appendingPathComponent("\(name)-error.txt")
        )
    }
    func verifyFailedCapture(
        _ name: String,
        collection: UIConsumerProcessCollection
    ) throws {
        let paths = capturePaths(name)
        try requireFailure("\(name) production capture fixture was accepted") {
            try captureUIConsumerOutput(
                paths: paths,
                collection: collection
            ) { $0 }
        }
        try require(
            !manager.fileExists(atPath: paths.success.path),
            "\(name) left a success JSON after failure"
        )
        let partial = try Data(contentsOf: paths.partial)
        let error = try Data(contentsOf: paths.error)
        try require(
            !partial.isEmpty && !error.isEmpty,
            "\(name) did not preserve nonempty partial evidence and error"
        )
        let partialText = String(decoding: partial, as: UTF8.self)
        let errorText = String(decoding: error, as: UTF8.self)
        try require(
            !partialText.contains("UNRELATED_SECRET")
                && !partialText.contains("--token")
                && !errorText.contains("UNRELATED_SECRET")
                && !errorText.contains("--token"),
            "\(name) leaked an unrelated process row or arguments"
        )
        let evidence = try JSONDecoder().decode(
            UIConsumerPartialEvidence.self,
            from: partial
        )
        try require(
            evidence.rows.allSatisfy {
                expectedUIConsumerNames.contains($0.name)
            },
            "\(name) partial evidence included an unrelated process"
        )
    }
    let publicProcessFixtureWithUnrelated =
        publicProcessFixture + "\n" + unrelatedProcessFixture
    let successfulCapturePaths = capturePaths("valid")
    try captureUIConsumerOutput(
        paths: successfulCapturePaths,
        collection: UIConsumerProcessCollection(
            exitStatus: 0,
            output: publicProcessFixtureWithUnrelated
        )
    ) { $0 }
    try require(
        manager.fileExists(atPath: successfulCapturePaths.success.path)
            && !manager.fileExists(atPath: successfulCapturePaths.partial.path)
            && !manager.fileExists(atPath: successfulCapturePaths.error.path),
        "valid production capture did not publish only the success JSON"
    )
    try verifyFailedCapture(
        "missing",
        collection: UIConsumerProcessCollection(
            exitStatus: 0,
            output: expectedUIConsumers.dropLast().enumerated()
                .map { index, expected in
                    publicProcessFixtureLine(
                        pid: Int32(100 + index),
                        second: index,
                        path: expected.executablePath
                    )
                }
                .joined(separator: "\n") + "\n" + unrelatedProcessFixture
        )
    )
    try verifyFailedCapture(
        "duplicate",
        collection: UIConsumerProcessCollection(
            exitStatus: 0,
            output: publicProcessFixtureWithUnrelated
                + "\n"
                + publicProcessFixtureLine(
                    pid: 999,
                    second: 9,
                    path: expectedUIConsumers[0].executablePath
                )
        )
    )
    try verifyFailedCapture(
        "wrong-path",
        collection: UIConsumerProcessCollection(
            exitStatus: 0,
            output: publicProcessFixtureWithUnrelated.replacingOccurrences(
                of: expectedUIConsumers[0].executablePath,
                with: "/tmp/System Settings"
            )
        )
    )
    try verifyFailedCapture(
        "invalid-time",
        collection: UIConsumerProcessCollection(
            exitStatus: 0,
            output: publicProcessFixtureWithUnrelated.replacingOccurrences(
                of: "Wed Jul 29 00:00:00 2026",
                with: "Wed Jul 29 99:99:99 2026"
            )
        )
    )
    try verifyFailedCapture(
        "nonpositive-pid",
        collection: UIConsumerProcessCollection(
            exitStatus: 0,
            output: publicProcessFixtureWithUnrelated.replacingOccurrences(
                of: publicProcessFixtureLine(
                    pid: 100,
                    second: 0,
                    path: expectedUIConsumers[0].executablePath
                ),
                with: publicProcessFixtureLine(
                    pid: 0,
                    second: 0,
                    path: expectedUIConsumers[0].executablePath
                )
            )
        )
    )
    try verifyFailedCapture(
        "malformed",
        collection: UIConsumerProcessCollection(
            exitStatus: 0,
            output: publicProcessFixtureWithUnrelated.replacingOccurrences(
                of: "  100 Wed Jul 29 00:00:00 2026",
                with: "invalid"
            )
        )
    )
    try verifyFailedCapture(
        "collector-nonzero",
        collection: UIConsumerProcessCollection(
            exitStatus: 3,
            output: unrelatedProcessFixture
        )
    )
    let preLoginConsumers = expectedUIConsumerNames.enumerated().map { index, name in
        UIConsumerReport(
            name: name,
            processIdentifier: Int32(100 + index),
            launchTime: "2026-07-29T00:00:0\(index)Z",
            launchTimeEpochSeconds: 1000 + Double(index),
            executablePath: uiConsumerPath[name]!
        )
    }
    let postLoginConsumers = expectedUIConsumerNames.enumerated().map { index, name in
        UIConsumerReport(
            name: name,
            processIdentifier: Int32(200 + index),
            launchTime: "2026-07-29T01:00:0\(index)Z",
            launchTimeEpochSeconds: 2000 + Double(index),
            executablePath: uiConsumerPath[name]!
        )
    }
    _ = try validateUILoginGeneration(
        before: preLoginConsumers,
        after: postLoginConsumers
    )
    try requireFailure("unchanged UI consumer PID fixture was accepted") {
        var stale = postLoginConsumers
        stale[0] = UIConsumerReport(
            name: stale[0].name,
            processIdentifier: preLoginConsumers[0].processIdentifier,
            launchTime: stale[0].launchTime,
            launchTimeEpochSeconds: stale[0].launchTimeEpochSeconds,
            executablePath: stale[0].executablePath
        )
        _ = try validateUILoginGeneration(before: preLoginConsumers, after: stale)
    }
    try requireFailure("unchanged UI consumer launch-time fixture was accepted") {
        var stale = postLoginConsumers
        stale[0] = UIConsumerReport(
            name: stale[0].name,
            processIdentifier: stale[0].processIdentifier,
            launchTime: preLoginConsumers[0].launchTime,
            launchTimeEpochSeconds: preLoginConsumers[0].launchTimeEpochSeconds,
            executablePath: stale[0].executablePath
        )
        _ = try validateUILoginGeneration(before: preLoginConsumers, after: stale)
    }
    try requireFailure("earlier UI consumer launch-time fixture was accepted") {
        var stale = postLoginConsumers
        stale[0] = UIConsumerReport(
            name: stale[0].name,
            processIdentifier: stale[0].processIdentifier,
            launchTime: "2026-07-28T23:59:59Z",
            launchTimeEpochSeconds:
                preLoginConsumers[0].launchTimeEpochSeconds - 1,
            executablePath: stale[0].executablePath
        )
        _ = try validateUILoginGeneration(before: preLoginConsumers, after: stale)
    }
    try requireFailure("changed UI consumer executable path fixture was accepted") {
        var stale = postLoginConsumers
        stale[0] = UIConsumerReport(
            name: stale[0].name,
            processIdentifier: stale[0].processIdentifier,
            launchTime: stale[0].launchTime,
            launchTimeEpochSeconds: stale[0].launchTimeEpochSeconds,
            executablePath: "/tmp/System Settings"
        )
        _ = try validateUILoginGeneration(before: preLoginConsumers, after: stale)
    }
    try requireFailure("missing post-login UI consumer fixture was accepted") {
        _ = try validateUILoginGeneration(
            before: preLoginConsumers,
            after: Array(postLoginConsumers.dropLast())
        )
    }
    let unchangedGenerationFixture = expectedUIConsumers.enumerated()
        .map { index, expected in
            publicProcessFixtureLine(
                pid: Int32(100 + index),
                second: index,
                path: expected.executablePath
            )
        }
        .joined(separator: "\n") + "\n" + unrelatedProcessFixture
    let generationFailurePaths = capturePaths("generation-unchanged")
    try requireFailure("generation capture failure published success") {
        try captureUIConsumerOutput(
            paths: generationFailurePaths,
            collection: UIConsumerProcessCollection(
                exitStatus: 0,
                output: unchangedGenerationFixture
            )
        ) { after in
            try validateUILoginGeneration(
                before: preLoginConsumers,
                after: after
            )
        }
    }
    let generationPartial = try Data(
        contentsOf: generationFailurePaths.partial
    )
    let generationError = try Data(contentsOf: generationFailurePaths.error)
    try require(
        !manager.fileExists(atPath: generationFailurePaths.success.path)
            && !generationPartial.isEmpty
            && !generationError.isEmpty,
        "verify-login-generation did not preserve failure evidence"
    )
    try require(
        !String(decoding: generationPartial, as: UTF8.self)
            .contains("UNRELATED_SECRET")
            && !String(decoding: generationError, as: UTF8.self)
                .contains("UNRELATED_SECRET"),
        "verify-login-generation leaked an unrelated process row"
    )
}

do {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    if CommandLine.arguments.count == 2, CommandLine.arguments[1] == "--self-test" {
        try selfTest()
    } else if CommandLine.arguments.count == 3, CommandLine.arguments[1] == "signature" {
        let report = try codeSignatureReport(
            app: URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        )
        FileHandle.standardOutput.write(try encoder.encode(report))
        FileHandle.standardOutput.write(Data("\n".utf8))
    } else if CommandLine.arguments.count == 4, CommandLine.arguments[1] == "snapshot" {
        let bundleIdentifier = CommandLine.arguments[2]
        let app = URL(fileURLWithPath: CommandLine.arguments[3], isDirectory: true)
        let snapshot = try rawSnapshot(
            app: app,
            bundleIdentifier: bundleIdentifier,
            observations: observations(bundleIdentifier: bundleIdentifier),
            currentSource: currentSourceReport(),
            processes: processReports(bundleIdentifier: bundleIdentifier)
        )
        FileHandle.standardOutput.write(try encoder.encode(snapshot))
        FileHandle.standardOutput.write(Data("\n".utf8))
    } else if CommandLine.arguments.count == 5,
              CommandLine.arguments[1] == "ui-consumers"
    {
        try captureUIConsumerOutput(
            paths: UIConsumerCapturePaths(
                success: URL(fileURLWithPath: CommandLine.arguments[2]),
                partial: URL(fileURLWithPath: CommandLine.arguments[3]),
                error: URL(fileURLWithPath: CommandLine.arguments[4])
            ),
            collection: publicProcessTableCollection()
        ) { $0 }
    } else if CommandLine.arguments.count == 6,
              CommandLine.arguments[1] == "verify-login-generation"
    {
        try captureUIConsumerOutput(
            paths: UIConsumerCapturePaths(
                success: URL(fileURLWithPath: CommandLine.arguments[3]),
                partial: URL(fileURLWithPath: CommandLine.arguments[4]),
                error: URL(fileURLWithPath: CommandLine.arguments[5])
            ),
            collection: publicProcessTableCollection()
        ) { after in
            let before: [UIConsumerReport]
            do {
                before = try JSONDecoder().decode(
                    [UIConsumerReport].self,
                    from: Data(
                        contentsOf: URL(
                            fileURLWithPath: CommandLine.arguments[2]
                        )
                    )
                )
            } catch {
                throw AuditFailure.failed(
                    "pre-login UI consumer snapshot is invalid"
                )
            }
            return try validateUILoginGeneration(before: before, after: after)
        }
    } else if CommandLine.arguments.count == 8,
              CommandLine.arguments[1] == "verify-artifact"
    {
        let app = URL(
            fileURLWithPath: CommandLine.arguments[2],
            isDirectory: true
        )
        let expectedCodeSignature = try JSONDecoder().decode(
            CodeSignatureReport.self,
            from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[7]))
        )
        try verifyStrictCodeSignature(app: app)
        let report = try validateArtifactAuthority(
            app: app,
            expectedCommit: CommandLine.arguments[3],
            expectedBuildNumber: CommandLine.arguments[4],
            expectedBuildVariant: CommandLine.arguments[5],
            expectedExecutableSHA256: CommandLine.arguments[6],
            expectedCodeSignature: expectedCodeSignature,
            actualCodeSignature: codeSignatureReport(app: app)
        )
        FileHandle.standardOutput.write(try encoder.encode(report))
        FileHandle.standardOutput.write(Data("\n".utf8))
    } else if CommandLine.arguments.count == 11, CommandLine.arguments[1] == "audit" {
        guard let auditMode = AuditMode(rawValue: CommandLine.arguments[2]) else {
            throw AuditFailure.usage
        }
        let bundleIdentifier = CommandLine.arguments[3]
        let app = URL(fileURLWithPath: CommandLine.arguments[4], isDirectory: true)
        let sourceAssets = URL(
            fileURLWithPath: CommandLine.arguments[5],
            isDirectory: true
        )
        let expectedCodeSignature = try JSONDecoder().decode(
            CodeSignatureReport.self,
            from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[10]))
        )
        let report = try validateInstalledContract(
            app: app,
            bundleIdentifier: bundleIdentifier,
            sourceAssets: sourceAssets,
            expectedCommit: CommandLine.arguments[6],
            expectedBuildNumber: CommandLine.arguments[7],
            expectedBuildVariant: CommandLine.arguments[8],
            expectedExecutableSHA256: CommandLine.arguments[9],
            expectedCodeSignature: expectedCodeSignature,
            installedCodeSignature: codeSignatureReport(app: app),
            auditMode: auditMode,
            observations: observations(bundleIdentifier: bundleIdentifier),
            currentSource: currentSourceReport(),
            processes: processReports(bundleIdentifier: bundleIdentifier)
        )
        FileHandle.standardOutput.write(try encoder.encode(report))
        FileHandle.standardOutput.write(Data("\n".utf8))
    } else {
        throw AuditFailure.usage
    }
} catch {
    FileHandle.standardError.write(Data("installed mode icon audit failed: \(error)\n".utf8))
    exit(1)
}
