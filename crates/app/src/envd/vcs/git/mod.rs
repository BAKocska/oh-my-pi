//! Git repository discovery, admission, and process execution.

pub mod commands;
pub mod diff;
pub mod lock;
pub mod mutation;
pub mod query;
pub mod refs;
pub mod repo;
pub mod runner;

#[cfg(test)]
mod tests;
