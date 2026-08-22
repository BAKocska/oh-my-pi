//! Session-local display-name allocation above stable opaque agent identities.

use std::collections::HashSet;

use omp_core::{Str, sf};
use parking_lot::Mutex;
use thiserror::Error;

const RESERVED_ADVISOR: &str = "__advisor";
const ADJECTIVES: [&str; 12] = [
	"Amber", "Brisk", "Calm", "Clever", "Daring", "Eager", "Gentle", "Keen", "Lucid", "Quiet",
	"Swift", "Wary",
];
const ANIMALS: [&str; 12] = [
	"Badger", "Crane", "Fox", "Heron", "Lynx", "Marten", "Otter", "Panda", "Raven", "Stoat", "Tern",
	"Wolf",
];

/// Invalid caller-supplied agent display name.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AgentNameError {
	/// Names must be non-empty ASCII identifiers beginning with a letter.
	#[error(
		"agent name must begin with an ASCII letter and contain only letters, digits, '_' or '-'"
	)]
	Invalid,
	/// Caller names are intentionally short because they appear in IRC routing.
	#[error("agent name exceeds the 32-character display limit")]
	TooLong,
}

/// Race-safe allocator for session-local display aliases.
#[derive(Default)]
pub struct AgentNameAllocator {
	taken: Mutex<HashSet<Str>>,
}

impl AgentNameAllocator {
	/// Creates an allocator with the advisor artifact stem permanently reserved.
	pub fn new() -> Self {
		let mut taken = HashSet::new();
		taken.insert(Str::new_static(RESERVED_ADVISOR));
		Self { taken: Mutex::new(taken) }
	}

	/// Reserves one historical or already-published display stem.
	pub fn reserve(&self, name: &str) {
		self.taken.lock().insert(fold(name));
	}

	/// Allocates a caller alias or deterministic adjective-animal fallback.
	///
	/// Child aliases are prefixed with the parent's display name. Collisions are
	/// compared case-insensitively and receive `-N` suffixes starting at two.
	pub fn allocate(
		&self,
		stable_id: &str,
		parent: Option<&str>,
		requested: Option<&str>,
	) -> Result<Str, AgentNameError> {
		let leaf = match requested.map(str::trim).filter(|name| !name.is_empty()) {
			Some(name) => {
				validate(name)?;
				Str::new(name)
			},
			None => generated(stable_id),
		};
		let base = parent.map_or_else(|| leaf.clone(), |parent| sf!("{}.{}", parent, leaf));
		let mut taken = self.taken.lock();
		let mut candidate = base.clone();
		let mut suffix = 2_u32;
		while taken.contains(&fold(candidate.as_str())) {
			candidate = sf!("{}-{}", base, suffix);
			suffix = suffix.saturating_add(1);
		}
		taken.insert(fold(candidate.as_str()));
		Ok(candidate)
	}
}

fn validate(name: &str) -> Result<(), AgentNameError> {
	if name.len() > 32 {
		return Err(AgentNameError::TooLong);
	}
	let mut bytes = name.bytes();
	if !bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
		|| !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
	{
		return Err(AgentNameError::Invalid);
	}
	Ok(())
}

fn fold(name: &str) -> Str {
	Str::from(name.to_ascii_lowercase())
}

fn generated(stable_id: &str) -> Str {
	let hash = stable_id
		.bytes()
		.fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
			(hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
		});
	let adjective = ADJECTIVES[(hash as usize) % ADJECTIVES.len()];
	let animal = ANIMALS[((hash >> 16) as usize) % ANIMALS.len()];
	sf!("{}{}", adjective, animal)
}
