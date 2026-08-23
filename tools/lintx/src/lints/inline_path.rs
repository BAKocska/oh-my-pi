//! `long-path` / `relative-path`: inline qualified paths that must be
//! imported instead. One family, one module: candidate discovery, the
//! finding, and its fix heuristic all live here — the two lints differ only
//! in which paths they claim.
//!
//! - `long-path`: qualified paths that violate the explicit import policy.
//! - `std-path`: `std::module::item` paths that should start at `module`.
//! - `tokio-path`: `tokio::module::item` paths, except `tokio::fs`.
//! - `relative-path`: any `crate::` / `super::` / `self::` inline path.

use std::ops::Range;

use ra_ap_syntax::{
	SyntaxKind,
	ast::{self, AstNode},
};

use crate::{
	bindings::Bindings,
	fix::PathFix,
	lint::{Diagnosis, FileContext, Lint, RealtimeSink},
	scope,
};

/// The `long-path` lint; `max_segments` is the largest allowed inline
/// segment count (three by default, allowing two `::` separators).
pub struct LongPath {
	/// Paths with more segments than this are flagged.
	pub max_segments: usize,
}

/// The `relative-path` lint; no configuration.
pub struct RelativePath;
/// The `std-path` lint; no configuration.
pub struct StdPath;
/// The `tokio-path` lint; no configuration.
pub struct TokioPath;

/// One inline qualified path that should be an import.
pub struct Finding {
	span:     Range<usize>,
	rendered: String,
	segments: usize,
	relative: bool,
	fix:      Option<PathFix>,
}

impl Finding {
	fn new(ctx: &FileContext<'_>, path: &ast::Path, names: Vec<String>) -> Self {
		let range = path.syntax().text_range();
		Self {
			span:     range.start().into()..range.end().into(),
			fix:      plan(ctx, path, &names),
			segments: names.len(),
			relative: is_relative_root(&names[0]),
			rendered: names.join("::"),
		}
	}
}

impl Diagnosis for Finding {
	fn span(&self) -> Range<usize> {
		self.span.clone()
	}

	fn message(&self) -> String {
		if self.relative {
			let root = self.rendered.split("::").next().unwrap_or_default();
			format!("`{}` uses a `{root}::` inline path; import it", self.rendered)
		} else {
			format!("`{}` has {} segments; import it", self.rendered, self.segments)
		}
	}

	fn autofixable(&self) -> bool {
		self.fix.is_some()
	}

	fn fix(self) -> Option<PathFix> {
		self.fix
	}
}

impl Lint for LongPath {
	type Instance = Finding;

	const NAME: &'static str = "long-path";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		let bindings = Bindings::collect(&ctx.tree);
		for (path, names) in inline_paths(ctx) {
			if requires_import(&names, self.max_segments)
				&& !is_relative_root(&names[0])
				&& names[0] != "std"
				&& names[0] != "tokio"
				&& !(names.len() <= self.max_segments && bare_name_collision(&bindings, &path, &names))
			{
				sink.push(Finding::new(ctx, &path, names));
			}
		}
	}
}

impl Lint for StdPath {
	type Instance = Finding;

	const NAME: &'static str = "std-path";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for (path, names) in inline_paths(ctx) {
			if names[0] == "std" && names.len() >= 3 {
				sink.push(Finding::new(ctx, &path, names));
			}
		}
	}
}

impl Lint for TokioPath {
	type Instance = Finding;

	const NAME: &'static str = "tokio-path";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for (path, names) in inline_paths(ctx) {
			if names[0] == "tokio" && names.len() >= 3 && names[1] != "fs" {
				sink.push(Finding::new(ctx, &path, names));
			}
		}
	}
}

impl Lint for RelativePath {
	type Instance = Finding;

	const NAME: &'static str = "relative-path";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for (path, names) in inline_paths(ctx) {
			if is_relative_root(&names[0]) {
				sink.push(Finding::new(ctx, &path, names));
			}
		}
	}
}

/// Names importing which would shadow the std prelude — always keep a
/// qualifier for these instead (`io::Result`, not `Result`).
const PRELUDE: &[&str] = &[
	"Result",
	"Option",
	"Some",
	"None",
	"Ok",
	"Err",
	"Box",
	"String",
	"Vec",
	"Drop",
	"Send",
	"Sync",
	"Sized",
	"Unpin",
	"Clone",
	"Copy",
	"Default",
	"Eq",
	"Ord",
	"PartialEq",
	"PartialOrd",
	"Debug",
	"Hash",
	"Iterator",
	"IntoIterator",
	"Extend",
	"From",
	"Into",
	"TryFrom",
	"TryInto",
	"AsRef",
	"AsMut",
	"FnOnce",
	"FnMut",
	"Fn",
	"ToString",
	"ToOwned",
];
/// Types that always read better without a module qualifier.
const BARE_NAMES: &[&str] = &[
	"AccessFlags",
	"AccountScope",
	"ActionProxy",
	"ActivationTrigger",
	"AgentCampaignControlBackend",
	"AgentClient",
	"AgentDurableJobRegistrar",
	"AgentGoalControl",
	"ArtifactCatalog",
	"AsyncFd",
	"AsyncPipeReader",
	"AudioFormatBits",
	"AuthSpecKind",
	"AutolearnCampaign",
	"Barrier",
	"BlobStore",
	"BodyFactoryHandle",
	"BorrowedStrDeserializer",
	"BridgeHostError",
	"BudgetReservation",
	"BuiltinConfig",
	"ButtonState",
	"CacheRetentionSetting",
	"CallbackConcurrency",
	"CallbackDispatcher",
	"CampaignControlAuthorityFactory",
	"CampaignHandle",
	"CancellationToken",
	"ChatParentHost",
	"ChatProviderControlBackend",
	"ChatStream",
	"CodexRedemption",
	"CodexTransportPreference",
	"CollabCommandHandle",
	"CollabLink",
	"CollabOwnerCommand",
	"CollabRole",
	"CollabSessionAuthority",
	"ColorSpace",
	"CompactDiffOptions",
	"CompareOp",
	"ComponentProxy",
	"ConnectionConfig",
	"ContextDiscoveryOptions",
	"ContextRc",
	"ControlAuthority",
	"ControlAuthorityFactory",
	"ControlCompositionError",
	"ControlConnectionIdentity",
	"ControlDispatch",
	"ControlEffect",
	"ControlInvocationAuthority",
	"ControlMailboxEvent",
	"ControlPromptProjectionDispatcher",
	"ControlProtocolError",
	"ConversationError",
	"CooldownReason",
	"CredentialShaperRegistry",
	"CredentialSourceSpec",
	"DBusProxy",
	"DataUrlContext",
	"DeclarationSet",
	"DecoderOptions",
	"DecodingError",
	"DestinationPrecondition",
	"DirectShareStore",
	"Display",
	"DocServerTask",
	"EditableTextProxy",
	"Effort",
	"EmbeddedPython",
	"EnvdHostOwnerBackends",
	"Errno",
	"ErrorPhase",
	"EvalSessionControl",
	"ExecutionReceipt",
	"ExtHostSpec",
	"ExtensionManifest",
	"FcntlArg",
	"Firehose",
	"FixedControlAuthorityFactory",
	"GetPropertyReply",
	"GithubCache",
	"GithubCredentialBridge",
	"Gitignore",
	"GrantedContextRoot",
	"GuestInputDisposition",
	"GuestInputError",
	"GuestRelayPump",
	"GuestSessionRestore",
	"GzDecoder",
	"GzEncoder",
	"HeaderName",
	"HeaderValue",
	"HealthClient",
	"HostKey",
	"HtmlThemePalette",
	"ImageKind",
	"InferenceBridge",
	"InferenceClient",
	"Instant",
	"JoinSet",
	"KeyState",
	"LocalError",
	"LocalFs",
	"LogicalSize",
	"MainLoopRc",
	"MissedTickBehavior",
	"ModelSelection",
	"MouseButton",
	"MutationScope",
	"Mutex",
	"Notify",
	"OAuthCompletion",
	"OAuthCustomDispatchError",
	"OAuthExchangeKind",
	"OFlag",
	"OpaqueJson",
	"OpenFile",
	"OpenOptions",
	"OutputChannel",
	"OutputFlags",
	"OwnerPipeListener",
	"ParentSessionHost",
	"ParsedUri",
	"Path",
	"PathBuf",
	"PathconfVar",
	"Pid",
	"PlanArtifactStore",
	"PlaybackStream",
	"PodSerializer",
	"PointCx",
	"PowerActivity",
	"PresentationEffect",
	"PresenterSlot",
	"PreviewRegistry",
	"PrivateKeyWithHashAlg",
	"ProductionPromptHead",
	"ProductionProviderApplicationOwner",
	"PromptAssetId",
	"PromptControlOwner",
	"PromptOverrides",
	"PromptSettings",
	"PromptSnapshot",
	"ProvenanceKind",
	"ProviderControlAuthorityFactory",
	"ProviderModelEvent",
	"PyDict",
	"PyKeyError",
	"PyOSError",
	"PyRuntimeError",
	"PyValueError",
	"QuoteContext",
	"RawDir",
	"RawValue",
	"Receiver",
	"ReflectionHost",
	"ReflectionHostError",
	"RegexBuilder",
	"RelayEndpoint",
	"RemoteImage",
	"RenameFlags",
	"ResolverTable",
	"ResourceCapability",
	"ResourceCompletion",
	"RetainedChat",
	"RetryAction",
	"RouteProviderService",
	"RuntimePromptMemorySource",
	"SaFlags",
	"SchemaSettings",
	"SealedBodyPlacement",
	"SearchFeatureBits",
	"SecretKind",
	"SecretMode",
	"SecretRule",
	"SecretSessionError",
	"SecretSessionSnapshot",
	"ServiceManifest",
	"SessionIndex",
	"SessionTitleState",
	"SessionTree",
	"SetArg",
	"SettingsManager",
	"SettingsManagerError",
	"SettingsPaths",
	"ShareProjection",
	"ShareStore",
	"ShareStoreKind",
	"SigAction",
	"SigHandler",
	"SigSet",
	"SignalKind",
	"Signals",
	"SkillInvocationKind",
	"SnapshotFault",
	"SourceSpec",
	"SpawnRequestV1",
	"SpawnSchemaMode",
	"SpliceFlags",
	"StateRevision",
	"StaticDeclarations",
	"Str",
	"StreamBox",
	"StreamFlags",
	"StreamRecoveryKind",
	"StringValueParser",
	"TaskDescriptionSnapshot",
	"TcpListener",
	"TcpStream",
	"TelemetryControlAuthority",
	"TelemetryIndex",
	"TelemetryIndexQuery",
	"TerminalBreadcrumbs",
	"Termios",
	"TextProxy",
	"ThinkingEffort",
	"TimeVal",
	"TimeZone",
	"TimeoutBounds",
	"TokenRateMeter",
	"ToolChoice",
	"ToolResultContent",
	"UiControlAuthority",
	"UiControlResult",
	"UnixListener",
	"UnixStream",
	"Url",
	"UsageBucketWidth",
	"UsageWho",
	"ValueProxy",
	"VerdictAuthority",
	"VerdictAuthorityIdentity",
	"VerdictControlAuthority",
	"VideoInfoRaw",
	"WaitPidFlag",
	"WebEndpoint",
	"WebError",
	"WebSocketConfig",
	"WirePolicyId",
	"WkFrames",
	"WriteOperation",
	"YieldType",
];

/// Generic names that always retain one identifying module qualifier.
const QUALIFIED_NAMES: &[&str] = &[
	"Accuracy",
	"Action",
	"ArchiveFormat",
	"AttachOutput",
	"Attribute",
	"AudioFormat",
	"AvailabilityDelta",
	"Background",
	"BashCommand",
	"BashRedirect",
	"Blob",
	"Body",
	"Builder",
	"CampaignPoint",
	"Catalog",
	"Change",
	"Channel",
	"ChatParams",
	"CollabFrame",
	"Command",
	"ComposerStyle",
	"Config",
	"ConflictReason",
	"Content",
	"ContextKind",
	"ContextRef",
	"ContextType",
	"ContextValue",
	"ControlError",
	"ControlRequestContext",
	"Cost",
	"CreateWorktree",
	"CredentialMeta",
	"Credit",
	"Cursor",
	"DapCapability",
	"Data",
	"Date",
	"Decision",
	"Decoder",
	"Delta",
	"DesktopEffects",
	"Detail",
	"Dir",
	"DirectFilesystemRequest",
	"Direction",
	"DiskState",
	"DocEffects",
	"DocumentOp",
	"EditorSpec",
	"EffectEnvelope",
	"Endpoint",
	"Entry",
	"Error",
	"ErrorClass",
	"ErrorKind",
	"Event",
	"ExecEffects",
	"ExecStatusMsg",
	"Executor",
	"Expect",
	"Fallback",
	"Fault",
	"FaultCode",
	"File",
	"Format",
	"Frame",
	"Framing",
	"GenerateImageRequest",
	"GitignoreBuilder",
	"Granularity",
	"Handle",
	"HookEventId",
	"HookPhase",
	"HostExit",
	"HostInfo",
	"Id",
	"ImageAttachment",
	"ImageFormat",
	"InferenceEffects",
	"Input",
	"InvokeInput",
	"InvokeTool",
	"Item",
	"JournalWorkerEnvelope",
	"JsonSchema",
	"Key",
	"Kind",
	"LifecycleWorkerEnvelope",
	"ListWorkspaceSnapshots",
	"LiteralPathProbe",
	"LiveSet",
	"LocalFlags",
	"Map",
	"MaterializeSite",
	"MergeMode",
	"MergeWorktree",
	"Message",
	"MessageRef",
	"Method",
	"Modality",
	"ModalityBits",
	"Mode",
	"MountSlot",
	"Mutation",
	"NetRef",
	"Op",
	"Operation",
	"Outcome",
	"ParamType",
	"Parser",
	"PartDelta",
	"PartEnd",
	"PartStart",
	"PathRef",
	"Payload",
	"Permissions",
	"Pod",
	"Point",
	"Policy",
	"PolicyDenied",
	"PreludeParam",
	"Probe",
	"ProcessIdentity",
	"ProcessInfo",
	"ProcessStarted",
	"ProcessState",
	"Quality",
	"RawTerminalInputFrame",
	"Reader",
	"Reasoning",
	"Recency",
	"RecvError",
	"RelayControl",
	"Resettable",
	"ResizeRequest",
	"Resolve",
	"Resource",
	"ResponseFormat",
	"RestartPolicy",
	"RestartSpec",
	"RestoreWorkspace",
	"ResultPayload",
	"RetainedFrame",
	"RetainedFrameEnvelope",
	"Router",
	"Scheme",
	"Scope",
	"Script",
	"SearchRequest",
	"SearchResponse",
	"SelectSpec",
	"Selection",
	"Sender",
	"Serializer",
	"ServerHello",
	"ServiceCall",
	"Setting",
	"ShellProfileInput",
	"Signal",
	"SnapshotWorkspace",
	"Source",
	"Span",
	"SpeakRequest",
	"Spec",
	"State",
	"StateScope",
	"Status",
	"Step",
	"StopReason",
	"StoreError",
	"Syntax",
	"Target",
	"Thread",
	"ThreadDelta",
	"Time",
	"Tool",
	"ToolCall",
	"ToolResult",
	"Transport",
	"TurnEvent",
	"Type",
	"UiControlRequest",
	"UiEffect",
	"Unexpected",
	"Unit",
	"Usage",
	"User",
	"Value",
	"ValueList",
	"ValueMap",
	"ValueSource",
	"Verdict",
	"VideoFormat",
	"WaitStatus",
	"WholeDocument",
	"Win32",
	"WirePolicy",
	"WorkspaceConflict",
	"WorkspaceRestored",
	"WorkspaceRootSetRequest",
	"WorkspaceSnapshot",
	"WorktreeOp",
	"Writer",
];

fn requires_import(names: &[String], max_segments: usize) -> bool {
	if names.first().is_some_and(|name| name == "std") && names.len() > 2 {
		return true;
	}
	if names.first().is_some_and(|name| name == "windows_sys") {
		return names.len() > max_segments;
	}
	if let Some(index) = bare_type_index(names) {
		return index > 0;
	}
	names.len() > max_segments
}
fn bare_name_collision(bindings: &Bindings, path: &ast::Path, names: &[String]) -> bool {
	let Some(index) = bare_type_index(names) else {
		return false;
	};
	index > 0 && bindings.binds(&scope::chain(path.syntax()), &names[index])
}

fn bare_type_index(names: &[String]) -> Option<usize> {
	names
		.iter()
		.enumerate()
		.find(|(index, name)| {
			BARE_NAMES.contains(&name.as_str())
				&& (*index == 0
					|| names[..*index]
						.iter()
						.all(|segment| is_module_name(segment)))
		})
		.map(|(index, _)| index)
		.or_else(|| {
			let collection_root = starts_with(names, &["std", "collections"])
				|| names.first().is_some_and(|name| name == "collections");
			collection_root
				.then(|| names.iter().position(|name| is_type_name(name)))
				.flatten()
		})
}

fn qualified_type_index(names: &[String]) -> Option<usize> {
	names
		.iter()
		.enumerate()
		.find(|(index, name)| {
			(PRELUDE.contains(&name.as_str()) || QUALIFIED_NAMES.contains(&name.as_str()))
				&& *index > 0
				&& names[..*index]
					.iter()
					.all(|segment| is_module_name(segment))
		})
		.map(|(index, _)| index)
}

fn is_module_name(name: &str) -> bool {
	name.chars().next().is_some_and(char::is_lowercase) || matches!(name, "crate" | "self" | "super")
}

fn is_type_name(name: &str) -> bool {
	name.chars().next().is_some_and(char::is_uppercase) && name.chars().any(char::is_lowercase)
}

fn starts_with(names: &[String], prefix: &[&str]) -> bool {
	names.len() >= prefix.len()
		&& names
			.iter()
			.zip(prefix)
			.all(|(name, expected)| name == expected)
}

/// Decide what to import and what to keep at the use site, then let the
/// fixer construct the plan.
///
/// Only names with an explicit policy are imported bare. Generic names retain
/// one module qualifier; unknown type names stay diagnostics instead of
/// inventing aliases or guessing the preferred spelling.
fn plan(ctx: &FileContext<'_>, path: &ast::Path, names: &[String]) -> Option<PathFix> {
	if is_relative_root(&names[0]) {
		if let Some(index) = qualified_type_index(names) {
			return PathFix::plan(ctx.text, path, &names[..index], names.len() - index + 1);
		}
		if let Some(index) = names.iter().position(|name| is_type_name(name)) {
			return PathFix::plan(ctx.text, path, &names[..=index], names.len() - index);
		}
		return PathFix::plan(ctx.text, path, names, 1);
	}
	if let Some(index) = bare_type_index(names) {
		if index == 1 && matches!(names[0].as_str(), "collections" | "path" | "fmt") {
			return None;
		}
		return PathFix::plan(ctx.text, path, &names[..=index], names.len() - index);
	}
	if let Some(index) = qualified_type_index(names) {
		if index == 0 {
			return None;
		}
		return PathFix::plan(ctx.text, path, &names[..index], names.len() - index + 1);
	}
	if names.first().is_some_and(|name| name == "std") {
		if let Some(index) = names.iter().position(|name| is_type_name(name)) {
			return PathFix::plan(ctx.text, path, &names[..index], names.len() - index + 1);
		}
		return PathFix::plan(ctx.text, path, &names[..names.len() - 1], 2);
	}
	if names.first().is_some_and(|name| name == "tokio") {
		if names.get(1).is_some_and(|name| name == "fs") {
			return None;
		}
		if let Some(index) = names.iter().position(|name| is_type_name(name)) {
			return PathFix::plan(ctx.text, path, &names[..index], names.len() - index + 1);
		}
		return PathFix::plan(ctx.text, path, &names[..names.len() - 1], 2);
	}
	if names.iter().any(|name| is_type_name(name)) || names.len() < 3 {
		return None;
	}
	None
}

/// Whether a path roots at the local crate tree rather than an external one.
fn is_relative_root(name: &str) -> bool {
	matches!(name, "crate" | "super" | "self")
}

/// Every topmost inline qualified path in the file with its segment names —
/// use items, visibility restrictions, and unanalyzable shapes excluded.
fn inline_paths<'c>(
	ctx: &'c FileContext<'_>,
) -> impl Iterator<Item = (ast::Path, Vec<String>)> + 'c {
	ctx.tree.syntax().descendants().filter_map(|node| {
		let path = ast::Path::cast(node)?;
		// Only topmost paths: a qualifier's parent is the outer PATH node.
		if path.syntax().parent()?.kind() == SyntaxKind::PATH {
			return None;
		}
		// Skip use items and visibility restrictions (`pub(in crate::x)`).
		if path
			.syntax()
			.ancestors()
			.any(|ancestor| matches!(ancestor.kind(), SyntaxKind::USE | SyntaxKind::VISIBILITY))
		{
			return None;
		}
		let names = segment_names(&path)?;
		(names.len() >= 2).then_some((path, names))
	})
}

/// Segment names of `path`, outermost-first, or `None` when the path has
/// shapes these lints won't touch (type anchors `<T as Tr>::`, generic args
/// in qualifiers).
fn segment_names(path: &ast::Path) -> Option<Vec<String>> {
	let mut names = Vec::new();
	let mut anchored = false;
	for segment in path.segments() {
		if segment.type_anchor().is_some() {
			anchored = true;
			continue;
		}
		if let Some(name) = segment.name_ref() {
			names.push(name.text().to_string());
		} else if segment.crate_token().is_some() {
			names.push("crate".into());
		} else if segment.super_token().is_some() {
			names.push("super".into());
		} else if segment.self_token().is_some() {
			names.push("self".into());
		} else {
			return None;
		}
	}
	if anchored && names.len() <= 1 {
		return None;
	}
	Some(names)
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::requires_import;
	use crate::{
		fix,
		lint::{AnyLint, FileContext},
	};

	fn names(path: &str) -> Vec<String> {
		path.split("::").map(str::to_owned).collect()
	}

	fn fixed(input: &str) -> (String, usize) {
		let ctx = FileContext::new(Path::new("fixture.rs"), input);
		let rule = super::LongPath { max_segments: 3 };
		let mut diagnostics = Vec::new();
		rule.detect_erased(&ctx, &mut |diagnosis| diagnostics.push(diagnosis));
		super::StdPath.detect_erased(&ctx, &mut |diagnosis| diagnostics.push(diagnosis));
		super::TokioPath.detect_erased(&ctx, &mut |diagnosis| diagnostics.push(diagnosis));
		fix::apply(&ctx, diagnostics, "")
	}

	#[test]
	fn allows_short_and_qualified_generic_paths() {
		assert!(!requires_import(&names("aa::bbb"), 3));
		assert!(!requires_import(&names("io::Error::new"), 3));
		assert!(!requires_import(&names("io::ErrorKind::NotFound"), 3));
		assert!(!requires_import(&names("toml::Value::String"), 3));
		assert!(!requires_import(&names("png::BitDepth::Eight"), 3));
		assert!(!requires_import(&names("grep::SearchResult::default"), 3));
		assert!(!requires_import(&names("SettingKind::Path"), 3));
		assert!(!requires_import(&names("thread::v1::Blob"), 3));
	}

	#[test]
	fn enforces_explicit_namespace_styles() {
		for path in [
			"std::io::Error",
			"std::collections::HashMap",
			"collections::HashMap",
			"std::path::Path",
			"path::Path",
			"url::Url",
			"omp_core::Str",
			"std::fmt::Display",
		] {
			assert!(requires_import(&names(path), 3), "{path}");
		}
		assert!(!requires_import(&names("HashMap::new"), 3));
		assert!(!requires_import(&names("Path::new"), 3));
	}

	#[test]
	fn fixer_uses_named_types_and_qualified_names_without_aliases() {
		let (fixed, applied) = fixed(
			"fn f() { let _ = std::io::Error::other(\"x\"); let _ = std::collections::HashMap::<u8, \
			 u8>::new(); let _ = std::path::Path::new(\"x\"); let _ = \
			 std::time::Duration::from_secs(1); let _ = tokio::runtime::Handle::current(); }",
		);
		assert_eq!(applied, 5);
		assert!(fixed.contains("use std::collections::HashMap;"));
		assert!(fixed.contains("use std::io;"));
		assert!(fixed.contains("use std::path::Path;"));
		assert!(fixed.contains("io::Error::other"));
		assert!(fixed.contains("HashMap::<u8, u8>::new"));
		assert!(fixed.contains("use std::time;"));
		assert!(fixed.contains("Path::new"));
		assert!(fixed.contains("use tokio::runtime;"));

		assert!(fixed.contains("time::Duration::from_secs"));
		assert!(fixed.contains("runtime::Handle::current"));
		assert!(!fixed.contains(" as "));
	}

	#[test]
	fn unknown_type_names_remain_diagnostics() {
		let (output, applied) = fixed("use flume::Receiver;\nfn f(_: watch::Receiver<u8>) {}");
		assert_eq!(applied, 0);
		assert_eq!(output, "use flume::Receiver;\nfn f(_: watch::Receiver<u8>) {}");

		let (output, applied) = fixed("fn f() { let _ = tokio::fs::File::open(\"x\"); }");
		assert_eq!(applied, 0);
		assert_eq!(output, "fn f() { let _ = tokio::fs::File::open(\"x\"); }");

		let (output, applied) = fixed("fn f(value: alpha::beta::gamma::Widget) {}");
		assert_eq!(applied, 0);
		assert_eq!(output, "fn f(value: alpha::beta::gamma::Widget) {}");
		let (output, applied) = fixed("fn f<T: fmt::Display>() {}");
		assert_eq!(applied, 0);
		assert_eq!(output, "fn f<T: fmt::Display>() {}");
	}

	#[test]
	fn classified_names_follow_owner_policy() {
		for name in [
			"BridgeHostError",
			"CompareOp",
			"ControlConnectionIdentity",
			"ControlProtocolError",
			"Effort",
			"Instant",
			"Pid",
		] {
			assert!(super::BARE_NAMES.contains(&name));
		}
		for name in [
			"Accuracy",
			"Body",
			"Event",
			"Fault",
			"Input",
			"Kind",
			"Message",
			"Op",
			"Operation",
			"Outcome",
			"Sender",
			"Signal",
			"Unit",
		] {
			assert!(super::QUALIFIED_NAMES.contains(&name));
		}

		let (output, applied) = fixed(
			"fn f(_: omp_envd::exthost::control::ControlProtocolError) { let _ = \
			 omp_proto::thread::v1::item::Kind::Message; }",
		);
		assert_eq!(applied, 2);
		assert!(output.contains("use omp_envd::exthost::control::ControlProtocolError;"));
		assert!(output.contains("use omp_proto::thread::v1::item;"));
		assert!(output.contains("item::Kind::Message"));
		assert!(!output.contains(" as "));
	}
}
