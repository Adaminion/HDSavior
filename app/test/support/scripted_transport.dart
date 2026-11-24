/// A transport whose "engine" is the test itself: the test sees every byte
/// the client writes and decides, byte by byte, what comes back.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:hdsavior/engine/transport.dart';

/// Hands out [ScriptedConnection]s.
final class ScriptedTransport implements EngineTransport {
  /// When set, [start] fails with this error.
  Object? startError;

  /// When set, [start] waits for it before it returns.
  Completer<void>? startGate;

  /// Every connection handed out so far.
  final List<ScriptedConnection> connections = [];

  /// The most recent connection.
  ScriptedConnection get connection => connections.last;

  @override
  String get description => 'scripted engine';

  @override
  Future<EngineConnection> start() async {
    await startGate?.future;
    final error = startError;
    if (error != null) throw error;
    final connection = ScriptedConnection();
    connections.add(connection);
    return connection;
  }
}

/// One "engine process" under the control of a test.
final class ScriptedConnection implements EngineConnection {
  final _stdout = StreamController<List<int>>();
  final _stderr = StreamController<List<int>>();
  final _exit = Completer<int>();
  final _written = BytesBuilder();

  /// Whether the client closed the engine's standard input.
  bool inputClosed = false;

  /// Whether the client killed the engine.
  bool killed = false;

  /// What the "process" does when its input is closed: exit with this code,
  /// or (if `null`) keep running, like an engine that hangs.
  int? exitCodeOnInputClosed = 0;

  /// The same for a kill.
  int? exitCodeOnKill = -9;

  @override
  Stream<List<int>> get stdout => _stdout.stream;

  @override
  Stream<List<int>> get stderr => _stderr.stream;

  @override
  Future<int> get exitCode => _exit.future;

  @override
  void write(List<int> bytes) => _written.add(bytes);

  @override
  Future<void> closeInput() async {
    inputClosed = true;
    final code = exitCodeOnInputClosed;
    if (code != null) exit(code);
  }

  @override
  void kill() {
    killed = true;
    final code = exitCodeOnKill;
    if (code != null) exit(code);
  }

  /// Everything the client has written so far, as text.
  String get writtenText => utf8.decode(_written.toBytes());

  /// The complete request lines written so far, decoded.
  List<Map<String, Object?>> get requests => [
    for (final line in const LineSplitter().convert(writtenText))
      if (line.isNotEmpty) jsonDecode(line) as Map<String, Object?>,
  ];

  /// The most recent request.
  Map<String, Object?> get lastRequest => requests.last;

  /// The id of the most recent request.
  int get lastId => lastRequest['id']! as int;

  /// Writes [bytes] to the engine's standard output as one chunk.
  void emitBytes(List<int> bytes) => _stdout.add(bytes);

  /// Writes [text] (no newline is added) as one chunk.
  void emit(String text) => emitBytes(utf8.encode(text));

  /// Writes [message] as one JSON line.
  void emitLine(Object? message) => emit('${jsonEncode(message)}\n');

  /// Answers request [id] with [result].
  void respond(int id, [Map<String, Object?> result = const {}]) =>
      emitLine({'id': id, 'ok': true, 'result': result});

  /// Answers request [id] with an error.
  void respondError(int id, String code, {String message = '', Object? data}) => emitLine({
    'id': id,
    'ok': false,
    'error': {'code': code, 'message': message, 'data': data},
  });

  /// Emits an event.
  void event(String name, Object? data) => emitLine({'event': name, 'data': data});

  /// Writes [text] to the engine's standard error.
  void emitStderr(String text) => _stderr.add(utf8.encode(text));

  /// The process ends: its streams close and its exit code is reported.
  void exit(int code) {
    if (_exit.isCompleted) return;
    unawaited(_stdout.close());
    unawaited(_stderr.close());
    _exit.complete(code);
  }

  /// The process ends, but its output stays open (a child that inherited
  /// the pipe keeps it alive).
  void exitKeepingOutputOpen(int code) {
    if (!_exit.isCompleted) _exit.complete(code);
  }

  /// The process closes its output but does not end.
  void closeOutputOnly() => unawaited(_stdout.close());
}
