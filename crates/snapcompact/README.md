# omp-snapcompact

`omp-snapcompact` is OMP's pure-Rust bitmap archive renderer for context compaction. It rasterizes pre-normalized conversation text with bundled bitmap and TrueType fonts, then encodes compact PNG frames for model requests.

The crate owns bounded text-to-image rendering, sentence-colored and monochrome ink variants, repeated-line redundancy, configurable cell geometry, one- or two-column layouts, and dimmed tool-output spans. Its `archive` module adds provider-aware frame shapes, atomic data-URL elision, bounded text chunking, framing, and savings accounting. Rendering is deterministic and allocation-bounded, while text normalization, wrapping, and provider request construction remain caller responsibilities.
