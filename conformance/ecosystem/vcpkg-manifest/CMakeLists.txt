# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 4.5)
list(APPEND VCPKG_INSTALL_OPTIONS
  "--x-buildtrees-root=${CMAKE_BINARY_DIR}/vcpkg-buildtrees"
  "--x-packages-root=${CMAKE_BINARY_DIR}/vcpkg-packages"
)
project(VcpkgProbe LANGUAGES CXX)
find_package(fmt CONFIG REQUIRED)
find_package(ZLIB REQUIRED)
find_package(spdlog CONFIG REQUIRED)
find_package(nlohmann_json CONFIG REQUIRED)
find_package(Boost CONFIG REQUIRED)
add_executable(vcpkg-probe main.cpp)
target_compile_features(vcpkg-probe PRIVATE cxx_std_17)
target_link_libraries(vcpkg-probe PRIVATE
  fmt::fmt ZLIB::ZLIB spdlog::spdlog nlohmann_json::nlohmann_json Boost::headers
)
enable_testing()
add_test(NAME consumer COMMAND vcpkg-probe)
