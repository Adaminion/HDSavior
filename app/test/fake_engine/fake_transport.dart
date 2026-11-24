/// An in-process [EngineTransport] over the fake engine, for tests and for
/// running the app without a real engine.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:hdsavior/engine/transport.dart';

import 'fake_engine.dart';

/// Starts a new [FakeEngine] on every [start], as starting a process would.
///
/// All engines share one [FakeWorld] (the "machine": its disks and the files
/// written so far) and one [FakeScript].
final class FakeEngineTransport implements EngineTransport {
  FakeEngineTransport({
    this.options = const FakeEngineOptions(),
    FakeWorld? world,
    FakeScript? script,
    this.startError,
  }) : world = world ?? FakeWorld(),
       script = script ?? FakeScript();

  final FakeEngineOptions options;
  final FakeWorld world;
  final FakeScript script;

  /// When set, [start] fails with this error (an engine that is missing).
  Object? startError;

  /// How often [start] was called successfully.
  int starts = 0;

  FakeEngine? _engine;

  /// The engine of the most recent [start]. Use it to inject failures, to
  /// step jobs and to inspect the requests it received.
  FakeEngine get engine => _engine ?? (throw StateError('The fake engine was not started yet.'));

  @override
  String get description => 'fake engine';

  @override
  Future<EngineConnection> start() async {
    final error = startError;
    if (error != null) throw error;
    starts++;
    final engine = FakeEngine(options: options, world: world, script: script);
    _engine = engine;
    return _FakeConnection(engine);
  }
}

final class _FakeConnection implements EngineConnection {
  _FakeConnection(this._engine) {
    _engine.output.listen(
      (line) => _stdout.add(utf8.encode('$line\n')),
      onDone: _stdout.close,
    );
    _engine.stderrLines.listen(
      (line) => _stderr.add(utf8.encode('$line\n')),
      onDone: _stderr.close,
    );
  }

  final FakeEngine _engine;
  final _stdout = StreamController<List<int>>();
  final _stderr = StreamController<List<int>>();
  final _pending = BytesBuilder(copy: false);

  @override
  Stream<List<int>> get stdout => _stdout.stream;

  @override
  Stream<List<int>> get stderr => _stderr.stream;

  @override
  void write(List<int> bytes) {
    // The engine reads lines: keep an unfinished one until its newline comes.
    var start = 0;
    for (var i = 0; i < bytes.length; i++) {
      if (bytes[i] != 0x0A) continue;
      _pending.add(bytes.sublist(start, i));
      _engine.handleLine(utf8.decode(_pending.takeBytes()));
      start = i + 1;
    }
    if (start < bytes.length) _pending.add(bytes.sublist(start));
  }

  @override
  Future<void> closeInput() async => _engine.endOfInput();

  @override
  Future<int> get exitCode => _engine.exitCode;

  @override
  void kill() => _engine.crash(exitCode: -9);
}
