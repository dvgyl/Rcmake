cmake_minimum_required(VERSION 4.5)
project(HipNvidiaFixture LANGUAGES CXX HIP)

add_executable(nv_hip_app main.hip)
set_target_properties(nv_hip_app PROPERTIES
  HIP_ARCHITECTURES "75-real;80-virtual"
)
