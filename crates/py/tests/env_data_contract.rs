//! Verifies Python environment calls remain invocation-scoped and
//! generation-fenced.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn invocation_backend_is_scoped_and_generation_fenced() {
	let engine = Engine::builder().init().expect("boot embedded Python");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import omp
import omp.env

try:
    omp.env.info()
except omp.EnvUnavailable:
    pass
else:
    raise AssertionError("Environment authority leaked outside an invocation")

class Backend:
    def __init__(self):
        self.calls = []

    async def worktree(self):
        self.calls.append(("worktree",))
        return omp.env.WorktreeInfo("wt", omp.EnvPath("file:///workspace/wt"), "base", 4)

    async def process_restart(self, name, generation):
        self.calls.append(("process_restart", name, generation))
        return omp.env.StartedProcess(name, generation + 1, "tcp://127.0.0.1:9000")

    async def http_request(self, method, url, **options):
        self.calls.append(("http_request", method, url, options))
        return omp.env.HttpResponse(200, {}, b"ok", url)

    async def fs_stat(self, path):
        self.calls.append(("fs_stat", path))
        return omp.env.PathMeta(path, omp.env.FileKind.DIRECTORY, 0)

backend = Backend()
receipt = omp.env.EnvInfo(
    workspace_id=b"workspace", root=omp.EnvPath("file:///workspace"),
    server_epoch=b"epoch", server_version="test", server_build="build",
    schema_rev=1,
    capabilities=frozenset({omp.env.Capability.WORKTREE, omp.env.Capability.PROCESS,
                            omp.env.Capability.NET, omp.env.Capability.FS_READ}), remote=False,
)
tokens = omp.env._install_backend(backend, receipt)

async def exercise():
    worktree = await omp.env.worktree()
    assert worktree.id == "wt" and worktree.generation == 4
    process = await omp.env.Process("daemon", 7).restart()
    assert process.generation == 8
    assert process.endpoint == "tcp://127.0.0.1:9000"
    response = await omp.env.http_get("https://example.test")
    assert response.status == 200 and response.body == b"ok"
    meta = await omp.env.fs.stat(omp.EnvPath("file:///workspace"))
    assert isinstance(meta, omp.env.PathMeta)
    assert meta.kind is omp.env.FileKind.DIRECTORY

asyncio.run(exercise())
omp.env._reset_backend(tokens)
try:
    omp.env.info()
except omp.EnvUnavailable:
    pass
else:
    raise AssertionError("Environment authority survived invocation reset")
"#
				),
				None,
				None,
			)
		})
		.expect("exercise scoped DATA contract");
}
