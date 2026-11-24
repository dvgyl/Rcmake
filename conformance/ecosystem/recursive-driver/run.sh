#!/bin/sh
# SPDX-License-Identifier: MIT OR Apache-2.0
set -eux
test -z "${RCMAKE_ORACLE_CMAKE+x}"
for tool in cmake ctest cpack; do
    test "$(command -v "$tool")" = "${PATH%%:*}/$tool"
done
cmake -S "$1" -B "$2" -G Ninja
cmake --build "$2" --parallel 2
ctest --test-dir "$2" --no-tests=error --output-on-failure
cmake --install "$2" --prefix "$2/stage"
cpack --config "$2/CPackConfig.cmake" -B "$2/packages"
test -f "$2/stage/share/recursive-child/CMakeLists.txt"
test -f "$2/packages/"*.tar.gz
