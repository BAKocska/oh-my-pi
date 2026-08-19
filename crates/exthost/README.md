# omp-exthost

`omp-exthost` is the crate boundary for OMP's extension-host process. The host will embed the extension interpreter, own its toolhost connection and handler state, and remain separate from the model-facing eval kernel.

The architecture uses one child per extension, keyed by `(layer, tier, extension)`, so extensions do not share fate by default. Callback entry is actor-serialized unless an extension explicitly opts into concurrency; frame multiplexing does not imply concurrent callback execution. This skeleton defines only the structural boundary and starts no interpreter, process, transport, or runtime service.
