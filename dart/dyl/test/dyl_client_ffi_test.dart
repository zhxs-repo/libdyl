/// End-to-end smoke test for the Dart client against a real native library.
///
/// The normal `dart test` run skips this file. Point `DYL_FFI_LIB` at the
/// built shared library to exercise the FFI boundary:
///
///   DYL_FFI_LIB=../target/debug/dyl.dll dart test \
///       test/dyl_client_ffi_test.dart
library;

import 'dart:io';

import 'package:dyl/dyl.dart';
import 'package:test/test.dart';

void main() {
  final libPath = Platform.environment['DYL_FFI_LIB'];

  group('client against native library', () {
    late DylClient client;

    setUpAll(() {
      client = DylClient(DylBindings.load(libPath));
    });

    tearDownAll(() {
      client.dispose();
    });

    test('validates the ABI and parses capabilities', () {
      expect((client.abiVersion >> 16) & 0xffff, DylClient.requiredAbiMajor);

      final capabilities = client.capabilities;
      expect(capabilities.abi.major, DylClient.requiredAbiMajor);
      expect(capabilities.platform, Platform.operatingSystem);
      expect(capabilities.presentation.externalSurface, isTrue);
      expect(capabilities.presentation.textureSharing, isFalse);
      expect(capabilities.features.danmakuXmlEntities, isTrue);
    });

    test('snapshot starts idle', () {
      final snapshot = client.snapshot();
      expect(snapshot.schema, schemaVersion);
      expect(snapshot.state, PlayerState.idle);
      expect(snapshot.positionMs, 0);
      expect(snapshot.volume, 100);
      expect(snapshot.muted, isFalse);
      expect(snapshot.surface.status, SurfaceStatus.none);
    });

    test('state errors arrive as command_completed, not throws', () async {
      final completion = await client.play();
      expect(completion.requestId, greaterThan(0));
      expect(completion.op, CommandOp.play);
      expect(completion.status, CommandStatus.error);
      expect(completion.error?.code, ErrorCode.invalidState);
    });

    test('settings commands round-trip through the snapshot', () async {
      final mute = await client.setMute(true);
      expect(mute.status, CommandStatus.ok);
      expect(client.snapshot().muted, isTrue);

      final unmute = await client.setMute(false);
      expect(unmute.status, CommandStatus.ok);
      expect(client.snapshot().muted, isFalse);
    });

    test('load failures reject the media future', () async {
      final missing = '${Directory.systemTemp.path}'
          '/dyl-missing-${DateTime.now().microsecondsSinceEpoch}.mp4';
      await expectLater(
        client.loadFile(missing),
        throwsA(
          isA<DylCommandException>()
              .having((e) => e.completion.op, 'op', CommandOp.loadFile)
              .having(
                (e) => e.status,
                'status',
                CommandStatus.error,
              )
              .having((e) => e.error, 'error', isNotNull),
        ),
      );
    });

    test('detach is idempotent and resize needs an attached surface', () {
      client.detachSurface();

      expect(
        () => client.resizeSurface(
          const HandleSurfaceDescriptor(
            kind: SurfaceKind.windowsHwnd,
            handle: '0x1',
            width: 640,
            height: 360,
            scaleFactor: 1.0,
          ),
        ),
        throwsA(isA<DylException>()),
      );
    });

    test('loadFile resolves with the media discovered by demux', () async {
      final video = Platform.environment['DYL_TEST_VIDEO'];
      if (video == null) {
        markTestSkipped('set DYL_TEST_VIDEO to load a real file');
        return;
      }

      final media = await client.loadFile(video);
      expect(media.video?.codec, isNotEmpty);
      expect(media.video?.width, greaterThan(0));
      expect(media.video?.height, greaterThan(0));
      expect(client.snapshot().state, PlayerState.ready);
    });
  },
      skip: libPath == null
          ? 'set DYL_FFI_LIB to run FFI smoke tests'
          : false);
}
