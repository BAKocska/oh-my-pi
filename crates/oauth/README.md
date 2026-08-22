# omp-oauth

`omp-oauth` provides provider-independent, bounded OAuth protocol primitives for OMP. It covers authentication challenge discovery, authorization-server and protected-resource metadata, native authorization-code flows with PKCE, loopback callbacks, dynamic client registration, token exchange, and refresh grants.

## Structure

- `discovery` extracts validated OAuth challenge evidence from HTTP headers and bounded JSON error bodies.
- `metadata` discovers and parses authorization-server and protected-resource metadata.
- `authorization` constructs browser authorization requests and completes code exchanges after state validation.
- `pkce` generates secret PKCE material for S256 authorization flows.
- `callback` validates redirect/listener pairs and receives one authorization grant on bounded loopback HTTP listeners.
- `registration` resolves configured clients or performs dynamic client registration.
- `token` exchanges authorization codes and refresh tokens and parses token responses.
- `http` defines the transport boundary and provides a rustls-backed system client.

## Philosophy

The crate owns wire operations and authorization protocol state, not credential selection, persistence, or leases. Network inputs, redirects, and response bodies are validated and bounded at the protocol boundary, while secret-bearing values remain separate from diagnostic evidence.
