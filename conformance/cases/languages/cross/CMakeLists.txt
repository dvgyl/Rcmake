cmake_minimum_required(VERSION 4.5)
project(StagingAndLinkDependencies LANGUAGES C)
add_library(link_requirements INTERFACE)
set_property(TARGET link_requirements PROPERTY INTERFACE_LINK_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/interface.in")
add_executable(app main.c)
target_link_libraries(app PRIVATE link_requirements)
set_property(TARGET app PROPERTY LINK_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/direct.in")
install(TARGETS app DESTINATION bin)
if(PROBE_RUNTIME_LIBRARY)
  add_library(staged_dependency SHARED IMPORTED)
  set_property(TARGET staged_dependency PROPERTY IMPORTED_LOCATION "${PROBE_RUNTIME_LIBRARY}")
  target_link_libraries(app PRIVATE staged_dependency)
  target_compile_definitions(app PRIVATE HAVE_STAGED_LIBRARY)
  set_target_properties(app PROPERTIES
    BUILD_WITH_INSTALL_RPATH ON
    INSTALL_RPATH "${CMAKE_STAGING_PREFIX}/explicit"
    INSTALL_RPATH_USE_LINK_PATH ON)
endif()
