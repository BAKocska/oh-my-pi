//! Focused embedded proof for MCP, parameter-cursor, and URL CONTROL contracts.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn control_auxiliary_contract() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio

import omp


class AuxiliaryBackend:
    def __init__(self):
        self.calls = []

    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.mcp.mount":
            return {
                "catalog_epoch": 9,
                "devices": [{
                    "name": "github_search",
                    "family": "mcp",
                    "rev": 4,
                    "server": "github",
                    "definition": {
                        "name": "search",
                        "description": "Search GitHub",
                        "inputSchema": {"type": "object"},
                    },
                    "documentation": "GitHub server",
                }],
            }
        if operation == "omp.mcp.invoke":
            return {
                "content": [{"type": "text", "text": "found"}],
                "structured_content": {"count": 1},
                "meta": None,
                "is_error": False,
                "truncated": False,
                "dispatch_certainty": 2,
                "retry_count": 0,
                "auth_retried": False,
                "effects_unknown": False,
            }
        if operation == "omp.mcp.unmount":
            return {"removed": True}
        if operation == "omp.mcp.servers":
            return {"definition_epoch": 9, "servers": [{
                "name": "github",
                "state": 3,
                "protocol_version": "2025-11-25",
                "instructions": "GitHub server",
                "endpoints": ["search"],
                "resources": [{
                    "uri": "repo://current",
                    "name": "current",
                    "media_type": "application/json",
                    "template": False,
                }],
                "prompts": ["review"],
                "last_error": None,
            }]}
        if operation == "omp.urls.read":
            return "authoritative bytes"
        if operation == "omp.params.args":
            return {"value": {"query": "needle"}, "phase": "ARGS_FINALIZED"}
        if operation == "omp.params.raw":
            return "{'query':'needle'}"
        if operation == "omp.params.committed":
            if arguments["invocation_id"] == "aborted":
                return {"aborted": "assistant item disappeared"}
            return {
                "value": '{"query":"needle"}',
                "phase": "EFFECTS_AUTHORIZED",
            }
        if operation == "omp.params.pull":
            if arguments["invocation_id"] == "interrupted":
                return {"interrupt": {"kind": "steer", "reason": "new request"}}
            return {"value": "needle", "repairs": []}
        if operation == "omp.params.next_interrupt":
            return {"closed": True}
        raise AssertionError(f"unexpected operation: {operation}")


async def exercise():
    backend = AuxiliaryBackend()
    omp._install_control_backend(backend)

    mounted = await omp.mcp.mount(omp.mcp.McpMount(
        server="github",
        transport=omp.mcp.Http("https://example.test/mcp"),
        precedence=omp.Precedence.ENHANCEMENT,
    ))
    assert len(mounted) == 1
    assert mounted[0].precedence == int(omp.Precedence.ENHANCEMENT)
    invocation = await mounted[0](query="needle")
    assert invocation["structured_content"] == {"count": 1}
    assert ("omp.mcp.invoke", {
        "server": "github",
        "tool": "search",
        "arguments": {"query": "needle"},
    }) in backend.calls

    inventory = await omp.mcp.servers()
    assert inventory[0].state is omp.mcp.McpServerState.CONNECTED
    assert inventory[0].protocol_version == "2025-11-25"
    assert inventory[0].resources == (
        omp.mcp.McpResource(
            "repo://current", "current", "application/json", False
        ),
    )
    assert inventory[0].prompts == ("review",)
    await omp.mcp.unmount("github")

    @omp.params
    class SearchArgs:
        query: str

    cursor = omp.IncomingParams(
        name="search",
        rev=omp.Rev("mcp", 4),
        invocation_id="call-1",
        shape=SearchArgs,
    )
    decoded = await cursor.args()
    assert decoded == SearchArgs("needle")
    assert await cursor.raw() == "{'query':'needle'}"
    assert await cursor.committed() == '{"query":"needle"}'
    assert cursor.is_authorized
    params_calls = [call for call in backend.calls if call[0].startswith("omp.params.")]
    assert params_calls[0] == (
        "omp.params.args",
        {
            "invocation_id": "call-1",
            "interruptible": False,
            "expected": "SearchArgs",
        },
    )

    pull_cursor = omp.IncomingParams(
        name="search", rev=omp.Rev("mcp", 4), invocation_id="pull-1"
    )
    assert await pull_cursor.arg("query", alias=("q",), coerce=omp.Coerce.STRING) == "needle"
    pull_call = backend.calls[-1]
    assert pull_call[0] == "omp.params.pull"
    assert pull_call[1]["path"] == ["query"]
    assert pull_call[1]["aliases"] == ["q"]
    assert pull_call[1]["coercions"] == ["string"]

    aborted = omp.IncomingParams(
        name="search", rev=omp.Rev("mcp", 4), invocation_id="aborted"
    )
    try:
        await aborted.committed()
    except omp.CommitAborted as error:
        assert "disappeared" in str(error)
    else:
        raise AssertionError("commit abort was not preserved")

    interrupted = omp.IncomingParams(
        name="search", rev=omp.Rev("mcp", 4), invocation_id="interrupted"
    )
    try:
        await interrupted.interruptable().arg("query")
    except omp.Interrupted as error:
        assert error.interrupt.reason == "new request"
    else:
        raise AssertionError("interrupt was not preserved")

    closed = omp.IncomingParams(
        name="search", rev=omp.Rev("mcp", 4), invocation_id="closed"
    )
    try:
        await closed.next_interrupt()
    except omp.InterruptClosed:
        pass
    else:
        raise AssertionError("closed interrupt stream was not preserved")

    urls = omp.urls
    old_snapshot = (
        urls._scheme_source,
        urls._scheme_hash,
        urls._scheme_cache,
    )
    try:
        urls._bind_scheme_source(lambda: (
            b"auxiliary-contract",
            ((
                urls.Scheme.FILE,
                urls.SchemeInfo(True, False, True, "files"),
            ),),
        ))
        assert await urls.read("notes.txt", "1-2") == "authoritative bytes"
        assert backend.calls[-1] == (
            "omp.urls.read", {"url": "notes.txt:1-2"}
        )
    finally:
        urls._scheme_source, urls._scheme_hash, urls._scheme_cache = old_snapshot


asyncio.run(exercise())
"#
				),
				None,
				None,
			)
		})
		.expect("auxiliary CONTROL contract holds");
}
