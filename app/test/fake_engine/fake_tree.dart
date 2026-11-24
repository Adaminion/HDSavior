/// The generated node tree of the fake engine.
///
/// A [FakeTree] is an immutable, deterministic stand-in for what a scan finds:
/// several file systems ("instances") with folders, files, lost folders and
/// virtual folders, every rating and every flag of docs/PROTOCOL.md. It is
/// stored as parallel typed arrays so that a tree of a few hundred thousand
/// nodes is built in a fraction of a second and costs a few megabytes.
///
/// Pure Dart on purpose (no Flutter, no package imports): the fake engine can
/// also run as a plain `dart` process.
library;

import 'dart:typed_data';

/// The flags of PROTOCOL `Row.flags`, in the order of the document. Bit `i`
/// of a node's flag word stands for `fakeFlagNames[i]`.
const List<String> fakeFlagNames = [
  'deleted',
  'torn_record',
  'missing_fragments',
  'size_estimated',
  'out_of_bounds',
  'overwritten',
  'overlap',
  'compressed',
  'encrypted',
  'placeholder',
  'sparse',
  'no_data',
  'bad_sectors',
  'older_generation',
  'embedded',
  'stray',
  'signature_ok',
  'signature_mismatch',
];

const int fDeleted = 1 << 0;
const int fTornRecord = 1 << 1;
const int fMissingFragments = 1 << 2;
const int fSizeEstimated = 1 << 3;
const int fOutOfBounds = 1 << 4;
const int fOverwritten = 1 << 5;
const int fOverlap = 1 << 6;
const int fCompressed = 1 << 7;
const int fEncrypted = 1 << 8;
const int fPlaceholder = 1 << 9;
const int fSparse = 1 << 10;
const int fNoData = 1 << 11;
const int fBadSectors = 1 << 12;
const int fOlderGeneration = 1 << 13;
const int fEmbedded = 1 << 14;
const int fStray = 1 << 15;
const int fSignatureOk = 1 << 16;
const int fSignatureMismatch = 1 << 17;

/// Node kinds, as indices into [fakeKindNames].
const int kDir = 0;
const int kFile = 1;
const int kLostDir = 2;
const int kVirtual = 3;
const int kCarved = 4;
const List<String> fakeKindNames = ['dir', 'file', 'lost_dir', 'virtual', 'carved'];

/// Ratings, as indices into [fakeRatingNames] (best first).
const int rGood = 0;
const int rUncertain = 1;
const int rDamaged = 2;
const int rUnsupported = 3;
const List<String> fakeRatingNames = ['good', 'uncertain', 'damaged', 'unsupported'];

/// "No timestamp" in the time arrays (`null` on the wire).
const int noTime = -0x4000000000000000;

/// Sizes of the sample pictures that fake_content.dart serves (its tests
/// check that the generated pictures have exactly these sizes).
const int jpegSampleBytes = 1282;
const int pngSampleBytes = 9332;
const int bmpSampleBytes = 9270;

/// What `read_file` returns for a node (see fake_content.dart).
const int cBinary = 0;
const int cText = 1;
const int cJpegSample = 2;
const int cPng = 3;
const int cBmp = 4;
const int cTruncatedJpeg = 5;

/// Extensions of generated files. Index 0 is "no extension".
const List<String> fakeExts = [
  '', 'jpg', 'png', 'gif', 'bmp', 'tif', 'heic', 'cr3', 'pdf', 'docx', //
  'xlsx', 'pptx', 'txt', 'csv', 'zip', '7z', 'rar', 'mp3', 'wav', 'mp4',
  'mkv', 'avi', 'webp', 'html', 'psd', 'exe', 'dll', 'iso', 'vhd',
];

/// How often each extension is drawn (same order as [fakeExts]).
const List<int> _extWeights = [
  2, 30, 6, 1, 1, 1, 3, 1, 8, 7, //
  4, 2, 6, 2, 3, 1, 1, 6, 1, 4,
  1, 1, 1, 2, 1, 1, 1, 1, 0,
];

/// SplitMix64 finalizer: a good 64-bit mixing function.
int mix64(int x) {
  x = (x ^ (x >>> 30)) * 0xBF58476D1CE4E5B9;
  x = (x ^ (x >>> 27)) * 0x94D049BB133111EB;
  return x ^ (x >>> 31);
}

/// The rating of a file with [flags] (docs/ALGORITHMS.md, G3).
int fakeRating(
  int flags, {
  required bool geometryValidated,
  bool carvedUnverified = false,
}) {
  if (flags & (fCompressed | fEncrypted | fPlaceholder | fEmbedded) != 0) {
    return rUnsupported;
  }
  if (flags &
          (fMissingFragments |
              fOutOfBounds |
              fOverwritten |
              fBadSectors |
              fSignatureMismatch) !=
      0) {
    return rDamaged;
  }
  if (flags &
              (fDeleted |
                  fOverlap |
                  fTornRecord |
                  fNoData |
                  fOlderGeneration |
                  fStray) !=
          0 ||
      !geometryValidated ||
      carvedUnverified) {
    return rUncertain;
  }
  return rGood;
}

/// One file system of the generated tree.
final class FakeInstance {
  const FakeInstance({
    required this.id,
    required this.kind,
    required this.label,
    required this.root,
    required this.firstNode,
    required this.endNode,
    this.embeddedIn,
    this.lost,
    this.orphans,
  });

  final int id;

  /// `main`, `older`, `other`, `embedded` or `strays`.
  final String kind;
  final String label;
  final int root;

  /// The instance owns the node ids `firstNode <= id < endNode`.
  final int firstNode;
  final int endNode;
  final String? embeddedIn;

  /// Node ids of the virtual folders, if the instance has them.
  final int? lost;
  final int? orphans;
}

/// Well-known nodes of the main instance, for tests.
final class FakeLandmarks {
  const FakeLandmarks({
    required this.flagSamples,
    required this.previewSamples,
    required this.hugeFolder,
    required this.hugeFolderFiles,
    required this.manyFolders,
    required this.manyFoldersCount,
    required this.deletedOnlyFolder,
    required this.mixedDeletedFolder,
    required this.documents,
    required this.sampleJpeg,
    required this.samplePng,
    required this.sampleBmp,
    required this.sampleText,
    required this.sampleEmpty,
    required this.sampleBinary,
    required this.sampleTruncatedJpeg,
    required this.sampleCompressed,
  });

  /// Folder with one file per flag, named `<flag>.dat`.
  final int flagSamples;

  /// Folder with files whose content `read_file` serves as real pictures
  /// and text.
  final int previewSamples;

  /// Folder with [hugeFolderFiles] files and no subfolders.
  final int hugeFolder;
  final int hugeFolderFiles;

  /// Folder with [manyFoldersCount] subfolders.
  final int manyFolders;
  final int manyFoldersCount;

  /// A deleted folder that holds only deleted files (hidden unless
  /// `show_deleted`).
  final int deletedOnlyFolder;

  /// A deleted folder that still holds non-deleted files (always listed).
  final int mixedDeletedFolder;

  /// The first ordinary top-level folder.
  final int documents;
  final int sampleJpeg;
  final int samplePng;
  final int sampleBmp;
  final int sampleText;
  final int sampleEmpty;
  final int sampleBinary;
  final int sampleTruncatedJpeg;

  /// A compressed file: `read_file` answers `unsupported`.
  final int sampleCompressed;
}

/// The immutable generated tree. See the library documentation.
final class FakeTree {
  FakeTree._({
    required this.seed,
    required this.parent,
    required this.kind,
    required this.firstChild,
    required this.childCount,
    required this.size,
    required this.modified,
    required this.created,
    required this.flags,
    required this.ext,
    required this.content,
    required this.instanceOf,
    required this.aggFiles,
    required this.aggDeletedFiles,
    required this.aggDeletedSize,
    required this.subdirsTotal,
    required this.subdirsVisible,
    required this.fixedNames,
    required this.instances,
    required this.landmarks,
  });

  static final Map<String, FakeTree> _cache = {};

  /// Smallest tree that still holds every special folder.
  static const int minMainNodes = 400;

  /// Returns the tree for [seed] with exactly [mainNodes] nodes in the main
  /// instance (plus about 440 in the others). Trees are cached: generating
  /// is cheap but not free, and the tree is immutable.
  factory FakeTree.generate({int seed = 1, int mainNodes = 3000}) {
    if (mainNodes < minMainNodes) {
      throw ArgumentError.value(mainNodes, 'mainNodes', 'must be >= $minMainNodes');
    }
    return _cache.putIfAbsent(
      '$seed/$mainNodes',
      () => _TreeBuilder(seed, mainNodes).build(),
    );
  }

  final int seed;

  /// Parent id; -1 for an instance root.
  final Int32List parent;
  final Uint8List kind;

  /// The children of a folder are the ids
  /// `firstChild <= id < firstChild + childCount`.
  final Int32List firstChild;
  final Int32List childCount;

  /// Files: real size. Folders: total size of the non-deleted files below.
  final Int64List size;

  /// Unix milliseconds, or [noTime].
  final Int64List modified;
  final Int64List created;
  final Uint32List flags;

  /// Index into [fakeExts].
  final Uint8List ext;

  /// Content kind for `read_file` (`cBinary`, `cText`, ...).
  final Uint8List content;
  final Uint8List instanceOf;

  /// Folders: non-deleted files below, recursively.
  final Int32List aggFiles;
  final Int32List aggDeletedFiles;
  final Int64List aggDeletedSize;

  /// Folders: child folders, and those of them that are listed without
  /// `show_deleted`.
  final Int32List subdirsTotal;
  final Int32List subdirsVisible;

  /// Names that are not derived from the id.
  final Map<int, String> fixedNames;
  final List<FakeInstance> instances;
  final FakeLandmarks landmarks;

  int get count => parent.length;

  bool isFolder(int id) => kind[id] != kFile && kind[id] != kCarved;

  bool isDeleted(int id) => flags[id] & fDeleted != 0;

  /// The display name of node [id].
  String nameOf(int id) {
    final fixed = fixedNames[id];
    if (fixed != null) return fixed;
    final p = parent[id];
    final index = p < 0 ? 0 : id - firstChild[p];
    final h = mix64(id * 0x9E3779B97F4A7C15 + seed);
    if (kind[id] == kLostDir) return 'Folder #${1000 + id % 9000}';
    if (kind[id] != kFile) {
      return '${_dirWords[(h & 0x7fffffff) % _dirWords.length]} ${index + 1}';
    }
    final extension = fakeExts[ext[id]];
    final number = index + 1;
    final style = (h >>> 8 & 0x7fffffff) % 1000;
    final String stem;
    var upperExt = false;
    if (style < 3) {
      stem = 'Ünïcödé résumé $number';
    } else if (style < 6) {
      stem = '写真 $number';
    } else if (style < 7) {
      stem = 'broken�name $number';
    } else if (extension == 'jpg' || extension == 'heic' || extension == 'cr3') {
      stem = style.isEven
          ? 'IMG_${number.toString().padLeft(4, '0')}'
          : 'DSC${number.toString().padLeft(5, '0')}';
      upperExt = true;
    } else if (extension.isEmpty) {
      stem = style.isEven ? 'README $number' : 'notes $number';
    } else {
      stem = '${_fileWords[style % _fileWords.length]} $number';
    }
    if (extension.isEmpty) return stem;
    return '$stem.${upperExt ? extension.toUpperCase() : extension}';
  }

  /// The ids of the ancestors of [id], root first.
  List<int> ancestorsOf(int id) {
    final chain = <int>[];
    for (var p = parent[id]; p >= 0; p = parent[p]) {
      chain.add(p);
    }
    return chain.reversed.toList(growable: false);
  }

  static const List<String> _dirWords = [
    'Photos', 'Documents', 'Music', 'Videos', 'Projects', 'Work', 'Backup', //
    'Downloads', 'Archive', 'Holiday', 'Invoices', 'School', 'Scans', 'Letters',
    'Family', 'Taxes', 'Recipes', 'Clients',
  ];

  static const List<String> _fileWords = [
    'Report', 'Invoice', 'Letter', 'Scan', 'Draft', 'Budget', 'Track', //
    'Minutes', 'Offer', 'Contract', 'Slides', 'export', 'backup', 'Chapter',
  ];
}

enum _Style { normal, flat, pairDirs }

final class _Work {
  const _Work(this.dir, this.budget, this.depth, this.style);
  final int dir;

  /// Number of nodes to create below [dir].
  final int budget;
  final int depth;
  final _Style style;
}

final class _TreeBuilder {
  _TreeBuilder(this.seed, this.mainNodes)
    : total = mainNodes + _olderNodes + _otherNodes + _embeddedNodes + _strayNodes;

  static const int _olderNodes = 200;
  static const int _otherNodes = 150;
  static const int _embeddedNodes = 60;
  static const int _strayNodes = 30;
  static const int _maxDepth = 16;

  final int seed;
  final int mainNodes;
  final int total;

  late final parent = Int32List(total);
  late final kind = Uint8List(total);
  late final firstChild = Int32List(total);
  late final childCount = Int32List(total);
  late final size = Int64List(total);
  late final modified = Int64List(total);
  late final created = Int64List(total);
  late final flags = Uint32List(total);
  late final ext = Uint8List(total);
  late final content = Uint8List(total);
  late final instanceOf = Uint8List(total);
  final fixedNames = <int, String>{};
  final queue = <_Work>[];
  int next = 0;
  int currentInstance = 0;
  int instanceFlags = 0;

  static final int _weightSum = _extWeights.reduce((a, b) => a + b);

  int _hash(int id, int salt) => mix64(id * 0x9E3779B97F4A7C15 + seed * 31 + salt);

  /// Allocates the block of [n] children of [dir].
  int _alloc(int dir, int n) {
    final first = next;
    firstChild[dir] = first;
    childCount[dir] = n;
    for (var i = 0; i < n; i++) {
      parent[first + i] = dir;
      instanceOf[first + i] = currentInstance;
    }
    next += n;
    return first;
  }

  int _newRoot() {
    final id = next++;
    parent[id] = -1;
    instanceOf[id] = currentInstance;
    _dir(id);
    return id;
  }

  void _times(int id) {
    final h = _hash(id, 1) & 0x7fffffffffffffff;
    // 2012-01-01 .. 2026-01-01
    const start = 1325376000000;
    const span = 441849600000;
    final m = start + h % span;
    modified[id] = h % 67 == 0 ? noTime : m;
    created[id] = h % 101 == 0 ? noTime : m - (h >>> 20) % 8640000000;
  }

  void _dir(int id, {int nodeKind = kDir, bool deleted = false, String? name}) {
    kind[id] = nodeKind;
    flags[id] = deleted ? fDeleted : 0;
    _times(id);
    if (nodeKind == kVirtual) {
      modified[id] = noTime;
      created[id] = noTime;
    }
    if (name != null) fixedNames[id] = name;
  }

  /// A file whose attributes come from the hash of its id.
  void _randomFile(int id) {
    final h = _hash(id, 2) & 0x7fffffffffffffff;
    var pick = h % _weightSum;
    var e = 0;
    while (pick >= _extWeights[e]) {
      pick -= _extWeights[e];
      e++;
    }
    // Sizes from a few bytes to several GiB (beyond 32 bits), small ones
    // being the most common.
    final magnitude = 6 + (h >>> 12) % 21 + ((h >>> 40) % 97 == 0 ? 7 : 0);
    final bytes = ((h >>> 17) % (1 << magnitude)) + 1;

    var f = 0;
    final roll = (h >>> 7) % 1000;
    if (roll < 40) {
      f = fDeleted;
    } else if (roll < 45) {
      f = fOverlap;
    } else if (roll < 48) {
      f = fTornRecord;
    } else if (roll < 53) {
      f = fNoData;
    } else if (roll < 63) {
      f = fMissingFragments | (roll.isEven ? fSizeEstimated : 0);
    } else if (roll < 66) {
      f = fOutOfBounds;
    } else if (roll < 86) {
      f = fOverwritten | (roll.isEven ? fDeleted : 0);
    } else if (roll < 88) {
      f = fBadSectors;
    } else if (roll < 93) {
      f = fSignatureMismatch;
    } else if (roll < 103) {
      f = fCompressed;
    } else if (roll < 106) {
      f = fEncrypted;
    } else if (roll < 109) {
      f = fPlaceholder;
    } else if (roll < 400) {
      f = fSignatureOk;
    } else if (roll < 410) {
      f = fSparse;
    }
    _file(id, e, f & fNoData != 0 ? 0 : bytes, f);
  }

  void _file(
    int id,
    int extIndex,
    int bytes,
    int fileFlags, {
    String? name,
    int contentKind = cBinary,
  }) {
    kind[id] = kFile;
    ext[id] = extIndex;
    size[id] = bytes;
    flags[id] = fileFlags | instanceFlags;
    final extension = fakeExts[extIndex];
    content[id] =
        contentKind != cBinary
            ? contentKind
            : (extension == 'txt' || extension == 'csv' || extension == 'html')
            ? cText
            : cBinary;
    _times(id);
    if (name != null) fixedNames[id] = name;
  }

  static int _extIndex(String extension) {
    final index = fakeExts.indexOf(extension);
    if (index < 0) throw ArgumentError.value(extension, 'extension');
    return index;
  }

  /// Creates exactly [work].budget nodes below [work].dir, queueing the
  /// subfolders for later so that every folder's children are contiguous.
  void _expand(_Work work) {
    final dir = work.dir;
    final budget = work.budget;
    switch (work.style) {
      case _Style.flat:
        final first = _alloc(dir, budget);
        for (var i = 0; i < budget; i++) {
          _randomFile(first + i);
        }
      case _Style.pairDirs:
        // budget = 3 * m: m subfolders with two files each.
        final m = budget ~/ 3;
        final first = _alloc(dir, m);
        for (var i = 0; i < m; i++) {
          _dir(first + i);
          queue.add(_Work(first + i, 2, work.depth + 1, _Style.flat));
        }
      case _Style.normal:
        if (budget == 0) {
          firstChild[dir] = next;
          return;
        }
        final h = _hash(dir, 3) & 0x7fffffffffffffff;
        var files = budget < 3 ? budget : 2 + h % 30;
        if (files > budget) files = budget;
        var dirs = 0;
        final remaining = budget - files;
        if (work.depth >= _maxDepth || remaining == 0) {
          files = budget;
        } else {
          dirs = 1 + (h >>> 9) % 6;
          if (dirs > remaining) dirs = remaining;
        }
        final first = _alloc(dir, dirs + files);
        if (dirs > 0) {
          final rest = budget - files - dirs;
          final weights = List<int>.generate(
            dirs,
            (i) => 1 + (_hash(first + i, 4) & 0x7fffffff) % 8,
          );
          final weightSum = weights.reduce((a, b) => a + b);
          var given = 0;
          for (var i = 0; i < dirs; i++) {
            // Roughly one folder in forty was deleted.
            _dir(first + i, deleted: (_hash(first + i, 5) & 0xffff) % 40 == 0);
            var share = rest * weights[i] ~/ weightSum;
            if (i == dirs - 1) share = rest - given;
            given += share;
            queue.add(_Work(first + i, share, work.depth + 1, _Style.normal));
          }
        }
        for (var i = dirs; i < dirs + files; i++) {
          _randomFile(first + i);
          // Files of a deleted folder are mostly deleted too.
          if (flags[dir] & fDeleted != 0 && i.isEven) flags[first + i] |= fDeleted;
        }
    }
  }

  void _drain() {
    // The queue grows while it is processed (breadth first).
    for (var i = 0; i < queue.length; i++) {
      _expand(queue[i]);
    }
    queue.clear();
  }

  FakeTree build() {
    final instances = <FakeInstance>[];

    // --- main instance -----------------------------------------------------
    currentInstance = 0;
    instanceFlags = 0;
    final root = _newRoot();
    fixedNames[root] = '';
    final huge = mainNodes ~/ 6;
    var many = mainNodes ~/ 40;
    if (many > 4000) many = 4000;
    const lostDirs = 5;
    const lostFilesEach = 6;
    const orphanFiles = 12;
    const topDirs = [
      'Documents', 'Photos', 'Music', 'Videos', 'Projects', 'Work', //
      'Backups', 'Downloads', 'Archive',
    ];
    const rootFiles = 5;
    const special = 8;
    final fixed =
        1 + // root
        special + topDirs.length + rootFiles + // children of the root
        fakeFlagNames.length + // Flag samples
        8 + // Preview samples
        huge +
        3 * many +
        lostDirs * (1 + lostFilesEach) +
        orphanFiles +
        8 + // Deleted folder
        6; // Mixed deleted
    final rest = mainNodes - fixed;
    if (rest < 0) throw StateError('mainNodes too small: $mainNodes < $fixed');

    final c = _alloc(root, special + topDirs.length + rootFiles);
    final flagSamples = c;
    final previewSamples = c + 1;
    final hugeFolder = c + 2;
    final manyFolders = c + 3;
    final lostRoot = c + 4;
    final orphanRoot = c + 5;
    final deletedOnly = c + 6;
    final mixedDeleted = c + 7;
    _dir(flagSamples, name: 'Flag samples');
    _dir(previewSamples, name: 'Preview samples');
    _dir(hugeFolder, name: 'Huge folder');
    _dir(manyFolders, name: 'Many folders');
    _dir(lostRoot, nodeKind: kVirtual, name: '[Lost folders]');
    _dir(orphanRoot, nodeKind: kVirtual, name: '[Orphaned files]');
    _dir(deletedOnly, deleted: true, name: 'Deleted folder');
    _dir(mixedDeleted, deleted: true, name: 'Mixed deleted');
    final firstTop = c + special;
    for (var i = 0; i < topDirs.length; i++) {
      _dir(firstTop + i, name: topDirs[i]);
    }
    for (var i = 0; i < rootFiles; i++) {
      _randomFile(firstTop + topDirs.length + i);
    }

    // Flag samples: one file per flag, with exactly that flag.
    final flagFirst = _alloc(flagSamples, fakeFlagNames.length);
    for (var i = 0; i < fakeFlagNames.length; i++) {
      final bit = 1 << i;
      _file(
        flagFirst + i,
        _extIndex('pdf'),
        bit == fNoData ? 0 : 100000 + 1000 * i,
        bit,
        name: '${fakeFlagNames[i]}.pdf',
      );
    }

    // Preview samples: files whose content read_file serves faithfully.
    final p = _alloc(previewSamples, 8);
    _file(p, _extIndex('jpg'), jpegSampleBytes, fSignatureOk, name: 'picture.jpg', contentKind: cJpegSample);
    _file(p + 1, _extIndex('png'), pngSampleBytes, fSignatureOk, name: 'picture.png', contentKind: cPng);
    _file(p + 2, _extIndex('bmp'), bmpSampleBytes, fSignatureOk, name: 'picture.bmp', contentKind: cBmp);
    _file(p + 3, _extIndex('txt'), 6400, 0, name: 'notes.txt', contentKind: cText);
    _file(p + 4, _extIndex('txt'), 0, fNoData, name: 'empty.txt', contentKind: cText);
    _file(p + 5, _extIndex('zip'), 20 * 1024 * 1024, 0, name: 'large archive.zip');
    _file(
      p + 6,
      _extIndex('jpg'),
      48000,
      fSignatureMismatch,
      name: 'damaged picture.jpg',
      contentKind: cTruncatedJpeg,
    );
    _file(p + 7, _extIndex('docx'), 250000, fCompressed, name: 'compressed.docx');

    queue.add(_Work(hugeFolder, huge, 1, _Style.flat));
    queue.add(_Work(manyFolders, 3 * many, 1, _Style.pairDirs));

    // [Lost folders]: lost_dir nodes with files.
    final l = _alloc(lostRoot, lostDirs);
    for (var i = 0; i < lostDirs; i++) {
      _dir(l + i, nodeKind: kLostDir);
      queue.add(_Work(l + i, lostFilesEach, 2, _Style.flat));
    }

    // [Orphaned files]
    final o = _alloc(orphanRoot, orphanFiles);
    for (var i = 0; i < orphanFiles; i++) {
      _randomFile(o + i);
      fixedNames[o + i] = 'File #${5000 + i * 7}.${fakeExts[ext[o + i]].isEmpty ? 'bin' : fakeExts[ext[o + i]]}';
    }

    // Deleted folder: only deleted files. Mixed deleted: half and half.
    final d = _alloc(deletedOnly, 8);
    for (var i = 0; i < 8; i++) {
      _randomFile(d + i);
      flags[d + i] = fDeleted;
    }
    final m = _alloc(mixedDeleted, 6);
    for (var i = 0; i < 6; i++) {
      _randomFile(m + i);
      flags[m + i] = i < 3 ? fDeleted : fSignatureOk;
    }

    // The ordinary folders share what is left.
    var given = 0;
    for (var i = 0; i < topDirs.length; i++) {
      var share = rest ~/ topDirs.length;
      if (i == topDirs.length - 1) share = rest - given;
      given += share;
      queue.add(_Work(firstTop + i, share, 1, _Style.normal));
    }
    _drain();
    if (next != mainNodes) {
      throw StateError('main instance has $next nodes, planned $mainNodes');
    }
    instances.add(
      FakeInstance(
        id: 0,
        kind: 'main',
        label: '',
        root: root,
        firstNode: 0,
        endNode: next,
        lost: lostRoot,
        orphans: orphanRoot,
      ),
    );
    final landmarks = FakeLandmarks(
      flagSamples: flagSamples,
      previewSamples: previewSamples,
      hugeFolder: hugeFolder,
      hugeFolderFiles: huge,
      manyFolders: manyFolders,
      manyFoldersCount: many,
      deletedOnlyFolder: deletedOnly,
      mixedDeletedFolder: mixedDeleted,
      documents: firstTop,
      sampleJpeg: p,
      samplePng: p + 1,
      sampleBmp: p + 2,
      sampleText: p + 3,
      sampleEmpty: p + 4,
      sampleBinary: p + 5,
      sampleTruncatedJpeg: p + 6,
      sampleCompressed: p + 7,
    );

    // --- the other instances -----------------------------------------------
    void addInstance(
      String kindName,
      String label,
      int nodes,
      int extraFlags, {
      String? embeddedIn,
      bool flat = false,
    }) {
      currentInstance = instances.length;
      instanceFlags = extraFlags;
      final first = next;
      final instanceRoot = _newRoot();
      fixedNames[instanceRoot] = label;
      queue.add(
        _Work(instanceRoot, nodes - 1, 0, flat ? _Style.flat : _Style.normal),
      );
      _drain();
      if (next - first != nodes) {
        throw StateError('instance $kindName has ${next - first} nodes, planned $nodes');
      }
      instances.add(
        FakeInstance(
          id: instances.length,
          kind: kindName,
          label: label,
          root: instanceRoot,
          firstNode: first,
          endNode: next,
          embeddedIn: embeddedIn,
        ),
      );
    }

    addInstance('older', '[Older file system]', _olderNodes, fOlderGeneration);
    addInstance('other', '[File system 2]', _otherNodes, 0);
    addInstance(
      'embedded',
      '[Inside backup.vhd]',
      _embeddedNodes,
      fEmbedded,
      embeddedIn: 'Backups/backup.vhd',
    );
    addInstance('strays', '[Stray records]', _strayNodes, fStray, flat: true);
    if (next != total) throw StateError('built $next nodes, planned $total');

    // --- aggregates, bottom-up (children have larger ids than parents) ------
    final aggFiles = Int32List(total);
    final aggDeletedFiles = Int32List(total);
    final aggDeletedSize = Int64List(total);
    final subdirsTotal = Int32List(total);
    final subdirsVisible = Int32List(total);
    for (var id = total - 1; id >= 0; id--) {
      final up = parent[id];
      if (up < 0) continue;
      if (kind[id] == kFile) {
        if (flags[id] & fDeleted != 0) {
          aggDeletedFiles[up] += 1;
          aggDeletedSize[up] += size[id];
        } else {
          aggFiles[up] += 1;
          size[up] += size[id];
        }
      } else {
        aggFiles[up] += aggFiles[id];
        aggDeletedFiles[up] += aggDeletedFiles[id];
        aggDeletedSize[up] += aggDeletedSize[id];
        size[up] += size[id];
        subdirsTotal[up] += 1;
        if (flags[id] & fDeleted == 0 || aggFiles[id] > 0) subdirsVisible[up] += 1;
      }
    }

    return FakeTree._(
      seed: seed,
      parent: parent,
      kind: kind,
      firstChild: firstChild,
      childCount: childCount,
      size: size,
      modified: modified,
      created: created,
      flags: flags,
      ext: ext,
      content: content,
      instanceOf: instanceOf,
      aggFiles: aggFiles,
      aggDeletedFiles: aggDeletedFiles,
      aggDeletedSize: aggDeletedSize,
      subdirsTotal: subdirsTotal,
      subdirsVisible: subdirsVisible,
      fixedNames: fixedNames,
      instances: instances,
      landmarks: landmarks,
    );
  }
}
