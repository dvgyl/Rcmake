cmake_minimum_required(VERSION 4.5)
project(SwiftGenexCustomModFixture LANGUAGES Swift)

add_library(libA STATIC)
target_sources(libA PRIVATE "$<$<CONFIG:Debug>:a.swift>")
set_target_properties(libA PROPERTIES
  Swift_COMPILATION_MODE incremental
  Swift_MODULE_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}/mods_a"
)

add_library(libB STATIC b.swift)
target_link_libraries(libB PUBLIC libA)
set_target_properties(libB PROPERTIES
  Swift_COMPILATION_MODE incremental
  Swift_MODULE_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}/mods_b"
)

add_library(unrelated STATIC u.swift)
set_target_properties(unrelated PROPERTIES
  Swift_COMPILATION_MODE incremental
)

# Consumer executable with no module action of its own.
add_executable(app main.swift)
target_link_libraries(app PRIVATE libB unrelated)
