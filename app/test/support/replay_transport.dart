/// Recording and replaying conversations with an engine.
///
/// A transcript is what docs/ARCHITECTURE.md (section 5.6) calls
/// `docs/protocol-examples.jsonl`: one JSON object per line, the requests the
/// client wrote (`cmd`) and the lines the engine wrote (responses with `ok`,
/// events with `event`), in the order they were exchanged.
///
/// * [RecordingTransport] wraps any transport and writes down the
///   conversation it carries.
/// * [ReplayTransport] plays a transcript back: every request the client
///   writes is matched with the next recorded request of the same command,
///   and the lines the engine wrote after that request are written back,
///   the response carrying the client's own id. So the app can be driven by
///   what a real engine said.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:hdsavior/engine/transport.dart';

/// Splits bytes into lines (without the newline), across chunk boundaries.
final class _LineCutter {
  final _pending = BytesBuilder(copy: false);

  List<String> add(List<int> bytes) {
    final lines = <String>[];
    var start = 0;
    for (var i = 0; i < bytes.length; i++) {
      if (bytes[i] != 0x0A) continue;
      _pending.add(bytes.sublist(start, i));
      lines.add(utf8.decode(_pending.takeBytes(), allowMalformed: true));
      start = i + 1;
    }
    if (start < bytes.length) _pending.add(bytes.sublist(start));
    return lines;
  }
}

/// Wraps [inner] and records the conversation of every connection it makes
/// into [lines] (all connections into the same list).
final class RecordingTransport implements EngineTransport {
  RecordingTransport(this.inner);

  final EngineTransport inner;

  /// The transcript so far.
  final List<String> lines = [];

  @override
  String get description => 'recording ${inner.description}';

  @override
  Future<EngineConnection> start() async => _RecordingConnection(await inner.start(), lines);
}

final class _RecordingConnection implements EngineConnection {
  _RecordingConnection(this._inner, this._lines) {
    final output = _LineCutter();
    stdout = _inner.stdout.map((chunk) {
      for (final line in output.add(chunk)) {
        if (line.trim().isNotEmpty) _lines.add(line.trim());
      }
      return chunk;
    });
  }

  final EngineConnection _inner;
  final List<String> _lines;
  final _input = _LineCutter();

  @override
  late final Stream<List<int>> stdout;

  @override
  Stream<List<int>> get stderr => _inner.stderr;

  @override
  void write(List<int> bytes) {
    for (final line in _input.add(bytes)) {
      if (line.trim().isNotEmpty) _lines.add(line.trim());
    }
    _inner.write(bytes);
  }

  @override
  Future<void> closeInput() => _inner.closeInput();

  @override
  Future<int> get exitCode => _inner.exitCode;

  @override
  void kill() => _inner.kill();
}

/// One recorded request and the lines the engine wrote after it.
final class _Exchange {
  _Exchange(this.request);

  final Map<String, Object?> request;
  final List<Map<String, Object?>> output = [];

  String get command => request['cmd']! as String;
}

/// Plays a transcript back (see the library documentation).
///
/// Requests are matched by command, in order. A request that does not match
/// the next recorded one is answered with an `internal` error naming both
/// commands and noted in [mismatches]; so is a request after the end of the
/// transcript.
final class ReplayTransport implements EngineTransport {
  ReplayTransport(Iterable<String> transcript) {
    for (final line in transcript) {
      if (line.trim().isEmpty) continue;
      final message = jsonDecode(line) as Map<String, Object?>;
      if (message.containsKey('cmd')) {
        _exchanges.add(_Exchange(message));
      } else if (_exchanges.isEmpty) {
        _prologue.add(message);
      } else {
        _exchanges.last.output.add(message);
      }
    }
  }

  /// Reads a transcript file.
  factory ReplayTransport.fromFile(String path) => ReplayTransport(File(path).readAsLinesSync());

  final List<Map<String, Object?>> _prologue = [];
  final List<_Exchange> _exchanges = [];
  int _next = 0;

  /// Requests that did not match the transcript, described.
  final List<String> mismatches = [];

  /// The recorded requests, in order: what a test sends to replay them all.
  List<Map<String, Object?>> get requests => [for (final exchange in _exchanges) exchange.request];

  /// Whether every recorded request has been asked for.
  bool get finished => _next >= _exchanges.length;

  @override
  String get description => 'replayed transcript';

  @override
  Future<EngineConnection> start() async => _ReplayConnection(this);
}

final class _ReplayConnection implements EngineConnection {
  _ReplayConnection(this._replay) {
    // What the engine wrote before the first request (log events, say).
    scheduleMicrotask(() => _prologue(_replay._prologue));
  }

  final ReplayTransport _replay;
  final _stdout = StreamController<List<int>>();
  final _exit = Completer<int>();
  final _input = _LineCutter();

  void _prologue(List<Map<String, Object?>> lines) {
    for (final line in lines) {
      _emit(line);
    }
  }

  void _emit(Map<String, Object?> message) {
    if (!_stdout.isClosed) _stdout.add(utf8.encode('${jsonEncode(message)}\n'));
  }

  @override
  Stream<List<int>> get stdout => _stdout.stream;

  @override
  Stream<List<int>> get stderr => const Stream.empty();

  @override
  void write(List<int> bytes) {
    for (final line in _input.add(bytes)) {
      if (line.trim().isNotEmpty) _answer(jsonDecode(line) as Map<String, Object?>);
    }
  }

  void _answer(Map<String, Object?> request) {
    final id = request['id'];
    final command = request['cmd'];
    if (_replay.finished) {
      _replay.mismatches.add('$command after the end of the transcript');
      _emit(_error(id, 'replay: the transcript has no more requests ($command)'));
      return;
    }
    final exchange = _replay._exchanges[_replay._next];
    if (exchange.command != command) {
      _replay.mismatches.add('expected ${exchange.command}, got $command');
      _emit(_error(id, 'replay: expected ${exchange.command}, got $command'));
      return;
    }
    _replay._next++;
    final recordedId = exchange.request['id'];
    for (final line in exchange.output) {
      final isResponse = line.containsKey('ok') && line['id'] == recordedId;
      _emit(isResponse ? {...line, 'id': id} : line);
    }
  }

  static Map<String, Object?> _error(Object? id, String message) => {
    'id': id,
    'ok': false,
    'error': {'code': 'internal', 'message': message, 'data': null},
  };

  void _end(int code) {
    if (_exit.isCompleted) return;
    unawaited(_stdout.close());
    _exit.complete(code);
  }

  @override
  Future<void> closeInput() async => _end(0);

  @override
  Future<int> get exitCode => _exit.future;

  @override
  void kill() => _end(-9);
}
