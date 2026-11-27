cmake_minimum_required(VERSION 4.5)
project(ClangCudaFixture LANGUAGES CXX)
enable_language(CUDA)

add_executable(clang_sep main.cu device.cu)
set_target_properties(clang_sep PROPERTIES
  CUDA_SEPARABLE_COMPILATION ON
  CUDA_ARCHITECTURES 75
)

add_library(clang_ptx OBJECT ptx.cu)
set_target_properties(clang_ptx PROPERTIES
  CUDA_PTX_COMPILATION ON
  CUDA_ARCHITECTURES 75
)
