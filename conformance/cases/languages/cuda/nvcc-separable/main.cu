__device__ int dev_value();
extern "C" int rcmake_mock_device_link();

int main() {
  return (dev_value() == 42 && rcmake_mock_device_link() == 42) ? 0 : 1;
}
