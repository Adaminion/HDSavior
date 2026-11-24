// The paged row cache: fixed page size, bounded memory, cancellation of
// obsolete requests, and its behaviour when rows or the epoch change.
import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/row_cache.dart';

NodeRow row(int id) => NodeRow(
  id: id,
  parent: 0,
  name: 'file $id',
  kind: NodeKind.file,
  ext: 'txt',
  size: id,
  deleted: false,
);

/// One request the cache has made.
final class Call {
  Call(this.query, this.offset, this.limit, this.cancel);
  final RowQuery query;
  final int offset;
  final int limit;
  final CancelToken cancel;
  final Completer<RowsPage> _completer = Completer();

  int get page => offset ~/ limit;
  bool get isDone => _completer.isCompleted;

  /// Answers with rows whose ids are their index in a listing of [total].
  void answer(int total, {int base = 0}) => _completer.complete(
    RowsPage(
      total: total,
      rows: [for (var i = offset; i < offset + limit && i < total; i++) row(base + i)],
    ),
  );

  void fail(Object error) => _completer.completeError(error);
}

/// A fetcher under the test's control: every request waits until the test
/// answers it. A request whose token is cancelled fails at once, as it does
/// in the engine client.
final class Fetcher {
  final List<Call> calls = [];

  Future<RowsPage> call(RowQuery query, int offset, int limit, CancelToken cancel) {
    final call = Call(query, offset, limit, cancel);
    calls.add(call);
    cancel.addListener(() {
      if (!call.isDone) call.fail(const RequestCancelledException('list_children'));
    });
    return call._completer.future;
  }

  List<Call> get open => [for (final call in calls) if (!call.isDone) call];

  Call get last => calls.last;
}

const folder = ChildrenQuery(node: 7);

void main() {
  late Fetcher fetcher;
  late RowCache cache;

  RowCache newCache({int pageSize = 10, int maxCachedPages = 4, int maxPendingPerList = 2}) => cache = RowCache(
    fetch: (fetcher = Fetcher()).call,
    pageSize: pageSize,
    maxCachedPages: maxCachedPages,
    maxPendingPerList: maxPendingPerList,
  );

  /// Loads [page] of [list] of a listing with [total] rows.
  Future<void> loadPage(PagedRows list, int page, int total) async {
    expect(list.rowAt(page * list.pageSize), isNull);
    final call = fetcher.open.singleWhere((c) => c.page == page && identical(c.query, list.query));
    call.answer(total);
    await list.idle;
  }

  group('paging', () {
    test('rows are fetched in pages of the fixed size, on demand', () async {
      final list = newCache().open(folder);
      expect(list.total, isNull);
      expect(list.isLoading, isFalse);
      expect(fetcher.calls, isEmpty, reason: 'nothing is fetched before it is asked for');

      expect(list.rowAt(25), isNull);
      expect(fetcher.calls.single.offset, 20);
      expect(fetcher.calls.single.limit, 10);
      expect(fetcher.calls.single.query, folder);
      expect(list.isLoading, isTrue);

      // Asking again while the page is on its way does not ask the engine again.
      expect(list.rowAt(25), isNull);
      expect(list.rowAt(29), isNull);
      expect(fetcher.calls, hasLength(1));

      fetcher.last.answer(95);
      await list.idle;
      expect(list.total, 95);
      expect(list.isLoading, isFalse);
      expect(list.rowAt(25)!.id, 25);
      expect(list.rowAt(20)!.id, 20);
      expect(list.rowAt(29)!.id, 29);
      expect(fetcher.calls, hasLength(1), reason: 'served from memory');
      expect(list.loadedPages, 1);
    });

    test('listeners are notified when a page arrives, and never during rowAt', () async {
      final list = newCache().open(folder);
      var notified = 0;
      list.addListener(() => notified++);
      list.rowAt(0);
      expect(notified, 0, reason: 'rowAt is called while widgets build');
      await pumpEventQueue();
      expect(notified, 1, reason: 'isLoading changed');
      fetcher.last.answer(30);
      await list.idle;
      await pumpEventQueue();
      expect(notified, 2);
    });

    test('the last page is short; rows beyond the total are not requested', () async {
      final list = newCache().open(folder);
      await loadPage(list, 0, 23);
      await loadPage(list, 2, 23);
      expect(list.rowAt(22)!.id, 22);
      expect(list.rowAt(23), isNull);
      expect(list.rowAt(500), isNull);
      expect(fetcher.calls, hasLength(2), reason: 'index 23 and 500 are beyond the listing');
    });

    test('load fetches the first page once, to learn the total', () async {
      final list = newCache().open(folder);
      list.load();
      list.load();
      expect(fetcher.calls, hasLength(1));
      fetcher.last.answer(0);
      await list.idle;
      expect(list.total, 0);
      list.load();
      expect(fetcher.calls, hasLength(1));
      expect(list.rowAt(0), isNull);
      expect(fetcher.calls, hasLength(1));
    });

    test('peek never fetches', () async {
      final list = newCache().open(folder);
      expect(list.peek(3), isNull);
      expect(fetcher.calls, isEmpty);
      await loadPage(list, 0, 30);
      expect(list.peek(3)!.id, 3);
      expect(list.peek(13), isNull);
      expect(fetcher.calls, hasLength(1));
    });

    test('equal queries share one list; different queries do not', () {
      newCache();
      final a = cache.open(const ChildrenQuery(node: 7, exts: ['jpg'], ratings: [Rating.good]));
      final b = cache.open(ChildrenQuery(node: 7, exts: ['jpg'].toList(), ratings: [Rating.good].toList()));
      expect(identical(a, b), isTrue);
      final variants = <RowQuery>[
        const ChildrenQuery(node: 8, exts: ['jpg'], ratings: [Rating.good]),
        const ChildrenQuery(node: 7, exts: ['png'], ratings: [Rating.good]),
        const ChildrenQuery(node: 7, exts: ['jpg'], ratings: [Rating.damaged]),
        const ChildrenQuery(node: 7, exts: ['jpg']),
        const ChildrenQuery(node: 7, exts: ['jpg'], ratings: [Rating.good], dirsOnly: true),
        const ChildrenQuery(node: 7, exts: ['jpg'], ratings: [Rating.good], showDeleted: true),
        const ChildrenQuery(node: 7, exts: ['jpg'], ratings: [Rating.good], sort: SortKey.size),
        const ChildrenQuery(node: 7, exts: ['jpg'], ratings: [Rating.good], desc: true),
        const SearchQuery(under: 7),
        const SearchQuery(under: 7, filter: FileFilter(query: 'a')),
        const SearchQuery(under: 7, filter: FileFilter(query: 'b')),
        const SearchQuery(under: 8, filter: FileFilter(query: 'a')),
        const SearchQuery(filter: FileFilter(query: 'a')),
      ];
      final lists = {a, for (final query in variants) cache.open(query)};
      expect(lists, hasLength(variants.length + 1));
      expect(identical(cache.open(const SearchQuery(under: 7, filter: FileFilter(query: 'a'))), cache.open(variants[9])), isTrue);
    });
  });

  group('bounded memory', () {
    test('at most maxCachedPages pages are kept; the least recently used go first', () async {
      final list = newCache(maxCachedPages: 3).open(folder);
      for (var page = 0; page < 3; page++) {
        await loadPage(list, page, 1000);
      }
      expect(cache.cachedPages, 3);
      // Page 0 is read again, so page 1 is now the oldest.
      expect(list.rowAt(0), isNotNull);
      await loadPage(list, 3, 1000);
      expect(cache.cachedPages, 3);
      expect(list.peek(0), isNotNull);
      expect(list.peek(10), isNull, reason: 'page 1 was evicted');
      expect(list.peek(20), isNotNull);
      expect(list.peek(30), isNotNull);

      // An evicted page is simply fetched again.
      final before = fetcher.calls.length;
      expect(list.rowAt(10), isNull);
      expect(fetcher.calls, hasLength(before + 1));
    });

    test('the budget is shared by all lists', () async {
      newCache(maxCachedPages: 3);
      final a = cache.open(const ChildrenQuery(node: 1));
      final b = cache.open(const ChildrenQuery(node: 2));
      await loadPage(a, 0, 100);
      await loadPage(a, 1, 100);
      await loadPage(b, 0, 100);
      await loadPage(b, 1, 100);
      expect(cache.cachedPages, 3);
      expect(a.loadedPages + b.loadedPages, 3);
      expect(a.peek(0), isNull, reason: 'the oldest page of all lists went');
      expect(a.peek(10), isNotNull);
    });

    test('scrolling through a huge listing never holds more than the budget', () async {
      final list = newCache(pageSize: 200, maxCachedPages: 8, maxPendingPerList: 4).open(folder);
      const total = 1000000;
      for (var index = 0; index < total; index += 4321) {
        list.rowAt(index);
        for (final call in fetcher.open) {
          call.answer(total);
        }
        await list.idle;
        expect(cache.cachedPages, lessThanOrEqualTo(8));
        expect(list.loadedPages, lessThanOrEqualTo(8));
        expect(list.rowAt(index)!.id, index);
      }
    });

    test('lists that nobody uses are forgotten when there are too many', () {
      cache = RowCache(fetch: (fetcher = Fetcher()).call, pageSize: 10, maxIdleLists: 5);
      final kept = cache.open(const ChildrenQuery(node: 0));
      kept.addListener(() {});
      for (var node = 1; node < 50; node++) {
        cache.open(ChildrenQuery(node: node));
      }
      expect(cache.listCount, lessThanOrEqualTo(6));
      expect(identical(cache.open(const ChildrenQuery(node: 0)), kept), isTrue, reason: 'a list with listeners stays');
    });
  });

  group('obsolete requests', () {
    test('requests that scrolling has left behind are cancelled', () async {
      final list = newCache(maxPendingPerList: 2).open(folder);
      // Each "frame" shows a new page; the engine has answered none so far.
      for (var page = 0; page < 6; page++) {
        list.rowAt(page * 10);
        await pumpEventQueue(times: 2);
      }
      expect(fetcher.calls, hasLength(6));
      expect(fetcher.calls.map((c) => c.cancel.isCancelled), [true, true, true, true, false, false]);
      expect(fetcher.open.map((c) => c.page), [4, 5]);

      for (final call in fetcher.open) {
        call.answer(1000);
      }
      await list.idle;
      expect(list.peek(40), isNotNull);
      expect(list.peek(50), isNotNull);
      expect(list.peek(0), isNull);
    });

    test('pages that are on screen together are never cancelled for each other', () async {
      final list = newCache(maxPendingPerList: 2).open(folder);
      var notified = 0;
      list.addListener(() => notified++);
      // One build shows rows of three pages, but only two requests may run.
      void build() {
        list.rowAt(5);
        list.rowAt(15);
        list.rowAt(25);
      }

      build();
      expect(fetcher.open.map((c) => c.page), [0, 1]);
      expect(fetcher.calls.every((c) => !c.cancel.isCancelled), isTrue);

      // Rebuilding (as the notification makes the widget do) changes nothing.
      for (var i = 0; i < 5; i++) {
        await pumpEventQueue(times: 2);
        build();
      }
      expect(fetcher.calls, hasLength(2), reason: 'no request is cancelled and sent again in a loop');

      // When a page arrives, the next build asks for the one that had to wait.
      fetcher.calls[0].answer(1000);
      await pumpEventQueue();
      build();
      expect(fetcher.open.map((c) => c.page), [1, 2]);
      for (final call in fetcher.open) {
        call.answer(1000);
      }
      await list.idle;
      build();
      expect([list.rowAt(5)!.id, list.rowAt(15)!.id, list.rowAt(25)!.id], [5, 15, 25]);
      expect(fetcher.calls, hasLength(3));
      expect(notified, lessThan(10));
    });

    test('cancelPending drops what is on its way; the rows can be asked for again', () async {
      final list = newCache().open(folder);
      list.rowAt(0);
      list.rowAt(10);
      list.cancelPending();
      expect(fetcher.calls.every((c) => c.cancel.isCancelled), isTrue);
      await list.idle;
      expect(list.isLoading, isFalse);
      expect(list.error, isNull, reason: 'a cancelled request is not an error');

      expect(list.rowAt(0), isNull);
      expect(fetcher.calls, hasLength(3));
      fetcher.last.answer(50);
      await list.idle;
      expect(list.rowAt(0)!.id, 0);
    });

    test('a late answer to a cancelled request is ignored', () async {
      // A fetcher that answers even though it was cancelled.
      final completers = <Completer<RowsPage>>[];
      cache = RowCache(
        fetch: (query, offset, limit, cancel) {
          final completer = Completer<RowsPage>();
          completers.add(completer);
          return completer.future;
        },
        pageSize: 10,
      );
      final list = cache.open(folder);
      list.rowAt(0);
      list.refresh();
      list.rowAt(0);
      completers[0].complete(RowsPage(total: 5, rows: [row(900)]));
      await pumpEventQueue();
      expect(list.total, isNull);
      expect(list.peek(0), isNull);
      completers[1].complete(RowsPage(total: 7, rows: [row(1)]));
      await list.idle;
      expect(list.total, 7);
      expect(list.peek(0)!.id, 1);
    });
  });

  group('errors', () {
    test('a failed page is reported and not requested again until retry', () async {
      final list = newCache().open(folder);
      list.rowAt(0);
      fetcher.last.fail(EngineException(const EngineError(code: 'io_error', message: 'read failed')));
      await list.idle;
      expect(list.error!.kind, EngineErrorCode.ioError);
      expect(list.isLoading, isFalse);

      for (var i = 0; i < 5; i++) {
        expect(list.rowAt(0), isNull);
        await pumpEventQueue(times: 2);
      }
      expect(fetcher.calls, hasLength(1), reason: 'a list that rebuilds must not hammer the engine');

      list.retry();
      expect(list.error, isNull);
      expect(list.rowAt(0), isNull);
      expect(fetcher.calls, hasLength(2));
      fetcher.last.answer(3);
      await list.idle;
      expect(list.rowAt(0)!.id, 0);
    });

    test('other pages still load after a failure, and a success clears the error', () async {
      final list = newCache().open(folder);
      list.rowAt(0);
      fetcher.last.fail(EngineException(const EngineError(code: 'internal')));
      await list.idle;
      list.rowAt(10);
      fetcher.last.answer(100);
      await list.idle;
      expect(list.error, isNull);
      expect(list.rowAt(10)!.id, 10);
    });

    test('a defect in the fetcher does not leave the list loading for ever', () async {
      cache = RowCache(
        fetch: (query, offset, limit, cancel) => Future.error(ArgumentError('bad query')),
        pageSize: 10,
      );
      final list = cache.open(folder);
      await runZonedGuarded(
        () async {
          list.rowAt(0);
          await pumpEventQueue();
        },
        (error, stack) => expect(error, isArgumentError),
      );
      expect(list.isLoading, isFalse);
      list.rowAt(0);
      await pumpEventQueue();
      expect(list.isLoading, isFalse, reason: 'not requested again');
    });
  });

  group('when rows change', () {
    test('refresh drops the rows but keeps the total until the new answer', () async {
      final list = newCache().open(folder);
      await loadPage(list, 0, 30);
      list.refresh();
      expect(list.total, 30, reason: 'so the list does not jump');
      expect(list.peek(0), isNull);
      expect(list.rowAt(0), isNull);
      fetcher.last.answer(28, base: 1000);
      await list.idle;
      expect(list.total, 28);
      expect(list.rowAt(0)!.id, 1000);
    });

    test('invalidate (same epoch): every list fetches again; failures are forgotten', () async {
      newCache();
      final a = cache.open(const ChildrenQuery(node: 1));
      final b = cache.open(const ChildrenQuery(node: 2));
      await loadPage(a, 0, 30);
      b.rowAt(0);
      fetcher.last.fail(EngineException(const EngineError(code: 'internal')));
      await b.idle;
      a.rowAt(10); // on its way when the rows change
      final onItsWay = fetcher.last;
      var notified = 0;
      a.addListener(() => notified++);

      cache.invalidate();
      expect(onItsWay.cancel.isCancelled, isTrue);
      expect(cache.cachedPages, 0);
      expect(a.isClosed, isFalse);
      expect(a.total, 30);
      expect(a.peek(0), isNull);
      expect(b.error, isNull);
      await pumpEventQueue();
      expect(notified, greaterThan(0), reason: 'so that the widgets rebuild and ask again');

      expect(a.rowAt(0), isNull);
      fetcher.last.answer(31, base: 500);
      await a.idle;
      expect(a.rowAt(0)!.id, 500);
      expect(a.total, 31);
      expect(b.rowAt(0), isNull);
      expect(fetcher.open, hasLength(1), reason: 'the failed page is requested again');
    });

    test('a listing that became shorter loses the pages beyond its end', () async {
      final list = newCache().open(folder);
      await loadPage(list, 0, 100);
      await loadPage(list, 5, 100);
      list.rowAt(10);
      fetcher.last.answer(15);
      await list.idle;
      expect(list.total, 15);
      expect(list.peek(50), isNull);
      expect(list.loadedPages, 2);
      expect(cache.cachedPages, 2);
    });

    test('reset (new epoch): lists are closed, forgotten and fetch nothing any more', () async {
      newCache();
      final list = cache.open(folder);
      await loadPage(list, 0, 30);
      list.rowAt(10);
      final onItsWay = fetcher.last;
      var notified = 0;
      list.addListener(() => notified++);

      cache.reset();
      expect(notified, 1);
      expect(list.isClosed, isTrue);
      expect(list.total, isNull);
      expect(list.isLoading, isFalse);
      expect(onItsWay.cancel.isCancelled, isTrue);
      expect(cache.cachedPages, 0);
      expect(cache.listCount, 0);

      final calls = fetcher.calls.length;
      expect(list.rowAt(0), isNull);
      list.load();
      list.refresh();
      expect(fetcher.calls, hasLength(calls), reason: 'ids of the old epoch are never sent again');
      expect(identical(cache.open(folder), list), isFalse, reason: 'the same query gets a new list');
    });
  });
}
