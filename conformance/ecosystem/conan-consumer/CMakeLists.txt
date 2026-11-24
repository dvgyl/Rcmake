# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 4.5)
project(ConanProbe LANGUAGES CXX)
find_package(ZLIB CONFIG REQUIRED)
add_executable(conan-probe main.cpp)
target_link_libraries(conan-probe PRIVATE ZLIB::ZLIB)
enable_testing()
add_test(NAME consumer COMMAND conan-probe)
