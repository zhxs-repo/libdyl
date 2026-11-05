/// 契约模型：命令、事件、快照、Surface、能力、错误码、状态矩阵。
///
/// 与 `libdyl/contract/` golden fixtures 一一对应；本文件只做
/// 解析/序列化，不包含播放控制逻辑。
library;

part 'types.dart';
part 'protocol.dart';

const int schemaVersion = 1;

int _int(Object? value) => (value as num).toInt();

double _double(Object? value) => (value as num).toDouble();

List<String> _strings(Object? value) => (value as List<dynamic>).cast<String>();

T _enumFromWire<T>(
  List<T> values,
  String Function(T) wire,
  String input,
  String label,
) {
  for (final value in values) {
    if (wire(value) == input) return value;
  }
  throw FormatException('unknown $label: $input');
}
