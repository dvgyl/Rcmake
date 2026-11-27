cmake_minimum_required(VERSION 4.5)
project(CudaSeparableFixture LANGUAGES CXX)
enable_language(CUDA)

add_executable(basic basic.cu)

add_library(devlib STATIC device.cu)
set_target_properties(devlib PROPERTIES
  CUDA_SEPARABLE_COMPILATION ON
  CUDA_ARCHITECTURES "75-real;80-virtual"
)

add_library(resolved_lib STATIC resolved.cu)
set_target_properties(resolved_lib PROPERTIES
  CUDA_SEPARABLE_COMPILATION ON
  CUDA_RESOLVE_DEVICE_SYMBOLS ON
)

add_executable(app main.cu)
target_link_libraries(app PRIVATE devlib)
target_compile_features(app PRIVATE cuda_std_17)
set_target_properties(app PROPERTIES
  CUDA_SEPARABLE_COMPILATION ON
  CUDA_STANDARD 17
  CUDA_STANDARD_REQUIRED ON
)
if(MOCK_CUDA_LINKER)
  set_target_properties(app PROPERTIES CUDA_LINKER_LAUNCHER "${MOCK_CUDA_LINKER}")
endif()

add_library(arch_off OBJECT arch_off.cu)
set_target_properties(arch_off PROPERTIES CUDA_ARCHITECTURES OFF)

add_library(ptx_obj OBJECT ptx.cu)
set_target_properties(ptx_obj PROPERTIES CUDA_PTX_COMPILATION ON CUDA_ARCHITECTURES 75)
