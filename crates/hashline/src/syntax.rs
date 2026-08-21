//! Cached tree-sitter syntax probes for replacement-boundary validation.

use std::{
	collections::{HashMap, VecDeque},
	sync::{Arc, LazyLock},
};

use omp_ast::block::{
	BlockRangeOptions, EnclosingBoundaryOptions, LineRange, NodeSpan, enclosing_block_boundaries,
	node_chain_at,
};
use omp_core::{Hash32, Str};
use parking_lot::Mutex;

const CACHE_LIMIT: usize = 256;
type ContentKey = (Hash32, Str);
type BoundaryKey = (Hash32, Str, u32, u32);
type ChainKey = (Hash32, Str, u32);

#[derive(Default)]
struct ProbeCache {
	parses:         HashMap<ContentKey, bool>,
	parse_order:    VecDeque<ContentKey>,
	boundaries:     HashMap<BoundaryKey, Vec<u32>>,
	boundary_order: VecDeque<BoundaryKey>,
	chains:         HashMap<ChainKey, Arc<[NodeSpan]>>,
	chain_order:    VecDeque<ChainKey>,
}

static CACHE: LazyLock<Mutex<ProbeCache>> = LazyLock::new(|| Mutex::new(ProbeCache::default()));

/// Returns true only when the path identifies a supported language and the text
/// parses cleanly.
pub fn parses_cleanly(path: Option<&str>, text: &str) -> bool {
	let Some(path) = path else { return false };
	let key = (Hash32::sum(text), Str::new(path));
	if let Some(value) = CACHE.lock().parses.get(&key) {
		return *value;
	}
	let line_count = text.split('\n').count().max(1);
	let end_line = u32::try_from(line_count).unwrap_or(u32::MAX);
	let ok = enclosing_block_boundaries(EnclosingBoundaryOptions {
		code:   if text.is_empty() {
			"\n".to_owned()
		} else {
			text.to_owned()
		},
		lang:   None,
		path:   Some(path.to_owned()),
		ranges: vec![LineRange { start_line: 1, end_line }],
	})
	.ok()
	.flatten()
	.is_some();
	let mut guard = CACHE.lock();
	if guard.parses.len() >= CACHE_LIMIT
		&& let Some(oldest) = guard.parse_order.pop_front()
	{
		guard.parses.remove(&oldest);
	}
	guard.parse_order.push_back(key.clone());
	guard.parses.insert(key, ok);
	ok
}

/// Returns the named syntax-node chain containing `line`, innermost first.
///
/// Unknown languages, blank or out-of-range lines, and parse failures produce
/// an empty chain, so callers treat absence as no structural evidence.
pub fn node_chain(text: &str, path: &str, line: usize) -> Arc<[NodeSpan]> {
	let line = u32::try_from(line).unwrap_or(u32::MAX);
	let key = (Hash32::sum(text), Str::new(path), line);
	if let Some(value) = CACHE.lock().chains.get(&key) {
		return value.clone();
	}
	let chain: Arc<[NodeSpan]> = node_chain_at(BlockRangeOptions {
		code: text.to_owned(),
		lang: None,
		path: Some(path.to_owned()),
		line,
	})
	.ok()
	.flatten()
	.unwrap_or_default()
	.into();
	let mut guard = CACHE.lock();
	if guard.chains.len() >= CACHE_LIMIT
		&& let Some(oldest) = guard.chain_order.pop_front()
	{
		guard.chains.remove(&oldest);
	}
	guard.chain_order.push_back(key.clone());
	guard.chains.insert(key, chain.clone());
	chain
}

/// Returns whether a syntax-node boundary outside a visible range is the
/// requested source line.
///
/// Unknown languages and broken source return false, so callers never infer
/// structural evidence from an error-recovery tree.
pub fn is_enclosing_boundary(
	text: &str,
	path: &str,
	start_line: usize,
	end_line: usize,
	boundary: u32,
) -> bool {
	let start_line = u32::try_from(start_line).unwrap_or(u32::MAX);
	let end_line = u32::try_from(end_line).unwrap_or(u32::MAX);
	let key = (Hash32::sum(text), Str::new(path), start_line, end_line);
	if let Some(value) = CACHE.lock().boundaries.get(&key) {
		return value.contains(&boundary);
	}
	let boundaries = enclosing_block_boundaries(EnclosingBoundaryOptions {
		code:   text.to_owned(),
		lang:   None,
		path:   Some(path.to_owned()),
		ranges: vec![LineRange { start_line, end_line }],
	})
	.ok()
	.flatten()
	.unwrap_or_default();
	let found = boundaries.contains(&boundary);
	let mut guard = CACHE.lock();
	if guard.boundaries.len() >= CACHE_LIMIT
		&& let Some(oldest) = guard.boundary_order.pop_front()
	{
		guard.boundaries.remove(&oldest);
	}
	guard.boundary_order.push_back(key.clone());
	guard.boundaries.insert(key, boundaries);
	found
}
