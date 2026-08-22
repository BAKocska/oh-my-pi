# omp-rpc

`omp-rpc` provides transport and protocol-negotiation plumbing for omp's gRPC services and the framed `omp rpc` embedding protocol. It supports owner-only local Unix-domain sockets, TCP connections secured with mutual TLS, Content-Length framed stdio, and a typed child-process client.

## Structure

- `client` drives the stdio embedding protocol, including host tools, generation-fenced host URI resources, typed authentication exchanges, cancellation, and fail-closed shutdown.
- `framing` bounds, encodes, and incrementally reassembles v1/v2 Content-Length frames.
- `protocol` owns typed stdio requests, events, host-resource frames, and terminal outcomes.
- `health` wraps gRPC liveness and per-service readiness reporting.
- `hello` implements the initial peer handshake and schema-revision compatibility checks.
- `tls` builds client and server TLS configuration.
- `uds` listens for and connects to Unix-domain socket transports.
- The crate-level `Error` type unifies I/O, transport, RPC, TLS, schema-negotiation, and unsupported-transport failures.

## Philosophy

Transport concerns stay separate from service behavior while local and network clients share the same protocol. Connections negotiate compatibility before exchanging application data so protobuf unknown-field behavior cannot silently discard data from a newer client. Health reporting uses the standard `grpc.health.v1` protocol rather than a project-specific alternative.
