//! Reserved boundary for OMP's deferred process-confinement integration.
//!
//! Version 1 provides no sandbox enforcement, and this crate intentionally
//! exposes no confinement API. Extensions, shell builtins, and child processes
//! remain unconfined. Future isolation work belongs here only after the planned
//! VM-grade vibevmm and isobox architecture is available.
