/// The machine the fake engine pretends to run on: its disks, and the files
/// it has "written" (sessions, images). A [FakeWorld] outlives a fake engine
/// process, as a real disk does, so a restarted engine finds the session file
/// its predecessor saved.
///
/// Behaviour is chosen through names, so that it can be triggered from the
/// running app as well as from tests:
///
/// Destinations (`check_destination` and every command with a destination),
/// by drive letter or by first path component:
///
/// | location                                   | verdict                               |
/// |--------------------------------------------|---------------------------------------|
/// | the image file that is the source          | `overlaps_source`, `source_image`     |
/// | a volume of the source disk inside a       | `overlaps_source` with the reason of  |
/// | protected range (see below)                | the first range it meets              |
/// | another volume of the source disk          | `same_disk` (`ok` for `session` and   |
/// |                                            | `app_data` on the system volume, S3)  |
/// | `/overlap/...`                             | `overlaps_source`, `session_range`    |
/// | `L:\...`, `/lost-volume/...`               | inside the lost volume of the main    |
/// |                                            | file system: `overlaps_source`,       |
/// |                                            | `lost_volume`, with lost files; while |
/// |                                            | no such volume is known (no catalog,  |
/// |                                            | geometry not validated) `same_disk`   |
/// | `S:\...`, `/same-disk/...`                 | `same_disk`                           |
/// | `U:\...`, `/unknown/...`, `\\localhost\..` | `unknown_disk`                        |
/// | `Q:\...`, `/missing/...`                   | `not_found`                           |
/// | `R:\...`, `/readonly/...`                  | `not_writable`                        |
/// | `T:\...`, `/tiny/...`                      | `ok`, but only 1 MiB free             |
/// | `K:\...`, `/fat32/...`                     | `ok`, FAT32 with its 4 GiB file limit |
/// | anything that is not absolute              | `invalid_path`                        |
/// | everything else                            | `ok`                                  |
///
/// The protected ranges are what the guard protects at that moment
/// (docs/ARCHITECTURE.md, S2): with a catalog its `protected` list (the
/// session range and the lost volumes); during a scan the session range; for
/// `scan_start` and `image_start` the range of the new job as well. Before a
/// session exists (state O) the fake protects the whole source, because the
/// next scan or image covers all of it unless told otherwise. That last rule
/// is the fake's choice: the protocol leaves open what the guard says about
/// the source disk before a range is known, and the job's own check is the
/// one that counts.
///
/// Image files (`open_source` with kind `image`), by file name: `missing` ->
/// `not_found`; `denied` -> `access_denied`; `nopart` -> no partition table;
/// `gpt` -> GPT with two partitions; `big` -> 4 TB; otherwise an 8 GiB image
/// with an MBR and one NTFS partition.
///
/// Session files (`session_load`), by file name, unless the path was saved
/// by the fake itself: `missing` -> `not_found`; `corrupt` -> `io_error`;
/// `newer` -> `unsupported`; `detached` -> loads without a source;
/// `identity` -> loads with an `identity_differs` warning; `partial` -> an
/// incomplete scan; otherwise a complete session of disk 1.
///
/// Sources for `session_attach_source` (and the `source` of
/// `session_load`, where the mismatch is reported in `source_error`), by
/// path: `mismatch` -> `source_mismatch` (`failed: ["records"]`) always;
/// `fewsamples` -> `source_mismatch` (`failed: ["too_few_samples"]`) unless
/// forced.
library;

const int _kib = 1024;
const int _mib = 1024 * _kib;
const int _gib = 1024 * _mib;

Map<String, Object?> _warning(String code, String message) => {'code': code, 'message': message};

/// A session file the fake has written.
final class FakeSavedSession {
  FakeSavedSession({
    required this.source,
    required this.rangeStart,
    required this.rangeEnd,
    required this.bytesDone,
    required this.collectCarveHits,
  });

  /// PROTOCOL `SourceInfo` of the scanned source.
  final Map<String, Object?> source;
  final int rangeStart;
  final int rangeEnd;
  final int bytesDone;
  final bool collectCarveHits;
}

/// See the library documentation.
final class FakeWorld {
  FakeWorld();

  /// Session files written by `scan_start` / `session_save`, by path.
  final Map<String, FakeSavedSession> savedSessions = {};

  /// Image files written by `image_start`, by path.
  final Set<String> imageFiles = {};

  /// The physical disks of the pretended machine (PROTOCOL `Disk`).
  late final List<Map<String, Object?>> disks = _buildDisks();

  static Map<String, Object?> _partition(
    int index,
    int offset,
    int size,
    String type,
    String typeId,
    String? fs, {
    List<String> letters = const [],
    String? label,
  }) => {
    'index': index,
    'offset': offset,
    'size': size,
    'type': type,
    'type_id': typeId,
    'fs': fs,
    'ntfs': fs == 'ntfs'
        ? {
            'cluster_size': 4096,
            'total_sectors': size ~/ 512 - 1,
            'serial': (0xA1B2C3D4E5F60718 ^ (offset * 31)).toRadixString(16).toUpperCase().padLeft(16, '0'),
          }
        : null,
    'letters': letters,
    'label': label,
  };

  static const String _basicData = 'EBD0A0A2-B9E5-4433-87C0-68B6B72699C7';

  static List<Map<String, Object?>> _buildDisks() {
    Map<String, Object?> disk({
      required int number,
      required int size,
      required String model,
      required String serial,
      required String bus,
      required String media,
      required bool? trim,
      required String table,
      required List<Map<String, Object?>> partitions,
      bool removable = false,
      bool isSystem = false,
      List<String> flags = const [],
      int physicalSectorSize = 4096,
    }) => {
      'kind': 'disk',
      'path': '\\\\.\\PhysicalDrive$number',
      'size': size,
      'sector_size': 512,
      'physical_sector_size': physicalSectorSize,
      'model': model,
      'serial': serial,
      'disk_number': number,
      'number': number,
      'bus': bus,
      'removable': removable,
      'is_system': isSystem,
      'media': media,
      'trim': trim,
      'flags': flags,
      'table': table,
      'partitions': partitions,
    };

    return [
      // The system disk: an SSD with TRIM. Scanning it is discouraged.
      disk(
        number: 0,
        size: 512110190592,
        model: 'Samsung SSD 980 PRO 500GB',
        serial: 'S5GXNX0R000000',
        bus: 'NVMe',
        media: 'ssd',
        trim: true,
        isSystem: true,
        table: 'gpt',
        physicalSectorSize: 512,
        partitions: [
          _partition(1, _mib, 100 * _mib, 'EFI system', 'C12A7328-F81F-11D2-BA4B-00A0C93EC93B', 'fat'),
          _partition(2, 101 * _mib, 16 * _mib, 'Microsoft reserved', 'E3C9E316-0B5C-4DB8-817D-F92DF00215AE', null),
          _partition(3, 117 * _mib, 487485 * _mib - 117 * _mib, 'Basic data', _basicData, 'ntfs', letters: ['C:'], label: 'Windows'),
          _partition(4, 487485 * _mib, 900 * _mib, 'Windows recovery', 'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC', 'ntfs'),
        ],
      ),
      // The disk to rescue: a 4 TB USB hard disk that was just formatted.
      disk(
        number: 1,
        size: 4000787030016,
        model: 'WDC WD40EZRZ-00GXCB0',
        serial: 'WD-WCC7K0000000',
        bus: 'USB',
        media: 'hdd',
        trim: false,
        table: 'gpt',
        partitions: [
          _partition(1, _mib, 4000785104896, 'Basic data', _basicData, 'ntfs', letters: ['E:'], label: 'New Volume'),
        ],
      ),
      // An internal hard disk with an MBR and a mix of file systems.
      disk(
        number: 2,
        size: 1000204886016,
        model: 'ST1000DM010-2EP102',
        serial: 'ZN1A0000',
        bus: 'SATA',
        media: 'hdd',
        trim: false,
        table: 'mbr',
        partitions: [
          _partition(1, _mib, 500 * _gib, 'NTFS / exFAT', '07', 'ntfs', letters: ['D:'], label: 'Data'),
          _partition(2, _mib + 500 * _gib, 200 * _gib, 'NTFS / exFAT', '07', 'exfat', letters: ['F:'], label: 'Media'),
          _partition(3, _mib + 700 * _gib, 100 * _gib, 'FAT32 (LBA)', '0C', 'fat', letters: ['G:']),
          _partition(4, _mib + 800 * _gib, 100 * _gib, 'NTFS / exFAT', '07', 'bitlocker', letters: ['I:']),
          _partition(5, _mib + 900 * _gib, 20 * _gib, 'Linux', '83', 'other'),
          _partition(6, _mib + 920 * _gib, 10 * _gib, 'NTFS / exFAT', '07', null),
        ],
      ),
      // A USB stick without partition table (one pseudo-partition, index 0).
      disk(
        number: 3,
        size: 31914983424,
        model: 'SanDisk Ultra',
        serial: '4C530001000000000000',
        bus: 'USB',
        media: 'unknown',
        trim: null,
        removable: true,
        table: 'none',
        physicalSectorSize: 512,
        partitions: [
          _partition(0, 0, 31914983424, '', '', 'fat', letters: ['H:'], label: 'STICK'),
        ],
      ),
      // A mounted virtual disk.
      disk(
        number: 4,
        size: 64 * _gib,
        model: 'Msft Virtual Disk',
        serial: '',
        bus: 'Virtual',
        media: 'unknown',
        trim: true,
        flags: ['virtual'],
        table: 'mbr',
        partitions: [
          _partition(1, _mib, 64 * _gib - _mib, 'NTFS / exFAT', '07', 'ntfs', letters: ['V:'], label: 'VM data'),
        ],
      ),
      // A card reader without a card.
      disk(
        number: 5,
        size: 0,
        model: 'Generic STORAGE DEVICE',
        serial: '000000000821',
        bus: 'USB',
        media: 'unknown',
        trim: null,
        removable: true,
        flags: ['no_media'],
        table: 'none',
        physicalSectorSize: 512,
        partitions: [],
      ),
      // A member of a Storage Spaces pool, and a dynamic disk.
      disk(
        number: 6,
        size: 2000398934016,
        model: 'TOSHIBA DT01ACA200',
        serial: 'Z4E0000AS',
        bus: 'SATA',
        media: 'hdd',
        trim: false,
        flags: ['spaces', 'dynamic'],
        table: 'gpt',
        partitions: [
          _partition(1, _mib, 2000397795328, 'Storage Spaces', 'E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D', 'other'),
        ],
      ),
    ];
  }

  Map<String, Object?>? diskByNumber(int number) {
    for (final disk in disks) {
      if (disk['number'] == number) return disk;
    }
    return null;
  }

  /// What `list_disks` says about [disk], which is the complete truth (the
  /// partition table that `open_source` reads): "The disk list is made
  /// without reading a single sector, so its partitions are the volumes
  /// Windows exposes on that disk: index is Windows' partition number, fs is
  /// the mounted file system (null for a raw volume), type and type_id are
  /// empty, ntfs is null, and partitions without a volume are missing."
  ///
  /// Partitions without a volume: the Microsoft reserved partition, foreign
  /// ones (Linux) and Storage Spaces members. A locked BitLocker volume and
  /// an empty one are raw volumes (`fs` null).
  static Map<String, Object?> listDisksEntry(Map<String, Object?> disk) => {
    ...disk,
    'partitions': [
      for (final p in (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>())
        if (_hasVolume(p))
          {
            ...p,
            'type': '',
            'type_id': '',
            'fs': switch (p['fs']) {
              'ntfs' || 'fat' || 'exfat' || 'refs' => p['fs'],
              _ => null,
            },
            'ntfs': null,
          },
    ],
  };

  static bool _hasVolume(Map<String, Object?> partition) => switch (partition['type']) {
    'Microsoft reserved' || 'Linux' || 'Storage Spaces' => false,
    _ => true,
  };

  /// PROTOCOL `SourceInfo` of a disk entry.
  static Map<String, Object?> sourceInfoOf(Map<String, Object?> disk) => {
    for (final key in const [
      'kind', 'path', 'size', 'sector_size', 'physical_sector_size', //
      'model', 'serial', 'disk_number',
    ])
      key: disk[key],
  };

  /// The `open_source` warnings for a disk.
  static List<Map<String, Object?>> diskWarnings(Map<String, Object?> disk) {
    final partitions = (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>();
    final mounted = [
      for (final p in partitions) ...(p['letters']! as List<Object?>).cast<String>(),
    ];
    return [
      if (disk['trim'] == true)
        _warning(
          'trim_supported',
          'This device supports TRIM. A format on such a device usually discards the data for good.',
        ),
      if (mounted.isNotEmpty)
        _warning(
          'mounted_volume',
          'Windows is using a volume on this disk (${mounted.join(', ')}). Do not save anything to it.',
        ),
      if (disk['is_system'] == true) ...[
        _warning('system_disk', 'This disk holds Windows itself. Windows keeps writing to it.'),
        _warning('page_file_on_source', 'A page file is on this disk. It is being overwritten constantly.'),
        _warning('app_on_source', 'HDSavior itself runs from this disk.'),
      ],
    ];
  }

  /// The image file at [path]: `(SourceInfo, table, partitions)`.
  static (Map<String, Object?>, String, List<Map<String, Object?>>) image(String path, int? sectorSize) {
    final name = fileName(path).toLowerCase();
    final size = name.contains('big') ? 4000787030016 : 8 * _gib;
    final source = <String, Object?>{
      'kind': 'image',
      'path': path,
      'size': size,
      'sector_size': sectorSize ?? 512,
      'physical_sector_size': sectorSize ?? 512,
      'model': null,
      'serial': null,
      'disk_number': null,
    };
    if (name.contains('nopart')) {
      return (source, 'none', [_partition(0, 0, size, '', '', 'ntfs')]);
    }
    if (name.contains('gpt')) {
      return (
        source,
        'gpt',
        [
          _partition(1, _mib, size ~/ 2, 'Basic data', _basicData, 'ntfs'),
          _partition(2, _mib + size ~/ 2, size ~/ 2 - 2 * _mib, 'Basic data', _basicData, 'exfat'),
        ],
      );
    }
    return (source, 'mbr', [_partition(1, _mib, size - _mib, 'NTFS / exFAT', '07', 'ntfs')]);
  }

  /// The last component of a Windows or POSIX path.
  static String fileName(String path) {
    final cut = path.lastIndexOf(RegExp(r'[\\/]'));
    return cut < 0 ? path : path.substring(cut + 1);
  }

  /// Joins a directory and a file name with the separator the directory uses.
  static String join(String directory, String name) {
    final separator = directory.contains(r'\') || RegExp(r'^[A-Za-z]:').hasMatch(directory) ? r'\' : '/';
    return directory.endsWith(separator) ? '$directory$name' : '$directory$separator$name';
  }

  /// What names the volume of [path]: an upper-case drive letter (`D`), a
  /// UNC server (`unc:server`), the first component of a POSIX path
  /// (`/tmp`), or `null` if the path is not absolute.
  static String? volumeOf(String path) {
    final drive = RegExp(r'^([A-Za-z]):[\\/]').firstMatch(path);
    if (drive != null) return drive.group(1)!.toUpperCase();
    final unc = RegExp(r'^\\\\([^\\/]+)[\\/][^\\/]+').firstMatch(path);
    if (unc != null) return 'unc:${unc.group(1)!.toLowerCase()}';
    final posix = RegExp(r'^/([^/]*)').firstMatch(path);
    if (posix != null) return '/${posix.group(1)}';
    return null;
  }

  /// PROTOCOL `check_destination` result for [path].
  ///
  /// [sourceDisk] is the `Disk` entry of the open source (`null` for an
  /// image file), [sourceImage] the path of the image file that is the
  /// source (`null` for a disk). [protected] are the ranges the guard
  /// protects right now (see the library documentation). [filesIn] counts
  /// the catalog files, and their bytes, stored in an absolute byte range of
  /// the source; `null` when there is no catalog.
  Map<String, Object?> checkDestination(
    String path, {
    required String purpose,
    required int? needBytes,
    required Map<String, Object?>? sourceDisk,
    String? sourceImage,
    List<FakeProtectedRange> protected = const [],
    (int, int) Function(int start, int end)? filesIn,
  }) {
    Map<String, Object?> result(
      String verdict,
      String message, {
      String? reason,
      int? free = 8000000000000,
      String? fs = 'NTFS',
      int? maxFileBytes,
      List<int> disks = const [0],
      (int, int) lost = (0, 0),
    }) => {
      'verdict': verdict,
      'reason': reason,
      'message': message,
      'free_bytes': free,
      'enough_space': needBytes == null || free == null ? null : free >= needBytes,
      'fs': fs,
      'max_file_bytes': maxFileBytes,
      'disk_numbers': disks,
      'lost_files': lost.$1,
      'lost_bytes': lost.$2,
    };

    final volume = volumeOf(path);
    if (volume == null) {
      return result(
        'invalid_path',
        'Enter a full path that starts with a drive letter (D:\\...) or a network share (\\\\server\\share\\...).',
        free: null,
        fs: null,
        disks: const [],
      );
    }
    if (sourceImage != null && _samePath(path, sourceImage)) {
      return result(
        'overlaps_source',
        'This is the disk image that is being rescued. HDSavior never writes to it.',
        reason: 'source_image',
        disks: const [0],
      );
    }

    final sourceNumber = sourceDisk?['number'] as int?;
    (int, int) lostIn(int start, int end) => filesIn?.call(start, end) ?? (0, 0);
    FakeProtectedRange? firstHit(int start, int end) {
      for (final range in protected) {
        if (range.start < end && start < range.end) return range;
      }
      return null;
    }

    Map<String, Object?> overlaps(String reason, (int, int) lost) => result(
      'overlaps_source',
      switch (reason) {
        'lost_volume' =>
          'This location is inside the file system that was lost, which reaches beyond the scanned area. '
              'Saving there would overwrite files you may want back. Use another disk.',
        _ => 'This location is on the part of the disk that is being rescued. '
            'Saving there would overwrite the files you want back.',
      },
      reason: reason,
      disks: [?sourceNumber],
      lost: lost,
    );

    // A volume of the source disk: protected, or the same disk.
    if (sourceDisk != null) {
      for (final p in (sourceDisk['partitions']! as List<Object?>).cast<Map<String, Object?>>()) {
        final letters = (p['letters']! as List<Object?>).cast<String>().map((l) => l[0].toUpperCase());
        if (!letters.contains(volume)) continue;
        final start = p['offset']! as int;
        final end = start + (p['size']! as int);
        final hit = firstHit(start, end);
        if (hit != null) return overlaps(hit.reason, lostIn(start, end));
        final systemVolume = sourceDisk['is_system'] == true && volume == 'C';
        if (systemVolume && (purpose == 'session' || purpose == 'app_data')) {
          return result('ok', '', disks: [?sourceNumber]);
        }
        return result(
          'same_disk',
          'This location is on the disk that is being rescued, outside the part that is protected. '
              'Use another disk if you can.',
          disks: [?sourceNumber],
          lost: lostIn(start, end),
        );
      }
    }

    switch (volume) {
      case '/overlap':
        final range = protected.isEmpty ? null : protected.first;
        return overlaps('session_range', range == null ? (0, 0) : lostIn(range.start, range.end));
      case 'L' || '/lost-volume':
        final lostVolumes = [for (final range in protected) if (range.main) range];
        if (lostVolumes.isEmpty) {
          return result(
            'same_disk',
            filesIn == null
                ? 'This location is on the disk that is being rescued. Until a scan has found the lost file '
                      'system nobody knows how far it reached. Use another disk if you can.'
                : 'This location is on the disk that is being rescued, outside the part that is protected. '
                      'Use another disk if you can.',
            disks: [?sourceNumber],
          );
        }
        // A volume that covers the last tenth of the main lost file system.
        final reach = lostVolumes.first;
        return overlaps('lost_volume', lostIn(reach.end - (reach.end - reach.start) ~/ 10, reach.end));
      case 'S' || '/same-disk':
        return result(
          'same_disk',
          'This location is on the same physical disk as the rescued volume. Use another disk if you can.',
          disks: [?sourceNumber],
        );
      case 'U' || '/unknown' || 'unc:localhost' || 'unc:127.0.0.1':
        return result(
          'unknown_disk',
          'HDSavior cannot tell which disk this location is on.',
          disks: const [],
        );
      case 'Q' || '/missing':
        return result('not_found', 'This location does not exist.', free: null, fs: null, disks: const []);
      case 'R' || '/readonly':
        return result('not_writable', 'This location cannot be written to.', free: 0);
      case 'T' || '/tiny':
        return result('ok', '', free: _mib);
      case 'K' || '/fat32':
        return result('ok', '', fs: 'FAT32', maxFileBytes: 4294967295);
    }
    if (volume.startsWith('unc:')) return result('ok', '', disks: const []);
    return result('ok', '');
  }

  /// Whether two native paths name the same file (case-insensitive, either
  /// separator).
  static bool _samePath(String a, String b) =>
      a.replaceAll('/', r'\').toLowerCase() == b.replaceAll('/', r'\').toLowerCase();
}

/// One entry of PROTOCOL `CatalogInfo.protected`: an absolute byte range of
/// the source that no destination may touch, and why.
final class FakeProtectedRange {
  const FakeProtectedRange(this.start, this.end, this.reason, {this.main = false});

  final int start;

  /// Exclusive.
  final int end;

  /// `session_range` or `lost_volume`.
  final String reason;

  /// The lost volume of the main instance (not sent).
  final bool main;

  Map<String, Object?> toJson() => {'start': start, 'end': end, 'reason': reason};

  @override
  String toString() => 'FakeProtectedRange($start, $end, $reason)';
}
