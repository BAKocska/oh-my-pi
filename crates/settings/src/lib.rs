//! Typed settings reflection and immutable snapshot primitives.

pub mod schema;
pub mod snapshot;

pub use inventory;
pub use schema::{
	Condition, DomainDescriptor, DomainRegistration, DynamicOption, FieldDescriptor, OptionProvider,
	SettingKind, SettingOption, SettingScope, SettingsDomain, ValidationError, registered_domains,
};
pub use snapshot::{
	DomainRevision, Revision, ScopedValues, SettingsSnapshot, SnapshotError, SnapshotMode,
	SnapshotPublisher, Subscription, TypedProjection, deep_merge, resolve_path_scoped,
};
