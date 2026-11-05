/// High-level Dart API for the Dyl Player ABI.
///
/// Commands are asynchronous envelopes with a `request_id`; every accepted
/// command produces exactly one [CommandCompletedEvent]. The read-only
/// [Snapshot] is the source of truth, and [events] are notifications — after
/// handling an event, re-read [snapshot] instead of reconstructing state from
/// event deltas.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'dyl_bindings.dart';
import 'contract/contract.dart';

/// Exception thrown when the native layer rejects a call synchronously.
class DylException implements Exception {
  DylException(this.codeNum, this.message);

  /// Numeric error code (negative). See [ErrorCode].
  final int codeNum;
  final String message;

  ErrorCode? get code => ErrorCode.fromCodeNum(codeNum);

  @override
  String toString() => 'DylException(${code?.wire ?? codeNum}): $message';
}

/// An accepted command that did not complete with [CommandStatus.ok].
///
/// [CommandStatus.superseded] and [CommandStatus.cancelled] are normal
/// outcomes, not error codes; inspect [status] rather than assuming failure.
class DylCommandException implements Exception {
  DylCommandException(this.completion);

  final CommandCompletedEvent completion;

  CommandStatus get status => completion.status;
  ErrorInfo? get error => completion.error;

  @override
  String toString() {
    final detail = completion.error?.message;
    return 'DylCommandException(${completion.op.wire}: '
        '${completion.status.wire}${detail == null ? '' : ': $detail'})';
  }
}

class _PendingCommand {
  _PendingCommand(this.completer, this.timer);

  final Completer<CommandCompletedEvent> completer;
  final Timer timer;
}

/// A `load_file` in flight: the command future resolves on `command_completed`,
/// the caller-facing future resolves on the matching `file_loaded`.
class _PendingLoad {
  _PendingLoad(this.completer);

  final Completer<MediaInfo> completer;
  FileLoadedEvent? fileLoaded;
}

/// High-level client for the Dyl Player.
///
/// ```dart
/// final client = DylClient(DylBindings.load());
///
/// client.events.listen((envelope) {
///   final event = envelope.event;
///   if (event is FileLoadedEvent) {
///     print('duration: ${event.durationMs} ms');
///   }
/// });
///
/// final media = await client.loadFile('/path/to/video.mp4');
/// print('video: ${media.video?.width}x${media.video?.height}');
/// await client.play();
///
/// client.dispose();
/// ```
class DylClient {
  /// Create a new player instance.
  ///
  /// Throws [DylException] when the loaded library reports an ABI major
  /// other than 0, or when `dyl_create` fails.
  DylClient(
    this._bindings, {
    Duration pollInterval = const Duration(milliseconds: 16),
  }) {
    final packed = _bindings.abiVersion();
    _abiVersion = packed;
    final major = (packed >> 16) & 0xffff;
    if (major != requiredAbiMajor) {
      throw DylException(
        ErrorCode.notSupported.codeNum,
        'unsupported ABI major $major (packed 0x'
        '${packed.toRadixString(16)}); this client requires $requiredAbiMajor',
      );
    }

    _handle = _bindings.create();
    if (_handle == nullptr) {
      throw DylException(
        ErrorCode.internal.codeNum,
        'dyl_create returned a null handle',
      );
    }
    _pollTimer = Timer.periodic(pollInterval, (_) => _pollEvents());
  }

  /// ABI major supported by this client.
  static const int requiredAbiMajor = 0;

  final DylBindings _bindings;
  late final Pointer<DylHandle> _handle;
  late final int _abiVersion;
  Timer? _pollTimer;
  bool _disposed = false;

  final StreamController<EventEnvelope> _eventController =
      StreamController<EventEnvelope>.broadcast();

  final Map<int, _PendingCommand> _pending = {};
  final Map<int, _PendingLoad> _pendingLoads = {};
  int _nextRequestId = 1;

  Capabilities? _capabilities;

  /// Whether [dispose] has been called.
  bool get isDisposed => _disposed;

  /// Packed ABI version returned by `dyl_abi_version()`.
  int get abiVersion => _abiVersion;

  /// Static capabilities of the loaded build.
  Capabilities get capabilities => _capabilities ??= _readCapabilities();

  /// Event notifications. Semantic events precede the matching
  /// `command_completed`, and `seq` increases monotonically (gaps allowed).
  Stream<EventEnvelope> get events => _eventController.stream;

  // ═══════════════════════════════════════════════════
  // Commands
  // ═══════════════════════════════════════════════════

  /// Send [command] and resolve on its `command_completed` event.
  ///
  /// The completion event is returned as-is: check [CommandCompletedEvent
  /// .status] — `superseded` and `cancelled` are normal outcomes, not errors.
  /// If no completion arrives within [timeout], the returned future fails with
  /// a [TimeoutException]; that `request_id` is never reused.
  ///
  /// Throws [DylException] for a synchronous rejection (the command was not
  /// accepted at all).
  Future<CommandCompletedEvent> execute(
    Command command, {
    Duration timeout = const Duration(seconds: 10),
  }) =>
      _dispatch(command, timeout).$2;

  /// Send [command] and return the request id the native side accepted plus
  /// the future that resolves on `command_completed`.
  (int, Future<CommandCompletedEvent>) _dispatch(
    Command command,
    Duration timeout,
  ) {
    _ensureAlive();

    final requestId = _nextRequestId++;
    final completer = Completer<CommandCompletedEvent>();
    final timer = Timer(timeout, () {
      final pending = _pending.remove(requestId);
      if (pending == null || pending.completer.isCompleted) return;
      pending.completer.completeError(
        TimeoutException(
          'command ${command.op.wire} (request_id $requestId) timed out',
          timeout,
        ),
      );
      final load = _pendingLoads.remove(requestId);
      if (load != null && !load.completer.isCompleted) {
        load.completer.completeError(
          TimeoutException(
            'command ${command.op.wire} (request_id $requestId) timed out',
            timeout,
          ),
        );
      }
    });
    _pending[requestId] = _PendingCommand(completer, timer);

    final envelope = CommandEnvelope(
      requestId: requestId,
      command: command,
    );
    final json = jsonEncode(envelope.toJson());
    final cmdPtr = json.toNativeUtf8();
    final int result;
    try {
      result = _bindings.command(_handle, cmdPtr);
    } finally {
      calloc.free(cmdPtr);
    }

    if (result < 0) {
      _pending.remove(requestId)?.timer.cancel();
      throw DylException(result, _takeErrorMessage(result));
    }
    if (result != requestId) {
      // The engine echoes a client-supplied request_id; keep the pending entry
      // addressable by whatever id the native side actually used.
      final pending = _pending.remove(requestId);
      if (pending != null) _pending[result] = pending;
    }
    return (result, completer.future);
  }

  /// Load a local media file and resolve with the media discovered by demux.
  ///
  /// Existence and demux failures reject the future with
  /// [DylCommandException]; a superseded load (a newer `loadFile` arrived
  /// first) is reported the same way with [CommandStatus.superseded].
  Future<MediaInfo> loadFile(
    String url, {
    Duration timeout = const Duration(seconds: 10),
  }) {
    _ensureAlive();
    final pending = _PendingLoad(Completer<MediaInfo>());
    final (requestId, _) = _dispatch(LoadFileCommand(url), timeout);
    // Events are only polled on a timer, so nothing can arrive between
    // `_dispatch` returning and this registration.
    _pendingLoads[requestId] = pending;
    return pending.completer.future;
  }

  /// Start or resume playback. From `ended`/`stopped` this restarts at 0.
  Future<CommandCompletedEvent> play() => execute(const PlayCommand());

  Future<CommandCompletedEvent> pause() => execute(const PauseCommand());

  /// Release decode resources and land in `stopped`.
  Future<CommandCompletedEvent> stop() => execute(const StopCommand());

  /// Seek to an absolute position. Negative positions are rejected.
  Future<CommandCompletedEvent> seek(
    Duration position, {
    SeekMode mode = SeekMode.exact,
  }) =>
      execute(SeekCommand(
        positionMs: position.inMilliseconds,
        mode: mode,
      ));

  /// Seek by a signed delta, clamped atomically to `[0, duration_ms]`.
  Future<CommandCompletedEvent> seekRelative(
    Duration delta, {
    SeekMode mode = SeekMode.exact,
  }) =>
      execute(SeekRelativeCommand(
        deltaMs: delta.inMilliseconds,
        mode: mode,
      ));

  /// Set volume (0–100).
  Future<CommandCompletedEvent> setVolume(int volume) =>
      execute(SetVolumeCommand(volume));

  Future<CommandCompletedEvent> setMute(bool muted) =>
      execute(SetMuteCommand(muted));

  Future<CommandCompletedEvent> setSubtitleVisible(bool visible) =>
      execute(SetSubtitleVisibleCommand(visible));

  /// Load an external subtitle file (SRT/ASS/SSA).
  Future<CommandCompletedEvent> loadSubtitle(String path) =>
      execute(LoadSubtitleCommand(path));

  Future<CommandCompletedEvent> setDanmakuVisible(bool visible) =>
      execute(SetDanmakuVisibleCommand(visible));

  /// Set danmaku opacity (0–100).
  Future<CommandCompletedEvent> setDanmakuOpacity(int opacity) =>
      execute(SetDanmakuOpacityCommand(opacity));

  /// Load a Bilibili XML danmaku file.
  Future<CommandCompletedEvent> loadDanmaku(String path) =>
      execute(LoadDanmakuCommand(path));

  /// Switch the audio or subtitle track by stream index (from `media.tracks`).
  /// The kernel reopens the decode pipeline, so this passes through `loading`.
  /// Video tracks are not switchable and are rejected with `not_supported`.
  Future<CommandCompletedEvent> selectTrack({
    required TrackKind kind,
    required int index,
  }) =>
      execute(SelectTrackCommand(kind: kind, index: index));

  /// Switch the decode mode. `auto` prefers a hardware decoder and falls back
  /// to software; a build/platform without a verified hardware path rejects it
  /// synchronously with `not_supported` (see `capabilities.features.hw_decode`).
  /// While a pipeline is alive the kernel reopens it (passes through `loading`,
  /// keeps the position) and no `file_loaded` is emitted; the effective backend
  /// is published as `snapshot.decode`.
  Future<CommandCompletedEvent> setDecodeMode(DecodeMode mode) =>
      execute(SetDecodeModeCommand(mode));

  // ═══════════════════════════════════════════════════
  // Snapshot / properties
  // ═══════════════════════════════════════════════════

  /// Read the full read-only snapshot. This is the single source of truth.
  Snapshot snapshot() {
    _ensureAlive();
    final ptr = _bindings.getSnapshot(_handle);
    if (ptr == nullptr) {
      throw DylException(
        ErrorCode.internal.codeNum,
        _takeErrorMessage(ErrorCode.internal.codeNum),
      );
    }
    try {
      final decoded = jsonDecode(ptr.toDartString());
      return Snapshot.fromJson(decoded as Map<String, dynamic>);
    } finally {
      _bindings.freeString(ptr);
    }
  }

  /// Read a top-level snapshot field as a decoded JSON value.
  ///
  /// Returns null for unknown names; the reason is left in `last_error`.
  Object? property(String name) {
    _ensureAlive();
    final namePtr = name.toNativeUtf8();
    try {
      final ptr = _bindings.getProperty(_handle, namePtr);
      if (ptr == nullptr) return null;
      try {
        return jsonDecode(ptr.toDartString());
      } finally {
        _bindings.freeString(ptr);
      }
    } finally {
      calloc.free(namePtr);
    }
  }

  /// Take (and clear) the most recent synchronous error message.
  String? get lastError {
    if (_disposed) return null;
    return _takeLastError();
  }

  // ═══════════════════════════════════════════════════
  // Surface
  // ═══════════════════════════════════════════════════

  /// Attach or replace the external surface.
  ///
  /// Transactional: on failure the previous output stays attached. May be
  /// called before or after [loadFile]; without a surface, video frames are
  /// dropped while audio and the clock continue.
  void attachSurface(SurfaceDescriptor descriptor) {
    _ensureAlive();
    final json = jsonEncode(descriptor.toJson());
    final ptr = json.toNativeUtf8();
    final int result;
    try {
      result = _bindings.surfaceAttach(_handle, ptr);
    } finally {
      calloc.free(ptr);
    }
    if (result != 0) {
      throw DylException(result, _takeErrorMessage(result));
    }
  }

  /// Detach synchronously. Only after this returns may the host release the
  /// native handle.
  void detachSurface() {
    _ensureAlive();
    final result = _bindings.surfaceDetach(_handle);
    if (result != 0) {
      throw DylException(result, _takeErrorMessage(result));
    }
  }

  /// Resize the attached surface. The descriptor must stay the same kind as
  /// the attached surface; only `width`/`height` are used.
  void resizeSurface(SurfaceDescriptor descriptor) {
    _ensureAlive();
    final json = jsonEncode(descriptor.toJson());
    final ptr = json.toNativeUtf8();
    final int result;
    try {
      result = _bindings.surfaceResize(_handle, ptr);
    } finally {
      calloc.free(ptr);
    }
    if (result != 0) {
      throw DylException(result, _takeErrorMessage(result));
    }
  }

  /// Ask the kernel to present the current frame again.
  ///
  /// The kernel stops presenting after pause/EOF. When the host window content
  /// is erased anyway (Windows child-window repaint, window restore), call this
  /// to restore the frozen picture. No-op when no surface is attached.
  void redrawSurface() {
    _ensureAlive();
    final result = _bindings.surfaceRedraw(_handle);
    if (result != 0) {
      throw DylException(result, _takeErrorMessage(result));
    }
  }

  // ═══════════════════════════════════════════════════
  // Internals
  // ═══════════════════════════════════════════════════

  void _ensureAlive() {
    if (_disposed) throw StateError('DylClient has been disposed');
  }

  Capabilities _readCapabilities() {
    _ensureAlive();
    final ptr = _bindings.capabilities();
    if (ptr == nullptr) {
      throw DylException(
        ErrorCode.internal.codeNum,
        'dyl_capabilities returned null',
      );
    }
    try {
      final decoded = jsonDecode(ptr.toDartString());
      return Capabilities.fromJson(decoded as Map<String, dynamic>);
    } finally {
      _bindings.freeString(ptr);
    }
  }

  String _takeErrorMessage(int code) {
    final message = _takeLastError();
    if (message != null && message.isNotEmpty) return message;
    final ptr = _bindings.errorString(code);
    if (ptr == nullptr) return 'error $code';
    return ptr.toDartString();
  }

  String? _takeLastError() {
    final ptr = _bindings.lastError(_handle);
    if (ptr == nullptr) return null;
    try {
      return ptr.toDartString();
    } finally {
      _bindings.freeString(ptr);
    }
  }

  void _pollEvents() {
    if (_disposed) return;
    while (true) {
      final ptr = _bindings.pollEvent(_handle);
      if (ptr == nullptr) return;

      final String json;
      try {
        json = ptr.toDartString();
      } finally {
        _bindings.freeString(ptr);
      }

      EventEnvelope envelope;
      try {
        final decoded = jsonDecode(json);
        envelope = EventEnvelope.fromJson(decoded as Map<String, dynamic>);
      } catch (error, stack) {
        if (!_eventController.isClosed) {
          _eventController.addError(
            FormatException('invalid event JSON from native layer: $error'),
            stack,
          );
        }
        continue;
      }

      final event = envelope.event;

      if (event is FileLoadedEvent) {
        // `file_loaded` precedes the command's `command_completed`, so the
        // media is already stored when the completion below resolves.
        _pendingLoads[event.requestId]?.fileLoaded = event;
      }

      if (event is CommandCompletedEvent) {
        final pending = _pending.remove(event.requestId);
        if (pending != null) {
          pending.timer.cancel();
          if (!pending.completer.isCompleted) {
            pending.completer.complete(event);
          }
        }

        final load = _pendingLoads.remove(event.requestId);
        if (load != null && !load.completer.isCompleted) {
          if (event.status != CommandStatus.ok) {
            load.completer.completeError(DylCommandException(event));
          } else if (load.fileLoaded != null) {
            load.completer.complete(load.fileLoaded!.media);
          } else {
            load.completer.completeError(
              DylException(
                ErrorCode.internal.codeNum,
                'load_file completed without a file_loaded event',
              ),
            );
          }
        }
      }

      if (!_eventController.isClosed) _eventController.add(envelope);
    }
  }

  /// Dispose the client and release all native resources.
  ///
  /// Pending command futures fail with a [StateError].
  void dispose() {
    if (_disposed) return;
    _disposed = true;

    _pollTimer?.cancel();
    _pollTimer = null;

    for (final pending in _pending.values) {
      pending.timer.cancel();
      if (!pending.completer.isCompleted) {
        pending.completer.completeError(
          StateError('DylClient has been disposed'),
        );
      }
    }
    _pending.clear();

    for (final load in _pendingLoads.values) {
      if (!load.completer.isCompleted) {
        load.completer.completeError(
          StateError('DylClient has been disposed'),
        );
      }
    }
    _pendingLoads.clear();

    _bindings.destroy(_handle);

    if (!_eventController.isClosed) {
      _eventController.close();
    }
  }
}
