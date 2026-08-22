//! Findings-first local security review over ordinary restricted child agents.
//!
//! This module intentionally owns no coordinator, scan database, cloud client,
//! remediation workflow, or `security://` resolver.

pub mod model;
pub mod profile;
pub mod result;
