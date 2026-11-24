/// The catalog of the fake engine: a [FakeTree] plus what changes at run
/// time (carved nodes, geometry overrides), and the catalog queries of
/// docs/PROTOCOL.md answered as JSON maps.
library;

import 'dart:convert';

import 'fake_content.dart';
import 'fake_tree.dart';

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
    required this.rangeEnd,
  });

  int clusterSize;
  int volumeOffset;
  String clusterSource;
  String offsetSource;
  int indexDirs;
  int signatureChecked;
  int signatureOk;
  bool validated;
  final int rangeEnd;

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
    'total_clusters': rangeEnd > volumeOffset ? (rangeEnd - volumeOffset) ~/ clusterSize : 0,
  };

  _Geometry copy() => _Geometry(
    clusterSize: clusterSize,
    volumeOffset: volumeOffset,
    clusterSource: clusterSource,
    offsetSource: offsetSource,
    indexDirs: indexDirs,
    signatureChecked: signatureChecked,
    signatureOk: signatureOk,
    validated: validated,
    rangeEnd: rangeEnd,
  );
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
    required this.scannedBytes,
    required this.collectCarveHits,
    this.sourceAttached = true,
    List<Map<String, Object?>> warnings = const [],
  }) : warnings = List.of(warnings) {
    _nextDynId = tree.count;
    for (final instance in tree.instances) {
      final main = instance.kind == 'main';
      final own = main || instance.kind == 'other';
      final best = _Geometry(
        clusterSize: 4096,
        volumeOffset: instance.kind == 'other' ? rangeStart + 104857600 : rangeStart,
        clusterSource: own ? 'attr' : 'default',
        offsetSource: main ? 'index' : (own ? 'boot' : 'inherited'),
        indexDirs: main ? 310 : (own ? 12 : 0),
        signatureChecked: own ? 200 : 0,
        signatureOk: own ? 197 : 0,
        validated: own,
        rangeEnd: rangeEnd,
      );
      _geometry.add(best);
      _candidates.add([
        best.copy(),
        if (main)
          _Geometry(
            clusterSize: 4096,
            volumeOffset: rangeStart + 32256,
            clusterSource: 'attr',
            offsetSource: 'backup_boot',
            indexDirs: 0,
            signatureChecked: 200,
            signatureOk: 3,
            validated: false,
            rangeEnd: rangeEnd,
          ),
      ]);
    }
    _computeStats();
  }

  final FakeTree tree;

  /// Changes whenever the node set is rebuilt.
  int epoch;
  final int rangeStart;
  final int rangeEnd;
  final int scannedBytes;
  final bool collectCarveHits;
  bool sourceAttached;
  final List<Map<String, Object?>> warnings;

  final List<_Geometry> _geometry = [];
  final List<List<_Geometry>> _candidates = [];
  final List<_InstanceStats> _stats = [];

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

  int get mainRoot => tree.instances.first.root;

  /// The node id of `[Carved files]`, `null` before carving found anything.
  int? get carvedRoot => _carvedRoot;

  int get carvedFiles => _carvedFiles;

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

  String _name(int id) => id < tree.count ? tree.nameOf(id) : _dyn[id]!.name;

  String _ext(int id) =>
      id < tree.count ? (_isFolder(id) ? '' : fakeExts[tree.ext[id]]) : _dyn[id]!.ext;

  int _flags(int id) => id < tree.count ? tree.flags[id] : _dyn[id]!.flags;

  bool _deleted(int id) => _flags(id) & fDeleted != 0;

  int _instance(int id) => id < tree.count ? tree.instanceOf[id] : 0;

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
        geometryValidated: _geometry[0].validated,
        carvedUnverified: !node.endVerified,
      );
    }
    return fakeRating(
      tree.flags[id],
      geometryValidated: _geometry[tree.instanceOf[id]].validated,
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

  /// PROTOCOL `CatalogInfo`.
  Map<String, Object?> info() => {
    'epoch': epoch,
    'instances': [
      for (final instance in tree.instances)
        {
          'id': instance.id,
          'kind': instance.kind,
          'label': instance.label,
          'root': instance.root,
          'records': _stats[instance.id].records,
          'files': _stats[instance.id].files,
          'dirs': _stats[instance.id].dirs,
          'deleted_files': _stats[instance.id].deletedFiles,
          'lost_dirs': _stats[instance.id].lostDirs,
          'bytes': _stats[instance.id].bytes,
          'time_from': _stats[instance.id].timeFrom,
          'time_to': _stats[instance.id].timeTo,
          'embedded_in': instance.embeddedIn,
          'geometry': _geometry[instance.id].toJson(),
          'geometry_candidates': [
            for (final candidate in _candidates[instance.id]) candidate.toJson(),
          ],
          'virtual': {
            'lost': instance.lost,
            'orphans': instance.orphans,
            'carved': instance.id == 0 ? _carvedRoot : null,
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
    final geometry = _geometry[_instance(node)];
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
      'instance': _instance(node),
      'accessed': modified == null ? null : modified + 86400000,
      'other_names': [
        if (!folder && h % 50 == 0) 'Copy of ${_name(node)}',
      ],
      'record': hasRecord
          ? {
              'number': 256 + node,
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

  /// Checks the arguments of `geometry_set`.
  void checkGeometry({required int instance, required int clusterSize, required int volumeOffset, required int sourceSize}) {
    if (instance < 0 || instance >= _geometry.length) {
      throw FakeError('not_found', 'instance $instance does not exist');
    }
    final powerOfTwo = clusterSize > 0 && clusterSize & (clusterSize - 1) == 0;
    if (!powerOfTwo || clusterSize < 512 || clusterSize > 2097152) {
      throw FakeError('bad_request', 'cluster_size must be a power of two from 512 to 2 MiB');
    }
    if (volumeOffset < 0 || volumeOffset % 512 != 0 || volumeOffset >= sourceSize) {
      throw FakeError('bad_request', 'volume_offset must be a multiple of 512 inside the source');
    }
  }

  /// `geometry_set` (arguments already checked with [checkGeometry]). The
  /// tree and the epoch stay; ratings are recomputed; carved nodes are
  /// discarded when the main instance changes.
  void setGeometry({required int instance, required int clusterSize, required int volumeOffset}) {
    final candidate = _candidates[instance].where(
      (c) => c.clusterSize == clusterSize && c.volumeOffset == volumeOffset,
    );
    if (candidate.isNotEmpty) {
      _geometry[instance] = candidate.first.copy();
    } else {
      _geometry[instance] = _Geometry(
        clusterSize: clusterSize,
        volumeOffset: volumeOffset,
        clusterSource: 'manual',
        offsetSource: 'manual',
        indexDirs: 0,
        signatureChecked: 200,
        signatureOk: 1,
        validated: false,
        rangeEnd: rangeEnd,
      );
    }
    if (instance == 0) {
      _discardCarved((_) => true);
      final root = _carvedRoot;
      if (root != null) {
        _dyn[root]!.removed = true;
        _carvedRoot = null;
      }
    }
    _generation++;
  }
}
