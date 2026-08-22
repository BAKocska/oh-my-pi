//! First-party support for GNU quoting, numeric and size parsers, filesystem
//! helpers, mode parsing, checksum engines, and platform probes.
//!
//! Behavior parity is defended by colocated tests.

pub(crate) mod backup;
pub(crate) mod basenc;
pub(crate) mod checksum;
pub(crate) mod clap_ext;
pub(crate) mod entries;
pub(crate) mod fsutil;
pub(crate) mod human;
pub(crate) mod line_ending;
pub(crate) mod mode;
pub(crate) mod mounts;
pub(crate) mod num;
pub(crate) mod parse;
pub(crate) mod posix;
pub(crate) mod quote;
pub(crate) mod ranges;
pub(crate) mod safe_traversal;
pub(crate) mod sys;
pub(crate) mod version_cmp;
pub(crate) mod xattr;
