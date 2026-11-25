//! # rsboot
//!
//! Project bootstrapper / CLI for the rsbatis ecosystem: `rsboot new`,
//! `rsboot generate`, `rsboot migrate`.
//!
//! ## Status: reserved placeholder (0.1.0-alpha.1)
//!
//! The crate name is reserved by the rsbatis project so it cannot be taken by
//! an unrelated project before the real implementation lands. There is no
//! functional API here yet; do not depend on it.

#![forbid(unsafe_code)]

/// Marker proving this crate is owned by the rsbatis project.
#[doc(hidden)]
pub const RESERVED_FOR_RSBATIS: &str = "rsboot";
