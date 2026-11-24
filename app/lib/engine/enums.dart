/// The closed sets of strings used by the engine protocol (docs/PROTOCOL.md).
///
/// Every enum carries its wire name in [WireEnum.wire]. Enums that appear in
/// results and events have an `unknown` member: a value this version of the
/// app does not know (a newer engine) is shown as "unknown" instead of making
/// the whole message unreadable. Enums that are only sent (sort keys,
/// purposes) have no such member.
library;

/// An enum value with the string that represents it on the wire.
abstract interface class WireEnum {
  String get wire;
}

T _parse<T extends WireEnum>(List<T> values, String wire, T unknown) {
  for (final value in values) {
    if (value.wire == wire) return value;
  }
  return unknown;
}

/// `SourceInfo.kind`, and the `kind` argument of `open_source`.
enum SourceKind implements WireEnum {
  disk('disk'),
  image('image'),
  unknown('');

  const SourceKind(this.wire);
  @override
  final String wire;
  static SourceKind parse(String wire) => _parse(values, wire, unknown);
}

/// `Partition.fs`: the file system sniffed from the partition's first sector.
enum FileSystemKind implements WireEnum {
  ntfs('ntfs'),
  fat('fat'),
  exfat('exfat'),
  refs('refs'),
  bitlocker('bitlocker'),
  other('other'),
  unknown('');

  const FileSystemKind(this.wire);
  @override
  final String wire;
  static FileSystemKind parse(String wire) => _parse(values, wire, unknown);
}

/// `Disk.media`, from the seek-penalty property. `unknown` is a protocol value.
enum MediaKind implements WireEnum {
  hdd('hdd'),
  ssd('ssd'),
  unknown('unknown');

  const MediaKind(this.wire);
  @override
  final String wire;
  static MediaKind parse(String wire) => _parse(values, wire, unknown);
}

/// `Disk.flags`.
enum DiskFlag implements WireEnum {
  virtual('virtual'),
  spaces('spaces'),
  dynamicDisk('dynamic'),
  noMedia('no_media'),
  unknown('');

  const DiskFlag(this.wire);
  @override
  final String wire;
  static DiskFlag parse(String wire) => _parse(values, wire, unknown);
}

/// `Disk.table` and the `table` member of the `open_source` result.
enum PartitionTableKind implements WireEnum {
  mbr('mbr'),
  gpt('gpt'),
  none('none'),
  unknown('');

  const PartitionTableKind(this.wire);
  @override
  final String wire;
  static PartitionTableKind parse(String wire) => _parse(values, wire, unknown);
}

/// `Warning.code`.
enum WarningCode implements WireEnum {
  trimSupported('trim_supported'),
  mountedVolume('mounted_volume'),
  pageFileOnSource('page_file_on_source'),
  systemDisk('system_disk'),
  /// HDSavior itself runs from the disk being rescued. Raised by the app,
  /// never sent by the engine (docs/PROTOCOL.md, Warning).
  appOnSource('app_on_source'),
  sourceReadsZero('source_reads_zero'),
  carveHitsCapped('carve_hits_capped'),
  liveVolumeIncomplete('live_volume_incomplete'),
  writesNotDetectable('writes_not_detectable'),
  scanIncomplete('scan_incomplete'),
  outOfRangeFiles('out_of_range_files'),
  identityDiffers('identity_differs'),

  /// Records of an older file system could not be separated cleanly.
  mixedGenerations('mixed_generations'),

  /// The session file lies inside the lost volume: save the session
  /// elsewhere.
  sessionFileAtRisk('session_file_at_risk'),

  /// The session was written by an older engine; its times are approximate.
  oldSessionFormat('old_session_format'),

  /// The last write to the session file failed (disk full, disk gone): the
  /// file does not hold the session as it is; save it elsewhere.
  sessionFileNotSaved('session_file_not_saved'),

  /// `session_load`: the file ends early or holds no scanned data.
  sessionIncomplete('session_incomplete'),
  unknown('');

  const WarningCode(this.wire);
  @override
  final String wire;
  static WarningCode parse(String wire) => _parse(values, wire, unknown);
}

/// `Geometry.cluster_source`: where the cluster size comes from.
enum ClusterSource implements WireEnum {
  attr('attr'),
  boot('boot'),
  byDefault('default'),
  manual('manual'),
  unknown('');

  const ClusterSource(this.wire);
  @override
  final String wire;
  static ClusterSource parse(String wire) => _parse(values, wire, unknown);
}

/// `Geometry.offset_source`: where the volume offset comes from.
enum OffsetSource implements WireEnum {
  mft('mft'),

  /// Votes of directory index blocks (`index` on the wire; an enum value
  /// cannot be called `index` in Dart).
  indexVotes('index'),
  boot('boot'),
  backupBoot('backup_boot'),
  signature('signature'),
  byDefault('default'),
  inherited('inherited'),
  manual('manual'),
  unknown('');

  const OffsetSource(this.wire);
  @override
  final String wire;
  static OffsetSource parse(String wire) => _parse(values, wire, unknown);
}

/// `Instance.kind`: what kind of file system the scan found.
enum InstanceKind implements WireEnum {
  /// The file system that was lost by the format.
  main('main'),

  /// A file system from before the previous format.
  older('older'),

  /// Another volume (whole-disk scans).
  other('other'),

  /// A file system inside a disk-image file; browse only.
  embedded('embedded'),

  /// Records that belong to no reconstructed file system.
  strays('strays'),
  unknown('');

  const InstanceKind(this.wire);
  @override
  final String wire;
  static InstanceKind parse(String wire) => _parse(values, wire, unknown);
}

/// `kind` argument of `instance_set`: the user's verdict about one file
/// system.
enum InstanceOverride implements WireEnum {
  /// "This is the one I want": make it the main instance.
  main('main'),

  /// Treat an `embedded` or `older` instance as a volume of its own, so that
  /// its files can be recovered.
  other('other'),

  /// Withdraw the override and let the engine decide again.
  auto('auto');

  const InstanceOverride(this.wire);
  @override
  final String wire;
}

/// Why a range of the source is protected (`CatalogInfo.protected`), or why
/// a destination overlaps the source (`check_destination.reason`).
enum ProtectionReason implements WireEnum {
  /// The scanned (or imaged) range.
  sessionRange('session_range'),

  /// A lost file system with validated geometry. It can reach beyond the
  /// scanned range, into other partitions of the disk.
  lostVolume('lost_volume'),

  /// The image file that is the source (destinations only).
  sourceImage('source_image'),
  unknown('');

  const ProtectionReason(this.wire);
  @override
  final String wire;
  static ProtectionReason parse(String wire) => _parse(values, wire, unknown);
}

/// `Row.kind`.
enum NodeKind implements WireEnum {
  dir('dir'),
  file('file'),
  lostDir('lost_dir'),
  virtual('virtual'),
  carved('carved'),
  unknown('');

  const NodeKind(this.wire);
  @override
  final String wire;
  static NodeKind parse(String wire) => _parse(values, wire, unknown);

  /// Folders are `dir`, `lost_dir` and `virtual`; `file` and `carved` are
  /// files. An unknown kind is treated as a file (it cannot be opened).
  bool get isFolder =>
      this == NodeKind.dir ||
      this == NodeKind.lostDir ||
      this == NodeKind.virtual;
}

/// `Row.rating` (docs/ALGORITHMS.md, G3).
enum Rating implements WireEnum {
  good('good'),
  uncertain('uncertain'),
  damaged('damaged'),
  unsupported('unsupported'),
  unknown('');

  const Rating(this.wire);
  @override
  final String wire;
  static Rating parse(String wire) => _parse(values, wire, unknown);

  /// The four ratings of the protocol, best first (for filter controls).
  static const List<Rating> known = [good, uncertain, damaged, unsupported];
}

/// `Row.flags` (docs/ALGORITHMS.md, G1).
enum FileFlag implements WireEnum {
  deleted('deleted'),
  tornRecord('torn_record'),
  missingFragments('missing_fragments'),
  sizeEstimated('size_estimated'),
  outOfBounds('out_of_bounds'),
  overwritten('overwritten'),
  overlap('overlap'),
  compressed('compressed'),
  encrypted('encrypted'),
  placeholder('placeholder'),
  sparse('sparse'),
  noData('no_data'),
  badSectors('bad_sectors'),
  olderGeneration('older_generation'),
  embedded('embedded'),
  stray('stray'),
  signatureOk('signature_ok'),
  signatureMismatch('signature_mismatch'),
  unknown('');

  const FileFlag(this.wire);
  @override
  final String wire;
  static FileFlag parse(String wire) => _parse(values, wire, unknown);
}

/// `sort` argument of `list_children` and `search`.
enum SortKey implements WireEnum {
  name('name'),
  size('size'),
  modified('modified'),
  rating('rating'),
  ext('ext');

  const SortKey(this.wire);
  @override
  final String wire;
}

/// `scan_progress.phase`.
enum ScanPhase implements WireEnum {
  /// Reading where the old MFT is expected first.
  mftProbe('mft_probe'),

  /// Reading the rest of the range. Most of the tree is found by now, so
  /// "Stop and show results" becomes worthwhile.
  sweep('sweep'),

  /// Building the catalog; `job_cancel` is ignored in this phase.
  analyzing('analyzing'),
  unknown('');

  const ScanPhase(this.wire);
  @override
  final String wire;
  static ScanPhase parse(String wire) => _parse(values, wire, unknown);
}

/// `status` of the `*_done` events.
enum JobStatus implements WireEnum {
  completed('completed'),

  /// A scan that the user stopped: the catalog holds what was read.
  stopped('stopped'),

  /// A recover, carve or image job that the user cancelled.
  cancelled('cancelled'),
  failed('failed'),
  unknown('');

  const JobStatus(this.wire);
  @override
  final String wire;
  static JobStatus parse(String wire) => _parse(values, wire, unknown);
}

/// `job` member of the `*_start` results.
enum JobKind implements WireEnum {
  scan('scan'),
  recover('recover'),
  carve('carve'),
  image('image'),
  unknown('');

  const JobKind(this.wire);
  @override
  final String wire;
  static JobKind parse(String wire) => _parse(values, wire, unknown);
}

/// `verdict` of `check_destination`.
enum DestinationVerdict implements WireEnum {
  ok('ok'),

  /// Inside the session range: always refused, there is no override.
  overlapsSource('overlaps_source'),

  /// On the source disk outside the session range: needs `allow_same_disk`.
  sameDisk('same_disk'),

  /// The disk cannot be determined: needs `allow_unknown_disk`.
  unknownDisk('unknown_disk'),
  notFound('not_found'),
  notWritable('not_writable'),

  /// Not an absolute drive or UNC path.
  invalidPath('invalid_path'),
  unknown('');

  const DestinationVerdict(this.wire);
  @override
  final String wire;
  static DestinationVerdict parse(String wire) => _parse(values, wire, unknown);
}

/// `purpose` argument of `check_destination`.
enum DestinationPurpose implements WireEnum {
  recover('recover'),
  image('image'),
  session('session'),

  /// The UI's own directories (docs/ARCHITECTURE.md, S2).
  appData('app_data');

  const DestinationPurpose(this.wire);
  @override
  final String wire;
}

/// `on_exists` argument of `recover_start`: what to do with files that
/// existed at the destination before the run.
enum OnExists implements WireEnum {
  rename('rename'),
  skip('skip'),
  overwrite('overwrite');

  const OnExists(this.wire);
  @override
  final String wire;
}

/// `log.level`.
enum LogLevel implements WireEnum {
  info('info'),
  warn('warn'),
  error('error'),
  unknown('');

  const LogLevel(this.wire);
  @override
  final String wire;
  static LogLevel parse(String wire) => _parse(values, wire, unknown);
}

/// `task_progress.task`: a long plain request that reports progress.
enum TaskKind implements WireEnum {
  sessionLoad('session_load'),
  geometrySet('geometry_set'),
  unknown('');

  const TaskKind(this.wire);
  @override
  final String wire;
  static TaskKind parse(String wire) => _parse(values, wire, unknown);
}

/// Error codes: those of the protocol, plus codes the client itself raises
/// when the engine process is the problem (they never appear on the wire).
enum EngineErrorCode implements WireEnum {
  badRequest('bad_request'),
  unknownCommand('unknown_command'),
  busy('busy'),
  noSource('no_source'),
  noSession('no_session'),
  noCatalog('no_catalog'),
  staleCatalog('stale_catalog'),
  notFound('not_found'),
  ioError('io_error'),
  accessDenied('access_denied'),
  destinationRefused('destination_refused'),
  sourceMismatch('source_mismatch'),
  sourceLost('source_lost'),
  noSpace('no_space'),
  exists('exists'),
  unsupported('unsupported'),
  cancelled('cancelled'),
  internal('internal'),

  /// Client side: the engine process ended while the request was pending.
  engineExited('engine_exited'),

  /// Client side: the request was made while no engine process was running.
  engineNotRunning('engine_not_running'),

  /// Client side: the engine executable could not be started.
  engineStartFailed('engine_start_failed'),

  /// Client side: the engine answered with something the app cannot read.
  protocolError('protocol_error'),

  /// A code this version of the app does not know.
  unknown('');

  const EngineErrorCode(this.wire);
  @override
  final String wire;
  static EngineErrorCode parse(String wire) => _parse(values, wire, unknown);

  /// Whether the code says that the engine process itself is unavailable.
  bool get isEngineGone =>
      this == engineExited ||
      this == engineNotRunning ||
      this == engineStartFailed;
}
