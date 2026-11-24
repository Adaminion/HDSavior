/// Paged access to the engine's listings.
///
/// The UI holds no file data: a folder can have a million entries, so a list
/// widget asks for the rows it is about to show ([PagedRows.rowAt]) and the
/// cache fetches them from the engine in pages of fixed size, keeps a bounded
/// number of pages in memory and cancels requests that scrolling has made
/// obsolete.
library;

import 'dart:async';
import 'dart:collection';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';

/// What a listing shows. Equal queries share one [PagedRows].
@immutable
sealed class RowQuery {
  const RowQuery({
    this.sort = SortKey.name,
    this.desc = false,
    this.showDeleted = false,
  });

  final SortKey sort;
  final bool desc;

  /// Whether deleted files (and folders that hold only deleted files) are
  /// listed.
  final bool showDeleted;
}

bool _sameList<T>(List<T>? a, List<T>? b) {
  if (identical(a, b)) return true;
  if (a == null || b == null || a.length != b.length) return false;
  for (var i = 0; i < a.length; i++) {
    if (a[i] != b[i]) return false;
  }
  return true;
}

/// The children of folder [node] (`list_children`).
final class ChildrenQuery extends RowQuery {
  const ChildrenQuery({
    required this.node,
    this.dirsOnly = false,
    this.exts,
    this.ratings,
    super.sort,
    super.desc,
    super.showDeleted,
  });

  final int node;

  /// Only folders: what the tree shows.
  final bool dirsOnly;

  /// Filter for files (folders are always listed); `null` means any.
  final List<String>? exts;
  final List<Rating>? ratings;

  @override
  bool operator ==(Object other) =>
      other is ChildrenQuery &&
      other.node == node &&
      other.dirsOnly == dirsOnly &&
      other.sort == sort &&
      other.desc == desc &&
      other.showDeleted == showDeleted &&
      _sameList(other.exts, exts) &&
      _sameList(other.ratings, ratings);

  @override
  int get hashCode => Object.hash(
    node,
    dirsOnly,
    sort,
    desc,
    showDeleted,
    exts == null ? null : Object.hashAll(exts!),
    ratings == null ? null : Object.hashAll(ratings!),
  );

  @override
  String toString() =>
      'ChildrenQuery(node: $node, dirsOnly: $dirsOnly, sort: ${sort.wire}, '
      'desc: $desc, showDeleted: $showDeleted, exts: $exts, ratings: $ratings)';
}

/// The files below [under] that match [filter] (`search`). Rows carry their
/// path and ancestor ids.
final class SearchQuery extends RowQuery {
  const SearchQuery({
    this.under,
    this.filter = FileFilter.any,
    super.sort,
    super.desc,
    super.showDeleted,
  });

  /// The subtree to search; `null` means the main root.
  final int? under;
  final FileFilter filter;

  @override
  bool operator ==(Object other) =>
      other is SearchQuery &&
      other.under == under &&
      other.filter == filter &&
      other.sort == sort &&
      other.desc == desc &&
      other.showDeleted == showDeleted;

  @override
  int get hashCode => Object.hash(under, filter, sort, desc, showDeleted);

  @override
  String toString() =>
      'SearchQuery(under: $under, filter: $filter, sort: ${sort.wire}, '
      'desc: $desc, showDeleted: $showDeleted)';
}

/// Fetches one page of [query] from the engine. [cancel] is cancelled when
/// the page is no longer wanted.
typedef RowPageFetcher =
    Future<RowsPage> Function(
      RowQuery query,
      int offset,
      int limit,
      CancelToken cancel,
    );

/// One listing, loaded page by page on demand.
///
/// A list widget calls [rowAt] for every row it builds and listens to this
/// object; it rebuilds when pages arrive. Rows that are not loaded yet are
/// `null` (show a placeholder). [total] is `null` until the first page has
/// arrived; call [load] to fetch it without asking for a row.
///
/// Created by [RowCache.open]. After [RowCache.reset] (a new catalog epoch)
/// the list is [isClosed]: it has no rows and fetches nothing; open a new one.
class PagedRows extends ChangeNotifier {
  PagedRows._(this._cache, this.query);

  final RowCache _cache;

  /// What this list shows.
  final RowQuery query;

  final Map<int, List<NodeRow>> _pages = {};

  /// Requests on their way, least recently wanted first.
  final LinkedHashMap<int, CancelToken> _pending = LinkedHashMap();

  /// Pages wanted since the last turn of the event loop: one build of the
  /// list widget. They are the pages on screen right now.
  final Set<int> _wantedNow = {};
  final Set<int> _failed = {};
  int? _total;
  EngineException? _error;
  bool _closed = false;
  bool _notifyScheduled = false;

  /// Changes when the rows are dropped, so that answers to requests made
  /// before are ignored.
  int _generation = 0;
  Completer<void>? _idle;

  /// Number of rows of the whole listing; `null` until known.
  int? get total => _total;

  /// Whether pages are being fetched.
  bool get isLoading => _pending.isNotEmpty;

  /// Why the last fetch failed; `null` if it did not. A failed page is not
  /// requested again until [retry], [refresh] or [RowCache.invalidate].
  EngineException? get error => _error;

  /// The list belongs to a catalog epoch that has ended.
  bool get isClosed => _closed;

  /// Rows per page.
  int get pageSize => _cache.pageSize;

  /// Pages currently in memory.
  int get loadedPages => _pages.length;

  /// Completes when no request is on its way (immediately if none is).
  Future<void> get idle {
    if (_pending.isEmpty) return Future.value();
    return (_idle ??= Completer<void>()).future;
  }

  /// The row at [index], or `null` if it is not loaded (yet). Asking for a
  /// row that is not loaded starts fetching its page.
  NodeRow? rowAt(int index) {
    RangeError.checkNotNegative(index, 'index');
    if (_closed) return null;
    final page = index ~/ pageSize;
    final rows = _pages[page];
    if (rows == null) {
      final total = _total;
      if (total == null || index < total) _want(page);
      return null;
    }
    _cache._touch(this, page);
    final offset = index - page * pageSize;
    return offset < rows.length ? rows[offset] : null;
  }

  /// The row at [index] if it is in memory; never fetches.
  NodeRow? peek(int index) {
    final rows = _pages[index ~/ pageSize];
    final offset = index % pageSize;
    return rows != null && offset < rows.length ? rows[offset] : null;
  }

  /// Fetches the first page if nothing is known yet (to learn [total]).
  void load() {
    if (_closed || _total != null || _pages.isNotEmpty) return;
    _want(0);
  }

  /// Drops the rows and fetches again what is asked for next. [total] is
  /// kept until the new answer arrives, so a list does not jump.
  void refresh() {
    if (_closed) return;
    _drop();
    _notifyLater();
  }

  /// Forgets a failure, so that the failed pages are requested again.
  void retry() {
    if (_failed.isEmpty && _error == null) return;
    _failed.clear();
    _error = null;
    _notifyLater();
  }

  /// Cancels the requests on their way (the list is about to be replaced by
  /// another one).
  void cancelPending() {
    if (_pending.isEmpty) return;
    final tokens = List.of(_pending.values);
    _pending.clear();
    for (final token in tokens) {
      token.cancel();
    }
    _settle();
    _notifyLater();
  }

  void _want(int page) {
    if (_failed.contains(page)) return;
    if (_wantedNow.isEmpty) scheduleMicrotask(_wantedNow.clear);
    _wantedNow.add(page);
    final known = _pending.remove(page);
    if (known != null) {
      _pending[page] = known; // most recently wanted last
      return;
    }
    if (_pending.length >= _cache.maxPendingPerList) {
      // Make room by cancelling a request that scrolling has left behind:
      // it is not worth its turn at the engine. Pages wanted in this very
      // build are all on screen; if they alone fill the budget, this one
      // waits (it is asked for again when one of them has arrived).
      int? obsolete;
      for (final candidate in _pending.keys) {
        if (!_wantedNow.contains(candidate)) {
          obsolete = candidate;
          break;
        }
      }
      if (obsolete == null) return;
      _pending.remove(obsolete)!.cancel();
    }
    final token = CancelToken();
    _pending[page] = token;
    _notifyLater(); // isLoading changed; rowAt may run during a build
    unawaited(_fetch(page, token));
  }

  Future<void> _fetch(int page, CancelToken token) async {
    final generation = _generation;
    try {
      final result = await _cache._fetch(
        query,
        page * pageSize,
        pageSize,
        token,
      );
      if (generation != _generation || !identical(_pending[page], token)) {
        return;
      }
      _pending.remove(page);
      _error = null;
      _total = result.total;
      _pages[page] = result.rows;
      // Pages beyond a listing that has become shorter are stale.
      final lastPage = result.total == 0 ? 0 : (result.total - 1) ~/ pageSize;
      _pages.removeWhere((index, _) {
        if (index <= lastPage) return false;
        _cache._forget(this, index);
        return true;
      });
      _cache._loaded(this, page);
    } on RequestCancelledException {
      // Nothing to show for it. If the page is still on screen, the next
      // build asks for it again.
      if (identical(_pending[page], token)) _pending.remove(page);
      _settle();
      return;
    } on EngineException catch (e) {
      if (generation != _generation || !identical(_pending[page], token)) {
        return;
      }
      _pending.remove(page);
      _failed.add(page);
      _error = e;
    } on Object {
      // A defect (for example an invalid query): do not leave the list
      // "loading" for ever, and let the error surface.
      if (identical(_pending[page], token)) _pending.remove(page);
      _failed.add(page);
      _settle();
      rethrow;
    }
    _settle();
    if (!_closed) notifyListeners();
  }

  void _settle() {
    if (_pending.isNotEmpty) return;
    _idle?.complete();
    _idle = null;
  }

  void _notifyLater() {
    if (_notifyScheduled || _closed) return;
    _notifyScheduled = true;
    scheduleMicrotask(() {
      _notifyScheduled = false;
      if (!_closed) notifyListeners();
    });
  }

  /// Drops rows and requests; the next [rowAt] fetches afresh.
  void _drop() {
    _generation++;
    final tokens = List.of(_pending.values);
    _pending.clear();
    for (final token in tokens) {
      token.cancel();
    }
    for (final page in _pages.keys) {
      _cache._forget(this, page);
    }
    _pages.clear();
    _failed.clear();
    _error = null;
    _settle();
  }

  void _close() {
    if (_closed) return;
    _drop();
    _total = null;
    _closed = true;
    notifyListeners();
  }

  void _evict(int page) => _pages.remove(page);

  bool get _unused =>
      _pages.isEmpty && _pending.isEmpty && !hasListeners;
}

/// All listings of the current catalog epoch, with one memory budget.
///
/// * Fixed page size ([pageSize] rows per request).
/// * Bounded memory: at most [maxCachedPages] pages are kept across all
///   lists; the least recently used page is dropped first.
/// * Obsolete requests are cancelled: per list at most [maxPendingPerList]
///   pages are on their way, the most recently wanted ones.
class RowCache {
  RowCache({
    required this._fetch,
    this.pageSize = 200,
    this.maxCachedPages = 64,
    this.maxPendingPerList = 4,
    this.maxIdleLists = 256,
  }) : assert(pageSize >= 1 && pageSize <= maxListLimit),
       assert(maxCachedPages >= 1),
       assert(maxPendingPerList >= 1);

  final RowPageFetcher _fetch;

  /// Rows per page (at most 1000, the protocol's limit).
  final int pageSize;

  /// Pages kept in memory across all lists.
  final int maxCachedPages;

  /// Pages of one list that may be on their way at the same time.
  final int maxPendingPerList;

  /// Lists remembered although nobody uses them.
  final int maxIdleLists;

  final Map<RowQuery, PagedRows> _lists = {};

  /// Loaded pages, least recently used first.
  final LinkedHashMap<(PagedRows, int), Null> _recent = LinkedHashMap();

  /// The most recently used page (the last key of [_recent]), to make the
  /// common case of [_touch] free: a list widget reads many rows of the same
  /// page in a row.
  (PagedRows, int)? _newest;

  /// Pages in memory across all lists.
  int get cachedPages => _recent.length;

  /// Lists currently known.
  int get listCount => _lists.length;

  /// The list for [query]: the existing one if the query is already open.
  PagedRows open(RowQuery query) {
    final existing = _lists[query];
    if (existing != null) return existing;
    if (_lists.length >= maxIdleLists) {
      _lists.removeWhere((_, list) => list._unused);
    }
    return _lists[query] = PagedRows._(this, query);
  }

  /// The rows may have changed while node ids stayed valid (carving, a new
  /// geometry, a recovery): every list drops its rows and fetches again on
  /// demand.
  void invalidate() {
    for (final list in _lists.values) {
      list._drop();
      list._notifyLater();
    }
    _recent.clear();
    _newest = null;
  }

  /// Node ids are no longer valid (a new catalog epoch, or no catalog at
  /// all): every list is closed and forgotten.
  void reset() {
    final lists = List.of(_lists.values);
    _lists.clear();
    _recent.clear();
    _newest = null;
    for (final list in lists) {
      list._close();
    }
  }

  void _touch(PagedRows list, int page) {
    final key = (list, page);
    if (_newest == key) return;
    if (_recent.containsKey(key)) {
      _recent.remove(key);
      _recent[key] = null;
      _newest = key;
    }
  }

  void _loaded(PagedRows list, int page) {
    final key = (list, page);
    _recent.remove(key);
    _recent[key] = null;
    _newest = key;
    while (_recent.length > maxCachedPages) {
      final oldest = _recent.keys.first;
      _recent.remove(oldest);
      final (victim, victimPage) = oldest;
      victim._evict(victimPage);
    }
  }

  void _forget(PagedRows list, int page) {
    final key = (list, page);
    _recent.remove(key);
    if (_newest == key) _newest = null;
  }
}
