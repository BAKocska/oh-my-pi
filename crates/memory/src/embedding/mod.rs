//! Isolated local-embedding protocol, worker, and restartable supervisor.

pub mod protocol;
pub mod supervisor;
pub mod worker;

pub use protocol::{InboundFrame, ModelId, OutboundFrame};
pub use supervisor::{EmbeddingSupervisor, SupervisorConfig};
