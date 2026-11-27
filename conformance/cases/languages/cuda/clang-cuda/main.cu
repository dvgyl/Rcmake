__device__ int clang_dev_val();
extern "C" int rcmake_mock_clang_device_link();

int main() {
  return (clang_dev_val() == 42 && rcmake_mock_clang_device_link() == 42) ? 0 : 1;
}
