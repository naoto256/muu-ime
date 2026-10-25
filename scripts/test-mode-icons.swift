// Checks the committed icon assets, their sources and the plist wiring,
// without an installed IME:
// - the three authoritative artwork SVGs by hash, and their provenance README;
// - each asset's format, sizes, embedded sRGB profile (recomputed from the
//   standard matrices), palette and rendered appearance, with optional
//   preview sheets in MUU_MODE_ICON_PREVIEW_DIR;
// - the Info.plist input-mode dictionaries and Japanese strings, with
//   negative fixtures for each rule;
// - the Xcode project's single Resources membership for each asset;
// - optionally a built Release.app's copies, which must be byte-equal.
// `--generated-asset-sets A B` only checks that two generator runs produced
// the same eight files.
import AppKit
import ColorSync
import CoreFoundation
import CryptoKit
import Foundation
import ImageIO

private struct ExpectedMode {
    let identifier: String
    let suffix: String
    let glyph: String
    let localizedName: String
    let isDefault: Bool
}

private enum ExpectedModeMetadataValue {
    case string(String)
    case boolean(Bool)
    case stringDictionary([String: String])

    var wrongTypeFixture: Any {
        switch self {
        case .string:
            return false
        case .boolean:
            return "false"
        case .stringDictionary:
            return "not a dictionary"
        }
    }

    var driftFixture: Any {
        switch self {
        case let .string(value):
            return "\(value).drift"
        case let .boolean(value):
            return !value
        case let .stringDictionary(value):
            return value.merging(["Unexpected": "drift"]) { current, _ in current }
        }
    }

    func requireMatch(_ actual: Any?, key: String, context: String) throws {
        guard let actual else {
            throw TestFailure.failed("\(context).\(key) is missing")
        }
        switch self {
        case let .string(expected):
            try require(
                CFGetTypeID(actual as CFTypeRef) == CFStringGetTypeID(),
                "\(context).\(key) must be a string"
            )
            try require(
                actual as? String == expected,
                "\(context).\(key) changed"
            )
        case let .boolean(expected):
            try require(
                CFGetTypeID(actual as CFTypeRef) == CFBooleanGetTypeID(),
                "\(context).\(key) must be a boolean"
            )
            try require(
                actual as? Bool == expected,
                "\(context).\(key) changed"
            )
        case let .stringDictionary(expected):
            try require(
                CFGetTypeID(actual as CFTypeRef) == CFDictionaryGetTypeID(),
                "\(context).\(key) must be a dictionary"
            )
            try require(
                actual as? [String: String] == expected,
                "\(context).\(key) changed"
            )
        }
    }
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

private let removedPublicModeSuffixes = [
    "Katakana",
    "HalfWidthKana",
    "FullWidthRoman",
]

private func expectedModeMetadata(
    for mode: ExpectedMode,
    productBundleIdentifier: String
) -> [String: ExpectedModeMetadataValue] {
    [
        "TISIconIsTemplate": .boolean(true),
        "TISIconLabels": .stringDictionary(["Primary": mode.glyph]),
        "TISInputSourceID": .string("\(productBundleIdentifier).\(mode.suffix)"),
        "TISIntendedLanguage": .string("ja"),
        "tsInputModeAlternateMenuIconFileKey": .string(
            "MuuMode\(mode.suffix)Selected.tiff"
        ),
        "tsInputModeDefaultStateKey": .boolean(mode.isDefault),
        "tsInputModeIsVisibleKey": .boolean(true),
        "tsInputModeMenuIconFileKey": .string("MuuMode\(mode.suffix).tiff"),
        "tsInputModePaletteIconFileKey": .string(
            "MuuMode\(mode.suffix)Button.tiff"
        ),
        "tsInputModePrimaryInScriptKey": .boolean(mode.isDefault),
        "tsInputModeScriptKey": .string("smJapanese"),
    ]
}

private var modeSurfaceAssetNames: [String] {
    expectedModes.flatMap { mode in
        [
            "MuuMode\(mode.suffix).tiff",
            "MuuMode\(mode.suffix)Selected.tiff",
            "MuuMode\(mode.suffix)Button.tiff",
        ]
    }
}

private var generatedAssetNames: [String] {
    modeSurfaceAssetNames + [
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

// Pinned hashes. Changing the artwork means updating these together with the
// README's recorded hashes and the regenerated assets.
private let expectedArtworkSHA256 = [
    "MuuComponent.svg": "2f8cbf932ebd34b5013feb6a0d7e18ee206830c9dd936f3fa957f6c53a55b15e",
    "MuuHiragana.svg": "68688b5f270f0d19b9be04baadabbda119d3c6476316609860daad73d40c66f8",
    "MuuRoman.svg": "8a02a2d5dcfa1b748e407c66d013de9f7c8e59a22f5ef7c5af40a7e77e12cfa2",
]

private let expectedUnchangedComponentAssetSHA256 = [
    "MuuInputMethod.tiff": "0907afac0fc4d3fd2fb15aa0047949497eb6d0ea01b85697e4b1b73b4b64a597",
    "MuuIME.icns": "ab884a23cca0a252380b31abf5874a54c13dc5a7b3a897dc790f49e358570335",
]

// Hashes of the earlier placeholder artwork, which must never come back.
private let retiredCrudeAssetSHA256: Set<String> = [
    "88b7beaabdc9261472eae951f5b3cb4720d20383f5eabcad5517cd84916a38b5",
    "9e3ec2ea65dfb3506bae72cdcdb3078ceb4149bbf8f9d712626b9ea40bd0d455",
    "60fdb1c815c0ab66fba0862e44c9e782c5d4db6fe5ab6e16eb5e5bd00ec279c0",
    "332af7ab7bb5bec08531069148214d01593c41545212a5fd2db481bfbbad4b68",
]

private enum TestFailure: Error, CustomStringConvertible {
    case failed(String)

    var description: String {
        switch self {
        case let .failed(message):
            return message
        }
    }
}

private func require(_ condition: @autoclosure () -> Bool, _ message: String) throws {
    guard condition() else {
        throw TestFailure.failed(message)
    }
}

// Negative fixtures: the operation must throw; `message` is the failure if
// it was accepted instead.
private func requireFailure(_ message: String, _ operation: () throws -> Void) throws {
    do {
        try operation()
    } catch {
        return
    }
    throw TestFailure.failed(message)
}

private func sha256Hex(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

private func requireNotRetiredArtwork(_ hash: String, context: String) throws {
    try require(
        !retiredCrudeAssetSHA256.contains(hash),
        "\(context) still uses the retired fixed-stroke artwork"
    )
}

private func inspectArtworkSources(at directory: URL) throws {
    let filenames = try Set(FileManager.default.contentsOfDirectory(atPath: directory.path))
    let expectedFilenames = Set(expectedArtworkSHA256.keys).union(["README.md"])
    try require(
        filenames == expectedFilenames,
        "artwork source directory must contain exactly three SVG sources and its provenance README"
    )
    for (filename, expectedHash) in expectedArtworkSHA256 {
        let data = try Data(contentsOf: directory.appendingPathComponent(filename))
        try require(
            sha256Hex(data) == expectedHash,
            "\(filename) differs from the authoritative repository SVG"
        )
    }
    let readme = try String(
        contentsOf: directory.appendingPathComponent("README.md"),
        encoding: .utf8
    )
    try require(
        readme.contains("authoritative, repository-owned")
            && readme.contains("no third-party font outline")
            && expectedArtworkSHA256.values.allSatisfy(readme.contains),
        "artwork provenance or delivery hashes are incomplete"
    )
    for retiredHash in retiredCrudeAssetSHA256 {
        try requireFailure("retired crude artwork hash fixture was accepted") {
            try requireNotRetiredArtwork(retiredHash, context: "retired fixture")
        }
    }
}

private func dictionary(_ value: Any?, named name: String) throws -> [String: Any] {
    guard let dictionary = value as? [String: Any] else {
        throw TestFailure.failed("\(name) is not a dictionary")
    }
    return dictionary
}

private func requireJapaneseInfoPlistStrings(
    _ data: Data,
    context: String
) throws {
    let raw = try PropertyListSerialization.propertyList(
        from: data,
        options: [],
        format: nil
    )
    let strings = try dictionary(raw, named: "\(context) InfoPlist.strings")
    try require(
        strings as NSDictionary == expectedJapaneseInfoPlistStrings as NSDictionary,
        "\(context) Japanese display-name metadata changed"
    )
    for suffix in removedPublicModeSuffixes {
        try require(
            data.range(of: Data("com.apple.inputmethod.Japanese.\(suffix)".utf8)) == nil,
            "\(context) still localizes removed public mode \(suffix)"
        )
    }
}

private func inspectJapaneseInfoPlistStrings(
    at url: URL,
    context: String
) throws -> Data {
    let data = try Data(contentsOf: url)
    try requireJapaneseInfoPlistStrings(data, context: context)
    return data
}

private func requireExpectedAssets(
    available: Set<String>,
    expected: [String],
    context: String
) throws {
    let missing = Set(expected).subtracting(available)
    try require(missing.isEmpty, "\(context) is missing assets: \(missing.sorted())")
}

private func requireExactGeneratedAssets(at directory: URL) throws {
    let available = try Set(FileManager.default.contentsOfDirectory(atPath: directory.path))
    try require(
        generatedAssetNames.count == 8 && available == Set(generatedAssetNames),
        "\(directory.lastPathComponent) must contain exactly the 8 authoritative icon assets"
    )
}

// Contract: each generated directory holds exactly the 8 authoritative file
// names. Negatives: an extra (retired PDF) file, a wrong name while the count
// is still 8, and a missing file. The per-file cmp elsewhere cannot see
// extra files, and a count alone cannot see a renamed one.
private func inspectGeneratedAssetSets(_ directories: [URL]) throws {
    for directory in directories {
        try requireExactGeneratedAssets(at: directory)
    }
    let manager = FileManager.default
    let fixture = manager.temporaryDirectory.appendingPathComponent(
        "muu-generated-asset-set-\(UUID().uuidString)", isDirectory: true
    )
    try manager.createDirectory(at: fixture, withIntermediateDirectories: true)
    defer { try? manager.removeItem(at: fixture) }
    for name in generatedAssetNames {
        try Data().write(to: fixture.appendingPathComponent(name))
    }
    try requireExactGeneratedAssets(at: fixture)
    let extra = fixture.appendingPathComponent("MuuModeHiragana.pdf")
    try Data("obsolete PDF fixture".utf8).write(to: extra)
    try requireFailure("extra generated PDF fixture was accepted") {
        try requireExactGeneratedAssets(at: fixture)
    }
    try manager.removeItem(at: fixture.appendingPathComponent(generatedAssetNames[0]))
    try requireFailure("eight generated files with a wrong name were accepted") {
        try requireExactGeneratedAssets(at: fixture)
    }
    try manager.removeItem(at: extra)
    try requireFailure("missing generated asset fixture was accepted") {
        try requireExactGeneratedAssets(at: fixture)
    }
}

private func requireMatchingAssetBytes(
    source: Data,
    bundled: Data,
    name: String
) throws {
    try require(source == bundled, "\(name) differs between source and Release bundle")
}

private let legacyIconMetadataTerms = ["CustomIcon"]

private func requireNoLegacyIconMetadata(
    in data: Data,
    context: String
) throws {
    for term in legacyIconMetadataTerms {
        try require(
            data.range(of: Data(term.utf8)) == nil,
            "\(context) contains obsolete \(term) metadata"
        )
    }
    try require(
        data.range(of: Data(".pdf".utf8)) == nil,
        "\(context) contains an obsolete mode PDF reference"
    )
}

private func bitmapRepresentationSizes(at url: URL) throws -> Set<String> {
    let representations = (
        NSBitmapImageRep.imageReps(withContentsOf: url) ?? []
    ).compactMap { $0 as? NSBitmapImageRep }
    try require(!representations.isEmpty, "\(url.lastPathComponent) has no bitmap representations")
    return Set(representations.map { "\($0.pixelsWide)x\($0.pixelsHigh)" })
}

private func requireSurfaceAssetContract(
    base: Data,
    selected: Data,
    button: Data,
    mode: String
) throws {
    try require(
        base == selected && selected == button,
        "\(mode) Menu, Alternate, and Palette TIFFs must be byte-identical"
    )
}

private extension Data {
    mutating func replaceUInt16LE(at offset: Int, with value: UInt16) {
        replaceSubrange(offset ..< offset + 2, with: [
            UInt8(value & 0xff),
            UInt8((value >> 8) & 0xff),
        ])
    }

    mutating func replaceUInt32LE(at offset: Int, with value: UInt32) {
        replaceSubrange(offset ..< offset + 4, with: [
            UInt8(value & 0xff),
            UInt8((value >> 8) & 0xff),
            UInt8((value >> 16) & 0xff),
            UInt8((value >> 24) & 0xff),
        ])
    }

    mutating func replaceUInt32BE(at offset: Int, with value: UInt32) {
        replaceSubrange(offset ..< offset + 4, with: [
            UInt8((value >> 24) & 0xff),
            UInt8((value >> 16) & 0xff),
            UInt8((value >> 8) & 0xff),
            UInt8(value & 0xff),
        ])
    }

    func uint16LE(at offset: Int) throws -> UInt16 {
        try require(offset >= 0 && offset + 2 <= count, "binary UInt16 offset is out of bounds")
        return UInt16(self[offset]) | (UInt16(self[offset + 1]) << 8)
    }

    func uint32LE(at offset: Int) throws -> UInt32 {
        try require(offset >= 0 && offset + 4 <= count, "binary UInt32 offset is out of bounds")
        return UInt32(self[offset])
            | (UInt32(self[offset + 1]) << 8)
            | (UInt32(self[offset + 2]) << 16)
            | (UInt32(self[offset + 3]) << 24)
    }

    func uint32BE(at offset: Int) throws -> UInt32 {
        try require(offset >= 0 && offset + 4 <= count, "binary UInt32 offset is out of bounds")
        return (UInt32(self[offset]) << 24)
            | (UInt32(self[offset + 1]) << 16)
            | (UInt32(self[offset + 2]) << 8)
            | UInt32(self[offset + 3])
    }

    func uint16BE(at offset: Int) throws -> UInt16 {
        try require(offset >= 0 && offset + 2 <= count, "binary UInt16 offset is out of bounds")
        return (UInt16(self[offset]) << 8) | UInt16(self[offset + 1])
    }
}

private struct ICNSRepresentation: Equatable {
    let tag: String
    let logicalSize: Int
    let scale: Int
    let pixelsWide: Int
    let pixelsHigh: Int
}

private let expectedICNSSpecifications: [(tag: String, logicalSize: Int, scale: Int)] = [
    ("icp4", 16, 1),
    ("ic11", 16, 2),
    ("icp5", 32, 1),
    ("ic12", 32, 2),
    ("ic07", 128, 1),
    ("ic13", 128, 2),
    ("ic08", 256, 1),
    ("ic14", 256, 2),
]

private func parseICNSRepresentations(_ data: Data) throws -> [ICNSRepresentation] {
    try require(data.count >= 8, "ICNS header is truncated")
    try require(
        String(data: data[0 ..< 4], encoding: .ascii) == "icns",
        "ICNS magic changed"
    )
    let rootLength = Int(try data.uint32BE(at: 4))
    try require(rootLength == data.count, "ICNS root length changed")
    let specificationByTag = Dictionary(
        uniqueKeysWithValues: expectedICNSSpecifications.map { ($0.tag, $0) }
    )
    var representations: [ICNSRepresentation] = []
    var offset = 8
    while offset < data.count {
        try require(offset + 8 <= data.count, "ICNS chunk header is truncated")
        guard let tag = String(data: data[offset ..< offset + 4], encoding: .ascii) else {
            throw TestFailure.failed("ICNS chunk tag is not ASCII")
        }
        let length = Int(try data.uint32BE(at: offset + 4))
        try require(length >= 32 && offset + length <= data.count, "\(tag) chunk length is invalid")
        guard let specification = specificationByTag[tag] else {
            throw TestFailure.failed("unexpected ICNS representation tag \(tag)")
        }
        let pngOffset = offset + 8
        try require(
            data[pngOffset ..< pngOffset + 8] == Data([137, 80, 78, 71, 13, 10, 26, 10]),
            "\(tag) does not contain a PNG payload"
        )
        let pixelsWide = Int(try data.uint32BE(at: pngOffset + 16))
        let pixelsHigh = Int(try data.uint32BE(at: pngOffset + 20))
        representations.append(
            ICNSRepresentation(
                tag: tag,
                logicalSize: specification.logicalSize,
                scale: specification.scale,
                pixelsWide: pixelsWide,
                pixelsHigh: pixelsHigh
            )
        )
        offset += length
    }
    try require(offset == data.count, "ICNS chunks do not consume the container")
    return representations
}

private func filterICNS(_ data: Data, keepingTags: Set<String>) throws -> Data {
    var keptChunks = Data()
    var offset = 8
    while offset < data.count {
        try require(offset + 8 <= data.count, "ICNS fixture chunk header is truncated")
        guard let tag = String(data: data[offset ..< offset + 4], encoding: .ascii) else {
            throw TestFailure.failed("ICNS fixture tag is not ASCII")
        }
        let length = Int(try data.uint32BE(at: offset + 4))
        try require(length >= 8 && offset + length <= data.count, "ICNS fixture length is invalid")
        if keepingTags.contains(tag) {
            keptChunks.append(data.subdata(in: offset ..< offset + length))
        }
        offset += length
    }
    var result = Data("icns".utf8)
    result.append(Data(repeating: 0, count: 4))
    result.append(keptChunks)
    result.replaceUInt32BE(at: 4, with: UInt32(result.count))
    return result
}

private func requireICNSRetinaContract(
    _ representations: [ICNSRepresentation],
    context: String
) throws {
    let expectedKeys = Set(
        expectedICNSSpecifications.map { "\($0.tag):\($0.logicalSize):\($0.scale)" }
    )
    let actualKeys = representations.map {
        "\($0.tag):\($0.logicalSize):\($0.scale)"
    }
    try require(
        Set(actualKeys) == expectedKeys && actualKeys.count == expectedKeys.count,
        "\(context) must contain each standard 1x/2x ICNS representation exactly once"
    )
    let logicalScaleKeys = representations.map { "\($0.logicalSize)@\($0.scale)x" }
    try require(
        Set(logicalScaleKeys).count == logicalScaleKeys.count,
        "\(context) contains a duplicate logical-size/scale representation"
    )
    for representation in representations {
        try require(
            representation.pixelsWide == representation.logicalSize * representation.scale
                && representation.pixelsHigh
                    == representation.logicalSize * representation.scale,
            "\(context) \(representation.tag) has an incorrect tag/scale pixel mapping"
        )
    }
}

private struct ICCXYZ {
    let x: Double
    let y: Double
    let z: Double

    static func + (lhs: ICCXYZ, rhs: ICCXYZ) -> ICCXYZ {
        ICCXYZ(x: lhs.x + rhs.x, y: lhs.y + rhs.y, z: lhs.z + rhs.z)
    }
}

private let iccD50 = ICCXYZ(x: 0.9642, y: 1.0, z: 0.82491)
private let iccD65 = ICCXYZ(x: 0.950455, y: 1.0, z: 1.08905)
private let iccRedColorant = ICCXYZ(x: 0.4360747, y: 0.2225045, z: 0.0139322)
private let iccGreenColorant = ICCXYZ(x: 0.3850649, y: 0.7168786, z: 0.0971045)
private let iccBlueColorant = ICCXYZ(x: 0.1430804, y: 0.0606169, z: 0.7141733)
private let iccBradfordD65ToD50 = [
    1.0478112, 0.0228866, -0.0501270,
    0.0295424, 0.9904844, -0.0170491,
    -0.0092345, 0.0150436, 0.7521316,
]

private func fixed16Bits(_ value: Double) -> UInt32 {
    UInt32(bitPattern: Int32((value * 65_536.0).rounded()))
}

private func fixed16Value(in data: Data, at offset: Int) throws -> Double {
    Double(Int32(bitPattern: try data.uint32BE(at: offset))) / 65_536.0
}

private func requireApprox(
    _ actual: Double,
    _ expected: Double,
    _ message: String,
    tolerance: Double = 1.0 / 65_536.0
) throws {
    try require(abs(actual - expected) <= tolerance, "\(message): \(actual) != \(expected)")
}

private func requireXYZ(
    _ actual: ICCXYZ,
    _ expected: ICCXYZ,
    tolerance: Double = 1.0 / 65_536.0,
    mode: String,
    name: String
) throws {
    try requireApprox(actual.x, expected.x, "\(mode) \(name).X", tolerance: tolerance)
    try requireApprox(actual.y, expected.y, "\(mode) \(name).Y", tolerance: tolerance)
    try requireApprox(actual.z, expected.z, "\(mode) \(name).Z", tolerance: tolerance)
}

private func iccTagRange(
    _ signature: String,
    in profile: Data,
    mode: String
) throws -> Range<Int> {
    try require(signature.utf8.count == 4, "ICC tag signature must be four bytes")
    let count = Int(try profile.uint32BE(at: 128))
    try require(
        count >= 1 && 132 + count * 12 <= profile.count,
        "\(mode) ICC tag table is out of bounds"
    )
    let signatureData = Data(signature.utf8)
    var matches: [Range<Int>] = []
    for index in 0 ..< count {
        let entry = 132 + index * 12
        guard profile.subdata(in: entry ..< entry + 4) == signatureData else {
            continue
        }
        let offset = Int(try profile.uint32BE(at: entry + 4))
        let size = Int(try profile.uint32BE(at: entry + 8))
        try require(
            offset >= 0 && size >= 0 && offset <= profile.count - size,
            "\(mode) ICC \(signature) payload is out of bounds"
        )
        matches.append(offset ..< offset + size)
    }
    try require(matches.count == 1, "\(mode) ICC must contain one \(signature) tag")
    return matches[0]
}

private func iccXYZ(
    _ signature: String,
    in profile: Data,
    mode: String
) throws -> ICCXYZ {
    let range = try iccTagRange(signature, in: profile, mode: mode)
    try require(range.count == 20, "\(mode) ICC \(signature) must be one XYZ value")
    try require(
        profile.subdata(in: range.lowerBound ..< range.lowerBound + 4) == Data("XYZ ".utf8),
        "\(mode) ICC \(signature) has the wrong payload type"
    )
    return ICCXYZ(
        x: try fixed16Value(in: profile, at: range.lowerBound + 8),
        y: try fixed16Value(in: profile, at: range.lowerBound + 12),
        z: try fixed16Value(in: profile, at: range.lowerBound + 16)
    )
}

private func iccFixedArray(
    _ signature: String,
    count: Int,
    in profile: Data,
    mode: String
) throws -> [Double] {
    let range = try iccTagRange(signature, in: profile, mode: mode)
    try require(range.count == 8 + count * 4, "\(mode) ICC \(signature) has the wrong length")
    try require(
        profile.subdata(in: range.lowerBound ..< range.lowerBound + 4) == Data("sf32".utf8),
        "\(mode) ICC \(signature) has the wrong payload type"
    )
    return try (0 ..< count).map {
        try fixed16Value(in: profile, at: range.lowerBound + 8 + $0 * 4)
    }
}

private func iccCurve(
    _ signature: String,
    in profile: Data,
    mode: String
) throws -> [UInt16] {
    let range = try iccTagRange(signature, in: profile, mode: mode)
    try require(
        profile.subdata(in: range.lowerBound ..< range.lowerBound + 4) == Data("curv".utf8),
        "\(mode) ICC \(signature) has the wrong payload type"
    )
    let count = Int(try profile.uint32BE(at: range.lowerBound + 8))
    try require(range.count == 12 + count * 2, "\(mode) ICC \(signature) has the wrong length")
    return try (0 ..< count).map {
        try profile.uint16BE(at: range.lowerBound + 12 + $0 * 2)
    }
}

private func multiply(matrix: [Double], vector: ICCXYZ) -> ICCXYZ {
    precondition(matrix.count == 9)
    return ICCXYZ(
        x: matrix[0] * vector.x + matrix[1] * vector.y + matrix[2] * vector.z,
        y: matrix[3] * vector.x + matrix[4] * vector.y + matrix[5] * vector.z,
        z: matrix[6] * vector.x + matrix[7] * vector.y + matrix[8] * vector.z
    )
}

private func firstTIFFICCEntry(in data: Data, mode: String) throws -> Int {
    let ifdOffset = Int(try data.uint32LE(at: 4))
    let entryCount = Int(try data.uint16LE(at: ifdOffset))
    for entryIndex in 0 ..< entryCount {
        let entry = ifdOffset + 2 + entryIndex * 12
        if try data.uint16LE(at: entry) == 34_675 {
            return entry
        }
    }
    throw TestFailure.failed("\(mode) TIFF first IFD has no ICC entry")
}

private func colorComponents(_ color: NSColor) -> (red: CGFloat, green: CGFloat, blue: CGFloat, alpha: CGFloat)? {
    guard let converted = color.usingColorSpace(.sRGB) else {
        return nil
    }
    return (converted.redComponent, converted.greenComponent, converted.blueComponent, converted.alphaComponent)
}

private func isOrange(_ color: NSColor) -> Bool {
    guard let value = colorComponents(color) else {
        return false
    }
    return value.red > 0.8
        && value.green > 0.35
        && value.green < 0.75
        && value.blue < 0.35
        && value.alpha > 0.98
}

private func isWhite(_ color: NSColor) -> Bool {
    guard let value = colorComponents(color) else {
        return false
    }
    return value.red > 0.98 && value.green > 0.98 && value.blue > 0.98 && value.alpha > 0.98
}

private struct RGBA8 {
    let red: Int
    let green: Int
    let blue: Int
    let alpha: Int
}

private func managedSRGB8(
    in representation: NSBitmapImageRep,
    x: Int,
    y: Int,
    sourceColorSpace: NSColorSpace?
) -> RGBA8? {
    let sourceColor: NSColor
    if let rawPixels = representation.bitmapData,
       representation.samplesPerPixel == 4
    {
        let offset = y * representation.bytesPerRow + x * representation.samplesPerPixel
        guard let sourceColorSpace else {
            // render() owns an explicit sRGB CGContext, so these bytes are
            // already the managed destination values. NSBitmapImageRep's
            // colorAt(_:y:) retags them as Generic RGB and would convert twice.
            return RGBA8(
                red: Int(rawPixels[offset]),
                green: Int(rawPixels[offset + 1]),
                blue: Int(rawPixels[offset + 2]),
                alpha: Int(rawPixels[offset + 3])
            )
        }
        sourceColor = NSColor(
            colorSpace: sourceColorSpace,
            components: [
                CGFloat(rawPixels[offset]) / 255.0,
                CGFloat(rawPixels[offset + 1]) / 255.0,
                CGFloat(rawPixels[offset + 2]) / 255.0,
                CGFloat(rawPixels[offset + 3]) / 255.0,
            ],
            count: 4
        )
    } else {
        guard let color = representation.colorAt(x: x, y: y) else {
            return nil
        }
        sourceColor = color
    }
    guard let value = colorComponents(sourceColor) else {
        return nil
    }
    return RGBA8(
        red: Int((value.red * 255.0).rounded()),
        green: Int((value.green * 255.0).rounded()),
        blue: Int((value.blue * 255.0).rounded()),
        alpha: Int((value.alpha * 255.0).rounded())
    )
}

private func requireManagedSRGBPalette(
    _ representation: NSBitmapImageRep,
    mode: String,
    asset: String,
    sourceColorSpace: NSColorSpace? = nil
) throws {
    var managedOrangePixels = 0
    var managedWhitePixels = 0
    for y in 0 ..< representation.pixelsHigh {
        for x in 0 ..< representation.pixelsWide {
            guard let managed = managedSRGB8(
                in: representation,
                x: x,
                y: y,
                sourceColorSpace: sourceColorSpace
            ),
                  managed.alpha >= 254
            else {
                continue
            }
            if abs(managed.red - 242) <= 1,
               abs(managed.green - 140) <= 1,
               abs(managed.blue - 40) <= 1
            {
                managedOrangePixels += 1
            }
            if abs(managed.red - 255) <= 1,
               abs(managed.green - 255) <= 1,
               abs(managed.blue - 255) <= 1
            {
                managedWhitePixels += 1
            }
        }
    }
    let scale = representation.pixelsWide / 16
    try require(
        managedOrangePixels >= 40 * scale * scale,
        "\(mode) \(asset) managed sRGB orange is not #F28C28 at \(representation.pixelsWide)px"
    )
    try require(
        managedWhitePixels >= 5 * scale * scale,
        "\(mode) \(asset) managed sRGB white is not #FFFFFF at \(representation.pixelsWide)px"
    )
}

private func inspectBitmap(
    _ representation: NSBitmapImageRep,
    sourceColorSpace: NSColorSpace,
    logicalSize: NSSize,
    mode: String
) throws {
    let width = representation.pixelsWide
    let height = representation.pixelsHigh
    let scale = height / Int(logicalSize.height)
    try require(scale == 1 || scale == 2, "\(mode) TIFF has an unexpected \(height)px height")
    try require(
        width == Int(logicalSize.width) * scale,
        "\(mode) TIFF representation does not preserve its logical aspect ratio"
    )
    try require(
        representation.size == logicalSize,
        "\(mode) TIFF representation does not declare \(logicalSize) logical size"
    )

    guard let corner = representation.colorAt(x: 0, y: 0),
          let cornerComponents = colorComponents(corner)
    else {
        throw TestFailure.failed("\(mode) TIFF corner could not be sampled")
    }
    try require(cornerComponents.alpha < 0.05, "\(mode) TIFF has no transparent outer corner")

    var orangePixels = 0
    var whitePixels = 0
    var exactSRGBOrangePixels = 0
    let rawPixels = representation.bitmapData
    let bytesPerRow = representation.bytesPerRow
    let samplesPerPixel = representation.samplesPerPixel
    for y in 0 ..< height {
        for x in 0 ..< width {
            guard let color = representation.colorAt(x: x, y: y) else {
                continue
            }
            if isOrange(color) {
                orangePixels += 1
            }
            if isWhite(color) {
                whitePixels += 1
            }
            if let rawPixels, samplesPerPixel == 4 {
                let offset = y * bytesPerRow + x * samplesPerPixel
                if rawPixels[offset] == 242,
                   rawPixels[offset + 1] == 140,
                   rawPixels[offset + 2] == 40,
                   rawPixels[offset + 3] == 255
                {
                    exactSRGBOrangePixels += 1
                }
            }
        }
    }
    try require(
        orangePixels >= 65 * scale * scale,
        "\(mode) orange background is too small at \(width)px (\(orangePixels) pixels)"
    )
    try require(
        whitePixels >= 5 * scale * scale,
        "\(mode) white glyph is not legible at \(width)px (\(whitePixels) pixels)"
    )
    try require(
        exactSRGBOrangePixels >= 40 * scale * scale,
        "\(mode) has no exact sRGB #F28C28 background pixels at \(width)px"
    )
    try requireManagedSRGBPalette(
        representation,
        mode: mode,
        asset: "TIFF",
        sourceColorSpace: sourceColorSpace
    )
}

private func inspectTemplateMaskBitmap(
    _ representation: NSBitmapImageRep,
    sourceColorSpace: NSColorSpace,
    logicalSize: NSSize,
    mode: String
) throws -> GlyphMask {
    let width = representation.pixelsWide
    let height = representation.pixelsHigh
    let scale = height / Int(logicalSize.height)
    try require(scale == 1 || scale == 2, "\(mode) has an unexpected \(height)px height")
    try require(
        width == Int(logicalSize.width) * scale && representation.size == logicalSize,
        "\(mode) does not preserve its 22x16 logical size"
    )
    guard let pixels = representation.bitmapData, representation.samplesPerPixel == 4 else {
        throw TestFailure.failed("\(mode) does not expose RGBA pixels")
    }

    var transparentPixels = 0
    var nontransparentPixels = 0
    var opaqueBlackPixels = 0
    var orangePixels = 0
    var nonblackVisiblePixels = 0
    for y in 0 ..< height {
        for x in 0 ..< width {
            let offset = y * representation.bytesPerRow + x * representation.samplesPerPixel
            let red = pixels[offset]
            let green = pixels[offset + 1]
            let blue = pixels[offset + 2]
            let alpha = pixels[offset + 3]
            if alpha == 0 {
                transparentPixels += 1
                try require(
                    red == 0 && green == 0 && blue == 0,
                    "\(mode) transparent RGB bytes are not canonical zero"
                )
                continue
            }
            nontransparentPixels += 1
            if red != 0 || green != 0 || blue != 0 {
                nonblackVisiblePixels += 1
            }
            if alpha == 255 {
                opaqueBlackPixels += 1
            }
            if red == 242 && green == 140 && blue == 40 {
                orangePixels += 1
            }
        }
    }
    try require(transparentPixels > 0, "\(mode) has no transparent background")
    try require(nontransparentPixels >= 5 * scale * scale, "\(mode) glyph is empty")
    try require(opaqueBlackPixels >= 5 * scale * scale, "\(mode) glyph is not legible")
    try require(orangePixels == 0, "\(mode) still contains orange tile pixels")
    try require(
        nonblackVisiblePixels == 0,
        "\(mode) contains a non-black visible pixel"
    )

    var managedBlackPixels = 0
    for y in 0 ..< height {
        for x in 0 ..< width {
            guard let managed = managedSRGB8(
                in: representation,
                x: x,
                y: y,
                sourceColorSpace: sourceColorSpace
            ), managed.alpha >= 254 else {
                continue
            }
            try require(
                managed.red <= 1 && managed.green <= 1 && managed.blue <= 1,
                "\(mode) managed color is not sRGB black"
            )
            managedBlackPixels += 1
        }
    }
    try require(
        managedBlackPixels >= 5 * scale * scale,
        "\(mode) managed sRGB black glyph is not legible"
    )
    return try glyphMask(from: representation)
}

private struct GlyphMask {
    let width: Int
    let height: Int
    let pixels: Set<Int>

    var boundingWidth: Int {
        let xValues = pixels.map { $0 % width }
        guard let minimum = xValues.min(), let maximum = xValues.max() else {
            return 0
        }
        return maximum - minimum + 1
    }
}

private func glyphMask(from representation: NSBitmapImageRep) throws -> GlyphMask {
    guard let data = representation.bitmapData, representation.samplesPerPixel == 4 else {
        throw TestFailure.failed("TIFF representation does not expose RGBA pixels")
    }
    var pixels: Set<Int> = []
    for y in 0 ..< representation.pixelsHigh {
        for x in 0 ..< representation.pixelsWide {
            let offset = y * representation.bytesPerRow + x * representation.samplesPerPixel
            if data[offset + 3] > 250 {
                pixels.insert(y * representation.pixelsWide + x)
            }
        }
    }
    return GlyphMask(
        width: representation.pixelsWide,
        height: representation.pixelsHigh,
        pixels: pixels
    )
}

private func requireDistinctModePair(
    first: GlyphMask,
    second: GlyphMask,
    names: String
) throws {
    try require(
        first.width == second.width && first.height == second.height,
        "\(names) masks have different canvases"
    )
    let scale = first.width / 16
    try require(
        abs(first.boundingWidth - second.boundingWidth) >= scale,
        "\(names) are not visibly different in glyph width at \(first.width)px"
    )
    let symmetricDifference = first.pixels.symmetricDifference(second.pixels).count
    try require(
        symmetricDifference >= 24 * scale * scale,
        "\(names) rendered pixel difference is too small at \(first.width)px"
    )
    let intersection = first.pixels.intersection(second.pixels).count
    let union = first.pixels.union(second.pixels).count
    let overlap = union == 0 ? 1.0 : Double(intersection) / Double(union)
    try require(overlap <= 0.5, "\(names) rendered shapes overlap too closely at \(first.width)px")
}

private func extractTIFFProfiles(from data: Data, mode: String) throws -> [Data] {
    try require(data.count >= 8, "\(mode) TIFF is truncated")
    try require(data.prefix(2) == Data("II".utf8), "\(mode) TIFF must use deterministic little endian")
    let magic = try data.uint16LE(at: 2)
    try require(magic == 42, "\(mode) TIFF magic is invalid")

    var profiles: [Data] = []
    var ifdOffset = Int(try data.uint32LE(at: 4))
    var visited: Set<Int> = []
    while ifdOffset != 0 {
        try require(visited.insert(ifdOffset).inserted, "\(mode) TIFF IFD chain contains a cycle")
        let entryCount = Int(try data.uint16LE(at: ifdOffset))
        let nextOffsetLocation = ifdOffset + 2 + entryCount * 12
        try require(nextOffsetLocation + 4 <= data.count, "\(mode) TIFF IFD is truncated")
        var foundProfile = false
        for entryIndex in 0 ..< entryCount {
            let entry = ifdOffset + 2 + entryIndex * 12
            let tag = try data.uint16LE(at: entry)
            guard tag == 34_675 else {
                continue
            }
            let entryType = try data.uint16LE(at: entry + 2)
            try require(entryType == 7, "\(mode) ICC tag has the wrong type")
            let length = Int(try data.uint32LE(at: entry + 4))
            let offset = Int(try data.uint32LE(at: entry + 8))
            try require(length > 128 && offset + length <= data.count, "\(mode) ICC tag is truncated")
            profiles.append(data.subdata(in: offset ..< offset + length))
            foundProfile = true
        }
        try require(foundProfile, "\(mode) TIFF representation has no embedded ICC profile")
        ifdOffset = Int(try data.uint32LE(at: nextOffsetLocation))
    }
    try require(profiles.count == 2, "\(mode) TIFF must embed ICC in both representations")
    return profiles
}

private func inspectSRGBProfile(_ profile: Data, mode: String) throws {
    let declaredSize = try profile.uint32BE(at: 0)
    try require(Int(declaredSize) == profile.count, "\(mode) ICC size header is wrong")
    try require(
        profile.subdata(in: 36 ..< 40) == Data("acsp".utf8),
        "\(mode) ICC signature is wrong"
    )
    try require(
        profile.range(of: Data("Muu sRGB".utf8)) != nil,
        "\(mode) ICC does not identify the deterministic Muu sRGB profile"
    )
    try require(
        profile.subdata(in: 12 ..< 16) == Data("mntr".utf8),
        "\(mode) ICC must be a display profile"
    )
    try require(
        profile.subdata(in: 20 ..< 24) == Data("XYZ ".utf8),
        "\(mode) ICC must use the XYZ profile connection space"
    )

    let headerIlluminant = ICCXYZ(
        x: try fixed16Value(in: profile, at: 68),
        y: try fixed16Value(in: profile, at: 72),
        z: try fixed16Value(in: profile, at: 76)
    )
    try requireXYZ(headerIlluminant, iccD50, mode: mode, name: "header illuminant")
    let mediaWhitePoint = try iccXYZ("wtpt", in: profile, mode: mode)
    try requireXYZ(mediaWhitePoint, iccD50, mode: mode, name: "media white point")

    let red = try iccXYZ("rXYZ", in: profile, mode: mode)
    let green = try iccXYZ("gXYZ", in: profile, mode: mode)
    let blue = try iccXYZ("bXYZ", in: profile, mode: mode)
    try requireXYZ(red, iccRedColorant, mode: mode, name: "red colorant")
    try requireXYZ(green, iccGreenColorant, mode: mode, name: "green colorant")
    try requireXYZ(blue, iccBlueColorant, mode: mode, name: "blue colorant")

    let chad = try iccFixedArray("chad", count: 9, in: profile, mode: mode)
    for (index, expected) in iccBradfordD65ToD50.enumerated() {
        try requireApprox(chad[index], expected, "\(mode) chad[\(index)]")
    }
    let adaptedWhite = multiply(matrix: chad, vector: iccD65)
    let colorantWhite = red + green + blue
    try requireXYZ(
        adaptedWhite,
        colorantWhite,
        tolerance: 5.0e-4,
        mode: mode,
        name: "D65 adaptation/colorant white consistency"
    )
    try requireXYZ(
        colorantWhite,
        mediaWhitePoint,
        tolerance: 5.0e-4,
        mode: mode,
        name: "colorant/media white consistency"
    )

    let redCurve = try iccCurve("rTRC", in: profile, mode: mode)
    let greenCurve = try iccCurve("gTRC", in: profile, mode: mode)
    let blueCurve = try iccCurve("bTRC", in: profile, mode: mode)
    try require(redCurve == greenCurve && greenCurve == blueCurve, "\(mode) ICC RGB TRCs differ")
    try require(redCurve.count == 256, "\(mode) ICC sRGB TRC must have 256 samples")
    for (index, sample) in redCurve.enumerated() {
        let encoded = Double(index) / 255.0
        let linear = encoded <= 0.04045
            ? encoded / 12.92
            : pow((encoded + 0.055) / 1.055, 2.4)
        let expected = UInt16((linear * 65_535.0).rounded())
        try require(
            abs(Int(sample) - Int(expected)) <= 1,
            "\(mode) ICC sRGB TRC sample \(index) is inconsistent"
        )
    }

    var creationError: Unmanaged<CFError>?
    guard let unmanagedProfile = ColorSyncProfileCreate(profile as CFData, &creationError) else {
        let detail = creationError?.takeRetainedValue().localizedDescription ?? "unknown error"
        throw TestFailure.failed("\(mode) ICC cannot be opened by ColorSync: \(detail)")
    }
    let colorSyncProfile = unmanagedProfile.takeRetainedValue()
    var verificationErrors: Unmanaged<CFError>?
    var verificationWarnings: Unmanaged<CFError>?
    try require(
        ColorSyncProfileVerify(
            colorSyncProfile,
            &verificationErrors,
            &verificationWarnings
        ),
        "\(mode) ICC failed ColorSync verification"
    )
    try require(
        verificationErrors == nil && verificationWarnings == nil,
        "\(mode) ICC produced ColorSync errors or warnings"
    )
}

// `launch` lets the self-test below run a fake sips; production uses `run()`.
private func inspectSIPSProfile(
    at url: URL, mode: String, launch: (Process) throws -> Void = { try $0.run() }
) throws {
    let process = Process()
    let output = Pipe()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/sips")
    process.arguments = ["-g", "profile", url.path]
    process.standardOutput = output
    process.standardError = output
    try launch(process)
    // Both streams share the pipe; drain it before waiting for the child.
    let text = String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    process.waitUntilExit()
    try require(process.terminationStatus == 0, "\(mode) sips profile inspection failed")
    try require(text.contains("profile: Muu sRGB"), "\(mode) is not recognized as Muu sRGB by ColorSync")
}

// The audit script's large-output check, applied to sips. A fake writes
// 128 KiB to stdout and to stderr, past the pipe buffer, before the profile
// line. Waiting before draining would block the child; the 30 s guard turns
// that into a failure instead of a hang, and the profile line written last
// must still be found.
private func validateSIPSOutputDraining() throws {
    let manager = FileManager.default
    let root = manager.temporaryDirectory.appendingPathComponent("muu-sips-output-\(UUID().uuidString)")
    try manager.createDirectory(at: root, withIntermediateDirectories: true,
                                attributes: [.posixPermissions: 0o700])
    defer { try? manager.removeItem(at: root) }
    let tool = root.appendingPathComponent("fake-sips")
    let padding = String(repeating: "x", count: 128 * 1024)
    let script = """
    #!/bin/bash
    printf '%s\\n' '\(padding)'
    printf '%s\\n' '\(padding)' >&2
    printf '%s\\n' 'profile: Muu sRGB'
    """
    try Data(script.utf8).write(to: tool)
    try manager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: tool.path)
    var guardTask: DispatchWorkItem?
    defer { guardTask?.cancel() }
    try inspectSIPSProfile(at: root.appendingPathComponent("fixture.tiff"), mode: "large-output fixture") {
        process in
        process.executableURL = tool
        process.arguments = []
        try process.run()
        // A hang guard for this private fake, not a performance assertion.
        let task = DispatchWorkItem { [weak process] in
            if let process, process.isRunning { process.terminate() }
        }
        guardTask = task
        DispatchQueue.global().asyncAfter(deadline: .now() + 30, execute: task)
    }
    print("large-output sips fixture passed")
}

private func render(
    image: NSImage,
    logicalSize: NSSize,
    scale: Int,
    background: NSColor
) throws -> NSBitmapImageRep {
    let pixelWidth = Int(logicalSize.width) * scale
    let pixelHeight = Int(logicalSize.height) * scale
    guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
          let bitmapContext = CGContext(
              data: nil,
              width: pixelWidth,
              height: pixelHeight,
              bitsPerComponent: 8,
              bytesPerRow: 0,
              space: colorSpace,
              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
          )
    else {
        throw TestFailure.failed("could not create the \(scale)x rendering context")
    }
    bitmapContext.scaleBy(x: CGFloat(scale), y: CGFloat(scale))
    let context = NSGraphicsContext(cgContext: bitmapContext, flipped: false)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = context
    background.setFill()
    NSRect(origin: .zero, size: logicalSize).fill()
    image.draw(
        in: NSRect(origin: .zero, size: logicalSize),
        from: .zero,
        operation: .sourceOver,
        fraction: 1
    )
    context.flushGraphics()
    NSGraphicsContext.restoreGraphicsState()
    guard let cgImage = bitmapContext.makeImage() else {
        throw TestFailure.failed("could not read the \(scale)x sRGB rendering context")
    }
    let representation = NSBitmapImageRep(cgImage: cgImage)
    representation.size = logicalSize
    return representation
}

private func renderTemplateMask(
    image: NSImage,
    logicalSize: NSSize,
    scale: Int,
    foreground: NSColor,
    background: NSColor
) throws -> NSBitmapImageRep {
    let pixelWidth = Int(logicalSize.width) * scale
    let pixelHeight = Int(logicalSize.height) * scale
    guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
          let bitmapContext = CGContext(
              data: nil,
              width: pixelWidth,
              height: pixelHeight,
              bitsPerComponent: 8,
              bytesPerRow: 0,
              space: colorSpace,
              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
          )
    else {
        throw TestFailure.failed("could not create the \(scale)x template preview context")
    }
    bitmapContext.scaleBy(x: CGFloat(scale), y: CGFloat(scale))
    let context = NSGraphicsContext(cgContext: bitmapContext, flipped: false)
    let bounds = NSRect(origin: .zero, size: logicalSize)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = context
    NSColor.clear.setFill()
    bounds.fill(using: .copy)
    image.draw(in: bounds, from: .zero, operation: .sourceOver, fraction: 1)
    foreground.setFill()
    bounds.fill(using: .sourceIn)
    background.setFill()
    bounds.fill(using: .destinationOver)
    context.flushGraphics()
    NSGraphicsContext.restoreGraphicsState()
    guard let cgImage = bitmapContext.makeImage() else {
        throw TestFailure.failed("could not read the \(scale)x template preview context")
    }
    let representation = NSBitmapImageRep(cgImage: cgImage)
    representation.size = logicalSize
    return representation
}

private func inspectRenderedImage(_ representation: NSBitmapImageRep, mode: String, appearance: String) throws {
    var orangePixels = 0
    var whitePixels = 0
    for y in 0 ..< representation.pixelsHigh {
        for x in 0 ..< representation.pixelsWide {
            guard let color = representation.colorAt(x: x, y: y) else {
                continue
            }
            if isOrange(color) {
                orangePixels += 1
            }
            if isWhite(color) {
                whitePixels += 1
            }
        }
    }
    try require(orangePixels > 0, "\(mode) loses its orange background on \(appearance)")
    try require(whitePixels > 0, "\(mode) loses its white glyph on \(appearance)")
}

private struct Preview {
    let column: Int
    let row: Int
    let image: NSBitmapImageRep
}

private func writePreviewSheet(
    _ previews: [Preview],
    rowCount: Int,
    filename: String,
    to directory: URL
) throws {
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    let cellSize = 128
    guard let sheet = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: cellSize * expectedModes.count,
        pixelsHigh: cellSize * rowCount,
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bitmapFormat: [],
        bytesPerRow: 0,
        bitsPerPixel: 0
    ), let context = NSGraphicsContext(bitmapImageRep: sheet) else {
        throw TestFailure.failed("could not create mode icon preview sheet")
    }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = context
    context.imageInterpolation = .none
    for preview in previews {
        let image = NSImage(size: NSSize(width: 22, height: 16))
        image.addRepresentation(preview.image)
        image.draw(
            in: NSRect(
                x: preview.column * cellSize,
                y: (rowCount - 1 - preview.row) * cellSize,
                width: cellSize,
                height: cellSize
            ),
            from: .zero,
            operation: .copy,
            fraction: 1
        )
    }
    context.flushGraphics()
    NSGraphicsContext.restoreGraphicsState()
    guard let png = sheet.representation(using: .png, properties: [:]) else {
        throw TestFailure.failed("could not encode mode icon preview sheet")
    }
    try png.write(to: directory.appendingPathComponent(filename))
}

private func writeComponentPreview(
    image: NSImage,
    to directory: URL
) throws {
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    let settings = [
        (scale: 1, background: NSColor.white, appearance: "Light 1x"),
        (scale: 2, background: NSColor.white, appearance: "Light 2x"),
        (scale: 1, background: NSColor.black, appearance: "Dark 1x"),
        (scale: 2, background: NSColor.black, appearance: "Dark 2x"),
    ]
    let rendered = try settings.map { setting in
        let representation = try render(
            image: image,
            logicalSize: NSSize(width: 16, height: 16),
            scale: setting.scale,
            background: setting.background
        )
        try inspectRenderedImage(
            representation,
            mode: "InputMethod ICNS",
            appearance: setting.appearance
        )
        return representation
    }
    let cellSize = 128
    guard let sheet = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: cellSize * rendered.count,
        pixelsHigh: cellSize,
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bitmapFormat: [],
        bytesPerRow: 0,
        bitsPerPixel: 0
    ), let context = NSGraphicsContext(bitmapImageRep: sheet) else {
        throw TestFailure.failed("could not create component icon preview sheet")
    }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = context
    context.imageInterpolation = .none
    for (index, representation) in rendered.enumerated() {
        let preview = NSImage(size: NSSize(width: 16, height: 16))
        preview.addRepresentation(representation)
        preview.draw(
            in: NSRect(
                x: index * cellSize,
                y: 0,
                width: cellSize,
                height: cellSize
            ),
            from: .zero,
            operation: .copy,
            fraction: 1
        )
    }
    context.flushGraphics()
    NSGraphicsContext.restoreGraphicsState()
    guard let png = sheet.representation(using: .png, properties: [:]) else {
        throw TestFailure.failed("could not encode component icon preview sheet")
    }
    try png.write(
        to: directory.appendingPathComponent("component-icon-light-dark-1x-2x.png")
    )
}

private func inspectAssets(at directory: URL, previewDirectory: URL?) throws {
    var previews: [Preview] = []
    var adaptivePreviews: [Preview] = []
    var masks: [String: [Int: GlyphMask]] = [:]
    for (modeIndex, mode) in expectedModes.enumerated() {
        let tiffURL = directory.appendingPathComponent("MuuMode\(mode.suffix).tiff")
        let selectedTIFFURL = directory.appendingPathComponent(
            "MuuMode\(mode.suffix)Selected.tiff"
        )
        let buttonTIFFURL = directory.appendingPathComponent(
            "MuuMode\(mode.suffix)Button.tiff"
        )
        let tiffData = try Data(contentsOf: tiffURL)
        let selectedTIFFData = try Data(contentsOf: selectedTIFFURL)
        let buttonTIFFData = try Data(contentsOf: buttonTIFFURL)
        try requireNotRetiredArtwork(
            sha256Hex(tiffData),
            context: "\(mode.suffix) mode asset"
        )
        try requireSurfaceAssetContract(
            base: tiffData,
            selected: selectedTIFFData,
            button: buttonTIFFData,
            mode: mode.suffix
        )
        if modeIndex == 0 {
            var driftedAlias = selectedTIFFData
            driftedAlias[driftedAlias.startIndex] ^= 0x01
            try requireFailure("mode surface byte-drift fixture was accepted") {
                try requireSurfaceAssetContract(
                    base: tiffData,
                    selected: driftedAlias,
                    button: buttonTIFFData,
                    mode: "alias drift fixture"
                )
            }
            var legacyDistinctMenu = tiffData
            legacyDistinctMenu[legacyDistinctMenu.startIndex] ^= 0x01
            try requireFailure("legacy distinct Menu fixture was accepted") {
                try requireSurfaceAssetContract(
                    base: legacyDistinctMenu,
                    selected: selectedTIFFData,
                    button: buttonTIFFData,
                    mode: "legacy distinct Menu fixture"
                )
            }
        }
        let tiffProfiles = try extractTIFFProfiles(from: tiffData, mode: mode.suffix)
        let alternateProfiles = try extractTIFFProfiles(
            from: selectedTIFFData,
            mode: "\(mode.suffix) Alternate"
        )
        let paletteProfiles = try extractTIFFProfiles(
            from: buttonTIFFData,
            mode: "\(mode.suffix) Palette"
        )
        try require(
            alternateProfiles == tiffProfiles && paletteProfiles == tiffProfiles,
            "\(mode.suffix) Menu, Alternate, and Palette must embed identical ICC profile bytes"
        )
        for profile in tiffProfiles {
            try inspectSRGBProfile(profile, mode: mode.suffix)
        }
        for profile in alternateProfiles {
            try inspectSRGBProfile(profile, mode: "\(mode.suffix) Alternate")
        }
        for profile in paletteProfiles {
            try inspectSRGBProfile(profile, mode: "\(mode.suffix) Palette")
        }
        try inspectSIPSProfile(at: tiffURL, mode: mode.suffix)
        try inspectSIPSProfile(at: selectedTIFFURL, mode: "\(mode.suffix) Alternate")
        try inspectSIPSProfile(at: buttonTIFFURL, mode: "\(mode.suffix) Palette")

        if modeIndex == 0 {
            let iccEntry = try firstTIFFICCEntry(in: tiffData, mode: mode.suffix)
            var d65MediaWhite = tiffProfiles[0]
            let whitePointRange = try iccTagRange("wtpt", in: d65MediaWhite, mode: "D65 fixture")
            d65MediaWhite.replaceUInt32BE(
                at: whitePointRange.lowerBound + 8,
                with: fixed16Bits(iccD65.x)
            )
            d65MediaWhite.replaceUInt32BE(
                at: whitePointRange.lowerBound + 12,
                with: fixed16Bits(iccD65.y)
            )
            d65MediaWhite.replaceUInt32BE(
                at: whitePointRange.lowerBound + 16,
                with: fixed16Bits(iccD65.z)
            )
            try requireFailure("D65 media-white fixture was accepted") {
                try inspectSRGBProfile(d65MediaWhite, mode: "D65 media-white fixture")
            }

            var wrongEndian = tiffData
            wrongEndian.replaceSubrange(0 ..< 2, with: Data("MM".utf8))
            try requireFailure("big-endian TIFF fixture was accepted") {
                _ = try extractTIFFProfiles(from: wrongEndian, mode: "endianness fixture")
            }

            var malformedLength = tiffData
            malformedLength.replaceUInt32LE(at: iccEntry + 4, with: UInt32.max)
            try requireFailure("malformed ICC length fixture was accepted") {
                _ = try extractTIFFProfiles(from: malformedLength, mode: "length fixture")
            }

            var malformedOffset = tiffData
            malformedOffset.replaceUInt32LE(at: iccEntry + 8, with: UInt32.max)
            try requireFailure("malformed ICC offset fixture was accepted") {
                _ = try extractTIFFProfiles(from: malformedOffset, mode: "offset fixture")
            }

            let firstIFD = Int(try tiffData.uint32LE(at: 4))
            let firstIFDEntries = Int(try tiffData.uint16LE(at: firstIFD))
            let nextIFDOffset = firstIFD + 2 + firstIFDEntries * 12
            var missingPage = tiffData
            missingPage.replaceUInt32LE(at: nextIFDOffset, with: 0)
            try requireFailure("single-page TIFF fixture was accepted") {
                _ = try extractTIFFProfiles(from: missingPage, mode: "page-count fixture")
            }

            var malformedIFDCount = tiffData
            malformedIFDCount.replaceUInt16LE(at: firstIFD, with: UInt16.max)
            try requireFailure("malformed IFD count fixture was accepted") {
                _ = try extractTIFFProfiles(from: malformedIFDCount, mode: "IFD-count fixture")
            }

            var missingICC = tiffData
            missingICC.replaceUInt16LE(at: iccEntry, with: 34_674)
            try requireFailure("missing-ICC TIFF fixture was accepted") {
                _ = try extractTIFFProfiles(from: missingICC, mode: "missing-ICC fixture")
            }

        }

        guard let tiffColorSpace = NSColorSpace(iccProfileData: tiffProfiles[0]) else {
            throw TestFailure.failed("\(mode.suffix) ICC could not create an AppKit color space")
        }
        let bitmapRepresentations = (
            NSBitmapImageRep.imageReps(withContentsOf: tiffURL) ?? []
        ).compactMap { $0 as? NSBitmapImageRep }
        let alternateRepresentations = (
            NSBitmapImageRep.imageReps(withContentsOf: selectedTIFFURL) ?? []
        ).compactMap { $0 as? NSBitmapImageRep }
        let paletteRepresentations = (
            NSBitmapImageRep.imageReps(withContentsOf: buttonTIFFURL) ?? []
        ).compactMap { $0 as? NSBitmapImageRep }
        try require(
            Set(bitmapRepresentations.map { "\($0.pixelsWide)x\($0.pixelsHigh)" })
                == Set(["22x16", "44x32"]),
            "\(mode.suffix) TIFF must contain exactly 22x16 and 44x32 representations"
        )
        try require(
            Set(alternateRepresentations.map { "\($0.pixelsWide)x\($0.pixelsHigh)" })
                == Set(["22x16", "44x32"]),
            "\(mode.suffix) Alternate must contain exactly 22x16 and 44x32 representations"
        )
        try require(
            Set(paletteRepresentations.map { "\($0.pixelsWide)x\($0.pixelsHigh)" })
                == Set(["22x16", "44x32"]),
            "\(mode.suffix) Palette must contain exactly 22x16 and 44x32 representations"
        )
        for representation in bitmapRepresentations {
            masks[mode.suffix, default: [:]][representation.pixelsHigh] =
                try inspectTemplateMaskBitmap(
                    representation,
                    sourceColorSpace: tiffColorSpace,
                    logicalSize: NSSize(width: 22, height: 16),
                    mode: "\(mode.suffix) Menu"
                )
        }
        guard let paletteColorSpace = NSColorSpace(iccProfileData: paletteProfiles[0]) else {
            throw TestFailure.failed("\(mode.suffix) Palette ICC could not create AppKit color space")
        }
        guard let alternateColorSpace = NSColorSpace(iccProfileData: alternateProfiles[0]) else {
            throw TestFailure.failed("\(mode.suffix) Alternate ICC could not create AppKit color space")
        }
        for representation in alternateRepresentations {
            _ = try inspectTemplateMaskBitmap(
                representation,
                sourceColorSpace: alternateColorSpace,
                logicalSize: NSSize(width: 22, height: 16),
                mode: "\(mode.suffix) Alternate"
            )
        }
        for representation in paletteRepresentations {
            _ = try inspectTemplateMaskBitmap(
                representation,
                sourceColorSpace: paletteColorSpace,
                logicalSize: NSSize(width: 22, height: 16),
                mode: "\(mode.suffix) Palette"
            )
        }

        if modeIndex == 0,
           let representation = bitmapRepresentations.first,
           let fixtureData = representation.tiffRepresentation,
           let coloredFixture = NSBitmapImageRep(data: fixtureData),
           let pixels = coloredFixture.bitmapData
        {
            for y in 0 ..< coloredFixture.pixelsHigh {
                for x in 0 ..< coloredFixture.pixelsWide {
                    let offset = y * coloredFixture.bytesPerRow
                        + x * coloredFixture.samplesPerPixel
                    if pixels[offset + 3] != 0 {
                        pixels[offset] = 242
                        pixels[offset + 1] = 140
                        pixels[offset + 2] = 40
                    }
                }
            }
            try requireFailure("legacy orange mode-mask fixture was accepted") {
                _ = try inspectTemplateMaskBitmap(
                    coloredFixture,
                    sourceColorSpace: tiffColorSpace,
                    logicalSize: NSSize(width: 22, height: 16),
                    mode: "legacy orange mode-mask fixture"
                )
            }
        }

        guard let image = NSImage(contentsOf: tiffURL) else {
            throw TestFailure.failed("\(mode.suffix) TIFF could not be rendered")
        }
        for setting in [
            (row: 0, scale: 1, foreground: NSColor.black, background: NSColor.white),
            (row: 1, scale: 2, foreground: NSColor.black, background: NSColor.white),
            (row: 2, scale: 1, foreground: NSColor.white, background: NSColor.black),
            (row: 3, scale: 2, foreground: NSColor.white, background: NSColor.black),
        ] {
            let rendered = try renderTemplateMask(
                image: image,
                logicalSize: NSSize(width: 22, height: 16),
                scale: setting.scale,
                foreground: setting.foreground,
                background: setting.background
            )
            previews.append(Preview(column: modeIndex, row: setting.row, image: rendered))
        }
        let adaptiveSettings = [
            (
                row: 0,
                scale: 1,
                foreground: NSColor.white,
                background: NSColor(srgbRed: 0, green: 122.0 / 255, blue: 1, alpha: 1)
            ),
            (
                row: 1,
                scale: 2,
                foreground: NSColor.white,
                background: NSColor(srgbRed: 0, green: 122.0 / 255, blue: 1, alpha: 1)
            ),
            (
                row: 2,
                scale: 1,
                foreground: NSColor.black,
                background: NSColor(srgbRed: 1, green: 149.0 / 255, blue: 0, alpha: 1)
            ),
            (
                row: 3,
                scale: 2,
                foreground: NSColor.black,
                background: NSColor(srgbRed: 1, green: 149.0 / 255, blue: 0, alpha: 1)
            ),
        ]
        for setting in adaptiveSettings {
            let rendered = try renderTemplateMask(
                image: image,
                logicalSize: NSSize(width: 22, height: 16),
                scale: setting.scale,
                foreground: setting.foreground,
                background: setting.background
            )
            adaptivePreviews.append(
                Preview(column: modeIndex, row: setting.row, image: rendered)
            )
        }
    }

    let componentTIFFURL = directory.appendingPathComponent("MuuInputMethod.tiff")
    let componentTIFFData = try Data(contentsOf: componentTIFFURL)
    try require(
        sha256Hex(componentTIFFData) == expectedUnchangedComponentAssetSHA256["MuuInputMethod.tiff"],
        "component TIFF changed during the mode-mask update"
    )
    try requireNotRetiredArtwork(
        sha256Hex(componentTIFFData),
        context: "component TIFF"
    )
    let hiraganaTileData = try Data(
        contentsOf: directory.appendingPathComponent("MuuModeHiragana.tiff")
    )
    try require(
        componentTIFFData != hiraganaTileData,
        "component TIFF must not reuse the Hiragana mode tile"
    )
    let componentProfiles = try extractTIFFProfiles(
        from: componentTIFFData,
        mode: "InputMethod"
    )
    try require(componentProfiles.count == 2, "component TIFF must carry two ICC profiles")
    for profile in componentProfiles {
        try inspectSRGBProfile(profile, mode: "InputMethod")
    }
    let componentTIFFReps = (
        NSBitmapImageRep.imageReps(withContentsOf: componentTIFFURL) ?? []
    ).compactMap { $0 as? NSBitmapImageRep }
    try require(
        Set(componentTIFFReps.map { "\($0.pixelsWide)x\($0.pixelsHigh)" })
            == Set(["17x17", "34x34"]),
        "component TIFF must contain exactly 17x17 and 34x34 representations"
    )
    for representation in componentTIFFReps {
        try requireManagedSRGBPalette(
            representation,
            mode: "InputMethod",
            asset: "component TIFF"
        )
    }

    let icnsURL = directory.appendingPathComponent("MuuIME.icns")
    let icnsData = try Data(contentsOf: icnsURL)
    try require(
        sha256Hex(icnsData) == expectedUnchangedComponentAssetSHA256["MuuIME.icns"],
        "component ICNS changed during the mode-mask update"
    )
    try requireNotRetiredArtwork(sha256Hex(icnsData), context: "component ICNS")
    let parsedICNSRepresentations = try parseICNSRepresentations(icnsData)
    try requireICNSRetinaContract(
        parsedICNSRepresentations,
        context: "component ICNS"
    )
    try requireFailure("legacy four-representation ICNS fixture was accepted") {
        let legacyData = try filterICNS(
            icnsData,
            keepingTags: Set(["icp4", "icp5", "ic07", "ic08"])
        )
        try requireICNSRetinaContract(
            try parseICNSRepresentations(legacyData),
            context: "legacy four-representation fixture"
        )
    }
    try requireFailure("missing ICNS Retina representation fixture was accepted") {
        let missingData = try filterICNS(
            icnsData,
            keepingTags: Set(expectedICNSSpecifications.dropLast().map { $0.tag })
        )
        try requireICNSRetinaContract(
            try parseICNSRepresentations(missingData),
            context: "missing representation fixture"
        )
    }
    var wrongScaleFixture = parsedICNSRepresentations
    if let index = wrongScaleFixture.firstIndex(where: { $0.tag == "ic11" }) {
        let representation = wrongScaleFixture[index]
        wrongScaleFixture[index] = ICNSRepresentation(
            tag: representation.tag,
            logicalSize: representation.logicalSize,
            scale: representation.scale,
            pixelsWide: representation.pixelsWide * 2,
            pixelsHigh: representation.pixelsHigh * 2
        )
    }
    try requireFailure("incorrect ICNS tag/scale fixture was accepted") {
        try requireICNSRetinaContract(
            wrongScaleFixture,
            context: "wrong tag-scale fixture"
        )
    }
    try requireFailure("duplicate ICNS logical representation fixture was accepted") {
        try requireICNSRetinaContract(
            parsedICNSRepresentations + [parsedICNSRepresentations[0]],
            context: "duplicate logical representation fixture"
        )
    }
    guard let icnsImage = NSImage(contentsOf: icnsURL) else {
        throw TestFailure.failed("component ICNS could not be loaded")
    }
    let icnsReps = icnsImage.representations.compactMap { $0 as? NSBitmapImageRep }
    let expectedAppKitRepresentations = Set(
        expectedICNSSpecifications.map {
            "\($0.logicalSize):\($0.logicalSize * $0.scale)"
        }
    )
    try require(
        Set(icnsReps.map { "\(Int($0.size.width)):\($0.pixelsWide)" })
            == expectedAppKitRepresentations,
        "AppKit did not preserve the ICNS logical-size/scale representations"
    )
    for representation in icnsReps {
        guard let profile = representation.colorSpace.iccProfileData else {
            throw TestFailure.failed("component ICNS representation is missing its ICC profile")
        }
        try inspectSRGBProfile(profile, mode: "InputMethod ICNS")
        try require(
            profile == componentProfiles[0],
            "component TIFF and ICNS must embed identical sRGB profile bytes"
        )
        try requireManagedSRGBPalette(
            representation,
            mode: "InputMethod",
            asset: "component ICNS"
        )
    }
    if let previewDirectory {
        try writeComponentPreview(image: icnsImage, to: previewDirectory)
    }
    for size in [16, 32] {
        guard let hiragana = masks["Hiragana"]?[size],
              let roman = masks["Roman"]?[size]
        else {
            throw TestFailure.failed("mode distinction masks are incomplete at \(size)px")
        }
        try requireDistinctModePair(
            first: hiragana,
            second: roman,
            names: "Hiragana/Roman"
        )
    }

    let oldNearlyIdentical = GlyphMask(
        width: 16,
        height: 16,
        pixels: Set(0 ..< 40)
    )
    let oldOnePixelDifference = GlyphMask(
        width: 16,
        height: 16,
        pixels: Set(0 ..< 39).union([41])
    )
    try requireFailure("one-pixel mode distinction fixture was accepted") {
        try requireDistinctModePair(
            first: oldNearlyIdentical,
            second: oldOnePixelDifference,
            names: "old one-pixel fixture"
        )
    }
    if let previewDirectory {
        try writePreviewSheet(
            previews,
            rowCount: 4,
            filename: "mode-icons-light-dark-1x-2x.png",
            to: previewDirectory
        )
        try writePreviewSheet(
            adaptivePreviews,
            rowCount: 4,
            filename: "mode-template-adaptive-1x-2x.png",
            to: previewDirectory
        )
    }
}

private func legacyIconShapeDefects(in plist: [String: Any]) throws -> [String] {
    var defects: [String] = []
    if plist["TISIconIsTemplate"] != nil {
        defects.append("template flag")
    }
    if plist["tsInputMethodIconFileKey"] as? String == "MuuModeHiragana.tiff" {
        defects.append("mode tile top-level fallback")
    }
    if plist["CFBundleIconFile"] == nil {
        defects.append("missing CFBundle icon")
    }
    let component = try dictionary(plist["ComponentInputModeDict"], named: "ComponentInputModeDict")
    let modes = try dictionary(component["tsInputModeListKey"], named: "tsInputModeListKey")
    for mode in expectedModes {
        let definition = try dictionary(modes[mode.identifier], named: mode.identifier)
        guard let labels = definition["TISIconLabels"] as? [String: Any] else {
            defects.append("\(mode.suffix) missing TISIconLabels")
            continue
        }
        if Set(labels.keys) != Set(["Primary"])
            || labels["Primary"] as? String != mode.glyph
        {
            defects.append("\(mode.suffix) label contract")
        }
        if definition["TISIconIsTemplate"] as? Bool != true {
            defects.append("\(mode.suffix) missing template authority")
        }
        let surfaceKeys = [
            "tsInputModeMenuIconFileKey",
            "tsInputModeAlternateMenuIconFileKey",
            "tsInputModePaletteIconFileKey",
        ]
        if surfaceKeys.filter({ definition[$0] != nil }).count != surfaceKeys.count {
            defects.append("\(mode.suffix) menu-only mapping")
        }
    }
    return defects
}

private func requireNoLegacyIconShape(in plist: [String: Any]) throws {
    let defects = try legacyIconShapeDefects(in: plist)
    try require(defects.isEmpty, "legacy icon metadata remains: \(defects)")
}

private func requireModeMetadataContract(
    in plist: [String: Any],
    productBundleIdentifier: String,
    context: String
) throws {
    let component = try dictionary(
        plist["ComponentInputModeDict"],
        named: "\(context) ComponentInputModeDict"
    )
    let modes = try dictionary(
        component["tsInputModeListKey"],
        named: "\(context) tsInputModeListKey"
    )
    try require(
        Set(modes.keys) == Set(expectedModes.map(\.identifier)),
        "\(context) fixed mode identifiers changed"
    )

    guard let rawOrder = component["tsVisibleInputModeOrderedArrayKey"] else {
        throw TestFailure.failed("\(context) visible mode ordering is missing")
    }
    try require(
        CFGetTypeID(rawOrder as CFTypeRef) == CFArrayGetTypeID(),
        "\(context) visible mode ordering must be an array"
    )
    try require(
        rawOrder as? [String] == expectedModes.map(\.identifier),
        "\(context) visible mode ordering changed"
    )

    for mode in expectedModes {
        let definition = try dictionary(
            modes[mode.identifier],
            named: "\(context) \(mode.identifier)"
        )
        let expected = expectedModeMetadata(
            for: mode,
            productBundleIdentifier: productBundleIdentifier
        )
        try require(
            Set(definition.keys) == Set(expected.keys),
            "\(context) \(mode.suffix) metadata keys changed"
        )
        for key in expected.keys.sorted() {
            try expected[key]!.requireMatch(
                definition[key],
                key: key,
                context: "\(context) \(mode.suffix)"
            )
        }
    }
}

private func requireInputSourcePlistContract(
    data: Data,
    plist: [String: Any],
    productBundleIdentifier: String,
    context: String
) throws {
    try requireNoLegacyIconShape(in: plist)
    try requireNoLegacyIconMetadata(in: data, context: context)
    try requireModeMetadataContract(
        in: plist,
        productBundleIdentifier: productBundleIdentifier,
        context: context
    )
}

private func replacingModeDefinition(
    in plist: [String: Any],
    mode: ExpectedMode,
    transform: (inout [String: Any]) throws -> Void
) throws -> [String: Any] {
    var fixture = plist
    var component = try dictionary(
        fixture["ComponentInputModeDict"],
        named: "fixture ComponentInputModeDict"
    )
    var modes = try dictionary(
        component["tsInputModeListKey"],
        named: "fixture tsInputModeListKey"
    )
    var definition = try dictionary(
        modes[mode.identifier],
        named: "fixture \(mode.identifier)"
    )
    try transform(&definition)
    modes[mode.identifier] = definition
    component["tsInputModeListKey"] = modes
    fixture["ComponentInputModeDict"] = component
    return fixture
}

private func requireModeMetadataNegativeFixtures(
    plist: [String: Any],
    productBundleIdentifier: String
) throws {
    let mode = expectedModes[0]
    let expected = expectedModeMetadata(
        for: mode,
        productBundleIdentifier: productBundleIdentifier
    )
    for key in expected.keys.sorted() {
        let missing = try replacingModeDefinition(in: plist, mode: mode) {
            $0.removeValue(forKey: key)
        }
        try requireFailure("\(key) missing fixture passed the mode metadata gate") {
            try requireModeMetadataContract(
                in: missing,
                productBundleIdentifier: productBundleIdentifier,
                context: "\(key) missing fixture"
            )
        }

        let wrongType = try replacingModeDefinition(in: plist, mode: mode) {
            $0[key] = expected[key]!.wrongTypeFixture
        }
        try requireFailure("\(key) wrong-type fixture passed the mode metadata gate") {
            try requireModeMetadataContract(
                in: wrongType,
                productBundleIdentifier: productBundleIdentifier,
                context: "\(key) wrong-type fixture"
            )
        }

        let drift = try replacingModeDefinition(in: plist, mode: mode) {
            $0[key] = expected[key]!.driftFixture
        }
        try requireFailure("\(key) drift fixture passed the mode metadata gate") {
            try requireModeMetadataContract(
                in: drift,
                productBundleIdentifier: productBundleIdentifier,
                context: "\(key) drift fixture"
            )
        }
    }

    var orderFixture = plist
    var component = try dictionary(
        orderFixture["ComponentInputModeDict"],
        named: "order fixture ComponentInputModeDict"
    )
    var order = expectedModes.map(\.identifier)
    order.swapAt(0, 1)
    component["tsVisibleInputModeOrderedArrayKey"] = order
    orderFixture["ComponentInputModeDict"] = component
    try requireFailure("mode-order drift fixture passed the mode metadata gate") {
        try requireModeMetadataContract(
            in: orderFixture,
            productBundleIdentifier: productBundleIdentifier,
            context: "mode-order drift fixture"
        )
    }
}

private func inspectPlist(at url: URL, assetsDirectory: URL) throws {
    let data = try Data(contentsOf: url)
    let plist = try dictionary(
        PropertyListSerialization.propertyList(from: data, options: [], format: nil),
        named: "Info.plist root"
    )
    try requireInputSourcePlistContract(
        data: data,
        plist: plist,
        productBundleIdentifier: "$(PRODUCT_BUNDLE_IDENTIFIER)",
        context: "source Info.plist"
    )
    try requireModeMetadataNegativeFixtures(
        plist: plist,
        productBundleIdentifier: "$(PRODUCT_BUNDLE_IDENTIFIER)"
    )
    try require(
        plist["tsInputMethodIconFileKey"] as? String == "MuuInputMethod.tiff",
        "the input-method component icon mapping changed"
    )
    try require(
        plist["CFBundleIconFile"] as? String == "MuuIME",
        "the CFBundle component icon mapping changed"
    )
    try require(
        plist["CFBundleVersion"] as? String == "$(CURRENT_PROJECT_VERSION)",
        "source CFBundleVersion must use the injected build number"
    )
    try require(
        plist["MuuSourceCommit"] as? String == "$(MUU_SOURCE_COMMIT)",
        "source commit provenance mapping changed"
    )
    try require(
        plist["MuuBuildVariant"] as? String == "$(MUU_BUILD_VARIANT)",
        "source build variant provenance mapping changed"
    )
    let sourceLocalizationURL = url
        .deletingLastPathComponent()
        .appendingPathComponent("ja.lproj/InfoPlist.strings")
    _ = try inspectJapaneseInfoPlistStrings(
        at: sourceLocalizationURL,
        context: "source"
    )
    var oldLocalization = expectedJapaneseInfoPlistStrings
    oldLocalization["com.apple.inputmethod.Japanese.Katakana"] = "Muu IME カタカナ"
    let oldLocalizationData = try PropertyListSerialization.data(
        fromPropertyList: oldLocalization,
        format: .xml,
        options: 0
    )
    try requireFailure("legacy public mode localization fixture was accepted") {
        try requireJapaneseInfoPlistStrings(
            oldLocalizationData,
            context: "legacy localization fixture"
        )
    }
    var driftedLocalization = expectedJapaneseInfoPlistStrings
    driftedLocalization["com.apple.inputmethod.Japanese.Roman"] = "changed"
    let driftedLocalizationData = try PropertyListSerialization.data(
        fromPropertyList: driftedLocalization,
        format: .xml,
        options: 0
    )
    try requireFailure("localized display-name drift fixture was accepted") {
        try requireJapaneseInfoPlistStrings(
            driftedLocalizationData,
            context: "drifted localization fixture"
        )
    }

    let component = try dictionary(plist["ComponentInputModeDict"], named: "ComponentInputModeDict")
    let modes = try dictionary(component["tsInputModeListKey"], named: "tsInputModeListKey")
    let sourceAssets = try Set(FileManager.default.contentsOfDirectory(atPath: assetsDirectory.path))
    try require(
        sourceAssets == Set(generatedAssetNames),
        "source asset directory must contain exactly the 8 authoritative assets"
    )
    for suffix in removedPublicModeSuffixes {
        try require(
            !sourceAssets.contains(where: { $0.contains("MuuMode\(suffix)") }),
            "source assets still expose removed public mode \(suffix)"
        )
    }
    try requireFailure("missing source asset fixture was accepted") {
        try requireExpectedAssets(
            available: Set(generatedAssetNames.dropLast()),
            expected: generatedAssetNames,
            context: "missing source fixture"
        )
    }

    for mode in expectedModes {
        let definition = try dictionary(modes[mode.identifier], named: mode.identifier)
        let expectedMappings = [
            "tsInputModeMenuIconFileKey": "MuuMode\(mode.suffix).tiff",
            "tsInputModeAlternateMenuIconFileKey": "MuuMode\(mode.suffix)Selected.tiff",
            "tsInputModePaletteIconFileKey": "MuuMode\(mode.suffix)Button.tiff",
        ]
        for (key, asset) in expectedMappings {
            try require(
                definition[key] as? String == asset,
                "\(mode.suffix) \(key) mapping changed"
            )
            try require(
                FileManager.default.fileExists(
                    atPath: assetsDirectory.appendingPathComponent(asset).path
                ),
                "\(asset) is missing"
            )
            let sourceRepresentationSizes = try bitmapRepresentationSizes(
                at: assetsDirectory.appendingPathComponent(asset)
            )
            try require(
                sourceRepresentationSizes == Set(["22x16", "44x32"]),
                "\(asset) must contain exactly 22x16 and 44x32 representations"
            )
        }
    }

    var legacyFixture = plist
    legacyFixture["TISIconIsTemplate"] = false
    legacyFixture["tsInputMethodIconFileKey"] = "MuuModeHiragana.tiff"
    legacyFixture.removeValue(forKey: "CFBundleIconFile")
    var legacyComponent = component
    var legacyModes = modes
    for mode in expectedModes {
        var definition = try dictionary(legacyModes[mode.identifier], named: mode.identifier)
        definition["TISIconLabels"] = [
            "Primary": mode.glyph,
            "CustomIcon": "MuuMode\(mode.suffix)@2x.pdf",
        ]
        definition.removeValue(forKey: "tsInputModeAlternateMenuIconFileKey")
        definition.removeValue(forKey: "tsInputModePaletteIconFileKey")
        legacyModes[mode.identifier] = definition
    }
    legacyComponent["tsInputModeListKey"] = legacyModes
    legacyFixture["ComponentInputModeDict"] = legacyComponent
    try requireFailure("legacy Muu icon metadata fixture was accepted") {
        try requireNoLegacyIconShape(in: legacyFixture)
    }
    let legacyFixtureData = try PropertyListSerialization.data(
        fromPropertyList: legacyFixture,
        format: .xml,
        options: 0
    )
    try requireFailure("legacy icon metadata fixture passed the metadata gate") {
        try requireNoLegacyIconMetadata(in: legacyFixtureData, context: "legacy icon metadata fixture")
    }
    try requireFailure("legacy icon metadata fixture passed the production plist contract") {
        try requireInputSourcePlistContract(
            data: legacyFixtureData,
            plist: legacyFixture,
            productBundleIdentifier: "$(PRODUCT_BUNDLE_IDENTIFIER)",
            context: "legacy icon metadata production fixture"
        )
    }

    var fiveModeFixture = plist
    var fiveModeComponent = component
    var fiveModes = modes
    for suffix in removedPublicModeSuffixes {
        fiveModes["com.apple.inputmethod.Japanese.\(suffix)"] = [
            "TISInputSourceID": "$(PRODUCT_BUNDLE_IDENTIFIER).\(suffix)",
        ]
    }
    fiveModeComponent["tsInputModeListKey"] = fiveModes
    fiveModeComponent["tsVisibleInputModeOrderedArrayKey"] = [
        "com.apple.inputmethod.Japanese.Hiragana",
        "com.apple.inputmethod.Japanese.Katakana",
        "com.apple.inputmethod.Japanese.HalfWidthKana",
        "com.apple.inputmethod.Japanese.FullWidthRoman",
        "com.apple.inputmethod.Japanese.Roman",
    ]
    fiveModeFixture["ComponentInputModeDict"] = fiveModeComponent
    try requireFailure("legacy five-mode public fixture passed the two-mode contract") {
        try requireModeMetadataContract(
            in: fiveModeFixture,
            productBundleIdentifier: "$(PRODUCT_BUNDLE_IDENTIFIER)",
            context: "legacy five-mode fixture"
        )
    }
}

private func occurrences(of needle: String, in haystack: String) -> Int {
    haystack.components(separatedBy: needle).count - 1
}

private func pbxSection(_ name: String, in project: String) throws -> String {
    let begin = "/* Begin \(name) section */"
    let end = "/* End \(name) section */"
    guard let beginRange = project.range(of: begin),
          let endRange = project.range(of: end),
          beginRange.upperBound <= endRange.lowerBound
    else {
        throw TestFailure.failed("PBX section \(name) is missing")
    }
    return String(project[beginRange.upperBound ..< endRange.lowerBound])
}

private func inspectPBXMembershipSections(
    buildFiles: String,
    fileReferences: String,
    resourcesPhase: String
) throws {
    for asset in generatedAssetNames {
        try require(
            occurrences(of: "/* \(asset) in Resources */", in: buildFiles) == 1,
            "\(asset) must have exactly one PBXBuildFile Resources entry"
        )
        try require(
            occurrences(of: "/* \(asset) */", in: fileReferences) == 1,
            "\(asset) must have exactly one PBXFileReference"
        )
        try require(
            occurrences(of: "/* \(asset) in Resources */", in: resourcesPhase) == 1,
            "\(asset) must occur exactly once in Copy Bundle Resources"
        )
    }
}

private func inspectPBXMembership(at url: URL) throws {
    let project = try String(contentsOf: url, encoding: .utf8)
    try requireNoLegacyIconMetadata(
        in: Data(project.utf8),
        context: "PBX project"
    )
    let buildFiles = try pbxSection("PBXBuildFile", in: project)
    let fileReferences = try pbxSection("PBXFileReference", in: project)
    let resourcesPhase = try pbxSection("PBXResourcesBuildPhase", in: project)
    try inspectPBXMembershipSections(
        buildFiles: buildFiles,
        fileReferences: fileReferences,
        resourcesPhase: resourcesPhase
    )
    try require(
        occurrences(of: "/* InfoPlist.strings in Resources */", in: buildFiles) == 1
            && occurrences(of: "/* InfoPlist.strings in Resources */", in: resourcesPhase) == 1
            && occurrences(of: "/* InfoPlist.strings */", in: project) >= 2
            && occurrences(of: "path = ja.lproj/InfoPlist.strings;", in: fileReferences) == 1,
        "Japanese InfoPlist.strings must be localized and copied exactly once"
    )
    for suffix in removedPublicModeSuffixes {
        try require(
            !project.contains("MuuMode\(suffix)"),
            "PBX project still references removed public mode \(suffix)"
        )
    }

    let fixtureAsset = generatedAssetNames[0]
    let fixtureMarker = "/* \(fixtureAsset) in Resources */"
    try requireFailure("missing PBX Resources membership fixture was accepted") {
        try inspectPBXMembershipSections(
            buildFiles: buildFiles,
            fileReferences: fileReferences,
            resourcesPhase: resourcesPhase.replacingOccurrences(
                of: fixtureMarker,
                with: "/* removed fixture */"
            )
        )
    }
    try requireFailure("duplicate PBX Resources membership fixture was accepted") {
        try inspectPBXMembershipSections(
            buildFiles: buildFiles,
            fileReferences: fileReferences,
            resourcesPhase: resourcesPhase + "\n\(fixtureMarker)"
        )
    }
}

private func inspectReleaseBundle(app: URL, sourceAssets: URL) throws {
    let resources = app.appendingPathComponent("Contents/Resources", isDirectory: true)
    let infoURL = app.appendingPathComponent("Contents/Info.plist", isDirectory: false)
    let compiledPlistData = try Data(contentsOf: infoURL)
    _ = try inspectJapaneseInfoPlistStrings(
        at: resources.appendingPathComponent("ja.lproj/InfoPlist.strings"),
        context: "Release bundle"
    )
    _ = try inspectJapaneseInfoPlistStrings(
        at: sourceAssets
            .deletingLastPathComponent()
            .appendingPathComponent("ja.lproj/InfoPlist.strings"),
        context: "source"
    )
    let plist = try dictionary(
        PropertyListSerialization.propertyList(
            from: compiledPlistData,
            options: [],
            format: nil
        ),
        named: "compiled Info.plist root"
    )
    guard let compiledBundleIdentifier = plist["CFBundleIdentifier"] as? String else {
        throw TestFailure.failed("compiled CFBundleIdentifier is missing")
    }
    try requireInputSourcePlistContract(
        data: compiledPlistData,
        plist: plist,
        productBundleIdentifier: compiledBundleIdentifier,
        context: "compiled Info.plist"
    )
    let component = try dictionary(plist["ComponentInputModeDict"], named: "compiled ComponentInputModeDict")
    let modes = try dictionary(component["tsInputModeListKey"], named: "compiled tsInputModeListKey")

    var mappedAssets: Set<String> = [
        "MuuInputMethod.tiff",
        "MuuIME.icns",
    ]
    try require(
        plist["tsInputMethodIconFileKey"] as? String == "MuuInputMethod.tiff",
        "compiled component TIS icon mapping changed"
    )
    try require(
        plist["CFBundleIconFile"] as? String == "MuuIME",
        "compiled CFBundle icon mapping changed"
    )
    guard let expectedCommit = ProcessInfo.processInfo.environment["MUU_EXPECTED_SOURCE_COMMIT"],
          let expectedBuildNumber = ProcessInfo.processInfo.environment["MUU_EXPECTED_BUILD_NUMBER"],
          let expectedBuildVariant = ProcessInfo.processInfo.environment["MUU_EXPECTED_BUILD_VARIANT"]
    else {
        throw TestFailure.failed("compiled provenance expectations are missing")
    }
    try require(
        plist["MuuSourceCommit"] as? String == expectedCommit,
        "compiled source commit provenance changed"
    )
    try require(
        plist["CFBundleVersion"] as? String == expectedBuildNumber,
        "compiled CFBundleVersion does not match the monotonic build number"
    )
    try require(
        plist["MuuBuildVariant"] as? String == expectedBuildVariant,
        "compiled build variant provenance changed"
    )
    let componentTIFFSizes = try bitmapRepresentationSizes(
        at: resources.appendingPathComponent("MuuInputMethod.tiff")
    )
    try require(
        componentTIFFSizes == Set(["17x17", "34x34"]),
        "compiled component TIFF must contain exactly 17x17 and 34x34 representations"
    )
    let componentBundleData = try Data(
        contentsOf: resources.appendingPathComponent("MuuInputMethod.tiff")
    )
    let hiraganaBundleData = try Data(
        contentsOf: resources.appendingPathComponent("MuuModeHiragana.tiff")
    )
    try require(
        componentBundleData != hiraganaBundleData,
        "compiled component icon must not reuse the Hiragana tile"
    )
    let bundledICNSRepresentations = try parseICNSRepresentations(
        Data(contentsOf: resources.appendingPathComponent("MuuIME.icns"))
    )
    try requireICNSRetinaContract(
        bundledICNSRepresentations,
        context: "compiled CFBundle ICNS"
    )
    for mode in expectedModes {
        let definition = try dictionary(modes[mode.identifier], named: "compiled \(mode.identifier)")
        var surfaceData: [Data] = []
        for key in [
            "tsInputModeMenuIconFileKey",
            "tsInputModeAlternateMenuIconFileKey",
            "tsInputModePaletteIconFileKey",
        ] {
            guard let asset = definition[key] as? String else {
                throw TestFailure.failed("compiled \(mode.identifier).\(key) mapping is missing")
            }
            mappedAssets.insert(asset)
            let bundledURL = resources.appendingPathComponent(asset)
            let bundledRepresentationSizes = try bitmapRepresentationSizes(at: bundledURL)
            try require(
                bundledRepresentationSizes == Set(["22x16", "44x32"]),
                "compiled \(mode.suffix) \(key) asset dimensions changed"
            )
            surfaceData.append(try Data(contentsOf: bundledURL))
        }
        try requireSurfaceAssetContract(
            base: surfaceData[0],
            selected: surfaceData[1],
            button: surfaceData[2],
            mode: "compiled \(mode.suffix)"
        )
    }
    try require(
        mappedAssets == Set(generatedAssetNames),
        "compiled Info.plist does not map exactly the 8 authoritative icon assets"
    )

    let bundledAssets = try Set(FileManager.default.contentsOfDirectory(atPath: resources.path))
    try require(
        bundledAssets.allSatisfy { !$0.hasPrefix("MuuMode") || !$0.hasSuffix(".pdf") },
        "Release bundle contains an obsolete mode PDF"
    )
    try requireExpectedAssets(
        available: bundledAssets,
        expected: generatedAssetNames,
        context: "Release bundle Resources"
    )
    for asset in generatedAssetNames {
        let source = try Data(contentsOf: sourceAssets.appendingPathComponent(asset))
        let bundled = try Data(contentsOf: resources.appendingPathComponent(asset))
        try requireMatchingAssetBytes(source: source, bundled: bundled, name: asset)
    }

    try requireFailure("missing bundle asset fixture was accepted") {
        try requireExpectedAssets(
            available: Set(generatedAssetNames.dropFirst()),
            expected: generatedAssetNames,
            context: "missing bundle fixture"
        )
    }
    let sourceFixture = try Data(
        contentsOf: sourceAssets.appendingPathComponent(generatedAssetNames[0])
    )
    var alteredBundleFixture = sourceFixture
    alteredBundleFixture.append(0)
    try requireFailure("bundle byte-difference fixture was accepted") {
        try requireMatchingAssetBytes(
            source: sourceFixture,
            bundled: alteredBundleFixture,
            name: "byte-difference fixture"
        )
    }
}

do {
    if CommandLine.arguments.count == 4,
       CommandLine.arguments[1] == "--generated-asset-sets"
    {
        try inspectGeneratedAssetSets(CommandLine.arguments.dropFirst(2).map {
            URL(fileURLWithPath: $0, isDirectory: true)
        })
        exit(0)
    }
    guard CommandLine.arguments.count == 5 || CommandLine.arguments.count == 6 else {
        throw TestFailure.failed(
            "usage: test-mode-icons <assets-directory> <artwork-directory> <Info.plist> <project.pbxproj> [Release.app]\n"
                + "       test-mode-icons --generated-asset-sets <generated-a> <generated-b>"
        )
    }
    let assets = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    let artwork = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
    let plist = URL(fileURLWithPath: CommandLine.arguments[3], isDirectory: false)
    let project = URL(fileURLWithPath: CommandLine.arguments[4], isDirectory: false)
    let previewDirectory = ProcessInfo.processInfo.environment["MUU_MODE_ICON_PREVIEW_DIR"].map {
        URL(fileURLWithPath: $0, isDirectory: true)
    }
    try validateSIPSOutputDraining()
    try inspectArtworkSources(at: artwork)
    try inspectAssets(at: assets, previewDirectory: previewDirectory)
    try inspectPlist(at: plist, assetsDirectory: assets)
    try inspectPBXMembership(at: project)
    if CommandLine.arguments.count == 6 {
        let app = URL(fileURLWithPath: CommandLine.arguments[5], isDirectory: true)
        try inspectReleaseBundle(app: app, sourceAssets: assets)
    }
} catch {
    FileHandle.standardError.write(Data("mode icon test failed: \(error)\n".utf8))
    exit(1)
}
