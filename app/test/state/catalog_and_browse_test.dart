// The catalog and its epoch, the row cache against a large fake catalog, and
// navigation and filters.
import 'dart:math';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';

import '../support/fake_app.dart';

void main() {
  late FakeApp fake;
  AppController app() => fake.app;
  CatalogController catalog() => fake.app.catalog;
  int root() => fake.app.catalog.info!.mainInstance!.root;

  tearDown(() => fake.dispose());

  /// Loads the first page of [list].
  Future<PagedRows> loaded(PagedRows list) async {
    list.load();
    await list.idle;
    return list;
  }

  group('epochs', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
      await fake.scan();
    });

    test('a catalog with a new epoch drops the selection and every cached row', () async {
      final first = catalog().info!;
      final list = await loaded(catalog().rows.open(ChildrenQuery(node: root())));
      catalog().selection.setChecked(fake.landmarks.documents, [root()], true);
      expect(catalog().rows.cachedPages, 1);
      final changes = catalog().epochChanges;
      var notified = 0;
      catalog().addListener(() => notified++);

      await app().source.loadSession(r'C:\s\other.hdsc');
      final second = catalog().info!;
      expect(second.epoch, isNot(first.epoch));
      expect(catalog().epochChanges, changes + 1);
      expect(notified, greaterThan(0));
      expect(catalog().selection.isEmpty, isTrue);
      expect(list.isClosed, isTrue);
      expect(list.total, isNull);
      expect(catalog().rows.cachedPages, 0);
      expect(catalog().rows.listCount, 0);
    });

    test('the same epoch again keeps the selection and fetches the rows afresh', () async {
      final list = await loaded(catalog().rows.open(ChildrenQuery(node: root())));
      catalog().selection.setChecked(fake.landmarks.documents, [root()], true);
      final changes = catalog().epochChanges;

      await catalog().refresh();
      expect(catalog().epochChanges, changes);
      expect(catalog().selection.include, [fake.landmarks.documents]);
      expect(list.isClosed, isFalse);
      expect(list.peek(0), isNull);
      expect(list.rowAt(0), isNull);
      await list.idle;
      expect(list.rowAt(0), isNotNull);
    });

    test('stale_catalog on any guarded request triggers a refresh to the new epoch', () async {
      final old = catalog().epoch!;
      final list = await loaded(catalog().rows.open(ChildrenQuery(node: root())));
      catalog().selection.setChecked(root(), const [], true);
      fake.engine.bumpEpoch();

      await expectLater(catalog().nodeInfo(root()), throwsA(engineError(EngineErrorCode.staleCatalog)));
      await pumpEventQueue();
      expect(catalog().epoch, isNot(old));
      expect(catalog().epoch, fake.engine.catalog!.epoch);
      expect(catalog().selection.isEmpty, isTrue);
      expect(list.isClosed, isTrue);
      expect(fake.commands.where((c) => c == 'catalog_info'), hasLength(1));
      // With the new epoch the same question is answered.
      expect((await catalog().nodeInfo(root())).row.id, root());
    });

    test('a stale page request heals the listing: the browse controller gets a fresh list', () async {
      final before = app().browse.rows!;
      fake.engine.bumpEpoch();
      before.rowAt(0);
      await before.idle;
      await pumpEventQueue();
      expect(before.isClosed, isTrue);
      final after = app().browse.rows!;
      expect(identical(after, before), isFalse);
      await loaded(after);
      expect(after.total, greaterThan(0));
      expect(after.error, isNull);
    });

    test('many stale answers cause one refresh', () async {
      fake.engine.bumpEpoch();
      final results = await Future.wait([
        for (var i = 0; i < 6; i++) catalog().nodeInfo(root()).then<Object?>((v) => v, onError: (Object e) => e),
      ]);
      await pumpEventQueue();
      expect(results.whereType<EngineException>().where((e) => e.kind == EngineErrorCode.staleCatalog), isNotEmpty);
      expect(fake.commands.where((c) => c == 'catalog_info'), hasLength(1));
    });

    test('no_catalog clears the catalog; without a catalog nothing is sent', () async {
      fake.engine.failNext('node_info', 'no_catalog');
      await expectLater(catalog().nodeInfo(root()), throwsA(engineError(EngineErrorCode.noCatalog)));
      expect(catalog().info, isNull);
      expect(catalog().epoch, isNull);
      expect(catalog().sourceAttached, isFalse);

      final sent = fake.commands.length;
      await expectLater(catalog().nodeInfo(0), throwsA(engineError(EngineErrorCode.noCatalog)));
      expect(fake.commands, hasLength(sent), reason: 'without an epoch there is nothing to ask');
    });

    test('refresh when the engine has no catalog clears it; other errors are passed on', () async {
      // Opening a source drops the engine's session and catalog. (After
      // close_source the engine keeps them, browse only.)
      await fake.openDisk();
      expect(catalog().info, isNull);
      await catalog().refresh();
      expect(catalog().info, isNull);
      expect(catalog().isRefreshing, isFalse);

      fake.engine.failNext('catalog_info', 'internal');
      await expectLater(catalog().refresh(), throwsA(engineError(EngineErrorCode.internal)));
      expect(catalog().isRefreshing, isFalse);
    });

    test('other errors leave the catalog alone', () async {
      final epoch = catalog().epoch;
      await expectLater(catalog().nodeInfo(987654321), throwsA(engineError(EngineErrorCode.notFound)));
      expect(catalog().epoch, epoch);
      expect(fake.commands, isNot(contains('catalog_info')));
    });
  });

  group('geometry', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
      await fake.scan();
    });

    test('a new geometry keeps epoch and selection, re-rates the files, and reports progress', () async {
      final epoch = catalog().epoch;
      final main = catalog().info!.mainInstance!;
      expect(main.geometry.validated, isTrue);
      expect(main.geometryCandidates, hasLength(2));
      catalog().selection.setChecked(fake.landmarks.documents, [root()], true);
      final list = await loaded(catalog().rows.open(ChildrenQuery(node: fake.landmarks.flagSamples, showDeleted: true)));
      Rating? ratingOfSignatureOk() {
        for (var i = 0; i < list.total!; i++) {
          final row = list.rowAt(i);
          if (row != null && row.name == 'signature_ok.pdf') return row.rating;
        }
        return null;
      }

      expect(ratingOfSignatureOk(), Rating.good);

      fake.script.taskTicks = 2;
      final setting = catalog().setGeometry(instance: main.id, clusterSize: 512, volumeOffset: 2097152);
      await fake.step();
      expect(app().task.value!.task, TaskKind.geometrySet);
      await fake.step();
      final info = await setting;
      expect(app().task.value, isNull);
      expect(info.epoch, epoch);
      expect(catalog().info!.mainInstance!.geometry.clusterSize, 512);
      expect(catalog().info!.mainInstance!.geometry.clusterSource, ClusterSource.manual);
      expect(catalog().info!.mainInstance!.geometry.validated, isFalse);
      expect(catalog().selection.include, [fake.landmarks.documents]);
      expect(list.isClosed, isFalse);

      expect(list.rowAt(0), isNull, reason: 'the rows are fetched again');
      await list.idle;
      expect(ratingOfSignatureOk(), Rating.uncertain, reason: 'not validated: at most uncertain');
      expect(fake.lastArgs('geometry_set'), {
        'epoch': catalog().epoch,
        'instance': 0,
        'cluster_size': 512,
        'volume_offset': 2097152,
      });
    });

    test('changing the main geometry forgets ticked carved files (they are discarded)', () async {
      await app().carve.start(kinds: ['jpeg']);
      await fake.finishJob();
      final carvedRoot = catalog().info!.mainInstance!.virtual.carved!;
      catalog().selection
        ..setChecked(carvedRoot, [root()], true)
        ..setChecked(fake.landmarks.documents, [root()], true);

      // Another instance: carved files stay, and so does their tick.
      final other = catalog().info!.instances[2];
      await catalog().setGeometry(instance: other.id, clusterSize: 4096, volumeOffset: 512);
      expect(catalog().selection.include, [fake.landmarks.documents, carvedRoot]..sort());

      await catalog().setGeometry(instance: 0, clusterSize: 8192, volumeOffset: 1048576);
      expect(catalog().selection.include, [fake.landmarks.documents]);
      expect(catalog().info!.carved, 0);
      expect(catalog().info!.mainInstance!.virtual.carved, isNull);
      await pumpEventQueue();
      expect(app().recover.summaryError, isNull);
    });

    test('a refused geometry changes nothing', () async {
      catalog().selection.setChecked(root(), const [], true);
      await expectLater(
        catalog().setGeometry(instance: 0, clusterSize: 1000, volumeOffset: 0),
        throwsA(engineError(EngineErrorCode.badRequest)),
      );
      expect(catalog().info!.mainInstance!.geometry.clusterSize, 4096);
      expect(catalog().selection.include, [root()]);
      expect(app().task.value, isNull);
    });
  });

  group('file systems (instance_set)', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
      await fake.scan();
    });

    test('"this is the one I want": a new epoch with the chosen main file system', () async {
      final older = catalog().info!.instances.firstWhere((i) => i.kind == InstanceKind.older);
      final list = await loaded(catalog().rows.open(ChildrenQuery(node: root())));
      catalog().selection.setChecked(fake.landmarks.documents, [root()], true);
      app().browse.open(FolderLocation(fake.landmarks.documents, [root()]));
      final epoch = catalog().epoch;
      final guard = catalog().guardRevision;

      final info = await catalog().setInstanceKind(instance: older.id, kind: InstanceOverride.main);
      expect(fake.lastArgs('instance_set'), {'epoch': epoch, 'instance': older.id, 'kind': 'main'});
      expect(info.epoch, isNot(epoch));
      expect(catalog().info, same(info));
      final main = catalog().info!.mainInstance!;
      expect(main.root, older.root);
      expect(main.records, older.records);
      expect(main.manual, isTrue);
      expect(catalog().info!.instances.firstWhere((i) => i.root == 0).kind, InstanceKind.other);
      expect(catalog().selection.isEmpty, isTrue, reason: 'the ids of the old epoch are void');
      expect(list.isClosed, isTrue);
      expect(app().browse.folder, FolderLocation(older.root), reason: 'back to the root of the new main file system');
      expect(catalog().guardRevision, greaterThan(guard));

      // The ids of the new catalog count now.
      final epoch2 = catalog().epoch;
      await catalog().setInstanceKind(instance: main.id, kind: InstanceOverride.auto);
      expect(fake.lastArgs('instance_set'), {'epoch': epoch2, 'instance': main.id, 'kind': 'auto'});
      expect(catalog().info!.mainInstance!.root, 0);
      expect(catalog().info!.mainInstance!.manual, isFalse);
      expect(app().browse.folder, FolderLocation(catalog().info!.mainInstance!.root));
    });

    test('"treat as a separate volume" turns an embedded file system into one whose files can be recovered', () async {
      final embedded = catalog().info!.instances.firstWhere((i) => i.kind == InstanceKind.embedded);
      final before = await app().api.selectionSummary(epoch: catalog().epoch!, include: [embedded.root], exclude: []);
      expect(before.unsupported, before.files, reason: 'browse only');

      await catalog().setInstanceKind(instance: embedded.id, kind: InstanceOverride.other);
      final now = catalog().info!.instances.firstWhere((i) => i.root == embedded.root);
      expect(now.kind, InstanceKind.other);
      expect(now.manual, isTrue);
      expect(now.embeddedIn, isNull);
      catalog().selection.setChecked(now.root, const [], true);
      await pumpEventQueue();
      final after = app().recover.summary!;
      expect(after.files, before.files);
      // Only what is unsupported for reasons of its own (compressed, ...) stays so.
      expect(after.unsupported, lessThan(before.files ~/ 10));
      expect(after.bytes, greaterThan(before.bytes), reason: 'their content will be written now');
    });

    test('a refused change leaves everything as it was', () async {
      final strays = catalog().info!.instances.firstWhere((i) => i.kind == InstanceKind.strays);
      catalog().selection.setChecked(root(), const [], true);
      final epoch = catalog().epoch;
      final guard = catalog().guardRevision;
      await expectLater(
        catalog().setInstanceKind(instance: strays.id, kind: InstanceOverride.other),
        throwsA(engineError(EngineErrorCode.badRequest)),
      );
      expect(catalog().epoch, epoch);
      expect(catalog().selection.include, [root()]);
      expect(catalog().guardRevision, guard);
    });
  });

  group('the guard revision', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
      await fake.scan();
    });

    test('changes with the catalog, its epoch, its protected ranges and a geometry; not with a mere refresh', () async {
      final start = catalog().guardRevision;
      expect(catalog().protectedRanges, catalog().info!.protected);
      expect(catalog().protectedRanges.first.reason, ProtectionReason.sessionRange);

      await catalog().refresh();
      expect(catalog().guardRevision, start, reason: 'same epoch, same protected ranges');
      await app().carve.start(kinds: ['png']);
      await fake.finishJob();
      expect(catalog().guardRevision, start, reason: 'carving keeps the epoch and the ranges');

      // Selecting a candidate changes no range, but the files lie elsewhere now.
      final other = catalog().info!.instances.firstWhere((i) => i.kind == InstanceKind.other);
      await catalog().setGeometry(
        instance: other.id,
        clusterSize: other.geometry.clusterSize,
        volumeOffset: other.geometry.volumeOffset,
      );
      expect(catalog().guardRevision, start + 1);

      // A geometry that is not validated drops a protected lost volume.
      final ranges = catalog().protectedRanges.length;
      await catalog().setGeometry(instance: 0, clusterSize: 8192, volumeOffset: 1048576);
      expect(catalog().protectedRanges, hasLength(ranges - 1));
      expect(catalog().guardRevision, start + 2);

      // Other protected ranges in the same epoch, learnt from a refresh.
      await app().api.geometrySet(epoch: catalog().epoch!, instance: 0, clusterSize: 4096, volumeOffset: 1048576);
      expect(catalog().guardRevision, start + 2, reason: 'the controller has not seen it yet');
      await catalog().refresh();
      expect(catalog().protectedRanges, hasLength(ranges));
      expect(catalog().guardRevision, start + 3);

      fake.engine.bumpEpoch();
      await catalog().refresh();
      expect(catalog().guardRevision, start + 4);

      await app().source.close();
      expect(catalog().protectedRanges, isEmpty);
      expect(catalog().guardRevision, start + 5);
    });
  });

  group('details and preview', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
      await fake.scan();
    });

    test('node_info gives the ancestor ids the selection model needs', () async {
      final list = await loaded(catalog().rows.open(SearchQuery(under: fake.landmarks.documents)));
      final row = list.rowAt(0)!;
      final info = await catalog().nodeInfo(row.id);
      expect(info.row.name, row.name);
      expect(info.ancestorIds, row.ancestorIds);
      expect(info.path, row.path);
      catalog().selection.setChecked(row.id, info.ancestorIds, true);
      expect(catalog().selection.stateOf(fake.landmarks.documents, [root()]), CheckState.partial);
    });

    test('readContent returns real pictures, whole or cut off', () async {
      final jpeg = await catalog().readContent(fake.landmarks.sampleJpeg, maxBytes: 1 << 20);
      expect(jpeg.complete, isTrue);
      expect(jpeg.kind, 'jpeg');
      expect(jpeg.bytes.sublist(0, 2), [0xFF, 0xD8]);
      expect(jpeg.bytes, hasLength(jpeg.size));

      final head = await catalog().readContent(fake.landmarks.sampleBinary, maxBytes: 600000);
      expect(head.complete, isFalse);
      expect(head.bytes, hasLength(600000));
      expect(head.size, 20 * 1024 * 1024);
      expect(fake.commands.where((c) => c == 'read_file'), hasLength(1 + 3), reason: '256 KiB chunks');

      await expectLater(
        catalog().readContent(fake.landmarks.sampleCompressed, maxBytes: 100),
        throwsA(engineError(EngineErrorCode.unsupported)),
      );
      final cancel = CancelToken()..cancel();
      await expectLater(
        catalog().readContent(fake.landmarks.sampleJpeg, maxBytes: 100, cancel: cancel),
        throwsA(isA<RequestCancelledException>()),
      );
    });
  });

  group('a catalog of a few hundred thousand nodes', () {
    setUp(() async {
      fake = FakeApp(mainNodes: 300000, maxCachedPages: 16);
      await fake.start();
      await fake.scan();
    });

    test('rows anywhere in a folder of 50,000 files are served page by page within the memory budget', () async {
      expect(catalog().info!.nodes, 300440);
      final list = catalog().rows.open(ChildrenQuery(node: fake.landmarks.hugeFolder, sort: SortKey.size, desc: true));
      await loaded(list);
      final total = list.total!;
      expect(total, greaterThan(45000));

      // Ground truth: the same listing asked for directly, in big steps.
      final truth = <int, NodeRow>{};
      final random = Random(3);
      final indexes = [0, 199, 200, total - 1, for (var i = 0; i < 40; i++) random.nextInt(total)];
      for (final index in indexes) {
        final page = await app().api.listChildren(
          epoch: catalog().epoch!,
          node: fake.landmarks.hugeFolder,
          sort: SortKey.size,
          desc: true,
          offset: index,
          limit: 1,
        );
        truth[index] = page.rows.single;
      }

      final watch = Stopwatch()..start();
      for (final index in indexes) {
        if (list.rowAt(index) == null) await list.idle;
        final row = list.rowAt(index)!;
        expect(row.id, truth[index]!.id, reason: 'row $index');
        expect(row.size, truth[index]!.size);
        expect(catalog().rows.cachedPages, lessThanOrEqualTo(16));
      }
      expect(watch.elapsed, lessThan(const Duration(seconds: 20)));
      // The first page went out of memory long ago; it comes back on demand.
      expect(list.loadedPages, 16);
      expect(list.peek(0), isNull);
      expect(list.rowAt(0), isNull);
      await list.idle;
      // Sorted by size, largest first.
      expect(list.rowAt(0)!.size, greaterThanOrEqualTo(list.rowAt(199)!.size));
      final requests = fake.engine.requests.where((r) => r.command == 'list_children' && r.args['limit'] == 200);
      expect(requests.every((r) => (r.args['offset']! as int) % 200 == 0), isTrue, reason: 'pages of the fixed size');
    });

    test('ticking the root is one entry; the engine counts the quarter of a million files', () async {
      app().selection.setChecked(root(), const [], true);
      expect(app().selection.include, [root()]);
      await pumpEventQueue();
      final summary = app().recover.summary!;
      expect(summary.files, greaterThan(200000));
      expect(fake.lastArgs('selection_summary')['include'], [root()]);
      // Every file below is selected without the app ever having seen it.
      final deep = (await loaded(catalog().rows.open(const SearchQuery(filter: FileFilter(query: 'img_'))))).rowAt(0)!;
      expect(app().selection.isSelected(deep.id, deep.ancestorIds!), isTrue);
      expect(app().selection.stateOf(deep.parent!, deep.ancestorIds!.sublist(0, deep.ancestorIds!.length - 1)), CheckState.checked);
    });

    test('search across the whole tree is paged like a folder', () async {
      app().browse.filters = const ViewFilters(query: 'IMG_', exts: ['jpg']);
      final list = await loaded(app().browse.rows!);
      expect(list.query, isA<SearchQuery>());
      expect(list.total, greaterThan(10000));
      final row = list.rowAt(150)!;
      expect(row.name.toLowerCase(), contains('img_'));
      expect(row.ext, 'jpg');
      expect(row.ancestorIds!.first, root());
      expect(list.rowAt(list.total! - 1), isNull);
      await list.idle;
      expect(list.rowAt(list.total! - 1), isNotNull);
      expect(list.loadedPages, 2);
    });
  });

  group('browse', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
    });

    test('nothing to show without a catalog; the main root when one arrives', () async {
      final browse = app().browse;
      expect(browse.folder, isNull);
      expect(browse.rows, isNull);
      expect(browse.query, isNull);
      expect(browse.folders(0), isNull);
      var notified = 0;
      browse.addListener(() => notified++);
      await fake.scan();
      expect(notified, greaterThan(0));
      expect(browse.folder, FolderLocation(root()));
      final rows = await loaded(browse.rows!);
      expect(rows.query, ChildrenQuery(node: root()));
      expect(identical(browse.rows, rows), isTrue, reason: 'the same list until something changes');
      final names = [for (var i = 0; i < rows.total!; i++) rows.rowAt(i)!.name];
      expect(names, containsAll(['[Lost folders]', '[Orphaned files]', 'Documents', 'Flag samples']));
      expect(names, isNot(contains('Deleted folder')));
    });

    test('opening folders, with the ancestors the selection model needs', () async {
      await fake.scan();
      final browse = app().browse;
      final documents = browse.folder!.child(fake.landmarks.documents);
      expect(documents, FolderLocation(fake.landmarks.documents, [root()]));
      expect(documents.childAncestors, [root(), fake.landmarks.documents]);
      var notified = 0;
      browse.addListener(() => notified++);
      browse.open(documents);
      expect(notified, 1);
      browse.open(FolderLocation(fake.landmarks.documents, [root()]));
      expect(notified, 1, reason: 'the same location again changes nothing');

      final rows = await loaded(browse.rows!);
      final file = [for (var i = 0; i < rows.total!; i++) rows.rowAt(i)!].firstWhere((r) => r.isFile);
      expect(browse.ancestorsOf(file), [root(), fake.landmarks.documents]);
      app().selection.toggle(file.id, browse.ancestorsOf(file));
      expect(app().selection.stateOf(fake.landmarks.documents, [root()]), CheckState.partial);
      expect(app().selection.stateOf(root(), const []), CheckState.partial);

      // Another file system found by the scan.
      final older = catalog().info!.instances[1];
      browse.openRoot(older);
      expect(browse.folder, FolderLocation(older.root));
      browse.openRoot();
      expect(browse.folder, FolderLocation(root()));
    });

    test('filters and sort order decide what is asked of the engine', () async {
      await fake.scan();
      final browse = app().browse;
      browse.open(FolderLocation(fake.landmarks.documents, [root()]));

      browse.filters = const ViewFilters(showDeleted: true, exts: ['jpg', 'png']);
      browse.sortBy(SortKey.size);
      expect(browse.isSearch, isFalse);
      expect(
        browse.query,
        ChildrenQuery(node: fake.landmarks.documents, exts: const ['jpg', 'png'], showDeleted: true, sort: SortKey.size),
      );
      browse.sortBy(SortKey.size);
      expect(browse.descending, isTrue, reason: 'the same column again reverses');
      browse.sortBy(SortKey.modified);
      expect(browse.descending, isFalse, reason: 'a new column starts ascending');
      browse.sortBy(SortKey.modified, descending: true);
      expect((browse.sort, browse.descending), (SortKey.modified, true));

      // Hiding damaged files is a rating filter.
      browse.filters = browse.filters.withDamagedHidden(true);
      expect(browse.filters.hidesDamaged, isTrue);
      expect((browse.query! as ChildrenQuery).ratings, [Rating.good, Rating.uncertain, Rating.unsupported]);
      browse.filters = browse.filters.withDamagedHidden(false);
      expect(browse.filters.ratings, isNull);

      // A name or size filter needs `search`, below the current folder.
      browse.filters = browse.filters.copyWith(query: '  report ', minSize: () => 1000);
      expect(browse.isSearch, isTrue);
      expect(
        browse.query,
        SearchQuery(
          under: fake.landmarks.documents,
          filter: const FileFilter(query: 'report', exts: ['jpg', 'png'], minSize: 1000),
          showDeleted: true,
          sort: SortKey.modified,
          desc: true,
        ),
      );
      final rows = await loaded(browse.rows!);
      expect(rows.error, isNull);
      expect(fake.lastArgs('search')['under'], fake.landmarks.documents);
      expect(fake.lastArgs('search')['filter'], {
        'query': 'report',
        'exts': ['jpg', 'png'],
        'min_size': 1000,
      });

      browse.filters = browse.filters.copyWith(query: '', minSize: () => null, exts: () => null);
      expect(browse.isSearch, isFalse);
      expect(browse.query, isA<ChildrenQuery>());
    });

    test('an empty list of types or ratings means "any", as null does', () async {
      await fake.scan();
      final browse = app().browse;
      browse.filters = const ViewFilters(exts: [], ratings: []);
      expect(browse.query, ChildrenQuery(node: root()));
      expect(browse.filters.hidesDamaged, isFalse);
      final rows = await loaded(browse.rows!);
      expect(rows.error, isNull);
      expect(rows.total, greaterThan(0));
      browse.filters = const ViewFilters(exts: [], ratings: [], query: 'img');
      expect((browse.query! as SearchQuery).filter, const FileFilter(query: 'img'));
      expect((await loaded(browse.rows!)).error, isNull);
    });

    test('attaching another source than the one that was open forgets its partitions', () async {
      await fake.scan();
      expect(app().source.current!.partitions, hasLength(1));
      await app().source.attachSource(kind: SourceKind.image, path: r'D:\copies\disk1.img');
      expect(app().source.current!.info.path, r'D:\copies\disk1.img');
      expect(app().source.current!.partitions, isEmpty);
      expect(app().source.current!.table, isNull);
    });

    test('search rows bring their own ancestors', () async {
      await fake.scan();
      final browse = app().browse..filters = const ViewFilters(query: 'img');
      final rows = await loaded(browse.rows!);
      final row = rows.rowAt(0)!;
      expect(row.ancestorIds, isNotNull);
      expect(browse.ancestorsOf(row), row.ancestorIds);
    });

    test('changing the view replaces the list and cancels what the old one was waiting for', () async {
      await fake.scan();
      final browse = app().browse;
      final first = browse.rows!;
      first.rowAt(0);
      expect(first.isLoading, isTrue);
      var notified = 0;
      browse.addListener(() => notified++);
      browse.filters = const ViewFilters(query: 'a');
      expect(notified, 1);
      expect(first.isLoading, isFalse, reason: 'its request was cancelled');
      final second = browse.rows!;
      expect(identical(second, first), isFalse);
      browse.filters = const ViewFilters(query: 'a');
      expect(notified, 1, reason: 'equal filters change nothing');
      expect(identical(browse.rows, second), isTrue);

      // Typing quickly: only the last search is worth an answer.
      for (final text in ['ab', 'abc', 'abcd', 'img']) {
        browse.filters = ViewFilters(query: text);
        browse.rows!.load();
        await Future<void>.delayed(Duration.zero);
      }
      final last = browse.rows!;
      await last.idle;
      await pumpEventQueue();
      expect(last.total, greaterThan(0));
      final searches = fake.engine.requests.where((r) => r.command == 'search').map((r) => (r.args['filter']! as Map)['query']);
      expect(searches.last, 'img');
      expect(searches.length, lessThan(5), reason: 'at least one obsolete search never reached the engine');
    });

    test('the tree: child folders only, following "show deleted"', () async {
      await fake.scan();
      final browse = app().browse;
      final folders = await loaded(browse.folders(root())!);
      expect(folders.query, ChildrenQuery(node: root(), dirsOnly: true));
      final rows = [for (var i = 0; i < folders.total!; i++) folders.rowAt(i)!];
      expect(rows.every((r) => r.isFolder), isTrue);
      expect(rows.map((r) => r.name), isNot(contains('Deleted folder')));
      expect(rows.firstWhere((r) => r.name == 'Many folders').hasChildren, isTrue);
      expect(rows.firstWhere((r) => r.name == 'Huge folder').hasChildren, isFalse);
      expect(identical(browse.folders(root()), folders), isTrue);

      browse.filters = const ViewFilters(showDeleted: true);
      final withDeleted = await loaded(browse.folders(root())!);
      expect([for (var i = 0; i < withDeleted.total!; i++) withDeleted.rowAt(i)!.name], contains('Deleted folder'));
    });

    test('a new epoch returns to the main root and to fresh lists', () async {
      await fake.scan();
      final browse = app().browse;
      browse.open(FolderLocation(fake.landmarks.documents, [root()]));
      final rows = await loaded(browse.rows!);
      await app().source.loadSession(r'C:\s\x.hdsc');
      expect(rows.isClosed, isTrue);
      expect(browse.folder, FolderLocation(root()));
      expect(browse.rows!.isClosed, isFalse);
      await app().source.close();
      expect(browse.folder, isNull);
      expect(browse.rows, isNull);
    });
  });

  test('ViewFilters and RecoverOptions are values', () {
    const filters = ViewFilters(showDeleted: true, exts: ['jpg'], ratings: [Rating.good], query: 'x', minSize: 1, maxSize: 2);
    expect(filters, ViewFilters(showDeleted: true, exts: ['jpg'].toList(), ratings: [Rating.good].toList(), query: 'x', minSize: 1, maxSize: 2));
    expect(filters.hashCode, filters.copyWith().hashCode);
    expect(filters.copyWith(), filters);
    expect(filters.copyWith(showDeleted: false), isNot(filters));
    expect(filters.copyWith(maxSize: () => null).maxSize, isNull);
    expect(filters.copyWith(maxSize: () => null).minSize, 1);
    expect(const ViewFilters().isSearch, isFalse);
    expect(const ViewFilters(query: '   ').isSearch, isFalse);
    expect(const ViewFilters(maxSize: 5).isSearch, isTrue);
    expect(const ViewFilters().fileFilter, FileFilter.any);

    const options = RecoverOptions(includeDeleted: true, filter: FileFilter(query: 'a'));
    expect(options, const RecoverOptions(includeDeleted: true, filter: FileFilter(query: 'a')));
    expect(options.copyWith(skipDamaged: true).includeDeleted, isTrue);
    expect(options.copyWith(filter: () => null).filter, isNull);
    expect(options.copyWith().hashCode, options.hashCode);
    expect(options.copyWith(onExists: OnExists.overwrite), isNot(options));
  });
}
