# omp-ar

`omp-ar` provides bounded, lazy reads for ZIP, TAR (plain and gzip/bzip2/xz/zstd/`.Z`/LZMA-compressed), Electron ASAR, RAR 4/5, 7z, ISO 9660, Microsoft CAB, cpio, RPM, Unix ar, Debian package, LZH/LHA, and ARJ archives, plus single-stream compressed files and deterministic ZIP and TAR-family writes. It sniffs in-memory inputs, infers formats from common archive extensions, and indexes seekable sources before reading member payloads on demand.

## Formats

- ZIP reading supports stored and DEFLATE members, CRC-32 verification, ZIP64 metadata, legacy CP437 and Info-ZIP Unicode names, extended timestamps, prepended archives, and capability-scoped extraction. ZIP writing emits ordinary deterministic archives and reports inputs that require ZIP64.
- TAR reading supports V7, USTAR, GNU long names and links, PAX path/link/size records, hard links, safe symbolic-link aliases, and bounded old-GNU sparse expansion. PAX sparse members remain listable but reject payload reads because tar 0.4.46 does not expand them. Compressed tars decode whole-stream under limits; a compressed stream that is not a tar surfaces as a single stem-named member.
- Electron ASAR reading validates the Chromium Pickle and JSON index, keeps packed members seek-lazy, resolves safe in-archive links, and reads `unpacked` members from the adjacent `.asar.unpacked` tree. ASAR writing is not supported.
- RAR reading covers RAR5 (methods 0-5 with delta/x86/ARM filters, solid chains, symlinks) and RAR4 (stored plus RAR 2.9 LZ with standard filters); PPMd, RarVM programs, encryption, multi-volume sets, and recovery records fail precisely.
- 7z reading covers Copy/LZMA/LZMA2/Delta/BCJ-x86 folders (including encoded headers and solid folders); BCJ2, PPMd, AES, and other coders fail precisely. XZ decoding covers multi-stream/multi-block files, all standard checks, and every standard BCJ filter.
- ISO 9660 reading prefers Joliet names, merges Rock Ridge NM/PX/SL metadata, and handles multi-extent and interleaved files; High Sierra and UDF-only images fail precisely.
- CAB (None/MSZIP/LZX; Quantum fails precisely), cpio (newc/crc/odc/old-binary), RPM payloads, Unix ar (BSD/GNU/COFF naming), Debian packages (control/data trees), LZH (lh0/lh4-lh7/lzs/lz4), and ARJ (methods 0-4) are read-only with checksum verification where the container records them.
- TAR and TAR.GZ writing emits deterministic USTAR/GNU file, directory, hard-link, and symbolic-link records. Gzip output fixes the header modification time at zero.

## Safety

Archive paths are normalized once, unsafe paths never enter the index, and limits bound decoded archives, indexes, members, materialized output, path bytes, path depth, entries, and link rewrites. TAR.GZ input is bounded while decompressing; ZIP, plain TAR, and packed ASAR members stay seek-lazy.

## Example

```rust
use omp_ar::{Archive, tar, zip};

let members = [("hello.txt", b"hello".as_slice())];
for bytes in [zip::encode(members)?, tar::encode(members)?, tar::encode_gzip(members)?] {
	let mut archive = Archive::from_bytes(&bytes)?;
	assert_eq!(archive.read("hello.txt")?, b"hello");
}
# Ok::<(), omp_ar::Error>(())
```
