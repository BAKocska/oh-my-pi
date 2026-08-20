## What the pi original did

`@sreetej510/pi-hpc-tools` exposed `ls_hpc`, `read_file_hpc`, and `grep_hpc` by launching `plink.exe` for every operation. It kept cluster credentials in plaintext configuration and put the password in the child process arguments. Remote output crossed into the extension host before local truncation, so even bytes that were immediately discarded still made the trip.

## The omp shape

The three devices declare `place = "worker:hpc"`, so their bodies execute in the persistent attached worker described by the manifest; the supervisor owns the tunnel and the Python module contains no SSH client, shell, credential, timeout, or connection lifecycle code. `remote_grep` invokes `rg` with an argv list and parses its JSON stream inside that body, returning only structured matches, while `remote_ls` returns metadata and `remote_read` returns only the requested bytes. Results over the frozen one-megabyte worker threshold are wrapped as `omp.Spill(value: bytes)`, following the placement worked port in `docs/py/04-placement.md` §1 and the DATA-side account in `docs/py/11-env.md` §4.

The attached site is deliberately `unmanaged = true`: it is a bare trusted machine with no omp Environment or docserver authority. Trust is granted by the operator rather than claimed in `omp.toml`. The supervisor supplies the worker authentication key as the first stdin frame before protocol traffic, never through argv or the environment, per the resolved first-stdin-frame ruling in `docs/py/04-placement.md`.

## Gaps

- `omp.Spill` deliberately raises `omp.BoundaryError` on unmanaged workers, matching `docs/py/04-placement.md` §Leaf topology. Oversized results from this attached bare worker therefore fail at the boundary rather than becoming blob references.
