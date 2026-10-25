//! Semantic rejection classes shared by dictionary protocols. The host ABI
//! maps these to its own numeric results; domain codes stay with each error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditErrorClass {
    Busy,
    RevisionMismatch,
    CoreInvalid,
    Internal,
    Rejected,
}
