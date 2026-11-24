/// The life of the engine process as the UI sees it.
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';

/// Whether the app can work.
enum EngineStatus {
  /// No engine process (not started yet, or shut down).
  stopped,

  /// The process is being started and asked for its version.
  starting,

  /// The engine runs and speaks the protocol version this app knows.
  ready,

  /// The engine runs but speaks another protocol version; the app must not
  /// use it.
  incompatible,

  /// The engine could not be started, or it ended by itself. See
  /// [EngineController.error].
  failed,
}

/// Starts the engine, checks its protocol version and notices when it dies.
class EngineController extends ChangeNotifier {
  EngineController(this._api) {
    _exits = _api.client.exits.listen(_onExit);
  }

  final EngineApi _api;
  late final StreamSubscription<EngineExit> _exits;

  EngineStatus _status = EngineStatus.stopped;
  HelloResult? _hello;
  EngineException? _error;
  EngineExit? _lastExit;
  Future<void>? _starting;

  EngineStatus get status => _status;

  bool get isReady => _status == EngineStatus.ready;

  /// The engine's answer to `hello`; `null` unless it runs.
  HelloResult? get hello => _hello;

  /// Why [status] is [EngineStatus.failed]: `engine_start_failed` with the
  /// reason, or `engine_exited`.
  EngineException? get error => _error;

  /// The last time the engine ended by itself, with the tail of its log.
  EngineExit? get lastExit => _lastExit;

  /// What is started as the engine (the path of the executable).
  String get engineDescription => _api.client.engineDescription;

  /// Starts the engine and asks for its version. Never throws: a failure
  /// becomes [status] `failed` with [error]. Does nothing if the engine is
  /// starting or running.
  Future<void> start() => _starting ??= _start().whenComplete(() => _starting = null);

  Future<void> _start() async {
    if (_status == EngineStatus.ready || _status == EngineStatus.incompatible) {
      return;
    }
    _status = EngineStatus.starting;
    _error = null;
    _hello = null;
    notifyListeners();
    try {
      if (_api.client.state == EngineRunState.stopped) {
        await _api.client.start();
      }
      final hello = await _api.hello();
      _hello = hello;
      _status = hello.protocol == supportedProtocolVersion
          ? EngineStatus.ready
          : EngineStatus.incompatible;
    } on EngineException catch (e) {
      _error = e;
      _status = EngineStatus.failed;
      // An engine that started but cannot say hello is of no use.
      if (_api.client.state == EngineRunState.running) {
        unawaited(_api.client.kill());
      }
    }
    notifyListeners();
  }

  /// Ends the engine ([graceful]: politely with `shutdown`, otherwise by
  /// force, for one that no longer answers) and starts a fresh one. The new
  /// engine knows nothing of the old one's source, session or catalog.
  Future<void> restart({bool graceful = true}) async {
    await _starting;
    if (graceful) {
      await _api.client.shutdown();
    } else {
      await _api.client.kill();
    }
    _status = EngineStatus.stopped;
    await start();
  }

  /// Shuts the engine down cleanly (for the end of the app): `shutdown`,
  /// end of input, and a kill only if it does not exit within [grace].
  Future<void> shutdown({Duration grace = const Duration(seconds: 10)}) async {
    await _api.client.shutdown(grace: grace);
    _status = EngineStatus.stopped;
    _hello = null;
    notifyListeners();
  }

  void _onExit(EngineExit exit) {
    _hello = null;
    if (exit.expected) {
      if (_status != EngineStatus.starting) _status = EngineStatus.stopped;
    } else {
      _lastExit = exit;
      _status = EngineStatus.failed;
      _error = EngineException.client(
        EngineErrorCode.engineExited,
        'The engine stopped unexpectedly'
        '${exit.exitCode == null ? '' : ' (exit code ${exit.exitCode})'}.',
      );
    }
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_exits.cancel());
    super.dispose();
  }
}
