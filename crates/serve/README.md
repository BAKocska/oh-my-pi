# omp-serve

`omp-serve` projects OMP's canonical inference, authentication, and content-addressed blob services onto the generated Tonic gRPC interfaces. It translates protobuf requests and streaming frames into typed domain operations, then maps domain results, events, and failures back to stable wire responses.

## Structure

- `inference` exposes catalog discovery and typed inference operations, including streaming turns, media generation, realtime sessions, usage, and retained conversation context.
- `auth` serves interactive credential login flows, account state, usage reporting, and credential health operations.
- `blob` provides bounded streaming upload and download, metadata lookup, and deletion over the daemon-owned content-addressed blob store.
- `InferenceRpc`, `AuthRpc`, and `BlobRpc` are the public server projections registered by the daemon.

## Philosophy

This crate is a transport boundary, not a second implementation of OMP's service semantics. Canonical types and behavior remain in the inference, catalog, tool, and storage crates; `omp-serve` is responsible for validation, protocol conversion, stream lifecycle, and gRPC status mapping. Long-lived state is injected from daemon-owned registries and stores so RPC handlers project shared state rather than creating competing authorities.
