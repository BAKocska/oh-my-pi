# omp-executor

`omp-executor` owns scheduling for OMP's UI and orchestration core. It provides a small production thread pool, a seeded single-thread deterministic scheduler with virtual time, and executor-neutral task, timer, interval, timeout, and blocking-work APIs.

`Task` and `BridgeTask` cancel when dropped. Explicit fire-and-forget work must call `detach`.

`TokioBridge` is the single embedded Tokio runtime for edge libraries that require Tokio's reactor. Futures created by Tokio-bound libraries must be spawned on the bridge and only the returned `BridgeTask` may be awaited from core code. Unix `Signals` exposes process signals as an async stream through a self-pipe.
