/// The state of one kind of engine job (scan, recover, carve, image).
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';

/// Where a job is in its life.
enum JobPhase {
  /// No job, and no result to show.
  idle,

  /// The `*_start` request was sent and not answered yet.
  starting,

  /// The engine accepted the job; progress events arrive.
  running,

  /// `job_cancel` was sent; the job is still winding down.
  cancelling,

  /// The job has ended; [JobController.result] and [JobController.failure]
  /// say how.
  done,
}

/// The one job the engine can run at a time ("at most one job runs at a
/// time; starting another answers `busy`"). Screens use it to disable what
/// cannot be started right now.
///
/// [value] is the kind of the active job, `null` if none. A job counts from
/// the moment its start is requested; one whose start the engine refuses
/// (`busy`) never displaces the job that is really running.
class JobTracker extends ChangeNotifier implements ValueListenable<JobKind?> {
  /// The jobs that were requested and have not ended, oldest first.
  final List<JobKind> _active = [];

  @override
  JobKind? get value => _active.isEmpty ? null : _active.first;

  /// Whether a job of any kind is active.
  bool get isBusy => _active.isNotEmpty;

  /// A job of [kind] was requested.
  void enter(JobKind kind) {
    if (_active.contains(kind)) return;
    final before = value;
    _active.add(kind);
    if (value != before) notifyListeners();
  }

  /// The job of [kind] has ended or was refused.
  void leave(JobKind kind) {
    final before = value;
    if (_active.remove(kind) && value != before) notifyListeners();
  }
}

/// Follows one kind of job from `*_start` to `*_done`.
///
/// [S] is the result of the start command, [P] the progress event, [D] the
/// done event. Subclasses add the `start` method with the job's arguments and
/// react to the end of the job in [onDone].
///
/// The engine writes the response to `*_start` before the job's events, but
/// the code that awaits the response may run after the handler of an event
/// that follows closely (a resumed complete scan emits `scan_done` at once).
/// The controller therefore treats whatever arrives first as the sign that
/// the job was accepted, and never lets the late continuation overwrite a
/// newer state.
abstract class JobController<S, P extends JobProgressEvent, D extends JobDoneEvent>
    extends ChangeNotifier {
  JobController(this.api, {required this.kind, this.tracker}) {
    _events = api.client.events.listen(_onEvent);
    _exits = api.client.exits.listen(_onEngineExit);
  }

  @protected
  final EngineApi api;

  /// Which job this controller follows.
  final JobKind kind;

  /// Told when the job becomes active or ends; may be shared by all job
  /// controllers.
  final JobTracker? tracker;

  late final StreamSubscription<EngineEvent> _events;
  late final StreamSubscription<EngineExit> _exits;

  JobPhase _phase = JobPhase.idle;
  int _run = 0;
  int _acceptedRun = 0;
  S? _started;
  P? _progress;
  D? _result;
  EngineError? _failure;
  EngineException? _startError;

  JobPhase get phase => _phase;

  /// The job was requested or is running.
  bool get isActive =>
      _phase == JobPhase.starting ||
      _phase == JobPhase.running ||
      _phase == JobPhase.cancelling;

  /// The answer to the start command; `null` until it has arrived.
  S? get started => _started;

  /// The latest progress event of the current (or last) job.
  P? get progress => _progress;

  /// The done event of the last job; `null` while it runs, and when it ended
  /// without one (the engine died).
  D? get result => _result;

  /// How the last job ended: the status of its done event, `failed` if it
  /// ended without one, `null` if there is nothing to report.
  JobStatus? get status =>
      _result?.status ?? (_failure != null ? JobStatus.failed : null);

  /// Why the last job failed: the error of its done event, `engine_exited`
  /// if the engine died under it, or `protocol_error` if its done event
  /// could not be read. `null` otherwise.
  EngineError? get failure => _failure;

  /// Why the last start request was refused (`busy`, `destination_refused`
  /// with the check result, ...). Cleared by the next start.
  EngineException? get startError => _startError;

  /// Sends the start command through [send] and follows the job.
  ///
  /// Throws a [StateError] if this job is already active, and rethrows the
  /// [EngineException] of a refused start (also kept in [startError]).
  @protected
  Future<S> begin(Future<S> Function() send) async {
    if (isActive) throw StateError('A ${kind.wire} job is already active.');
    final run = ++_run;
    _phase = JobPhase.starting;
    _started = null;
    _progress = null;
    _result = null;
    _failure = null;
    _startError = null;
    tracker?.enter(kind);
    notifyListeners();
    try {
      final started = await send();
      if (run == _run) {
        _started = started;
        if (_phase == JobPhase.starting) _phase = JobPhase.running;
        _accepted();
        notifyListeners();
      }
      return started;
    } on EngineException catch (e) {
      if (run == _run && _phase == JobPhase.starting) {
        _phase = JobPhase.idle;
        _startError = e;
        _release();
        notifyListeners();
      }
      rethrow;
    }
  }

  /// Asks the engine to stop the job (`job_cancel`). The job then ends with
  /// its done event: `stopped` for a scan (its results are kept), `cancelled`
  /// for the others. Does nothing if the job is not active.
  Future<void> cancel() async {
    if (!isActive) return;
    if (_phase == JobPhase.running) {
      _phase = JobPhase.cancelling;
      notifyListeners();
    }
    try {
      await api.jobCancel();
    } on EngineException {
      // An engine that is gone has no job left; its exit ends this one.
    }
  }

  /// Forgets the last job's outcome ([JobPhase.done] becomes
  /// [JobPhase.idle]). Does nothing while a job is active.
  void reset() {
    if (isActive) return;
    _phase = JobPhase.idle;
    _started = null;
    _progress = null;
    _result = null;
    _failure = null;
    _startError = null;
    notifyListeners();
  }

  /// Called once per job when the engine has accepted it (its start command
  /// succeeded or its first event arrived, whichever comes first).
  @protected
  void onAccepted() {}

  /// Called when the job has ended with [done].
  @protected
  void onDone(D done) {}

  void _accepted() {
    if (_acceptedRun == _run) return;
    _acceptedRun = _run;
    onAccepted();
  }

  void _release() => tracker?.leave(kind);

  void _onEvent(EngineEvent event) {
    if (event is P) {
      // A progress event without a job of ours is ignored (there is none to
      // attribute it to).
      if (!isActive) return;
      if (_phase == JobPhase.starting) _phase = JobPhase.running;
      _progress = event;
      _accepted();
      notifyListeners();
    } else if (event is D) {
      if (isActive) _accepted();
      _result = event;
      _failure = event.error;
      _end();
      onDone(event);
      notifyListeners();
    } else if (event is MalformedEvent && event.doneOf == kind) {
      _result = null;
      _failure = EngineError(
        code: EngineErrorCode.protocolError.wire,
        message:
            'The engine reported the end of the job in a form this app does '
            'not understand (${event.problem.message}).',
      );
      _end();
      notifyListeners();
    }
  }

  void _onEngineExit(EngineExit exit) {
    if (!isActive) return;
    _result = null;
    _failure = EngineError(
      code: EngineErrorCode.engineExited.wire,
      message: exit.expected
          ? 'The engine was shut down while the job was running.'
          : 'The engine stopped unexpectedly while the job was running.',
    );
    _end();
    notifyListeners();
  }

  void _end() {
    // A continuation of begin() that runs after this only fills in
    // [started]: it changes the phase only while it is still `starting`.
    _phase = JobPhase.done;
    _release();
  }

  @override
  void dispose() {
    unawaited(_events.cancel());
    unawaited(_exits.cancel());
    super.dispose();
  }
}
