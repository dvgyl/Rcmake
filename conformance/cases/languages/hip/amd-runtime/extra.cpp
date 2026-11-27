#ifndef __HIP__
#error "extra.cpp with LANGUAGE HIP was not compiled as HIP"
#endif
extern "C" int hip_extra_val() { return 5; }
