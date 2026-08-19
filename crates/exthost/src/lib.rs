//! Process boundary for OMP extension hosts.
//!
//! The extension host is structurally separate from the model-facing eval
//! kernel. Its eventual process topology is one child per extension, keyed by
//! layer, tier, and extension, with actor-serialized callback entry by default.
//! This skeleton starts no interpreter, process, transport, or runtime service.
