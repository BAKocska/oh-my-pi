# omp-env

`omp-env` is the typed client boundary for OMP's `env/v1` protocol. It
correlates invocation, command, session, named-process, worker, and blob
requests over bidirectional frame transports and exposes server events as
asynchronous request streams.

`omp-env` does not expose or implement the environment host. The live daemon,
tool executor, document and process authorities, Python extension host, and
worker supervision are in `omp-envd`.

## Structure

- `EnvClient` is the general environment-protocol client. It supports decoded
  in-process transports as well as connected transports without changing the
  request API.
- `Invocation`, `RunGuard`, and the streaming handles retain request identity,
  cancellation, and server-event correlation.
- `ExtensionEnvClient` and `WorkerEnvClient` are capability-reduced DATA
  clients for host-managed children; they are not host implementations.
- `project_state` derives project-keyed state and transport paths shared by
  clients that need to find the owning daemon.
- `frame`, `document_frame`, and `blob_frame` re-export the generated wire
  contracts used at transport boundaries.

## Philosophy

The crate deliberately owns no world resources. Files, processes, document
leases, workspace search, and blob storage remain behind the environment
service. In-process and remote deployments feed the same frame client;
per-invocation and per-command `RunGuard`s provide nonblocking,
request-scoped cancellation without ending server-owned sessions. Detached
work must relinquish its guard explicitly.

Extension hosts connect with `ExtensionEnvClient::connect_uds`. Construction
completes the `ClientHello` handshake and consumes a `DataScope`; the resulting
handle stamps immutable invocation identity, effect token, host/session
generations, and PTY restriction on every DATA, document, filesystem, LSP,
exec, named-process, blob, search, HTTP, and cancellation frame. Its API
deliberately cannot emit another hello, invoke a tool, answer admission, shut
down the owner, or retire the server. Remote `ProtocolError` values remain
typed as `ClientError::Protocol` (or `EffectsNotAuthorized` for an uncommitted
effect), and streaming handles retain explicit cancellation.

## Development

Use `just check-pkg omp-env` and `just test-pkg omp-env`. Use `just e2e` or an
exact narrower E2E recipe from `just --list` only for joined client/host
behavior.
