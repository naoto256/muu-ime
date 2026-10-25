import Foundation

#if MUU_TEST
// Synthetic learning snapshots only. Production Swift transports opaque files.
struct PersonalFrequencySnapshot: Codable, Equatable {
    static let currentSchemaVersion = 1
    static let empty = Self(
        schemaVersion: currentSchemaVersion,
        dictionaryPayloadSHA256: nil,
        dictionaryTokenCount: nil,
        observationCount: 0,
        entries: []
    )

    let schemaVersion: Int
    let dictionaryPayloadSHA256: String?
    let dictionaryTokenCount: UInt64?
    let observationCount: UInt64
    let entries: [PersonalFrequencyEntry]

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case dictionaryPayloadSHA256 = "dictionary_payload_sha256"
        case dictionaryTokenCount = "dictionary_token_count"
        case observationCount = "observation_count"
        case entries
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(schemaVersion, forKey: .schemaVersion)
        if let dictionaryPayloadSHA256 {
            try container.encode(dictionaryPayloadSHA256, forKey: .dictionaryPayloadSHA256)
        } else {
            try container.encodeNil(forKey: .dictionaryPayloadSHA256)
        }
        if let dictionaryTokenCount {
            try container.encode(dictionaryTokenCount, forKey: .dictionaryTokenCount)
        } else {
            try container.encodeNil(forKey: .dictionaryTokenCount)
        }
        try container.encode(observationCount, forKey: .observationCount)
        try container.encode(entries, forKey: .entries)
    }
}

struct PersonalFrequencyEntry: Codable, Equatable {
    let dictionaryTokenID: UInt64
    let count: UInt16

    enum CodingKeys: String, CodingKey {
        case dictionaryTokenID = "dictionary_token_id"
        case count
    }
}

struct UserDictionaryRecord: Codable, Equatable {
    let reading: String
    let surface: String
    let partOfSpeech: MuuDictionaryPartOfSpeech
    let priority: UserDictionaryPriority

    enum CodingKeys: String, CodingKey {
        case reading
        case surface
        case partOfSpeech = "part_of_speech"
        case priority
    }
}

#endif
