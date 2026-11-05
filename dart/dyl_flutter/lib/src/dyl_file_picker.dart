/// 通过 Android SAF 选择本地文件，返回**真实可读路径**。
///
/// 为什么需要它：`DylClient.loadFile` 在 Rust 侧会先做 `Path::exists()` 预检，
/// 再让 FFmpeg 按普通文件路径打开。SAF 直接回传的 `content://` URI 两条都不满足，
/// 所以插件那边会把选中的文件**复制**到 app 私有目录（`filesDir/picked/`），
/// 这里拿到的是那个副本的绝对路径 —— 进程内可读、不受临时授权影响。
///
/// ```dart
/// final file = await DylFilePicker.pickFile();
/// if (file != null) client.loadFile(file.path);
/// ```
library;

import 'package:flutter/services.dart';

/// 一次选择的结果。
class PickedFile {
  const PickedFile({required this.path, required this.name, required this.size});

  /// app 私有目录里的绝对路径，可直接交给 `loadFile` / `loadSubtitle`。
  final String path;

  /// 用户看到的原始文件名（未改写扩展名，大小写保持原样）。
  final String name;

  /// 字节数。
  final int size;

  @override
  String toString() => 'PickedFile($name, $size bytes, $path)';
}

class DylFilePicker {
  DylFilePicker._();

  static const _channel = MethodChannel('dev.dylplayer.dyl_flutter');

  /// 弹出系统文件选择器。
  ///
  /// [mimeTypes] 只是给系统选择器的提示；字幕/弹幕的扩展名（`.ass`/`.ssa`/`.xml`）
  /// 在 Android MIME 表里没有对应项，传空列表（默认）会用 `*/*` 打开，
  /// 再由调用方按扩展名过滤。
  ///
  /// 用户取消时返回 `null`（不算错误）。
  ///
  /// 刻意**不加超时**：人在选择器里翻目录、找文件花几分钟是常事，
  /// 任何时限都会把正常操作判成失败。这里曾经挂过一个 5 分钟的
  /// "失联保护"，结果自己翻目录花了 7 分钟就被误杀了 ——
  /// 反而制造了真正会卡死 UI 的那种 bug。
  ///
  /// 原生侧保证一定回包：取消、Activity 销毁、引擎卸载都会回；
  /// 即便真有一次失联，再点一次会被新的一次选择顶掉（见
  /// DylFlutterPlugin 里的 PICK_SUPERSEDED），所以按钮不会卡死。
  static Future<PickedFile?> pickFile({List<String> mimeTypes = const []}) async {
    final result = await _channel.invokeMapMethod<String, dynamic>(
      'pickFile',
      <String, Object?>{'mimeTypes': mimeTypes},
    );
    if (result == null) return null;

    final path = result['path'] as String?;
    if (path == null || path.isEmpty) return null;
    return PickedFile(
      path: path,
      name: result['name'] as String? ?? path.split('/').last,
      size: (result['size'] as num?)?.toInt() ?? 0,
    );
  }

  /// 按扩展名挑选字幕文件（`.ass` / `.ssa` / `.srt` / `.vtt` / `.sub`）。
  static Future<PickedFile?> pickSubtitle() =>
      pickFile().then((f) => _keepIfExtension(f, _subtitleExtensions));

  /// 按扩展名挑选弹幕文件（bilibili XML / JSON）。
  static Future<PickedFile?> pickDanmaku() =>
      pickFile().then((f) => _keepIfExtension(f, _danmakuExtensions));

  static const _subtitleExtensions = {'.ass', '.ssa', '.srt', '.vtt', '.sub'};
  static const _danmakuExtensions = {'.xml', '.json'};

  static PickedFile? _keepIfExtension(PickedFile? file, Set<String> allowed) {
    if (file == null) return null;
    final dot = file.name.lastIndexOf('.');
    final ext = dot < 0 ? '' : file.name.substring(dot).toLowerCase();
    if (!allowed.contains(ext)) {
      throw FormatException(
        'Unsupported file type "${file.name}". Expected one of ${allowed.join(', ')}.',
      );
    }
    return file;
  }
}