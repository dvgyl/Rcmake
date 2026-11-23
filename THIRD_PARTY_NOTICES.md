# Third-Party Notices

Rcmake's native Rust source code (`crates/*`, `tools/*`, `scripts/*`) is dual-licensed under **MIT OR Apache-2.0** (see `LICENSE-MIT` and `LICENSE-APACHE`).

Rcmake also bundles and attributes the following permissively licensed third-party assets and dependencies:

## 1. CMake 4.5.1 Standard Modules & Templates (`BSD-3-Clause`)

- **Upstream project:** CMake 4.5.1 (Kitware, Inc. and Contributors)
- **Upstream URL:** <https://github.com/Kitware/CMake/releases/tag/v4.5.1>
- **Licence:** OSI-approved BSD 3-Clause License
- **Vendored paths:** `modules/cmake-4.5/Modules/` and `modules/cmake-4.5/Templates/`
- **Full licence & contributors:**
  - `third_party/licences/CMake-BSD-3-Clause.txt`
  - `third_party/licences/CMake-CONTRIBUTORS.rst`

All original Kitware and contributor copyright notices inside `modules/cmake-4.5/` are preserved intact.

## 2. Rust Crate Dependencies

*(Updated automatically/manually whenever workspace dependencies are added; all direct and transitive crates must be compatible with `MIT OR Apache-2.0`.)*

### Compression codecs of `rcmake-archive` (ADR-010)

These crates compile third-party codec code into the `rcmake` binary. Their licence texts are in `third_party/licences/`.

| Component | Licence | Licence text |
| --- | --- | --- |
| Zstandard 1.5.7 C library (Meta Platforms, Inc. and affiliates), through `zstd-sys` | `BSD-3-Clause` (BSD option of the BSD/GPLv2 dual licence) | `third_party/licences/zstd-BSD-3-Clause.txt` |
| `zstd` and `zstd-safe` Rust bindings (Alexandre Bury) | `BSD-3-Clause` | `third_party/licences/zstd-rs-BSD-3-Clause.txt` |
| XZ Utils `liblzma` (Lasse Collin and contributors), through `liblzma-sys` | `0BSD` | `third_party/licences/xz-liblzma-0BSD.txt` |
| `liblzma` and `liblzma-sys` Rust bindings | `MIT OR Apache-2.0` | Same terms as Rcmake (`LICENSE-MIT`, `LICENSE-APACHE`) |
| `libbz2-rs-sys` (Rust port of bzip2/libbzip2 by Julian R Seward; Trifecta Tech Foundation and contributors) | `bzip2-1.0.6` | `third_party/licences/libbz2-rs-sys-bzip2-1.0.6.txt` |
| `bzip2` Rust bindings | `MIT OR Apache-2.0` | Same terms as Rcmake (`LICENSE-MIT`, `LICENSE-APACHE`) |
