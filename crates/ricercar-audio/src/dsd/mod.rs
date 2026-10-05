//! DSD sources: the shared reader contract and the readers behind it.

pub mod sacd;
// `len()` is an optional length, there is no `is_empty()` to pair it with.
#[allow(clippy::len_without_is_empty)]
pub mod source;
