/// Dyl Player — Dart FFI client for the C ABI.
///
/// ## Quick start
///
/// ```dart
/// import 'package:dyl/dyl.dart';
///
/// void main() async {
///   final bindings = DylBindings.load();
///   final client = DylClient(bindings);
///
///   client.events.listen((envelope) {
///     switch (envelope.event) {
///       case FileLoadedEvent(:final durationMs):
///         print('Loaded: $durationMs ms');
///       case ErrorEvent(:final message):
///         print('Error: $message');
///       default:
///         break;
///     }
///   });
///
///   await client.loadFile('/path/to/video.mp4');
///   await client.play();
///
///   // ... later
///   client.dispose();
/// }
/// ```
///
/// The client validates the ABI major version (0) at construction. Commands are
/// asynchronous; each accepted command produces one `command_completed` event,
/// which is also what the returned future completes with.
library;

export 'src/dyl_bindings.dart'
    show DylBindings, DylHandle, DylEventCallbackNative;
export 'src/dyl_client.dart'
    show DylClient, DylCommandException, DylException;
export 'src/contract/contract.dart';
