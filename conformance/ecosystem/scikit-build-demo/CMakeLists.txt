# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 4.5)
project(ScikitBuildProbe LANGUAGES C)
find_package(Python COMPONENTS Interpreter Development.Module REQUIRED)
Python_add_library(rcmake_scikit_probe MODULE WITH_SOABI src/demo.c)
install(TARGETS rcmake_scikit_probe DESTINATION .)
