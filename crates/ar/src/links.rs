//! Generic lazy symbolic-link path resolution over indexed entries.
//!
//! Formats whose links are plain path aliases (ASAR, ISO Rock Ridge, RAR,
//! 7z, cpio) resolve through here; TAR keeps its specialized resolver with
//! hard-link and pending-classification semantics in `tar::reader`.

use omp_core::{Str, StrMut};

use crate::{Entry, Error, Limits, Result, entry::Storage, path::validate};

/// Rewrites `path` through link aliases until it no longer crosses one.
///
/// Bounded by [`Limits::link_depth`]; exceeding it fails with
/// [`Error::LinkResolutionDepth`] rather than looping on cyclic aliases.
pub(crate) fn resolve_alias_path(entries: &[Entry], path: Str, limits: Limits) -> Result<Str> {
	let original = path.clone();
	let mut resolved = path;
	let mut rewrites = 0_u64;
	loop {
		let Some((end, target)) = find_alias(entries, resolved.as_str()) else {
			return Ok(resolved);
		};
		if rewrites == limits.link_depth {
			return Err(Error::LinkResolutionDepth { path: original, limit: limits.link_depth });
		}
		rewrites += 1;
		let suffix = resolved.get(end..).unwrap_or("").trim_start_matches('/');
		resolved = join_target(target, suffix, limits)?;
	}
}

fn find_alias<'a>(entries: &'a [Entry], path: &str) -> Option<(usize, &'a str)> {
	let mut end = path.len();
	while end > 0 {
		if let Ok(index) = entries.binary_search_by(|entry| entry.path().cmp(&path[..end]))
			&& let Storage::Link { target_path, .. } = &entries[index].storage
		{
			return Some((end, target_path.as_str()));
		}
		end = path[..end].rfind('/').unwrap_or(0);
	}
	None
}

fn join_target(target: &str, suffix: &str, limits: Limits) -> Result<Str> {
	let separator = usize::from(!target.is_empty() && !suffix.is_empty());
	let length = target
		.len()
		.checked_add(separator)
		.and_then(|length| length.checked_add(suffix.len()))
		.ok_or(Error::InvalidArchive("link path length overflow"))?;
	if length as u64 > limits.path_size {
		return Err(Error::PathTooLong { actual: length as u64, limit: limits.path_size });
	}
	let mut joined = StrMut::with_capacity(length);
	joined.push_str(target);
	if separator == 1 {
		joined.push('/');
	}
	joined.push_str(suffix);
	let joined = joined.freeze();
	validate(&joined, limits)?;
	Ok(joined)
}
