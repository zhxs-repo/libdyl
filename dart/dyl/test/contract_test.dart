import 'dart:convert';
import 'dart:io';

import 'package:dyl/contract.dart';
import 'package:test/test.dart';

/// `dart test` 通常以包根目录为 cwd；同时兼容从仓库根目录运行。
String get _contractRoot {
  for (final candidate in [
    '../../contract',
    'contract',
    '../../../contract',
  ]) {
    if (Directory(candidate).existsSync()) return candidate;
  }
  throw StateError('找不到 contract 夹具目录');
}

dynamic _loadJson(String relative) =>
    jsonDecode(File('$_contractRoot/$relative').readAsStringSync());

List<String> _jsonFiles(String dir) {
  final files = Directory('$_contractRoot/$dir')
      .listSync()
      .whereType<File>()
      .map((file) => file.uri.pathSegments.last)
      .where((name) => name.endsWith('.json'))
      .toList()
    ..sort();
  return files;
}

void _expectCell(
  StateMatrix matrix,
  CommandOp op,
  PlayerState state, {
  required bool accepted,
  Object? to,
}) {
  final cell = matrix.cell(op, state);
  expect(cell, isNotNull, reason: '${op.wire} × ${state.wire}');
  if (accepted) {
    expect(cell!.status, MatrixStatus.accepted,
        reason: '${op.wire} × ${state.wire}');
    expect(cell.to?.toJson(), equals(to), reason: '${op.wire} × ${state.wire}');
  } else {
    expect(cell!.status, MatrixStatus.rejected,
        reason: '${op.wire} × ${state.wire}');
    expect(cell.to, isNull, reason: '${op.wire} × ${state.wire}');
  }
}

void main() {
  group('commands', () {
    test('fixtures round-trip and cover every op', () {
      final seen = <CommandOp>{};
      for (final file in _jsonFiles('commands')) {
        final raw = _loadJson('commands/$file') as Map<String, dynamic>;
        final envelope = CommandEnvelope.fromJson(raw);
        expect(envelope.schema, schemaVersion, reason: file);
        expect(envelope.requestId, isNotNull, reason: file);
        expect(envelope.toJson(), equals(raw), reason: file);
        seen.add(envelope.command.op);
      }
      expect(seen, equals(CommandOp.values.toSet()));
    });

    test('command JSON is internally tagged', () {
      const envelope = CommandEnvelope(
        requestId: 101,
        command: LoadFileCommand(r'D:\video.mp4'),
      );
      final json = envelope.toJson();
      expect(json['op'], 'load_file');
      expect(json['url'], r'D:\video.mp4');
      expect(json.containsKey('LoadFile'), isFalse);

      const seek = CommandEnvelope(
        requestId: 104,
        command: SeekCommand(positionMs: 90000, mode: SeekMode.exact),
      );
      expect(seek.command.op, CommandOp.seek);
      expect(seek.toJson()['mode'], 'exact');

      const keyframeSeek = CommandEnvelope(
        requestId: 106,
        command: SeekCommand(positionMs: 90000, mode: SeekMode.keyframe),
      );
      expect(keyframeSeek.command.op, CommandOp.seek);
      expect(keyframeSeek.toJson()['mode'], 'keyframe');
    });
  });

  group('events', () {
    test('sequences round-trip and seq is monotonic', () {
      final files = _jsonFiles('events');
      expect(files, hasLength(7));

      for (final file in files) {
        final raw = _loadJson('events/$file') as List<dynamic>;
        final events = raw
            .map((item) =>
                EventEnvelope.fromJson(item as Map<String, dynamic>))
            .toList();
        expect(events, isNotEmpty, reason: file);
        expect(events.first.seq, 1, reason: file);
        for (var i = 1; i < events.length; i++) {
          expect(events[i].seq, greaterThan(events[i - 1].seq), reason: file);
        }
        expect(events.map((event) => event.toJson()).toList(), equals(raw),
            reason: file);
      }
    });

    test('load sequence orders semantic events before completion', () {
      final raw =
          _loadJson('events/load_success_sequence.json') as List<dynamic>;
      final events = raw
          .map((item) => EventEnvelope.fromJson(item as Map<String, dynamic>))
          .toList();

      final loading = events[0].event as StateChangedEvent;
      expect(loading.from, PlayerState.idle);
      expect(loading.to, PlayerState.loading);

      final loaded = events[1].event as FileLoadedEvent;
      expect(loaded.requestId, 101);
      expect(loaded.durationMs, 120000);
      expect(loaded.media.video!.width, 1920);
      expect(loaded.media.audio!.sampleRate, 48000);
      expect(loaded.media.tracks.length, 2);
      expect(loaded.media.tracks.map((t) => t.kind),
          containsAll(<TrackKind>[TrackKind.video, TrackKind.audio]));
      expect(loaded.media.tracks.every((t) => t.selected), isTrue);

      final ready = events[2].event as StateChangedEvent;
      expect(ready.to, PlayerState.ready);

      final done = events[3].event as CommandCompletedEvent;
      expect(done.requestId, loaded.requestId);
      expect(done.status, CommandStatus.ok);
      expect(done.error, isNull);
    });

    test('seek sequence reports latest and supersedes old', () {
      final raw =
          _loadJson('events/seek_superseded_sequence.json') as List<dynamic>;
      final events = raw
          .map((item) => EventEnvelope.fromJson(item as Map<String, dynamic>))
          .toList();

      final seek = events[0].event as SeekCompletedEvent;
      expect(seek.requestId, 115);
      expect(seek.fromMs, 12340);
      expect(seek.toMs, 90000);
      expect(seek.actualMs, greaterThanOrEqualTo(seek.toMs));
      expect(seek.mode, SeekMode.exact);

      final ok = events[1].event as CommandCompletedEvent;
      expect(ok.requestId, 115);
      expect(ok.status, CommandStatus.ok);

      final old = events[2].event as CommandCompletedEvent;
      expect(old.requestId, 104);
      expect(old.status, CommandStatus.superseded);
    });

    test('track switch sequence carries the updated selection', () {
      final raw =
          _loadJson('events/track_switch_sequence.json') as List<dynamic>;
      final events = raw
          .map((item) => EventEnvelope.fromJson(item as Map<String, dynamic>))
          .toList();

      final loading = events[0].event as StateChangedEvent;
      expect(loading.from, PlayerState.paused);
      expect(loading.to, PlayerState.loading);

      final loaded = events[1].event as FileLoadedEvent;
      expect(loaded.requestId, 114);
      expect(loaded.media.audio!.codec, 'dts');
      expect(loaded.media.audio!.channels, 6);
      expect(
          loaded.media.tracks
              .where((t) => t.kind == TrackKind.audio && t.selected)
              .map((t) => t.index),
          equals([2]));
      expect(
          loaded.media.tracks
              .where((t) => t.kind == TrackKind.audio && !t.selected)
              .map((t) => t.index),
          equals([1]));

      final resumed = events[2].event as StateChangedEvent;
      expect(resumed.from, PlayerState.loading);
      expect(resumed.to, PlayerState.paused);

      final done = events[3].event as CommandCompletedEvent;
      expect(done.requestId, loaded.requestId);
      expect(done.op, CommandOp.selectTrack);
      expect(done.status, CommandStatus.ok);
    });

    test('eof and error sequences match state contract', () {
      final eofRaw = _loadJson('events/eof_sequence.json') as List<dynamic>;
      final eof = eofRaw
          .map((item) => EventEnvelope.fromJson(item as Map<String, dynamic>))
          .toList();
      final ended = eof[0].event as StateChangedEvent;
      expect(ended.from, PlayerState.playing);
      expect(ended.to, PlayerState.ended);
      expect((eof[1].event as PositionChangedEvent).positionMs, 120000);

      final errorRaw =
          _loadJson('events/pipeline_error_sequence.json') as List<dynamic>;
      final failure = errorRaw
          .map((item) => EventEnvelope.fromJson(item as Map<String, dynamic>))
          .toList();
      final errorState = failure[0].event as StateChangedEvent;
      expect(errorState.to, PlayerState.error);
      final error = failure[1].event as ErrorEvent;
      expect(error.code, ErrorCode.decode);
      expect(error.codeNum, -5);
      expect(error.fatal, isTrue);
    });

    test('settings and surface sequences are complete objects', () {
      final settingsRaw =
          _loadJson('events/settings_sequence.json') as List<dynamic>;
      final settings = settingsRaw
          .map((item) => EventEnvelope.fromJson(item as Map<String, dynamic>))
          .toList();
      final changed = settings[0].event as SettingsChangedEvent;
      expect(changed.volume, 80);
      expect(changed.muted, isTrue);
      expect(changed.decodeMode, DecodeMode.software);
      expect(changed.danmakuOpacity, 80);
      final done = settings[1].event as CommandCompletedEvent;
      expect(done.op, CommandOp.setMute);
      expect(done.requestId, 107);

      final surfaceRaw =
          _loadJson('events/surface_sequence.json') as List<dynamic>;
      final surface = surfaceRaw
          .map((item) => EventEnvelope.fromJson(item as Map<String, dynamic>))
          .toList();
      final attached = surface[0].event as SurfaceAttachedEvent;
      expect(attached.kind, SurfaceKind.windowsHwnd);
      expect(attached.width, 1280);
      final resized = surface[1].event as SurfaceResizedEvent;
      expect(resized.width, 1600);
      expect((surface[2].event as SurfaceDetachedEvent).kind,
          SurfaceKind.windowsHwnd);
    });
  });

  group('snapshot', () {
    Snapshot load(String name) {
      final raw = _loadJson('snapshot/$name.json') as Map<String, dynamic>;
      final snapshot = Snapshot.fromJson(raw);
      expect(snapshot.toJson(), equals(raw), reason: name);
      return snapshot;
    }

    test('idle has no media and no surface', () {
      final idle = load('idle');
      expect(idle.state, PlayerState.idle);
      expect(idle.source, isNull);
      expect(idle.media, isNull);
      expect(idle.surface.status, SurfaceStatus.none);
      expect(idle.surface.kind, isNull);
      expect(idle.surface.width, 0);
      expect(idle.subtitle.loaded, isFalse);
      expect(idle.danmaku.count, 0);
      expect(idle.decode.mode, DecodeMode.software);
      expect(idle.decode.backend, DecodeBackend.software);
      expect(idle.decode.device, isNull);
    });

    test('playing is mid-file with an attached surface', () {
      final playing = load('playing');
      expect(playing.state, PlayerState.playing);
      expect(playing.positionMs, lessThan(playing.durationMs));
      expect(playing.surface.status, SurfaceStatus.attached);
      expect(playing.lastError, isNull);
      expect(playing.decode.mode, DecodeMode.auto);
      expect(playing.decode.backend, DecodeBackend.hardware);
      expect(playing.decode.device, 'd3d11va');
    });

    test('ended position equals duration', () {
      final ended = load('ended');
      expect(ended.state, PlayerState.ended);
      expect(ended.positionMs, ended.durationMs);
    });

    test('error keeps last_error', () {
      final error = load('error');
      expect(error.state, PlayerState.error);
      expect(error.lastError, isNotNull);
      expect(error.lastError!.code, ErrorCode.decode);
      expect(error.lastError!.codeNum, -5);
      expect(error.lastError!.message, isNotEmpty);
    });
  });

  group('surface descriptors', () {
    test('fixtures round-trip with the right kind', () {
      const cases = {
        'windows_hwnd': SurfaceKind.windowsHwnd,
        'macos_nsview': SurfaceKind.macosNsview,
        'x11_window': SurfaceKind.x11Window,
        'wayland_surface': SurfaceKind.waylandSurface,
        'android_native_window': SurfaceKind.androidNativeWindow,
      };
      for (final entry in cases.entries) {
        final raw =
            _loadJson('surface/${entry.key}.json') as Map<String, dynamic>;
        final descriptor = SurfaceDescriptor.fromJson(raw);
        expect(descriptor.toJson(), equals(raw), reason: entry.key);
        expect(descriptor.kind, entry.value, reason: entry.key);
        expect(descriptor.width, greaterThan(0), reason: entry.key);
        expect(descriptor.height, greaterThan(0), reason: entry.key);
      }

      final x11 = SurfaceDescriptor.fromJson(
        _loadJson('surface/x11_window.json') as Map<String, dynamic>,
      );
      expect(x11, isA<X11SurfaceDescriptor>());
      final wayland = SurfaceDescriptor.fromJson(
        _loadJson('surface/wayland_surface.json') as Map<String, dynamic>,
      );
      expect(wayland, isA<WaylandSurfaceDescriptor>());
    });
  });

  group('capabilities', () {
    test('fixtures round-trip and respect presentation rules', () {
      for (final platform in ['android', 'linux', 'macos', 'windows']) {
        final raw =
            _loadJson('capabilities/$platform.json') as Map<String, dynamic>;
        final caps = Capabilities.fromJson(raw);
        expect(caps.toJson(), equals(raw), reason: platform);
        expect(caps.schema, schemaVersion);
        expect(caps.abi, AbiVersion.current);
        expect(caps.platform, platform);
        expect(caps.presentation.externalSurface, isTrue);
        expect(caps.presentation.textureSharing, isFalse);
        expect(caps.presentation.attachBeforeLoad, isTrue);
        expect(caps.presentation.embedded, isTrue);
        expect(caps.presentation.detached, platform != 'android');
        expect(caps.presentation.surfaceKinds, hasLength(1));
        expect(caps.features.overlayInCore, isTrue);
        expect(caps.features.embeddedAssStyling, isTrue);
        expect(caps.features.trackSwitching, isTrue,
            reason: 'select_track 已实现音轨与字幕轨切换');
        expect(caps.features.seekModes, [SeekMode.exact, SeekMode.keyframe]);
        expect(caps.gpuBackends, isNotEmpty);
        expect(caps.protocols, isNotEmpty,
            reason: 'protocols 是本构建实际能打开的 URL 协议集合');
        expect(caps.protocols, contains('http'));
        expect(caps.protocols, isNot(contains('file')),
            reason: '本地路径不是 URL 协议');
        // https 是否可用取决于构建时 FFmpeg 是否带 TLS 后端
        expect(caps.protocols.contains('https'),
            platform == 'android' ? isFalse : isTrue);
      }
    });

    test('ABI version packs as major<<16|minor<<8|patch', () {
      expect(AbiVersion.current.packed, 6 << 8);
      expect(AbiVersion.current.isCompatible, isTrue);
      expect(const AbiVersion(major: 6, minor: 0, patch: 0).isCompatible,
          isFalse);
    });
  });

  group('errors', () {
    test('catalog matches the code table', () {
      final raw = _loadJson('errors.json') as Map<String, dynamic>;
      final catalog = ErrorCatalog.fromJson(raw);
      expect(catalog.toJson(), equals(raw));
      expect(catalog.errors, hasLength(ErrorCode.values.length));

      for (final entry in catalog.errors) {
        expect(entry.code.codeNum, entry.codeNum, reason: entry.constant);
        expect(entry.code.constant, entry.constant, reason: entry.constant);
      }
      expect(
        catalog.errors.map((entry) => entry.code).toSet(),
        equals(ErrorCode.values.toSet()),
      );
      expect(ErrorCode.fromCodeNum(-2), ErrorCode.invalidState);
      expect(ErrorCode.fromCodeNum(4242), isNull);
    });
  });

  group('state matrix', () {
    late StateMatrix matrix;

    setUpAll(() {
      final raw = _loadJson('state_matrix.json') as Map<String, dynamic>;
      matrix = StateMatrix.fromJson(raw);
      expect(matrix.toJson(), equals(raw));
    });

    test('covers every command × state cell', () {
      expect(matrix.states, PlayerState.values);
      expect(matrix.matrix.keys.toSet(), equals(CommandOp.values.toSet()));
      expect(matrix.missingCells(), isEmpty);
      expect(matrix.reject.channel, 'command_completed');
      expect(matrix.reject.status, CommandStatus.error);
      expect(matrix.reject.error.code, ErrorCode.invalidState);
      expect(matrix.reject.error.codeNum, -2);
    });

    test('load_file is accepted everywhere and cancels in-flight loads', () {
      for (final state in PlayerState.values) {
        _expectCell(
          matrix,
          CommandOp.loadFile,
          state,
          accepted: true,
          to: ['loading', 'ready', 'error'],
        );
      }
      expect(
        matrix
            .cell(CommandOp.loadFile, PlayerState.loading)!
            .cancelsInflightLoad,
        isTrue,
      );
    });

    test('play / pause follow the state table', () {
      const playTargets = {
        PlayerState.ready: 'playing',
        PlayerState.playing: 'same',
        PlayerState.paused: 'playing',
        PlayerState.ended: 'playing',
        PlayerState.stopped: 'playing',
      };
      for (final state in PlayerState.values) {
        _expectCell(
          matrix,
          CommandOp.play,
          state,
          accepted: playTargets.containsKey(state),
          to: playTargets[state],
        );
      }

      for (final state in PlayerState.values) {
        final target = switch (state) {
          PlayerState.playing => 'paused',
          PlayerState.paused => 'same',
          _ => null,
        };
        _expectCell(
          matrix,
          CommandOp.pause,
          state,
          accepted: target != null,
          to: target,
        );
      }
    });

    test('stop is idempotent in idle/stopped and stops everywhere else', () {
      for (final state in PlayerState.values) {
        final same =
            state == PlayerState.idle || state == PlayerState.stopped;
        _expectCell(
          matrix,
          CommandOp.stop,
          state,
          accepted: true,
          to: same ? 'same' : 'stopped',
        );
      }
    });

    test('seek reopens ended/stopped at paused', () {
      for (final op in [CommandOp.seek, CommandOp.seekRelative]) {
        for (final state in PlayerState.values) {
          final target = switch (state) {
            PlayerState.ready ||
            PlayerState.playing ||
            PlayerState.paused =>
              'same',
            PlayerState.ended || PlayerState.stopped => 'paused',
            _ => null,
          };
          _expectCell(
            matrix,
            op,
            state,
            accepted: target != null,
            to: target,
          );
        }
      }
    });

    test('set_decode_mode reopens a live pipeline and persists otherwise', () {
      const reloads = {
        PlayerState.ready: 'ready',
        PlayerState.playing: 'playing',
        PlayerState.paused: 'paused',
      };
      for (final state in PlayerState.values) {
        final live = reloads[state];
        _expectCell(
          matrix,
          CommandOp.setDecodeMode,
          state,
          accepted: state != PlayerState.loading,
          to: live == null ? 'same' : ['loading', live],
        );
      }
    });

    test('settings always apply; overlay loads need media', () {
      for (final op in [
        CommandOp.setVolume,
        CommandOp.setMute,
        CommandOp.setSubtitleVisible,
        CommandOp.setDanmakuVisible,
        CommandOp.setDanmakuOpacity,
      ]) {
        for (final state in PlayerState.values) {
          _expectCell(matrix, op, state, accepted: true, to: 'same');
        }
      }

      for (final op in [CommandOp.loadSubtitle, CommandOp.loadDanmaku]) {
        for (final state in PlayerState.values) {
          final accepted = state != PlayerState.idle &&
              state != PlayerState.loading &&
              state != PlayerState.error;
          _expectCell(
            matrix,
            op,
            state,
            accepted: accepted,
            to: accepted ? 'same' : null,
          );
        }
      }
    });
  });
}
