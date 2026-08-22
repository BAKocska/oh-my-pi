//! lintx — omp's custom lint engine for path/style rules.
//!
//! Rules (one module each under `lints/`):
//!   long-path      inline qualified path with more than N segments (default 2)
//!   relative-path  inline `crate::` / `super::` / `self::` qualified path
//!   arc-struct     struct where most fields are Arc-wrapped
//!   mutex-arc      `Mutex<Arc<T>>` / `RwLock<Option<Arc<T>>>` → arc-swap
//!
//! `--fix` rewrites offending paths to their tail and inserts a `use` into
//! the nearest enclosing module scope, iterating each file to a fixpoint
//! (nested paths need a second pass). Ambiguous cases — name collisions,
//! glob/prelude shadowing, `#[cfg]`-gated code, alias-rooted paths — stay
//! diagnostics. Run `cargo fmt` afterwards.

mod bindings;
mod fix;
mod lint;
mod lints;
mod scope;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use lint::FileContext;
use walkdir::WalkDir;

#[derive(Default)]
struct Options {
	fix: bool,
	max_segments: usize,
	paths: Vec<PathBuf>,
}

fn main() {
	let mut opts = Options { max_segments: 2, ..Options::default() };
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--fix" => opts.fix = true,
			"--max-segments" => {
				opts.max_segments =
					args.next().and_then(|v| v.parse().ok()).expect("--max-segments <N>");
			}
			_ => opts.paths.push(PathBuf::from(arg)),
		}
	}
	if opts.paths.is_empty() {
		opts.paths.push(PathBuf::from("."));
	}

	let rules = lints::all(opts.max_segments);
	let mut totals: BTreeMap<&'static str, usize> = BTreeMap::new();
	let mut fixed_files = 0usize;
	let mut fixes_applied = 0usize;
	for root in &opts.paths {
		for entry in WalkDir::new(root)
			.into_iter()
			.filter_entry(|e| {
				let name = e.file_name().to_string_lossy();
				name != "target" && name != "vendor" && !name.starts_with('.')
			})
			.filter_map(Result::ok)
			.filter(|e| e.file_type().is_file())
			.filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
		{
			let path = entry.path();
			let Ok(mut text) = std::fs::read_to_string(path) else { continue };
			let own_crate = crate_name_for(path);
			// Nested fixes (a linted path inside another's generic args) are
			// deferred to the next pass; iterate to a fixpoint.
			let mut changed = false;
			for pass in 0..5 {
				let ctx = FileContext::new(path, &text);
				let mut diags = Vec::new();
				for rule in &rules {
					rule.detect_erased(&ctx, &mut |diag| {
						if pass == 0 {
							*totals.entry(diag.rule).or_default() += 1;
							println!("{}", diag.render(&ctx));
						}
						diags.push(diag);
					});
				}
				if !opts.fix {
					break;
				}
				let (new_text, applied) = fix::apply(&ctx, diags, &own_crate);
				if applied == 0 {
					break;
				}
				fixes_applied += applied;
				changed = true;
				drop(ctx);
				text = new_text;
			}
			if changed {
				std::fs::write(path, text).expect("write fixed file");
				fixed_files += 1;
			}
		}
	}
	let mut summary = String::new();
	for (rule, n) in &totals {
		let _ = write!(summary, "{rule}: {n}  ");
	}
	eprintln!("\n== {summary}");
	if opts.fix {
		eprintln!("== applied {fixes_applied} fixes across {fixed_files} files");
	}
}
/// `lib` name (hyphens normalized) of the crate containing `file`, from the
/// nearest ancestor Cargo.toml. The fixer imports self-referencing paths via
/// `crate::` instead.
///
/// Only files under the crate's `src/` qualify: integration tests, benches,
/// and examples are separate crates where the lib is named, not `crate`.
fn crate_name_for(file: &std::path::Path) -> String {
	let mut dir = file.parent();
	while let Some(d) = dir {
		if let Ok(manifest) = std::fs::read_to_string(d.join("Cargo.toml")) {
			if !file.strip_prefix(d).is_ok_and(|rel| rel.starts_with("src")) {
				return String::new();
			}
			if let Some(name) = manifest
				.lines()
				.find_map(|l| l.trim().strip_prefix("name").and_then(|r| r.trim().strip_prefix('=')))
			{
				return name.trim().trim_matches('"').replace('-', "_");
			}
		}
		dir = d.parent();
	}
	String::new()
}
