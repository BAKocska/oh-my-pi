# omp-sdk

`omp-sdk` is the stable native embedding boundary for constructing OMP sessions. It exposes owned configuration, deterministic prompt and context patch callbacks, credential leases, bounded discovery, workspace formatting, model selection, native tool registration, and Python eval contracts.

The facade deliberately keeps application composition, provider wire codecs, credential stores, UI state, and process handles private. Callback output is lowered into typed authority-owned operations before it can affect a turn.
