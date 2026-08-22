use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn async_data_handles_preserve_streaming_and_close_semantics() {
	let engine = Engine::builder().init().expect("boot embedded Python");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import omp
import omp.env

class Stream:
    def __init__(self, values):
        self.values = iter(values)
        self.closed = False
    def __iter__(self):
        return self
    def __next__(self):
        return next(self.values)
    def close(self):
        self.closed = True

class Upload:
    pass

class Backend:
    def __init__(self):
        self.calls = []
        self.streams = []
        self.upload = Upload()
        self.chunks = []
        self.aborted = False

    def stream(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.env.blobs.stream":
            stream = Stream([b"a", b"b"])
        elif operation == "omp.env.docs.Doc.events":
            stream = Stream([{
                "sequence": 4, "kind": "committed",
                "revision": {"sequence": 2, "content_hash": b"new"},
                "previous_revision": {"sequence": 1, "content_hash": b"old"},
                "txn_id": b"txn", "invalidated_txn_ids": (), "previous_path": None,
            }])
        else:
            raise AssertionError(operation)
        self.streams.append(stream)
        return stream

    def blob_writer(self):
        return self.upload

    def abort_blob(self, upload):
        assert upload is self.upload
        self.aborted = True

    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.env.BlobWriter.write":
            assert arguments["upload"] is self.upload
            self.chunks.append(arguments["chunk"])
            return None
        if operation == "omp.env.BlobWriter.commit":
            return omp.BlobRef(bytes.fromhex("00" * 32), sum(map(len, self.chunks)))
        raise AssertionError(operation)

backend = Backend()
receipt = omp.env.EnvInfo(
    workspace_id=b"workspace", root=omp.EnvPath("file:///workspace"),
    server_epoch=b"epoch", server_version="test", server_build="build",
    schema_rev=1,
    capabilities=frozenset({omp.env.Capability.BLOB, omp.env.Capability.DOC_READ}),
    remote=False,
)
tokens = omp.env._install_backend(backend, receipt)

async def chunks():
    yield b"one"
    yield b"two"

async def exercise():
    reference = omp.BlobRef(bytes.fromhex("11" * 32), 2)
    stream = omp.env.blobs.stream(reference)
    assert await anext(stream) == b"a"
    await stream.aclose()
    assert backend.streams[-1].closed

    stored = await omp.env.blobs.put(chunks())
    assert stored.size == 6 and backend.chunks == [b"one", b"two"]

    doc = omp.env.Doc(b"lease", omp.EnvPath("file:///workspace/a.py"))
    event = await anext(doc.events())
    assert event.kind is omp.env.DocEventKind.COMMITTED
    assert event.revision.sequence == 2 and event.previous_revision.sequence == 1

asyncio.run(exercise())
omp.env._reset_backend(tokens)
"#
				),
				None,
				None,
			)
		})
		.expect("exercise async DATA handles");
}
