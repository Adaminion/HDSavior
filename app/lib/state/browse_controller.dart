/// What the Results screen shows: the current folder, the view filters, the
/// sort order, and the listings that follow from them.
library;

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';
import 'catalog_controller.dart';
import 'row_cache.dart';

/// A folder together with the ids of its ancestors (root first), which is
/// what the selection model needs to know about every node below it.
@immutable
final class FolderLocation {
  const FolderLocation(this.id, [this.ancestors = const []]);

  /// Node id of the folder.
  final int id;

  /// Ids of its ancestors, root first; empty for an instance root.
  final List<int> ancestors;

  /// The ancestor ids of a direct child of this folder.
  List<int> get childAncestors => [...ancestors, id];

  /// The location of the child folder [child].
  FolderLocation child(int child) => FolderLocation(child, childAncestors);

  @override
  bool operator ==(Object other) =>
      other is FolderLocation &&
      other.id == id &&
      listEquals(other.ancestors, ancestors);

  @override
  int get hashCode => Object.hash(id, Object.hashAll(ancestors));

  @override
  String toString() => 'FolderLocation($id, $ancestors)';
}

/// The filters of the file table.
@immutable
final class ViewFilters {
  const ViewFilters({
    this.showDeleted = false,
    this.ratings,
    this.exts,
    this.query = '',
    this.minSize,
    this.maxSize,
  });

  /// Whether deleted files (and folders that hold only deleted files) are
  /// shown.
  final bool showDeleted;

  /// Only files with these ratings; `null`: any. (An empty list also means
  /// any: the protocol has no way to say "none".)
  final List<Rating>? ratings;

  /// Only files with these extensions (lower case, without the dot); `null`
  /// or empty: any.
  final List<String>? exts;

  /// Search text: a case-insensitive part of the file name. Empty: no
  /// search.
  final String query;

  /// Only files of at least / at most this size in bytes; `null`: no limit.
  final int? minSize;
  final int? maxSize;

  /// Whether the table shows search results (files from the whole subtree)
  /// instead of the direct children of the folder. A name or size filter
  /// needs `search`; `list_children` only filters by type and rating.
  bool get isSearch =>
      query.trim().isNotEmpty || minSize != null || maxSize != null;

  /// Whether files rated `damaged` are hidden.
  bool get hidesDamaged =>
      ratingsOrNull != null && !ratings!.contains(Rating.damaged);

  /// These filters with damaged files hidden ([hide]) or with no rating
  /// filter at all.
  ViewFilters withDamagedHidden(bool hide) => copyWith(
    ratings: () => hide
        ? const [Rating.good, Rating.uncertain, Rating.unsupported]
        : null,
  );

  /// A copy with some members replaced. The nullable members take a function
  /// so that they can be set to `null`: `copyWith(exts: () => null)`.
  ViewFilters copyWith({
    bool? showDeleted,
    List<Rating>? Function()? ratings,
    List<String>? Function()? exts,
    String? query,
    int? Function()? minSize,
    int? Function()? maxSize,
  }) => ViewFilters(
    showDeleted: showDeleted ?? this.showDeleted,
    ratings: ratings == null ? this.ratings : ratings(),
    exts: exts == null ? this.exts : exts(),
    query: query ?? this.query,
    minSize: minSize == null ? this.minSize : minSize(),
    maxSize: maxSize == null ? this.maxSize : maxSize(),
  );

  /// [exts] as the protocol wants it: `null` for "any", never empty.
  List<String>? get extsOrNull => exts == null || exts!.isEmpty ? null : exts;

  /// [ratings] as the protocol wants it: `null` for "any", never empty.
  List<Rating>? get ratingsOrNull =>
      ratings == null || ratings!.isEmpty ? null : ratings;

  /// The protocol filter for `search`.
  FileFilter get fileFilter => FileFilter(
    query: query.trim().isEmpty ? null : query.trim(),
    exts: extsOrNull,
    minSize: minSize,
    maxSize: maxSize,
    ratings: ratingsOrNull,
  );

  @override
  bool operator ==(Object other) =>
      other is ViewFilters &&
      other.showDeleted == showDeleted &&
      listEquals(other.ratings, ratings) &&
      listEquals(other.exts, exts) &&
      other.query == query &&
      other.minSize == minSize &&
      other.maxSize == maxSize;

  @override
  int get hashCode => Object.hash(
    showDeleted,
    ratings == null ? null : Object.hashAll(ratings!),
    exts == null ? null : Object.hashAll(exts!),
    query,
    minSize,
    maxSize,
  );
}

/// Navigation and filters of the Results screen.
///
/// The controller owns no rows. [rows] is the paged listing for the current
/// folder, filters and sort order; [folders] gives the paged child folders of
/// any node for the tree. Both come from the catalog's row cache and are
/// replaced when what they show changes; listeners are notified, and
/// requests of the listing that was left behind are cancelled.
///
/// When the catalog gets a new epoch, the controller returns to the root of
/// the main file system (node ids of the old epoch mean nothing any more).
class BrowseController extends ChangeNotifier {
  BrowseController(this._catalog) {
    _catalog.addListener(_onCatalogChanged);
    _seenEpochChanges = _catalog.epochChanges;
    _folder = _mainRoot();
  }

  final CatalogController _catalog;
  int _seenEpochChanges = 0;

  FolderLocation? _folder;
  ViewFilters _filters = const ViewFilters();
  SortKey _sort = SortKey.name;
  bool _descending = false;
  PagedRows? _rows;

  FolderLocation? _mainRoot() => switch (_catalog.info?.mainInstance) {
    null => null,
    final main => FolderLocation(main.root),
  };

  void _onCatalogChanged() {
    if (_catalog.epochChanges == _seenEpochChanges) return;
    _seenEpochChanges = _catalog.epochChanges;
    _folder = _mainRoot();
    _rows = null;
    notifyListeners();
  }

  /// The folder whose content the table shows; `null` without a catalog.
  FolderLocation? get folder => _folder;

  ViewFilters get filters => _filters;

  set filters(ViewFilters value) {
    if (value == _filters) return;
    _filters = value;
    _queryChanged();
  }

  SortKey get sort => _sort;

  bool get descending => _descending;

  /// Whether the table shows search results below [folder] instead of its
  /// direct children.
  bool get isSearch => _filters.isSearch;

  /// Sorts the table by [key]. Without [descending], choosing the current
  /// key again reverses the direction and a new key starts ascending.
  void sortBy(SortKey key, {bool? descending}) {
    final direction = descending ?? (key == _sort ? !_descending : false);
    if (key == _sort && direction == _descending) return;
    _sort = key;
    _descending = direction;
    _queryChanged();
  }

  /// Shows the content of [location] in the table.
  void open(FolderLocation location) {
    if (location == _folder) return;
    _folder = location;
    _queryChanged();
  }

  /// Shows the root of [instance] (default: the main file system).
  void openRoot([Instance? instance]) {
    final target = instance ?? _catalog.info?.mainInstance;
    if (target != null) open(FolderLocation(target.root));
  }

  void _queryChanged() {
    _rows?.cancelPending();
    _rows = null;
    notifyListeners();
  }

  /// The query behind [rows]; `null` without a catalog.
  RowQuery? get query {
    final folder = _folder;
    if (folder == null || !_catalog.hasCatalog) return null;
    if (_filters.isSearch) {
      return SearchQuery(
        under: folder.id,
        filter: _filters.fileFilter,
        showDeleted: _filters.showDeleted,
        sort: _sort,
        desc: _descending,
      );
    }
    return ChildrenQuery(
      node: folder.id,
      exts: _filters.extsOrNull,
      ratings: _filters.ratingsOrNull,
      showDeleted: _filters.showDeleted,
      sort: _sort,
      desc: _descending,
    );
  }

  /// The rows of the table: the children of [folder], or the search results
  /// below it when [isSearch]. `null` without a catalog.
  PagedRows? get rows {
    final current = _rows;
    if (current != null && !current.isClosed) return current;
    final query = this.query;
    return _rows = query == null ? null : _catalog.rows.open(query);
  }

  /// The child folders of [node], for the tree: folders only, by name, with
  /// the same "show deleted" setting as the table. `null` without a catalog.
  PagedRows? folders(int node) {
    if (!_catalog.hasCatalog) return null;
    return _catalog.rows.open(
      ChildrenQuery(
        node: node,
        dirsOnly: true,
        showDeleted: _filters.showDeleted,
      ),
    );
  }

  /// The ancestor ids (root first) of a row of [rows], as the selection
  /// model wants them: those the engine sent with a search result, else the
  /// current folder and its ancestors.
  List<int> ancestorsOf(NodeRow row) =>
      row.ancestorIds ?? _folder?.childAncestors ?? const [];

  @override
  void dispose() {
    _catalog.removeListener(_onCatalogChanged);
    super.dispose();
  }
}
