#ifdef HIP_MOCK_RUNTIME_DEFINE
#error "CXX compile group must not receive HIP runtime defines"
#endif

extern "C" int host_helper() {
  return 11;
}
