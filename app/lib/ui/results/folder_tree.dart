/// The model behind the folder tree of the Results screen: a lazily loaded,
/// paged tree flattened into a list of fixed-height entries.
///
/// The top-level entries are the roots of the file systems the scan found
/// (`CatalogInfo.instances`, the main one first and expanded). Below them
/// each expanded folder lists its child folders through a [PagedRows]
/// (`list_children` with `dirs_only`), so a folder with fifty thousand
/// subfolders costs a few pages, exactly like the file table.
///
/// An expanded folder is remembered by its position in its parent's
/// listing; positions are stable within an epoch for a given listing. If a
/// loaded row shows another id at that position (the listing changed), the
/// expansion is dropped.
library;

import 'dart:async';
import 'dart:collection';

import 'package:flutter/foundation.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';

/// One line of the flattened tree.
@immutable
final class TreeEntry {
  const TreeEntry._({
    required this.depth,
    required this.ancestors,
    this.instance,
    this.row,
    this.expanded = false,
    this.loading = false,
    this.error,
    this._parent,
    this._position = 0,
  });

  /// 0 for the root of a file system, 1 for its folders, and so on.
  final int depth;

  /// Ids of the ancestors, root first, without the entry itself (what the
  /// selection model needs).
  final List<int> ancestors;

  /// The file system, for a top-level entry.
  final Instance? instance;

  /// The folder, for entries below the top level; `null` while its page
  /// loads.
  final NodeRow? row;

  final bool expanded;

  /// A placeholder: the row (or the size of an expanded folder's listing)
  /// is not known yet.
  final bool loading;

  /// Why the listing of the parent could not be loaded.
  final EngineException? error;

  final _Folder? _parent;
  final int _position;

  /// The node id; `null` while loading.
  int? get id => instance?.root ?? row?.id;

  bool get isRoot => instance != null;

  /// Whether the entry can be expanded (it may have child folders).
  bool get expandable => instance != null || (row?.hasChildren ?? false);

  @override
  bool operator ==(Object other) =>
      other is TreeEntry &&
      other.depth == depth &&
      other.id == id &&
      other.expanded == expanded &&
      other.loading == loading &&
      identical(other._parent, _parent) &&
      other._position == _position;

  @override
  int get hashCode => Object.hash(depth, id, expanded, loading, _position);
}

/// An expanded folder (or file-system root).
final class _Folder {
  _Folder(this.id, this.ancestors, this.rows);

  final int id;
  final List<int> ancestors;
  final PagedRows rows;

  /// Expanded child folders by their position in [rows].
  final SplayTreeMap<int, _Folder> children = SplayTreeMap();

  /// Entries shown below this folder: its children and, recursively, theirs.
  /// A listing whose size is not known yet shows one placeholder.
  int get size {
    final total = rows.total;
    if (total == null) return 1;
    var size = total;
    for (final child in children.values) {
      size += child.size;
    }
    return size;
  }

  List<int> get childAncestors => [...ancestors, id];

  void forEach(void Function(_Folder folder) action) {
    action(this);
    for (final child in children.values) {
      child.forEach(action);
    }
  }
}

/// See the library documentation.
class FolderTree extends ChangeNotifier {
  FolderTree(this._catalog, this._browse) {
    _seenEpochChanges = _catalog.epochChanges;
    _seenShowDeleted = _browse.filters.showDeleted;
    _catalog.addListener(_onCatalogChanged);
    _browse.addListener(_onBrowseChanged);
    _rebuild(expandMain: true);
  }

  final CatalogController _catalog;
  final BrowseController _browse;

  List<Instance> _instances = const [];

  /// Expanded file-system roots by instance position.
  final Map<int, _Folder> _roots = {};
  int _seenEpochChanges = 0;
  bool _seenShowDeleted = false;
  bool _disposed = false;
  bool _repairScheduled = false;

  /// The file systems shown at the top level, main first.
  List<Instance> get instances => _instances;

  void _onCatalogChanged() {
    final info = _catalog.info;
    if (_catalog.epochChanges != _seenEpochChanges) {
      _seenEpochChanges = _catalog.epochChanges;
      _rebuild(expandMain: true);
      return;
    }
    // Same epoch (carving, a geometry): the instance figures may change.
    final instances = info?.instances ?? const <Instance>[];
    if (!identical(instances, _instances)) {
      _instances = instances;
      notifyListeners();
    }
  }

  void _onBrowseChanged() {
    final showDeleted = _browse.filters.showDeleted;
    if (showDeleted == _seenShowDeleted) return;
    _seenShowDeleted = showDeleted;
    // Other listings: keep which file systems were open, forget the rest.
    final open = Set.of(_roots.keys);
    _rebuild(expandMain: false);
    for (final index in open) {
      if (index < _instances.length) _expandRoot(index);
    }
    notifyListeners();
  }

  void _rebuild({required bool expandMain}) {
    for (final root in _roots.values) {
      root.forEach((folder) => folder.rows.removeListener(_changed));
    }
    _roots.clear();
    _instances = _catalog.info?.instances ?? const [];
    if (expandMain && _instances.isNotEmpty) _expandRoot(0);
    notifyListeners();
  }

  void _changed() {
    if (!_disposed) notifyListeners();
  }

  _Folder? _open(int id, List<int> ancestors) {
    final rows = _browse.folders(id);
    if (rows == null) return null;
    rows.addListener(_changed);
    rows.load();
    return _Folder(id, ancestors, rows);
  }

  void _expandRoot(int index) {
    final folder = _open(_instances[index].root, const []);
    if (folder != null) _roots[index] = folder;
  }

  /// Number of entries.
  int get length {
    var length = 0;
    for (var i = 0; i < _instances.length; i++) {
      length += 1 + (_roots[i]?.size ?? 0);
    }
    return length;
  }

  /// The entry at [index] (0 <= index < [length]). Asking for an entry whose
  /// row is not loaded starts loading its page; the tree notifies when it
  /// arrives. Safe to call during a build.
  TreeEntry entryAt(int index) {
    RangeError.checkValidIndex(index, this, 'index', length);
    var rest = index;
    for (var i = 0; i < _instances.length; i++) {
      final instance = _instances[i];
      final root = _roots[i];
      if (rest == 0) {
        return TreeEntry._(depth: 0, ancestors: const [], instance: instance, expanded: root != null);
      }
      rest--;
      if (root != null) {
        final size = root.size;
        if (rest < size) return _within(root, rest, 1);
        rest -= size;
      }
    }
    throw StateError('unreachable');
  }

  TreeEntry _within(_Folder folder, int index, int depth) {
    final ancestors = folder.childAncestors;
    if (folder.rows.total == null) {
      return TreeEntry._(depth: depth, ancestors: ancestors, loading: true, error: folder.rows.error, parent: folder);
    }
    var rest = index;
    var next = 0; // position of the next child not yet passed
    for (final MapEntry(key: position, value: child) in folder.children.entries) {
      final plain = position - next;
      if (rest < plain) return _child(folder, next + rest, depth, ancestors);
      rest -= plain;
      if (rest == 0) return _child(folder, position, depth, ancestors, expanded: child);
      rest--;
      final size = child.size;
      if (rest < size) return _within(child, rest, depth + 1);
      rest -= size;
      next = position + 1;
    }
    return _child(folder, next + rest, depth, ancestors);
  }

  TreeEntry _child(_Folder folder, int position, int depth, List<int> ancestors, {_Folder? expanded}) {
    final row = folder.rows.rowAt(position);
    if (expanded != null && row != null && row.id != expanded.id) {
      // The listing changed under the expansion: drop it after this build.
      _scheduleRepair();
    }
    return TreeEntry._(
      depth: depth,
      ancestors: ancestors,
      row: row,
      expanded: expanded != null,
      loading: row == null,
      error: row == null ? folder.rows.error : null,
      parent: folder,
      position: position,
    );
  }

  void _scheduleRepair() {
    if (_repairScheduled) return;
    _repairScheduled = true;
    scheduleMicrotask(() {
      _repairScheduled = false;
      if (_disposed) return;
      var changed = false;
      for (final root in _roots.values) {
        root.forEach((folder) {
          final wrong = [
            for (final MapEntry(key: position, value: child) in folder.children.entries)
              if (folder.rows.peek(position) case final row? when row.id != child.id) position,
          ];
          for (final position in wrong) {
            folder.children.remove(position)!.forEach((gone) => gone.rows.removeListener(_changed));
            changed = true;
          }
        });
      }
      if (changed) notifyListeners();
    });
  }

  /// Expands [entry]. Does nothing for an entry that cannot be expanded or
  /// is still loading.
  void expand(TreeEntry entry) {
    if (entry.expanded || !entry.expandable) return;
    final instance = entry.instance;
    if (instance != null) {
      final index = _instances.indexOf(instance);
      if (index < 0) return;
      _expandRoot(index);
      notifyListeners();
      return;
    }
    final parent = entry._parent;
    final row = entry.row;
    if (parent == null || row == null) return;
    final folder = _open(row.id, entry.ancestors);
    if (folder == null) return;
    parent.children[entry._position] = folder;
    notifyListeners();
  }

  /// Collapses [entry] (and forgets what was expanded below it).
  void collapse(TreeEntry entry) {
    if (!entry.expanded) return;
    final instance = entry.instance;
    _Folder? removed;
    if (instance != null) {
      final index = _instances.indexOf(instance);
      removed = _roots.remove(index);
    } else {
      removed = entry._parent?.children.remove(entry._position);
    }
    removed?.forEach((folder) => folder.rows.removeListener(_changed));
    notifyListeners();
  }

  void toggle(TreeEntry entry) => entry.expanded ? collapse(entry) : expand(entry);

  /// The index of the parent entry of the entry at [index], or `null` for a
  /// top-level entry.
  int? parentIndex(int index) {
    final depth = entryAt(index).depth;
    if (depth == 0) return null;
    for (var i = index - 1; i >= 0; i--) {
      if (entryAt(i).depth < depth) return i;
    }
    return null;
  }

  /// Retries the listings that failed.
  void retry() {
    for (final root in _roots.values) {
      root.forEach((folder) => folder.rows.retry());
    }
  }

  @override
  void dispose() {
    _disposed = true;
    _catalog.removeListener(_onCatalogChanged);
    _browse.removeListener(_onBrowseChanged);
    for (final root in _roots.values) {
      root.forEach((folder) => folder.rows.removeListener(_changed));
    }
    super.dispose();
  }
}
