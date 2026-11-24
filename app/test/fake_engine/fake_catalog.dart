/// The catalog of the fake engine: a [FakeTree] plus what changes at run
/// time (carved nodes, geometry overrides, the user's verdicts about the file
/// systems), and the catalog queries of docs/PROTOCOL.md answered as JSON
/// maps.
library;

import 'dart:convert';

import 'fake_content.dart';
import 'fake_tree.dart';
import 'fake_world.dart';

/// An error response: thrown by the fake's command handlers.
final class FakeError implements Exception {
  FakeError(this.code, this.message, [this.data]);

  final String code;
  final String message;
  final Map<String, Object?>? data;

  Map<String, Object?> toJson() => {'code': code, 'message': message, 'data': data};

  @override
  String toString() => 'FakeError($code: $message)';
}

/// A node created at run time: `[Carved files]`, its per-extension folders
/// and the carved files.
final class _DynNode {
  _DynNode({
    required this.id,
    required this.parent,
    required this.kind,
    required this.name,
    this.ext = '',
    this.size = 0,
    this.flags = 0,
    this.endVerified = true,
    this.carveKind,
  });

  final int id;
  final int parent;
  final int kind;
  final String name;
  final String ext;
  final int size;
  final int flags;
  final bool endVerified;

  /// The carve kind (`jpeg`, ...) of a carved file or of a kind folder.
  final String? carveKind;
  final List<int> children = [];
  bool removed = false;
}

/// The carve kinds of `hello.carve_kinds` with the extension of their files.
const Map<String, String> fakeCarveKinds = {
  'jpeg': 'jpg',
  'png': 'png',
  'gif': 'gif',
  'bmp': 'bmp',
  'tiff': 'tif',
  'pdf': 'pdf',
  'zip': 'zip',
  'mp4': 'mp4',
  'heic': 'heic',
  'cr3': 'cr3',
  'avi': 'avi',
  'wav': 'wav',
  'webp': 'webp',
  'mkv': 'mkv',
  'mp3': 'mp3',
  'sevenzip': '7z',
  'rar': 'rar',
};

const Map<String, String> _flagTexts = {
  'deleted': 'The file had been deleted before the disk was formatted.',
  'torn_record': 'The file record was only partly written when it was last saved.',
  'missing_fragments': 'Part of the description of where the file is stored was lost.',
  'size_estimated': 'The exact size is unknown; it was estimated from the space the file occupied.',
  'out_of_bounds': 'Part of the file lies outside the scanned area.',
  'overwritten': 'New data was written where this file was stored.',
  'overlap': 'Another file claims some of the same space.',
  'compressed': 'The file was stored with NTFS compression, which is not supported.',
  'encrypted': 'The file was encrypted by Windows (EFS) and cannot be read.',
  'placeholder': 'The file was a cloud placeholder: its content was never on this disk.',
  'sparse': 'The file contains empty regions that take no disk space.',
  'no_data': 'The file is empty.',
  'bad_sectors': 'Part of the file is in unreadable sectors.',
  'older_generation': 'The file belongs to a file system from before the previous format.',
  'embedded': 'The file is inside a disk image stored on the drive. Recover that image and open it.',
  'stray': 'The file record belongs to no reconstructed file system.',
  'signature_ok': 'The file starts with a valid header for its type.',
  'signature_mismatch': 'The file does not start like a file of its type.',
};

final class _Geometry {
  _Geometry({
    required this.clusterSize,
    required this.volumeOffset,
    required this.clusterSource,
    required this.offsetSource,
    required this.indexDirs,
    required this.signatureChecked,
    required this.signatureOk,
    required this.validated,
    required this.volumeEnd,
  });

  final int clusterSize;
  final int volumeOffset;
  final String clusterSource;
  final String offsetSource;
  final int indexDirs;
  final int signatureChecked;
  final int signatureOk;
  final bool validated;

  /// Where the volume ends on the source (exclusive): `volume_offset +
  /// total_clusters * cluster_size`, clipped to the source.
  final int volumeEnd;

  int get totalClusters => volumeEnd > volumeOffset ? (volumeEnd - volumeOffset) ~/ clusterSize : 0;

  /// The lost-volume range of docs/ALGORITHMS.md, F5.
  (int, int) get range => (volumeOffset, volumeOffset + totalClusters * clusterSize);

  bool sameValues(int clusterSize, int volumeOffset) =>
      this.clusterSize == clusterSize && this.volumeOffset == volumeOffset;

  /// This geometry as another instance inherits it (docs/ALGORITHMS.md, F4).
  _Geometry inherited() => _Geometry(
    clusterSize: clusterSize,
    volumeOffset: volumeOffset,
    clusterSource: clusterSource,
    offsetSource: 'inherited',
    indexDirs: 0,
    signatureChecked: 0,
    signatureOk: 0,
    validated: false,
    volumeEnd: volumeEnd,
  );

  Map<String, Object?> toJson() => {
    'cluster_size': clusterSize,
    'volume_offset': volumeOffset,
    'cluster_source': clusterSource,
    'offset_source': offsetSource,
    'index_dirs': indexDirs,
    'index_blocks': indexDirs * 4 ~/ 3,
    'signature_checked': signatureChecked,
    'signature_ok': signatureOk,
    'validated': validated,
    'total_clusters': totalClusters,
  };
}

final class _InstanceStats {
  int records = 0;
  int files = 0;
  int dirs = 0;
  int deletedFiles = 0;
  int lostDirs = 0;
  int bytes = 0;
  int? timeFrom;
  int? timeTo;
}

final class _SortItem {
  _SortItem(this.id, this.folder, this.lowerName);
  final int id;
  final bool folder;
  final String lowerName;
  String text = '';
  int number = 0;
  bool isNull = false;
}

/// See the library documentation.
final class FakeCatalog {
  FakeCatalog({
    required this.tree,
    required this.epoch,
    required this.rangeStart,
    required this.rangeEnd,
    required this.sourceSize,
    required this.scannedBytes,
    required this.collectCarveHits,
    this.sourceAttached = true,
    List<Map<String, Object?>> warnings = const [],
  }) : warnings = List.of(warnings) {
    _nextDynId = tree.count;
    for (final instance in tree.instances) {
      final own = _ownGeometry(instance);
      _own.add(own);
      _candidates.add([
        own,
        if (instance.kind == 'main')
          _Geometry(
            clusterSize: 4096,
            volumeOffset: rangeStart + 32256,
            clusterSource: 'attr',
            offsetSource: 'backup_boot',
            indexDirs: 0,
            signatureChecked: 200,
            signatureOk: 3,
            validated: false,
            volumeEnd: own.volumeEnd,
          ),
      ]);
      _manual.add(null);
    }
    _applyKinds();
    _computeStats();
  }

  /// How far the lost main file system reaches beyond the end of the scanned
  /// range, where the source has room: an old volume that was larger than the
  /// partition it was scanned as (re-partitioned disk). A destination on a
  /// partition behind the scanned one then overlaps a `lost_volume` range.
  static const int mainVolumeReach = 64 * 1024 * 1024 * 1024;

  final FakeTree tree;

  /// Changes whenever the node set is rebuilt.
  int epoch;
  final int rangeStart;
  final int rangeEnd;

  /// Size of the source in bytes (lost volumes are clipped to it).
  final int sourceSize;
  final int scannedBytes;
  final bool collectCarveHits;
  bool sourceAttached;
  final List<Map<String, Object?>> warnings;

  /// What the evidence of each instance says (its own geometry), the best
  /// candidates, and the user's `geometry_set` overrides.
  final List<_Geometry> _own = [];
  final List<List<_Geometry>> _candidates = [];
  final List<_Geometry?> _manual = [];
  final List<_InstanceStats> _stats = [];

  /// The user's verdicts (`instance_set`): instance -> `main` or `other`.
  ///
  /// Inside the fake an instance is known by its index in
  /// `FakeTree.instances` for good. On the wire `Instance.id` is its position
  /// in `CatalogInfo.instances` (the main one is 0), which changes when the
  /// user chooses another main instance: the protocol does not promise that
  /// instance ids survive `instance_set` (it starts a new epoch), so a client
  /// must take them from the current catalog, and the fake makes sure it
  /// does.
  final Map<int, String> _overrides = {};

  /// Current kind, label and container of each instance (by index), after
  /// the overrides.
  late List<String> _kinds;
  late List<String> _labels;
  late List<String?> _embeddedIn;
  int _mainId = 0;

  final Map<int, _DynNode> _dyn = {};
  int _nextDynId = 0;
  int? _carvedRoot;
  int _carvedFiles = 0;
  int _carvedBytes = 0;

  /// Bumped whenever rows change, to invalidate cached listings.
  int _generation = 0;
  final Map<String, List<int>> _listingCache = {};

  bool get scanComplete => scannedBytes >= rangeEnd - rangeStart;

  int get carveHits => collectCarveHits ? 48213 * scannedBytes ~/ (rangeEnd - rangeStart) : 0;

  /// The root of the main instance.
  int get mainRoot => tree.instances[_mainId].root;

  /// The node id of `[Carved files]`, `null` before carving found anything.
  int? get carvedRoot => _carvedRoot;

  int get carvedFiles => _carvedFiles;

  // --- file systems --------------------------------------------------------

  /// What the evidence of [instance] alone says about its geometry.
  _Geometry _ownGeometry(FakeInstance instance) => switch (instance.kind) {
    'main' => _Geometry(
      clusterSize: 4096,
      volumeOffset: rangeStart,
      clusterSource: 'attr',
      offsetSource: 'index',
      indexDirs: 310,
      signatureChecked: 200,
      signatureOk: 197,
      validated: true,
      volumeEnd: _clip(rangeEnd + mainVolumeReach),
    ),
    'other' => _Geometry(
      clusterSize: 4096,
      volumeOffset: rangeStart + 104857600,
      clusterSource: 'attr',
      offsetSource: 'boot',
      indexDirs: 12,
      signatureChecked: 200,
      signatureOk: 197,
      validated: true,
      volumeEnd: rangeEnd,
    ),
    // An older file system on the same partition: its own records place it
    // well, but most of its clusters have been reused since.
    'older' => _Geometry(
      clusterSize: 4096,
      volumeOffset: rangeStart,
      clusterSource: 'attr',
      offsetSource: 'boot',
      indexDirs: 4,
      signatureChecked: 120,
      signatureOk: 102,
      validated: true,
      volumeEnd: rangeEnd,
    ),
    // The clusters of a file system inside a disk-image file are offsets in
    // that file, not on the source: read directly they rarely match.
    _ => _Geometry(
      clusterSize: 4096,
      volumeOffset: rangeStart + 209715200,
      clusterSource: 'attr',
      offsetSource: 'default',
      indexDirs: 0,
      signatureChecked: 40,
      signatureOk: 9,
      validated: false,
      volumeEnd: rangeEnd,
    ),
  };

  int _clip(int offset) => offset > sourceSize ? sourceSize : offset;

  /// The geometry instance [id] is read with: the user's choice, its own
  /// when it is a volume of its own (`main`, `other`), else the main
  /// instance's (docs/ALGORITHMS.md, F4).
  _Geometry _geometryOf(int id) {
    final manual = _manual[id];
    if (manual != null) return manual;
    final kind = _kinds[id];
    if (kind == 'main' || kind == 'other') return _own[id];
    return _geometryOf(_mainId).inherited();
  }

  static int _kindFlag(String kind) => switch (kind) {
    'older' => fOlderGeneration,
    'embedded' => fEmbedded,
    'strays' => fStray,
    _ => 0,
  };

  /// Computes kinds, labels and containers from the engine's verdict and
  /// the user's overrides.
  void _applyKinds() {
    final kinds = [for (final instance in tree.instances) instance.kind];
    final engineMain = kinds.indexOf('main');
    _overrides.forEach((id, kind) {
      if (kind == 'other') kinds[id] = 'other';
    });
    for (final MapEntry(key: id, value: kind) in _overrides.entries) {
      if (kind != 'main') continue;
      if (engineMain != id && kinds[engineMain] == 'main') kinds[engineMain] = 'other';
      kinds[id] = 'main';
    }
    _kinds = kinds;
    _mainId = kinds.indexOf('main');
    _labels = [
      for (final instance in tree.instances)
        if (kinds[instance.id] == 'main')
          ''
        else if (kinds[instance.id] == instance.kind)
          instance.label
        else
          // A file system that became a volume of its own by the user's word.
          '[File system ${instance.id + 3}]',
    ];
    _embeddedIn = [
      for (final instance in tree.instances) kinds[instance.id] == 'embedded' ? instance.embeddedIn : null,
    ];
  }

  /// The index of the instance whose `Instance.id` is [wireId]; `not_found`
  /// if there is none.
  int _index(int wireId) {
    final order = _instanceOrder();
    if (wireId < 0 || wireId >= order.length) {
      throw FakeError('not_found', 'instance $wireId does not exist');
    }
    return order[wireId];
  }

  /// `Instance.id` of the instance with [index].
  int _wireId(int index) => _instanceOrder().indexOf(index);

  /// `instance_set`: the user overrules the engine's verdict about the
  /// instance with `Instance.id` [wireId]. The caller gives the catalog a new
  /// epoch. Carved nodes are discarded.
  void setInstanceKind(int wireId, String kind) {
    final id = _index(wireId);
    if (!const ['main', 'other', 'auto'].contains(kind)) {
      throw FakeError('bad_request', '"kind" must be "main", "other" or "auto"');
    }
    if (tree.instances[id].kind == 'strays') {
      throw FakeError('bad_request', 'The stray records are no file system of their own and cannot be changed.');
    }
    // "other turns an embedded or older instance into a volume of its own."
    final verdict = tree.instances[id].kind;
    if (kind == 'other' && verdict != 'embedded' && verdict != 'older') {
      throw FakeError(
        'bad_request',
        verdict == 'main'
            ? 'Choose the file system that should be the main one instead.'
            : 'This file system is a volume of its own already.',
      );
    }
    switch (kind) {
      case 'auto':
        _overrides.remove(id);
      case 'main':
        _overrides.removeWhere((_, k) => k == 'main');
        _overrides[id] = 'main';
      default:
        _overrides[id] = 'other';
    }
    _applyKinds();
    _discardAllCarved();
    _generation++;
  }

  /// PROTOCOL `CatalogInfo.protected`: the session range, then the
  /// lost-volume range of every `main` and `other` instance with validated
  /// geometry (docs/ALGORITHMS.md, F5), clipped to the source.
  List<FakeProtectedRange> protectedRanges() => [
    FakeProtectedRange(rangeStart, rangeEnd, 'session_range'),
    for (final id in _instanceOrder())
      if ((_kinds[id] == 'main' || _kinds[id] == 'other') && _geometryOf(id).validated)
        FakeProtectedRange(
          _geometryOf(id).range.$1,
          _clip(_geometryOf(id).range.$2),
          'lost_volume',
          main: id == _mainId,
        ),
  ];

  /// The catalog files, and their bytes, stored in the absolute byte range
  /// `[start, end)`: for every volume of its own, its share of the files in
  /// proportion to how much of its lost-volume range the range covers.
  (int, int) filesIn(int start, int end) {
    var files = 0;
    var bytes = 0;
    for (final instance in tree.instances) {
      final kind = _kinds[instance.id];
      if (kind != 'main' && kind != 'other') continue;
      final (from, to) = _geometryOf(instance.id).range;
      final overlap = (end < to ? end : to) - (start > from ? start : from);
      if (overlap <= 0 || to <= from) continue;
      final share = overlap / (to - from);
      final stats = _stats[instance.id];
      files += ((stats.files + stats.deletedFiles) * share).round();
      bytes += (stats.bytes * share).round();
    }
    return (files, bytes);
  }

  /// Instance ids in the order of `CatalogInfo.instances`: the main one
  /// first, then the others by id.
  List<int> _instanceOrder() => [
    _mainId,
    for (final instance in tree.instances)
      if (instance.id != _mainId) instance.id,
  ];

  // --- node access (generated and run-time nodes alike) --------------------

  bool exists(int id) {
    if (id < 0) return false;
    if (id < tree.count) return true;
    final node = _dyn[id];
    return node != null && !node.removed;
  }

  void _check(int id) {
    if (!exists(id)) throw FakeError('not_found', 'node $id does not exist');
  }

  int _kind(int id) => id < tree.count ? tree.kind[id] : _dyn[id]!.kind;

  bool _isFolder(int id) {
    final k = _kind(id);
    return k != kFile && k != kCarved;
  }

  int _parent(int id) => id < tree.count ? tree.parent[id] : _dyn[id]!.parent;

  String _name(int id) {
    if (id >= tree.count) return _dyn[id]!.name;
    // An instance root is named after its instance's current label.
    if (tree.parent[id] < 0) return _labels[tree.instanceOf[id]];
    return tree.nameOf(id);
  }

  String _ext(int id) =>
      id < tree.count ? (_isFolder(id) ? '' : fakeExts[tree.ext[id]]) : _dyn[id]!.ext;

  /// The flags of node [id]. The flags that follow from an instance's kind
  /// (`older_generation`, `embedded`, `stray`) change with the user's
  /// verdicts: a file of an older file system that the user made a volume of
  /// its own is no longer `older_generation`.
  int _flags(int id) {
    if (id >= tree.count) return _dyn[id]!.flags;
    final flags = tree.flags[id];
    final instance = tree.instanceOf[id];
    final generated = tree.instances[instance].kind;
    final current = _kinds[instance];
    if (generated == current) return flags;
    return flags & ~_kindFlag(generated) | _kindFlag(current);
  }

  bool _deleted(int id) => _flags(id) & fDeleted != 0;

  /// Carved nodes belong to the main instance.
  int _instance(int id) => id < tree.count ? tree.instanceOf[id] : _mainId;

  int? _modified(int id) {
    if (id >= tree.count) return null;
    final t = tree.modified[id];
    return t == noTime ? null : t;
  }

  int? _created(int id) {
    if (id >= tree.count) return null;
    final t = tree.created[id];
    return t == noTime ? null : t;
  }

  /// Files: size. Folders: total size of the non-deleted files below.
  int _size(int id) {
    if (id >= tree.count) {
      final node = _dyn[id]!;
      if (node.kind == kCarved) return node.size;
      var sum = 0;
      for (final child in node.children) {
        sum += _size(child);
      }
      return sum;
    }
    return tree.size[id] + (id == mainRoot ? _carvedBytes : 0);
  }

  int _files(int id) {
    if (id >= tree.count) {
      final node = _dyn[id]!;
      if (node.kind == kCarved) return 0;
      var sum = 0;
      for (final child in node.children) {
        sum += _kind(child) == kCarved ? 1 : _files(child);
      }
      return sum;
    }
    return tree.aggFiles[id] + (id == mainRoot ? _carvedFiles : 0);
  }

  /// The rating index of a file, or `null` for folders.
  int? _rating(int id) {
    if (_isFolder(id)) return null;
    if (id >= tree.count) {
      final node = _dyn[id]!;
      return fakeRating(
        node.flags,
        geometryValidated: _geometryOf(_mainId).validated,
        carvedUnverified: !node.endVerified,
      );
    }
    return fakeRating(
      _flags(id),
      geometryValidated: _geometryOf(tree.instanceOf[id]).validated,
    );
  }

  Iterable<int> _children(int id) sync* {
    if (id >= tree.count) {
      yield* _dyn[id]!.children;
      return;
    }
    final first = tree.firstChild[id];
    final n = tree.childCount[id];
    for (var i = 0; i < n; i++) {
      yield first + i;
    }
    if (id == mainRoot && _carvedRoot != null) yield _carvedRoot!;
  }

  List<int> _ancestors(int id) {
    final chain = <int>[];
    for (var p = _parent(id); p >= 0; p = _parent(p)) {
      chain.add(p);
    }
    return chain.reversed.toList(growable: false);
  }

  String _path(int id) {
    final parts = <String>[
      for (final a in _ancestors(id))
        if (_name(a).isNotEmpty) _name(a),
      _name(id),
    ];
    return parts.join('/');
  }

  /// Whether a child folder is listed: "with `show_deleted: false` a deleted
  /// folder with no non-deleted file below it is hidden".
  bool _folderListed(int id, bool showDeleted) =>
      showDeleted || !_deleted(id) || _files(id) > 0;

  bool _hasChildFolders(int id, bool showDeleted) {
    if (id >= tree.count) {
      return _dyn[id]!.children.any(_isFolder);
    }
    if (id == mainRoot && _carvedRoot != null) return true;
    return showDeleted ? tree.subdirsTotal[id] > 0 : tree.subdirsVisible[id] > 0;
  }

  List<String> _flagNames(int flags) => [
    for (var i = 0; i < fakeFlagNames.length; i++)
      if (flags & (1 << i) != 0) fakeFlagNames[i],
  ];

  /// PROTOCOL `Row`.
  Map<String, Object?> row(int id, {required bool showDeleted}) {
    final folder = _isFolder(id);
    final parent = _parent(id);
    final rating = _rating(id);
    final static = id < tree.count;
    return {
      'id': id,
      'parent': parent < 0 ? null : parent,
      'name': _name(id),
      'kind': fakeKindNames[_kind(id)],
      'ext': _ext(id),
      'size': _size(id),
      'modified': _modified(id),
      'created': _created(id),
      'deleted': _deleted(id),
      'rating': rating == null ? null : fakeRatingNames[rating],
      'flags': _flagNames(_flags(id)),
      'files': folder ? _files(id) : 0,
      'deleted_files': folder && static ? tree.aggDeletedFiles[id] : 0,
      'deleted_size': folder && static ? tree.aggDeletedSize[id] : 0,
      'has_children': folder && _hasChildFolders(id, showDeleted),
    };
  }

  // --- catalog_info --------------------------------------------------------

  void _computeStats() {
    for (final instance in tree.instances) {
      final stats = _InstanceStats();
      for (var id = instance.firstNode; id < instance.endNode; id++) {
        final k = tree.kind[id];
        if (k == kFile) {
          stats.records++;
          if (tree.flags[id] & fDeleted != 0) {
            stats.deletedFiles++;
          } else {
            stats.files++;
            stats.bytes += tree.size[id];
          }
          final t = tree.modified[id];
          if (t != noTime) {
            if (stats.timeFrom == null || t < stats.timeFrom!) stats.timeFrom = t;
            if (stats.timeTo == null || t > stats.timeTo!) stats.timeTo = t;
          }
        } else if (k == kDir) {
          stats.records++;
          if (tree.flags[id] & fDeleted == 0 && tree.parent[id] >= 0) stats.dirs++;
        } else if (k == kLostDir) {
          stats.lostDirs++;
        }
      }
      _stats.add(stats);
    }
  }

  /// PROTOCOL `CatalogInfo`. [sessionFileAtRisk]: the engine found the
  /// session file inside a protected range (docs/ALGORITHMS.md, B3).
  Map<String, Object?> info({bool sessionFileAtRisk = false}) => {
    'epoch': epoch,
    'instances': [
      for (final (position, id) in _instanceOrder().indexed)
        {
          'id': position,
          'kind': _kinds[id],
          'manual': _overrides.containsKey(id),
          'label': _labels[id],
          'root': tree.instances[id].root,
          'records': _stats[id].records,
          'files': _stats[id].files,
          'dirs': _stats[id].dirs,
          'deleted_files': _stats[id].deletedFiles,
          'lost_dirs': _stats[id].lostDirs,
          'bytes': _stats[id].bytes,
          'time_from': _stats[id].timeFrom,
          'time_to': _stats[id].timeTo,
          'embedded_in': _embeddedIn[id],
          'geometry': _geometryOf(id).toJson(),
          'geometry_candidates': [
            for (final candidate in _candidates[id]) candidate.toJson(),
          ],
          'virtual': {
            'lost': tree.instances[id].lost,
            'orphans': tree.instances[id].orphans,
            'carved': id == _mainId ? _carvedRoot : null,
          },
        },
    ],
    'nodes': tree.count + _dyn.values.where((n) => !n.removed).length,
    'live_records': 41,
    'post_format_records': 6,
    'live_volumes': [
      {
        'offset': rangeStart,
        'size': rangeEnd - rangeStart,
        'format_time': 1763900000000,
        'snapshot_time': sourceAttached ? 1763990000000 : null,
      },
    ],
    'range': {'start': rangeStart, 'end': rangeEnd},
    'protected': [for (final range in protectedRanges()) range.toJson()],
    'scanned_bytes': scannedBytes,
    'scan_complete': scanComplete,
    'bad_bytes': 0,
    'out_of_range_files': 2,
    'out_of_range_bytes': 123456789,
    'carve_hits': carveHits,
    'carved': _carvedFiles,
    'source_attached': sourceAttached,
    'warnings': [
      ...warnings,
      if (!scanComplete)
        {
          'code': 'scan_incomplete',
          'message': 'The scan was stopped early. Files on the unread part of the disk are missing.',
        },
      {
        'code': 'out_of_range_files',
        'message': '2 files have data outside the scanned area. Scan the whole disk to reach them.',
      },
      if (sessionFileAtRisk)
        {
          'code': 'session_file_at_risk',
          'message':
              'The session file lies inside the lost file system. HDSavior no longer writes to it: '
              'save the session somewhere else.',
        },
    ],
  };

  // --- listings ------------------------------------------------------------

  static int _compareItems(_SortItem a, _SortItem b, String sort, bool desc) {
    if (a.folder != b.folder) return a.folder ? -1 : 1;
    if (a.isNull != b.isNull) return a.isNull ? 1 : -1;
    var c = 0;
    if (!a.isNull) {
      c = switch (sort) {
        'name' => a.lowerName.compareTo(b.lowerName),
        'ext' => a.text.compareTo(b.text),
        _ => a.number.compareTo(b.number),
      };
      if (desc) c = -c;
    }
    if (c != 0) return c;
    c = a.lowerName.compareTo(b.lowerName);
    return c != 0 ? c : a.id.compareTo(b.id);
  }

  List<int> _sort(Iterable<int> ids, String sort, bool desc) {
    final items = <_SortItem>[];
    for (final id in ids) {
      final item = _SortItem(id, _isFolder(id), _name(id).toLowerCase());
      switch (sort) {
        case 'name':
          break;
        case 'size':
          item.number = _size(id);
        case 'modified':
          final t = _modified(id);
          item.isNull = t == null;
          item.number = t ?? 0;
        case 'rating':
          final r = _rating(id);
          item.isNull = r == null;
          item.number = r ?? 0;
        case 'ext':
          item.text = _ext(id);
        default:
          throw FakeError('bad_request', 'unknown sort key "$sort"');
      }
      items.add(item);
    }
    items.sort((a, b) => _compareItems(a, b, sort, desc));
    return [for (final item in items) item.id];
  }

  List<int> _cached(String key, List<int> Function() compute) {
    final fullKey = '$_generation|$key';
    final hit = _listingCache.remove(fullKey);
    if (hit != null) {
      _listingCache[fullKey] = hit; // most recently used last
      return hit;
    }
    final value = compute();
    _listingCache[fullKey] = value;
    while (_listingCache.length > 8) {
      _listingCache.remove(_listingCache.keys.first);
    }
    return value;
  }

  static Set<int>? _ratingSet(List<String>? ratings) {
    if (ratings == null) return null;
    return {
      for (final rating in ratings)
        if (fakeRatingNames.contains(rating))
          fakeRatingNames.indexOf(rating)
        else
          throw FakeError('bad_request', 'unknown rating "$rating"'),
    };
  }

  /// `list_children`.
  Map<String, Object?> listChildren({
    required int node,
    required int offset,
    required int limit,
    required String sort,
    required bool desc,
    required bool dirsOnly,
    required bool showDeleted,
    required List<String>? exts,
    required List<String>? ratings,
  }) {
    _check(node);
    if (!_isFolder(node)) throw FakeError('bad_request', 'node $node is not a folder');
    final ratingSet = _ratingSet(ratings);
    final extSet = exts?.toSet();
    final ids = _cached(
      'children|$node|$sort|$desc|$dirsOnly|$showDeleted|${jsonEncode(exts)}|${jsonEncode(ratings)}',
      () => _sort(
        _children(node).where((id) {
          if (_isFolder(id)) return _folderListed(id, showDeleted);
          if (dirsOnly) return false;
          if (!showDeleted && _deleted(id)) return false;
          if (extSet != null && !extSet.contains(_ext(id))) return false;
          if (ratingSet != null && !ratingSet.contains(_rating(id))) return false;
          return true;
        }),
        sort,
        desc,
      ),
    );
    return {
      'total': ids.length,
      'rows': [
        for (var i = offset; i < ids.length && i < offset + limit; i++)
          row(ids[i], showDeleted: showDeleted),
      ],
    };
  }

  /// All files (and carved files) below [root], depth first.
  Iterable<int> _filesBelow(int root) sync* {
    final stack = <int>[root];
    while (stack.isNotEmpty) {
      final id = stack.removeLast();
      if (_isFolder(id)) {
        stack.addAll(_children(id));
      } else {
        yield id;
      }
    }
  }

  bool Function(int id) _filterTest(Map<String, Object?>? filter) {
    if (filter == null) return (_) => true;
    final query = (filter['query'] as String?)?.toLowerCase();
    final exts = (filter['exts'] as List<Object?>?)?.cast<String>().toSet();
    final minSize = filter['min_size'] as int?;
    final maxSize = filter['max_size'] as int?;
    final ratings = _ratingSet((filter['ratings'] as List<Object?>?)?.cast<String>());
    if (exts != null && exts.isEmpty) {
      throw FakeError('bad_request', 'filter.exts must not be an empty list');
    }
    if (ratings != null && ratings.isEmpty) {
      throw FakeError('bad_request', 'filter.ratings must not be an empty list');
    }
    return (id) {
      if (query != null && query.isNotEmpty && !_name(id).toLowerCase().contains(query)) {
        return false;
      }
      if (exts != null && !exts.contains(_ext(id))) return false;
      final size = _size(id);
      if (minSize != null && size < minSize) return false;
      if (maxSize != null && size > maxSize) return false;
      if (ratings != null && !ratings.contains(_rating(id))) return false;
      return true;
    };
  }

  /// `search`.
  Map<String, Object?> search({
    required int? under,
    required Map<String, Object?>? filter,
    required bool showDeleted,
    required String sort,
    required bool desc,
    required int offset,
    required int limit,
  }) {
    final root = under ?? mainRoot;
    _check(root);
    final matches = _filterTest(filter);
    final ids = _cached(
      'search|$root|${jsonEncode(filter)}|$showDeleted|$sort|$desc',
      () => _sort(
        _filesBelow(root).where((id) => (showDeleted || !_deleted(id)) && matches(id)),
        sort,
        desc,
      ),
    );
    return {
      'total': ids.length,
      'rows': [
        for (var i = offset; i < ids.length && i < offset + limit; i++)
          {
            ...row(ids[i], showDeleted: showDeleted),
            'path': _path(ids[i]),
            'ancestor_ids': _ancestors(ids[i]),
          },
      ],
    };
  }

  /// `node_info`.
  Map<String, Object?> nodeInfo(int node) {
    _check(node);
    final folder = _isFolder(node);
    final k = _kind(node);
    final h = mix64(node * 0x9E3779B97F4A7C15 + 99) & 0x7fffffffffffffff;
    final geometry = _geometryOf(_instance(node));
    final size = _size(node);
    final resident = !folder && size > 0 && size <= 700;
    final lcn = 4 + h % 200000000;
    final hasRecord = k == kDir || k == kFile;
    final modified = _modified(node);
    return {
      'row': row(node, showDeleted: true),
      'ancestors': [
        for (final a in _ancestors(node)) {'id': a, 'name': _name(a)},
      ],
      'path': _path(node),
      'instance': _wireId(_instance(node)),
      'accessed': modified == null ? null : modified + 86400000,
      'other_names': [
        if (!folder && h % 50 == 0) 'Copy of ${_name(node)}',
      ],
      'record': hasRecord
          ? {
              // Some records are unnumbered strays: no number is known.
              'number': h % 97 == 0 ? null : 256 + node,
              'seq': 1 + h % 5,
              'disk_offset': geometry.volumeOffset + 3221225472 + (256 + node) * 1024,
              'extension_records': h % 60 == 0 ? 1 : 0,
            }
          : null,
      'data': folder
          ? null
          : {
              'resident': resident,
              'real_size': size,
              'initialized_size': size,
              'allocated_size': resident ? 0 : (size + geometry.clusterSize - 1) ~/ geometry.clusterSize * geometry.clusterSize,
              'runs': resident || size == 0 ? 0 : 1 + h % 4,
              'first_lcn': resident || size == 0 ? null : lcn,
              'first_offset': resident || size == 0 ? null : geometry.volumeOffset + lcn * geometry.clusterSize,
            },
      'flag_details': [
        for (final flag in _flagNames(_flags(node))) {'flag': flag, 'text': _flagTexts[flag]},
      ],
    };
  }

  // --- selection -----------------------------------------------------------

  /// What `selection_summary` reports, plus what a recovery will count:
  /// `{files, bytes, dirs, good, uncertain, damaged, unsupported}`.
  Map<String, int> summarize({
    required List<int> include,
    required List<int> exclude,
    required Map<String, Object?>? filter,
    required bool includeDeleted,
    required bool skipDamaged,
  }) {
    final marks = <int, bool>{};
    for (final id in exclude) {
      _check(id);
      marks[id] = false;
    }
    for (final id in include) {
      _check(id);
      marks[id] = true;
    }
    final matches = _filterTest(filter);
    final counts = [0, 0, 0, 0];
    var files = 0;
    var bytes = 0;
    final neededDirs = <int>{};

    void addAncestors(int id) {
      for (var p = _parent(id); p >= 0; p = _parent(p)) {
        // The main root is the destination folder itself.
        if (p != mainRoot && !neededDirs.add(p)) break;
      }
    }

    // Start at the marked nodes that have no marked ancestor: every other
    // mark is met on the way down from one of them.
    for (final top in marks.keys) {
      var hasMarkedAncestor = false;
      for (var p = _parent(top); p >= 0; p = _parent(p)) {
        if (marks.containsKey(p)) {
          hasMarkedAncestor = true;
          break;
        }
      }
      if (hasMarkedAncestor) continue;

      final stack = <(int, bool)>[(top, false)];
      while (stack.isNotEmpty) {
        final (id, inherited) = stack.removeLast();
        final selected = marks[id] ?? inherited;
        if (selected && marks[id] == true) addAncestors(id);
        if (_isFolder(id)) {
          if (selected && id != mainRoot) neededDirs.add(id);
          for (final child in _children(id)) {
            stack.add((child, selected));
          }
        } else if (selected && (includeDeleted || !_deleted(id)) && matches(id)) {
          files++;
          final rating = _rating(id)!;
          counts[rating]++;
          if (rating != rUnsupported && !(rating == rDamaged && skipDamaged)) {
            bytes += _size(id);
          }
        }
      }
    }
    return {
      'files': files,
      'bytes': bytes,
      'dirs': neededDirs.length,
      'good': counts[rGood],
      'uncertain': counts[rUncertain],
      'damaged': counts[rDamaged],
      'unsupported': counts[rUnsupported],
    };
  }

  // --- read_file -----------------------------------------------------------

  /// `read_file`.
  Map<String, Object?> readFile({required int node, required int offset, required int length}) {
    _check(node);
    if (_isFolder(node)) throw FakeError('bad_request', 'node $node is a folder');
    if (_rating(node) == rUnsupported) {
      throw FakeError(
        'unsupported',
        'This file cannot be read: ${_flagTexts[_flagNames(_flags(node) & (fCompressed | fEncrypted | fPlaceholder | fEmbedded)).first]}',
      );
    }
    final size = _size(node);
    final ext = _ext(node);
    final bytes = fakeFileBytes(
      id: node,
      ext: ext,
      size: size,
      contentKind: node < tree.count ? tree.content[node] : cBinary,
      offset: offset,
      length: length,
    );
    return {
      'data_b64': base64.encode(bytes),
      'size': size,
      'eof': offset + bytes.length >= size,
      'kind': offset == 0 && bytes.isNotEmpty ? sniffedKind(ext) : null,
    };
  }

  // --- carving and geometry ------------------------------------------------

  _DynNode _addDyn(_DynNode node) {
    _dyn[node.id] = node;
    if (node.parent >= tree.count) _dyn[node.parent]!.children.add(node.id);
    return node;
  }

  void _discardCarved(bool Function(_DynNode node) test) {
    for (final node in _dyn.values) {
      if (node.removed || node.id == _carvedRoot || !test(node)) continue;
      node.removed = true;
      if (node.kind == kCarved) {
        _carvedFiles--;
        _carvedBytes -= node.size;
      }
      _dyn[node.parent]?.children.remove(node.id);
    }
  }

  /// Replaces the carved nodes of [kinds] with what a run that got through
  /// [fraction] of its candidates finds. Returns `(found, bytes_found)`.
  (int, int) carve(List<String> kinds, {double fraction = 1.0}) {
    _discardCarved((node) => kinds.contains(node.carveKind));
    var found = 0;
    var bytesFound = 0;
    for (final kind in kinds) {
      final ext = fakeCarveKinds[kind];
      if (ext == null) throw FakeError('bad_request', 'unknown carve kind "$kind"');
      final h = mix64(kind.hashCode + epoch) & 0x7fffffff;
      final count = ((3 + h % 6) * fraction).floor();
      if (count == 0) continue;
      final root = _carvedRoot ??= _addDyn(
        _DynNode(id: _nextDynId++, parent: mainRoot, kind: kVirtual, name: '[Carved files]'),
      ).id;
      final folder = _addDyn(
        _DynNode(id: _nextDynId++, parent: root, kind: kVirtual, name: ext, carveKind: kind),
      );
      for (var i = 0; i < count; i++) {
        final fh = mix64(h * 131 + i) & 0x7fffffffffff;
        final sector = (rangeStart ~/ 512) + fh % ((rangeEnd - rangeStart) ~/ 512);
        final size = 20000 + fh % 6000000;
        final verified = fh % 4 != 0;
        _addDyn(
          _DynNode(
            id: _nextDynId++,
            parent: folder.id,
            kind: kCarved,
            name: 'f${sector.toString().padLeft(10, '0')}.$ext',
            ext: ext,
            size: size,
            flags: verified ? fSignatureOk : 0,
            endVerified: verified,
            carveKind: kind,
          ),
        );
        found++;
        bytesFound += size;
      }
    }
    _carvedFiles += found;
    _carvedBytes += bytesFound;
    _generation++;
    return (found, bytesFound);
  }

  /// Discards every carved node, `[Carved files]` included.
  void _discardAllCarved() {
    _discardCarved((_) => true);
    final root = _carvedRoot;
    if (root != null) {
      _dyn[root]!.removed = true;
      _carvedRoot = null;
    }
  }

  /// Checks the arguments of `geometry_set` ([instance] is an
  /// `Instance.id`).
  void checkGeometry({required int instance, required int clusterSize, required int volumeOffset}) {
    _index(instance);
    final powerOfTwo = clusterSize > 0 && clusterSize & (clusterSize - 1) == 0;
    if (!powerOfTwo || clusterSize < 512 || clusterSize > 2097152) {
      throw FakeError('bad_request', 'cluster_size must be a power of two from 512 to 2 MiB');
    }
    if (volumeOffset < 0 || volumeOffset % 512 != 0 || volumeOffset >= sourceSize) {
      throw FakeError('bad_request', 'volume_offset must be a multiple of 512 inside the source');
    }
  }

  /// `geometry_set` (arguments already checked with [checkGeometry]). The
  /// tree and the epoch stay; ratings and the protected ranges are
  /// recomputed; carved nodes are discarded when the geometry of the main
  /// instance changes. A pair equal to a candidate selects that candidate.
  void setGeometry({required int instance, required int clusterSize, required int volumeOffset}) {
    final id = _index(instance);
    // Carved nodes go only when the main instance's geometry really changes
    // (the geometry in effect, set again, keeps them).
    final unchanged = _geometryOf(id).sameValues(clusterSize, volumeOffset);
    final candidates = _candidates[id].where((c) => c.sameValues(clusterSize, volumeOffset));
    _manual[id] = candidates.isNotEmpty
        ? candidates.first
        : _Geometry(
            clusterSize: clusterSize,
            volumeOffset: volumeOffset,
            clusterSource: 'manual',
            offsetSource: 'manual',
            indexDirs: 0,
            signatureChecked: 200,
            signatureOk: 1,
            validated: false,
            volumeEnd: rangeEnd > volumeOffset ? rangeEnd : _clip(volumeOffset + clusterSize),
          );
    if (id == _mainId && !unchanged) _discardAllCarved();
    _generation++;
  }
}
