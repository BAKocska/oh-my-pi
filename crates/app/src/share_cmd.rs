//! Standalone encrypted transcript sharing over the production HTTP store.

use std::sync::Arc;

use miette::{IntoDiagnostic as _, miette};

use crate::{
	cli::ShareArgs,
	export::SessionTree,
	secrets::session::SecretSessionSnapshot,
	settings::{ExportSettings, ShareStore},
	share::{
		DirectShareStore, HTTP_MAX_SEALED_BYTES, ShareProjection, ShareStoreKind, seal, upload,
	},
};

/// Selects a live journal projection, irreversibly redacts it, seals it, and
/// uploads only ciphertext to the configured share store.
pub async fn run(args: ShareArgs) -> miette::Result<()> {
	let data_dir = crate::cli::data_dir(None)?;
	let journal = match args.journal {
		Some(path) => path,
		None => {
			let selection = crate::pickers::pick_session(&data_dir, None)
				.await
				.map_err(|error| miette!("{error}"))?
				.ok_or_else(|| miette!("no session selected"))?;
			selection
				.sessions_dir
				.join(format!("{}.jsonl", selection.session.id.0))
		},
	};
	let tree = SessionTree::load(&journal).map_err(|error| miette!("{error}"))?;
	let value = serde_json::to_value(tree).into_diagnostic()?;
	let project = std::env::current_dir().into_diagnostic()?;
	let configured = crate::settings::current(&data_dir).map_err(|error| miette!("{error}"))?;
	let secrets = SecretSessionSnapshot::build(
		0,
		&data_dir.join("secrets.toml"),
		&project.join(".omp/secrets.toml"),
		std::iter::empty(),
	)
	.map_err(|error| miette!("{error}"))?;
	let projection = ShareProjection::materialize_bounded(
		value,
		ExportSettings {
			share_redact_secrets: configured.export.share_redact_secrets && !args.no_redact,
		},
		&secrets,
		HTTP_MAX_SEALED_BYTES.saturating_sub(64 * 1024),
	);
	let sealed = seal(&projection).map_err(|error| miette!("{error}"))?;
	let server = args.server.as_ref().unwrap_or(&configured.share.server_url);
	let credentials = Arc::new(crate::envd::github_url::GithubCredentialBridge::new());
	let authority = Arc::new(crate::auth_backend::combined_authority(
		crate::daemon::open_credential_store(data_dir.join("credentials.db")).into_diagnostic()?,
	));
	credentials
		.bind(authority)
		.map_err(|_| miette!("GitHub credential authority is already bound"))?;
	let store =
		DirectShareStore::new(server.as_str(), credentials).map_err(|error| miette!("{error}"))?;
	let result = upload(
		&store,
		match configured.share.store {
			ShareStore::Http => ShareStoreKind::Http,
			ShareStore::Gist => ShareStoreKind::Gist,
		},
		&sealed,
		args.viewer.as_str(),
	)
	.await
	.map_err(|error| miette!("{error}"))?;
	println!("{}", result.url);
	Ok(())
}
