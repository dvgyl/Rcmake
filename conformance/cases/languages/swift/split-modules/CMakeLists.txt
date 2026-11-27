cmake_minimum_required(VERSION 4.5)
project(SwiftSplitModulesFixture LANGUAGES Swift)

# Generated Swift source to prove order-only dependencies on both module and object edges.
set(GEN_SWIFT "${CMAKE_CURRENT_BINARY_DIR}/gen.swift")
add_custom_command(
  OUTPUT "${GEN_SWIFT}"
  COMMAND "${CMAKE_COMMAND}" -E copy "${CMAKE_CURRENT_SOURCE_DIR}/gen.swift.in" "${GEN_SWIFT}"
  DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/gen.swift.in"
  COMMENT "Generating gen.swift"
)

# libA: incremental mode, separate module emission (CMP0215 NEW), custom module dir,
# equal-basename sources a/dup.swift + b/dup.swift, Swift 6, custom package name,
# explicit -emit-objc-header pass-through.
add_library(libA STATIC a/dup.swift b/dup.swift "${GEN_SWIFT}")
set_target_properties(libA PROPERTIES
  Swift_COMPILATION_MODE incremental
  Swift_MODULE_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}/custom_mods"
  Swift_LANGUAGE_VERSION 6
  Swift_PACKAGE_NAME CustomPkg
)
target_compile_options(libA PRIVATE
  -emit-objc-header
  -emit-objc-header-path
  "${CMAKE_CURRENT_BINARY_DIR}/libA-Swift.h"
)

# libB: singlefile mode, depends on libA (Audit #11 Edges #1, #2, #3).
add_library(libB STATIC b.swift)
target_link_libraries(libB PUBLIC libA)
set_target_properties(libB PROPERTIES
  Swift_COMPILATION_MODE singlefile
)

# libWmo: wholemodule mode, combined module + object action (Swift_SEPARATE_MODULE_EMISSION OFF).
add_library(libWmo STATIC w1.swift w2.swift)
set_target_properties(libWmo PROPERTIES
  Swift_COMPILATION_MODE wholemodule
  Swift_SEPARATE_MODULE_EMISSION OFF
)

# app: consumer executable with NO module action, importing libA, libB, and libWmo (Audit #11 Edge #1!).
add_executable(app main.swift)
target_link_libraries(app PRIVATE libA libB libWmo)
