# omp-memory

`omp-memory` owns OMP's default-off Mnemopi backend: canonical project bank identity, durable SQLite memory rows, derived recall indexes, four-voice recall, retention cursors, and isolated embedding-worker protocol.

Durable rows and retention cursors are authoritative. Vector, graph, FTS, and recall-cache state are generation-fenced projections that can be rebuilt without changing memory identity. Git discovery remains an Environment responsibility; callers pass the canonical primary repository root into bank construction.
