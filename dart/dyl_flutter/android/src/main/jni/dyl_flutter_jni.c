/**
 * dyl_flutter_jni.c — JNI bridge for ANativeWindow* extraction
 *
 * Provides Java_com_dylplayer_dyl_flutter_DylPlatformSurfaceView_getNativeWindowFromSurface
 * which calls Android NDK's ANativeWindow_fromSurface() to get the native
 * window pointer from a Java Surface object.
 *
 * The returned pointer (as jlong) is passed through MethodChannel to Dart,
 * then through FFI to Rust's dyl_surface_attach().
 *
 * Ownership: ANativeWindow_fromSurface returns a +1 reference. The normal path
 * hands that reference to Rust for the lifetime of the attached surface; the
 * kernel never calls ANativeWindow_release (surface_detach only guarantees the
 * wgpu Surface is gone). After dyl_surface_detach() returns success the host
 * must call releaseNativeWindow() with that pointer, or every attach/detach
 * cycle leaks a Surface plus its graphic buffers. releaseNativeWindow() is also
 * how an abandoned reference is given back (disposed client, DylException,
 * surface torn down between the request and the reply).
 */

#include <jni.h>
#include <android/native_window_jni.h>
#include <stdint.h>

JNIEXPORT jlong JNICALL
Java_dev_dylplayer_dyl_1flutter_DylPlatformSurfaceView_getNativeWindowFromSurface(
    JNIEnv *env,
    jclass clazz,
    jobject surface
) {
    if (surface == NULL) {
        return 0;
    }

    ANativeWindow *window = ANativeWindow_fromSurface(env, surface);
    if (window == NULL) {
        return 0;
    }

    return (jlong)(intptr_t)window;
}

/**
 * Give back a reference obtained from getNativeWindowFromSurface.
 *
 * Needed when Dart obtains a pointer but never hands it to
 * dyl_surface_attach() -- otherwise the +1 reference leaks for the lifetime
 * of the process and the SurfaceView can fail to acquire a new surface.
 */
JNIEXPORT void JNICALL
Java_dev_dylplayer_dyl_1flutter_DylPlatformSurfaceView_releaseNativeWindow(
    JNIEnv *env,
    jclass clazz,
    jlong window
) {
    if (window == 0) {
        return;
    }
    ANativeWindow_release((ANativeWindow *)(intptr_t)window);
}
