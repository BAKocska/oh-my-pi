//! Runtime symbol specification query surface.
//!
//! The canonical rows live beside [`omp_tool::OperationSpec`]. This module is a
//! thin application-facing view; it deliberately owns no copied metadata.

pub use omp_tool::{
	CallbackAbi, OperationSpec, PhaseLegalityRow, RuntimeDurationMetadata, RuntimeSymbolSpec,
	operation_spec, phase_legality_matrix, runtime_duration_metadata, runtime_symbols,
};
