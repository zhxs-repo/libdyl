/// Dyl Player Android example.
///
/// Demonstrates the full playback path: load a video (load and surface are decoupled),
/// play it on a SurfaceView driven by the Rust renderer, then overlay ASS
/// subtitles and danmaku on top.
///
/// The video is always chosen with the in-app file picker; nothing loads or
/// plays at startup. Subtitles and danmaku may be pre-set on the command line
/// and are attached once a video is picked (handy for automated runs):
///
///   flutter run --dart-define=DYL_SUBTITLE=/sdcard/Movies/001.ass \
///               --dart-define=DYL_DANMAKU=/sdcard/Movies/danmaku.xml
///
/// Those paths must be readable by the app: on Android put the files in its
/// private storage (no runtime permission needed), e.g.
/// /data/user/0/dev.dylplayer.dyl_player_example/files/001.ass.
library;

import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart' show LogicalKeyboardKey;
import 'package:dyl_flutter/dyl_flutter.dart';

void main() {
  runApp(const DylExampleApp());
}

class DylExampleApp extends StatelessWidget {
  const DylExampleApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Dyl Player',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: const PlayerPage(),
    );
  }
}

class PlayerPage extends StatefulWidget {
  const PlayerPage({super.key});

  @override
  State<PlayerPage> createState() => _PlayerPageState();
}

class _PlayerPageState extends State<PlayerPage> {
  /// 来自 --dart-define 的叠加层路径（便于自动化验证）。视频本身一律由选择器
  /// 选：启动不加载也不播放，否则每次打开都在放那个测试文件。
  static const _subtitlePath = String.fromEnvironment('DYL_SUBTITLE');
  static const _danmakuPath = String.fromEnvironment('DYL_DANMAKU');

  /// 会改变画面内容的命令：完成之后必须补一次 present（见 [_onEvent]）。
  static const _overlayOps = {
    CommandOp.setSubtitleVisible,
    CommandOp.loadSubtitle,
    CommandOp.setDanmakuVisible,
    CommandOp.setDanmakuOpacity,
    CommandOp.loadDanmaku,
  };

  String _videoPath = '';

  /// 正在拖动进度条时的临时位置（毫秒）；null = 没在拖动
  ///
  /// 非 null 时进度条和时间标签都用它，不跟播放位置同步 —— 否则会和
  /// [_ticker] 抢控制权，滑块在手指底下反复回弹。
  double? _scrubbing;

  DylClient? _client;
  String? _error;
  String _status = 'initializing';
  PlayerState _state = PlayerState.idle;
  Duration _position = Duration.zero;
  Duration _duration = Duration.zero;
  bool _subtitleOn = false;
  bool _danmakuOn = false;
  bool _picking = false;

  /// 沉浸模式：视频 view 铺满整个窗口（无 AppBar、无控制区）。
  ///
  /// 内核的弹幕画布就是 attach 的 surface，所以 surface 多大弹幕就铺多大；
  /// 退出只能靠键盘 —— Windows 上子 HWND 盖在 Flutter 视图上面，鼠标点不到
  /// Flutter 的按钮。
  bool _immersive = false;

  /// 解码设置与实际后端；快照的 `decode` 是真相，事件只用来及时刷新。
  DecodeMode _decodeMode = DecodeMode.software;
  DecodeBackend _decodeBackend = DecodeBackend.software;
  String? _decodeDevice;

  String get _decodeLabel {
    if (_decodeBackend == DecodeBackend.hardware) {
      final device = _decodeDevice;
      return device == null ? 'hardware' : 'hardware $device';
    }
    return 'software';
  }

  /// 轨道表：只来自 `media.tracks`（`file_loaded` 与快照两处都会刷新它）。
  List<TrackInfo> _tracks = const [];

  String _videoLabel = '';
  String _subtitleLabel = '';
  String _danmakuLabel = '';
  Timer? _ticker;

  /// AppBar 上的字幕/弹幕开关该不该出现：有可开关的东西才给按钮，
  /// 但**不看当前可见性**——隐藏中也要留着一键再打开。
  bool get _hasSubtitleSource =>
      _subtitleLabel.isNotEmpty ||
      _tracks.any((t) => t.kind == TrackKind.subtitle && t.selected);

  bool get _hasDanmakuSource => _danmakuLabel.isNotEmpty;

  @override
  void initState() {
    super.initState();
    _init();
  }

  void _init() {
    try {
      final client = DylClient(DylBindings.load());
      _client = client;
      client.events.listen(_onEvent);
      _startTicker();
      setState(() => _status = 'ready');
    } catch (e) {
      setState(() {
        _error = 'Failed to load libdyl: $e';
        _status = 'error';
      });
    }
  }

  void _onEvent(EventEnvelope envelope) {
    if (!mounted) return;

    var autoPlay = false;
    // 暂停/EOF 时内核不再自动出帧（静态画面抑制），所以只改叠加层状态/内容的
    // 命令不会自己反映到画面上——必须补一次 present，否则「关掉弹幕」后屏幕上的
    // 弹幕一条都不少，看着就像按钮没生效。
    var repaintFrozen = false;
    setState(() {
      switch (envelope.event) {
        case FileLoadedEvent(:final durationMs, :final media):
          _duration = Duration(milliseconds: durationMs);
          _position = Duration.zero;
          // 换轨也重发 file_loaded：只有真正的首次加载才自动播，否则会把内核
          // 「落回切换前的状态」这条语义覆盖成强制播放。
          autoPlay = _tracks.isEmpty;
          _tracks = media.tracks;
          _status =
              'loaded ${media.video?.codec ?? 'no video'} / '
              '${media.audio?.codec ?? 'no audio'}  ${_format(_duration)}';
        case StateChangedEvent(:final to):
          _state = to;
          switch (to) {
            case PlayerState.loading:
              _status = 'loading';
            case PlayerState.playing:
              _status = 'playing';
            case PlayerState.paused:
              _status = 'paused';
            case PlayerState.ended:
              _status = 'file ended';
            case PlayerState.stopped:
              _status = 'stopped';
            case PlayerState.error:
              _status = 'error';
            case PlayerState.idle:
            case PlayerState.ready:
              break;
          }
        case SeekCompletedEvent(:final actualMs):
          _position = Duration(milliseconds: actualMs);
        case PositionChangedEvent(:final positionMs):
          if (_scrubbing == null) {
            _position = Duration(milliseconds: positionMs);
          }
        case SettingsChangedEvent(
          :final decodeMode,
          :final subtitleVisible,
          :final danmakuVisible,
        ):
          _decodeMode = decodeMode;
          _subtitleOn = subtitleVisible;
          _danmakuOn = danmakuVisible;
        case CommandCompletedEvent(:final op, :final status, :final error):
          if (status == CommandStatus.error) {
            _error =
                '${op.wire} failed: '
                '${error?.message ?? error?.code.wire ?? 'unknown error'}';
          }
          // 等命令落地再补 present：`redrawSurface` 不排队，抢在控制线程之前画
          // 出来的还是旧状态。音量/播放控制那些不改画面的 op 不在表里。
          repaintFrozen =
              status == CommandStatus.ok && _overlayOps.contains(op);
        case ErrorEvent(:final message):
          _error = message;
          _status = 'error';
        case SurfaceAttachedEvent(:final width, :final height):
          if (_state == PlayerState.idle) {
            _status = 'surface attached ${width}x$height';
          }
        default:
          break;
      }
    });

    if (autoPlay) {
      // 内核的 state matrix 在 idle/loading 直接拒 load_subtitle|danmaku，所以
      // 命令行给的叠加层只能等 `file_loaded` 之后再挂。
      _loadOverlays();
      _fire(_client!.play());
    }
    if (repaintFrozen) {
      final c = _client;
      // redrawSurface 在没有 surface 时是 no-op，播放中多一次 present 也无害。
      if (c != null) _fire(Future<void>(() => c.redrawSurface()));
    }
  }

  /// 发送命令并把失败（同步拒绝 / 超时 / dispose）落到错误栏。
  ///
  /// 命令的异步结果还会以 `command_completed` 事件到达，业务状态在
  /// [_onEvent] 里处理；这里只兜底 throw 的情况。
  void _fire(Future<Object?> future) {
    unawaited(
      future.then<void>(
        (_) {},
        onError: (Object error) {
          if (!mounted) return;
          setState(() => _error = '$error');
        },
      ),
    );
  }

  void _loadOverlays() {
    final client = _client;
    if (client == null) return;
    if (_subtitlePath.isNotEmpty && File(_subtitlePath).existsSync()) {
      _fire(client.loadSubtitle(_subtitlePath));
      setState(() {
        _subtitleOn = true;
        _subtitleLabel = _subtitlePath.split('/').last;
      });
    }
    if (_danmakuPath.isNotEmpty && File(_danmakuPath).existsSync()) {
      _fire(client.loadDanmaku(_danmakuPath));
      setState(() {
        _danmakuOn = true;
        _danmakuLabel = _danmakuPath.split('/').last;
      });
    }
  }

  /// 通过系统文件选择器选视频。
  ///
  /// 选中的文件由插件复制到 app 私有目录，回传真实路径 —— `content://` URI
  /// 过不了 Rust 侧的本地路径检查。
  Future<void> _pickVideo() async {
    if (_picking) return;
    setState(() => _picking = true);
    try {
      final picked = await DylFilePicker.pickFile(mimeTypes: const ['video/*']);
      if (picked == null || !mounted) return; // 用户取消
      setState(() {
        _videoPath = picked.path;
        _videoLabel = picked.name;
        _error = null;
      });
      // load 不依赖 surface：选完就可以开始加载。
      _load();
    } catch (e) {
      // PlatformException = 原生侧报错；TimeoutException = 回包失联（见 pickFile）。
      if (mounted) setState(() => _error = 'pick video failed: $e');
    } finally {
      if (mounted) setState(() => _picking = false);
    }
  }

  Future<void> _pickSubtitle() async {
    try {
      final picked = await DylFilePicker.pickSubtitle();
      if (picked == null || !mounted) return;
      final client = _client;
      if (client == null) return;
      _fire(client.loadSubtitle(picked.path));
      setState(() {
        _subtitleOn = true;
        _subtitleLabel = picked.name;
      });
    } on FormatException catch (e) {
      setState(() => _error = e.message);
    } catch (e) {
      setState(() => _error = 'pick subtitle failed: $e');
    }
  }

  Future<void> _pickDanmaku() async {
    try {
      final picked = await DylFilePicker.pickDanmaku();
      if (picked == null || !mounted) return;
      final client = _client;
      if (client == null) return;
      _fire(client.loadDanmaku(picked.path));
      setState(() {
        _danmakuOn = true;
        _danmakuLabel = picked.name;
      });
    } on FormatException catch (e) {
      setState(() => _error = e.message);
    } catch (e) {
      setState(() => _error = 'pick danmaku failed: $e');
    }
  }

  /// 进度条滑块当前应显示的值（毫秒）
  double get _sliderValue {
    final max = _duration.inMilliseconds > 0 ? _duration.inMilliseconds : 1;
    if (_scrubbing != null) return _scrubbing!.clamp(0.0, max.toDouble());
    return _position.inMilliseconds.clamp(0, max).toDouble();
  }

  /// 进度条左侧的时间标签：拖动中显示目标位置，否则显示播放位置
  String get _positionLabel {
    if (_scrubbing != null) {
      return _format(Duration(milliseconds: _scrubbing!.round()));
    }
    return _format(_position);
  }

  void _seekTo(double ms) {
    final client = _client;
    if (client == null) return;
    _fire(client.seek(Duration(milliseconds: ms.round())));
    // 立即把本地位置推到目标值：seek 是异步生效的，不预置的话松手的一瞬间
    // 进度条会先弹回旧位置再跳过去。
    setState(() => _position = Duration(milliseconds: ms.round()));
  }

  void _load() {
    final client = _client;
    if (client == null) return;
    final path = _videoPath;
    if (path.isEmpty) {
      setState(() {
        _error = 'Pick a video file first';
        _status = 'pick a video to start';
      });
      return;
    }
    _fire(client.loadFile(path));
    setState(() {
      _error = null;
      // 新文件从 0 开始，别留着上一部的进度
      _position = Duration.zero;
      _scrubbing = null;
      _tracks = const [];
      _status = 'loading ${_videoLabel.isEmpty ? path : _videoLabel}';
    });
  }

  void _togglePlayPause() {
    final client = _client;
    if (client == null) return;
    if (_state == PlayerState.playing) {
      _fire(client.pause());
    } else {
      _fire(client.play());
    }
  }

  void _stop() {
    final client = _client;
    if (client == null) return;
    _fire(client.stop());
  }

  /// 定期用快照校正 UI。事件只作通知，快照才是真相。
  void _startTicker() {
    _ticker?.cancel();
    _ticker = Timer.periodic(const Duration(milliseconds: 200), (_) {
      final client = _client;
      if (client == null || !mounted || client.isDisposed) return;
      if (_scrubbing != null) return;

      final Snapshot snapshot;
      try {
        snapshot = client.snapshot();
      } catch (_) {
        return;
      }

      final position = Duration(milliseconds: snapshot.positionMs);
      final duration = Duration(milliseconds: snapshot.durationMs);
      final positionChanged =
          (position.inMilliseconds - _position.inMilliseconds).abs() >= 100;
      final decodeChanged =
          snapshot.decode.mode != _decodeMode ||
          snapshot.decode.backend != _decodeBackend ||
          snapshot.decode.device != _decodeDevice;
      final changed =
          positionChanged ||
          duration != _duration ||
          snapshot.state != _state ||
          decodeChanged;
      if (!changed) return;

      setState(() {
        _position = position;
        _duration = duration;
        _state = snapshot.state;
        _decodeMode = snapshot.decode.mode;
        _decodeBackend = snapshot.decode.backend;
        _decodeDevice = snapshot.decode.device;
      });
    });
  }

  /// Surface 就绪只影响 UI 状态；load/play 不再等它。
  void _onSurfaceReady() {
    if (!mounted) return;
    if (_state == PlayerState.idle) {
      setState(() => _status = 'surface ready');
    }
  }

  @override
  void dispose() {
    _ticker?.cancel();
    _client?.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final client = _client;
    final video = client == null
        ? const Center(child: CircularProgressIndicator())
        : DylVideoView(
            client: client,
            onSurfaceReady: _onSurfaceReady,
            backgroundColor: Colors.black,
            placeholder: const Center(
              child: Text(
                'Dyl video rendering requires Android or Windows',
                style: TextStyle(color: Colors.white38),
              ),
            ),
          );

    return CallbackShortcuts(
      bindings: {
        const SingleActivator(LogicalKeyboardKey.keyF): () =>
            setState(() => _immersive = !_immersive),
        // 整窗模式下 surface 盖住所有 Flutter 控件，退出只能靠键盘。字母键会被
        // 中文输入法接去做拼音合成（实测 F 打不开也关不掉），Esc 不经输入法，
        // 所以它才是这个模式下真正可用的退出键。
        const SingleActivator(LogicalKeyboardKey.escape): () {
          if (_immersive) setState(() => _immersive = false);
        },
      },
      child: Focus(
        autofocus: true,
        child: Scaffold(
          backgroundColor: _immersive ? Colors.black : null,
          // 整窗模式隐藏 AppBar：留着它会占掉一条，surface 就不是整窗了。
          appBar: _immersive
              ? null
              : AppBar(
                  title: const Text('Dyl Player'),
                  // AppBar 这一条**不用 tooltip**：视频是 Flutter 视图之上的子
                  // HWND，Flutter 画进视频矩形里的一切都会被盖住，而 tooltip 只会
                  // 出现在锚点的正上或正下——往上出不了 Flutter 视图，往下正好压在
                  // surface 上（实测两头都看不见）。这一条只有 56px，也塞不下气泡，
                  // 所以开关状态一律写进按钮文字，不靠悬停。
                  actions: [
                    TextButton.icon(
                      onPressed: () => setState(() => _immersive = true),
                      icon: const Icon(Icons.fullscreen, size: 18),
                      label: const Text('Fill'),
                    ),
                    // 开关按钮只要挂了叠加层就**常驻**：早先用 `if (_subtitleOn)`
                    // 包住，一隐藏按钮就跟着消失，再也没有第二次点击的机会。
                    if (_hasSubtitleSource)
                      TextButton.icon(
                        onPressed: () {
                          final next = !_subtitleOn;
                          setState(() => _subtitleOn = next);
                          final c = _client;
                          if (c != null) {
                            _fire(c.setSubtitleVisible(next));
                          }
                        },
                        icon: Icon(
                          _subtitleOn ? Icons.subtitles : Icons.subtitles_off,
                          size: 18,
                        ),
                        label: Text(_subtitleOn ? 'Subs on' : 'Subs off'),
                      ),
                    if (_hasDanmakuSource)
                      TextButton.icon(
                        onPressed: () {
                          final next = !_danmakuOn;
                          setState(() => _danmakuOn = next);
                          final c = _client;
                          if (c != null) {
                            _fire(c.setDanmakuVisible(next));
                          }
                        },
                        icon: Icon(
                          _danmakuOn
                              ? Icons.chat_bubble
                              : Icons.chat_bubble_outline,
                          size: 18,
                        ),
                        label: Text(_danmakuOn ? 'Danmaku on' : 'Danmaku off'),
                      ),
                  ],
                ),
          // `video` 必须**永远待在同一层级的同一槽位**：写成 `body: video` 会换掉
          // 祖先链，State 被 dispose → detach + 新建 surface → 内核 EOF 时新
          // renderer 手里没有帧，画面直接黑。槽位不变就只是 resizeSurface，
          // 冻结帧和弹幕都保得住。
          body: LayoutBuilder(
            builder: (context, constraints) {
              final controls = _buildControls(client);
              final sideBySide = !_immersive && constraints.maxHeight < 500;
              final panelWidth = (constraints.maxWidth * 0.4).clamp(
                200.0,
                360.0,
              );
              return Column(
                children: [
                  Expanded(
                    child: Row(
                      children: [
                        Expanded(child: video),
                        if (sideBySide)
                          SizedBox(
                            width: panelWidth,
                            child: SingleChildScrollView(child: controls),
                          ),
                      ],
                    ),
                  ),
                  if (!sideBySide && !_immersive) controls,
                ],
              );
            },
          ),
        ),
      ),
    );
  }

  Widget _buildControls(DylClient? client) {
    final playing = _state == PlayerState.playing;
    return SafeArea(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(_status, style: Theme.of(context).textTheme.bodySmall),
            Text(
              'decode: $_decodeLabel',
              style: Theme.of(context).textTheme.bodySmall,
            ),
            // 整窗模式下子 HWND 盖住整块 Flutter 视图：控件就算画出来也会被视频
            // 表面盖掉，所以退出只能靠键盘（Esc 不经输入法，F 会被中文拼音吃掉）。
            Text(
              'F fills the window (danmaku span it), Esc exits',
              style: Theme.of(context).textTheme.bodySmall,
            ),
            if (_error != null)
              Text(
                _error!,
                style: const TextStyle(color: Colors.redAccent),
                maxLines: 3,
                overflow: TextOverflow.ellipsis,
              ),
            const SizedBox(height: 8),
            Row(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                FilledButton.icon(
                  onPressed: client == null || _picking ? null : _pickVideo,
                  icon: const Icon(Icons.folder_open),
                  label: Text(
                    _videoLabel.isEmpty ? 'Video' : _short(_videoLabel),
                  ),
                ),
                const SizedBox(width: 8),
                IconButton.filled(
                  tooltip: 'play / pause',
                  onPressed: client == null ? null : _togglePlayPause,
                  icon: Icon(playing ? Icons.pause : Icons.play_arrow),
                ),
                const SizedBox(width: 8),
                IconButton.filled(
                  tooltip: 'stop',
                  onPressed: client == null ? null : _stop,
                  icon: const Icon(Icons.stop),
                ),
              ],
            ),
            const SizedBox(height: 8),
            // 字幕 / 弹幕：各选一个文件。选完立即 load，播放中也能换。
            Row(
              children: [
                Expanded(
                  child: _overlayPicker(
                    icon: Icons.subtitles_outlined,
                    label: _subtitleLabel.isEmpty
                        ? 'Subtitle'
                        : _short(_subtitleLabel),
                    onPressed: client != null && _canLoadOverlay
                        ? _pickSubtitle
                        : null,
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: _overlayPicker(
                    icon: Icons.chat_bubble_outline,
                    label: _danmakuLabel.isEmpty
                        ? 'Danmaku'
                        : _short(_danmakuLabel),
                    onPressed: client != null && _canLoadOverlay
                        ? _pickDanmaku
                        : null,
                  ),
                ),
              ],
            ),
            const SizedBox(height: 8),
            ?_decodeMenu(client),
            ?_trackMenuRow(),
            Row(
              children: [
                Text(_positionLabel),
                const Spacer(),
                Text(_format(_duration)),
              ],
            ),
            // 拖动时用 _scrubbing 里的值显示，**不**跟着 _position 回弹：
            // 定时器每 200ms 回报一次真实播放位置，seek 生效前它一直停在旧值，
            // 滑块就会在手指拖动的过程中一下一下跳回去。
            Slider(
              value: _sliderValue,
              max: _duration.inMilliseconds > 0
                  ? _duration.inMilliseconds.toDouble()
                  : 1,
              // 松手才真正 seek：onChanged 每像素都会触发，逐像素发 seek 会把
              // demuxer 打爆（一次拖动 = 几十次 seek）。
              onChangeStart: client == null
                  ? null
                  : (v) => setState(() => _scrubbing = v),
              onChanged: client == null
                  ? null
                  : (v) => setState(() => _scrubbing = v),
              onChangeEnd: client == null
                  ? null
                  : (v) {
                      setState(() => _scrubbing = null);
                      _seekTo(v);
                    },
            ),
          ],
        ),
      ),
    );
  }

  /// 内核的 state matrix 只在拿到媒体信息之后才接受 load_subtitle / load_danmaku
  /// （idle、loading、error 一律 `invalid_state`），所以没选中视频之前这两个按钮
  /// 不给点，而不是点完再吃一条错误。
  bool get _canLoadOverlay => switch (_state) {
    PlayerState.ready ||
    PlayerState.playing ||
    PlayerState.paused ||
    PlayerState.ended ||
    PlayerState.stopped => true,
    _ => false,
  };

  /// 控件条里的叠加层选择按钮。禁用时挂 tooltip 说明原因——这一条在视频 surface
  /// **之下**，气泡朝下画得出来，AppBar 那一条才画不出来。
  Widget _overlayPicker({
    required IconData icon,
    required String label,
    required VoidCallback? onPressed,
  }) {
    final button = OutlinedButton.icon(
      onPressed: onPressed,
      icon: Icon(icon, size: 18),
      label: Text(label, overflow: TextOverflow.ellipsis),
    );
    if (onPressed != null) return button;
    return Tooltip(message: 'pick a video first', child: button);
  }

  /// 解码方式下拉。`hw_decode` 能力位为假时整个菜单不出现：内核会在同步
  /// 校验里拒绝 `auto`（`not_supported`），列出来等于撒谎。
  Widget? _decodeMenu(DylClient? client) {
    if (client == null || !client.capabilities.features.hwDecode) return null;
    return Row(
      children: [
        Text('Decode', style: Theme.of(context).textTheme.bodySmall),
        const SizedBox(width: 6),
        Expanded(
          child: DropdownButton<DecodeMode>(
            isExpanded: true,
            value: _decodeMode,
            // 换解码方式要重开管线并经过 loading，中途再点只会被矩阵拒绝。
            onChanged: _state == PlayerState.loading
                ? null
                : (mode) {
                    if (mode == null) return;
                    setState(() => _error = null);
                    _fire(client.setDecodeMode(mode));
                  },
            items: const [
              DropdownMenuItem(
                value: DecodeMode.software,
                child: Text('Software'),
              ),
              DropdownMenuItem(
                value: DecodeMode.auto,
                child: Text('Hardware (auto)'),
              ),
            ],
          ),
        ),
      ],
    );
  }

  /// 音轨与字幕轨下拉，数据源只有 `media.tracks`。
  ///
  /// 只有一条轨的类别不给菜单；**没有视频轨菜单** —— 内核对 `kind: video`
  /// 同步报 `not_supported`，列出来等于撒谎。
  Widget? _trackMenuRow() {
    final menus = <Widget>[
      ?_trackMenu(TrackKind.audio, 'Audio'),
      ?_trackMenu(TrackKind.subtitle, 'Subs'),
    ];
    if (menus.isEmpty) return null;
    return Row(
      children: [
        for (var i = 0; i < menus.length; i++) ...[
          if (i > 0) const SizedBox(width: 8),
          Expanded(child: menus[i]),
        ],
      ],
    );
  }

  Widget? _trackMenu(TrackKind kind, String label) {
    final tracks = _tracks
        .where((track) => track.kind == kind)
        .toList(growable: false);
    if (tracks.length < 2) return null;
    var selectedIndex = -1;
    for (final track in tracks) {
      if (track.selected) selectedIndex = track.index;
    }
    return Row(
      children: [
        Text(label, style: Theme.of(context).textTheme.bodySmall),
        const SizedBox(width: 6),
        Expanded(
          child: DropdownButton<int>(
            isExpanded: true,
            value: selectedIndex < 0 ? null : selectedIndex,
            hint: const Text('none'),
            // 换轨要重开解码管线并经过 loading，中途再点只会拿到 superseded/busy。
            onChanged: _state == PlayerState.loading
                ? null
                : (index) => _selectTrack(kind, index),
            items: [
              for (final track in tracks)
                DropdownMenuItem(
                  value: track.index,
                  // 位图字幕轨（PGS/DVD/DVB…）内核不解码：列出来，但选不了。
                  enabled: _trackSwitchable(track),
                  child: Text(
                    _trackLabel(track),
                    overflow: TextOverflow.ellipsis,
                  ),
                ),
            ],
          ),
        ),
      ],
    );
  }

  void _selectTrack(TrackKind kind, int? index) {
    final client = _client;
    if (client == null || index == null) return;
    final future = client.selectTrack(kind: kind, index: index);
    // 上一轨失败留下的红字不该压在一次成功的切换上；切换失败由 _fire 重新写入。
    setState(() => _error = null);
    _fire(future);
  }

  /// 内核的文本字幕门（`SubtitleCodecId::is_text`）对认不出的 codec 统一报
  /// `unknown`，所以这里按 codec 标签判断，位图字幕一律不可切。
  static bool _trackSwitchable(TrackInfo track) => switch (track.kind) {
    TrackKind.video => false,
    TrackKind.audio => true,
    TrackKind.subtitle => _textSubtitleCodecs.contains(track.codec),
  };

  static const _textSubtitleCodecs = {
    'ass',
    'ssa',
    'subrip',
    'srt',
    'mov_text',
  };

  static String _trackLabel(TrackInfo track) {
    final parts = <String>['#${track.index}', track.codec];
    final language = track.language;
    if (language != null && language.isNotEmpty) parts.add(language);
    if (track.defaultTrack) parts.add('default');
    final meta = parts.join(' · ');
    final title = track.title?.trim();
    return title == null || title.isEmpty ? meta : '$title ($meta)';
  }

  /// 按钮上只放得下几个字符，文件名太长就截断。
  static String _short(String name, [int max = 14]) {
    if (name.length <= max) return name;
    final dot = name.lastIndexOf('.');
    final ext = dot > 0 ? name.substring(dot) : '';
    return '${name.substring(0, max - ext.length - 1)}…$ext';
  }

  static String _format(Duration d) {
    final m = d.inMinutes;
    final s = d.inSeconds % 60;
    final ms = (d.inMilliseconds % 1000) ~/ 100;
    return '$m:${s.toString().padLeft(2, '0')}.$ms';
  }
}
