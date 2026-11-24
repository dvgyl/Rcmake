# SPDX-License-Identifier: MIT OR Apache-2.0
project('meson-cmake-probe', 'c')
cmake = import('cmake')
local = cmake.subproject('local')
probe = executable('cmake_dep_probe', 'main.c', dependencies: local.dependency('local'))
test('consumer', probe)
