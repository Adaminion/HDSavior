/// The events of the engine protocol (docs/PROTOCOL.md, sections 3 and 4).
///
/// Events are the lines `{"event": name, "data": {...}}` that the engine
/// writes between responses. [parseEngineEvent] never throws: a name this
/// version does not know becomes an [UnknownEvent] (to be ignored), and a
/// known event with a wrong shape becomes a [MalformedEvent] (so that a job
/// whose `*_done` event cannot be read still ends in the UI).
library;

import 'enums.dart';
import 'json.dart';
import 'objects.dart';

/// Any event from the engine.
sealed class EngineEvent {
  const EngineEvent();

  /// The wire name, for example `scan_progress`.
  String get name;
}

/// A progress event of a job.
sealed class JobProgressEvent extends EngineEvent {
  const JobProgressEvent();

  JobKind get job;

  /// Completed share of the work, 0 to 1; `null` when the total is unknown.
  double? get fraction;

  /// Estimated remaining seconds; `null` when unknown.
  int? get etaSeconds;
}

/// The last event of a job. It is written after the job slot is free and the
/// resulting state (catalog, session file) can be queried.
sealed class JobDoneEvent extends EngineEvent {
  const JobDoneEvent();

  JobKind get job;
  JobStatus get status;

  /// Set when [status] is `failed`.
  EngineError? get error;
}

double? _fraction(int done, int total) =>
    total <= 0 ? null : (done / total).clamp(0.0, 1.0);

// --- scan --------------------------------------------------------------------

/// `scan_progress`.
final class ScanProgress extends JobProgressEvent {
  const ScanProgress({
    required this.phase,
    required this.bytesDone,
    required this.bytesTotal,
    required this.position,
    required this.speedBps,
    this.etaSeconds,
    required this.elapsedSeconds,
    required this.records,
    required this.files,
    required this.dirs,
    required this.indexBlocks,
    required this.carveHits,
    required this.badBytes,
    required this.warnings,
  });

  factory ScanProgress.fromJson(JsonObject json) => ScanProgress(
    phase: ScanPhase.parse(json.string('phase')),
    bytesDone: json.integer('bytes_done'),
    bytesTotal: json.integer('bytes_total'),
    position: json.integer('position'),
    speedBps: json.integer('speed_bps'),
    etaSeconds: json.integerOrNull('eta_seconds'),
    elapsedSeconds: json.integer('elapsed_seconds'),
    records: json.integer('records'),
    files: json.integer('files'),
    dirs: json.integer('dirs'),
    indexBlocks: json.integer('index_blocks'),
    carveHits: json.integer('carve_hits'),
    badBytes: json.integer('bad_bytes'),
    warnings: json.objects('warnings', Warning.fromJson),
  );

  static const String eventName = 'scan_progress';

  final ScanPhase phase;

  /// In phase `analyzing` these two describe the analysis steps, not bytes.
  final int bytesDone;
  final int bytesTotal;

  /// Absolute offset of the last chunk read.
  final int position;

  /// Bytes per second, averaged over the last 5 s.
  final int speedBps;

  /// `null` in phase `analyzing` or at speed 0.
  @override
  final int? etaSeconds;

  /// Whole seconds of this run.
  final int elapsedSeconds;

  /// MFT records found so far.
  final int records;

  /// In-use files found so far.
  final int files;

  /// In-use folders found so far.
  final int dirs;
  final int indexBlocks;
  final int carveHits;

  /// Bytes in unreadable sectors so far.
  final int badBytes;
  final List<Warning> warnings;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.scan;

  @override
  double? get fraction => _fraction(bytesDone, bytesTotal);
}

/// `scan_done`.
final class ScanDone extends JobDoneEvent {
  const ScanDone({
    required this.status,
    this.error,
    this.sessionPath,
    this.catalog,
  });

  factory ScanDone.fromJson(JsonObject json) => ScanDone(
    status: JobStatus.parse(json.string('status')),
    error: EngineError.fromMember(json, 'error'),
    sessionPath: json.stringOrNull('session_path'),
    catalog: switch (json.objectOrNull('catalog')) {
      null => null,
      final catalog => CatalogInfo.fromJson(catalog),
    },
  );

  static const String eventName = 'scan_done';

  /// `completed`; `stopped` (cancelled by the user: the catalog holds what
  /// was read and the scan can be resumed); or `failed`.
  @override
  final JobStatus status;

  /// Set when failed. `source_lost` keeps the session resumable.
  @override
  final EngineError? error;

  /// Where the session was saved; `null` if the scan had no session path.
  final String? sessionPath;

  /// `null` when the scan failed.
  final CatalogInfo? catalog;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.scan;
}

// --- recover -----------------------------------------------------------------

/// `recover_progress`.
final class RecoverProgress extends JobProgressEvent {
  const RecoverProgress({
    required this.filesDone,
    required this.filesTotal,
    required this.bytesDone,
    required this.bytesTotal,
    this.current,
    required this.speedBps,
    this.etaSeconds,
    required this.elapsedSeconds,
    required this.ok,
    required this.warnings,
    required this.failed,
    required this.skipped,
  });

  factory RecoverProgress.fromJson(JsonObject json) => RecoverProgress(
    filesDone: json.integer('files_done'),
    filesTotal: json.integer('files_total'),
    bytesDone: json.integer('bytes_done'),
    bytesTotal: json.integer('bytes_total'),
    current: json.stringOrNull('current'),
    speedBps: json.integer('speed_bps'),
    etaSeconds: json.integerOrNull('eta_seconds'),
    elapsedSeconds: json.integer('elapsed_seconds'),
    ok: json.integer('ok'),
    warnings: json.integer('warnings'),
    failed: json.integer('failed'),
    skipped: json.integer('skipped'),
  );

  static const String eventName = 'recover_progress';

  final int filesDone;
  final int filesTotal;
  final int bytesDone;
  final int bytesTotal;

  /// Display path of the file being written; `null` between files.
  final String? current;
  final int speedBps;
  @override
  final int? etaSeconds;
  final int elapsedSeconds;

  /// Files written and rated good so far.
  final int ok;

  /// Files written with a warning so far.
  final int warnings;
  final int failed;
  final int skipped;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.recover;

  @override
  double? get fraction =>
      bytesTotal > 0
      ? _fraction(bytesDone, bytesTotal)
      : _fraction(filesDone, filesTotal);
}

/// `recover_done`.
final class RecoverDone extends JobDoneEvent {
  const RecoverDone({
    required this.status,
    this.error,
    required this.filesTotal,
    required this.ok,
    required this.warnings,
    required this.failed,
    required this.skipped,
    required this.bytesWritten,
    required this.dest,
    this.reportPath,
  });

  factory RecoverDone.fromJson(JsonObject json) => RecoverDone(
    status: JobStatus.parse(json.string('status')),
    error: EngineError.fromMember(json, 'error'),
    filesTotal: json.integer('files_total'),
    ok: json.integer('ok'),
    warnings: json.integer('warnings'),
    failed: json.integer('failed'),
    skipped: json.integer('skipped'),
    bytesWritten: json.integer('bytes_written'),
    dest: json.string('dest'),
    reportPath: json.stringOrNull('report_path'),
  );

  static const String eventName = 'recover_done';

  /// `completed`, `cancelled` or `failed` (`no_space`, `source_lost`, ...).
  @override
  final JobStatus status;
  @override
  final EngineError? error;
  final int filesTotal;

  /// `ok + warnings + failed + skipped == filesTotal` when completed.
  final int ok;
  final int warnings;
  final int failed;
  final int skipped;
  final int bytesWritten;

  /// The destination folder.
  final String dest;

  /// The CSV report with one row per file; `null` if none was written.
  final String? reportPath;

  /// Files that were not reached (after a cancel or a failure).
  int get notAttempted => filesTotal - ok - warnings - failed - skipped;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.recover;
}

// --- carve -------------------------------------------------------------------

/// `carve_progress`.
final class CarveProgress extends JobProgressEvent {
  const CarveProgress({
    required this.candidatesDone,
    required this.candidatesTotal,
    required this.found,
    required this.bytesFound,
    this.etaSeconds,
  });

  factory CarveProgress.fromJson(JsonObject json) => CarveProgress(
    candidatesDone: json.integer('candidates_done'),
    candidatesTotal: json.integer('candidates_total'),
    found: json.integer('found'),
    bytesFound: json.integer('bytes_found'),
    etaSeconds: json.integerOrNull('eta_seconds'),
  );

  static const String eventName = 'carve_progress';

  final int candidatesDone;
  final int candidatesTotal;

  /// Files found so far.
  final int found;
  final int bytesFound;
  @override
  final int? etaSeconds;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.carve;

  @override
  double? get fraction => _fraction(candidatesDone, candidatesTotal);
}

/// `carve_done`.
final class CarveDone extends JobDoneEvent {
  const CarveDone({
    required this.status,
    this.error,
    required this.found,
    required this.bytesFound,
    this.catalog,
  });

  factory CarveDone.fromJson(JsonObject json) => CarveDone(
    status: JobStatus.parse(json.string('status')),
    error: EngineError.fromMember(json, 'error'),
    found: json.integer('found'),
    bytesFound: json.integer('bytes_found'),
    catalog: switch (json.objectOrNull('catalog')) {
      null => null,
      final catalog => CatalogInfo.fromJson(catalog),
    },
  );

  static const String eventName = 'carve_done';

  @override
  final JobStatus status;
  @override
  final EngineError? error;

  /// Carved files in the catalog after the run (results found before a
  /// cancel are kept).
  final int found;
  final int bytesFound;

  /// The catalog after carving (same epoch, new carved nodes).
  final CatalogInfo? catalog;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.carve;
}

// --- image -------------------------------------------------------------------

/// `image_progress`.
final class ImageProgress extends JobProgressEvent {
  const ImageProgress({
    required this.bytesDone,
    required this.bytesTotal,
    required this.speedBps,
    this.etaSeconds,
    required this.elapsedSeconds,
    required this.badBytes,
  });

  factory ImageProgress.fromJson(JsonObject json) => ImageProgress(
    bytesDone: json.integer('bytes_done'),
    bytesTotal: json.integer('bytes_total'),
    speedBps: json.integer('speed_bps'),
    etaSeconds: json.integerOrNull('eta_seconds'),
    elapsedSeconds: json.integer('elapsed_seconds'),
    badBytes: json.integer('bad_bytes'),
  );

  static const String eventName = 'image_progress';

  final int bytesDone;
  final int bytesTotal;
  final int speedBps;
  @override
  final int? etaSeconds;
  final int elapsedSeconds;

  /// Bytes in unreadable sectors so far (zero-filled in the image).
  final int badBytes;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.image;

  @override
  double? get fraction => _fraction(bytesDone, bytesTotal);
}

/// `image_done`.
final class ImageDone extends JobDoneEvent {
  const ImageDone({
    required this.status,
    this.error,
    required this.bytesWritten,
    required this.badBytes,
    required this.path,
    this.badMapPath,
  });

  factory ImageDone.fromJson(JsonObject json) => ImageDone(
    status: JobStatus.parse(json.string('status')),
    error: EngineError.fromMember(json, 'error'),
    bytesWritten: json.integer('bytes_written'),
    badBytes: json.integer('bad_bytes'),
    path: json.string('path'),
    badMapPath: json.stringOrNull('bad_map_path'),
  );

  static const String eventName = 'image_done';

  @override
  final JobStatus status;
  @override
  final EngineError? error;
  final int bytesWritten;
  final int badBytes;

  /// The image file.
  final String path;

  /// List of the unreadable sectors; `null` when there were none.
  final String? badMapPath;

  @override
  String get name => eventName;

  @override
  JobKind get job => JobKind.image;
}

// --- other events ------------------------------------------------------------

/// `task_progress`: progress of a long plain request (`session_load`,
/// `geometry_set`), during which later requests wait.
final class TaskProgress extends EngineEvent {
  const TaskProgress({required this.taskName, required this.fraction});

  factory TaskProgress.fromJson(JsonObject json) => TaskProgress(
    taskName: json.string('task'),
    fraction: json.number('fraction').clamp(0.0, 1.0),
  );

  static const String eventName = 'task_progress';

  /// The task as sent; [task] is the typed form.
  final String taskName;

  /// 0 to 1.
  final double fraction;

  TaskKind get task => TaskKind.parse(taskName);

  @override
  String get name => eventName;
}

/// `log`: an informational message from the engine.
final class LogEvent extends EngineEvent {
  const LogEvent({required this.level, required this.message});

  factory LogEvent.fromJson(JsonObject json) => LogEvent(
    level: LogLevel.parse(json.string('level')),
    message: json.string('message'),
  );

  static const String eventName = 'log';

  final LogLevel level;
  final String message;

  @override
  String get name => eventName;
}

/// An event this version of the app does not know. It is ignored.
final class UnknownEvent extends EngineEvent {
  const UnknownEvent(this.name, this.data);

  @override
  final String name;

  /// The `data` member as decoded from JSON.
  final Object? data;
}

/// A known event whose data does not have the documented shape.
final class MalformedEvent extends EngineEvent {
  const MalformedEvent(this.name, this.problem);

  @override
  final String name;

  /// What is wrong with it.
  final ProtocolFormatException problem;

  /// The job whose `*_done` event this was meant to be, else `null`. A job
  /// controller treats it as the end of that job with a `protocol_error`.
  JobKind? get doneOf => switch (name) {
    ScanDone.eventName => JobKind.scan,
    RecoverDone.eventName => JobKind.recover,
    CarveDone.eventName => JobKind.carve,
    ImageDone.eventName => JobKind.image,
    _ => null,
  };
}

/// Converts the `event` name and `data` member of an event line.
EngineEvent parseEngineEvent(String name, Object? data) {
  final EngineEvent Function(JsonObject json) fromJson;
  switch (name) {
    case ScanProgress.eventName:
      fromJson = ScanProgress.fromJson;
    case ScanDone.eventName:
      fromJson = ScanDone.fromJson;
    case RecoverProgress.eventName:
      fromJson = RecoverProgress.fromJson;
    case RecoverDone.eventName:
      fromJson = RecoverDone.fromJson;
    case CarveProgress.eventName:
      fromJson = CarveProgress.fromJson;
    case CarveDone.eventName:
      fromJson = CarveDone.fromJson;
    case ImageProgress.eventName:
      fromJson = ImageProgress.fromJson;
    case ImageDone.eventName:
      fromJson = ImageDone.fromJson;
    case TaskProgress.eventName:
      fromJson = TaskProgress.fromJson;
    case LogEvent.eventName:
      fromJson = LogEvent.fromJson;
    default:
      return UnknownEvent(name, data);
  }
  try {
    return fromJson(JsonObject.root(data, name));
  } on ProtocolFormatException catch (problem) {
    return MalformedEvent(name, problem);
  }
}
