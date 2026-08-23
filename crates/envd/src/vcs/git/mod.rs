//! Git repository discovery, admission, and process execution.
//!
//! Reads run in-process through gitoxide (`native`); the hardened system-Git
//! runner remains for mutations, network transfers, byte-exact patch capture,
//! and repositories gitoxide cannot open.

pub mod commands;
pub mod diff;
pub mod lock;
pub mod mutation;
pub mod native;
pub mod query;
pub mod refs;
pub mod repo;
pub mod runner;

#[cfg(test)]
mod tests;
