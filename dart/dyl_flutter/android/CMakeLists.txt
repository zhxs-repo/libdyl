cmake_minimum_required(VERSION 3.18)
project(dyl_flutter_jni LANGUAGES C)

add_library(dyl_flutter_jni SHARED
    src/main/jni/dyl_flutter_jni.c
)

# Link Android NDK libraries
target_link_libraries(dyl_flutter_jni
    android    # ANativeWindow_fromSurface
    log        # Android logging
)
