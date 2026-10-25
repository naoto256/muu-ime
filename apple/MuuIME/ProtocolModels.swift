import Foundation

/// The framing for every request and view on the C ABI: a format tag (1 =
/// JSON), the schema version as a little-endian UInt16, then the JSON body.
/// A view with another tag or version is refused rather than guessed at.
enum MuuEnvelope {
    static let formatTagJSON: UInt8 = 1
    static let schemaVersion: UInt16 = 1
    // Same bound as a request. The largest baseline scale view was 1,061,864
    // bytes; 229 ordinary trace views were at most 18,725 bytes.
    static let maximumViewBytes = 16 * 1024 * 1024

    enum DecodedView {
        case complete(ViewState)
        case recovered(ViewState)
        case commitOnly(String)
    }

    static func encode<T: Encodable>(_ value: T) throws -> Data {
        var data = Data([
            formatTagJSON,
            UInt8(schemaVersion & 0x00ff),
            UInt8((schemaVersion & 0xff00) >> 8),
        ])
        data.append(try JSONEncoder().encode(value))
        return data
    }

    static func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        try JSONDecoder().decode(type, from: payload(data))
    }

    private static func payload(_ data: Data) throws -> Data {
        guard data.count >= 3 else { throw MuuProtocolError.shortEnvelope }
        guard data[0] == formatTagJSON else { throw MuuProtocolError.unsupportedFormat }
        let version = UInt16(data[1]) | (UInt16(data[2]) << 8)
        guard version == schemaVersion else { throw MuuProtocolError.unsupportedSchema }
        return data.dropFirst(3)
    }

    static func decodeView(_ data: Data) throws -> ViewState {
        guard data.count <= maximumViewBytes else { throw MuuProtocolError.oversizedView }
        return try decode(ViewState.self, from: data)
    }

    // Only an OK ABI response may use this recovery. Header and JSON decoding
    // remain mandatory; no substring search or request replay recovers text.
    //
    // A commit is recovered only when all four hold: the ABI status is OK,
    // the envelope has the supported format and schema, the payload is valid
    // JSON, and `commit_text` decodes as a String. The full decode failed only
    // on display fields. Nothing else is rescued: a non-OK status, an unknown
    // format or version, short or oversized input, malformed JSON, or a
    // commit of the wrong type all throw as before. The recovery widens no
    // trust boundary; it only stops a broken candidate list or error from
    // taking a valid commit down with it.
    //
    // `.recovered` keeps the next preedit, caret, selection and mode when they
    // decode and their ranges check out. `.commitOnly` is the last resort when
    // they do not: no unverified range ever reaches AppKit.
    static func decodeViewResponse(_ data: Data, recoverCommit: Bool) throws -> DecodedView {
        guard data.count <= maximumViewBytes else { throw MuuProtocolError.oversizedView }
        let body = try payload(data)
        let decoder = JSONDecoder()
        do {
            return .complete(try decoder.decode(ViewState.self, from: body))
        } catch {
            guard recoverCommit else { throw error }
            let commit = try decoder.decode(CommittedText.self, from: body).commitText
            if let essential = try? decoder.decode(EssentialView.self, from: body),
                let view = essential.validated(commit: commit) {
                return .recovered(view)
            }
            return .commitOnly(commit)
        }
    }

    private struct CommittedText: Decodable {
        let commitText: String
        enum CodingKeys: String, CodingKey { case commitText = "commit_text" }
    }

    private struct EssentialView: Decodable {
        let preedit: [PreeditRun]
        let caretUTF16: Int
        let selection: TextRange?
        let mode: InputMode
        enum CodingKeys: String, CodingKey {
            case preedit, selection, mode
            case caretUTF16 = "caret_utf16"
        }

        // Every range is checked against the runs before it can become an
        // NSRange: contiguous runs whose lengths match their text, then caret
        // and selection inside them. `consumed` is true because Rust already
        // applied the key; the request is never sent again.
        func validated(commit: String) -> ViewState? {
            var length = 0
            for run in preedit {
                guard run.utf16Start == length,
                    run.utf16Length == run.text.utf16.count,
                    run.utf16Length <= Int.max - length else { return nil }
                length += run.utf16Length
            }
            guard caretUTF16 >= 0, caretUTF16 <= length else { return nil }
            if let selection {
                guard selection.start >= 0, selection.start <= length,
                    selection.len >= 0, selection.len <= length - selection.start else { return nil }
            }
            return ViewState(preedit: preedit, caretUTF16: caretUTF16, selection: selection,
                segments: nil, candidates: nil, commitText: commit, mode: mode,
                compositionModeTransition: nil, poisoned: nil, consumed: true, error: nil)
        }
    }
}

enum MuuProtocolError: Error {
    case shortEnvelope
    case unsupportedFormat
    case unsupportedSchema
    case oversizedView
}

/// Interpretation of dictionary ACKs only. The stores keep their own save
/// order and recovery state machines. An unknown code never proves that a
/// publication or rollback left Core unchanged.
///
/// The one place that maps ACK values, so the three stores can no longer
/// disagree on what a code means: 0 success; 2 BAD_ARG, 3 CREATE_FAILED,
/// 4 DECODE_ERR and 6 INTERNAL are refusals that changed nothing; 8 is a
/// refusal against a revision that has moved; 9 BUSY. CORE_INVALID (7) and
/// any value not listed, negatives included, are ambiguous.
///
/// Only the value mapping is shared. What a store does with it stays its
/// own on purpose. User passes `allowBusy: false`: it serializes its
/// operations and holds Core's only prepared slot, so a BUSY from its
/// publish is unexpected and it stops. Imported and Postal pass `true`,
/// where BUSY is a known refusal.
enum DictionaryAcknowledgement: Equatable {
    case success, refused, revisionMismatch, busy, ambiguous

    // The values come from muu_ffi.h's MUU_RESULT_* constants, not literals, so
    // the header is the one source of result numbers.
    init(_ status: Int32) {
        switch status {
        case MUU_RESULT_OK: self = .success
        case MUU_RESULT_BAD_ARG, MUU_RESULT_CREATE_FAILED, MUU_RESULT_DECODE_ERR, MUU_RESULT_INTERNAL: self = .refused
        case MUU_RESULT_REVISION_MISMATCH: self = .revisionMismatch
        case MUU_RESULT_BUSY: self = .busy
        default: self = .ambiguous
        }
    }

    /// Whether a publish ACK proves that nothing was published. When this is
    /// false and the ACK is not success, the store stops editing rather than
    /// guess.
    func isDefinitelyUnpublished(allowBusy: Bool) -> Bool {
        self == .refused || self == .revisionMismatch || (allowBusy && self == .busy)
    }

    // A revision conflict is definite nonpublication, but retrying against
    // that changed revision could destroy another operation's result.
    var canRetryRecovery: Bool { self == .refused || self == .busy }
}

/// A native prepare failed locally after acquiring a ticket and already
/// attempted its cancellation. Carry ACK 0 too: a Store may hold this ticket
/// from an earlier supply step. Its completion must not cancel it again.
/// The original error remains the screen error; a non-OK ACK also stops edits.
///
/// A thrown error carries one value, and the store needs three facts: the
/// cause for its screen, that this ticket's cancel is already spent, and
/// whether Core may still hold the reservation. Neither the original error
/// nor a cancel error alone says all of them.
struct DictionaryPreparationFailure: Error {
    let original: Error
    let cancellationStatus: Int32
}

struct EventRequest: Encodable {
    let kind = "key"
    let key: KeyEvent
}

struct KeyEvent: Encodable {
    let eventType: String
    let keycode: UInt16
    let modifiers: UInt64
    let charactersUTF8: String?
    let charactersIgnoringModifiers: String?
    let isRepeat: Bool

    enum CodingKeys: String, CodingKey {
        case eventType = "event_type"
        case keycode
        case modifiers
        case charactersUTF8 = "characters_utf8"
        case charactersIgnoringModifiers = "characters_ignoring_modifiers"
        case isRepeat = "is_repeat"
    }
}

struct CommandRequest: Encodable {
    let kind = "command"
    let command: String
    let arg: CommandArgument?

    init(command: String, arg: CommandArgument? = nil) {
        self.command = command
        self.arg = arg
    }
}

struct CommandArgument: Encodable {
    let id: String?
    let direction: String?
    let mode: String?

    static func candidate(id: String) -> Self {
        Self(id: id, direction: nil, mode: nil)
    }

    static func direction(_ direction: String) -> Self {
        Self(id: nil, direction: direction, mode: nil)
    }

    static func mode(_ mode: InputMode) -> Self {
        Self(id: nil, direction: nil, mode: mode.rawValue)
    }
}

/// Stable exchange-format identifiers pinned to Mozc's `user_pos.def`.
enum MuuDictionaryPartOfSpeech: String, CaseIterable, Codable, Equatable {
    case noPos = "NO_POS"
    case noun = "NOUN"
    case abbreviation = "ABBREVIATION"
    case suggestionOnly = "SUGGESTION_ONLY"
    case properNoun = "PROPER_NOUN"
    case personalName = "PERSONAL_NAME"
    case familyName = "FAMILY_NAME"
    case firstName = "FIRST_NAME"
    case organizationName = "ORGANIZATION_NAME"
    case placeName = "PLACE_NAME"
    case saIrregularConjugationNoun = "SA_IRREGULAR_CONJUGATION_NOUN"
    case adjectiveVerbalNoun = "ADJECTIVE_VERBAL_NOUN"
    case number = "NUMBER"
    case alphabet = "ALPHABET"
    case symbol = "SYMBOL"
    case emoticon = "EMOTICON"
    case adverb = "ADVERB"
    case prenounAdjectival = "PRENOUN_ADJECTIVAL"
    case conjunction = "CONJUNCTION"
    case interjection = "INTERJECTION"
    case prefix = "PREFIX"
    case counterSuffix = "COUNTER_SUFFIX"
    case genericSuffix = "GENERIC_SUFFIX"
    case personNameSuffix = "PERSON_NAME_SUFFIX"
    case placeNameSuffix = "PLACE_NAME_SUFFIX"
    case waGroup1Verb = "WA_GROUP1_VERB"
    case kaGroup1Verb = "KA_GROUP1_VERB"
    case saGroup1Verb = "SA_GROUP1_VERB"
    case taGroup1Verb = "TA_GROUP1_VERB"
    case naGroup1Verb = "NA_GROUP1_VERB"
    case maGroup1Verb = "MA_GROUP1_VERB"
    case raGroup1Verb = "RA_GROUP1_VERB"
    case gaGroup1Verb = "GA_GROUP1_VERB"
    case baGroup1Verb = "BA_GROUP1_VERB"
    case haGroup1Verb = "HA_GROUP1_VERB"
    case group2Verb = "GROUP2_VERB"
    case kuruGroup3Verb = "KURU_GROUP3_VERB"
    case suruGroup3Verb = "SURU_GROUP3_VERB"
    case zuruGroup3Verb = "ZURU_GROUP3_VERB"
    case ruGroup3Verb = "RU_GROUP3_VERB"
    case adjective = "ADJECTIVE"
    case sentenceEndingParticle = "SENTENCE_ENDING_PARTICLE"
    case punctuation = "PUNCTUATION"
    case freeStandingWord = "FREE_STANDING_WORD"
    case suppressionWord = "SUPPRESSION_WORD"
}

enum UserDictionaryPriority: String, Codable, CaseIterable, Equatable {
    case high
    case normal
    case low

    var displayName: String {
        switch self {
        case .high: "高"
        case .normal: "標準"
        case .low: "低"
        }
    }
}

/// Rust's whole answer to one request. Text, ranges and the caret are Rust's
/// values in UTF-16 units and are applied as given. `consumed` says whether
/// the key was used; a missing `poisoned` means false.
struct ViewState: Decodable {
    let preedit: [PreeditRun]
    let caretUTF16: Int
    let selection: TextRange?
    let segments: [SegmentView]?
    let candidates: CandidateList?
    let commitText: String?
    let mode: InputMode
    let compositionModeTransition: CompositionModeTransition?
    let poisoned: Bool?
    let consumed: Bool
    let error: ViewError?

    enum CodingKeys: String, CodingKey {
        case preedit
        case caretUTF16 = "caret_utf16"
        case selection
        case segments
        case candidates
        case commitText = "commit_text"
        case mode
        case compositionModeTransition = "composition_mode_transition"
        case poisoned
        case consumed
        case error
    }
}

enum CompositionModeTransition: String, Decodable {
    case reading
    case literal
}

struct PreeditRun: Decodable {
    let text: String
    let utf16Start: Int
    let utf16Length: Int
    let isFocused: Bool
    let isConverted: Bool

    enum CodingKeys: String, CodingKey {
        case text
        case utf16Start = "utf16_start"
        case utf16Length = "utf16_len"
        case isFocused = "is_focused"
        case isConverted = "is_converted"
    }
}

struct TextRange: Decodable {
    let start: Int
    let len: Int
}

// Presentation reads only whether segments exist and how many. Rust owns the
// boundaries and each segment's selected candidate; do not mirror them in
// Swift. An empty Decodable accepts any element, even a number or null, so
// this does not validate segment contents.
struct SegmentView: Decodable {}

struct CandidateList: Decodable {
    let items: [CandidateItem]
    let selectedIndex: Int

    enum CodingKeys: String, CodingKey {
        case items
        case selectedIndex = "selected_index"
    }
}

struct CandidateItem: Decodable {
    let id: String
    let text: String
    let annotation: String?
}

enum InputMode: String, Decodable {
    case hiragana
    case fullKatakana = "full_katakana"
    case halfKatakana = "half_katakana"
    case fullAlnum = "full_alnum"
    case halfAlnum = "half_alnum"

    // The system's identifiers for the Japanese input modes. Info.plist
    // declares only Hiragana and Roman as menu modes; the other three map
    // Rust's remaining modes for value(forTag:). Any other identifier is not
    // one of Muu's modes.
    init?(inputModeIdentifier: String) {
        switch inputModeIdentifier {
        case "com.apple.inputmethod.Japanese.Hiragana":
            self = .hiragana
        case "com.apple.inputmethod.Japanese.Katakana":
            self = .fullKatakana
        case "com.apple.inputmethod.Japanese.HalfWidthKana":
            self = .halfKatakana
        case "com.apple.inputmethod.Japanese.FullWidthRoman":
            self = .fullAlnum
        case "com.apple.inputmethod.Japanese.Roman":
            self = .halfAlnum
        default:
            return nil
        }
    }

    var inputModeIdentifier: String {
        switch self {
        case .hiragana: "com.apple.inputmethod.Japanese.Hiragana"
        case .fullKatakana: "com.apple.inputmethod.Japanese.Katakana"
        case .halfKatakana: "com.apple.inputmethod.Japanese.HalfWidthKana"
        case .fullAlnum: "com.apple.inputmethod.Japanese.FullWidthRoman"
        case .halfAlnum: "com.apple.inputmethod.Japanese.Roman"
        }
    }
}

struct ViewError: Decodable {
    let code: String
    // An object, as Rust's ViewError type guarantees. A mismatch fails the
    // full ViewState decode; bounded recovery for an OK response can retain
    // a separately decoded commit without trusting this display metadata.
    let params: [String: JSONValue]
}

enum JSONValue: Decodable {
    case string(String)
    case number(Double)
    case boolean(Bool)
    case object([String: JSONValue])
    case array([JSONValue])
    case null

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .boolean(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([String: JSONValue].self) {
            self = .object(value)
        } else {
            self = .array(try container.decode([JSONValue].self))
        }
    }
}
