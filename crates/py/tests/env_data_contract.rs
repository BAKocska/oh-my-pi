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

    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.env.worktree":
            return {"id": "wt", "root": "file:///workspace/wt", "base": "base", "generation": 4}
        if operation == "omp.env.Process.restart":
            return {"name": arguments["name"], "generation": arguments["generation"] + 1,
                    "endpoint": "tcp://127.0.0.1:9000"}
        if operation == "omp.env.http.get":
            return {"status": 200, "headers": {}, "body": b"ok", "final_url": arguments["url"]}
        raise AssertionError(operation)

    def stream(self, operation, arguments):
        self.calls.append((operation, arguments))
        return ()

backend = Backend()
receipt = omp.env.EnvInfo(
    workspace_id=b"workspace", root=omp.EnvPath("file:///workspace"),
    server_epoch=b"epoch", server_version="test", server_build="build",
    schema_rev=1,
    capabilities=frozenset({omp.env.Capability.WORKTREE, omp.env.Capability.PROCESS,
                            omp.env.Capability.NET}), remote=False,
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
