/// Loads the JSON examples of docs/PROTOCOL.md from
/// test/data/protocol_examples.txt and turns them into concrete JSON.
library;

import 'dart:collection';
import 'dart:convert';
import 'dart:io';

/// Where the verbatim copies are.
const String protocolExamplesPath = 'test/data/protocol_examples.txt';

/// The document they are copied from, relative to the app directory.
const String protocolDocumentPath = '../docs/PROTOCOL.md';

/// The objects that examples refer to by name.
const List<String> _placeholders = [
  'SourceInfo',
  'Partition',
  'Disk',
  'Warning',
  'Geometry',
  'Instance',
  'CatalogInfo',
  'Row',
  'Filter',
];

/// Values for the examples that list member names only
/// (`{"bytes_done", "bytes_total", ...}`).
const Map<String, Object?> _memberSamples = {
  'bytes_done': 1048576,
  'bytes_total': 4000787030016,
  'speed_bps': 157286400,
  'eta_seconds': 25430,
  'elapsed_seconds': 7,
  'bad_bytes': 512,
  'status': 'completed',
  'error': null,
  'bytes_written': 4000787030016,
  'path': r'D:\disk1.img',
  'bad_map_path': r'D:\disk1.img.bad.txt',
  'kind': 'disk',
};

/// The examples by name. See test/data/protocol_examples.txt for the names.
final class ProtocolExamples {
  ProtocolExamples._(this.verbatim);

  factory ProtocolExamples.load() {
    final text = File(protocolExamplesPath).readAsStringSync().replaceAll('\r\n', '\n');
    final verbatim = <String, String>{};
    String? name;
    final body = StringBuffer();
    void flush() {
      final current = name;
      if (current != null) verbatim[current] = body.toString().trim();
      body.clear();
    }

    for (final line in const LineSplitter().convert(text)) {
      if (line.startsWith('### ')) {
        flush();
        name = line.substring(4).trim();
      } else if (name != null) {
        body.writeln(line);
      }
    }
    flush();
    return ProtocolExamples._(verbatim);
  }

  /// The text of each example exactly as the document shows it.
  final Map<String, String> verbatim;

  Iterable<String> get names => verbatim.keys;

  /// The example [name] as JSON text with every placeholder filled in.
  String resolved(String name) {
    if (name == 'Disk') {
      // "SourceInfo members plus: ..."
      final source = resolved('SourceInfo').trim();
      final extra = resolved('Disk.extra').trim();
      return '${source.substring(0, source.length - 1)}, ${extra.substring(1)}';
    }
    var text = verbatim[name] ?? (throw ArgumentError.value(name, 'name', 'no such example'));

    // {"a", "b"}: member names only.
    final namesOnly = RegExp(r'^\{("[a-z_0-9]+"(, )?)+\}$');
    if (namesOnly.hasMatch(text)) {
      final members = RegExp(r'"([a-z_0-9]+)"').allMatches(text).map((m) => m.group(1)!);
      return jsonEncode({
        for (final member in members)
          member: _memberSamples.containsKey(member)
              ? _memberSamples[member]
              : (throw StateError('no sample value for member "$member" of $name')),
      });
    }

    // Row + {"path": ..., "ancestor_ids": ...}: a row with extra members.
    text = text.replaceAllMapped(RegExp(r'Row \+ (\{[^{}]*\})'), (m) {
      final row = resolved('Row').trim();
      return '${row.substring(0, row.length - 1)}, ${m.group(1)!.substring(1)}';
    });
    // [X, ...]: a list, shown with one element.
    text = text.replaceAll(', ...]', ']');
    // { ... }: some object.
    text = text.replaceAll('{ ... }', '{}');
    // "a"|"b": one of several strings.
    text = text.replaceAllMapped(RegExp(r'("[^"]*")(\|"[^"]*")+'), (m) => m.group(1)!);
    // "data_b64": "...": must be base64 to be decodable.
    text = text.replaceAll('"data_b64": "..."', '"data_b64": "SERTYXZpb3I="');
    // An object referred to by name, as a value.
    text = text.replaceAllMapped(
      RegExp('([:\\[]\\s*)(${_placeholders.join('|')})\\b'),
      (m) => '${m.group(1)}${resolved(m.group(2)!)}',
    );
    return text;
  }

  /// The example [name], decoded.
  Object? json(String name) => jsonDecode(resolved(name));

  /// The example [name] as a JSON object.
  Map<String, Object?> object(String name) => json(name)! as Map<String, Object?>;

  /// The example [name] wrapped so that the test can tell which members a
  /// parser has read.
  RecordingMap recording(String name) => RecordingMap(object(name));
}

Object? _wrap(Object? value) => switch (value) {
  final Map<String, Object?> map => RecordingMap(map),
  final List<Object?> list => [for (final element in list) _wrap(element)],
  _ => value,
};

/// A JSON object that remembers which of its members were read, in order to
/// check that a model class takes every documented member into account.
final class RecordingMap extends MapBase<String, Object?> {
  RecordingMap(Map<String, Object?> source)
    : _inner = {for (final entry in source.entries) entry.key: _wrap(entry.value)};

  final Map<String, Object?> _inner;
  final Set<String> _read = {};

  @override
  Object? operator [](Object? key) {
    if (key is String) _read.add(key);
    return _inner[key];
  }

  @override
  void operator []=(String key, Object? value) => throw UnsupportedError('read-only');

  @override
  void clear() => throw UnsupportedError('read-only');

  @override
  Object? remove(Object? key) => throw UnsupportedError('read-only');

  @override
  Iterable<String> get keys => _inner.keys;

  @override
  bool containsKey(Object? key) => _inner.containsKey(key);

  /// The paths of all members, at any depth, that nothing has read.
  List<String> unread([String path = r'$']) {
    final result = <String>[];
    void visit(Object? value, String at) {
      if (value is RecordingMap) {
        result.addAll(value.unread(at));
      } else if (value is List<Object?>) {
        for (var i = 0; i < value.length; i++) {
          visit(value[i], '$at[$i]');
        }
      }
    }

    for (final entry in _inner.entries) {
      final at = '$path.${entry.key}';
      if (_read.contains(entry.key)) {
        visit(entry.value, at);
      } else {
        result.add(at);
      }
    }
    return result;
  }
}

/// The JSON examples that docs/PROTOCOL.md contains right now: the lines of
/// the transport block, every fenced `json` block, and every inline code
/// span that holds an object.
Set<String> examplesInDocument(String document) {
  final text = document.replaceAll('\r\n', '\n');
  final found = <String>{};
  for (final match in RegExp(r'```(\w*)\n(.*?)```', dotAll: true).allMatches(text)) {
    final body = match.group(2)!.trim();
    if (match.group(1) == 'json') {
      found.add(body);
    } else {
      // The transport block: "request   {...}" lines.
      for (final line in const LineSplitter().convert(body)) {
        final object = RegExp(r'\{.*\}$').firstMatch(line);
        if (object != null) found.add(object.group(0)!);
      }
    }
  }
  for (final match in RegExp(r'`(\{[^`]*\})`').allMatches(text)) {
    found.add(match.group(1)!);
  }
  return found;
}
