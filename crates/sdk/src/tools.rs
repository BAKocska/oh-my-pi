//! Native tool identities and versioned custom-tool registration.

use omp_core::Str;
use omp_tool::{Claims, Presentation, Registry, RegistryError, Tool};
pub use omp_tools::{BuiltinToolIdentity, builtin_tool_identities};

/// SDK composition wrapper around the versioned native registry.
///
/// The production application supplies its authority-backed registry; SDK
/// embedders can then add native custom tools without converting them to an
/// untyped compatibility definition.
pub struct ToolRegistryBuilder {
	registry: Registry,
}

impl ToolRegistryBuilder {
	/// Starts from an authority-built production registry.
	#[must_use]
	pub const fn from_production(registry: Registry) -> Self {
		Self { registry }
	}

	/// Starts an empty registry for hosts that register every authority.
	#[must_use]
	pub fn empty() -> Self {
		Self { registry: Registry::new() }
	}

	/// Registers one versioned native custom tool.
	pub fn register<T: Tool>(
		&mut self,
		tool: T,
		presentation: Presentation,
		claims: Claims,
	) -> Result<&mut Self, RegistryError> {
		self.registry.register(tool, presentation, claims)?;
		Ok(self)
	}

	/// Protects essential harness-owned names from custom shadowing.
	pub fn protect_core<I, S>(&mut self, names: I) -> &mut Self
	where
		I: IntoIterator<Item = S>,
		S: Into<Str>,
	{
		self.registry.protect_core_claims(names);
		self
	}

	/// Returns the completed versioned registry.
	#[must_use]
	pub fn finish(self) -> Registry {
		self.registry
	}
}

impl Default for ToolRegistryBuilder {
	fn default() -> Self {
		Self::empty()
	}
}
