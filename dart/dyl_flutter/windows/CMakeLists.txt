# The Flutter tooling requires that developers have a version of Visual Studio
# installed that includes CMake 3.14 or later. You should not increase this
# version, as doing so will cause the plugin to fail to compile for some
# customers of the plugin.
cmake_minimum_required(VERSION 3.14)

# This project is added through a directory symlink under the app's
# windows/flutter/ephemeral/.plugin_symlinks, so resolve the real path before
# walking up to the workspace root.
get_filename_component(DYL_FLUTTER_PLUGIN_DIR
  "${CMAKE_CURRENT_SOURCE_DIR}" REALPATH)

# Project-level configuration.
set(PROJECT_NAME "dyl_flutter")
project(${PROJECT_NAME} LANGUAGES CXX)

# Explicitly opt in to modern CMake behaviors to avoid warnings with recent
# versions of CMake.
cmake_policy(VERSION 3.14...3.25)

# This value is used when generating builds using this plugin, so it must
# not be changed.
set(PLUGIN_NAME "dyl_flutter_plugin")

list(APPEND PLUGIN_SOURCES
  "dyl_flutter_plugin.cpp"
  "dyl_flutter_plugin.h"
  "dyl_surface_window.cpp"
  "dyl_surface_window.h"
)

# Define the plugin library target. Its name must not be changed (see comment
# on PLUGIN_NAME above).
add_library(${PLUGIN_NAME} SHARED
  "include/dyl_flutter/dyl_flutter_plugin.h"
  ${PLUGIN_SOURCES}
)

# Apply a standard set of build settings that are configured in the
# application-level CMakeLists.txt.
apply_standard_settings(${PLUGIN_NAME})

# Symbols are hidden by default to reduce the chance of accidental conflicts
# between plugins.
set_target_properties(${PLUGIN_NAME} PROPERTIES
  CXX_VISIBILITY_PRESET hidden)
target_compile_definitions(${PLUGIN_NAME} PRIVATE FLUTTER_PLUGIN_IMPL)
# Keep <windows.h> from defining min/max macros; the sources use std::max.
target_compile_definitions(${PLUGIN_NAME} PRIVATE NOMINMAX)
# Sources are UTF-8; the default MSVC code page on CJK Windows is 936.
target_compile_options(${PLUGIN_NAME} PRIVATE /utf-8)

target_include_directories(${PLUGIN_NAME} INTERFACE
  "${CMAKE_CURRENT_SOURCE_DIR}/include")
target_link_libraries(${PLUGIN_NAME} PRIVATE flutter flutter_wrapper_plugin)
# ole32/uuid for IFileOpenDialog, shell32 for the shell item helpers.
target_link_libraries(${PLUGIN_NAME} PRIVATE ole32 shell32 uuid)

# The Rust kernel is a DLL loaded by dart:ffi as "dyl.dll". It has to sit
# next to the executable because that is the first place Windows looks.
set(DYL_TARGET_DIR "${DYL_FLUTTER_PLUGIN_DIR}/../../../target")
set(DYL_FFI_DLL
  "${DYL_TARGET_DIR}/$<IF:$<CONFIG:Debug>,debug,release>/dyl.dll")

# List of absolute paths to libraries that should be bundled with the plugin.
if(EXISTS "${DYL_TARGET_DIR}/debug/dyl.dll" OR
   EXISTS "${DYL_TARGET_DIR}/release/dyl.dll")
  set(dyl_flutter_bundled_libraries
    "${DYL_FFI_DLL}"
    PARENT_SCOPE
  )
else()
  set(dyl_flutter_bundled_libraries
    ""
    PARENT_SCOPE
  )
  message(WARNING
    "dyl.dll was not found under ${DYL_TARGET_DIR}. "
    "Build the Rust kernel (cargo build -p dyl-ffi) before running.")
endif()
