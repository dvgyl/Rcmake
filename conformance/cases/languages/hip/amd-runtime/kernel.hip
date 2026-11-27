#include <hip/hip_runtime.h>
#include <hip_mock_runtime.h>
#ifndef HIP_MOCK_RUNTIME_DEFINE
#error "Missing HIP_MOCK_RUNTIME_DEFINE from hip::device runtime target"
#endif

extern "C" int hip_extra_val();
extern "C" int hip_kernel_val() {
  return 10 + hip_extra_val() + rcmake_mock_hip_runtime();
}
