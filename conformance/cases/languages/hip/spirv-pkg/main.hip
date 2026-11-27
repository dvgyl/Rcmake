#ifndef RCMAKE_MOCK_SPIRV_FIXUPS
#error "Missing spirv_fixups.h include"
#endif
extern "C" int rcmake_mock_chipstar_rt();
int main() {
  return rcmake_mock_chipstar_rt() == 88 ? 0 : 1;
}
