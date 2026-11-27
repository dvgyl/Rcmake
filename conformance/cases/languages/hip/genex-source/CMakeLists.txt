cmake_minimum_required(VERSION 4.5)
project(HipGenexSourceFixture LANGUAGES CXX HIP)

add_library(hiplib STATIC host.cpp)
target_sources(hiplib PRIVATE "$<$<CONFIG:Debug>:kernel.hip>")
set_target_properties(hiplib PROPERTIES
  HIP_ARCHITECTURES "gfx90a"
)

add_executable(consumer consumer.cpp)
target_link_libraries(consumer PRIVATE hiplib)
target_compile_definitions(consumer PRIVATE "$<$<CONFIG:Debug>:CONSUMER_EXPECT_HIP>")
