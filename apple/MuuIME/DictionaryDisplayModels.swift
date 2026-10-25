import Foundation

// Display labels and display-only metadata. File codecs and POS/base-form
// validation belong to Rust; these types never decode a stored dictionary.
extension MuuDictionaryPartOfSpeech {
    var displayName: String {
        switch self {
        case .noPos: "品詞なし"
        case .noun: "名詞"
        case .abbreviation: "短縮よみ"
        case .suggestionOnly: "サジェストのみ"
        case .properNoun: "固有名詞"
        case .personalName: "人名"
        case .familyName: "姓"
        case .firstName: "名"
        case .organizationName: "組織"
        case .placeName: "地名"
        case .saIrregularConjugationNoun: "名詞サ変"
        case .adjectiveVerbalNoun: "名詞形動"
        case .number: "数"
        case .alphabet: "アルファベット"
        case .symbol: "記号"
        case .emoticon: "顔文字"
        case .adverb: "副詞"
        case .prenounAdjectival: "連体詞"
        case .conjunction: "接続詞"
        case .interjection: "感動詞"
        case .prefix: "接頭語"
        case .counterSuffix: "助数詞"
        case .genericSuffix: "接尾一般"
        case .personNameSuffix: "接尾人名"
        case .placeNameSuffix: "接尾地名"
        case .waGroup1Verb: "動詞ワ行五段"
        case .kaGroup1Verb: "動詞カ行五段"
        case .saGroup1Verb: "動詞サ行五段"
        case .taGroup1Verb: "動詞タ行五段"
        case .naGroup1Verb: "動詞ナ行五段"
        case .maGroup1Verb: "動詞マ行五段"
        case .raGroup1Verb: "動詞ラ行五段"
        case .gaGroup1Verb: "動詞ガ行五段"
        case .baGroup1Verb: "動詞バ行五段"
        case .haGroup1Verb: "動詞ハ行四段"
        case .group2Verb: "動詞一段"
        case .kuruGroup3Verb: "動詞カ変"
        case .suruGroup3Verb: "動詞サ変"
        case .zuruGroup3Verb: "動詞ザ変"
        case .ruGroup3Verb: "動詞ラ変"
        case .adjective: "形容詞"
        case .sentenceEndingParticle: "終助詞"
        case .punctuation: "句読点"
        case .freeStandingWord: "独立語"
        case .suppressionWord: "抑制単語"
        }
    }

}

struct MuuDictionaryMetadata: Codable, Equatable {
    let id: String
    let name: String
    let version: String
    let author: String
    let license: String
    let provenance: String
}
