//! Configured, symlink-confined vault authority for `vault://` resources.

use std::{
	collections::BTreeMap,
	fs, io,
	path::{Component, Path, PathBuf},
	sync::Arc,
};

use omp_core::{CowBytes, Str};
use parking_lot::RwLock;
use serde::Deserialize;
use toml::de;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultFile {
	#[serde(default)]
	vaults: BTreeMap<Str, PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct VaultService {
	roots: Arc<RwLock<BTreeMap<Str, PathBuf>>>,
}

impl VaultService {
	pub fn load(path: &Path) -> Result<Self, VaultError> {
		let body = match fs::read_to_string(path) {
			Ok(body) => body,
			Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
			Err(source) => return Err(VaultError::Io { path: path.to_path_buf(), source }),
		};
		let parsed: VaultFile = toml::from_str(&body)
			.map_err(|source| VaultError::Parse { path: path.to_path_buf(), source })?;
		let mut roots = BTreeMap::new();
		for (name, root) in parsed.vaults {
			if name.is_empty() || name.contains(['/', '\\']) {
				return Err(VaultError::InvalidName { name });
			}
			let canonical = root
				.canonicalize()
				.map_err(|source| VaultError::Io { path: root.clone(), source })?;
			if !canonical.is_dir() {
				return Err(VaultError::NotDirectory { path: canonical });
			}
			roots.insert(name, canonical);
		}
		Ok(Self { roots: Arc::new(RwLock::new(roots)) })
	}

	pub fn names(&self) -> Vec<Str> {
		self.roots.read().keys().cloned().collect()
	}

	fn target(&self, vault: &str, relative: &str, for_write: bool) -> Result<PathBuf, VaultError> {
		let root = self
			.roots
			.read()
			.get(vault)
			.cloned()
			.ok_or_else(|| VaultError::Unknown { name: Str::new(vault) })?;
		let relative = Path::new(relative);
		if relative.is_absolute()
			|| relative
				.components()
				.any(|c| !matches!(c, Component::Normal(_)))
		{
			return Err(VaultError::Escape);
		}
		let target = root.join(relative);
		let mut existing = if for_write {
			target.parent().unwrap_or(&root)
		} else {
			target.as_path()
		};
		while for_write && !existing.exists() {
			existing = existing.parent().ok_or(VaultError::Escape)?;
		}
		let canonical = existing
			.canonicalize()
			.map_err(|source| VaultError::Io { path: existing.to_path_buf(), source })?;
		if !canonical.starts_with(&root) {
			return Err(VaultError::Escape);
		}
		if for_write
			&& let Ok(metadata) = fs::symlink_metadata(&target)
			&& metadata.file_type().is_symlink()
		{
			return Err(VaultError::Escape);
		}
		Ok(target)
	}

	pub fn read(
		&self,
		vault: &str,
		relative: &str,
		limit: usize,
	) -> Result<CowBytes<'static>, VaultError> {
		let path = self.target(vault, relative, false)?;
		let metadata =
			fs::metadata(&path).map_err(|source| VaultError::Io { path: path.clone(), source })?;
		if metadata.len() > limit as u64 {
			return Err(VaultError::Limit { limit });
		}
		fs::read(&path)
			.map(CowBytes::from)
			.map_err(|source| VaultError::Io { path, source })
	}

	pub fn write(
		&self,
		vault: &str,
		relative: &str,
		bytes: &[u8],
		limit: usize,
	) -> Result<(), VaultError> {
		if bytes.len() > limit {
			return Err(VaultError::Limit { limit });
		}
		let path = self.target(vault, relative, true)?;
		let parent = path.parent().ok_or(VaultError::Escape)?;
		fs::create_dir_all(parent)
			.map_err(|source| VaultError::Io { path: parent.to_path_buf(), source })?;
		let temporary = path.with_extension("omp-tmp");
		fs::write(&temporary, bytes)
			.map_err(|source| VaultError::Io { path: temporary.clone(), source })?;
		fs::rename(&temporary, &path).map_err(|source| VaultError::Io { path, source })
	}

	pub fn list(
		&self,
		vault: &str,
		relative: &str,
		limit: usize,
	) -> Result<(Vec<(Str, bool, u64)>, bool), VaultError> {
		let path = if relative.is_empty() {
			self
				.roots
				.read()
				.get(vault)
				.cloned()
				.ok_or_else(|| VaultError::Unknown { name: Str::new(vault) })?
		} else {
			self.target(vault, relative, false)?
		};
		let mut values = Vec::new();
		for item in
			fs::read_dir(&path).map_err(|source| VaultError::Io { path: path.clone(), source })?
		{
			let item = item.map_err(|source| VaultError::Io { path: path.clone(), source })?;
			let metadata = item
				.metadata()
				.map_err(|source| VaultError::Io { path: item.path(), source })?;
			values.push((
				Str::from(item.file_name().to_string_lossy().into_owned()),
				metadata.is_dir(),
				metadata.len(),
			));
		}
		values.sort_by(|a, b| a.0.cmp(&b.0));
		let truncated = values.len() > limit;
		values.truncate(limit);
		Ok((values, truncated))
	}
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
	#[error("cannot access vault path {path}")]
	Io {
		path:   PathBuf,
		#[source]
		source: io::Error,
	},
	#[error("invalid vault configuration {path}")]
	Parse {
		path:   PathBuf,
		#[source]
		source: de::Error,
	},
	#[error("invalid vault name {name}")]
	InvalidName { name: Str },
	#[error("vault root {path} is not a directory")]
	NotDirectory { path: PathBuf },
	#[error("vault {name} is not configured")]
	Unknown { name: Str },
	#[error("vault path escapes its configured root")]
	Escape,
	#[error("vault operation exceeded its {limit}-byte bound")]
	Limit { limit: usize },
}
