cmake_minimum_required(VERSION 3.25)
cmake_policy(SET CMP0157 OLD)
project(SwiftLegacyOldFixture LANGUAGES Swift)

add_library(old_lib STATIC lib.swift)
add_executable(old_app main.swift)
target_link_libraries(old_app PRIVATE old_lib)
