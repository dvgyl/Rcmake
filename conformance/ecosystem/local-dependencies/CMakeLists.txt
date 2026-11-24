# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 4.5)
project(LocalDependencies LANGUAGES C)
if(USE_PKG_CONFIG)
  configure_file(local.pc.in local.pc @ONLY)
  set(ENV{PKG_CONFIG_PATH} "${CMAKE_CURRENT_BINARY_DIR}")
  find_package(PkgConfig REQUIRED)
  pkg_check_modules(PROBE REQUIRED IMPORTED_TARGET local)
  add_executable(pkg-probe pkg-probe.c)
  target_link_libraries(pkg-probe PRIVATE PkgConfig::PROBE)
else()
  include(FetchContent)
  FetchContent_Declare(fetched SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/sub")
  FetchContent_MakeAvailable(fetched)

  include(ExternalProject)
  ExternalProject_Add(external
    SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/sub"
    CMAKE_GENERATOR "${CMAKE_GENERATOR}"
    CMAKE_ARGS "-DCMAKE_INSTALL_PREFIX=<INSTALL_DIR>"
  )
endif()
