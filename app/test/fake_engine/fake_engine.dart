/// A fake `hdsavior-engine serve`: the protocol of docs/PROTOCOL.md over a
/// generated in-memory world, for tests and for running the app without a
/// real engine.
///
/// It takes request lines ([FakeEngine.handleLine]) and produces response and
/// event lines ([FakeEngine.output]) exactly like the real engine's standard
/// streams, so it can sit behind an in-process transport
/// (fake_transport.dart) or run as a process of its own (serve.dart).
///
/// What it implements: every command and event; the states N, O, S, C, B, J
/// with the documented error per state; catalog epochs; one job at a time;
/// jobs that advance step by step with progress events and can be cancelled;
/// the destination guard's verdicts with the protected ranges of the moment;
/// the user's verdicts about file systems (`instance_set`); long requests with
/// `task_progress` during which later requests wait. What the catalog holds is described in
/// fake_tree.dart, the naming rules that select behaviour in fake_world.dart.
///
/// Failures are injected with [FakeEngine.failNext], [FakeEngine.exitOn],
/// [FakeEngine.crash], [FakeEngine.bumpEpoch] and [FakeEngine.script], or
/// over the wire with the non-protocol command `fake_control`.
///
/// Pure Dart on purpose: no Flutter and no package imports.
library;

import 'dart:async';
import 'dart:convert';

import 'fake_catalog.dart';
import 'fake_tree.dart';
import 'fake_world.dart';

export 'fake_catalog.dart' show FakeCatalog, FakeError, fakeCarveKinds;
export 'fake_tree.dart';
export 'fake_world.dart';

/// Fixed settings of one fake engine.
final class FakeEngineOptions {
  const FakeEngineOptions({
    this.mainNodes = 3000,
    this.seed = 1,
    this.tick = Duration.zero,
    this.elevated = true,
    this.os = 'windows',
    this.protocol = 1,
  });

  /// Nodes of the main file system of a complete scan (see [FakeTree]).
  final int mainNodes;
  final int seed;

  /// Time between two steps of a job. `null`: jobs only advance when
  /// [FakeEngine.advance] is called (deterministic tests).
  final Duration? tick;

  /// `false`: `hello.elevated` is false and opening a disk answers
  /// `access_denied`.
  final bool elevated;

  /// `hello.os`. `list_disks` is empty unless it is `windows`.
  final String os;

  /// `hello.protocol`.
  final int protocol;
}

/// How the fake's jobs behave. Can be changed at any time; a job reads the
/// values when it starts.
final class FakeScript {
  /// Progress steps of a full scan before the two `analyzing` steps.
  int scanSteps = 8;

  /// The scan fails with [scanFailCode] when it reaches this step.
  int? scanFailAtStep;
  String scanFailCode = 'source_lost';

  /// Bytes of unreadable sectors the scan meets per step (0: none).
  int scanBadBytesPerStep = 0;

  /// Warnings (PROTOCOL `Warning`) that the scan's progress events carry
  /// from step [scanWarningsFromStep] on.
  List<Map<String, Object?>> scanWarnings = [];
  int scanWarningsFromStep = 3;

  int recoverSteps = 6;
  int? recoverFailAtStep;
  String recoverFailCode = 'no_space';

  int carveSteps = 5;
  int? carveFailAtStep;
  String carveFailCode = 'source_lost';

  int imageSteps = 6;
  int? imageFailAtStep;
  String imageFailCode = 'source_lost';

  /// Bytes in unreadable sectors that an image job reports.
  int imageBadBytes = 0;

  /// Ticks that `session_load` and `geometry_set` take. During them
  /// `task_progress` events are emitted and later requests wait. 0: they
  /// answer at once (still with `task_progress` events before the answer).
  int taskTicks = 0;
}

/// A request the fake has received.
final class FakeRequest {
  FakeRequest(this.id, this.command, this.args);
  final int id;
  final String command;
  final Map<String, Object?> args;

  @override
  String toString() => '$command$args';
}

final class _Injected {
  _Injected(this.error, this.times);
  final FakeError error;
  int times;
}

final class _Session {
  _Session({
    required this.rangeStart,
    required this.rangeEnd,
    required this.collectCarveHits,
    required this.recordedSource,
    this.bytesDone = 0,
    this.path,
  });

  final int rangeStart;
  final int rangeEnd;
  final bool collectCarveHits;

  /// PROTOCOL `SourceInfo` of the scanned source.
  final Map<String, Object?> recordedSource;
  int bytesDone;
  String? path;

  int get bytesTotal => rangeEnd - rangeStart;
  bool get complete => bytesDone >= bytesTotal;
}

/// A job: a state machine that the engine steps once per tick.
abstract base class _Job {
  String get kind;
  bool cancelRequested = false;

  /// Does one step; returns `true` when the job has ended (it has then
  /// called [FakeEngine._finishJob]).
  bool step();
}

/// A long request (`session_load`, `geometry_set`).
final class _Task {
  _Task(this.name, this.ticks, this.finish);
  final String name;
  final int ticks;
  final void Function() finish;
  int done = 0;
}

/// See the library documentation.
final class FakeEngine {
  FakeEngine({this.options = const FakeEngineOptions(), FakeWorld? world, FakeScript? script})
    : world = world ?? FakeWorld(),
      script = script ?? FakeScript() {
    _stderr.add('hdsavior-engine (fake) serving, ${options.mainNodes} nodes per scan');
  }

  final FakeEngineOptions options;
  final FakeWorld world;
  final FakeScript script;

  final _out = StreamController<String>(sync: true);
  final _stderr = StreamController<String>(sync: true);
  final _exit = Completer<int>();

  /// The lines the engine writes to its standard output (no newline).
  Stream<String> get output => _out.stream;

  /// The lines the engine writes to its standard error.
  Stream<String> get stderrLines => _stderr.stream;

  /// Completes when the engine process "ends".
  Future<int> get exitCode => _exit.future;

  bool get hasExited => _exit.isCompleted;

  /// Every request received so far, in order.
  final List<FakeRequest> requests = [];

  // --- protocol state ------------------------------------------------------

  Map<String, Object?>? _source;
  Map<String, Object?>? _sourceDisk;
  bool _sourceAttached = false;
  _Session? _session;
  FakeCatalog? _catalog;
  _Job? _job;
  _Task? _task;
  int _epochCounter = 0;

  final List<String> _inbox = [];
  Timer? _timer;
  final Map<String, _Injected> _failures = {};
  String? _exitOnCommand;
  int _exitOnCode = 3;
  String? _exitOnPartialOutput;

  /// The current catalog, for tests that need ground truth.
  FakeCatalog? get catalog => _job is _ScanJob ? null : _catalog;

  /// The kind of the running job, `null` if none.
  String? get runningJob => _job?.kind;

  // --- injection -----------------------------------------------------------

  /// The next [times] requests of [command] are answered with this error
  /// instead of being handled.
  void failNext(
    String command,
    String code, {
    String message = 'injected failure',
    Map<String, Object?>? data,
    int times = 1,
  }) {
    _failures[command] = _Injected(FakeError(code, message, data), times);
  }

  /// When the next request of [command] arrives, the engine dies without
  /// answering it: it writes [partialOutput] (if any, without a newline),
  /// closes its output and exits with [exitCode].
  void exitOn(String command, {int exitCode = 3, String? partialOutput}) {
    _exitOnCommand = command;
    _exitOnCode = exitCode;
    _exitOnPartialOutput = partialOutput;
  }

  /// The engine dies now.
  void crash({int exitCode = 3}) {
    _stderr.add('fake engine: simulated crash');
    _terminate(exitCode);
  }

  /// Rebuilds the catalog behind the client's back: the next request that
  /// carries the old epoch answers `stale_catalog`.
  void bumpEpoch() {
    final catalog = _catalog;
    if (catalog != null) catalog.epoch = ++_epochCounter;
  }

  /// Writes [line] to the output as it is (protocol-violation tests).
  void emitRaw(String line) {
    if (!_out.isClosed) _out.add(line);
  }

  /// Emits a `log` event.
  void log(String level, String message) =>
      _event('log', {'level': level, 'message': message});

  // --- clock ---------------------------------------------------------------

  /// Advances the running job (or long request) by [steps] steps. Only
  /// needed when [FakeEngineOptions.tick] is `null`.
  void advance([int steps = 1]) {
    for (var i = 0; i < steps && !hasExited; i++) {
      _step();
    }
  }

  void _schedule() {
    final tick = options.tick;
    if (tick == null || _timer != null || hasExited) return;
    if (_job == null && _task == null) return;
    _timer = Timer(tick, () {
      _timer = null;
      _step();
      _schedule();
    });
  }

  void _step() {
    final task = _task;
    if (task != null) {
      task.done++;
      _event('task_progress', {'task': task.name, 'fraction': task.done / task.ticks});
      if (task.done >= task.ticks) {
        _task = null;
        task.finish();
        _drain();
      }
      return;
    }
    _job?.step();
  }

  // --- input ---------------------------------------------------------------

  /// One line of the engine's standard input.
  void handleLine(String line) {
    if (hasExited || line.trim().isEmpty) return;
    _inbox.add(line);
    _drain();
  }

  /// The engine's standard input was closed: like `shutdown`, without an
  /// answer.
  void endOfInput() {
    if (hasExited) return;
    _shutdown();
  }

  void _drain() {
    // Requests are handled one at a time in arrival order; while a long
    // request works, later ones wait.
    while (_inbox.isNotEmpty && _task == null && !hasExited) {
      _handle(_inbox.removeAt(0));
    }
    _schedule();
  }

  void _handle(String line) {
    Object? message;
    try {
      message = jsonDecode(line);
    } on FormatException {
      message = null;
    }
    final id = message is Map<String, Object?> ? message['id'] : null;
    if (message is! Map<String, Object?> || id is! int) {
      _emit({
        'id': null,
        'ok': false,
        'error': FakeError('bad_request', 'not a JSON object with an integer "id"').toJson(),
      });
      return;
    }
    final command = message['cmd'];
    final rawArgs = message['args'];
    if (command is! String || (rawArgs != null && rawArgs is! Map<String, Object?>)) {
      _fail(id, FakeError('bad_request', '"cmd" must be a string and "args" an object'));
      return;
    }
    final args = rawArgs is Map<String, Object?> ? rawArgs : const <String, Object?>{};
    requests.add(FakeRequest(id, command, args));

    if (command == _exitOnCommand) {
      _exitOnCommand = null;
      final partial = _exitOnPartialOutput;
      if (partial != null) _out.add(partial);
      _stderr.add('fake engine: simulated crash while handling $command');
      _terminate(_exitOnCode);
      return;
    }
    final injected = _failures[command];
    if (injected != null) {
      if (--injected.times <= 0) _failures.remove(command);
      _fail(id, injected.error);
      return;
    }

    try {
      final result = _dispatch(id, command, args);
      // `null`: the handler answers later (a long request) or not at all.
      if (result != null) _ok(id, result);
    } on FakeError catch (e) {
      _fail(id, e);
    } on TypeError catch (e) {
      _fail(id, FakeError('bad_request', 'an argument has the wrong type: $e'));
    }
  }

  // --- output --------------------------------------------------------------

  void _emit(Map<String, Object?> message) {
    if (!_out.isClosed) _out.add(jsonEncode(message));
  }

  void _ok(int id, Map<String, Object?> result) => _emit({'id': id, 'ok': true, 'result': result});

  void _fail(int id, FakeError error) => _emit({'id': id, 'ok': false, 'error': error.toJson()});

  void _event(String name, Map<String, Object?> data) => _emit({'event': name, 'data': data});

  void _terminate(int code) {
    if (hasExited) return;
    _timer?.cancel();
    _timer = null;
    _job = null;
    _task = null;
    _inbox.clear();
    unawaited(_out.close());
    unawaited(_stderr.close());
    _exit.complete(code);
  }

  void _shutdown() {
    // "A running scan stops reading and saves its session (no catalog is
    // built); other jobs are cancelled; then the engine exits."
    final job = _job;
    final session = _session;
    if (job is _ScanJob && session != null && session.path != null) {
      _saveSession(session.path!);
    }
    _terminate(0);
  }

  // --- arguments -----------------------------------------------------------

  static T _arg<T>(Map<String, Object?> args, String key) {
    final value = args[key];
    if (value is T && value != null) return value;
    throw FakeError(
      'bad_request',
      args.containsKey(key) ? '"$key" has the wrong type' : '"$key" is required',
    );
  }

  static T _opt<T>(Map<String, Object?> args, String key, T fallback) {
    final value = args[key];
    if (value == null) return fallback;
    if (value is T) return value as T;
    throw FakeError('bad_request', '"$key" has the wrong type');
  }

  static List<T>? _list<T>(Map<String, Object?> args, String key, {bool allowEmpty = false}) {
    final value = args[key];
    if (value == null) return null;
    if (value is! List<Object?> || value.any((e) => e is! T)) {
      throw FakeError('bad_request', '"$key" has the wrong type');
    }
    if (value.isEmpty && !allowEmpty) {
      throw FakeError('bad_request', '"$key" must not be an empty list (use null for "any")');
    }
    return value.cast<T>();
  }

  static Map<String, Object?>? _filter(Map<String, Object?> args) {
    final filter = _opt<Map<String, Object?>?>(args, 'filter', null);
    if (filter == null) return null;
    _list<String>(filter, 'exts');
    _list<String>(filter, 'ratings');
    _opt<String?>(filter, 'query', null);
    _opt<int?>(filter, 'min_size', null);
    _opt<int?>(filter, 'max_size', null);
    return filter;
  }

  static (int, int) _page(Map<String, Object?> args) {
    final offset = _opt<int>(args, 'offset', 0);
    final limit = _opt<int>(args, 'limit', 200);
    if (offset < 0 || limit < 1 || limit > 1000) {
      throw FakeError('bad_request', '"offset" must be >= 0 and "limit" from 1 to 1000');
    }
    return (offset, limit);
  }

  // --- state checks --------------------------------------------------------

  void _notBusy() {
    final job = _job;
    if (job != null) throw FakeError('busy', 'A ${job.kind} job is running.');
  }

  FakeCatalog _needCatalog() {
    final catalog = _catalog;
    if (_job is _ScanJob || catalog == null) {
      throw FakeError(
        'no_catalog',
        _job is _ScanJob ? 'There is no catalog while a scan is running.' : 'Nothing has been scanned yet.',
      );
    }
    return catalog;
  }

  FakeCatalog _needCatalogAt(Map<String, Object?> args) {
    final catalog = _needCatalog();
    final epoch = _arg<int>(args, 'epoch');
    if (epoch != catalog.epoch) {
      throw FakeError('stale_catalog', 'The catalog has changed (epoch ${catalog.epoch}, not $epoch).');
    }
    return catalog;
  }

  Map<String, Object?> _needSource() {
    final source = _source;
    if (source == null || !_sourceAttached) {
      throw FakeError('no_source', 'No source is attached.');
    }
    return source;
  }

  /// The ranges the destination guard protects now (docs/ARCHITECTURE.md,
  /// S2): with a catalog its `protected` list; during a scan (or after one
  /// that failed) the session range; before any session the whole source,
  /// which the next scan or image covers unless told otherwise (the fake's
  /// choice, see fake_world.dart). [job] is the range of a job that is about
  /// to start (`scan_start`, `image_start`), protected in addition.
  List<FakeProtectedRange> _protectedNow({(int, int)? job}) {
    final catalog = _job is _ScanJob ? null : _catalog;
    final session = _session;
    final size = (_source?['size'] as int?) ?? 0;
    return [
      if (job != null) FakeProtectedRange(job.$1, job.$2, 'session_range'),
      if (catalog != null)
        ...catalog.protectedRanges()
      else if (session != null)
        FakeProtectedRange(session.rangeStart, session.rangeEnd, 'session_range')
      else if (job == null && size > 0)
        FakeProtectedRange(0, size, 'session_range'),
    ];
  }

  /// PROTOCOL `check_destination` result for [path] with the protected
  /// ranges of the moment ([job]: see [_protectedNow]).
  Map<String, Object?> _check(String path, {required String purpose, required int? needBytes, (int, int)? job}) {
    final catalog = _job is _ScanJob ? null : _catalog;
    return world.checkDestination(
      path,
      purpose: purpose,
      needBytes: needBytes,
      sourceDisk: _sourceDisk,
      sourceImage: _source?['kind'] == 'image' ? _source!['path'] as String? : null,
      protected: _protectedNow(job: job),
      filesIn: catalog?.filesIn,
    );
  }

  /// Runs the destination guard; throws `destination_refused` with the
  /// check result as data unless the verdict is acceptable.
  Map<String, Object?> _guard(
    String path, {
    required String purpose,
    required int? needBytes,
    required Map<String, Object?> args,
    (int, int)? job,
  }) {
    final check = _check(path, purpose: purpose, needBytes: needBytes, job: job);
    final verdict = check['verdict'];
    final allowed =
        verdict == 'ok' ||
        (verdict == 'same_disk' && _opt<bool>(args, 'allow_same_disk', false)) ||
        (verdict == 'unknown_disk' && _opt<bool>(args, 'allow_unknown_disk', false));
    if (!allowed) {
      throw FakeError('destination_refused', check['message']! as String, check);
    }
    return check;
  }

  /// PROTOCOL `CatalogInfo` of the current catalog, with the warning
  /// `session_file_at_risk` when the session file lies inside a protected
  /// range (docs/ALGORITHMS.md, B3).
  Map<String, Object?> _info() {
    final catalog = _catalog!;
    final path = _session?.path;
    var atRisk = false;
    if (path != null) {
      final check = world.checkDestination(
        path,
        purpose: 'session',
        needBytes: null,
        sourceDisk: _sourceDisk,
        protected: catalog.protectedRanges(),
        filesIn: catalog.filesIn,
      );
      atRisk = check['verdict'] == 'overlaps_source';
    }
    return catalog.info(sessionFileAtRisk: atRisk);
  }

  // --- commands ------------------------------------------------------------

  Map<String, Object?>? _dispatch(int id, String command, Map<String, Object?> args) {
    switch (command) {
      case 'hello':
        return {
          'engine_version': '0.1.0-fake',
          'protocol': options.protocol,
          'os': options.os,
          'elevated': options.elevated,
          'raw_disk_support': options.os == 'windows',
          'carve_kinds': fakeCarveKinds.keys.toList(),
        };
      case 'list_disks':
        return {
          'disks': options.os == 'windows' ? [for (final disk in world.disks) FakeWorld.listDisksEntry(disk)] : const <Object?>[],
        };
      case 'job_cancel':
        _job?.cancelRequested = true;
        return {};
      case 'shutdown':
        _ok(id, {});
        _shutdown();
        return null;
      case 'open_source':
        return _openSource(args);
      case 'close_source':
        _notBusy();
        _source = null;
        _sourceDisk = null;
        _sourceAttached = false;
        _session = null;
        _catalog = null;
        return {};
      case 'scan_start':
        return _scanStart(args);
      case 'session_save':
        return _sessionSave(args);
      case 'session_load':
        return _sessionLoad(id, args);
      case 'session_attach_source':
        return _sessionAttach(args);
      case 'catalog_info':
        _needCatalog();
        return _info();
      case 'geometry_set':
        return _geometrySet(id, args);
      case 'instance_set':
        return _instanceSet(args);
      case 'list_children':
        final catalog = _needCatalogAt(args);
        final (offset, limit) = _page(args);
        return catalog.listChildren(
          node: _arg<int>(args, 'node'),
          offset: offset,
          limit: limit,
          sort: _opt<String>(args, 'sort', 'name'),
          desc: _opt<bool>(args, 'desc', false),
          dirsOnly: _opt<bool>(args, 'dirs_only', false),
          showDeleted: _opt<bool>(args, 'show_deleted', false),
          exts: _list<String>(args, 'exts'),
          ratings: _list<String>(args, 'ratings'),
        );
      case 'node_info':
        return _needCatalogAt(args).nodeInfo(_arg<int>(args, 'node'));
      case 'search':
        final catalog = _needCatalogAt(args);
        final (offset, limit) = _page(args);
        return catalog.search(
          under: _opt<int?>(args, 'under', null),
          filter: _filter(args),
          showDeleted: _opt<bool>(args, 'show_deleted', false),
          sort: _opt<String>(args, 'sort', 'name'),
          desc: _opt<bool>(args, 'desc', false),
          offset: offset,
          limit: limit,
        );
      case 'selection_summary':
        return _summarize(_needCatalogAt(args), args);
      case 'check_destination':
        _needSource();
        final purpose = _arg<String>(args, 'purpose');
        if (!const ['recover', 'image', 'session', 'app_data'].contains(purpose)) {
          throw FakeError('bad_request', 'unknown purpose "$purpose"');
        }
        return _check(_arg<String>(args, 'path'), purpose: purpose, needBytes: _opt<int?>(args, 'need_bytes', null));
      case 'recover_start':
        return _recoverStart(args);
      case 'carve_start':
        return _carveStart(args);
      case 'image_start':
        return _imageStart(args);
      case 'read_file':
        final catalog = _needCatalog();
        _needSource();
        if (catalog.epoch != _arg<int>(args, 'epoch')) {
          throw FakeError('stale_catalog', 'The catalog has changed.');
        }
        final length = _opt<int>(args, 'length', 65536);
        final offset = _opt<int>(args, 'offset', 0);
        if (length < 0 || length > 8388608 || offset < 0) {
          throw FakeError('bad_request', '"length" must be at most 8388608');
        }
        return catalog.readFile(node: _arg<int>(args, 'node'), offset: offset, length: length);
      case 'fake_control':
        return _control(args);
      default:
        throw FakeError('unknown_command', 'unknown command "$command"');
    }
  }

  Map<String, Object?> _summarize(FakeCatalog catalog, Map<String, Object?> args) => catalog.summarize(
    include: _list<int>(args, 'include', allowEmpty: true) ?? const [],
    exclude: _list<int>(args, 'exclude', allowEmpty: true) ?? const [],
    filter: _filter(args),
    includeDeleted: _opt<bool>(args, 'include_deleted', false),
    skipDamaged: _opt<bool>(args, 'skip_damaged', false),
  );

  Map<String, Object?> _openSource(Map<String, Object?> args) {
    _notBusy();
    final kind = _arg<String>(args, 'kind');
    final path = _arg<String>(args, 'path');
    final sectorSize = _opt<int?>(args, 'sector_size', null);
    final Map<String, Object?> source;
    final String table;
    final List<Map<String, Object?>> partitions;
    final List<Map<String, Object?>> warnings;
    Map<String, Object?>? disk;
    if (kind == 'disk') {
      final match = RegExp(r'^\\\\\.\\PhysicalDrive(\d+)$', caseSensitive: false).firstMatch(path);
      if (match == null) {
        throw FakeError('bad_request', r'A disk must be opened as \\.\PhysicalDrive<N>.');
      }
      disk = world.diskByNumber(int.parse(match.group(1)!));
      if (disk == null) throw FakeError('not_found', 'There is no disk $path.');
      if (!options.elevated) {
        throw FakeError('access_denied', 'Windows refused access to $path. HDSavior must run as administrator.');
      }
      if ((disk['flags']! as List<Object?>).contains('no_media')) {
        throw FakeError('io_error', 'There is no medium in $path.');
      }
      source = FakeWorld.sourceInfoOf(disk);
      table = disk['table']! as String;
      partitions = (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>();
      warnings = FakeWorld.diskWarnings(disk);
    } else if (kind == 'image') {
      final name = FakeWorld.fileName(path).toLowerCase();
      if (name.contains('missing')) throw FakeError('not_found', 'The file $path does not exist.');
      if (name.contains('denied')) throw FakeError('access_denied', 'The file $path cannot be opened for reading.');
      if (sectorSize != null && sectorSize != 512 && sectorSize != 4096) {
        throw FakeError('bad_request', '"sector_size" must be 512 or 4096');
      }
      (source, table, partitions) = FakeWorld.image(path, sectorSize);
      warnings = const [];
    } else {
      throw FakeError('bad_request', '"kind" must be "disk" or "image"');
    }
    // "Closes any previously open source and drops its session and catalog."
    _source = source;
    _sourceDisk = disk;
    _sourceAttached = true;
    _session = null;
    _catalog = null;
    return {'source': source, 'table': table, 'partitions': partitions, 'warnings': warnings};
  }

  // --- sessions ------------------------------------------------------------

  void _saveSession(String path) {
    final session = _session!;
    session.path = path;
    world.savedSessions[path] = FakeSavedSession(
      source: session.recordedSource,
      rangeStart: session.rangeStart,
      rangeEnd: session.rangeEnd,
      bytesDone: session.bytesDone,
      collectCarveHits: session.collectCarveHits,
    );
  }

  Map<String, Object?> _sessionSave(Map<String, Object?> args) {
    _notBusy();
    final session = _session;
    if (session == null) throw FakeError('no_session', 'There is no session to save.');
    final path = _arg<String>(args, 'path');
    _guard(path, purpose: 'session', needBytes: null, args: args);
    _saveSession(path);
    return {'path': path, 'bytes': 4096 + session.bytesDone ~/ 1048576 * 180};
  }

  /// The warning `mixed_generations` (docs/ALGORITHMS.md, D2).
  static const Map<String, Object?> _mixedGenerations = {
    'code': 'mixed_generations',
    'message':
        'Some records of an older file system could not be separated cleanly from the lost one. '
        'Files marked as older generation may belong to it.',
  };

  /// Builds the catalog for the current session (new epoch). A source whose
  /// path contains `mixed` gives the warning `mixed_generations`.
  FakeCatalog _buildCatalog({List<Map<String, Object?>> warnings = const []}) {
    final session = _session!;
    final fraction = session.bytesDone / session.bytesTotal;
    var nodes = (options.mainNodes * fraction).floor();
    if (session.complete) nodes = options.mainNodes;
    if (nodes < FakeTree.minMainNodes) nodes = FakeTree.minMainNodes;
    final sourcePath = (session.recordedSource['path'] as String?) ?? '';
    return FakeCatalog(
      tree: FakeTree.generate(seed: options.seed, mainNodes: nodes),
      epoch: ++_epochCounter,
      rangeStart: session.rangeStart,
      rangeEnd: session.rangeEnd,
      sourceSize: (session.recordedSource['size'] as int?) ?? session.rangeEnd,
      scannedBytes: session.bytesDone,
      collectCarveHits: session.collectCarveHits,
      sourceAttached: _sourceAttached,
      warnings: [
        ...warnings,
        if (sourcePath.toLowerCase().contains('mixed') && !warnings.contains(_mixedGenerations)) _mixedGenerations,
      ],
    );
  }

  Map<String, Object?>? _sessionLoad(int id, Map<String, Object?> args) {
    _notBusy();
    final path = _arg<String>(args, 'path');
    final override = _opt<Map<String, Object?>?>(args, 'source', null);
    final name = FakeWorld.fileName(path).toLowerCase();
    final saved = world.savedSessions[path];
    final warnings = <Map<String, Object?>>[];
    var attached = true;
    Map<String, Object?>? sourceError;

    final _Session session;
    if (saved != null) {
      session = _Session(
        rangeStart: saved.rangeStart,
        rangeEnd: saved.rangeEnd,
        collectCarveHits: saved.collectCarveHits,
        recordedSource: saved.source,
        bytesDone: saved.bytesDone,
        path: path,
      );
    } else {
      if (name.contains('missing')) throw FakeError('not_found', 'The file $path does not exist.');
      if (name.contains('corrupt')) throw FakeError('io_error', 'The session file $path is damaged.');
      if (name.contains('newer')) {
        throw FakeError('unsupported', 'The session file was written by a newer version of HDSavior.');
      }
      final disk = world.diskByNumber(1)!;
      final partition = (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>().first;
      final start = partition['offset']! as int;
      final end = start + (partition['size']! as int);
      session = _Session(
        rangeStart: start,
        rangeEnd: end,
        collectCarveHits: true,
        recordedSource: FakeWorld.sourceInfoOf(disk),
        bytesDone: name.contains('partial') ? (end - start) ~/ 512 ~/ 3 * 512 : end - start,
        path: path,
      );
      if (name.contains('detached')) {
        attached = false;
        sourceError = {
          'code': 'not_found',
          'message': 'The disk this session was made from is not connected.',
          'data': null,
        };
      }
      if (name.contains('identity')) {
        warnings.add({
          'code': 'identity_differs',
          'message': 'The disk reports a different model or serial number than when it was scanned.',
        });
      }
      if (name.contains('mixed')) warnings.add(_mixedGenerations);
    }

    var source = session.recordedSource;
    Map<String, Object?>? disk;
    if (override != null) {
      final overridePath = _arg<String>(override, 'path');
      if (overridePath.toLowerCase().contains('mismatch')) {
        throw FakeError('source_mismatch', 'This is not the disk the session was made from.', {
          'failed': ['content'],
        });
      }
      source = {...source, 'kind': _arg<String>(override, 'kind'), 'path': overridePath};
      attached = true;
      sourceError = null;
    }
    final number = source['disk_number'];
    if (number is int) disk = world.diskByNumber(number);

    void finish() {
      _session = session;
      _source = source;
      _sourceDisk = attached ? disk : null;
      _sourceAttached = attached;
      _catalog = _buildCatalog(warnings: warnings);
      _ok(id, {
        'source': source,
        'source_attached': attached,
        'source_error': sourceError,
        'catalog': _info(),
      });
    }

    _runTask('session_load', finish);
    return null;
  }

  Map<String, Object?> _sessionAttach(Map<String, Object?> args) {
    _notBusy();
    final session = _session;
    if (session == null) throw FakeError('no_session', 'There is no session to attach a source to.');
    final kind = _arg<String>(args, 'kind');
    final path = _arg<String>(args, 'path');
    final force = _opt<bool>(args, 'force', false);
    final lower = path.toLowerCase();
    if (lower.contains('mismatch')) {
      throw FakeError('source_mismatch', 'This is not the disk the session was made from.', {
        'failed': ['content'],
      });
    }
    if (lower.contains('fewsamples') && !force) {
      throw FakeError('source_mismatch', 'Too few records could be compared to be sure this is the same disk.', {
        'failed': ['samples'],
      });
    }
    if (kind == 'disk' && !options.elevated) {
      throw FakeError('access_denied', 'Windows refused access to $path. HDSavior must run as administrator.');
    }
    final source = {...session.recordedSource, 'kind': kind, 'path': path};
    final number = source['disk_number'];
    _source = source;
    _sourceDisk = number is int ? world.diskByNumber(number) : null;
    _sourceAttached = true;
    // What the records say stays true; what was said about the old source
    // (identity_differs) does not.
    final mixed = _catalog?.warnings.contains(_mixedGenerations) ?? false;
    _catalog = _buildCatalog(warnings: [if (mixed) _mixedGenerations]);
    return {'source': source, 'catalog': _info()};
  }

  /// Runs a long request: `task_progress` events, then [finish] (which
  /// writes the response). With `script.taskTicks == 0` all of it happens at
  /// once; otherwise it takes that many ticks and later requests wait.
  void _runTask(String name, void Function() finish) {
    final ticks = script.taskTicks;
    if (ticks <= 0) {
      _event('task_progress', {'task': name, 'fraction': 0.5});
      _event('task_progress', {'task': name, 'fraction': 1.0});
      finish();
      return;
    }
    _task = _Task(name, ticks, finish);
  }

  Map<String, Object?>? _geometrySet(int id, Map<String, Object?> args) {
    _notBusy();
    final catalog = _needCatalog();
    final instance = _arg<int>(args, 'instance');
    final clusterSize = _arg<int>(args, 'cluster_size');
    final volumeOffset = _arg<int>(args, 'volume_offset');
    // Arguments are checked before the long part starts.
    catalog.checkGeometry(instance: instance, clusterSize: clusterSize, volumeOffset: volumeOffset);
    _runTask('geometry_set', () {
      catalog.setGeometry(instance: instance, clusterSize: clusterSize, volumeOffset: volumeOffset);
      _ok(id, _info());
    });
    return null;
  }

  /// `instance_set`: the user overrules the verdict about one file system.
  /// The catalog is rebuilt (new epoch); carved nodes are discarded.
  Map<String, Object?> _instanceSet(Map<String, Object?> args) {
    _notBusy();
    final catalog = _needCatalog();
    catalog.setInstanceKind(_arg<int>(args, 'instance'), _arg<String>(args, 'kind'));
    catalog.epoch = ++_epochCounter;
    return _info();
  }

  // --- scan ----------------------------------------------------------------

  Map<String, Object?> _scanStart(Map<String, Object?> args) {
    final resume = _opt<bool>(args, 'resume', false);
    _notBusy();
    final source = _needSource();
    final size = source['size']! as int;
    final sector = source['sector_size']! as int;

    final _Session session;
    if (resume) {
      final existing = _session;
      if (existing == null) throw FakeError('no_session', 'There is no session to resume.');
      final start = _opt<int?>(args, 'start', null);
      final end = _opt<int?>(args, 'end', null);
      final carve = _opt<bool?>(args, 'collect_carve_hits', null);
      if ((start != null && start != existing.rangeStart) ||
          (end != null && end != existing.rangeEnd) ||
          (carve != null && carve != existing.collectCarveHits)) {
        throw FakeError('bad_request', 'A resumed scan must keep the range and options of its session.');
      }
      session = existing;
    } else {
      final start = _opt<int>(args, 'start', 0);
      final end = _opt<int?>(args, 'end', null) ?? size;
      if (start < 0 || end > size || start >= end || start % sector != 0 || end % sector != 0) {
        throw FakeError('bad_request', 'The range must be sector-aligned and inside the source.');
      }
      session = _Session(
        rangeStart: start,
        rangeEnd: end,
        collectCarveHits: _opt<bool>(args, 'collect_carve_hits', true),
        recordedSource: source,
      );
    }
    final sessionPath = _opt<String?>(args, 'session_path', null) ?? (resume ? session.path : null);
    if (sessionPath != null) {
      // Checked with the range of the new session, before anything is read.
      _guard(
        sessionPath,
        purpose: 'session',
        needBytes: null,
        args: args,
        job: (session.rangeStart, session.rangeEnd),
      );
    }

    // "Any scan_start discards the current catalog and carved nodes at once."
    _session = session;
    session.path = sessionPath;
    _catalog = null;
    _job = _ScanJob(this, session);
    return {'job': 'scan', 'bytes_total': session.bytesTotal};
  }

  void _finishJob() {
    _job = null;
  }

  // --- recover, carve, image -----------------------------------------------

  Map<String, Object?> _recoverStart(Map<String, Object?> args) {
    final catalog = _needCatalog();
    _needSource();
    _notBusy();
    if (catalog.epoch != _arg<int>(args, 'epoch')) {
      throw FakeError('stale_catalog', 'The catalog has changed.');
    }
    final dest = _arg<String>(args, 'dest');
    final onExists = _opt<String>(args, 'on_exists', 'rename');
    if (!const ['rename', 'skip', 'overwrite'].contains(onExists)) {
      throw FakeError('bad_request', 'unknown on_exists "$onExists"');
    }
    final summary = _summarize(catalog, args);
    final files = summary['files']! as int;
    final bytes = summary['bytes']! as int;
    if (files == 0 && summary['dirs'] == 0) {
      throw FakeError('bad_request', 'Nothing is selected.');
    }
    final check = _guard(dest, purpose: 'recover', needBytes: bytes, args: args);
    if (check['enough_space'] == false) {
      throw FakeError('no_space', 'There is not enough free space at the destination.', check);
    }
    _job = _RecoverJob(
      this,
      dest: dest,
      summary: summary.cast<String, int>(),
      skipDamaged: _opt<bool>(args, 'skip_damaged', false),
    );
    return {'job': 'recover', 'files': files, 'bytes': bytes};
  }

  Map<String, Object?> _carveStart(Map<String, Object?> args) {
    final catalog = _needCatalog();
    _needSource();
    _notBusy();
    final kinds = _list<String>(args, 'kinds') ?? fakeCarveKinds.keys.toList();
    for (final kind in kinds) {
      if (!fakeCarveKinds.containsKey(kind)) {
        throw FakeError('bad_request', 'unknown carve kind "$kind"');
      }
    }
    if (!catalog.collectCarveHits) {
      throw FakeError('unsupported', 'The scan did not collect signature hits, so there is nothing to carve.');
    }
    // The carved nodes of these kinds are replaced: the old ones go now.
    catalog.carve(kinds, fraction: 0);
    _job = _CarveJob(this, catalog, kinds);
    return {'job': 'carve', 'candidates': catalog.carveHits};
  }

  Map<String, Object?> _imageStart(Map<String, Object?> args) {
    final source = _needSource();
    _notBusy();
    final size = source['size']! as int;
    final sector = source['sector_size']! as int;
    final start = _opt<int>(args, 'start', 0);
    final end = _opt<int?>(args, 'end', null) ?? size;
    final dest = _arg<String>(args, 'dest');
    final resume = _opt<bool>(args, 'resume', false);
    if (start < 0 || end > size || start >= end || start % sector != 0 || end % sector != 0) {
      throw FakeError('bad_request', 'The range must be sector-aligned and inside the source.');
    }
    final bytes = end - start;
    // "The guard protects the imaged range in addition to the session's
    // protected ranges."
    final check = _guard(dest, purpose: 'image', needBytes: bytes, args: args, job: (start, end));
    final exists = world.imageFiles.contains(dest) || FakeWorld.fileName(dest).toLowerCase().contains('exists');
    if (exists && !resume) {
      throw FakeError('exists', 'The file $dest already exists.');
    }
    final maxFile = check['max_file_bytes'] as int?;
    if (maxFile != null && bytes > maxFile) {
      throw FakeError('no_space', 'The destination file system cannot hold a file of this size.', check);
    }
    if (check['enough_space'] == false) {
      throw FakeError('no_space', 'There is not enough free space at the destination.', check);
    }
    _job = _ImageJob(this, dest: dest, bytesTotal: bytes);
    return {'job': 'image', 'bytes': bytes};
  }

  // --- non-protocol control channel ----------------------------------------

  /// `fake_control`: drives the injection hooks over the wire, for when the
  /// fake runs as a separate process. Not part of the protocol.
  Map<String, Object?> _control(Map<String, Object?> args) {
    final exitOnCommand = _opt<String?>(args, 'exit_on', null);
    if (exitOnCommand != null) {
      exitOn(
        exitOnCommand,
        exitCode: _opt<int>(args, 'exit_code', 3),
        partialOutput: _opt<String?>(args, 'partial_output', null),
      );
    }
    final fail = _opt<Map<String, Object?>?>(args, 'fail_next', null);
    if (fail != null) {
      failNext(
        _arg<String>(fail, 'cmd'),
        _arg<String>(fail, 'code'),
        message: _opt<String>(fail, 'message', 'injected failure'),
        data: _opt<Map<String, Object?>?>(fail, 'data', null),
        times: _opt<int>(fail, 'times', 1),
      );
    }
    if (_opt<bool>(args, 'bump_epoch', false)) bumpEpoch();
    final stderrLine = _opt<String?>(args, 'stderr', null);
    if (stderrLine != null) _stderr.add(stderrLine);
    final raw = _opt<String?>(args, 'raw', null);
    if (raw != null) emitRaw(raw);
    final taskTicks = _opt<int?>(args, 'task_ticks', null);
    if (taskTicks != null) script.taskTicks = taskTicks;
    final scanFailAt = _opt<int?>(args, 'scan_fail_at_step', null);
    if (scanFailAt != null) script.scanFailAtStep = scanFailAt;
    return {};
  }
}

Map<String, Object?> _errorJson(String code, String message) => {'code': code, 'message': message, 'data': null};

String _failureText(String code) => switch (code) {
  'source_lost' => 'The disk stopped answering. Check the cable, then attach it again.',
  'no_space' => 'The destination is full.',
  'io_error' => 'A read or write failed.',
  _ => 'The job failed ($code).',
};

// --- jobs ---------------------------------------------------------------------

final class _ScanJob extends _Job {
  _ScanJob(this.engine, this.session)
    : steps = engine.script.scanSteps,
      failAt = engine.script.scanFailAtStep,
      failCode = engine.script.scanFailCode,
      badBytesPerStep = engine.script.scanBadBytesPerStep,
      warnings = List.of(engine.script.scanWarnings),
      warningsFrom = engine.script.scanWarningsFromStep {
    // A resumed scan continues where the session stopped.
    stepIndex = (steps * session.bytesDone / session.bytesTotal).round();
    resumedComplete = session.complete;
    engine.script.scanFailAtStep = null;
  }

  final FakeEngine engine;
  final _Session session;
  final int steps;
  final int? failAt;
  final String failCode;
  final int badBytesPerStep;
  final List<Map<String, Object?>> warnings;
  final int warningsFrom;
  int stepIndex = 0;
  int analysisStep = 0;
  bool stopped = false;
  bool resumedComplete = false;
  int elapsed = 0;

  @override
  String get kind => 'scan';

  Map<String, Object?> _progress(String phase) {
    final done = session.bytesDone;
    final total = session.bytesTotal;
    const speed = 157286400;
    final analyzing = phase == 'analyzing';
    return {
      'phase': phase,
      'bytes_done': analyzing ? analysisStep : done,
      'bytes_total': analyzing ? 2 : total,
      'position': session.rangeStart + (done >= total ? total - 512 : done),
      'speed_bps': analyzing ? 0 : speed,
      'eta_seconds': analyzing ? null : ((total - done) / speed).ceil(),
      'elapsed_seconds': elapsed,
      'records': (812401 * done / total).floor(),
      'files': (790001 * done / total).floor(),
      'dirs': (22300 * done / total).floor(),
      'index_blocks': (904 * done / total).floor(),
      'carve_hits': session.collectCarveHits ? (48213 * done / total).floor() : 0,
      'bad_bytes': badBytesPerStep * stepIndex,
      'warnings': stepIndex >= warningsFrom ? warnings : <Object?>[],
    };
  }

  @override
  bool step() {
    if (resumedComplete) {
      // "Resuming a complete session just emits scan_done."
      _done('completed', null);
      return true;
    }
    elapsed += 7;
    if (analysisStep > 0) {
      // Phase "analyzing": job_cancel is ignored.
      if (analysisStep == 2) {
        _done(stopped ? 'stopped' : 'completed', null);
        return true;
      }
      analysisStep++;
      engine._event('scan_progress', _progress('analyzing'));
      return false;
    }
    if (failAt != null && stepIndex == failAt) {
      if (failCode == 'source_lost') {
        // The session stays resumable, but its source is gone.
        engine._sourceAttached = false;
        engine._sourceDisk = null;
      }
      _done('failed', _errorJson(failCode, _failureText(failCode)));
      return true;
    }
    if (cancelRequested && session.bytesDone > 0) stopped = true;
    if (stopped || stepIndex >= steps) {
      analysisStep = 1;
      engine._event('scan_progress', _progress('analyzing'));
      return false;
    }
    stepIndex++;
    session.bytesDone =
        stepIndex >= steps ? session.bytesTotal : session.bytesTotal ~/ steps ~/ 512 * 512 * stepIndex;
    engine._event('scan_progress', _progress(stepIndex <= 2 ? 'mft_probe' : 'sweep'));
    return false;
  }

  void _done(String status, Map<String, Object?>? error) {
    final path = session.path;
    if (path != null) engine._saveSession(path);
    // "X_done is written after the job slot is free and the resulting state
    // is in place."
    engine._finishJob();
    Map<String, Object?>? catalog;
    if (error == null) {
      engine._catalog = engine._buildCatalog();
      catalog = engine._info();
    }
    engine._event('scan_done', {
      'status': status,
      'error': error,
      'session_path': path,
      'catalog': catalog,
    });
  }
}

final class _RecoverJob extends _Job {
  _RecoverJob(this.engine, {required this.dest, required this.summary, required this.skipDamaged})
    : steps = engine.script.recoverSteps,
      failAt = engine.script.recoverFailAtStep,
      failCode = engine.script.recoverFailCode {
    engine.script.recoverFailAtStep = null;
    final damaged = summary['damaged']!;
    okTotal = summary['good']!;
    // One good file in a thousand "changed since the scan".
    failedTotal = okTotal ~/ 1000;
    okTotal -= failedTotal;
    warningsTotal = summary['uncertain']! + (skipDamaged ? 0 : damaged);
    skippedTotal = summary['unsupported']! + (skipDamaged ? damaged : 0);
  }

  final FakeEngine engine;
  final String dest;
  final Map<String, int> summary;
  final bool skipDamaged;
  final int steps;
  final int? failAt;
  final String failCode;
  late int okTotal;
  late int failedTotal;
  late int warningsTotal;
  late int skippedTotal;
  int stepIndex = 0;

  @override
  String get kind => 'recover';

  int _part(int total) => stepIndex >= steps ? total : total * stepIndex ~/ steps;

  @override
  bool step() {
    if (cancelRequested) {
      _done('cancelled', null);
      return true;
    }
    if (failAt != null && stepIndex == failAt) {
      _done('failed', _errorJson(failCode, _failureText(failCode)));
      return true;
    }
    if (stepIndex >= steps) {
      _done('completed', null);
      return true;
    }
    stepIndex++;
    final files = summary['files']!;
    final bytes = summary['bytes']!;
    const speed = 104857600;
    engine._event('recover_progress', {
      'files_done': _part(files),
      'files_total': files,
      'bytes_done': _part(bytes),
      'bytes_total': bytes,
      'current': stepIndex >= steps ? null : 'Photos/IMG_${stepIndex.toString().padLeft(4, '0')}.JPG',
      'speed_bps': speed,
      'eta_seconds': ((bytes - _part(bytes)) / speed).ceil(),
      'elapsed_seconds': stepIndex * 7,
      'ok': _part(okTotal),
      'warnings': _part(warningsTotal),
      'failed': _part(failedTotal),
      'skipped': _part(skippedTotal),
    });
    return false;
  }

  void _done(String status, Map<String, Object?>? error) {
    engine._finishJob();
    const stamp = '20261124-031500';
    engine._event('recover_done', {
      'status': status,
      'error': error,
      'files_total': summary['files'],
      'ok': _part(okTotal),
      'warnings': _part(warningsTotal),
      'failed': _part(failedTotal),
      'skipped': _part(skippedTotal),
      'bytes_written': _part(summary['bytes']!),
      'dest': dest,
      'report_path': FakeWorld.join(dest, 'HDSavior-report-$stamp.csv'),
    });
  }
}

final class _CarveJob extends _Job {
  _CarveJob(this.engine, this.catalog, this.kinds)
    : steps = engine.script.carveSteps,
      failAt = engine.script.carveFailAtStep,
      failCode = engine.script.carveFailCode {
    engine.script.carveFailAtStep = null;
  }

  final FakeEngine engine;
  final FakeCatalog catalog;
  final List<String> kinds;
  final int steps;
  final int? failAt;
  final String failCode;
  int stepIndex = 0;

  @override
  String get kind => 'carve';

  @override
  bool step() {
    if (failAt != null && stepIndex == failAt) {
      _done('failed', _errorJson(failCode, _failureText(failCode)));
      return true;
    }
    if (cancelRequested || stepIndex >= steps) {
      _done(cancelRequested && stepIndex < steps ? 'cancelled' : 'completed', null);
      return true;
    }
    stepIndex++;
    final total = catalog.carveHits;
    engine._event('carve_progress', {
      'candidates_done': total * stepIndex ~/ steps,
      'candidates_total': total,
      'found': kinds.length * 5 * stepIndex ~/ steps,
      'bytes_found': kinds.length * 5 * 3000000 * stepIndex ~/ steps,
      'eta_seconds': (steps - stepIndex) * 9,
    });
    return false;
  }

  void _done(String status, Map<String, Object?>? error) {
    // "Results found before a cancel are kept."
    final (found, bytesFound) = catalog.carve(kinds, fraction: stepIndex / steps);
    engine._finishJob();
    engine._event('carve_done', {
      'status': status,
      'error': error,
      'found': found,
      'bytes_found': bytesFound,
      'catalog': engine._info(),
    });
  }
}

final class _ImageJob extends _Job {
  _ImageJob(this.engine, {required this.dest, required this.bytesTotal})
    : steps = engine.script.imageSteps,
      failAt = engine.script.imageFailAtStep,
      failCode = engine.script.imageFailCode,
      badBytes = engine.script.imageBadBytes {
    engine.script.imageFailAtStep = null;
  }

  final FakeEngine engine;
  final String dest;
  final int bytesTotal;
  final int steps;
  final int? failAt;
  final String failCode;
  final int badBytes;
  int stepIndex = 0;

  @override
  String get kind => 'image';

  int get _bytesDone => stepIndex >= steps ? bytesTotal : bytesTotal ~/ steps ~/ 512 * 512 * stepIndex;

  @override
  bool step() {
    if (cancelRequested) {
      _done('cancelled', null);
      return true;
    }
    if (failAt != null && stepIndex == failAt) {
      _done('failed', _errorJson(failCode, _failureText(failCode)));
      return true;
    }
    if (stepIndex >= steps) {
      _done('completed', null);
      return true;
    }
    stepIndex++;
    const speed = 157286400;
    engine._event('image_progress', {
      'bytes_done': _bytesDone,
      'bytes_total': bytesTotal,
      'speed_bps': speed,
      'eta_seconds': ((bytesTotal - _bytesDone) / speed).ceil(),
      'elapsed_seconds': stepIndex * 7,
      'bad_bytes': badBytes * stepIndex ~/ steps,
    });
    return false;
  }

  void _done(String status, Map<String, Object?>? error) {
    engine.world.imageFiles.add(dest);
    engine._finishJob();
    final bad = badBytes * stepIndex ~/ steps;
    engine._event('image_done', {
      'status': status,
      'error': error,
      'bytes_written': _bytesDone,
      'bad_bytes': bad,
      'path': dest,
      'bad_map_path': bad > 0 ? '$dest.bad.txt' : null,
    });
  }
}
