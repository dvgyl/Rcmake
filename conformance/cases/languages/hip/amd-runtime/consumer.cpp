extern "C" int hip_kernel_val();
extern "C" int rcmake_mock_hip_runtime();

int main() {
  return (hip_kernel_val() == 92 && rcmake_mock_hip_runtime() == 77) ? 0 : 1;
}
