part of 'contract.dart';

/// 命令。JSON 为扁平信封：`{"schema":1,"request_id":101,"op":"play"}`。
sealed class Command {
  const Command();

  factory Command.fromJson(Map<String, dynamic> json) {
    return switch (CommandOp.fromWire(json['op'] as String)) {
      CommandOp.loadFile => LoadFileCommand(json['url'] as String),
      CommandOp.play => const PlayCommand(),
      CommandOp.pause => const PauseCommand(),
      CommandOp.stop => const StopCommand(),
      CommandOp.seek => SeekCommand(
          positionMs: _int(json['position_ms']),
          mode: SeekMode.fromWire(json['mode'] as String),
        ),
      CommandOp.seekRelative => SeekRelativeCommand(
          deltaMs: _int(json['delta_ms']),
          mode: SeekMode.fromWire(json['mode'] as String),
        ),
      CommandOp.setVolume => SetVolumeCommand(_int(json['volume'])),
      CommandOp.setMute => SetMuteCommand(json['muted'] as bool),
      CommandOp.setSubtitleVisible =>
        SetSubtitleVisibleCommand(json['visible'] as bool),
      CommandOp.loadSubtitle => LoadSubtitleCommand(json['path'] as String),
      CommandOp.setDanmakuVisible =>
        SetDanmakuVisibleCommand(json['visible'] as bool),
      CommandOp.setDanmakuOpacity =>
        SetDanmakuOpacityCommand(_int(json['opacity'])),
      CommandOp.loadDanmaku => LoadDanmakuCommand(json['path'] as String),
      CommandOp.selectTrack => SelectTrackCommand(
          kind: TrackKind.fromWire(json['kind'] as String),
          index: _int(json['index']),
        ),
      CommandOp.setDecodeMode =>
        SetDecodeModeCommand(DecodeMode.fromWire(json['mode'] as String)),
    };
  }

  CommandOp get op;

  Map<String, dynamic> toJson();
}

class LoadFileCommand extends Command {
  const LoadFileCommand(this.url);

  final String url;

  @override
  CommandOp get op => CommandOp.loadFile;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'url': url};
}

class PlayCommand extends Command {
  const PlayCommand();

  @override
  CommandOp get op => CommandOp.play;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire};
}

class PauseCommand extends Command {
  const PauseCommand();

  @override
  CommandOp get op => CommandOp.pause;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire};
}

class StopCommand extends Command {
  const StopCommand();

  @override
  CommandOp get op => CommandOp.stop;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire};
}

class SeekCommand extends Command {
  const SeekCommand({required this.positionMs, required this.mode});

  final int positionMs;
  final SeekMode mode;

  @override
  CommandOp get op => CommandOp.seek;

  @override
  Map<String, dynamic> toJson() => {
        'op': op.wire,
        'position_ms': positionMs,
        'mode': mode.wire,
      };
}

class SeekRelativeCommand extends Command {
  const SeekRelativeCommand({required this.deltaMs, required this.mode});

  final int deltaMs;
  final SeekMode mode;

  @override
  CommandOp get op => CommandOp.seekRelative;

  @override
  Map<String, dynamic> toJson() => {
        'op': op.wire,
        'delta_ms': deltaMs,
        'mode': mode.wire,
      };
}

class SetVolumeCommand extends Command {
  const SetVolumeCommand(this.volume);

  final int volume;

  @override
  CommandOp get op => CommandOp.setVolume;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'volume': volume};
}

class SetMuteCommand extends Command {
  const SetMuteCommand(this.muted);

  final bool muted;

  @override
  CommandOp get op => CommandOp.setMute;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'muted': muted};
}

class SetSubtitleVisibleCommand extends Command {
  const SetSubtitleVisibleCommand(this.visible);

  final bool visible;

  @override
  CommandOp get op => CommandOp.setSubtitleVisible;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'visible': visible};
}

class LoadSubtitleCommand extends Command {
  const LoadSubtitleCommand(this.path);

  final String path;

  @override
  CommandOp get op => CommandOp.loadSubtitle;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'path': path};
}

class SetDanmakuVisibleCommand extends Command {
  const SetDanmakuVisibleCommand(this.visible);

  final bool visible;

  @override
  CommandOp get op => CommandOp.setDanmakuVisible;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'visible': visible};
}

class SetDanmakuOpacityCommand extends Command {
  const SetDanmakuOpacityCommand(this.opacity);

  final int opacity;

  @override
  CommandOp get op => CommandOp.setDanmakuOpacity;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'opacity': opacity};
}

class LoadDanmakuCommand extends Command {
  const LoadDanmakuCommand(this.path);

  final String path;

  @override
  CommandOp get op => CommandOp.loadDanmaku;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'path': path};
}

/// 换轨。内核重开解码管线，[kind] 为 `video` 时会被同步拒绝（`not_supported`）。
class SelectTrackCommand extends Command {
  const SelectTrackCommand({required this.kind, required this.index});

  final TrackKind kind;
  final int index;

  @override
  CommandOp get op => CommandOp.selectTrack;

  @override
  Map<String, dynamic> toJson() => {
        'op': op.wire,
        'kind': kind.wire,
        'index': index,
      };
}

/// 解码模式。`auto` 优先硬件解码、失败回落软件；平台未验证硬件路径时会被
/// 同步拒绝（`not_supported`），切换结果以快照 `decode` 为准。
class SetDecodeModeCommand extends Command {
  const SetDecodeModeCommand(this.mode);

  final DecodeMode mode;

  @override
  CommandOp get op => CommandOp.setDecodeMode;

  @override
  Map<String, dynamic> toJson() => {'op': op.wire, 'mode': mode.wire};
}

class CommandEnvelope {
  const CommandEnvelope({
    required this.command,
    this.requestId,
    this.schema = schemaVersion,
  });

  factory CommandEnvelope.fromJson(Map<String, dynamic> json) =>
      CommandEnvelope(
        schema: _int(json['schema']),
        requestId: json['request_id'] == null ? null : _int(json['request_id']),
        command: Command.fromJson(json),
      );

  final int schema;
  final int? requestId;
  final Command command;

  Map<String, dynamic> toJson() => {
        'schema': schema,
        if (requestId != null) 'request_id': requestId,
        ...command.toJson(),
      };
}

/// 事件。信封为 `{"schema":1,"seq":1,"type":"...","data":{...}}`。
sealed class Event {
  const Event();

  factory Event.fromJson(String type, Map<String, dynamic> data) {
    return switch (type) {
      'state_changed' => StateChangedEvent.fromJson(data),
      'file_loaded' => FileLoadedEvent.fromJson(data),
      'seek_completed' => SeekCompletedEvent.fromJson(data),
      'settings_changed' => SettingsChangedEvent.fromJson(data),
      'position_changed' => PositionChangedEvent.fromJson(data),
      'surface_attached' => SurfaceAttachedEvent.fromJson(data),
      'surface_detached' => SurfaceDetachedEvent.fromJson(data),
      'surface_resized' => SurfaceResizedEvent.fromJson(data),
      'command_completed' => CommandCompletedEvent.fromJson(data),
      'warning' => WarningEvent.fromJson(data),
      'error' => ErrorEvent.fromJson(data),
      _ => throw FormatException('unknown event type: $type'),
    };
  }

  String get type;

  Map<String, dynamic> toJson();
}

class StateChangedEvent extends Event {
  const StateChangedEvent({
    required this.from,
    required this.to,
    required this.reason,
  });

  factory StateChangedEvent.fromJson(Map<String, dynamic> json) =>
      StateChangedEvent(
        from: PlayerState.fromWire(json['from'] as String),
        to: PlayerState.fromWire(json['to'] as String),
        reason: StateChangeReason.fromWire(json['reason'] as String),
      );

  final PlayerState from;
  final PlayerState to;
  final StateChangeReason reason;

  @override
  String get type => 'state_changed';

  @override
  Map<String, dynamic> toJson() => {
        'from': from.wire,
        'to': to.wire,
        'reason': reason.wire,
      };
}

class FileLoadedEvent extends Event {
  const FileLoadedEvent({
    required this.requestId,
    required this.source,
    required this.durationMs,
    required this.media,
  });

  factory FileLoadedEvent.fromJson(Map<String, dynamic> json) =>
      FileLoadedEvent(
        requestId: _int(json['request_id']),
        source: json['source'] as String,
        durationMs: _int(json['duration_ms']),
        media: MediaInfo.fromJson(json['media'] as Map<String, dynamic>),
      );

  final int requestId;
  final String source;
  final int durationMs;
  final MediaInfo media;

  @override
  String get type => 'file_loaded';

  @override
  Map<String, dynamic> toJson() => {
        'request_id': requestId,
        'source': source,
        'duration_ms': durationMs,
        'media': media.toJson(),
      };
}

class SeekCompletedEvent extends Event {
  const SeekCompletedEvent({
    required this.requestId,
    required this.fromMs,
    required this.toMs,
    required this.actualMs,
    required this.mode,
  });

  factory SeekCompletedEvent.fromJson(Map<String, dynamic> json) =>
      SeekCompletedEvent(
        requestId: _int(json['request_id']),
        fromMs: _int(json['from_ms']),
        toMs: _int(json['to_ms']),
        actualMs: _int(json['actual_ms']),
        mode: SeekMode.fromWire(json['mode'] as String),
      );

  final int requestId;
  final int fromMs;
  final int toMs;
  final int actualMs;
  final SeekMode mode;

  @override
  String get type => 'seek_completed';

  @override
  Map<String, dynamic> toJson() => {
        'request_id': requestId,
        'from_ms': fromMs,
        'to_ms': toMs,
        'actual_ms': actualMs,
        'mode': mode.wire,
      };
}

class SettingsChangedEvent extends Event {
  const SettingsChangedEvent({
    required this.volume,
    required this.muted,
    required this.decodeMode,
    required this.subtitleVisible,
    required this.danmakuVisible,
    required this.danmakuOpacity,
  });

  factory SettingsChangedEvent.fromJson(Map<String, dynamic> json) =>
      SettingsChangedEvent(
        volume: _int(json['volume']),
        muted: json['muted'] as bool,
        decodeMode: DecodeMode.fromWire(json['decode_mode'] as String),
        subtitleVisible: json['subtitle_visible'] as bool,
        danmakuVisible: json['danmaku_visible'] as bool,
        danmakuOpacity: _int(json['danmaku_opacity']),
      );

  final int volume;
  final bool muted;
  final DecodeMode decodeMode;
  final bool subtitleVisible;
  final bool danmakuVisible;
  final int danmakuOpacity;

  @override
  String get type => 'settings_changed';

  @override
  Map<String, dynamic> toJson() => {
        'volume': volume,
        'muted': muted,
        'decode_mode': decodeMode.wire,
        'subtitle_visible': subtitleVisible,
        'danmaku_visible': danmakuVisible,
        'danmaku_opacity': danmakuOpacity,
      };
}

class PositionChangedEvent extends Event {
  const PositionChangedEvent(this.positionMs);

  factory PositionChangedEvent.fromJson(Map<String, dynamic> json) =>
      PositionChangedEvent(_int(json['position_ms']));

  final int positionMs;

  @override
  String get type => 'position_changed';

  @override
  Map<String, dynamic> toJson() => {'position_ms': positionMs};
}

class SurfaceAttachedEvent extends Event {
  const SurfaceAttachedEvent({
    required this.kind,
    required this.width,
    required this.height,
    required this.scaleFactor,
  });

  factory SurfaceAttachedEvent.fromJson(Map<String, dynamic> json) =>
      SurfaceAttachedEvent(
        kind: SurfaceKind.fromWire(json['kind'] as String),
        width: _int(json['width']),
        height: _int(json['height']),
        scaleFactor: _double(json['scale_factor']),
      );

  final SurfaceKind kind;
  final int width;
  final int height;
  final double scaleFactor;

  @override
  String get type => 'surface_attached';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind.wire,
        'width': width,
        'height': height,
        'scale_factor': scaleFactor,
      };
}

class SurfaceDetachedEvent extends Event {
  const SurfaceDetachedEvent(this.kind);

  factory SurfaceDetachedEvent.fromJson(Map<String, dynamic> json) =>
      SurfaceDetachedEvent(SurfaceKind.fromWire(json['kind'] as String));

  final SurfaceKind kind;

  @override
  String get type => 'surface_detached';

  @override
  Map<String, dynamic> toJson() => {'kind': kind.wire};
}

class SurfaceResizedEvent extends Event {
  const SurfaceResizedEvent({required this.width, required this.height});

  factory SurfaceResizedEvent.fromJson(Map<String, dynamic> json) =>
      SurfaceResizedEvent(
        width: _int(json['width']),
        height: _int(json['height']),
      );

  final int width;
  final int height;

  @override
  String get type => 'surface_resized';

  @override
  Map<String, dynamic> toJson() => {'width': width, 'height': height};
}

class CommandCompletedEvent extends Event {
  const CommandCompletedEvent({
    required this.requestId,
    required this.op,
    required this.status,
    this.error,
    this.result,
  });

  factory CommandCompletedEvent.fromJson(Map<String, dynamic> json) =>
      CommandCompletedEvent(
        requestId: _int(json['request_id']),
        op: CommandOp.fromWire(json['op'] as String),
        status: CommandStatus.fromWire(json['status'] as String),
        error: json['error'] == null
            ? null
            : ErrorInfo.fromJson(json['error'] as Map<String, dynamic>),
        result: json['result'] as Map<String, dynamic>?,
      );

  final int requestId;
  final CommandOp op;
  final CommandStatus status;
  final ErrorInfo? error;
  final Map<String, dynamic>? result;

  @override
  String get type => 'command_completed';

  @override
  Map<String, dynamic> toJson() => {
        'request_id': requestId,
        'op': op.wire,
        'status': status.wire,
        if (error != null) 'error': error!.toJson(),
        if (result != null) 'result': result,
      };
}

class WarningEvent extends Event {
  const WarningEvent({required this.code, required this.message});

  factory WarningEvent.fromJson(Map<String, dynamic> json) => WarningEvent(
        code: json['code'] as String,
        message: json['message'] as String,
      );

  final String code;
  final String message;

  @override
  String get type => 'warning';

  @override
  Map<String, dynamic> toJson() => {'code': code, 'message': message};
}

class ErrorEvent extends Event {
  const ErrorEvent({
    required this.code,
    required this.codeNum,
    required this.message,
    required this.fatal,
  });

  factory ErrorEvent.fromJson(Map<String, dynamic> json) => ErrorEvent(
        code: ErrorCode.fromWire(json['code'] as String),
        codeNum: _int(json['code_num']),
        message: json['message'] as String,
        fatal: json['fatal'] as bool,
      );

  final ErrorCode code;
  final int codeNum;
  final String message;
  final bool fatal;

  @override
  String get type => 'error';

  @override
  Map<String, dynamic> toJson() => {
        'code': code.wire,
        'code_num': codeNum,
        'message': message,
        'fatal': fatal,
      };
}

class EventEnvelope {
  const EventEnvelope({
    required this.seq,
    required this.event,
    this.schema = schemaVersion,
  });

  factory EventEnvelope.fromJson(Map<String, dynamic> json) =>
      EventEnvelope(
        schema: _int(json['schema']),
        seq: _int(json['seq']),
        event: Event.fromJson(
          json['type'] as String,
          json['data'] as Map<String, dynamic>,
        ),
      );

  final int schema;
  final int seq;
  final Event event;

  Map<String, dynamic> toJson() => {
        'schema': schema,
        'seq': seq,
        'type': event.type,
        'data': event.toJson(),
      };
}
