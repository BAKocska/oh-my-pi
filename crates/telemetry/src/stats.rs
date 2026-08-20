//! Provider usage-window and broker-fleet aggregation.

use std::collections::BTreeMap;

use omp_core::Str;

/// Token buckets reported for one provider by one broker client.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientProviderUsage {
	/// Provider identifier.
	pub provider:           Str,
	/// Non-cached input tokens.
	pub input_tokens:       u64,
	/// Output tokens.
	pub output_tokens:      u64,
	/// Cache-read input tokens.
	pub cache_read_tokens:  u64,
	/// Cache-write input tokens.
	pub cache_write_tokens: u64,
}

impl ClientProviderUsage {
	/// Returns the full token burn represented by all billable buckets.
	#[must_use]
	pub const fn total_tokens(&self) -> u64 {
		self
			.input_tokens
			.saturating_add(self.output_tokens)
			.saturating_add(self.cache_read_tokens)
			.saturating_add(self.cache_write_tokens)
	}
}

/// Per-provider usage reported by one client connected to the auth broker.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientUsageClientSummary {
	/// Provider token buckets observed by this client.
	pub providers: Vec<ClientProviderUsage>,
}

/// Sums broker client token burn by provider across the whole reporting fleet.
///
/// `None` means no client reported provider usage, allowing callers to fall
/// back to local telemetry rather than interpreting missing broker data as
/// zero burn.
#[must_use]
pub fn sum_fleet_tokens(clients: &[ClientUsageClientSummary]) -> Option<BTreeMap<Str, u64>> {
	let mut totals = BTreeMap::new();
	for provider in clients.iter().flat_map(|client| &client.providers) {
		let total = totals.entry(provider.provider.clone()).or_insert(0_u64);
		*total = total.saturating_add(provider.total_tokens());
	}
	(!totals.is_empty()).then_some(totals)
}

/// One provider usage-window observation retained for stats aggregation.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindowSnapshot {
	/// Provider identifier.
	pub provider:      Str,
	/// Stable provider-defined limit identifier.
	pub limit_id:      Str,
	/// Human-readable limit label.
	pub label:         Str,
	/// Human-readable duration or reset-window label.
	pub window_label:  Option<Str>,
	/// Used fraction at this observation, when reported.
	pub used_fraction: Option<f64>,
}

/// Stable grouping key for one provider-defined usage limit.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UsageWindowKey {
	/// Provider identifier.
	pub provider: Str,
	/// Stable provider-defined limit identifier.
	pub limit_id: Str,
}

/// Observations belonging to one provider-defined usage limit.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindowGroup<'a> {
	/// Stable provider and limit identity.
	pub key:       UsageWindowKey,
	/// Latest display label observed for the limit.
	pub label:     Str,
	/// Observations in input order.
	pub snapshots: Vec<&'a UsageWindowSnapshot>,
}

/// Groups usage observations by `(provider, limit_id)` in stable key order.
///
/// Duration labels are presentation metadata and deliberately never enter the
/// key: two distinct provider limits may share the same daily or weekly label.
#[must_use]
pub fn group_usage_windows_by_limit_id(
	snapshots: &[UsageWindowSnapshot],
) -> Vec<UsageWindowGroup<'_>> {
	let mut groups = BTreeMap::<UsageWindowKey, (Str, Vec<&UsageWindowSnapshot>)>::new();
	for snapshot in snapshots {
		let key = UsageWindowKey {
			provider: snapshot.provider.clone(),
			limit_id: snapshot.limit_id.clone(),
		};
		let label = display_label(snapshot);
		match groups.entry(key) {
			std::collections::btree_map::Entry::Vacant(entry) => {
				entry.insert((label, vec![snapshot]));
			},
			std::collections::btree_map::Entry::Occupied(mut entry) => {
				let (current_label, group_snapshots) = entry.get_mut();
				*current_label = label;
				group_snapshots.push(snapshot);
			},
		}
	}
	groups
		.into_iter()
		.map(|(key, (label, snapshots))| UsageWindowGroup { key, label, snapshots })
		.collect()
}

fn display_label(snapshot: &UsageWindowSnapshot) -> Str {
	let Some(window) = snapshot.window_label.as_deref() else {
		return snapshot.label.clone();
	};
	if contains_ascii_case_insensitive(snapshot.label.as_str(), window) {
		return snapshot.label.clone();
	}
	Str::from(format!("{} · {window}", snapshot.label))
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
	needle.is_empty()
		|| haystack
			.as_bytes()
			.windows(needle.len())
			.any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn provider(provider: &str, buckets: [u64; 4]) -> ClientProviderUsage {
		ClientProviderUsage {
			provider:           provider.into(),
			input_tokens:       buckets[0],
			output_tokens:      buckets[1],
			cache_read_tokens:  buckets[2],
			cache_write_tokens: buckets[3],
		}
	}

	#[test]
	fn sums_every_token_bucket_across_broker_clients() {
		let clients = [
			ClientUsageClientSummary {
				providers: vec![provider("anthropic", [10, 2, 3, 4]), provider("openai", [7, 1, 0, 0])],
			},
			ClientUsageClientSummary { providers: vec![provider("anthropic", [20, 5, 6, 7])] },
		];
		let totals = sum_fleet_tokens(&clients).expect("fleet usage");
		assert_eq!(totals.get("anthropic"), Some(&57));
		assert_eq!(totals.get("openai"), Some(&8));
		assert_eq!(sum_fleet_tokens(&[]), None);
	}

	#[test]
	fn distinct_limit_ids_never_merge_when_duration_labels_match() {
		let snapshots = [
			UsageWindowSnapshot {
				provider:      "anthropic".into(),
				limit_id:      "anthropic:7d".into(),
				label:         "Claude 7 Day".into(),
				window_label:  Some("7 Day".into()),
				used_fraction: Some(0.2),
			},
			UsageWindowSnapshot {
				provider:      "anthropic".into(),
				limit_id:      "anthropic:7d:fable".into(),
				label:         "Claude 7 Day (Fable)".into(),
				window_label:  Some("7 Day".into()),
				used_fraction: Some(0.6),
			},
		];
		let groups = group_usage_windows_by_limit_id(&snapshots);
		assert_eq!(groups.len(), 2);
		assert_eq!(groups[0].key.limit_id.as_str(), "anthropic:7d");
		assert_eq!(groups[1].key.limit_id.as_str(), "anthropic:7d:fable");
		assert_eq!(groups[0].snapshots.len(), 1);
		assert_eq!(groups[1].snapshots.len(), 1);
	}
}
