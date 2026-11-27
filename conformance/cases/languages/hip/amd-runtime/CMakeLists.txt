cmake_minimum_required(VERSION 4.5)
project(HipAmdRuntimeFixture LANGUAGES CXX HIP)

set_source_files_properties(extra.cpp PROPERTIES LANGUAGE HIP)
add_library(hiplib STATIC kernel.hip extra.cpp)
set_target_properties(hiplib PROPERTIES
  HIP_ARCHITECTURES "gfx90a:xnack+;gfx1100"
  HIP_STANDARD 17
  HIP_STANDARD_REQUIRED ON
)
target_compile_features(hiplib PUBLIC hip_std_17)

add_library(arch_off OBJECT arch_off.hip)
set_target_properties(arch_off PROPERTIES HIP_ARCHITECTURES OFF)

# Audit Finding #12: Pure C++ consumer linking static HIP library must receive
# hip::device's include dirs, compile definitions, and transitive hip::host library.
add_executable(consumer consumer.cpp)
target_link_libraries(consumer PRIVATE hiplib)
