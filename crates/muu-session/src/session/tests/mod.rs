//! Session behaviour tests, grouped by the contract they pin: input handling,
//! conversion and segment operations, learning, recording, shared resources
//! and vocabulary layers, plus the cross-session Core tests in `shared_core`.
use std::fs;
use std::sync::Arc;

use muu_dict::{
    DictionaryBuilder, LeftContextId, LexiconEntry, RightContextId, UnknownClass, UnknownDefinition,
};
use tempfile::NamedTempFile;

use crate::session::*;
use crate::test_support::*;
use muu_dict::CoarsePos;

mod conversion;
mod input;
mod learning;
mod recording;
mod resources;
mod shared_core;
mod vocabulary;
