/// Dyl Flutter Plugin — Android SurfaceView integration for Dyl Player.
///
/// Provides [DylVideoView] widget that renders video via Android
/// `SurfaceView` + `ANativeWindow` FFI, and handles Activity lifecycle
/// (pause/resume surface on background/foreground).
///
/// ## Usage
///
/// ```dart
/// import 'package:dyl_flutter/dyl_flutter.dart';
///
/// class PlayerScreen extends StatefulWidget { ... }
///
/// class _PlayerScreenState extends State<PlayerScreen> {
///   late final DylClient client;
///
///   @override
///   void initState() {
///     super.initState();
///     client = DylClient(DylBindings.load());
///   }
///
///   @override
///   Widget build(BuildContext context) {
///     return DylVideoView(client: client);
///   }
/// }
/// ```
library;

export 'package:dyl/dyl.dart';

export 'src/dyl_file_picker.dart' show DylFilePicker, PickedFile;
export 'src/dyl_video_view.dart' show DylVideoView;
