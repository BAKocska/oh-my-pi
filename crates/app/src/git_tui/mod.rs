//! Shared backend for the standalone and chat-hosted Git workbench.

pub mod avatar;
pub mod model;

use std::{
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use omp_chat_ui::git::{GitIntent, GitUpdate};
use omp_core::{IntoStr as _, Str};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use self::{
	avatar::AvatarLoader,
	model::{GitModel, GitModelError},
};

/// Result of applying one workbench intent.
#[derive(Debug, Default)]
pub struct GitIntentResult {
	/// Backend updates to deliver to the workbench in order.
	pub updates: Vec<GitUpdate>,
	/// Whether the workbench requested closure.
	pub close:   bool,
}

/// Cloneable controller shared by refresh and interaction tasks.
#[derive(Clone)]
pub struct GitSession {
	model:  Arc<Mutex<GitModel>>,
	avatar: Option<AvatarLoader>,
	busy:   Arc<AtomicBool>,
	cancel: CancellationToken,
}

impl GitSession {
	/// Opens a repository session and resolves an optional pinned revision.
	pub async fn open(
		cwd: &Path,
		revision: Option<&str>,
		cancel: CancellationToken,
	) -> Result<Self, GitModelError> {
		let model = GitModel::open(cwd, revision, &cancel).await?;
		Ok(Self {
			model: Arc::new(Mutex::new(model)),
			avatar: AvatarLoader::new(),
			busy: Arc::new(AtomicBool::new(false)),
			cancel,
		})
	}

	/// Returns the initial repository snapshot.
	pub async fn initial_snapshot(&self) -> Result<omp_chat_ui::git::GitSnapshot, GitModelError> {
		self.model.lock().await.force_refresh(&self.cancel).await
	}

	/// Polls for a changed repository snapshot.
	pub async fn poll_refresh(
		&self,
	) -> Result<Option<omp_chat_ui::git::GitSnapshot>, GitModelError> {
		self.model.lock().await.refresh(&self.cancel).await
	}

	/// Loads line counts for the current fast snapshot, returning the
	/// stats-enriched follow-up snapshot once.
	pub async fn deferred_stats(
		&self,
	) -> Result<Option<omp_chat_ui::git::GitSnapshot>, GitModelError> {
		self
			.model
			.lock()
			.await
			.load_deferred_stats(&self.cancel)
			.await
	}

	/// Applies one UI intent through the same path used by both workbench hosts.
	pub async fn handle(&self, intent: GitIntent) -> GitIntentResult {
		match intent {
			GitIntent::Close => {
				self.cancel.cancel();
				GitIntentResult { close: true, updates: Vec::new() }
			},
			GitIntent::Refresh => match self.model.lock().await.force_refresh(&self.cancel).await {
				Ok(snapshot) => one(GitUpdate::Snapshot(snapshot)),
				Err(error) => failed(error),
			},
			GitIntent::Load { area, path, orig_path, seq } => {
				let result = self
					.model
					.lock()
					.await
					.contents(area, path.as_str(), orig_path.as_deref(), &self.cancel)
					.await;
				match result {
					Ok(contents) => one(GitUpdate::Contents { seq, contents }),
					Err(error) => failed(error),
				}
			},
			GitIntent::Avatar { email } => {
				let png = if let Some(loader) = &self.avatar {
					let cwd = self.model.lock().await.cwd().to_path_buf();
					loader.load(email.as_str(), &cwd, &self.cancel).await
				} else {
					None
				};
				one(GitUpdate::Avatar { email, png })
			},
			intent => self.handle_action(intent).await,
		}
	}

	async fn handle_action(&self, intent: GitIntent) -> GitIntentResult {
		let Ok(_busy) = BusyGuard::enter(&self.busy) else {
			return GitIntentResult::default();
		};
		let mut model = self.model.lock().await;
		let action = match intent {
			GitIntent::StageFile(path) => model.stage(path.as_deref(), &self.cancel).await,
			GitIntent::UnstageFile(path) => model.unstage(path.as_deref(), &self.cancel).await,
			GitIntent::ApplyLines { op, path, old, new } => {
				model
					.apply_lines(op, path.as_str(), old, new, &self.cancel)
					.await
			},
			GitIntent::Commit { message, amend, stage_all } => {
				model
					.commit(message.as_str(), amend, stage_all, &self.cancel)
					.await
			},
			GitIntent::Refresh
			| GitIntent::Load { .. }
			| GitIntent::Avatar { .. }
			| GitIntent::Close => {
				unreachable!("non-action intent routed to action handler")
			},
		};
		let mut updates = Vec::with_capacity(2);
		match action {
			Ok(message) => updates.push(GitUpdate::ActionDone { message }),
			Err(error) => updates.push(GitUpdate::ActionFailed { message: render_error(&error) }),
		}
		match model.force_refresh(&self.cancel).await {
			Ok(snapshot) => updates.push(GitUpdate::Snapshot(snapshot)),
			Err(error) => updates.push(GitUpdate::ActionFailed { message: render_error(&error) }),
		}
		GitIntentResult { updates, close: false }
	}
}

struct BusyGuard<'a>(&'a AtomicBool);

impl<'a> BusyGuard<'a> {
	fn enter(busy: &'a AtomicBool) -> Result<Self, ()> {
		busy
			.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
			.map(|_| Self(busy))
			.map_err(|_| ())
	}
}

impl Drop for BusyGuard<'_> {
	fn drop(&mut self) {
		self.0.store(false, Ordering::Release);
	}
}

fn one(update: GitUpdate) -> GitIntentResult {
	GitIntentResult { updates: vec![update], close: false }
}

fn failed(error: GitModelError) -> GitIntentResult {
	one(GitUpdate::ActionFailed { message: render_error(&error) })
}

fn render_error(error: &GitModelError) -> Str {
	error.to_string().to_str()
}
