# omp-sandbox

`omp-sandbox` reserves the crate boundary for OMP's future process-confinement integration. OS-specific isolation primitives belong here, beside the environment daemon that spawns processes, rather than in `omp-env` or the agent loop.

Sandbox enforcement is explicitly deferred for v1. This crate currently exposes no confinement API and does not confine extensions, shell builtins, or child processes. Extensions are not a security boundary. Future enforcement must be built on the planned VM-grade vibevmm and isobox architecture before this crate can make an isolation claim.
