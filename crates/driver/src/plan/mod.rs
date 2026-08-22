//! Durable plan-mode state, artifacts, transitions, protection, and handoff.

pub mod artifacts;
pub mod handoff;
pub mod protection;
pub mod review;
pub mod state;
pub mod transition;

pub use artifacts::{PlanArtifact, PlanArtifactError, PlanArtifactStore, PlanTitleSource};
pub use handoff::{OverallPlanReference, PlanHandoffError};
pub use state::{DEFAULT_PLAN_URL, PlanState, PlanWorkflow};
pub use transition::{ModelSelection, PlanModelTransition, TransitionQueue};
