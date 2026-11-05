part of 'contract.dart';

/// 播放器状态。
enum PlayerState {
  idle('idle'),
  loading('loading'),
  ready('ready'),
  playing('playing'),
  paused('paused'),
  ended('ended'),
  stopped('stopped'),
  error('error');

  const PlayerState(this.wire);

  final String wire;

  static PlayerState fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'player state');
}

/// `state_changed` 的原因。
enum StateChangeReason {
  load('load'),
  play('play'),
  pause('pause'),
  stop('stop'),
  seek('seek'),
  eof('eof'),
  error('error'),
  restart('restart');

  const StateChangeReason(this.wire);

  final String wire;

  static StateChangeReason fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'state change reason');
}

/// Seek 模式。
enum SeekMode {
  exact('exact'),
  keyframe('keyframe');

  const SeekMode(this.wire);

  final String wire;

  static SeekMode fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'seek mode');
}

/// 解码方式。
enum DecodeMode {
  software('software'),
  auto('auto');

  const DecodeMode(this.wire);

  final String wire;

  static DecodeMode fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'decode mode');
}

/// 实际生效的解码后端。
enum DecodeBackend {
  software('software'),
  hardware('hardware');

  const DecodeBackend(this.wire);

  final String wire;

  static DecodeBackend fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'decode backend');
}

/// 命令 `op`。
enum CommandOp {
  loadFile('load_file'),
  play('play'),
  pause('pause'),
  stop('stop'),
  seek('seek'),
  seekRelative('seek_relative'),
  setVolume('set_volume'),
  setMute('set_mute'),
  setSubtitleVisible('set_subtitle_visible'),
  loadSubtitle('load_subtitle'),
  setDanmakuVisible('set_danmaku_visible'),
  setDanmakuOpacity('set_danmaku_opacity'),
  loadDanmaku('load_danmaku'),
  selectTrack('select_track'),
  setDecodeMode('set_decode_mode');

  const CommandOp(this.wire);

  final String wire;

  static CommandOp fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'command op');
}

/// `command_completed.status`。
enum CommandStatus {
  ok('ok'),
  error('error'),
  cancelled('cancelled'),
  superseded('superseded');

  const CommandStatus(this.wire);

  final String wire;

  static CommandStatus fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'command status');
}

/// Surface 类型。
enum SurfaceKind {
  windowsHwnd('windows_hwnd'),
  macosNsview('macos_nsview'),
  x11Window('x11_window'),
  waylandSurface('wayland_surface'),
  androidNativeWindow('android_native_window');

  const SurfaceKind(this.wire);

  final String wire;

  static SurfaceKind fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'surface kind');
}

/// 快照中的 Surface 状态。
enum SurfaceStatus {
  none('none'),
  attached('attached'),
  detached('detached'),
  lost('lost'),
  error('error');

  const SurfaceStatus(this.wire);

  final String wire;

  static SurfaceStatus fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'surface status');
}

/// 轨道种类。
enum TrackKind {
  video('video'),
  audio('audio'),
  subtitle('subtitle');

  const TrackKind(this.wire);

  final String wire;

  static TrackKind fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'track kind');
}

/// 错误码：字符串码、数字码与 C 常量名同源。
enum ErrorCode {
  ok(0, 'ok', 'DYL_OK'),
  invalidArgument(-1, 'invalid_argument', 'DYL_ERR_INVALID_ARGUMENT'),
  invalidState(-2, 'invalid_state', 'DYL_ERR_INVALID_STATE'),
  fileNotFound(-3, 'file_not_found', 'DYL_ERR_FILE_NOT_FOUND'),
  demux(-4, 'demux', 'DYL_ERR_DEMUX'),
  decode(-5, 'decode', 'DYL_ERR_DECODE'),
  render(-6, 'render', 'DYL_ERR_RENDER'),
  audio(-7, 'audio', 'DYL_ERR_AUDIO'),
  pipeline(-8, 'pipeline', 'DYL_ERR_PIPELINE'),
  notSupported(-9, 'not_supported', 'DYL_ERR_NOT_SUPPORTED'),
  surface(-10, 'surface', 'DYL_ERR_SURFACE'),
  busy(-11, 'busy', 'DYL_ERR_BUSY'),
  timeout(-12, 'timeout', 'DYL_ERR_TIMEOUT'),
  io(-13, 'io', 'DYL_ERR_IO'),
  cancelled(-14, 'cancelled', 'DYL_ERR_CANCELLED'),
  internal(-99, 'internal', 'DYL_ERR_INTERNAL');

  const ErrorCode(this.codeNum, this.wire, this.constant);

  final int codeNum;
  final String wire;
  final String constant;

  static ErrorCode fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'error code');

  static ErrorCode? fromCodeNum(int codeNum) {
    for (final value in values) {
      if (value.codeNum == codeNum) return value;
    }
    return null;
  }
}

/// 错误对象。`message` / `fatal` 按上下文出现：
/// `command_completed.error` 只有 code + code_num；快照 last_error 带 message；
/// `error` 事件带 message + fatal。
class ErrorInfo {
  const ErrorInfo({
    required this.code,
    required this.codeNum,
    this.message,
    this.fatal,
  });

  factory ErrorInfo.fromJson(Map<String, dynamic> json) => ErrorInfo(
        code: ErrorCode.fromWire(json['code'] as String),
        codeNum: _int(json['code_num']),
        message: json['message'] as String?,
        fatal: json['fatal'] as bool?,
      );

  final ErrorCode code;
  final int codeNum;
  final String? message;
  final bool? fatal;

  Map<String, dynamic> toJson() => {
        'code': code.wire,
        'code_num': codeNum,
        if (message != null) 'message': message,
        if (fatal != null) 'fatal': fatal,
      };
}

/// C ABI 版本。
class AbiVersion {
  const AbiVersion({
    required this.major,
    required this.minor,
    required this.patch,
  });

  factory AbiVersion.fromJson(Map<String, dynamic> json) => AbiVersion(
        major: _int(json['major']),
        minor: _int(json['minor']),
        patch: _int(json['patch']),
      );

  static const current = AbiVersion(major: 0, minor: 6, patch: 0);

  final int major;
  final int minor;
  final int patch;

  int get packed => (major << 16) | (minor << 8) | patch;

  bool get isCompatible => major == current.major;

  Map<String, dynamic> toJson() => {
        'major': major,
        'minor': minor,
        'patch': patch,
      };

  @override
  bool operator ==(Object other) =>
      other is AbiVersion &&
      other.major == major &&
      other.minor == minor &&
      other.patch == patch;

  @override
  int get hashCode => Object.hash(major, minor, patch);
}

/// 媒体信息。
class MediaInfo {
  const MediaInfo({this.video, this.audio, this.tracks = const []});

  factory MediaInfo.fromJson(Map<String, dynamic> json) => MediaInfo(
        video: json['video'] == null
            ? null
            : VideoStreamInfo.fromJson(
                json['video'] as Map<String, dynamic>,
              ),
        audio: json['audio'] == null
            ? null
            : AudioStreamInfo.fromJson(
                json['audio'] as Map<String, dynamic>,
              ),
        tracks: (json['tracks'] as List<dynamic>? ?? const [])
            .cast<Map<String, dynamic>>()
            .map(TrackInfo.fromJson)
            .toList(),
      );

  final VideoStreamInfo? video;
  final AudioStreamInfo? audio;

  /// 容器里全部视频/音频/字幕轨道；`selected` 说明本次实际用的是哪几条，
  /// `select_track` 换轨后由重发的 file_loaded 刷新。位图字幕轨始终未选中。
  final List<TrackInfo> tracks;

  Map<String, dynamic> toJson() => {
        'video': video?.toJson(),
        'audio': audio?.toJson(),
        'tracks': tracks.map((track) => track.toJson()).toList(),
      };
}

/// 一条可播放轨道的元数据。
class TrackInfo {
  const TrackInfo({
    required this.index,
    required this.kind,
    required this.codec,
    required this.defaultTrack,
    required this.selected,
    this.language,
    this.title,
  });

  factory TrackInfo.fromJson(Map<String, dynamic> json) => TrackInfo(
        index: _int(json['index']),
        kind: TrackKind.fromWire(json['kind'] as String),
        codec: json['codec'] as String,
        language: json['language'] as String?,
        title: json['title'] as String?,
        defaultTrack: json['default'] as bool,
        selected: json['selected'] as bool,
      );

  final int index;
  final TrackKind kind;
  final String codec;
  final String? language;
  final String? title;

  /// 容器自带的 default 标记。JSON 键仍是 `default`（保留字，故字段改名）。
  final bool defaultTrack;
  final bool selected;

  Map<String, dynamic> toJson() => {
        'index': index,
        'kind': kind.wire,
        'codec': codec,
        'language': language,
        'title': title,
        'default': defaultTrack,
        'selected': selected,
      };
}

class VideoStreamInfo {
  const VideoStreamInfo({
    required this.width,
    required this.height,
    required this.codec,
    required this.fps,
  });

  factory VideoStreamInfo.fromJson(Map<String, dynamic> json) =>
      VideoStreamInfo(
        width: _int(json['width']),
        height: _int(json['height']),
        codec: json['codec'] as String,
        fps: _double(json['fps']),
      );

  /// 显示宽度（像素宽高比 SAR 已应用）。
  final int width;

  /// 显示高度（SAR 已应用；当前恒等于编码高度）。
  final int height;
  final String codec;
  final double fps;

  Map<String, dynamic> toJson() => {
        'width': width,
        'height': height,
        'codec': codec,
        'fps': fps,
      };
}

class AudioStreamInfo {
  const AudioStreamInfo({
    required this.codec,
    required this.channels,
    required this.sampleRate,
  });

  factory AudioStreamInfo.fromJson(Map<String, dynamic> json) =>
      AudioStreamInfo(
        codec: json['codec'] as String,
        channels: _int(json['channels']),
        sampleRate: _int(json['sample_rate']),
      );

  final String codec;
  final int channels;
  final int sampleRate;

  Map<String, dynamic> toJson() => {
        'codec': codec,
        'channels': channels,
        'sample_rate': sampleRate,
      };
}

/// 只读快照。
class Snapshot {
  const Snapshot({
    required this.state,
    required this.positionMs,
    required this.durationMs,
    required this.volume,
    required this.muted,
    required this.decode,
    required this.source,
    required this.media,
    required this.subtitle,
    required this.danmaku,
    required this.surface,
    required this.stats,
    required this.lastError,
    this.schema = schemaVersion,
  });

  factory Snapshot.fromJson(Map<String, dynamic> json) => Snapshot(
        schema: _int(json['schema']),
        state: PlayerState.fromWire(json['state'] as String),
        positionMs: _int(json['position_ms']),
        durationMs: _int(json['duration_ms']),
        volume: _int(json['volume']),
        muted: json['muted'] as bool,
        decode: DecodeSnapshot.fromJson(
          json['decode'] as Map<String, dynamic>,
        ),
        source: json['source'] as String?,
        media: json['media'] == null
            ? null
            : MediaInfo.fromJson(json['media'] as Map<String, dynamic>),
        subtitle: SubtitleSnapshot.fromJson(
          json['subtitle'] as Map<String, dynamic>,
        ),
        danmaku: DanmakuSnapshot.fromJson(
          json['danmaku'] as Map<String, dynamic>,
        ),
        surface: SurfaceSnapshot.fromJson(
          json['surface'] as Map<String, dynamic>,
        ),
        stats: StatsSnapshot.fromJson(
          json['stats'] as Map<String, dynamic>,
        ),
        lastError: json['last_error'] == null
            ? null
            : ErrorInfo.fromJson(
                json['last_error'] as Map<String, dynamic>,
              ),
      );

  final int schema;
  final PlayerState state;
  final int positionMs;
  final int durationMs;
  final int volume;
  final bool muted;
  final DecodeSnapshot decode;
  final String? source;
  final MediaInfo? media;
  final SubtitleSnapshot subtitle;
  final DanmakuSnapshot danmaku;
  final SurfaceSnapshot surface;
  final StatsSnapshot stats;
  final ErrorInfo? lastError;

  Map<String, dynamic> toJson() => {
        'schema': schema,
        'state': state.wire,
        'position_ms': positionMs,
        'duration_ms': durationMs,
        'volume': volume,
        'muted': muted,
        'decode': decode.toJson(),
        'source': source,
        'media': media?.toJson(),
        'subtitle': subtitle.toJson(),
        'danmaku': danmaku.toJson(),
        'surface': surface.toJson(),
        'stats': stats.toJson(),
        'last_error': lastError?.toJson(),
      };
}

/// 解码设置与实际后端：`backend` 是本次管线真实使用的后端，没有管线时为
/// `software`（此刻没有解码在跑）；硬解生效时 `device` 是实际设备名。
class DecodeSnapshot {
  const DecodeSnapshot({
    required this.mode,
    required this.backend,
    required this.device,
  });

  factory DecodeSnapshot.fromJson(Map<String, dynamic> json) => DecodeSnapshot(
        mode: DecodeMode.fromWire(json['mode'] as String),
        backend: DecodeBackend.fromWire(json['backend'] as String),
        device: json['device'] as String?,
      );

  final DecodeMode mode;
  final DecodeBackend backend;
  final String? device;

  Map<String, dynamic> toJson() => {
        'mode': mode.wire,
        'backend': backend.wire,
        'device': device,
      };
}

class SubtitleSnapshot {
  const SubtitleSnapshot({
    required this.loaded,
    required this.visible,
    required this.source,
  });

  factory SubtitleSnapshot.fromJson(Map<String, dynamic> json) =>
      SubtitleSnapshot(
        loaded: json['loaded'] as bool,
        visible: json['visible'] as bool,
        source: json['source'] as String?,
      );

  final bool loaded;
  final bool visible;
  final String? source;

  Map<String, dynamic> toJson() => {
        'loaded': loaded,
        'visible': visible,
        'source': source,
      };
}

class DanmakuSnapshot {
  const DanmakuSnapshot({
    required this.loaded,
    required this.visible,
    required this.opacity,
    required this.count,
    required this.source,
  });

  factory DanmakuSnapshot.fromJson(Map<String, dynamic> json) =>
      DanmakuSnapshot(
        loaded: json['loaded'] as bool,
        visible: json['visible'] as bool,
        opacity: _int(json['opacity']),
        count: _int(json['count']),
        source: json['source'] as String?,
      );

  final bool loaded;
  final bool visible;
  final int opacity;
  final int count;
  final String? source;

  Map<String, dynamic> toJson() => {
        'loaded': loaded,
        'visible': visible,
        'opacity': opacity,
        'count': count,
        'source': source,
      };
}

class SurfaceSnapshot {
  const SurfaceSnapshot({
    required this.status,
    required this.kind,
    required this.width,
    required this.height,
    required this.scaleFactor,
  });

  factory SurfaceSnapshot.fromJson(Map<String, dynamic> json) =>
      SurfaceSnapshot(
        status: SurfaceStatus.fromWire(json['status'] as String),
        kind: json['kind'] == null
            ? null
            : SurfaceKind.fromWire(json['kind'] as String),
        width: _int(json['width']),
        height: _int(json['height']),
        scaleFactor: _double(json['scale_factor']),
      );

  final SurfaceStatus status;
  final SurfaceKind? kind;
  final int width;
  final int height;
  final double scaleFactor;

  Map<String, dynamic> toJson() => {
        'status': status.wire,
        'kind': kind?.wire,
        'width': width,
        'height': height,
        'scale_factor': scaleFactor,
      };
}

class StatsSnapshot {
  const StatsSnapshot({
    required this.renderedFrames,
    required this.droppedFrames,
  });

  factory StatsSnapshot.fromJson(Map<String, dynamic> json) =>
      StatsSnapshot(
        renderedFrames: _int(json['rendered_frames']),
        droppedFrames: _int(json['dropped_frames']),
      );

  final int renderedFrames;
  final int droppedFrames;

  Map<String, dynamic> toJson() => {
        'rendered_frames': renderedFrames,
        'dropped_frames': droppedFrames,
      };
}

/// Surface 描述符。句柄用字符串，避免 64 位指针丢精度。
sealed class SurfaceDescriptor {
  const SurfaceDescriptor({
    required this.width,
    required this.height,
    required this.scaleFactor,
    this.schema = schemaVersion,
  });

  factory SurfaceDescriptor.fromJson(Map<String, dynamic> json) {
    final kind = SurfaceKind.fromWire(json['kind'] as String);
    final width = _int(json['width']);
    final height = _int(json['height']);
    final scaleFactor = _double(json['scale_factor']);
    final schema = _int(json['schema']);

    return switch (kind) {
      SurfaceKind.windowsHwnd ||
      SurfaceKind.macosNsview ||
      SurfaceKind.androidNativeWindow =>
        HandleSurfaceDescriptor(
          kind: kind,
          handle: json['handle'] as String,
          width: width,
          height: height,
          scaleFactor: scaleFactor,
          schema: schema,
        ),
      SurfaceKind.x11Window => X11SurfaceDescriptor(
          handle: json['handle'] as String,
          display: json['display'] as String,
          width: width,
          height: height,
          scaleFactor: scaleFactor,
          schema: schema,
        ),
      SurfaceKind.waylandSurface => WaylandSurfaceDescriptor(
          surface: json['surface'] as String,
          display: json['display'] as String,
          width: width,
          height: height,
          scaleFactor: scaleFactor,
          schema: schema,
        ),
    };
  }

  final int schema;
  final int width;
  final int height;
  final double scaleFactor;

  SurfaceKind get kind;

  Map<String, dynamic> toJson();
}

class HandleSurfaceDescriptor extends SurfaceDescriptor {
  const HandleSurfaceDescriptor({
    required this.kind,
    required this.handle,
    required super.width,
    required super.height,
    required super.scaleFactor,
    super.schema,
  });

  @override
  final SurfaceKind kind;
  final String handle;

  @override
  Map<String, dynamic> toJson() => {
        'schema': schema,
        'kind': kind.wire,
        'handle': handle,
        'width': width,
        'height': height,
        'scale_factor': scaleFactor,
      };
}

class X11SurfaceDescriptor extends SurfaceDescriptor {
  const X11SurfaceDescriptor({
    required this.handle,
    required this.display,
    required super.width,
    required super.height,
    required super.scaleFactor,
    super.schema,
  });

  final String handle;
  final String display;

  @override
  SurfaceKind get kind => SurfaceKind.x11Window;

  @override
  Map<String, dynamic> toJson() => {
        'schema': schema,
        'kind': kind.wire,
        'handle': handle,
        'display': display,
        'width': width,
        'height': height,
        'scale_factor': scaleFactor,
      };
}

class WaylandSurfaceDescriptor extends SurfaceDescriptor {
  const WaylandSurfaceDescriptor({
    required this.surface,
    required this.display,
    required super.width,
    required super.height,
    required super.scaleFactor,
    super.schema,
  });

  final String surface;
  final String display;

  @override
  SurfaceKind get kind => SurfaceKind.waylandSurface;

  @override
  Map<String, dynamic> toJson() => {
        'schema': schema,
        'kind': kind.wire,
        'surface': surface,
        'display': display,
        'width': width,
        'height': height,
        'scale_factor': scaleFactor,
      };
}

/// 能力 JSON。
class Capabilities {
  const Capabilities({
    required this.abi,
    required this.platform,
    required this.gpuBackends,
    required this.protocols,
    required this.presentation,
    required this.features,
    required this.limits,
    this.schema = schemaVersion,
  });

  factory Capabilities.fromJson(Map<String, dynamic> json) => Capabilities(
        schema: _int(json['schema']),
        abi: AbiVersion.fromJson(json['abi'] as Map<String, dynamic>),
        platform: json['platform'] as String,
        gpuBackends: _strings(json['gpu_backends']),
        protocols: _strings(json['protocols']),
        presentation: PresentationCapabilities.fromJson(
          json['presentation'] as Map<String, dynamic>,
        ),
        features: FeatureCapabilities.fromJson(
          json['features'] as Map<String, dynamic>,
        ),
        limits: Limits.fromJson(json['limits'] as Map<String, dynamic>),
      );

  final int schema;
  final AbiVersion abi;
  final String platform;
  final List<String> gpuBackends;

  /// `load_file` 接受的 URL 协议：本构建实际编译且内核放行的 scheme。
  /// 本地路径不是协议，不在表内；没有 TLS 后端的构建（Android）只有 `http`。
  final List<String> protocols;
  final PresentationCapabilities presentation;
  final FeatureCapabilities features;
  final Limits limits;

  Map<String, dynamic> toJson() => {
        'schema': schema,
        'abi': abi.toJson(),
        'platform': platform,
        'gpu_backends': gpuBackends,
        'protocols': protocols,
        'presentation': presentation.toJson(),
        'features': features.toJson(),
        'limits': limits.toJson(),
      };
}

class PresentationCapabilities {
  const PresentationCapabilities({
    required this.externalSurface,
    required this.textureSharing,
    required this.attachBeforeLoad,
    required this.embedded,
    required this.detached,
    required this.surfaceKinds,
  });

  factory PresentationCapabilities.fromJson(Map<String, dynamic> json) =>
      PresentationCapabilities(
        externalSurface: json['external_surface'] as bool,
        textureSharing: json['texture_sharing'] as bool,
        attachBeforeLoad: json['attach_before_load'] as bool,
        embedded: json['embedded'] as bool,
        detached: json['detached'] as bool,
        surfaceKinds: (json['surface_kinds'] as List<dynamic>)
            .cast<String>()
            .map(SurfaceKind.fromWire)
            .toList(),
      );

  final bool externalSurface;
  final bool textureSharing;
  final bool attachBeforeLoad;
  final bool embedded;
  final bool detached;
  final List<SurfaceKind> surfaceKinds;

  Map<String, dynamic> toJson() => {
        'external_surface': externalSurface,
        'texture_sharing': textureSharing,
        'attach_before_load': attachBeforeLoad,
        'embedded': embedded,
        'detached': detached,
        'surface_kinds': surfaceKinds.map((kind) => kind.wire).toList(),
      };
}

class FeatureCapabilities {
  const FeatureCapabilities({
    required this.videoCodecs,
    required this.audioCodecs,
    required this.subtitleFormats,
    required this.embeddedAssStyling,
    required this.trackSwitching,
    required this.hwDecode,
    required this.danmakuFormats,
    required this.danmakuXmlEntities,
    required this.seekModes,
    required this.overlayInCore,
  });

  factory FeatureCapabilities.fromJson(Map<String, dynamic> json) =>
      FeatureCapabilities(
        videoCodecs: _strings(json['video_codecs']),
        audioCodecs: _strings(json['audio_codecs']),
        subtitleFormats: _strings(json['subtitle_formats']),
        embeddedAssStyling: json['embedded_ass_styling'] as bool,
        trackSwitching: json['track_switching'] as bool,
        hwDecode: json['hw_decode'] as bool,
        danmakuFormats: _strings(json['danmaku_formats']),
        danmakuXmlEntities: json['danmaku_xml_entities'] as bool,
        seekModes: (json['seek_modes'] as List<dynamic>)
            .cast<String>()
            .map(SeekMode.fromWire)
            .toList(),
        overlayInCore: json['overlay_in_core'] as bool,
      );

  final List<String> videoCodecs;
  final List<String> audioCodecs;
  final List<String> subtitleFormats;
  final bool embeddedAssStyling;

  /// `select_track` 是否可用。只覆盖音轨与字幕轨：视频轨切换未实现。
  final bool trackSwitching;

  /// 硬件解码是否已实测可用。为 false 时 `setDecodeMode(DecodeMode.auto)`
  /// 会被同步拒绝（not_supported），客户端不应显示硬解开关。
  final bool hwDecode;
  final List<String> danmakuFormats;
  final bool danmakuXmlEntities;
  final List<SeekMode> seekModes;
  final bool overlayInCore;

  Map<String, dynamic> toJson() => {
        'video_codecs': videoCodecs,
        'audio_codecs': audioCodecs,
        'subtitle_formats': subtitleFormats,
        'embedded_ass_styling': embeddedAssStyling,
        'track_switching': trackSwitching,
        'hw_decode': hwDecode,
        'danmaku_formats': danmakuFormats,
        'danmaku_xml_entities': danmakuXmlEntities,
        'seek_modes': seekModes.map((mode) => mode.wire).toList(),
        'overlay_in_core': overlayInCore,
      };
}

class Limits {
  const Limits({required this.maxTextureDimension2d});

  factory Limits.fromJson(Map<String, dynamic> json) => Limits(
        maxTextureDimension2d: _int(json['max_texture_dimension_2d']),
      );

  final int maxTextureDimension2d;

  Map<String, dynamic> toJson() => {
        'max_texture_dimension_2d': maxTextureDimension2d,
      };
}

/// 错误码目录。
class ErrorCatalog {
  const ErrorCatalog({required this.errors, this.schema = schemaVersion});

  factory ErrorCatalog.fromJson(Map<String, dynamic> json) => ErrorCatalog(
        schema: _int(json['schema']),
        errors: (json['errors'] as List<dynamic>)
            .cast<Map<String, dynamic>>()
            .map(ErrorCatalogEntry.fromJson)
            .toList(),
      );

  final int schema;
  final List<ErrorCatalogEntry> errors;

  Map<String, dynamic> toJson() => {
        'schema': schema,
        'errors': errors.map((entry) => entry.toJson()).toList(),
      };
}

class ErrorCatalogEntry {
  const ErrorCatalogEntry({
    required this.codeNum,
    required this.code,
    required this.constant,
  });

  factory ErrorCatalogEntry.fromJson(Map<String, dynamic> json) =>
      ErrorCatalogEntry(
        codeNum: _int(json['code_num']),
        code: ErrorCode.fromWire(json['code'] as String),
        constant: json['constant'] as String,
      );

  final int codeNum;
  final ErrorCode code;
  final String constant;

  Map<String, dynamic> toJson() => {
        'code_num': codeNum,
        'code': code.wire,
        'constant': constant,
      };
}

/// 状态矩阵单元格状态。
enum MatrixStatus {
  accepted('accepted'),
  rejected('rejected');

  const MatrixStatus(this.wire);

  final String wire;

  static MatrixStatus fromWire(String wire) =>
      _enumFromWire(values, (v) => v.wire, wire, 'matrix status');
}

/// 命令落点：`"same"`、单个状态或候选状态数组。
sealed class TransitionTarget {
  const TransitionTarget();

  factory TransitionTarget.fromJson(Object? json) {
    if (json is String) {
      if (json == 'same') return const SameTarget();
      return StateTarget(PlayerState.fromWire(json));
    }
    if (json is List<dynamic>) {
      return StateListTarget(
        json.cast<String>().map(PlayerState.fromWire).toList(),
      );
    }
    throw FormatException('invalid transition target: $json');
  }

  Object toJson();
}

class SameTarget extends TransitionTarget {
  const SameTarget();

  @override
  Object toJson() => 'same';
}

class StateTarget extends TransitionTarget {
  const StateTarget(this.state);

  final PlayerState state;

  @override
  Object toJson() => state.wire;
}

class StateListTarget extends TransitionTarget {
  const StateListTarget(this.states);

  final List<PlayerState> states;

  @override
  Object toJson() => states.map((state) => state.wire).toList();
}

class MatrixCell {
  const MatrixCell({
    required this.status,
    this.to,
    this.cancelsInflightLoad,
  });

  factory MatrixCell.fromJson(Map<String, dynamic> json) => MatrixCell(
        status: MatrixStatus.fromWire(json['status'] as String),
        to: json.containsKey('to')
            ? TransitionTarget.fromJson(json['to'])
            : null,
        cancelsInflightLoad: json['cancels_inflight_load'] as bool?,
      );

  final MatrixStatus status;
  final TransitionTarget? to;
  final bool? cancelsInflightLoad;

  Map<String, dynamic> toJson() => {
        'status': status.wire,
        if (to != null) 'to': to!.toJson(),
        if (cancelsInflightLoad != null)
          'cancels_inflight_load': cancelsInflightLoad,
      };
}

class RejectSpec {
  const RejectSpec({
    required this.channel,
    required this.status,
    required this.error,
  });

  factory RejectSpec.fromJson(Map<String, dynamic> json) => RejectSpec(
        channel: json['channel'] as String,
        status: CommandStatus.fromWire(json['status'] as String),
        error: ErrorInfo.fromJson(json['error'] as Map<String, dynamic>),
      );

  final String channel;
  final CommandStatus status;
  final ErrorInfo error;

  Map<String, dynamic> toJson() => {
        'channel': channel,
        'status': status.wire,
        'error': error.toJson(),
      };
}

/// 状态矩阵：命令 × 状态 → 允许/拒绝与落点。
class StateMatrix {
  const StateMatrix({
    required this.states,
    required this.notes,
    required this.reject,
    required this.matrix,
    this.schema = schemaVersion,
  });

  factory StateMatrix.fromJson(Map<String, dynamic> json) {
    final rawMatrix = json['matrix'] as Map<String, dynamic>;
    final matrix = <CommandOp, Map<PlayerState, MatrixCell>>{};
    for (final opEntry in rawMatrix.entries) {
      final row = opEntry.value as Map<String, dynamic>;
      matrix[CommandOp.fromWire(opEntry.key)] = {
        for (final cellEntry in row.entries)
          PlayerState.fromWire(cellEntry.key):
              MatrixCell.fromJson(cellEntry.value as Map<String, dynamic>),
      };
    }

    return StateMatrix(
      schema: _int(json['schema']),
      states: (json['states'] as List<dynamic>)
          .cast<String>()
          .map(PlayerState.fromWire)
          .toList(),
      notes: (json['notes'] as Map<String, dynamic>).cast<String, String>(),
      reject: RejectSpec.fromJson(json['reject'] as Map<String, dynamic>),
      matrix: matrix,
    );
  }

  final int schema;
  final List<PlayerState> states;
  final Map<String, String> notes;
  final RejectSpec reject;
  final Map<CommandOp, Map<PlayerState, MatrixCell>> matrix;

  MatrixCell? cell(CommandOp op, PlayerState state) => matrix[op]?[state];

  bool isAccepted(CommandOp op, PlayerState state) =>
      cell(op, state)?.status == MatrixStatus.accepted;

  /// 所有缺失的 (op, state) 组合；契约完整性测试用。
  List<String> missingCells() => [
        for (final op in CommandOp.values)
          for (final state in PlayerState.values)
            if (cell(op, state) == null) '${op.wire}/${state.wire}',
      ];

  Map<String, dynamic> toJson() => {
        'schema': schema,
        'states': states.map((state) => state.wire).toList(),
        'notes': notes,
        'reject': reject.toJson(),
        'matrix': {
          for (final opEntry in matrix.entries)
            opEntry.key.wire: {
              for (final cellEntry in opEntry.value.entries)
                cellEntry.key.wire: cellEntry.value.toJson(),
            },
        },
      };
}
