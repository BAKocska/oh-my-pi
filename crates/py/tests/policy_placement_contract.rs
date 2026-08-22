//! Focused CONTROL-contract proof for policy, placement, and prompt
//! invalidation.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn policy_placement_control_contract() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import json
import os
import socket
import tempfile
import threading

import omp
import omp_remote


class Backend:
    def __init__(self, worker_address):
        self.calls = []
        self.tier_calls = []
        self.worker_address = worker_address

    def tier_of(self, target):
        self.tier_calls.append(dict(target))
        identities = {
            (("kind", "core"), ("name", "read"), ("rev", "1")): "read",
            (
                ("family", "lint"),
                ("kind", "device"),
                ("name", "check"),
                ("rev", "2"),
            ): "write",
            (("kind", "mcp"), ("server", "github"), ("tool", "create")): "privileged",
        }
        return identities.get(tuple(sorted(target.items())))

    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.policy.parse":
            return {
                "source": arguments["script"],
                "rev": "bashir@3",
                "parser_rev": "tree-sitter-bash@test",
                "parse_ok": True,
                "parse_error": None,
                "truncated": False,
                "node_count": 0,
                "is_compound": False,
                "has_dynamic_eval": False,
                "lists": [],
                "commands": [],
                "functions": [],
                "reads": [],
                "writes": [],
                "net": [],
                "opaque": [],
            }
        if operation == "omp.policy.match_paths":
            return [{
                "lexical": arguments["path"],
                "resolved": "/workspace/file.txt",
                "absolute": "/workspace/file.txt",
                "access": arguments["access"],
                "origin": "argv",
                "command_index": 0,
                "outside_workspace": False,
                "exists": True,
                "dynamic": False,
                "span": {"start": 0, "end": 8, "line": 1, "column": 1},
            }]
        if operation == "omp.policy.capabilities":
            return {
                "backends": ["seatbelt"],
                "landlock_abi": None,
                "filesystem": True,
                "network": False,
                "domain_filtering": False,
                "resource_limits": True,
                "degraded": ["network unavailable"],
            }
        if operation in ("omp.policy.effective_profile", "omp.policy.install"):
            profile = {
                "mode": "enforce",
                "label": "extension",
            }
            if operation == "omp.policy.install":
                return {"handle_id": "profile-7", "profile": profile}
            return profile
        if operation == "omp.policy.enforcement":
            return {
                "filesystem": "hard",
                "network": "none",
                "process": "partial",
                "backend": "seatbelt",
                "degraded_reasons": ["network unavailable"],
            }
        if operation in ("omp.policy.revoke", "omp.policy.amend", "omp.policy.decide"):
            return None
        if operation == "omp.policy.pending":
            return [{
                "ticket_id": "ticket-1",
                "invocation_id": "call-1",
                "reasons": [{
                    "title": "Run command",
                    "body": "Review execution",
                    "subject": "echo ok",
                }],
                "state": "pending",
                "decision": None,
                "created_at": 1.5,
            }]
        if operation == "omp.prompts.invalidate":
            return 4
        if operation.startswith("omp.workers."):
            action = operation.rsplit(".", 1)[1]
            if action == "session":
                return {
                    "generation": arguments["generation"],
                    "family": "unix",
                    "address": self.worker_address,
                    "authkey_base64": None,
                }
            if action == "list":
                return [self.worker_info(1)]
            if action in ("get", "info"):
                return self.worker_info(arguments.get("generation", 1))
            if action == "restart":
                return self.worker_info(2)
            if action == "warm":
                return "ready"
            if action == "evict":
                return True
            if action == "stop":
                return None
        raise AssertionError(f"unexpected operation {operation}")

    @staticmethod
    def worker_info(generation):
        return {
            "name": "index",
            "generation": generation,
            "state": "ready",
            "site": {"kind": "env", "process": None, "ready": None},
            "pid": 17,
            "spawned_at_ms": 100,
            "last_call_at_ms": None,
            "calls": 2,
            "in_flight": 0,
            "code_cached": 1,
            "enforced": ["memory_bytes"],
            "fault": None,
        }


async def exercise(backend):
    assert omp.tier_of(omp.CoreTool("read", "1", {})) is omp.Tier.READ
    assert omp.tier_of(
        omp.DeviceCall("check", "lint", "2", {})
    ) is omp.Tier.WRITE
    assert omp.tier_of(
        omp.McpCall("github", "create", {})
    ) is omp.Tier.PRIVILEGED

    ir = await omp.policy.parse("echo ok")
    assert isinstance(ir, omp.BashIR)
    assert ir.rev == omp.BASH_IR_REV and ir.parse_ok

    paths = await omp.policy.match_paths(
        "file.txt", "*.txt", access=omp.Access.READ
    )
    assert isinstance(paths, tuple) and isinstance(paths[0], omp.PathRef)
    assert paths[0].access is omp.Access.READ

    capabilities = await omp.policy.capabilities()
    assert isinstance(capabilities, omp.SandboxCapabilities)
    assert capabilities.backends == (omp.SandboxBackend.SEATBELT,)

    profile = await omp.policy.effective_profile()
    assert isinstance(profile, omp.SandboxProfile)
    assert profile.label == "extension"
    receipt = await omp.policy.enforcement()
    assert receipt.filesystem is omp.FilesystemGrade.HARD

    handle = await omp.policy.install(
        omp.SandboxProfile(label="requested"), scope=omp.PolicyScope.SESSION
    )
    assert handle.profile.label == "extension"
    await handle.revoke()
    approval = omp.ApprovalSpec("Widen", "Review", "/outside")
    await omp.policy.amend(
        omp.SandboxProfile(label="amendment"),
        scope=omp.PolicyScope.CALL,
        reason="retry denied access",
        approval=approval,
    )

    tickets = await omp.policy.pending()
    assert isinstance(tickets, tuple) and isinstance(tickets[0], omp.ApprovalTicket)
    assert isinstance(tickets[0].reasons[0], omp.ApprovalSpec)
    decision = omp.ApprovalDecision(
        False,
        omp.PolicyScope.ONCE,
        omp.ApprovalSource.EXTERNAL,
        "reviewer",
        "denied",
        False,
    )
    await omp.policy.decide("ticket-1", decision)

    assert await omp.prompts.invalidate("memory") == 4
    try:
        await omp.prompts.invalidate("runtime")
    except omp.SlotClassConflict:
        pass
    else:
        raise AssertionError("frozen prompt invalidation must fail locally")

    worker = await omp.workers.get("index")
    assert worker.generation == 1
    assert (await worker.info()).generation == 1
    assert await worker.state() is omp.WorkerState.READY
    await worker.warm()

    def add(left, right):
        return left + right

    assert await worker.call(add, 2, 3) == 5
    assert [info.name for info in await omp.workers.list()] == ["index"]
    assert await omp.workers.evict("index") is True
    restarted = await omp.workers.restart("index")
    assert restarted.generation == 2
    await worker.stop()


with tempfile.TemporaryDirectory() as directory:
    address = os.path.join(directory, "worker.sock")
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(address)
    listener.listen(1)

    def serve_one():
        connection, _ = listener.accept()
        try:
            omp_remote.serve(connection)
        finally:
            connection.close()
            listener.close()

    server = threading.Thread(target=serve_one, daemon=True)
    server.start()
    backend = Backend(address)
    token = omp._control_backend.set(backend)
    try:
        asyncio.run(exercise(backend))
    finally:
        omp._control_backend.reset(token)
    server.join(timeout=2)
    assert not server.is_alive()

operations = [operation for operation, _ in backend.calls]
for _, arguments in backend.calls:
    json.dumps(arguments)
for required in (
    "omp.policy.parse",
    "omp.policy.match_paths",
    "omp.policy.capabilities",
    "omp.policy.effective_profile",
    "omp.policy.enforcement",
    "omp.policy.install",
    "omp.policy.revoke",
    "omp.policy.amend",
    "omp.policy.pending",
    "omp.policy.decide",
    "omp.prompts.invalidate",
    "omp.workers.get",
    "omp.workers.info",
    "omp.workers.warm",
    "omp.workers.session",
    "omp.workers.list",
    "omp.workers.evict",
    "omp.workers.restart",
    "omp.workers.stop",
):
    assert required in operations

install_arguments = next(
    arguments for operation, arguments in backend.calls
    if operation == "omp.policy.install"
)
assert install_arguments["scope"] == "session"
assert install_arguments["profile"]["mode"] == "enforce"
assert isinstance(install_arguments["profile"], dict)
assert next(
    arguments for operation, arguments in backend.calls
    if operation == "omp.workers.stop"
)["grace"] == 5.0

assert backend.tier_calls == [
    {"kind": "core", "name": "read", "rev": "1"},
    {
        "kind": "device",
        "name": "check",
        "family": "lint",
        "rev": "2",
    },
    {"kind": "mcp", "server": "github", "tool": "create"},
]
try:
    omp.tier_of(omp.CoreTool("read", "1", {}))
except omp.PolicyError:
    pass
else:
    raise AssertionError("tier lookup without authority must fail closed")
"#
				),
				None,
				None,
			)
		})
		.expect("policy and placement CONTROL contract");
}
