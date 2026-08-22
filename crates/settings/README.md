# omp-settings

`omp-settings` defines OMP's typed settings-domain reflection contract and immutable, revisioned snapshots. Runtime-owning crates describe their Rust settings type once through `SettingsDomain` and submit a linker-time registration; the application authority owns layering and persistence.

The crate deliberately contains no filesystem or process-global writer. Persistence, locking, migration, and production composition stay in `omp-app`.
