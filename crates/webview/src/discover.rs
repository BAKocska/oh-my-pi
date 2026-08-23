//! Installed browser discovery across platforms.
//!
//! Scans standard filesystem locations, application bundles, and search paths
//! for well-known Chromium- and Gecko-family browsers. Discovered browsers are
//! ordered with Chromium engines before Gecko engines, and in priority
//! declaration order within each family.

use std::{
	collections::HashSet,
	env, fs,
	path::{Path, PathBuf},
};

use omp_core::Str;

/// Engine family a discovered browser belongs to (decides the driver protocol).
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum EngineFamily {
	/// Chromium-family engine driven via Chrome `DevTools` Protocol.
	Chromium,
	/// Gecko-family engine driven via `WebDriver` `BiDi`.
	Gecko,
}

/// Well-known browsers we can discover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum BrowserKind {
	/// Google Chrome stable release.
	Chrome,
	/// Google Chrome Beta channel.
	ChromeBeta,
	/// Google Chrome Canary channel.
	ChromeCanary,
	/// Open-source Chromium browser.
	Chromium,
	/// Microsoft Edge browser.
	Edge,
	/// Brave Privacy Browser.
	Brave,
	/// Vivaldi browser.
	Vivaldi,
	/// Opera browser.
	Opera,
	/// Arc browser.
	Arc,
	/// Helium browser.
	Helium,
	/// Mozilla Firefox stable release.
	Firefox,
	/// Mozilla Firefox Developer Edition.
	FirefoxDeveloper,
	/// Mozilla Firefox Nightly channel.
	FirefoxNightly,
	/// `LibreWolf` privacy-focused browser.
	Librewolf,
	/// Waterfox browser.
	Waterfox,
	/// Zen Browser.
	Zen,
	/// Floorp browser.
	Floorp,
}

/// One discovered browser installation.
#[derive(Clone, Debug)]
pub struct InstalledBrowser {
	/// Specific browser installation identified.
	pub kind:   BrowserKind,
	/// The rendering engine / driver family.
	pub family: EngineFamily,
	/// Human-readable display name.
	pub name:   Str,
	/// Path to the browser executable.
	pub path:   PathBuf,
}

/// Static descriptor for a browser kind and its platform-specific search
/// definitions.
struct BrowserDescriptor {
	/// Well-known browser identity.
	kind:               BrowserKind,
	/// Rendering engine family.
	family:             EngineFamily,
	/// Human-readable display name.
	name:               &'static str,
	/// macOS application bundle and executable name pairs.
	#[cfg(target_os = "macos")]
	mac_bundles:        &'static [(&'static str, &'static str)],
	/// Linux executable names looked up along `PATH`.
	#[cfg(target_os = "linux")]
	linux_bins:         &'static [&'static str],
	/// Linux fixed paths (e.g. flatpak exports).
	#[cfg(target_os = "linux")]
	linux_fixed:        &'static [&'static str],
	/// Windows executable paths relative to standard application directories.
	#[cfg(target_os = "windows")]
	win_relative_paths: &'static [&'static str],
}

/// Static catalog of known browsers and their search patterns across platforms.
const BROWSERS: &[BrowserDescriptor] = &[
	BrowserDescriptor {
		kind: BrowserKind::Chrome,
		family: EngineFamily::Chromium,
		name: "Google Chrome",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Google Chrome", "Google Chrome")],
		#[cfg(target_os = "linux")]
		linux_bins: &["google-chrome", "google-chrome-stable"],
		#[cfg(target_os = "linux")]
		linux_fixed: &["/var/lib/flatpak/exports/bin/com.google.Chrome"],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Google\Chrome\Application\chrome.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::ChromeBeta,
		family: EngineFamily::Chromium,
		name: "Google Chrome Beta",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Google Chrome Beta", "Google Chrome Beta")],
		#[cfg(target_os = "linux")]
		linux_bins: &["google-chrome-beta"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Google\Chrome Beta\Application\chrome.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::ChromeCanary,
		family: EngineFamily::Chromium,
		name: "Google Chrome Canary",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Google Chrome Canary", "Google Chrome Canary")],
		#[cfg(target_os = "linux")]
		linux_bins: &["google-chrome-unstable"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Google\Chrome SxS\Application\chrome.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Chromium,
		family: EngineFamily::Chromium,
		name: "Chromium",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Chromium", "Chromium")],
		#[cfg(target_os = "linux")]
		linux_bins: &["chromium", "chromium-browser"],
		#[cfg(target_os = "linux")]
		linux_fixed: &["/var/lib/flatpak/exports/bin/org.chromium.Chromium"],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Chromium\Application\chrome.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Edge,
		family: EngineFamily::Chromium,
		name: "Microsoft Edge",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Microsoft Edge", "Microsoft Edge")],
		#[cfg(target_os = "linux")]
		linux_bins: &["microsoft-edge", "microsoft-edge-stable"],
		#[cfg(target_os = "linux")]
		linux_fixed: &["/var/lib/flatpak/exports/bin/com.microsoft.Edge"],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Microsoft\Edge\Application\msedge.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Brave,
		family: EngineFamily::Chromium,
		name: "Brave Browser",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Brave Browser", "Brave Browser")],
		#[cfg(target_os = "linux")]
		linux_bins: &["brave", "brave-browser"],
		#[cfg(target_os = "linux")]
		linux_fixed: &["/var/lib/flatpak/exports/bin/com.brave.Browser"],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"BraveSoftware\Brave-Browser\Application\brave.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Vivaldi,
		family: EngineFamily::Chromium,
		name: "Vivaldi",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Vivaldi", "Vivaldi")],
		#[cfg(target_os = "linux")]
		linux_bins: &["vivaldi", "vivaldi-stable"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Vivaldi\Application\vivaldi.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Opera,
		family: EngineFamily::Chromium,
		name: "Opera",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Opera", "Opera")],
		#[cfg(target_os = "linux")]
		linux_bins: &["opera"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Opera\opera.exe", r"Opera\launcher.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Arc,
		family: EngineFamily::Chromium,
		name: "Arc",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Arc", "Arc")],
		#[cfg(target_os = "linux")]
		linux_bins: &["arc"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"The Browser Company\Arc\Arc.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Helium,
		family: EngineFamily::Chromium,
		name: "Helium",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Helium", "Helium")],
		#[cfg(target_os = "linux")]
		linux_bins: &["helium"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Helium\helium.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Firefox,
		family: EngineFamily::Gecko,
		name: "Mozilla Firefox",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Firefox", "firefox"), ("Firefox", "Firefox")],
		#[cfg(target_os = "linux")]
		linux_bins: &["firefox", "firefox-esr"],
		#[cfg(target_os = "linux")]
		linux_fixed: &["/var/lib/flatpak/exports/bin/org.mozilla.firefox"],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Mozilla Firefox\firefox.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::FirefoxDeveloper,
		family: EngineFamily::Gecko,
		name: "Firefox Developer Edition",
		#[cfg(target_os = "macos")]
		mac_bundles: &[
			("Firefox Developer Edition", "firefox"),
			("Firefox Developer Edition", "Firefox"),
		],
		#[cfg(target_os = "linux")]
		linux_bins: &["firefox-developer-edition"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Firefox Developer Edition\firefox.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::FirefoxNightly,
		family: EngineFamily::Gecko,
		name: "Firefox Nightly",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Firefox Nightly", "firefox"), ("Firefox Nightly", "Firefox")],
		#[cfg(target_os = "linux")]
		linux_bins: &["firefox-nightly"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Firefox Nightly\firefox.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Librewolf,
		family: EngineFamily::Gecko,
		name: "LibreWolf",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("LibreWolf", "librewolf"), ("LibreWolf", "LibreWolf")],
		#[cfg(target_os = "linux")]
		linux_bins: &["librewolf"],
		#[cfg(target_os = "linux")]
		linux_fixed: &["/var/lib/flatpak/exports/bin/io.gitlab.librewolf-community"],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"LibreWolf\librewolf.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Waterfox,
		family: EngineFamily::Gecko,
		name: "Waterfox",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Waterfox", "Waterfox"), ("Waterfox", "waterfox")],
		#[cfg(target_os = "linux")]
		linux_bins: &["waterfox"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Waterfox\waterfox.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Zen,
		family: EngineFamily::Gecko,
		name: "Zen Browser",
		#[cfg(target_os = "macos")]
		mac_bundles: &[
			("Zen", "zen"),
			("Zen Browser", "zen"),
			("Zen", "Zen"),
			("Zen Browser", "Zen"),
		],
		#[cfg(target_os = "linux")]
		linux_bins: &["zen-browser", "zen"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Zen Browser\zen.exe", r"Zen\zen.exe"],
	},
	BrowserDescriptor {
		kind: BrowserKind::Floorp,
		family: EngineFamily::Gecko,
		name: "Floorp",
		#[cfg(target_os = "macos")]
		mac_bundles: &[("Floorp", "floorp"), ("Floorp", "Floorp")],
		#[cfg(target_os = "linux")]
		linux_bins: &["floorp"],
		#[cfg(target_os = "linux")]
		linux_fixed: &[],
		#[cfg(target_os = "windows")]
		win_relative_paths: &[r"Floorp\floorp.exe"],
	},
];

/// Collect candidate executable paths for a browser descriptor on macOS.
#[cfg(target_os = "macos")]
fn candidate_paths(candidate: &BrowserDescriptor) -> Vec<PathBuf> {
	let mut paths = Vec::new();
	let home = env::var_os("HOME").map(PathBuf::from);

	for &(app, exe) in candidate.mac_bundles {
		let bundle_rel = format!("{app}.app/Contents/MacOS/{exe}");
		paths.push(Path::new("/Applications").join(&bundle_rel));
		if let Some(home_dir) = &home {
			paths.push(home_dir.join("Applications").join(&bundle_rel));
		}
	}
	paths
}

/// Collect candidate executable paths for a browser descriptor on Linux.
#[cfg(target_os = "linux")]
fn candidate_paths(candidate: &BrowserDescriptor) -> Vec<PathBuf> {
	let mut paths = Vec::new();
	let path_var = env::var_os("PATH");
	let path_dirs: Vec<PathBuf> = path_var
		.as_ref()
		.map(|p| env::split_paths(p).collect())
		.unwrap_or_default();
	let home = env::var_os("HOME").map(PathBuf::from);

	for &bin in candidate.linux_bins {
		for dir in &path_dirs {
			paths.push(dir.join(bin));
		}
	}
	for &fixed in candidate.linux_fixed {
		paths.push(PathBuf::from(fixed));
		if let Some(home_dir) = &home
			&& let Some(name) = Path::new(fixed).file_name()
		{
			paths.push(home_dir.join(".local/share/flatpak/exports/bin").join(name));
		}
	}
	paths
}

/// Collect candidate executable paths for a browser descriptor on Windows.
#[cfg(target_os = "windows")]
fn candidate_paths(candidate: &BrowserDescriptor) -> Vec<PathBuf> {
	let mut roots = Vec::new();
	if let Some(val) = env::var_os("LOCALAPPDATA") {
		roots.push(PathBuf::from(val));
	}
	if let Some(val) = env::var_os("PROGRAMFILES") {
		roots.push(PathBuf::from(val));
	}
	if let Some(val) = env::var_os("PROGRAMFILES(X86)") {
		roots.push(PathBuf::from(val));
	}
	if let Some(val) = env::var_os("PROGRAMW6432") {
		roots.push(PathBuf::from(val));
	}

	let mut paths = Vec::new();
	for &rel in candidate.win_relative_paths {
		for root in &roots {
			paths.push(root.join(rel));
		}
	}
	paths
}

/// Fallback candidate paths on unsupported platforms.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn candidate_paths(_candidate: &BrowserDescriptor) -> Vec<PathBuf> {
	Vec::new()
}

/// Scan well-known locations (and PATH on Linux) for usable browsers.
///
/// Chromium-family results sort before Gecko; within a family, the
/// [`BrowserKind`] declaration order above is the priority order. Only
/// existing binaries are returned; deduplicate by resolved path.
pub fn discover() -> Vec<InstalledBrowser> {
	let mut results = Vec::new();
	let mut seen = HashSet::new();

	for candidate in BROWSERS {
		for path in candidate_paths(candidate) {
			if path.is_file() {
				let resolved = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
				if seen.insert(resolved) {
					results.push(InstalledBrowser {
						kind: candidate.kind,
						family: candidate.family,
						name: Str::new(candidate.name),
						path,
					});
				}
			}
		}
	}

	results
}

/// Heuristic: does this binary path look like a Gecko browser?
///
/// Performs a case-insensitive substring match on the file name for
/// `firefox`, `librewolf`, `waterfox`, `zen`, or `floorp`.
pub fn gecko_like(path: &Path) -> bool {
	let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
		return false;
	};
	let name = file_name.to_ascii_lowercase();
	name.contains("firefox")
		|| name.contains("librewolf")
		|| name.contains("waterfox")
		|| name.contains("zen")
		|| name.contains("floorp")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn gecko_like_matches_family_not_vendor_dirs() {
		assert!(gecko_like(Path::new("/opt/FireFox/firefox-bin")));
		assert!(gecko_like(Path::new("C:\\Apps\\LibreWolf\\librewolf.exe")));
		assert!(gecko_like(Path::new("/usr/bin/zen-browser")));
		// Vendor directories must not classify the binary.
		assert!(!gecko_like(Path::new("/home/firefox/chrome")));
		assert!(!gecko_like(Path::new(
			"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
		)));
	}
}
