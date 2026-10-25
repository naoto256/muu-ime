use muu_dict::{CoarsePos, LeftContextId, RightContextId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserPosBehavior {
    Runtime,
    Abbreviation,
    DormantSuggestion,
    Suppression,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserPosForm {
    pub form_id: usize,
    pub reading_suffix: &'static str,
    pub surface_suffix: &'static str,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos: CoarsePos,
    pub right_pos: CoarsePos,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserPosDefinition {
    pub id: &'static str,
    pub word_cost: i32,
    pub behavior: UserPosBehavior,
    pub base_reading_suffix: &'static str,
    pub base_surface_suffix: &'static str,
    pub forms: &'static [UserPosForm],
}

impl UserPosDefinition {
    // None when the reading or surface lacks the base-form suffix. The
    // suffixes come from the pinned conjugation table, which for
    // HA_GROUP1_VERB expects ふ/ふ while the user-dictionary codec accepts
    // う/ふ, so such entries never reach the effective layer (docs/src/internals/licenses-and-limitations.md「動作の制限」).
    // Forms-free POS (suggestion-only, suppression) expand to nothing.
    pub fn expand(self, reading: &str, surface: &str) -> Option<Vec<UserPosExpansion>> {
        let reading_stem = reading.strip_suffix(self.base_reading_suffix)?;
        let surface_stem = surface.strip_suffix(self.base_surface_suffix)?;
        Some(
            self.forms
                .iter()
                .map(|form| UserPosExpansion {
                    form_id: form.form_id,
                    reading: format!("{reading_stem}{}", form.reading_suffix),
                    surface: format!("{surface_stem}{}", form.surface_suffix),
                    left_id: form.left_id,
                    right_id: form.right_id,
                    left_pos: form.left_pos,
                    right_pos: form.right_pos,
                    word_cost: self.word_cost,
                })
                .collect(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserPosExpansion {
    pub form_id: usize,
    pub reading: String,
    pub surface: String,
    pub left_id: LeftContextId,
    pub right_id: RightContextId,
    pub left_pos: CoarsePos,
    pub right_pos: CoarsePos,
    pub word_cost: i32,
}

include!(concat!(env!("OUT_DIR"), "/user_pos_generated.rs"));

pub fn user_pos_definition(id: &str) -> Option<&'static UserPosDefinition> {
    USER_POS_DEFINITIONS
        .iter()
        .find(|definition| definition.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_authority_exposes_every_supported_pos_and_runtime_form() {
        assert_eq!(USER_POS_DEFINITIONS.len(), 45);
        assert_eq!(
            USER_POS_DEFINITIONS
                .iter()
                .map(|definition| definition.forms.len())
                .sum::<usize>(),
            162
        );
        assert_eq!(
            user_pos_definition("SUGGESTION_ONLY").unwrap().behavior,
            UserPosBehavior::DormantSuggestion
        );
        assert!(
            user_pos_definition("SUGGESTION_ONLY")
                .unwrap()
                .forms
                .is_empty()
        );
        assert_eq!(
            user_pos_definition("SUPPRESSION_WORD").unwrap().behavior,
            UserPosBehavior::Suppression
        );
        assert!(
            user_pos_definition("SUPPRESSION_WORD")
                .unwrap()
                .forms
                .is_empty()
        );
    }

    #[test]
    fn group1_verb_expands_from_the_declared_base_form() {
        let definition = user_pos_definition("KA_GROUP1_VERB").unwrap();
        let expansions = definition.expand("かく", "書く").unwrap();
        assert_eq!(expansions.len(), 8);
        assert!(
            expansions
                .iter()
                .any(|entry| entry.reading == "かき" && entry.surface == "書き")
        );
        assert!(
            expansions
                .iter()
                .any(|entry| entry.reading == "かい" && entry.surface == "書い")
        );
    }

    #[test]
    fn invalid_base_form_suffix_is_rejected() {
        let definition = user_pos_definition("KA_GROUP1_VERB").unwrap();
        assert!(definition.expand("かく", "書ける").is_none());
    }

    #[test]
    fn abbreviation_uses_the_pinned_special_context() {
        let definition = user_pos_definition("ABBREVIATION").unwrap();
        let expansion = &definition.expand("とうだい", "東大").unwrap()[0];
        assert_eq!(expansion.left_id.get(), 2673);
        assert_eq!(expansion.right_id.get(), 2673);
        assert_eq!(expansion.left_pos, CoarsePos::Other);
    }
}
