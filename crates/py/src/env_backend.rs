// Typed per-operation Environment DATA-plane method table.

macro_rules! backend_request_method {
	($name:ident, $operation:literal, [$($argument:literal),* $(,)?]) => {
		#[pymethods]
		impl PyEnvironmentBackend {
			#[pyo3(signature = (*args, **kwargs))]
			fn $name(
				&self,
				py: Python<'_>,
				args: &Bound<'_, PyTuple>,
				kwargs: Option<&Bound<'_, PyDict>>,
			) -> PyResult<Py<PyAny>> {
				self.forward_request(py, $operation, &[$($argument),*], args, kwargs)
			}
		}
	};
}

macro_rules! backend_stream_method {
	($name:ident, $operation:literal, [$($argument:literal),* $(,)?]) => {
		#[pymethods]
		impl PyEnvironmentBackend {
			#[pyo3(signature = (*args, **kwargs))]
			fn $name(
				&self,
				py: Python<'_>,
				args: &Bound<'_, PyTuple>,
				kwargs: Option<&Bound<'_, PyDict>>,
			) -> PyResult<Py<PyAny>> {
				self.forward_stream(py, $operation, &[$($argument),*], args, kwargs)
			}
		}
	};
}

backend_request_method!(worktree, "omp.env.worktree", []);
backend_request_method!(docs_open, "omp.env.docs.open", ["path", "language", "create"]);
backend_request_method!(docs_read_bytes, "omp.env.docs.read_bytes", ["path"]);
backend_request_method!(doc_read_bytes, "omp.env.docs.Doc.read_bytes", ["lease", "revision"]);
backend_request_method!(doc_refresh, "omp.env.docs.Doc.refresh", ["lease"]);
backend_request_method!(doc_lines, "omp.env.docs.Doc.lines", ["lease", "start", "end", "revision"]);
backend_request_method!(doc_summary, "omp.env.docs.Doc.summary", ["lease", "options"]);
backend_request_method!(doc_edit, "omp.env.docs.Doc.edit", ["lease", "edits"]);
backend_request_method!(doc_write, "omp.env.docs.Doc.write", ["lease", "data"]);
backend_request_method!(doc_hashline, "omp.env.docs.Doc.hashline", ["lease", "patch"]);
backend_request_method!(doc_close, "omp.env.docs.Doc.close", ["lease"]);
backend_stream_method!(doc_events, "omp.env.docs.Doc.events", ["lease"]);
backend_request_method!(txn_commit, "omp.env.Txn.commit", ["txn_id", "operations"]);
backend_request_method!(fs_stat, "omp.env.fs.stat", ["path"]);
backend_request_method!(fs_lstat, "omp.env.fs.lstat", ["path"]);
backend_request_method!(fs_canonicalize, "omp.env.fs.canonicalize", ["path"]);
backend_request_method!(fs_list_dir, "omp.env.fs.list_dir", ["path", "follow"]);
backend_request_method!(fs_read_link, "omp.env.fs.read_link", ["path"]);
backend_request_method!(fs_mkdir, "omp.env.fs.mkdir", ["path", "parents", "exist_ok"]);
backend_request_method!(fs_remove, "omp.env.fs.remove", ["path", "recursive", "revision"]);
backend_request_method!(fs_rename, "omp.env.fs.rename", ["src", "dest", "overwrite", "src_revision", "dest_revision"]);
backend_request_method!(fs_copy, "omp.env.fs.copy", ["src", "dest", "follow", "overwrite", "dest_revision"]);
backend_request_method!(fs_symlink, "omp.env.fs.symlink", ["target", "link", "kind", "relative", "overwrite"]);
backend_request_method!(fs_hard_link, "omp.env.fs.hard_link", ["src", "link", "follow", "overwrite"]);
backend_request_method!(fs_chmod, "omp.env.fs.chmod", ["path", "read_only", "executable", "follow", "revision"]);
backend_request_method!(lsp_bindings, "omp.env.lsp.bindings", ["path"]);
backend_request_method!(lsp_request, "omp.env.lsp.request", ["server", "method", "params", "lease", "on_stale", "timeout"]);
backend_request_method!(lsp_notify, "omp.env.lsp.notify", ["server", "method", "params"]);
backend_stream_method!(lsp_events, "omp.env.lsp.events", []);
backend_request_method!(session_run, "omp.env.Session.run", ["session", "script"]);
backend_request_method!(session_close, "omp.env.Session.close", ["session"]);
backend_request_method!(run_stdin, "omp.env.Run.stdin", ["run", "data"]);
backend_request_method!(run_eof, "omp.env.Run.eof", ["run"]);
backend_request_method!(run_signal, "omp.env.Run.signal", ["run", "signal"]);
backend_request_method!(run_resize, "omp.env.Run.resize", ["run", "rows", "columns"]);
backend_request_method!(run_wait, "omp.env.Run.wait", ["run"]);
backend_request_method!(run_detach, "omp.env.Run.detach", ["run", "name"]);
backend_stream_method!(run_events, "omp.env.Run.events", ["run"]);
backend_request_method!(process_info, "omp.env.Process.info", ["name", "generation"]);
backend_request_method!(process_restart, "omp.env.Process.restart", ["name", "generation"]);
backend_request_method!(process_send, "omp.env.Process.send", ["name", "generation", "data"]);
backend_request_method!(process_eof, "omp.env.Process.eof", ["name", "generation"]);
backend_request_method!(process_signal, "omp.env.Process.signal", ["name", "generation", "signal"]);
backend_request_method!(process_stop, "omp.env.Process.stop", ["name", "generation", "grace"]);
backend_stream_method!(process_output, "omp.env.Process.output", ["name", "generation", "after"]);
backend_stream_method!(process_states, "omp.env.Process.states", ["name", "generation"]);
backend_request_method!(proc_start, "omp.env.proc.start", ["name", "script", "cwd", "env", "pty", "restart", "ready"]);
backend_request_method!(proc_ensure, "omp.env.proc.ensure", ["name", "script", "cwd", "env", "pty", "restart", "ready"]);
backend_request_method!(proc_list, "omp.env.proc.list", []);
backend_request_method!(proc_adopt, "omp.env.proc.adopt", ["name"]);
backend_request_method!(find_files, "omp.env.find.files", ["root"]);
backend_request_method!(find_grep, "omp.env.find.grep", ["pattern", "root"]);
backend_stream_method!(find_walk, "omp.env.find.walk", ["root"]);
backend_request_method!(blobs_put_bytes, "omp.env.blobs.put", ["data"]);
backend_request_method!(blobs_put_path, "omp.env.blobs.put", ["data"]);
backend_request_method!(blobs_get, "omp.env.blobs.get", ["ref", "offset", "length"]);
backend_request_method!(blobs_stat, "omp.env.blobs.stat", ["ref"]);
backend_request_method!(blobs_delete, "omp.env.blobs.delete", ["ref"]);
backend_stream_method!(blobs_stream, "omp.env.blobs.stream", ["ref", "offset", "length"]);
backend_request_method!(blob_write, "omp.env.BlobWriter.write", ["upload", "chunk"]);
backend_request_method!(blob_commit, "omp.env.BlobWriter.commit", ["upload"]);
#[pymethods]
impl PyEnvironmentBackend {
	#[pyo3(signature = (*args, **kwargs))]
	fn session_open(
		&self,
		py: Python<'_>,
		args: &Bound<'_, PyTuple>,
		kwargs: Option<&Bound<'_, PyDict>>,
	) -> PyResult<Py<PyAny>> {
		if args.len() > 3 {
			return Err(PyTypeError::new_err("session_open takes at most 3 positional arguments"));
		}
		let arguments = PyDict::new(py);
		if let Some(kwargs) = kwargs {
			for (key, value) in kwargs {
				arguments.set_item(key, value)?;
			}
		}
		for (index, value) in args.iter().enumerate() {
			arguments.set_item(["cwd", "env", "pty"][index], value)?;
		}
		self.session(py, &arguments)
	}

	#[pyo3(signature = (*args, **kwargs))]
	fn http_request(
		&self,
		py: Python<'_>,
		args: &Bound<'_, PyTuple>,
		kwargs: Option<&Bound<'_, PyDict>>,
	) -> PyResult<Py<PyAny>> {
		if args.is_empty() {
			return Err(PyTypeError::new_err("http_request requires method"));
		}
		let method = args.iter().next().expect("nonempty args").extract::<String>()?;
		let operation = match method.as_str() {
			"GET" => "omp.env.http_get",
			"POST" => "omp.env.http_post",
			"PUT" => "omp.env.http_put",
			_ => return Err(PyValueError::new_err("HTTP method must be GET, POST, or PUT")),
		};
		let remaining = args.get_slice(1, args.len());
		self.forward_request(
			py,
			operation,
			&["url", "body", "headers", "timeout", "redirects"],
			&remaining,
			kwargs,
		)
	}
}

