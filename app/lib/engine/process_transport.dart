/// The real transport: `hdsavior-engine serve` as a child process.
library;

import 'dart:io';

import 'transport.dart';

/// Starts the engine executable with the `serve` subcommand and talks to it
/// over the child's standard streams.
///
/// The child inherits the app's elevated token on Windows (the app's manifest
/// requests administrator rights), which raw disk reads require.
final class ProcessEngineTransport implements EngineTransport {
  ProcessEngineTransport({
    required this.executable,
    this.arguments = const ['serve'],
  });

  /// Path of the engine executable (see `locateEngineExecutable`).
  final String executable;
  final List<String> arguments;

  @override
  String get description => executable;

  @override
  Future<EngineConnection> start() async {
    // No shell: the path is passed to the OS as it is. On Windows, Dart
    // starts console programs of a GUI app without opening a console window.
    final process = await Process.start(executable, arguments);
    return _ProcessConnection(process);
  }
}

final class _ProcessConnection implements EngineConnection {
  _ProcessConnection(this._process) {
    // Writing to an engine that has died fails asynchronously through the
    // sink's `done` future. The exit itself is reported through `exitCode`,
    // so the write error carries no news and must not become an unhandled
    // asynchronous error.
    _process.stdin.done.then<void>((_) {}, onError: (Object _) {});
  }

  final Process _process;
  bool _inputClosed = false;

  @override
  Stream<List<int>> get stdout => _process.stdout;

  @override
  Stream<List<int>> get stderr => _process.stderr;

  @override
  void write(List<int> bytes) {
    if (_inputClosed) return;
    try {
      _process.stdin.add(bytes);
    } on StateError {
      // The sink was closed underneath us: the engine is gone.
    }
  }

  @override
  Future<void> closeInput() async {
    if (_inputClosed) return;
    _inputClosed = true;
    try {
      await _process.stdin.close();
    } on Object {
      // Closing the pipe of a dead process can fail; there is nothing to do.
    }
  }

  @override
  Future<int> get exitCode => _process.exitCode;

  @override
  void kill() {
    _process.kill();
  }
}
