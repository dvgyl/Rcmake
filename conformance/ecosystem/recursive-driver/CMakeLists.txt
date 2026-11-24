# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 4.5)
project(RecursiveDriver VERSION 1.0.0 LANGUAGES C)
include(CMakePackageConfigHelpers)

configure_file(version.h.in version.h @ONLY)
configure_file(recursive-driver.pc.in recursive-driver.pc @ONLY)
add_executable(recursive-demo main.c)
target_include_directories(recursive-demo PRIVATE "${CMAKE_CURRENT_BINARY_DIR}")
install(TARGETS recursive-demo EXPORT RecursiveDriverTargets DESTINATION bin)
install(EXPORT RecursiveDriverTargets DESTINATION lib/cmake/RecursiveDriver)
configure_file(RecursiveDriverConfig.cmake.in RecursiveDriverConfig.cmake @ONLY)
write_basic_package_version_file(
  RecursiveDriverConfigVersion.cmake COMPATIBILITY SameMajorVersion
)
install(FILES
  "${CMAKE_CURRENT_BINARY_DIR}/RecursiveDriverConfig.cmake"
  "${CMAKE_CURRENT_BINARY_DIR}/RecursiveDriverConfigVersion.cmake"
  DESTINATION lib/cmake/RecursiveDriver
)
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/recursive-driver.pc" DESTINATION lib/pkgconfig)

enable_testing()
add_test(NAME executable COMMAND recursive-demo)
add_test(NAME recursive-tools
  COMMAND sh "${CMAKE_CURRENT_SOURCE_DIR}/run.sh"
  "${CMAKE_CURRENT_SOURCE_DIR}/sub" "${CMAKE_CURRENT_BINARY_DIR}/nested"
)
set(CPACK_GENERATOR TGZ)
include(CPack)
