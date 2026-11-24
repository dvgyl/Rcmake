# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 4.5)
project(LocalDependency LANGUAGES C)
add_library(local-dependency STATIC local.c)
install(TARGETS local-dependency DESTINATION lib)
