cmake_minimum_required(VERSION 4.5)
project(HipEmptyArchError LANGUAGES HIP)

add_library(bad STATIC bad.hip)
set_target_properties(bad PROPERTIES HIP_ARCHITECTURES "")
