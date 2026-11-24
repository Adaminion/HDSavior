/// The client side of the engine protocol's transport (docs/PROTOCOL.md,
/// section 1): owns the engine process, frames JSON lines, matches responses
/// to requests and distributes events.
library;

import 'dart:async';
import 'dart:collection';
import 'dart:convert';

import 'cancel.dart';
import 'enums.dart';
import 'errors.dart';
import 'events.dart';
import 'json.dart';
import 'objects.dart';
import 'transport.dart';

/// Life cycle of the engine process as the client sees it.
enum EngineRunState {
  /// No engine process: never started, shut down, or it ended by itself.
  stopped,

  /// The process is being started.
  starting,

  /// The process runs and takes requests.
  running,

  /// A shutdown is in progress; new requests are refused.
  stopping,
}

/// The end of an engine process.
final class EngineExit {
  const EngineExit({
    required this.exitCode,
    required this.expected,
    required this.stderrTail,
  });

  /// The process exit code; `null` if it could not be determined.
  final int? exitCode;

  /// `true` when the client ended the engine (shutdown, restart, kill);
  /// `false` when the engine ended by itself, which is a crash from the
  /// user's point of view.
  final bool expected;

  /// The last lines the engine wrote to its standard error, for diagnosis.
  final List<String> stderrTail;
}

final class _Request {
  _Request(this.id, this.command, this.line, this.cancel);

  final int id;
  final String command;

  /// The encoded request, without the newline.
  final String line;
  final CancelToken? cancel;
  final Completer<Map<String, Object?>> completer = Completer();
  void Function()? cancelListener;

  /// Cancelled after it was sent: the caller already has its answer
  /// (a [RequestCancelledException]) and the engine's response is dropped.
  bool abandoned = false;

  void detachCancel() {
    final listener = cancelListener;
    if (listener != null) cancel?.removeListener(listener);
    cancelListener = null;
  }
}

/// One engine process.
final class _Session {
  _Session(this.connection);

  final EngineConnection connection;
  StreamSubscription<String>? stdoutLines;
  StreamSubscription<String>? stderrLines;
  final Completer<void> finished = Completer();
  Timer? timer;
  bool stdoutDone = false;
  bool exited = false;
  int? exitCode;

  /// The client asked this process to end.
  bool expectedExit = false;
}

/// Talks to one engine process at a time over an [EngineTransport].
///
/// * Requests are JSON lines with an `id`; [request] completes with the
///   `result` object of the response that echoes that id, or fails with an
///   [EngineException] built from its `error` object.
/// * Lines with an `event` member are converted with [parseEngineEvent] and
///   published on [events] in the order the engine wrote them.
/// * When the process ends, every pending request fails with code
///   `engine_exited` and an [EngineExit] is published on [exits]; [start] can
///   then be called again.
///
/// The engine handles requests strictly one after the other. The client
/// therefore keeps at most [maxInFlight] requests at the engine and holds
/// the others in its own queue, where a request that is no longer needed can
/// still be cancelled before it costs anything (see [CancelToken]).
final class EngineClient {
  EngineClient(
    this._transport, {
    this.maxInFlight = 1,
    this.stderrTailLines = 50,
    this.settleTime = const Duration(seconds: 2),
  }) : assert(maxInFlight >= 1);

  final EngineTransport _transport;

  /// How many requests may be at the engine at the same time.
  final int maxInFlight;

  /// How many lines of the engine's standard error are remembered.
  final int stderrTailLines;

  /// How long the client waits for the second half of "the output closed"
  /// and "the process exited" before it treats the engine as gone, and for a
  /// killed process to disappear.
  final Duration settleTime;

  EngineRunState _state = EngineRunState.stopped;
  _Session? _session;

  /// Identifies the current call of [start]; a shutdown while the process is
  /// still being started moves on, which makes that start give up.
  int _startAttempt = 0;
  bool _disposed = false;
  int _nextId = 1;

  final Queue<_Request> _queue = ListQueue();
  final Map<int, _Request> _inFlight = {};
  final ListQueue<String> _stderrTail = ListQueue();

  final _events = StreamController<EngineEvent>.broadcast();
  final _states = StreamController<EngineRunState>.broadcast();
  final _exits = StreamController<EngineExit>.broadcast();
  final _stderr = StreamController<String>.broadcast();
  final _protocolErrors = StreamController<String>.broadcast();

  /// What the transport starts, for messages.
  String get engineDescription => _transport.description;

  EngineRunState get state => _state;

  bool get isRunning => _state == EngineRunState.running;

  /// Changes of [state].
  Stream<EngineRunState> get states => _states.stream;

  /// Every event from the engine, in order. Broadcast.
  Stream<EngineEvent> get events => _events.stream;

  /// The events of type [T], for example `on<ScanProgress>()`.
  Stream<T> on<T extends EngineEvent>() =>
      _events.stream.where((event) => event is T).cast<T>();

  /// The end of every engine process, expected or not. Pending requests have
  /// already failed when this fires.
  Stream<EngineExit> get exits => _exits.stream;

  /// The engine's log lines (standard error), for humans.
  Stream<String> get stderrLines => _stderr.stream;

  /// The last [stderrTailLines] lines of the engine's standard error.
  List<String> get stderrTail => List.unmodifiable(_stderrTail);

  /// Descriptions of output lines the client could not use (not JSON, a
  /// response to no request, a malformed event). Nothing to show to a user;
  /// useful in logs and tests.
  Stream<String> get protocolErrors => _protocolErrors.stream;

  /// Requests that were sent and not answered yet.
  int get inFlightCount => _inFlight.length;

  /// Requests waiting in the client's queue.
  int get queuedCount => _queue.length;

  // --- life cycle ----------------------------------------------------------

  /// Starts the engine process. Throws an [EngineException] with code
  /// `engine_start_failed` if that fails.
  Future<void> start() async {
    if (_disposed) throw StateError('EngineClient is disposed');
    if (_state != EngineRunState.stopped) {
      throw StateError('The engine is already ${_state.name}');
    }
    final attempt = ++_startAttempt;
    _setState(EngineRunState.starting);

    final EngineConnection connection;
    try {
      connection = await _transport.start();
    } on Object catch (e) {
      if (attempt == _startAttempt) _setState(EngineRunState.stopped);
      throw EngineException.client(
        EngineErrorCode.engineStartFailed,
        'The engine could not be started (${_transport.description}): $e',
      );
    }
    if (attempt != _startAttempt || _disposed) {
      connection.kill();
      throw EngineException.client(
        EngineErrorCode.engineStartFailed,
        'The start of the engine was cancelled.',
      );
    }

    final session = _Session(connection);
    _session = session;
    _stderrTail.clear();
    const decoder = Utf8Decoder(allowMalformed: true);
    session.stdoutLines = connection.stdout
        .transform(decoder)
        .transform(const LineSplitter())
        .listen(
          (line) => _onLine(session, line),
          onError: (Object e) => _report('reading the engine output: $e'),
          onDone: () => _onStdoutDone(session),
        );
    // Standard error must always be drained, or a chatty engine blocks.
    session.stderrLines = connection.stderr
        .transform(decoder)
        .transform(const LineSplitter())
        .listen(_onStderrLine, onError: (Object _) {});
    unawaited(
      connection.exitCode.then<void>(
        (code) => _onExit(session, code),
        onError: (Object _) => _onExit(session, null),
      ),
    );
    _setState(EngineRunState.running);
  }

  /// Ends the engine cleanly: sends `shutdown`, closes its standard input
  /// (either makes the engine save a running scan's session, cancel other
  /// jobs and exit), waits up to [grace] and kills the process only as a
  /// last resort. Completes when the process is gone. Safe to call in any
  /// state.
  Future<void> shutdown({Duration grace = const Duration(seconds: 10)}) async {
    if (_state == EngineRunState.starting) {
      // The process is not there yet; start() kills it when it appears.
      _startAttempt++;
      _setState(EngineRunState.stopped);
      return;
    }
    final session = _session;
    if (session == null) return;
    if (_state == EngineRunState.stopping) return session.finished.future;

    _setState(EngineRunState.stopping);
    session.expectedExit = true;
    _failQueued(
      (request) => EngineException.client(
        EngineErrorCode.engineNotRunning,
        'The engine is shutting down.',
        command: request.command,
      ),
    );

    // The answer to `shutdown` is not awaited: the exit is what counts.
    final id = _nextId++;
    final request = _Request(
      id,
      'shutdown',
      jsonEncode({'id': id, 'cmd': 'shutdown'}),
      null,
    );
    request.completer.future.ignore();
    _send(session, request);
    await session.connection.closeInput();

    await _waitOrKill(session, grace);
  }

  /// Ends the engine by force, without asking. Completes when the process is
  /// gone. For an engine that no longer answers.
  Future<void> kill() async {
    if (_state == EngineRunState.starting) {
      _startAttempt++;
      _setState(EngineRunState.stopped);
      return;
    }
    final session = _session;
    if (session == null) return;
    session.expectedExit = true;
    await _waitOrKill(session, Duration.zero);
  }

  /// Stops the engine if one runs ([graceful]: with [shutdown], otherwise
  /// with [kill]) and starts a fresh one. The new engine knows nothing of
  /// the old one's source, session or catalog.
  Future<void> restart({bool graceful = true}) async {
    if (graceful) {
      await shutdown();
    } else {
      await kill();
    }
    await start();
  }

  /// Shuts the engine down and closes all streams.
  Future<void> dispose() async {
    if (_disposed) return;
    await shutdown();
    _disposed = true;
    await Future.wait<void>([
      _events.close(),
      _states.close(),
      _exits.close(),
      _stderr.close(),
      _protocolErrors.close(),
    ]);
  }

  Future<void> _waitOrKill(_Session session, Duration grace) async {
    if (!session.finished.isCompleted && grace > Duration.zero) {
      await session.finished.future.timeout(grace, onTimeout: () {});
    }
    if (!session.finished.isCompleted) {
      session.connection.kill();
      // A killed process ends promptly; do not hang if the OS disagrees.
      await session.finished.future.timeout(
        settleTime,
        onTimeout: () => _finish(session),
      );
    }
  }

  // --- requests ------------------------------------------------------------

  /// Sends the command [command] with [args] and completes with the `result`
  /// object of its response.
  ///
  /// Fails with an [EngineException] when the engine answers with an error,
  /// is not running (`engine_not_running`), ends before it answers
  /// (`engine_exited`) or answers unreadably (`protocol_error`); and with a
  /// [RequestCancelledException] when [cancel] is cancelled first.
  ///
  /// [urgent] requests (`job_cancel`) are written to the engine at once
  /// instead of waiting in the client's queue.
  Future<Map<String, Object?>> request(
    String command, {
    Map<String, Object?>? args,
    CancelToken? cancel,
    bool urgent = false,
  }) {
    final session = _session;
    if (_state != EngineRunState.running || session == null) {
      return Future.error(
        EngineException.client(
          EngineErrorCode.engineNotRunning,
          _state == EngineRunState.stopping
              ? 'The engine is shutting down.'
              : 'The engine is not running.',
          command: command,
        ),
      );
    }
    if (cancel != null && cancel.isCancelled) {
      return Future.error(RequestCancelledException(command));
    }

    final id = _nextId++;
    final String line;
    try {
      line = jsonEncode({'id': id, 'cmd': command, 'args': ?args});
    } on JsonUnsupportedObjectError catch (e) {
      return Future.error(
        ArgumentError.value(args, 'args', 'cannot be encoded as JSON: $e'),
      );
    }
    final request = _Request(id, command, line, cancel);
    if (cancel != null) {
      void onCancel() => _cancel(request);
      request.cancelListener = onCancel;
      cancel.addListener(onCancel);
    }
    if (urgent) {
      _send(session, request);
    } else {
      _queue.add(request);
      _pump();
    }
    return request.completer.future;
  }

  void _pump() {
    final session = _session;
    if (session == null || _state != EngineRunState.running) return;
    while (_queue.isNotEmpty && _inFlight.length < maxInFlight) {
      _send(session, _queue.removeFirst());
    }
  }

  void _send(_Session session, _Request request) {
    _inFlight[request.id] = request;
    session.connection.write(utf8.encode('${request.line}\n'));
  }

  void _cancel(_Request request) {
    if (request.completer.isCompleted) return;
    request.detachCancel();
    if (!_queue.remove(request)) {
      // Already at the engine. Its slot stays taken until the engine has
      // answered, so that cancelled work is not piled up there.
      request.abandoned = true;
    }
    request.completer.completeError(RequestCancelledException(request.command));
  }

  void _failQueued(EngineException Function(_Request request) error) {
    final queued = List.of(_queue);
    _queue.clear();
    for (final request in queued) {
      _fail(request, error(request));
    }
  }

  void _fail(_Request request, Object error) {
    request.detachCancel();
    if (!request.completer.isCompleted) request.completer.completeError(error);
  }

  // --- input from the engine -----------------------------------------------

  void _onLine(_Session session, String line) {
    if (!identical(session, _session) || line.isEmpty) return;

    final Object? message;
    try {
      message = jsonDecode(line);
    } on FormatException {
      _report('a line that is not JSON: ${_clip(line)}');
      return;
    }
    if (message is! Map<String, Object?>) {
      _report('a line that is not a JSON object: ${_clip(line)}');
      return;
    }

    final eventName = message['event'];
    if (eventName is String) {
      final event = parseEngineEvent(eventName, message['data']);
      if (event is MalformedEvent) {
        _report('event ${event.name}: ${event.problem.message}');
      }
      if (!_events.isClosed) _events.add(event);
      return;
    }

    final id = message['id'];
    if (id is! int) {
      // The engine answers a line it could not read with "id": null. The
      // client only writes well-formed requests, so this is a defect.
      _report('a response without a request id: ${_clip(line)}');
      return;
    }
    final request = _inFlight.remove(id);
    if (request == null) {
      _report('a response to unknown request $id: ${_clip(line)}');
      return;
    }
    request.detachCancel();
    if (!request.abandoned) _complete(request, message);
    _pump();
  }

  void _complete(_Request request, Map<String, Object?> message) {
    final completer = request.completer;
    final ok = message['ok'];
    if (ok == true) {
      final result = message['result'];
      if (result is Map<String, Object?>) {
        completer.complete(result);
      } else if (result == null) {
        completer.complete(const {});
      } else {
        completer.completeError(
          _unreadable(request, 'its "result" is not an object'),
        );
      }
    } else if (ok == false) {
      try {
        final error = EngineError.fromJson(
          JsonObject.root(message['error'], '${request.command}.error'),
        );
        completer.completeError(
          EngineException(error, command: request.command),
        );
      } on ProtocolFormatException catch (e) {
        completer.completeError(_unreadable(request, e.message));
      }
    } else {
      completer.completeError(
        _unreadable(request, 'it has no boolean "ok" member'),
      );
    }
  }

  EngineException _unreadable(_Request request, String problem) {
    _report('response to ${request.command}: $problem');
    return EngineException.client(
      EngineErrorCode.protocolError,
      'The engine answered "${request.command}" in a form this app does not '
      'understand: $problem.',
      command: request.command,
    );
  }

  void _onStderrLine(String line) {
    _stderrTail.addLast(line);
    while (_stderrTail.length > stderrTailLines) {
      _stderrTail.removeFirst();
    }
    if (!_stderr.isClosed) _stderr.add(line);
  }

  // --- end of the process --------------------------------------------------

  void _onStdoutDone(_Session session) {
    session.stdoutDone = true;
    if (session.exited) {
      _finish(session);
    } else {
      // No more answers can arrive. A process that closed its output but
      // lingers is of no use: end it.
      session.timer ??= Timer(settleTime, () {
        session.connection.kill();
        _finish(session);
      });
    }
  }

  void _onExit(_Session session, int? exitCode) {
    session.exited = true;
    session.exitCode = exitCode;
    if (session.stdoutDone) {
      _finish(session);
    } else {
      // Let the lines the engine wrote before it ended arrive first.
      session.timer ??= Timer(settleTime, () => _finish(session));
    }
  }

  void _finish(_Session session) {
    if (session.finished.isCompleted) return;
    session.timer?.cancel();
    unawaited(session.stdoutLines?.cancel());
    unawaited(session.stderrLines?.cancel());
    session.finished.complete();
    if (!identical(session, _session)) return;
    _session = null;

    final exitCode = session.exitCode;
    final expected = session.expectedExit;
    EngineException gone(_Request request) => EngineException.client(
      EngineErrorCode.engineExited,
      expected
          ? 'The engine was shut down before it answered.'
          : 'The engine stopped unexpectedly'
                '${exitCode == null ? '' : ' (exit code $exitCode)'} '
                'before it answered.',
      command: request.command,
    );
    final pending = List.of(_inFlight.values);
    _inFlight.clear();
    for (final request in pending) {
      _fail(request, gone(request));
    }
    _failQueued(gone);

    _setState(EngineRunState.stopped);
    if (!_exits.isClosed) {
      _exits.add(
        EngineExit(
          exitCode: exitCode,
          expected: expected,
          stderrTail: List.unmodifiable(_stderrTail),
        ),
      );
    }
  }

  // --- helpers -------------------------------------------------------------

  void _setState(EngineRunState state) {
    if (_state == state) return;
    _state = state;
    if (!_states.isClosed) _states.add(state);
  }

  void _report(String problem) {
    if (!_protocolErrors.isClosed) _protocolErrors.add(problem);
  }

  static String _clip(String line) =>
      line.length <= 200 ? line : '${line.substring(0, 200)}... (${line.length} characters)';
}
