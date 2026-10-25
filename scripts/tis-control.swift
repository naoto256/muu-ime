import Carbon
import Foundation

enum TISError: Error, CustomStringConvertible {
    case usage
    case osStatus(OSStatus)

    var description: String {
        switch self {
        case .usage:
            "usage: tis-control.swift register <app-path> | migrate-two-mode <bundle-id> | verify-two-mode <bundle-id> | disable <bundle-id>"
        case let .osStatus(status):
            "Text Input Source operation failed with OSStatus \(status)"
        }
    }
}

func register(path: String) throws {
    let status = TISRegisterInputSource(URL(fileURLWithPath: path) as CFURL)
    guard status == noErr else { throw TISError.osStatus(status) }
}

// Muu ships two input modes. Older builds also registered the three below;
// migration disables them so they cannot be selected any more.
private let supportedSuffixes = ["Hiragana", "Roman"]
private let removedSuffixes = ["Katakana", "HalfWidthKana", "FullWidthRoman"]

private func tisString(_ source: TISInputSource, _ key: CFString) -> String? {
    guard let pointer = TISGetInputSourceProperty(source, key) else {
        return nil
    }
    return Unmanaged<AnyObject>
        .fromOpaque(pointer)
        .takeUnretainedValue() as? String
}

private func tisBool(_ source: TISInputSource, _ key: CFString) -> Bool {
    guard let pointer = TISGetInputSourceProperty(source, key) else {
        return false
    }
    return (Unmanaged<AnyObject>
        .fromOpaque(pointer)
        .takeUnretainedValue() as? NSNumber)?.boolValue ?? false
}

private func sources(bundleIdentifier: String, includeAll: Bool) -> [TISInputSource] {
    let filter = [kTISPropertyBundleID as String: bundleIdentifier] as CFDictionary
    guard let sourceList = TISCreateInputSourceList(filter, includeAll) else {
        return []
    }
    return sourceList.takeRetainedValue() as! [TISInputSource]
}

// All installed sources of the bundle by source ID, enabled or not. A source
// without an ID, or a duplicate ID, is treated as an error rather than guessed.
private func indexedSources(
    bundleIdentifier: String
) throws -> [String: TISInputSource] {
    var result: [String: TISInputSource] = [:]
    for source in sources(bundleIdentifier: bundleIdentifier, includeAll: true) {
        guard let sourceID = tisString(source, kTISPropertyInputSourceID),
              result[sourceID] == nil
        else {
            throw TISError.osStatus(OSStatus(paramErr))
        }
        result[sourceID] = source
    }
    return result
}

// Requires both current modes to be installed. If the user is on a removed
// mode, switch to Hiragana first, then disable every removed mode.
func migrateTwoMode(bundleIdentifier: String) throws {
    let bySourceID = try indexedSources(bundleIdentifier: bundleIdentifier)
    guard let hiragana = bySourceID["\(bundleIdentifier).Hiragana"],
          bySourceID["\(bundleIdentifier).Roman"] != nil
    else {
        throw TISError.osStatus(OSStatus(paramErr))
    }

    if let current = TISCopyCurrentKeyboardInputSource()?.takeRetainedValue(),
       let currentID = tisString(current, kTISPropertyInputSourceID),
       removedSuffixes.map({ "\(bundleIdentifier).\($0)" }).contains(currentID)
    {
        let status = TISSelectInputSource(hiragana)
        guard status == noErr else { throw TISError.osStatus(status) }
    }

    for suffix in removedSuffixes {
        guard let legacy = bySourceID["\(bundleIdentifier).\(suffix)"] else {
            continue
        }
        let status = TISDisableInputSource(legacy)
        guard status == noErr else { throw TISError.osStatus(status) }
    }
}

// True when both current modes exist and every removed mode is absent or
// neither enabled nor selectable. The installer retries registration up to
// ten times until this holds.
func verifyTwoMode(bundleIdentifier: String) -> Bool {
    guard let sourceByID = try? indexedSources(bundleIdentifier: bundleIdentifier) else {
        return false
    }
    guard supportedSuffixes.allSatisfy({
        sourceByID["\(bundleIdentifier).\($0)"] != nil
    }) else {
        return false
    }
    return removedSuffixes.allSatisfy { suffix in
        guard let legacy = sourceByID["\(bundleIdentifier).\(suffix)"] else {
            return true
        }
        return !tisBool(legacy, kTISPropertyInputSourceIsEnabled)
            && !tisBool(legacy, kTISPropertyInputSourceIsSelectCapable)
    }
}

func disable(bundleIdentifier: String) throws {
    let filter = [kTISPropertyBundleID as String: bundleIdentifier] as CFDictionary
    guard let sourceList = TISCreateInputSourceList(filter, false) else { return }
    let sources = sourceList.takeRetainedValue() as! [TISInputSource]
    for source in sources {
        let status = TISDisableInputSource(source)
        guard status == noErr else { throw TISError.osStatus(status) }
    }
}

// Exit status: 0 on success, 1 when verify-two-mode does not hold, 64 on a
// usage error or a failed TIS call.
do {
    guard CommandLine.arguments.count == 3 else { throw TISError.usage }
    switch CommandLine.arguments[1] {
    case "register":
        try register(path: CommandLine.arguments[2])
    case "migrate-two-mode":
        try migrateTwoMode(bundleIdentifier: CommandLine.arguments[2])
    case "verify-two-mode":
        exit(verifyTwoMode(bundleIdentifier: CommandLine.arguments[2]) ? 0 : 1)
    case "disable":
        try disable(bundleIdentifier: CommandLine.arguments[2])
    default:
        throw TISError.usage
    }
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8))
    exit(64)
}
