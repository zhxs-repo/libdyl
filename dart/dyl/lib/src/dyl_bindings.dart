/// Low-level dart:ffi bindings for the C ABI (dyl.h).
///
/// This file maps 1:1 to the exported symbols of dyl-ffi. Users should
/// prefer the high-level [DylClient] API instead.
library;

import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

// ═══════════════════════════════════════════════════
// Native types
// ═══════════════════════════════════════════════════

/// Opaque handle — Dart side only sees a pointer.
final class DylHandle extends Opaque {}

/// `DylEventCallback`: `void (*)(const char *, void *)`.
typedef DylEventCallbackNative = Void Function(
  Pointer<Utf8> eventJson,
  Pointer<Void> userData,
);

// ═══════════════════════════════════════════════════
// FFI function typedefs
// ═══════════════════════════════════════════════════

// uint32_t dyl_abi_version(void)
typedef _DylAbiVersionNative = Uint32 Function();
typedef _DylAbiVersionDart = int Function();

// char *dyl_capabilities(void)
typedef _DylCapabilitiesNative = Pointer<Utf8> Function();
typedef _DylCapabilitiesDart = Pointer<Utf8> Function();

// const char *dyl_error_string(int code)
typedef _DylErrorStringNative = Pointer<Utf8> Function(Int32);
typedef _DylErrorStringDart = Pointer<Utf8> Function(int);

// DylHandle *dyl_create(void)
typedef _DylCreateNative = Pointer<DylHandle> Function();
typedef _DylCreateDart = Pointer<DylHandle> Function();

// void dyl_destroy(DylHandle *handle)
typedef _DylDestroyNative = Void Function(Pointer<DylHandle>);
typedef _DylDestroyDart = void Function(Pointer<DylHandle>);

// int64_t dyl_command(DylHandle *handle, const char *command_json)
typedef _DylCommandNative = Int64 Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);
typedef _DylCommandDart = int Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);

// char *dyl_get_snapshot(DylHandle *handle)
typedef _DylGetSnapshotNative = Pointer<Utf8> Function(
  Pointer<DylHandle>,
);
typedef _DylGetSnapshotDart = Pointer<Utf8> Function(
  Pointer<DylHandle>,
);

// char *dyl_get_property(DylHandle *handle, const char *name)
typedef _DylGetPropertyNative = Pointer<Utf8> Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);
typedef _DylGetPropertyDart = Pointer<Utf8> Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);

// int dyl_surface_attach(DylHandle *handle, const char *desc_json)
typedef _DylSurfaceAttachNative = Int32 Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);
typedef _DylSurfaceAttachDart = int Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);

// int dyl_surface_detach(DylHandle *handle)
typedef _DylSurfaceDetachNative = Int32 Function(Pointer<DylHandle>);
typedef _DylSurfaceDetachDart = int Function(Pointer<DylHandle>);

// int dyl_surface_resize(DylHandle *handle, const char *desc_json)
typedef _DylSurfaceResizeNative = Int32 Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);
typedef _DylSurfaceResizeDart = int Function(
  Pointer<DylHandle>,
  Pointer<Utf8>,
);

// int dyl_surface_redraw(DylHandle *handle)
typedef _DylSurfaceRedrawNative = Int32 Function(Pointer<DylHandle>);
typedef _DylSurfaceRedrawDart = int Function(Pointer<DylHandle>);

// int dyl_set_event_callback(DylHandle *handle,
//                               void (*callback)(const char *, void *),
//                               void *user_data)
typedef _DylSetEventCallbackNative = Int32 Function(
  Pointer<DylHandle>,
  Pointer<NativeFunction<DylEventCallbackNative>>,
  Pointer<Void>,
);
typedef _DylSetEventCallbackDart = int Function(
  Pointer<DylHandle>,
  Pointer<NativeFunction<DylEventCallbackNative>>,
  Pointer<Void>,
);

// char *dyl_poll_event(DylHandle *handle)
typedef _DylPollEventNative = Pointer<Utf8> Function(Pointer<DylHandle>);
typedef _DylPollEventDart = Pointer<Utf8> Function(Pointer<DylHandle>);

// void dyl_free_string(char *text)
typedef _DylFreeStringNative = Void Function(Pointer<Utf8>);
typedef _DylFreeStringDart = void Function(Pointer<Utf8>);

// char *dyl_last_error(DylHandle *handle)
typedef _DylLastErrorNative = Pointer<Utf8> Function(Pointer<DylHandle>);
typedef _DylLastErrorDart = Pointer<Utf8> Function(Pointer<DylHandle>);

// ═══════════════════════════════════════════════════
// Bindings class
// ═══════════════════════════════════════════════════

/// Low-level bindings to libdyl / dyl.dll / libdyl.so.
///
/// Usage:
/// ```dart
/// final bindings = DylBindings.load();
/// final handle = bindings.create();
/// ```
class DylBindings {
  DylBindings._(this._lib);

  final DynamicLibrary _lib;

  late final _DylAbiVersionDart abiVersion =
      _lib.lookupFunction<_DylAbiVersionNative, _DylAbiVersionDart>(
    'dyl_abi_version',
  );

  late final _DylCapabilitiesDart capabilities =
      _lib.lookupFunction<_DylCapabilitiesNative, _DylCapabilitiesDart>(
    'dyl_capabilities',
  );

  late final _DylErrorStringDart errorString =
      _lib.lookupFunction<_DylErrorStringNative, _DylErrorStringDart>(
    'dyl_error_string',
  );

  late final _DylCreateDart create =
      _lib.lookupFunction<_DylCreateNative, _DylCreateDart>(
    'dyl_create',
  );

  late final _DylDestroyDart destroy =
      _lib.lookupFunction<_DylDestroyNative, _DylDestroyDart>(
    'dyl_destroy',
  );

  late final _DylCommandDart command =
      _lib.lookupFunction<_DylCommandNative, _DylCommandDart>(
    'dyl_command',
  );

  late final _DylGetSnapshotDart getSnapshot =
      _lib.lookupFunction<_DylGetSnapshotNative, _DylGetSnapshotDart>(
    'dyl_get_snapshot',
  );

  late final _DylGetPropertyDart getProperty =
      _lib.lookupFunction<_DylGetPropertyNative, _DylGetPropertyDart>(
    'dyl_get_property',
  );

  late final _DylSurfaceAttachDart surfaceAttach =
      _lib.lookupFunction<_DylSurfaceAttachNative, _DylSurfaceAttachDart>(
    'dyl_surface_attach',
  );

  late final _DylSurfaceDetachDart surfaceDetach =
      _lib.lookupFunction<_DylSurfaceDetachNative, _DylSurfaceDetachDart>(
    'dyl_surface_detach',
  );

  late final _DylSurfaceResizeDart surfaceResize =
      _lib.lookupFunction<_DylSurfaceResizeNative, _DylSurfaceResizeDart>(
    'dyl_surface_resize',
  );

  late final _DylSurfaceRedrawDart surfaceRedraw =
      _lib.lookupFunction<_DylSurfaceRedrawNative, _DylSurfaceRedrawDart>(
    'dyl_surface_redraw',
  );

  late final _DylSetEventCallbackDart setEventCallback = _lib.lookupFunction<
      _DylSetEventCallbackNative,
      _DylSetEventCallbackDart>('dyl_set_event_callback');

  late final _DylPollEventDart pollEvent =
      _lib.lookupFunction<_DylPollEventNative, _DylPollEventDart>(
    'dyl_poll_event',
  );

  late final _DylFreeStringDart freeString =
      _lib.lookupFunction<_DylFreeStringNative, _DylFreeStringDart>(
    'dyl_free_string',
  );

  late final _DylLastErrorDart lastError =
      _lib.lookupFunction<_DylLastErrorNative, _DylLastErrorDart>(
    'dyl_last_error',
  );

  /// Load the native library from the default platform path.
  ///
  /// - Windows: dyl.dll
  /// - macOS:   libdyl.dylib
  /// - Linux:   libdyl.so
  /// - Android: libdyl.so
  factory DylBindings.load([String? path]) {
    final lib = DynamicLibrary.open(path ?? _defaultLibPath());
    return DylBindings._(lib);
  }

  /// Load from the current process (for statically linked scenarios).
  factory DylBindings.loadFromProcess() {
    return DylBindings._(DynamicLibrary.process());
  }

  /// The library name must match what cargo emits for the `[lib]` name in
  /// crates/dyl-ffi/Cargo.toml.
  static String _defaultLibPath() {
    if (Platform.isWindows) return 'dyl.dll';
    if (Platform.isMacOS) return 'libdyl.dylib';
    if (Platform.isLinux) return 'libdyl.so';
    if (Platform.isAndroid) return 'libdyl.so';
    if (Platform.isIOS) return 'dyl.framework/dyl';
    throw UnsupportedError('Unsupported platform: ${Platform.operatingSystem}');
  }
}
