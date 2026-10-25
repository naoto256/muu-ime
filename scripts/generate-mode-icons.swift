#!/usr/bin/env swift

// Turns the committed artwork SVGs into the bundled icon files, with no image
// library: a strict SVG subset is parsed, rasterized with 4x4 supersampling
// and encoded by hand as TIFF and ICNS with an embedded sRGB profile. The
// output must be byte-for-byte reproducible; mode-icon-test regenerates the
// assets and compares them with the committed ones.

import Foundation

private struct ModeIcon {
    let name: String
    let artwork: TrustedSVGArtwork
}

private struct IconCanvas {
    let width: Int
    let height: Int
}

// Mode icons are template masks (black glyph, alpha only) that macOS tints
// for the menu bar; the component icon is the full-colour orange tile with a
// white glyph.
private enum ModeSurfaceAppearance {
    case fullColorTile
    case transparentBlackGlyph
}

private struct VectorPoint: Equatable {
    let x: Double
    let y: Double
}

private struct VectorEdge {
    let start: VectorPoint
    let end: VectorPoint
}

private struct TrustedSVGArtwork {
    let width: Int
    let height: Int
    let background: [VectorEdge]
    let glyph: [VectorEdge]
}

private let modeCanvas = IconCanvas(width: 22, height: 16)
private let componentTIFFCanvas = IconCanvas(width: 17, height: 17)
private let componentICNSCanvas = IconCanvas(width: 16, height: 16)
private let samplesPerAxis = 4
private let cubicSegments = 64
private let orange = (red: 242, green: 140, blue: 40)

private enum GenerationError: Error, CustomStringConvertible {
    case usage
    case encoding(String)

    var description: String {
        switch self {
        case .usage:
            return "usage: generate-mode-icons.swift <artwork-directory> <output-directory> | --self-test"
        case let .encoding(message):
            return message
        }
    }
}

private struct TrustedSVGSpec {
    let filename: String
    let width: Int
    let height: Int
}

// Absolute M/L/H/V/C/Z only, every coordinate inside the viewBox, every
// subpath closed. Cubics are flattened to 64 line segments. Anything else is
// an error rather than an approximation.
private struct SVGPathScanner {
    private let bytes: [UInt8]
    private var index = 0
    private var current: VectorPoint?
    private var subpathStart: VectorPoint?
    private var subpathOpen = false
    private(set) var edges: [VectorEdge] = []

    init(_ path: String) throws {
        guard path.unicodeScalars.allSatisfy(\.isASCII) else {
            throw GenerationError.encoding("SVG path data must be ASCII")
        }
        bytes = Array(path.utf8)
    }

    mutating func parse(width: Int, height: Int) throws -> [VectorEdge] {
        while true {
            skipSeparators()
            guard index < bytes.count else { break }
            let command = bytes[index]
            index += 1
            switch command {
            case 0x4D:
                try requireClosedBeforeMove()
                let point = try parsePoint(width: width, height: height)
                current = point
                subpathStart = point
                subpathOpen = true
            case 0x4C:
                try appendLine(to: parsePoint(width: width, height: height))
            case 0x48:
                let x = try parseCoordinate(limit: width)
                guard let current else {
                    throw GenerationError.encoding("SVG H command precedes M")
                }
                try appendLine(to: VectorPoint(x: x, y: current.y))
            case 0x56:
                let y = try parseCoordinate(limit: height)
                guard let current else {
                    throw GenerationError.encoding("SVG V command precedes M")
                }
                try appendLine(to: VectorPoint(x: current.x, y: y))
            case 0x43:
                let control1 = try parsePoint(width: width, height: height)
                let control2 = try parsePoint(width: width, height: height)
                let end = try parsePoint(width: width, height: height)
                try appendCubic(control1: control1, control2: control2, end: end)
            case 0x5A:
                try closeSubpath()
            default:
                throw GenerationError.encoding(
                    "SVG path command is outside the trusted M/L/H/V/C/Z subset"
                )
            }
        }
        guard !subpathOpen, !edges.isEmpty else {
            throw GenerationError.encoding("SVG paths must contain only closed, nonempty subpaths")
        }
        return edges
    }

    private mutating func skipSeparators() {
        while index < bytes.count {
            switch bytes[index] {
            case 0x09, 0x0A, 0x0D, 0x20, 0x2C:
                index += 1
            default:
                return
            }
        }
    }

    private mutating func parsePoint(width: Int, height: Int) throws -> VectorPoint {
        VectorPoint(
            x: try parseCoordinate(limit: width),
            y: try parseCoordinate(limit: height)
        )
    }

    private mutating func parseCoordinate(limit: Int) throws -> Double {
        skipSeparators()
        let start = index
        if index < bytes.count, bytes[index] == 0x2B || bytes[index] == 0x2D {
            index += 1
        }
        var digitCount = 0
        var decimalCount = 0
        while index < bytes.count {
            let byte = bytes[index]
            if byte >= 0x30, byte <= 0x39 {
                digitCount += 1
                index += 1
            } else if byte == 0x2E {
                decimalCount += 1
                index += 1
            } else {
                break
            }
        }
        guard digitCount > 0, decimalCount <= 1,
              let value = Double(String(decoding: bytes[start ..< index], as: UTF8.self)),
              value.isFinite,
              value >= 0,
              value <= Double(limit)
        else {
            throw GenerationError.encoding("SVG coordinate is invalid or outside its viewBox")
        }
        return value
    }

    private mutating func requireClosedBeforeMove() throws {
        if subpathOpen {
            throw GenerationError.encoding("SVG subpaths must close before a new M command")
        }
    }

    private mutating func appendLine(to end: VectorPoint) throws {
        guard subpathOpen, let start = current else {
            throw GenerationError.encoding("SVG line command precedes M")
        }
        edges.append(VectorEdge(start: start, end: end))
        current = end
    }

    private mutating func appendCubic(
        control1: VectorPoint,
        control2: VectorPoint,
        end: VectorPoint
    ) throws {
        guard subpathOpen, let start = current else {
            throw GenerationError.encoding("SVG cubic command precedes M")
        }
        var previous = start
        for step in 1 ... cubicSegments {
            let t = Double(step) / Double(cubicSegments)
            let oneMinusT = 1 - t
            let point = VectorPoint(
                x: oneMinusT * oneMinusT * oneMinusT * start.x
                    + 3 * oneMinusT * oneMinusT * t * control1.x
                    + 3 * oneMinusT * t * t * control2.x
                    + t * t * t * end.x,
                y: oneMinusT * oneMinusT * oneMinusT * start.y
                    + 3 * oneMinusT * oneMinusT * t * control1.y
                    + 3 * oneMinusT * t * t * control2.y
                    + t * t * t * end.y
            )
            edges.append(VectorEdge(start: previous, end: point))
            previous = point
        }
        current = end
    }

    private mutating func closeSubpath() throws {
        guard subpathOpen, let current, let subpathStart else {
            throw GenerationError.encoding("SVG Z command precedes M")
        }
        if current != subpathStart {
            edges.append(VectorEdge(start: current, end: subpathStart))
        }
        self.current = nil
        self.subpathStart = nil
        subpathOpen = false
    }
}

// The canonical artwork contract: one <svg> with exactly these four
// attributes, then exactly two <path> children, the orange background first
// and the white nonzero-filled glyph second, with no other element or text.
private final class TrustedSVGDelegate: NSObject, XMLParserDelegate {
    private let spec: TrustedSVGSpec
    private var depth = 0
    private var pathEdges: [[VectorEdge]] = []
    private(set) var failure: Error?
    private(set) var artwork: TrustedSVGArtwork?

    init(spec: TrustedSVGSpec) {
        self.spec = spec
    }

    func parser(
        _ parser: XMLParser,
        didStartElement elementName: String,
        namespaceURI _: String?,
        qualifiedName _: String?,
        attributes attributeDict: [String: String] = [:]
    ) {
        guard failure == nil else {
            parser.abortParsing()
            return
        }
        do {
            switch (depth, elementName) {
            case (0, "svg"):
                let expected = [
                    "xmlns": "http://www.w3.org/2000/svg",
                    "viewBox": "0 0 \(spec.width) \(spec.height)",
                    "width": "\(spec.width)",
                    "height": "\(spec.height)",
                ]
                guard attributeDict == expected else {
                    throw GenerationError.encoding(
                        "\(spec.filename) svg attributes differ from the canonical contract"
                    )
                }
            case (1, "path"):
                guard pathEdges.count < 2 else {
                    throw GenerationError.encoding("\(spec.filename) must contain exactly two paths")
                }
                let expectedFill = pathEdges.isEmpty ? "#F28C28" : "#FFFFFF"
                let allowedKeys: Set<String> = pathEdges.isEmpty
                    ? ["fill", "d"]
                    : ["fill", "fill-rule", "d"]
                guard Set(attributeDict.keys) == allowedKeys,
                      attributeDict["fill"] == expectedFill,
                      pathEdges.isEmpty || attributeDict["fill-rule"] == "nonzero",
                      let data = attributeDict["d"]
                else {
                    throw GenerationError.encoding(
                        "\(spec.filename) path attributes differ from the canonical contract"
                    )
                }
                var scanner = try SVGPathScanner(data)
                pathEdges.append(try scanner.parse(width: spec.width, height: spec.height))
            default:
                throw GenerationError.encoding(
                    "\(spec.filename) contains an element outside the trusted svg/path subset"
                )
            }
            depth += 1
        } catch {
            failure = error
            parser.abortParsing()
        }
    }

    func parser(
        _ parser: XMLParser,
        didEndElement elementName: String,
        namespaceURI _: String?,
        qualifiedName _: String?
    ) {
        guard failure == nil else { return }
        depth -= 1
        if depth < 0 || (depth == 0 && elementName != "svg") {
            failure = GenerationError.encoding("\(spec.filename) XML nesting is invalid")
            parser.abortParsing()
        }
    }

    func parser(_ parser: XMLParser, foundCharacters string: String) {
        if string.rangeOfCharacter(from: .whitespacesAndNewlines.inverted) != nil {
            failure = GenerationError.encoding("\(spec.filename) contains non-whitespace text")
            parser.abortParsing()
        }
    }

    func parser(_ parser: XMLParser, parseErrorOccurred parseError: Error) {
        if failure == nil {
            failure = parseError
        }
    }

    func finish() throws -> TrustedSVGArtwork {
        if let failure {
            throw failure
        }
        guard depth == 0, pathEdges.count == 2 else {
            throw GenerationError.encoding("\(spec.filename) must contain one svg and two paths")
        }
        let artwork = TrustedSVGArtwork(
            width: spec.width,
            height: spec.height,
            background: pathEdges[0],
            glyph: pathEdges[1]
        )
        self.artwork = artwork
        return artwork
    }
}

private func parseArtwork(_ data: Data, spec: TrustedSVGSpec) throws -> TrustedSVGArtwork {
    guard let source = String(data: data, encoding: .utf8),
          !source.contains("<!DOCTYPE"),
          !source.contains("<!ENTITY"),
          !source.contains("url(")
    else {
        throw GenerationError.encoding("\(spec.filename) is not canonical UTF-8 path-only SVG")
    }
    let delegate = TrustedSVGDelegate(spec: spec)
    let parser = XMLParser(data: data)
    parser.delegate = delegate
    parser.shouldProcessNamespaces = false
    parser.shouldReportNamespacePrefixes = false
    parser.shouldResolveExternalEntities = false
    guard parser.parse() else {
        if let failure = delegate.failure {
            throw failure
        }
        throw GenerationError.encoding("\(spec.filename) could not be parsed")
    }
    return try delegate.finish()
}

private func loadArtwork(_ spec: TrustedSVGSpec, from directory: URL) throws -> TrustedSVGArtwork {
    try parseArtwork(
        Data(contentsOf: directory.appendingPathComponent(spec.filename)),
        spec: spec
    )
}

private extension Data {
    mutating func appendUInt16LE(_ value: UInt16) {
        append(UInt8(value & 0xff))
        append(UInt8((value >> 8) & 0xff))
    }

    mutating func appendUInt32LE(_ value: UInt32) {
        append(UInt8(value & 0xff))
        append(UInt8((value >> 8) & 0xff))
        append(UInt8((value >> 16) & 0xff))
        append(UInt8((value >> 24) & 0xff))
    }

    mutating func appendUInt16BE(_ value: UInt16) {
        append(UInt8((value >> 8) & 0xff))
        append(UInt8(value & 0xff))
    }

    mutating func appendUInt32BE(_ value: UInt32) {
        append(UInt8((value >> 24) & 0xff))
        append(UInt8((value >> 16) & 0xff))
        append(UInt8((value >> 8) & 0xff))
        append(UInt8(value & 0xff))
    }

    mutating func appendASCII(_ text: String) {
        append(contentsOf: text.utf8)
    }

    mutating func align(to alignment: Int) {
        while count % alignment != 0 {
            append(0)
        }
    }

    mutating func replaceUInt32BE(at offset: Int, with value: UInt32) {
        replaceSubrange(offset ..< offset + 4, with: [
            UInt8((value >> 24) & 0xff),
            UInt8((value >> 16) & 0xff),
            UInt8((value >> 8) & 0xff),
            UInt8(value & 0xff),
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

    mutating func appendPNGChunk(type: String, payload: Data) {
        appendUInt32BE(UInt32(payload.count))
        let typeData = Data(type.utf8)
        append(typeData)
        append(payload)
        appendUInt32BE(crc32(typeData + payload))
    }
}

private func crc32(_ data: Data) -> UInt32 {
    var crc = UInt32.max
    for byte in data {
        crc ^= UInt32(byte)
        for _ in 0 ..< 8 {
            let mask = UInt32(bitPattern: -Int32(crc & 1))
            crc = (crc >> 1) ^ (0xedb8_8320 & mask)
        }
    }
    return crc ^ UInt32.max
}

private func adler32(_ data: Data) -> UInt32 {
    let modulus: UInt32 = 65_521
    var first: UInt32 = 1
    var second: UInt32 = 0
    for byte in data {
        first = (first + UInt32(byte)) % modulus
        second = (second + first) % modulus
    }
    return (second << 16) | first
}

private func fixed16(_ value: Double) -> UInt32 {
    UInt32(bitPattern: Int32((value * 65_536.0).rounded()))
}

private func iccDescription(_ text: String) -> Data {
    var data = Data()
    data.appendASCII("desc")
    data.append(contentsOf: repeatElement(UInt8(0), count: 4))
    let ascii = Array(text.utf8) + [0]
    data.appendUInt32BE(UInt32(ascii.count))
    data.append(contentsOf: ascii)
    data.appendUInt32BE(0)
    data.appendUInt32BE(0)
    data.appendUInt16BE(0)
    data.append(0)
    data.append(contentsOf: repeatElement(UInt8(0), count: 67))
    data.align(to: 4)
    return data
}

private func iccText(_ text: String) -> Data {
    var data = Data()
    data.appendASCII("text")
    data.append(contentsOf: repeatElement(UInt8(0), count: 4))
    data.appendASCII(text)
    data.append(0)
    data.align(to: 4)
    return data
}

private func iccXYZ(_ x: Double, _ y: Double, _ z: Double) -> Data {
    var data = Data()
    data.appendASCII("XYZ ")
    data.append(contentsOf: repeatElement(UInt8(0), count: 4))
    data.appendUInt32BE(fixed16(x))
    data.appendUInt32BE(fixed16(y))
    data.appendUInt32BE(fixed16(z))
    return data
}

private func iccFixedArray(_ values: [Double]) -> Data {
    var data = Data()
    data.appendASCII("sf32")
    data.append(contentsOf: repeatElement(UInt8(0), count: 4))
    for value in values {
        data.appendUInt32BE(fixed16(value))
    }
    return data
}

private func iccSRGBTransferCurve() -> Data {
    let samples: [UInt16] = [
        0, 20, 40, 60, 80, 99, 119, 139, 159, 179, 199, 219, 241, 264, 288, 313,
        340, 367, 396, 427, 458, 491, 526, 562, 599, 637, 677, 718, 761, 805, 851, 898,
        947, 997, 1048, 1101, 1156, 1212, 1270, 1330, 1391, 1453, 1517, 1583, 1651, 1720, 1790, 1863,
        1937, 2013, 2090, 2170, 2250, 2333, 2418, 2504, 2592, 2681, 2773, 2866, 2961, 3058, 3157, 3258,
        3360, 3464, 3570, 3678, 3788, 3900, 4014, 4129, 4247, 4366, 4488, 4611, 4736, 4864, 4993, 5124,
        5257, 5392, 5530, 5669, 5810, 5953, 6099, 6246, 6395, 6547, 6700, 6856, 7014, 7174, 7335, 7500,
        7666, 7834, 8004, 8177, 8352, 8528, 8708, 8889, 9072, 9258, 9445, 9635, 9828, 10_022, 10_219, 10_417,
        10_619, 10_822, 11_028, 11_235, 11_446, 11_658, 11_873, 12_090, 12_309, 12_530, 12_754, 12_980, 13_209, 13_440, 13_673, 13_909,
        14_146, 14_387, 14_629, 14_874, 15_122, 15_371, 15_623, 15_878, 16_135, 16_394, 16_656, 16_920, 17_187, 17_456, 17_727, 18_001,
        18_277, 18_556, 18_837, 19_121, 19_407, 19_696, 19_987, 20_281, 20_577, 20_876, 21_177, 21_481, 21_787, 22_096, 22_407, 22_721,
        23_038, 23_357, 23_678, 24_002, 24_329, 24_658, 24_990, 25_325, 25_662, 26_001, 26_344, 26_688, 27_036, 27_386, 27_739, 28_094,
        28_452, 28_813, 29_176, 29_542, 29_911, 30_282, 30_656, 31_033, 31_412, 31_794, 32_179, 32_567, 32_957, 33_350, 33_745, 34_143,
        34_544, 34_948, 35_355, 35_764, 36_176, 36_591, 37_008, 37_429, 37_852, 38_278, 38_706, 39_138, 39_572, 40_009, 40_449, 40_891,
        41_337, 41_785, 42_236, 42_690, 43_147, 43_606, 44_069, 44_534, 45_002, 45_473, 45_947, 46_423, 46_903, 47_385, 47_871, 48_359,
        48_850, 49_344, 49_841, 50_341, 50_844, 51_349, 51_858, 52_369, 52_884, 53_401, 53_921, 54_445, 54_971, 55_500, 56_032, 56_567,
        57_105, 57_646, 58_190, 58_737, 59_287, 59_840, 60_396, 60_955, 61_517, 62_082, 62_650, 63_221, 63_795, 64_372, 64_952, 65_535,
    ]
    var data = Data()
    data.appendASCII("curv")
    data.append(contentsOf: repeatElement(UInt8(0), count: 4))
    data.appendUInt32BE(UInt32(samples.count))
    for sample in samples {
        data.appendUInt16BE(sample)
    }
    return data
}

private func makeSRGBProfile() -> Data {
    // Provenance/license: this profile is serialized here from the standard
    // sRGB transfer curve, ICC D50 media white, D50-adapted colorants, and the
    // Bradford D65-to-D50 chromatic-adaptation matrix defined by
    // IEC 61966-2-1/ICC. The source primaries use D65, while ICC v4 display
    // profiles encode the media white and PCS illuminant as D50. No
    // third-party ICC profile bytes are copied; this implementation is
    // MIT-licensed with the rest of Muu.
    let tagPayloads: [(String, Data)] = [
        ("desc", iccDescription("Muu sRGB")),
        ("cprt", iccText("Muu IME deterministic sRGB profile")),
        ("wtpt", iccXYZ(0.9642, 1.0, 0.82491)),
        ("rXYZ", iccXYZ(0.4360747, 0.2225045, 0.0139322)),
        ("gXYZ", iccXYZ(0.3850649, 0.7168786, 0.0971045)),
        ("bXYZ", iccXYZ(0.1430804, 0.0606169, 0.7141733)),
        ("rTRC", iccSRGBTransferCurve()),
        ("gTRC", iccSRGBTransferCurve()),
        ("bTRC", iccSRGBTransferCurve()),
        ("chad", iccFixedArray([
            1.0478112, 0.0228866, -0.0501270,
            0.0295424, 0.9904844, -0.0170491,
            -0.0092345, 0.0150436, 0.7521316,
        ])),
    ]

    var profile = Data(repeating: 0, count: 128)
    profile.replaceSubrange(4 ..< 8, with: Array("Muu ".utf8))
    profile.replaceUInt32BE(at: 8, with: 0x0430_0000)
    profile.replaceSubrange(12 ..< 16, with: Array("mntr".utf8))
    profile.replaceSubrange(16 ..< 20, with: Array("RGB ".utf8))
    profile.replaceSubrange(20 ..< 24, with: Array("XYZ ".utf8))
    let date: [UInt16] = [1970, 1, 1, 0, 0, 0]
    for (index, value) in date.enumerated() {
        profile.replaceSubrange(24 + index * 2 ..< 26 + index * 2, with: [
            UInt8((value >> 8) & 0xff),
            UInt8(value & 0xff),
        ])
    }
    profile.replaceSubrange(36 ..< 40, with: Array("acsp".utf8))
    profile.replaceSubrange(48 ..< 52, with: Array("Muu ".utf8))
    profile.replaceSubrange(52 ..< 56, with: Array("sRGB".utf8))
    profile.replaceUInt32BE(at: 64, with: 0)
    profile.replaceUInt32BE(at: 68, with: fixed16(0.9642))
    profile.replaceUInt32BE(at: 72, with: fixed16(1.0))
    profile.replaceUInt32BE(at: 76, with: fixed16(0.82491))
    profile.replaceSubrange(80 ..< 84, with: Array("Muu ".utf8))

    profile.appendUInt32BE(UInt32(tagPayloads.count))
    let tableOffset = profile.count
    profile.append(contentsOf: repeatElement(UInt8(0), count: tagPayloads.count * 12))

    for (index, item) in tagPayloads.enumerated() {
        profile.align(to: 4)
        let payloadOffset = profile.count
        profile.append(item.1)
        let entryOffset = tableOffset + index * 12
        profile.replaceSubrange(entryOffset ..< entryOffset + 4, with: Array(item.0.utf8))
        profile.replaceUInt32BE(at: entryOffset + 4, with: UInt32(payloadOffset))
        profile.replaceUInt32BE(at: entryOffset + 8, with: UInt32(item.1.count))
    }
    profile.replaceUInt32BE(at: 0, with: UInt32(profile.count))
    return profile
}

private func pathContains(_ edges: [VectorEdge], point: VectorPoint) -> Bool {
    var winding = 0
    for edge in edges {
        let cross = (edge.end.x - edge.start.x) * (point.y - edge.start.y)
            - (point.x - edge.start.x) * (edge.end.y - edge.start.y)
        if edge.start.y <= point.y {
            if edge.end.y > point.y, cross > 0 {
                winding += 1
            }
        } else if edge.end.y <= point.y, cross < 0 {
            winding -= 1
        }
    }
    return winding != 0
}

// One bit per sample (4x4 per pixel), filled by the nonzero winding rule with
// a scanline: crossings are sorted by x and the winding is updated while
// walking each sample row.
private func sampleMask(
    edges: [VectorEdge],
    artwork: TrustedSVGArtwork,
    canvas: IconCanvas,
    scale: Int
) -> [UInt8] {
    let pixelWidth = canvas.width * scale
    let pixelHeight = canvas.height * scale
    let sampleWidth = pixelWidth * samplesPerAxis
    let sampleHeight = pixelHeight * samplesPerAxis
    var mask = [UInt8](repeating: 0, count: sampleWidth * sampleHeight)

    for sampleY in 0 ..< sampleHeight {
        let y = (Double(sampleY) + 0.5) * Double(artwork.height) / Double(sampleHeight)
        var crossings: [(x: Double, delta: Int)] = []
        crossings.reserveCapacity(edges.count / 2)
        for edge in edges {
            if edge.start.y <= y, edge.end.y > y {
                let ratio = (y - edge.start.y) / (edge.end.y - edge.start.y)
                crossings.append((
                    x: edge.start.x + ratio * (edge.end.x - edge.start.x),
                    delta: 1
                ))
            } else if edge.start.y > y, edge.end.y <= y {
                let ratio = (y - edge.start.y) / (edge.end.y - edge.start.y)
                crossings.append((
                    x: edge.start.x + ratio * (edge.end.x - edge.start.x),
                    delta: -1
                ))
            }
        }
        crossings.sort {
            if $0.x == $1.x {
                return $0.delta < $1.delta
            }
            return $0.x < $1.x
        }
        var winding = crossings.reduce(0) { $0 + $1.delta }
        var crossingIndex = 0
        for sampleX in 0 ..< sampleWidth {
            let x = (Double(sampleX) + 0.5) * Double(artwork.width) / Double(sampleWidth)
            while crossingIndex < crossings.count, crossings[crossingIndex].x <= x {
                winding -= crossings[crossingIndex].delta
                crossingIndex += 1
            }
            if winding != 0 {
                mask[sampleY * sampleWidth + sampleX] = 1
            }
        }
    }
    return mask
}

private func rasterize(
    icon: ModeIcon,
    canvas: IconCanvas,
    scale: Int,
    appearance: ModeSurfaceAppearance
) throws -> Data {
    let pixelWidth = canvas.width * scale
    let pixelHeight = canvas.height * scale
    let sampleWidth = pixelWidth * samplesPerAxis
    let sampleCount = samplesPerAxis * samplesPerAxis
    let backgroundMask = sampleMask(
        edges: icon.artwork.background,
        artwork: icon.artwork,
        canvas: canvas,
        scale: scale
    )
    let glyphMask = sampleMask(
        edges: icon.artwork.glyph,
        artwork: icon.artwork,
        canvas: canvas,
        scale: scale
    )
    var result = Data()
    result.reserveCapacity(pixelWidth * pixelHeight * 4)

    for pixelY in 0 ..< pixelHeight {
        for pixelX in 0 ..< pixelWidth {
            var glyphSamples = 0
            var backgroundSamples = 0
            var glyphSamplesInsideBackground = 0
            for sampleY in 0 ..< samplesPerAxis {
                for sampleX in 0 ..< samplesPerAxis {
                    let maskIndex = (pixelY * samplesPerAxis + sampleY) * sampleWidth
                        + pixelX * samplesPerAxis + sampleX
                    if glyphMask[maskIndex] != 0 {
                        glyphSamples += 1
                    }
                    if backgroundMask[maskIndex] != 0 {
                        backgroundSamples += 1
                        if glyphMask[maskIndex] != 0 {
                            glyphSamplesInsideBackground += 1
                        }
                    }
                }
            }

            if appearance == .transparentBlackGlyph {
                if glyphSamples == 0 {
                    // Fully transparent pixels have canonical zero RGB bytes.
                    result.append(contentsOf: [0, 0, 0, 0])
                } else {
                    let alpha = glyphSamples * 255 / sampleCount
                    result.append(contentsOf: [0, 0, 0, UInt8(alpha)])
                }
                continue
            }

            if backgroundSamples == 0 {
                result.append(contentsOf: [0, 0, 0, 0])
                continue
            }
            let backgroundOnly = backgroundSamples - glyphSamplesInsideBackground
            let alpha = backgroundSamples * 255 / sampleCount
            let red = (255 * glyphSamplesInsideBackground + orange.red * backgroundOnly)
                / backgroundSamples
            let green = (255 * glyphSamplesInsideBackground + orange.green * backgroundOnly)
                / backgroundSamples
            let blue = (255 * glyphSamplesInsideBackground + orange.blue * backgroundOnly)
                / backgroundSamples
            result.append(contentsOf: [UInt8(red), UInt8(green), UInt8(blue), UInt8(alpha)])
        }
    }
    return result
}

private struct TIFFPage {
    let scale: Int
    let pixels: Data
}

private struct TIFFEntry {
    let tag: UInt16
    let type: UInt16
    let count: UInt32
    let value: UInt32
}

private func makeTIFF(
    icon: ModeIcon,
    canvas: IconCanvas,
    profile: Data,
    appearance: ModeSurfaceAppearance
) throws -> Data {
    let pages = try [1, 2].map {
        TIFFPage(
            scale: $0,
            pixels: try rasterize(
                icon: icon,
                canvas: canvas,
                scale: $0,
                appearance: appearance
            )
        )
    }
    let entryCount = 18
    let ifdSize = 2 + entryCount * 12 + 4
    let firstIFDOffset = 8
    let payloadStart = firstIFDOffset + ifdSize * pages.count
    var payload = Data()

    func appendPayload(_ value: Data, alignment: Int = 4) -> UInt32 {
        payload.align(to: alignment)
        let offset = payloadStart + payload.count
        payload.append(value)
        return UInt32(offset)
    }

    var pageEntries: [[TIFFEntry]] = []
    for (pageIndex, page) in pages.enumerated() {
        let width = canvas.width * page.scale
        let height = canvas.height * page.scale
        var bits = Data()
        for _ in 0 ..< 4 {
            bits.appendUInt16LE(8)
        }
        let bitsOffset = appendPayload(bits)

        var resolution = Data()
        resolution.appendUInt32LE(UInt32(72 * page.scale))
        resolution.appendUInt32LE(1)
        let xResolutionOffset = appendPayload(resolution)
        let yResolutionOffset = appendPayload(resolution)
        let profileOffset = appendPayload(profile)
        let pixelOffset = appendPayload(page.pixels)
        let pageNumber = UInt32(UInt16(pageIndex)) | (UInt32(UInt16(pages.count)) << 16)

        pageEntries.append([
            TIFFEntry(tag: 254, type: 4, count: 1, value: 0),
            TIFFEntry(tag: 256, type: 4, count: 1, value: UInt32(width)),
            TIFFEntry(tag: 257, type: 4, count: 1, value: UInt32(height)),
            TIFFEntry(tag: 258, type: 3, count: 4, value: bitsOffset),
            TIFFEntry(tag: 259, type: 3, count: 1, value: 1),
            TIFFEntry(tag: 262, type: 3, count: 1, value: 2),
            TIFFEntry(tag: 273, type: 4, count: 1, value: pixelOffset),
            TIFFEntry(tag: 274, type: 3, count: 1, value: 1),
            TIFFEntry(tag: 277, type: 3, count: 1, value: 4),
            TIFFEntry(tag: 278, type: 4, count: 1, value: UInt32(height)),
            TIFFEntry(tag: 279, type: 4, count: 1, value: UInt32(page.pixels.count)),
            TIFFEntry(tag: 282, type: 5, count: 1, value: xResolutionOffset),
            TIFFEntry(tag: 283, type: 5, count: 1, value: yResolutionOffset),
            TIFFEntry(tag: 284, type: 3, count: 1, value: 1),
            TIFFEntry(tag: 296, type: 3, count: 1, value: 2),
            TIFFEntry(tag: 297, type: 3, count: 2, value: pageNumber),
            TIFFEntry(tag: 338, type: 3, count: 1, value: 2),
            TIFFEntry(tag: 34_675, type: 7, count: UInt32(profile.count), value: profileOffset),
        ])
    }

    var result = Data()
    result.appendASCII("II")
    result.appendUInt16LE(42)
    result.appendUInt32LE(UInt32(firstIFDOffset))
    for (index, entries) in pageEntries.enumerated() {
        result.appendUInt16LE(UInt16(entries.count))
        for entry in entries.sorted(by: { $0.tag < $1.tag }) {
            result.appendUInt16LE(entry.tag)
            result.appendUInt16LE(entry.type)
            result.appendUInt32LE(entry.count)
            result.appendUInt32LE(entry.value)
        }
        let next = index + 1 < pageEntries.count ? firstIFDOffset + ifdSize * (index + 1) : 0
        result.appendUInt32LE(UInt32(next))
    }
    result.append(payload)
    return result
}

private func makeZlibStoredStream(_ payload: Data) -> Data {
    var stream = Data([0x78, 0x01])
    var offset = 0
    while offset < payload.count {
        let length = min(65_535, payload.count - offset)
        let isFinal = offset + length == payload.count
        stream.append(isFinal ? 1 : 0)
        stream.appendUInt16LE(UInt16(length))
        stream.appendUInt16LE(~UInt16(length))
        stream.append(payload[offset ..< offset + length])
        offset += length
    }
    stream.appendUInt32BE(adler32(payload))
    return stream
}

private func makePNG(
    pixels: Data,
    width: Int,
    height: Int,
    profile: Data
) -> Data {
    var scanlines = Data()
    scanlines.reserveCapacity(height * (1 + width * 4))
    for row in 0 ..< height {
        scanlines.append(0)
        let start = row * width * 4
        scanlines.append(pixels[start ..< start + width * 4])
    }

    var header = Data()
    header.appendUInt32BE(UInt32(width))
    header.appendUInt32BE(UInt32(height))
    header.append(contentsOf: [8, 6, 0, 0, 0])

    var colorProfile = Data("Muu sRGB".utf8)
    colorProfile.append(contentsOf: [0, 0])
    colorProfile.append(makeZlibStoredStream(profile))

    var result = Data([137, 80, 78, 71, 13, 10, 26, 10])
    result.appendPNGChunk(type: "IHDR", payload: header)
    result.appendPNGChunk(type: "iCCP", payload: colorProfile)
    result.appendPNGChunk(type: "IDAT", payload: makeZlibStoredStream(scanlines))
    result.appendPNGChunk(type: "IEND", payload: Data())
    return result
}

private func makeICNS(icon: ModeIcon, profile: Data) throws -> Data {
    // Standard ICNS type semantics: the same pixel dimensions may represent
    // different logical sizes. Keep both tags so Retina consumers do not
    // reinterpret a 1x representation as a scaled fallback.
    let representations: [
        (type: String, logicalSize: Int, displayScale: Int, rasterScale: Int)
    ] = [
        ("icp4", 16, 1, 1),
        ("ic11", 16, 2, 2),
        ("icp5", 32, 1, 2),
        ("ic12", 32, 2, 4),
        ("ic07", 128, 1, 8),
        ("ic13", 128, 2, 16),
        ("ic08", 256, 1, 16),
        ("ic14", 256, 2, 32),
    ]
    var chunks = Data()
    for representation in representations {
        precondition(
            componentICNSCanvas.width * representation.rasterScale
                == representation.logicalSize * representation.displayScale
        )
        let pixels = try rasterize(
            icon: icon,
            canvas: componentICNSCanvas,
            scale: representation.rasterScale,
            appearance: .fullColorTile
        )
        let width = componentICNSCanvas.width * representation.rasterScale
        let height = componentICNSCanvas.height * representation.rasterScale
        let png = makePNG(
            pixels: pixels,
            width: width,
            height: height,
            profile: profile
        )
        chunks.appendASCII(representation.type)
        chunks.appendUInt32BE(UInt32(png.count + 8))
        chunks.append(png)
    }

    var result = Data()
    result.appendASCII("icns")
    result.appendUInt32BE(UInt32(chunks.count + 8))
    result.append(chunks)
    return result
}

// The plain, Selected and Button variants of a mode icon are the same bytes:
// Info.plist names a file for each role, and all three are template masks.
private func writeAssets(icon: ModeIcon, profile: Data, to directory: URL) throws {
    let templateMaskTIFF = try makeTIFF(
        icon: icon,
        canvas: modeCanvas,
        profile: profile,
        appearance: .transparentBlackGlyph
    )
    for suffix in ["", "Selected", "Button"] {
        try templateMaskTIFF.write(
            to: directory.appendingPathComponent("MuuMode\(icon.name)\(suffix).tiff"),
            options: .atomic
        )
    }
}

private func writeComponentAssets(icon: ModeIcon, profile: Data, to directory: URL) throws {
    let tiff = try makeTIFF(
        icon: icon,
        canvas: componentTIFFCanvas,
        profile: profile,
        appearance: .fullColorTile
    )
    let icns = try makeICNS(icon: icon, profile: profile)
    try tiff.write(
        to: directory.appendingPathComponent("MuuInputMethod.tiff"),
        options: .atomic
    )
    try icns.write(
        to: directory.appendingPathComponent("MuuIME.icns"),
        options: .atomic
    )
}

private func requireParserFailure(
    _ name: String,
    svg: String,
    spec: TrustedSVGSpec
) throws {
    do {
        _ = try parseArtwork(Data(svg.utf8), spec: spec)
    } catch {
        return
    }
    throw GenerationError.encoding("trusted SVG parser accepted \(name)")
}

private func runParserSelfTest() throws {
    let spec = TrustedSVGSpec(filename: "fixture.svg", width: 2, height: 2)
    let valid = """
    <?xml version="1.0" encoding="UTF-8"?>
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 2" width="2" height="2">
      <path fill="#F28C28" d="M 0,0 H 2 V 2 H 0 Z"/>
      <path fill="#FFFFFF" fill-rule="nonzero" d="M 0.5,0.5 L 1.5,0.5 L 1.5,1.5 L 0.5,1.5 Z"/>
    </svg>
    """
    let artwork = try parseArtwork(Data(valid.utf8), spec: spec)
    guard artwork.width == 2,
          artwork.height == 2,
          pathContains(artwork.background, point: VectorPoint(x: 0.25, y: 0.25)),
          pathContains(artwork.glyph, point: VectorPoint(x: 1, y: 1)),
          !pathContains(artwork.glyph, point: VectorPoint(x: 0.25, y: 0.25))
    else {
        throw GenerationError.encoding("trusted SVG parser self-test produced wrong geometry")
    }

    // Positive: a cubic (C) path must still produce filled geometry. With the
    // original M/L/H/V/Z cases this replaces the old check that those command
    // letters appear in the parser source.
    let cubic = valid.replacingOccurrences(
        of: "M 0,0 H 2",
        with: "M 0,0 C 0.5,0 1.5,0 2,0"
    )
    guard cubic.contains(" C "),
          pathContains(
              try parseArtwork(Data(cubic.utf8), spec: spec).background,
              point: VectorPoint(x: 0.25, y: 0.25)
          )
    else {
        throw GenerationError.encoding("supported cubic SVG lost its filled geometry")
    }

    // Negatives fed to the real parser: any DTD or entity (replacing the
    // `shouldResolveExternalEntities = false` spelling check), a paint
    // reference and a stylesheet. Each must be rejected, not just unread.
    for (name, svg) in [
        ("internal DTD", valid.replacingOccurrences(
            of: "<svg ", with: "<!DOCTYPE svg [<!ENTITY unused 'value'>]><svg "
        )),
        ("external entity", valid.replacingOccurrences(
            of: "<svg ",
            with: "<!DOCTYPE svg [<!ENTITY external SYSTEM 'file:///path/to/not-read.svg'>]><svg "
        )),
        ("paint reference", valid.replacingOccurrences(of: "#FFFFFF", with: "url(#paint)")),
        ("stylesheet", valid.replacingOccurrences(of: "</svg>", with: "<style>path { fill: red }</style></svg>")),
    ] {
        try requireParserFailure(name, svg: svg, spec: spec)
    }

    try requireParserFailure(
        "text element fixture",
        svg: valid.replacingOccurrences(of: "</svg>", with: "<text>M</text></svg>"),
        spec: spec
    )
    try requireParserFailure(
        "relative command fixture",
        svg: valid.replacingOccurrences(of: "M 0.5,0.5", with: "m 0.5,0.5"),
        spec: spec
    )
    try requireParserFailure(
        "transform fixture",
        svg: valid.replacingOccurrences(
            of: "fill=\"#FFFFFF\"",
            with: "fill=\"#FFFFFF\" transform=\"scale(1)\""
        ),
        spec: spec
    )
    try requireParserFailure(
        "external image fixture",
        svg: valid.replacingOccurrences(
            of: "</svg>",
            with: "<image href=\"https://example.invalid/a.png\"/></svg>"
        ),
        spec: spec
    )
    try requireParserFailure(
        "wrong fill fixture",
        svg: valid.replacingOccurrences(of: "#FFFFFF", with: "#FFFFFE"),
        spec: spec
    )
    try requireParserFailure(
        "wrong viewBox fixture",
        svg: valid.replacingOccurrences(of: "viewBox=\"0 0 2 2\"", with: "viewBox=\"0 0 3 2\""),
        spec: spec
    )
}

do {
    if CommandLine.arguments == [CommandLine.arguments[0], "--self-test"] {
        try runParserSelfTest()
        exit(0)
    }
    guard CommandLine.arguments.count == 3 else {
        throw GenerationError.usage
    }
    let artworkDirectory = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    let outputDirectory = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
    try FileManager.default.createDirectory(
        at: outputDirectory,
        withIntermediateDirectories: true
    )
    let hiraganaSpec = TrustedSVGSpec(filename: "MuuHiragana.svg", width: 22, height: 16)
    let romanSpec = TrustedSVGSpec(filename: "MuuRoman.svg", width: 22, height: 16)
    let componentSpec = TrustedSVGSpec(filename: "MuuComponent.svg", width: 17, height: 17)
    let icons = [
        ModeIcon(
            name: "Hiragana",
            artwork: try loadArtwork(hiraganaSpec, from: artworkDirectory)
        ),
        ModeIcon(
            name: "Roman",
            artwork: try loadArtwork(romanSpec, from: artworkDirectory)
        ),
    ]
    let componentIcon = ModeIcon(
        name: "Component",
        artwork: try loadArtwork(componentSpec, from: artworkDirectory)
    )
    let profile = makeSRGBProfile()
    for icon in icons {
        try writeAssets(icon: icon, profile: profile, to: outputDirectory)
    }
    try writeComponentAssets(icon: componentIcon, profile: profile, to: outputDirectory)
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8))
    exit(1)
}
