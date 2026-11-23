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
