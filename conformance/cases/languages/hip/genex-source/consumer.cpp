extern "C" int host_helper();
#ifdef CONSUMER_EXPECT_HIP
extern "C" int hip_genex_val();
extern "C" int rcmake_mock_hip_runtime();
#endif

int main() {
#ifdef CONSUMER_EXPECT_HIP
  return (host_helper() == 11 && hip_genex_val() == 87 && rcmake_mock_hip_runtime() == 77) ? 0 : 1;
#else
  return host_helper() == 11 ? 0 : 1;
#endif
}
