/// DylVideoView — 原生窗口/视图集成 widget
///
/// - **Android**：创建 `SurfaceView` PlatformView，获取 `ANativeWindow*` 指针，
///   通过 FFI 传给 Rust 层的 `dyl_surface_attach`。
/// - **Windows**：插件在 Flutter 视图里创建子 HWND，Dart 按 widget 的物理像素
///   矩形同步位置，再把 HWND 作为 `windows_hwnd` 描述符交给 kernel 渲染。
///   kernel 自己创建 GPU 设备与交换链，不走 Flutter texture 共享。
/// - 其他平台：显示 [DylVideoView.placeholder] 或占位提示。
///
/// 自动监听 App 生命周期（Android）：
/// - `AppLifecycleState.paused` → `client.detachSurface()`
/// - `AppLifecycleState.resumed` → 重新获取 Surface 并 `client.attachSurface()`
library;

import 'dart:async';
import 'dart:io' show Platform;

import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:dyl/dyl.dart';

/// 视频渲染 Widget。
class DylVideoView extends StatefulWidget {
  const DylVideoView({
    super.key,
    required this.client,
    this.onSurfaceReady,
    this.backgroundColor = const Color(0xFF000000),
    this.placeholder,
  });

  /// Dyl 播放器客户端实例。
  final DylClient client;

  /// Surface 成功交给渲染线程后回调（每次重建 Surface 都会调用）。
  ///
  /// load 与 surface 解耦：可以先 [DylClient.loadFile] 再等 surface，
  /// 没有 surface 时视频帧被丢弃、音频和时钟继续。这个回调只用于更新 UI 状态。
  final VoidCallback? onSurfaceReady;

  /// 视频区域背景色（Surface 未就绪时显示）。
  final Color backgroundColor;

  /// 不支持原生渲染的平台上的占位 widget。
  final Widget? placeholder;

  @override
  State<DylVideoView> createState() => _DylVideoViewState();
}

class _DylVideoViewState extends State<DylVideoView>
    with WidgetsBindingObserver {
  static const _viewType = 'dev.dylplayer.dyl_flutter/surface_view';
  static const _channel = MethodChannel('dev.dylplayer.dyl_flutter');

  /// Desktop surface ids are plugin-side keys, not PlatformView ids. Kept small
  /// and positive so they stay inside an int32 on the channel.
  static int _nextDesktopSurfaceId = 1;

  bool get _isWindows => !kIsWeb && Platform.isWindows;

  int? _viewId;
  bool _surfaceReady = false;
  int _surfaceGeneration = 0;
  double _scaleFactor = 1.0;

  /// 最近一次 attach 的句柄（十六进制字符串）。resize 只取尺寸，但 Surface 描述符
  /// 仍要求句柄字段存在，所以复用这个值而不重新取 ANativeWindow 引用。
  String? _surfaceHandle;

  /// 已经交给内核的 ANativeWindow 引用。按契约，detach 成功（渲染线程确认
  /// drop 了 Surface）后由宿主释放；不释放会一直拽着旧 Surface 和它的
  /// buffer 队列，每次前后台切换都涨一份图形内存。
  int? _surfaceWindow;

  // View whose getNativeWindow request is in flight. A second request while one
  // is pending would race for the same ANativeWindow, and the loser would have
  // to give its reference back.
  int? _attachingViewId;

  // ── Windows state ──────────────────────────────────────────────
  final GlobalKey _desktopSurfaceKey = GlobalKey();
  int? _desktopId;
  String? _desktopHandle;
  Rect? _lastDesktopFrame;
  bool _creatingDesktopSurface = false;
  bool _desktopSyncRunning = false;
  bool _desktopSyncRequested = false;
  bool _desktopSyncScheduled = false;
  DateTime? _lastDesktopRedrawAt;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    _surfaceGeneration = identityHashCode(this);
    if (_isWindows) {
      _desktopId = _nextDesktopSurfaceId++;
    }
    // Android recreates the SurfaceView surface on its own (screen off/on,
    // activity recreation, split-screen) without any Flutter lifecycle change.
    // Without this the renderer would keep presenting into a dead ANativeWindow
    // and the video stays black after returning to the foreground.
    _channel.setMethodCallHandler(_handlePlatformEvent);
  }

  @override
  void didUpdateWidget(covariant DylVideoView oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.client != widget.client) {
      // Reuse the same native window; the surface is simply re-attached to the
      // new client on the next frame.
      _detachCurrentSurface();
      if (_isWindows) {
        _lastDesktopFrame = null;
        _scheduleDesktopSync();
      }
    }
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final scale = MediaQuery.devicePixelRatioOf(context);
    if (scale != _scaleFactor) {
      _scaleFactor = scale;
      if (_isWindows) {
        _scheduleDesktopSync();
      }
    }
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _channel.setMethodCallHandler(null);
    _detachCurrentSurface();
    _destroyPlatformView();
    _destroyDesktopSurface();
    super.dispose();
  }

  // ── Native → Dart surface events ───────────────────────────────

  Future<void> _handlePlatformEvent(MethodCall call) async {
    if (call.method == 'onSurfaceInvalidated') {
      // The native child window was repainted by GDI/DWM. The swapchain is the
      // only thing that draws it, and the kernel stops presenting on pause/EOF,
      // so the frozen frame has to be presented again explicitly.
      final args = call.arguments;
      final viewId = args is Map ? args['viewId'] as int? : null;
      if (viewId == null || viewId == _desktopId) {
        _requestDesktopRedraw();
      }
      return;
    }
    if (call.method == 'onSurfaceDestroyed') {
      // The native window is about to be torn down. detachSurface() is
      // synchronous on the Rust side and only returns once the render thread
      // has released its ANativeWindow reference, so it is safe for the host to
      // destroy the window right after.
      debugPrint('DylVideoView: native reported surface destroyed, detaching');
      _detachCurrentSurface();
    } else if (call.method == 'onSurfaceCreated') {
      debugPrint('DylVideoView: native reported surface created');
      final viewId = _viewId;
      if (viewId != null && !_surfaceReady) {
        unawaited(_attachSurface(viewId));
      }
    } else if (call.method == 'onSurfaceResized') {
      final viewId = _viewId;
      if (viewId != null && _surfaceReady) {
        // Ask for the current size only. getNativeWindow also hands out a +1
        // ANativeWindow reference, which has to be given straight back since
        // this path only resizes the surface Rust already holds.
        int? borrowed;
        try {
          final result = await _channel.invokeMethod<Map<dynamic, dynamic>>(
            'getNativeWindow',
            {'viewId': viewId, 'generation': _surfaceGeneration},
          );
          borrowed = result?['nativeWindow'] as int? ?? 0;
          final width = result?['width'] as int? ?? 0;
          final height = result?['height'] as int? ?? 0;
          if (width > 0 && height > 0) {
            final handle = _surfaceHandle;
            if (handle != null) {
              widget.client.resizeSurface(
                HandleSurfaceDescriptor(
                  kind: SurfaceKind.androidNativeWindow,
                  handle: handle,
                  width: width,
                  height: height,
                  scaleFactor: _scaleFactor,
                ),
              );
            }
            debugPrint('DylVideoView: resized to ${width}x$height');
          }
        } on PlatformException catch (e) {
          debugPrint('DylVideoView: resize failed: ${e.message}');
        } on DylException catch (e) {
          debugPrint('DylVideoView: resizeSurface failed: $e');
        } finally {
          if (borrowed != null && borrowed != 0) {
            await _releaseNativeWindow(borrowed);
          }
        }
      }
    }
  }

  // ── Lifecycle ──────────────────────────────────────────────────

  /// An OS resize changes the widget box without rebuilding this widget: the
  /// example hoists the `DylVideoView` instance, so `updateChild` sees an
  /// identical widget and skips `build()`. Without this hook the child HWND and
  /// the kernel surface keep the pre-resize rect whenever nothing else dirties
  /// the tree (paused or EOF), which leaves the video painted into one corner.
  @override
  void didChangeMetrics() {
    if (_isWindows) {
      _scheduleDesktopSync();
    }
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    // Windows keeps the child HWND alive while minimized or unfocused; the
    // swapchain stays valid, so there is nothing to detach.
    if (_isWindows) return;
    switch (state) {
      case AppLifecycleState.paused:
      case AppLifecycleState.hidden:
      case AppLifecycleState.inactive:
        // Surface 可能即将被销毁 — detach
        if (_surfaceReady) {
          _detachCurrentSurface();
        }
        break;
      case AppLifecycleState.resumed:
        // Activity 恢复 — 如果 PlatformView 还在，重新 attach
        if (_viewId != null && !_surfaceReady) {
          unawaited(_reattachSurface());
        }
        break;
      case AppLifecycleState.detached:
        _detachCurrentSurface();
        break;
    }
  }

  // ── PlatformView management (Android) ──────────────────────────

  void _handlePlatformViewCreated(int id) {
    if (!mounted) return;
    _viewId = id;
    unawaited(_attachSurface(id));
  }

  Future<void> _attachSurface(int viewId) async {
    if (!mounted || widget.client.isDisposed) return;

    // Only one attach may be in flight; a concurrent surfaceCreated event
    // (or a resume while the first request is still pending) would otherwise
    // race for the same native window.
    if (_attachingViewId != null) return;
    _attachingViewId = viewId;

    int? nativeWindow;
    try {
      final result = await _channel.invokeMethod<Map<dynamic, dynamic>>(
        'getNativeWindow',
        {'viewId': viewId, 'generation': _surfaceGeneration},
      );
      // The reply is worthless if the view went away (or another attach won
      // the race) while we were waiting. Ownership is still ours, so it has to
      // be returned to the host rather than dropped.
      if (result == null || !mounted || _viewId != viewId || widget.client.isDisposed) {
        final orphan = result?['nativeWindow'] as int? ?? 0;
        if (orphan != 0) {
          await _releaseNativeWindow(orphan);
        }
        return;
      }

      final window = result['nativeWindow'] as int? ?? 0;
      nativeWindow = window;
      final width = result['width'] as int? ?? 0;
      final height = result['height'] as int? ?? 0;

      if (window == 0) {
        debugPrint('DylVideoView: nativeWindow is null (surface not ready)');
        return;
      }
      if (width <= 0 || height <= 0) {
        debugPrint(
          'DylVideoView: surface size is ${width}x$height, waiting for layout',
        );
        return;
      }

      final handle = '0x${_pointerHex(window)}';

      // Ownership transfer point: after this the render thread owns the window
      // and releases it on detach, so we must not release it ourselves.
      widget.client.attachSurface(
        HandleSurfaceDescriptor(
          kind: SurfaceKind.androidNativeWindow,
          handle: handle,
          width: width,
          height: height,
          scaleFactor: _scaleFactor,
        ),
      );
      nativeWindow = null;
      _surfaceHandle = handle;
      _surfaceWindow = window;
      _surfaceReady = true;
      debugPrint(
        'DylVideoView: surface attached ($width x $height, '
        'nativeWindow=0x${_pointerHex(window)})',
      );
      widget.onSurfaceReady?.call();
    } on PlatformException catch (e) {
      debugPrint('DylVideoView: attachSurface failed: ${e.message}');
    } on DylException catch (e) {
      debugPrint('DylVideoView: attachSurface failed: $e');
    } finally {
      // Any window we received but did not hand over has to go back, or the
      // SurfaceView runs out of native windows and stops getting new surfaces.
      if (nativeWindow != null && nativeWindow != 0) {
        await _releaseNativeWindow(nativeWindow);
      }
      _attachingViewId = null;
    }
  }

  /// 归还一个未交给 Rust 的 ANativeWindow 引用。
  Future<void> _releaseNativeWindow(int window) async {
    try {
      await _channel.invokeMethod<void>(
        'releaseNativeWindow',
        {'window': window},
      );
      debugPrint('DylVideoView: released native window 0x${_pointerHex(window)}');
    } on PlatformException catch (e) {
      debugPrint('DylVideoView: releaseNativeWindow failed: ${e.message}');
    }
  }

  Future<void> _reattachSurface() async {
    final viewId = _viewId;
    if (viewId == null || !mounted) return;
    await _attachSurface(viewId);
  }

  /// ANativeWindow 指针从 JNI 以 64 位原样传回，Android 的分配器会给高位打
  /// top-byte tag（0xB4…），在 Dart 的有符号 int 里就是负数。内核按无符号
  /// 十六进制解析句柄，直接 `toRadixString` 会变成 "0x-4bff…" 被拒绝，
  /// 所以这里按原始 64 位比特格式化。
  static String _pointerHex(int window) =>
      BigInt.from(window).toUnsigned(64).toRadixString(16);

  void _detachCurrentSurface() {
    if (_surfaceReady && !widget.client.isDisposed) {
      try {
        widget.client.detachSurface();
        debugPrint('DylVideoView: surface detached');
        // detach 成功后内核不再持有窗口，才轮到宿主归还这一份引用；
        // detach 失败时句柄仍归内核，此时释放会造成 use-after-free。
        final window = _surfaceWindow;
        if (window != null) {
          unawaited(_releaseNativeWindow(window));
        }
      } on DylException catch (e) {
        debugPrint('DylVideoView: detachSurface failed: $e');
      }
    }
    _surfaceReady = false;
    _surfaceHandle = null;
    _surfaceWindow = null;
  }

  Future<void> _destroyPlatformView() async {
    final viewId = _viewId;
    if (viewId == null) return;
    _viewId = null;
    try {
      await _channel.invokeMethod('destroyView', {'viewId': viewId});
    } on PlatformException {
      // Ignore — view may already be destroyed
    }
  }

  // ── Windows surface management ─────────────────────────────────

  void _scheduleDesktopSync() {
    if (_desktopSyncScheduled) return;
    _desktopSyncScheduled = true;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _desktopSyncScheduled = false;
      if (!mounted) return;
      unawaited(_runDesktopSync());
    });
  }

  /// Re-presents the current frame after the native window was repainted.
  ///
  /// Paint storms are coalesced: at most one request per frame interval, since
  /// the kernel call is synchronous and each one acquires and presents.
  void _requestDesktopRedraw() {
    if (!mounted || !_surfaceReady || widget.client.isDisposed) return;
    final now = DateTime.now();
    final last = _lastDesktopRedrawAt;
    if (last != null && now.difference(last) < const Duration(milliseconds: 16)) {
      return;
    }
    _lastDesktopRedrawAt = now;
    try {
      widget.client.redrawSurface();
    } on DylException catch (e) {
      debugPrint('DylVideoView: redrawSurface failed: $e');
    } on Object catch (e) {
      // A missing export means the loaded DLL predates the client.
      debugPrint('DylVideoView: redrawSurface unavailable: $e');
    }
  }

  /// Single-flight wrapper: several builds can schedule a sync in one frame, and
  /// two overlapping runs would both attach (the second attach replaces the
  /// first device while it is still rendering).
  Future<void> _runDesktopSync() async {
    if (_desktopSyncRunning) {
      _desktopSyncRequested = true;
      return;
    }
    _desktopSyncRunning = true;
    try {
      do {
        _desktopSyncRequested = false;
        await _syncDesktopFrame();
      } while (_desktopSyncRequested && mounted);
    } finally {
      _desktopSyncRunning = false;
    }
  }

  /// Keeps the native child window aligned with this widget and attaches it to
  /// the engine once a size is known.
  Future<void> _syncDesktopFrame() async {
    if (!mounted || !_isWindows) return;
    final client = widget.client;
    final id = _desktopId;
    if (client.isDisposed || id == null) return;

    final renderBox =
        _desktopSurfaceKey.currentContext?.findRenderObject() as RenderBox?;
    if (renderBox == null || !renderBox.attached || !renderBox.hasSize) return;
    final size = renderBox.size;
    if (size.isEmpty) return;

    final dpr = MediaQuery.devicePixelRatioOf(context);
    final origin = renderBox.localToGlobal(Offset.zero);
    final width = (size.width * dpr).round();
    final height = (size.height * dpr).round();
    if (width <= 0 || height <= 0) return;
    final frame = Rect.fromLTWH(
      (origin.dx * dpr).roundToDouble(),
      (origin.dy * dpr).roundToDouble(),
      width.toDouble(),
      height.toDouble(),
    );

    if (_desktopHandle == null) {
      if (_creatingDesktopSurface) return;
      _creatingDesktopSurface = true;
      try {
        final created =
            await _channel.invokeMapMethod<String, dynamic>('createSurface', {
          'viewId': id,
          'width': width,
          'height': height,
        });
        if (!mounted || _desktopId != id || widget.client.isDisposed) {
          await _channel.invokeMethod('destroySurface', {'viewId': id});
          return;
        }
        final handle = created?['handle'] as String?;
        if (handle == null || handle.isEmpty) {
          debugPrint('DylVideoView: createSurface returned no handle');
          return;
        }
        _desktopHandle = handle;
      } on PlatformException catch (e) {
        debugPrint('DylVideoView: createSurface failed: ${e.message}');
        return;
      } finally {
        _creatingDesktopSurface = false;
      }
    }

    final handle = _desktopHandle;
    if (handle == null || !mounted || _desktopId != id || client.isDisposed) {
      return;
    }

    if (!_surfaceReady) {
      // Position while hidden, attach, then show: an unattached child window
      // would flash the desktop before the kernel starts presenting.
      await _placeDesktopWindow(id, frame, visible: false);
      if (!mounted || _desktopId != id || client.isDisposed) return;
      try {
        client.attachSurface(
          HandleSurfaceDescriptor(
            kind: SurfaceKind.windowsHwnd,
            handle: handle,
            width: width,
            height: height,
            scaleFactor: dpr,
          ),
        );
      } on DylException catch (e) {
        debugPrint('DylVideoView: attachSurface failed: $e');
        return;
      }
      _surfaceReady = true;
      _lastDesktopFrame = frame;
      await _placeDesktopWindow(id, frame, visible: true);
      debugPrint(
        'DylVideoView: windows surface attached '
        '(handle=$handle, ${width}x$height @ $dpr)',
      );
      widget.onSurfaceReady?.call();
      return;
    }

    await _placeDesktopWindow(id, frame, visible: true);

    final last = _lastDesktopFrame;
    if (last == null ||
        last.width != frame.width ||
        last.height != frame.height) {
      _lastDesktopFrame = frame;
      if (!mounted || _desktopId != id || client.isDisposed) return;
      try {
        client.resizeSurface(
          HandleSurfaceDescriptor(
            kind: SurfaceKind.windowsHwnd,
            handle: handle,
            width: width,
            height: height,
            scaleFactor: dpr,
          ),
        );
        debugPrint(
          'DylVideoView: windows surface resized '
          '(${width}x$height @ $dpr, frame=${frame.left.round()},${frame.top.round()})',
        );
      } on DylException catch (e) {
        debugPrint('DylVideoView: resizeSurface failed: $e');
      }
    }
  }

  Future<void> _placeDesktopWindow(int id, Rect frame, {required bool visible}) async {
    try {
      await _channel.invokeMethod('resizeSurface', {
        'viewId': id,
        'x': frame.left.round(),
        'y': frame.top.round(),
        'width': frame.width.round(),
        'height': frame.height.round(),
        'visible': visible,
      });
    } on PlatformException catch (e) {
      debugPrint('DylVideoView: resizeSurface failed: ${e.message}');
    }
  }

  Future<void> _destroyDesktopSurface() async {
    final id = _desktopId;
    _desktopId = null;
    _desktopHandle = null;
    _lastDesktopFrame = null;
    if (id == null) return;
    try {
      await _channel.invokeMethod('destroySurface', {'viewId': id});
    } on PlatformException {
      // Ignore — the plugin may already be gone during shutdown.
    }
  }

  // ── Build ──────────────────────────────────────────────────────

  @override
  Widget build(BuildContext context) {
    if (_isWindows) {
      // Any rebuild can change the widget's position or the DPR; re-sync the
      // native child window after this frame's layout.
      _scheduleDesktopSync();
      return ColoredBox(
        color: widget.backgroundColor,
        child: SizedBox.expand(key: _desktopSurfaceKey),
      );
    }

    if (kIsWeb || !Platform.isAndroid) {
      return widget.placeholder ??
          ColoredBox(
            color: widget.backgroundColor,
            child: const Center(
              child: Text(
                'Video rendering requires Android or Windows',
                style: TextStyle(color: Color(0xFF888888)),
              ),
            ),
          );
    }

    return ColoredBox(
      color: widget.backgroundColor,
      child: AndroidView(
        viewType: _viewType,
        layoutDirection: TextDirection.ltr,
        creationParamsCodec: const StandardMessageCodec(),
        creationParams: <String, Object?>{
          'generation': _surfaceGeneration,
        },
        onPlatformViewCreated: _handlePlatformViewCreated,
        hitTestBehavior: PlatformViewHitTestBehavior.opaque,
        gestureRecognizers: const <Factory<OneSequenceGestureRecognizer>>{},
      ),
    );
  }
}
