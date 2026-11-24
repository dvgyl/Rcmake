# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 4.5)
project(RecursiveChild VERSION 1.0.0 LANGUAGES NONE)
enable_testing()
add_test(NAME child COMMAND cmake -E echo "Child test passed")
install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/CMakeLists.txt" DESTINATION share/recursive-child)
set(CPACK_GENERATOR TGZ)
include(CPack)
