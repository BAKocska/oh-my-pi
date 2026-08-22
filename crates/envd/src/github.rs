//! Direct GitHub API and isolated pull-request worktree host.

use std::{
	path::{Path, PathBuf},
	process::Command,
	sync::Arc,
	time::Duration,
};

use async_trait::async_trait;
use bytes::BytesMut;
use futures::StreamExt as _;
use http::{
	HeaderMap, HeaderValue,
	header::{ACCEPT, USER_AGENT},
};
use omp_core::{Str, sf};
use omp_inference::auth::HeaderPlacement;
use omp_tools::github::{Fault, GithubHost, Operation, Params, Payload};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::github_url::GithubCredentialBridge;

const API: &str = "https://api.github.com";
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Combined-credential GitHub owner.
pub(crate) struct GithubService {
	root:        PathBuf,
	worktrees:   PathBuf,
	credentials: Arc<GithubCredentialBridge>,
	client:      wreq::Client,
}

impl GithubService {
	pub(crate) fn new(
		root: PathBuf,
		state_dir: &Path,
		credentials: Arc<GithubCredentialBridge>,
	) -> Arc<Self> {
		Arc::new(Self {
			root,
			worktrees: state_dir.join("github-worktrees"),
			credentials,
			client: wreq::Client::builder()
				.redirect(wreq::redirect::Policy::none())
				.build()
				.expect("GitHub client"),
		})
	}

	async fn request(
		&self,
		method: Method,
		path: &str,
		body: Option<&Value>,
	) -> Result<ApiResponse, Fault> {
		let mut headers = HeaderMap::new();
		headers.insert(USER_AGENT, HeaderValue::from_static("omp-github-device"));
		headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
		headers.insert("x-github-api-version", HeaderValue::from_static("2022-11-28"));
		if let Some(lease) = self.credentials.lease().await.map_err(|error| Fault {
			code:    sf!("github_credentials_failed"),
			message: Str::new(error.message().clone()),
		})? {
			lease
				.apply_header(&HeaderPlacement::bearer(), &mut headers)
				.map_err(|_| Fault {
					code:    sf!("github_credentials_failed"),
					message: sf!("GitHub credential projection failed"),
				})?;
		}
		let url = format!("{API}{path}");
		let request = match method {
			Method::Get => self.client.get(url),
			Method::Post => self.client.post(url),
		};
		let request = if let Some(body) = body {
			request.json(body)
		} else {
			request
		};
		let response = request.headers(headers).send().await.map_err(http_fault)?;
		let status = response.status().as_u16();
		let remaining = response
			.headers()
			.get("x-ratelimit-remaining")
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.parse().ok());
		let reset = response
			.headers()
			.get("x-ratelimit-reset")
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.parse().ok());
		let mut bytes = BytesMut::new();
		let mut stream = response.bytes_stream();
		while let Some(chunk) = stream.next().await {
			let chunk = chunk.map_err(http_fault)?;
			if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
				return Err(fault("github_response_too_large", "GitHub response exceeds 16 MiB"));
			}
			bytes.extend_from_slice(&chunk);
		}
		if !(200..300).contains(&status) {
			let message = serde_json::from_slice::<Value>(&bytes)
				.ok()
				.and_then(|value| {
					value
						.get("message")
						.and_then(Value::as_str)
						.map(str::to_owned)
				})
				.unwrap_or_else(|| format!("GitHub API returned HTTP {status}"));
			return Err(Fault { code: sf!("github_http_error"), message: Str::new(message) });
		}
		let value = if bytes.is_empty() {
			Value::Null
		} else {
			serde_json::from_slice(&bytes)
				.map_err(|_| fault("github_invalid_response", "GitHub returned malformed JSON"))?
		};
		Ok(ApiResponse { value, remaining, reset })
	}

	fn repo(&self, requested: Option<&str>) -> Result<Str, Fault> {
		requested.map(Str::new).map_or_else(
			|| {
				super::github_url::infer_repo(&self.root).map_err(|error| Fault {
					code:    sf!("github_repo_unresolved"),
					message: Str::new(error.message().clone()),
				})
			},
			Ok,
		)
	}
}

#[async_trait]
impl GithubHost for GithubService {
	async fn execute(&self, params: Params) -> Result<Payload, Fault> {
		let response = match params.op {
			Operation::RepoView => {
				self
					.request(
						Method::Get,
						&format!("/repos/{}", self.repo(params.repo.as_deref())?),
						None,
					)
					.await?
			},
			Operation::FileRead => {
				let repo = self.repo(params.repo.as_deref())?;
				let path = required(params.path.as_deref(), "file_read requires `path`")?;
				let mut endpoint = format!("/repos/{repo}/contents/{path}");
				if let Some(branch) = &params.branch {
					endpoint.push_str("?ref=");
					endpoint.push_str(
						&url::form_urlencoded::byte_serialize(branch.as_bytes()).collect::<String>(),
					);
				}
				let mut response = self.request(Method::Get, &endpoint, None).await?;
				if let Some(encoded) = response.value.get("content").and_then(Value::as_str) {
					let compact = encoded
						.bytes()
						.filter(|byte| !byte.is_ascii_whitespace())
						.collect::<Vec<_>>();
					let decoded = omp_core::base64::decode(&compact).into_vec().map_err(|_| {
						fault("github_invalid_content", "GitHub file content was not valid base64")
					})?;
					response.value = json!({ "repo": repo, "path": path, "content": String::from_utf8_lossy(&decoded), "size": decoded.len() });
				}
				response
			},
			Operation::PrCreate => {
				let repo = self.repo(params.repo.as_deref())?;
				let body = json!({
					"title": required(params.title.as_deref(), "pr_create requires `title`")?,
					"body": params.body.as_deref().unwrap_or(""),
					"head": required(params.head.as_deref(), "pr_create requires `head`")?,
					"base": params.base.as_deref().unwrap_or("main"),
					"draft": params.draft,
				});
				self
					.request(Method::Post, &format!("/repos/{repo}/pulls"), Some(&body))
					.await?
			},
			Operation::PrCheckout => self.checkout(&params).await?,
			Operation::PrPush => self.push(&params).await?,
			Operation::SearchIssues
			| Operation::SearchPrs
			| Operation::SearchCode
			| Operation::SearchCommits
			| Operation::SearchRepos => self.search(&params).await?,
			Operation::RunWatch => self.watch(&params).await?,
		};
		Ok(Payload {
			op:                   params.op,
			result:               response.value,
			rate_limit_remaining: response.remaining,
			rate_limit_reset:     response.reset,
		})
	}
}

impl GithubService {
	async fn search(&self, params: &Params) -> Result<ApiResponse, Fault> {
		let (kind, tag) = match params.op {
			Operation::SearchIssues => ("issues", Some("is:issue")),
			Operation::SearchPrs => ("issues", Some("is:pr")),
			Operation::SearchCode => ("code", None),
			Operation::SearchCommits => ("commits", None),
			Operation::SearchRepos => ("repositories", None),
			_ => unreachable!(),
		};
		if params.op == Operation::SearchCode && (params.since.is_some() || params.until.is_some()) {
			return Err(fault("github_invalid_search", "code search does not support date bounds"));
		}
		let mut query = params.query.as_deref().unwrap_or("").trim().to_owned();
		if let Some(tag) = tag {
			if !query.is_empty() {
				query.push(' ');
			}
			query.push_str(tag);
		}
		if params.op != Operation::SearchRepos && !has_scope(&query) {
			let repo = self.repo(params.repo.as_deref())?;
			query.push_str(" repo:");
			query.push_str(&repo);
		}
		let field = params.date_field.as_deref().unwrap_or("created");
		if let Some(since) = &params.since {
			query.push(' ');
			query.push_str(field);
			query.push_str(":>=");
			query.push_str(since);
		}
		if let Some(until) = &params.until {
			query.push(' ');
			query.push_str(field);
			query.push_str(":<=");
			query.push_str(until);
		}
		if query.is_empty() {
			return Err(fault("github_invalid_search", "search requires `query` or a date bound"));
		}
		let encoded = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
		let limit = params.limit.unwrap_or(30).clamp(1, 100);
		self
			.request(Method::Get, &format!("/search/{kind}?q={encoded}&per_page={limit}"), None)
			.await
	}

	async fn checkout(&self, params: &Params) -> Result<ApiResponse, Fault> {
		let repo = self.repo(params.repo.as_deref())?;
		let prs = params
			.pr
			.as_deref()
			.ok_or_else(|| fault("github_invalid_pr", "pr_checkout requires `pr`"))?;
		let mut checkouts = Vec::new();
		for value in prs {
			let number = pr_number(value)?;
			let api = self
				.request(Method::Get, &format!("/repos/{repo}/pulls/{number}"), None)
				.await?;
			let clone_url = api
				.value
				.pointer("/head/repo/clone_url")
				.and_then(Value::as_str)
				.ok_or_else(|| {
					fault("github_invalid_response", "pull request head repository is unavailable")
				})?;
			let head = api
				.value
				.pointer("/head/ref")
				.and_then(Value::as_str)
				.ok_or_else(|| {
					fault("github_invalid_response", "pull request head branch is unavailable")
				})?;
			let path = self.worktrees.join(format!("pr-{number}"));
			let root = self.root.clone();
			let path_for_git = path.clone();
			let git_clone_url = clone_url.to_owned();
			let git_head = head.to_owned();
			tokio::task::spawn_blocking(move || {
				checkout_git(&root, &path_for_git, number, &git_clone_url, &git_head)
			})
			.await
			.map_err(|_| fault("github_git_failed", "GitHub checkout worker failed"))??;
			std::fs::create_dir_all(&path).map_err(io_fault)?;
			let metadata = CheckoutMetadata {
				repo:      repo.to_string(),
				clone_url: clone_url.to_owned(),
				head:      head.to_owned(),
			};
			std::fs::write(
				path.join(".omp-pr-checkout.json"),
				serde_json::to_vec(&metadata).expect("metadata serializes"),
			)
			.map_err(io_fault)?;
			checkouts.push(json!({ "pr": number, "branch": format!("pr-{number}"), "path": path }));
		}
		Ok(ApiResponse {
			value:     json!({ "checkouts": checkouts }),
			remaining: None,
			reset:     None,
		})
	}

	async fn push(&self, params: &Params) -> Result<ApiResponse, Fault> {
		let prs = params.pr.as_deref().ok_or_else(|| {
			fault("github_invalid_pr", "pr_push requires `pr` from a prior checkout")
		})?;
		let mut pushed = Vec::new();
		for value in prs {
			let number = pr_number(value)?;
			let path = self.worktrees.join(format!("pr-{number}"));
			let metadata: CheckoutMetadata = serde_json::from_slice(
				&std::fs::read(path.join(".omp-pr-checkout.json")).map_err(io_fault)?,
			)
			.map_err(|_| {
				fault("github_checkout_missing", "pull request checkout metadata is invalid")
			})?;
			let force = params.force_with_lease;
			let path_for_git = path.clone();
			tokio::task::spawn_blocking(move || push_git(&path_for_git, &metadata, force))
				.await
				.map_err(|_| fault("github_git_failed", "GitHub push worker failed"))??;
			pushed.push(json!({ "pr": number, "path": path }));
		}
		Ok(ApiResponse { value: json!({ "pushed": pushed }), remaining: None, reset: None })
	}

	async fn watch(&self, params: &Params) -> Result<ApiResponse, Fault> {
		let repo = self.repo(params.repo.as_deref())?;
		let mut receipt = None;
		for attempt in 0..100u32 {
			let response = if let Some(run) = &params.run {
				self
					.request(Method::Get, &format!("/repos/{repo}/actions/runs/{}", run_id(run)?), None)
					.await?
			} else {
				let branch = params.branch.as_deref().unwrap_or("main");
				let encoded =
					url::form_urlencoded::byte_serialize(branch.as_bytes()).collect::<String>();
				self
					.request(
						Method::Get,
						&format!("/repos/{repo}/actions/runs?branch={encoded}&per_page=20"),
						None,
					)
					.await?
			};
			let done = runs_complete(&response.value);
			receipt = Some(response);
			if done || attempt == 99 {
				break;
			}
			tokio::time::sleep(Duration::from_secs(if attempt < 20 { 3 } else { 15 })).await;
		}
		receipt.ok_or_else(|| fault("github_actions_missing", "no GitHub Actions runs were returned"))
	}
}

#[derive(Clone, Copy)]
enum Method {
	Get,
	Post,
}
struct ApiResponse {
	value:     Value,
	remaining: Option<u64>,
	reset:     Option<u64>,
}
#[derive(Deserialize, Serialize)]
struct CheckoutMetadata {
	repo:      String,
	clone_url: String,
	head:      String,
}

fn checkout_git(
	root: &Path,
	path: &Path,
	number: u64,
	remote: &str,
	head: &str,
) -> Result<(), Fault> {
	let parent = path
		.parent()
		.ok_or_else(|| fault("github_git_failed", "invalid worktree path"))?;
	std::fs::create_dir_all(parent).map_err(io_fault)?;
	run_git(root, &["fetch", remote, head])?;
	let branch = format!("pr-{number}");
	let path = path.to_string_lossy();
	run_git(root, &["worktree", "add", "-B", &branch, path.as_ref(), "FETCH_HEAD"])
}

fn push_git(path: &Path, metadata: &CheckoutMetadata, force: bool) -> Result<(), Fault> {
	let mut args = vec!["push"];
	if force {
		args.push("--force-with-lease");
	}
	let refspec = format!("HEAD:{}", metadata.head);
	args.extend([metadata.clone_url.as_str(), refspec.as_str()]);
	run_git(path, &args)
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<(), Fault> {
	let output = Command::new("git")
		.current_dir(cwd)
		.args(args)
		.output()
		.map_err(io_fault)?;
	if output.status.success() {
		return Ok(());
	}
	Err(Fault {
		code:    sf!("github_git_failed"),
		message: Str::new(String::from_utf8_lossy(&output.stderr).trim()),
	})
}
fn has_scope(query: &str) -> bool {
	query.split_whitespace().any(|part| {
		["repo:", "org:", "user:", "owner:"]
			.iter()
			.any(|prefix| part.starts_with(prefix))
	})
}
fn pr_number(value: &str) -> Result<u64, Fault> {
	value
		.trim_end_matches('/')
		.rsplit('/')
		.next()
		.unwrap_or(value)
		.parse()
		.map_err(|_| fault("github_invalid_pr", "pull request must be a number or URL"))
}
fn run_id(value: &str) -> Result<u64, Fault> {
	value
		.trim_end_matches('/')
		.rsplit('/')
		.next()
		.unwrap_or(value)
		.parse()
		.map_err(|_| fault("github_invalid_run", "Actions run must be an id or URL"))
}
fn runs_complete(value: &Value) -> bool {
	if let Some(status) = value.get("status").and_then(Value::as_str) {
		return status == "completed";
	}
	value
		.get("workflow_runs")
		.and_then(Value::as_array)
		.is_some_and(|runs| {
			!runs.is_empty()
				&& runs
					.iter()
					.all(|run| run.get("status").and_then(Value::as_str) == Some("completed"))
		})
}
fn required<'a>(value: Option<&'a str>, message: &'static str) -> Result<&'a str, Fault> {
	value.ok_or_else(|| fault("github_invalid_request", message))
}
fn fault(code: &'static str, message: &'static str) -> Fault {
	Fault { code: Str::new_static(code), message: Str::new_static(message) }
}
fn http_fault(error: wreq::Error) -> Fault {
	Fault { code: sf!("github_transport_failed"), message: Str::new(error.to_string()) }
}
fn io_fault(error: std::io::Error) -> Fault {
	Fault { code: sf!("github_io_failed"), message: Str::new(error.to_string()) }
}
