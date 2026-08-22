# omp-ext

Extension configuration, dependency resolution, lockfiles, index metadata, and
local trust state for OMP.

## Structural philosophy

The crate is a pure domain: deterministic data transformations over declared
configuration and durable on-disk state. It owns no process, no connection, and
no CLI. Everything that needs a running Environment — Git materialization,
site publication — lives in the host or the CLI driver above it, so this crate
sits below both and can be reasoned about as data in, data out.

- `config`: declared extension configuration, overlays, scopes, and CLI
  contributions.
- `lock`: reproducible lockfiles and local installed/enabled records.
- `resolver`: the `uv` resolution driver and R1-R12 policy checks.
- `trust`: signature verification and trust tiers.
- `index`, `upgrade`, `doctor`: index metadata, generation commits, and
  integrity diagnostics.
