// The flattened, lazily loaded folder tree of the Results screen, against
// the fake engine: what each line is, expanding and collapsing, paging of
// large folders, and what happens when the listings change underneath.
import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';
import 'package:hdsavior/ui/results/folder_tree.dart';

import '../support/fake_app.dart';

void main() {
  late FakeApp fake;
  late FolderTree tree;

  AppController app() => fake.app;

  /// Asks for every line (as a list widget would) and lets pages arrive.
  Future<List<TreeEntry>> lines() async {
    for (var round = 0; round < 10; round++) {
      final entries = [for (var i = 0; i < tree.length; i++) tree.entryAt(i)];
      if (!entries.any((e) => e.loading)) return entries;
      await fake.settle();
    }
    fail('the tree did not finish loading');
  }

  setUp(() async {
    fake = FakeApp();
    await fake.start();
    await fake.scan();
    tree = FolderTree(app().catalog, app().browse);
  });

  tearDown(() async {
    tree.dispose();
    await fake.dispose();
  });

  test('the file systems are the top level, the main one first and expanded', () async {
    final entries = await lines();
    final instances = app().catalog.info!.instances;
    final roots = entries.where((e) => e.isRoot).toList();
    expect([for (final r in roots) r.instance!.root], [for (final i in instances) i.root]);
    expect(entries.first.isRoot, isTrue);
    expect(entries.first.expanded, isTrue);
    expect(roots.skip(1).every((r) => !r.expanded), isTrue);
    // The lines under the main root are its child folders, in order.
    final mainRoot = instances.first.root;
    final children = entries.skip(1).takeWhile((e) => !e.isRoot).toList();
    final listing = await fake.run(
      () => app().api.listChildren(epoch: app().catalog.epoch!, node: mainRoot, dirsOnly: true, limit: 1000),
    );
    expect([for (final c in children) c.id], [for (final r in listing.rows) r.id]);
    expect(children.every((c) => c.depth == 1 && listEquals(c.ancestors, [mainRoot])), isTrue);
  });

  test('expanding a folder inserts its child folders below it; collapsing removes them', () async {
    var entries = await lines();
    final documentsIndex = entries.indexWhere((e) => e.id == fake.landmarks.documents);
    final documents = entries[documentsIndex];
    expect(documents.expandable, isTrue);
    final before = tree.length;
    tree.expand(documents);
    entries = await lines();
    final expanded = entries[documentsIndex];
    expect(expanded.expanded, isTrue);
    final listing = await fake.run(
      () => app().api.listChildren(epoch: app().catalog.epoch!, node: documents.id!, dirsOnly: true),
    );
    expect(tree.length, before + listing.total);
    for (var i = 0; i < listing.total; i++) {
      final child = entries[documentsIndex + 1 + i];
      expect(child.id, listing.rows[i].id);
      expect(child.depth, 2);
      expect(child.ancestors, [...documents.ancestors, documents.id]);
    }
    // The line after them is the next sibling of Documents.
    expect(entries[documentsIndex + 1 + listing.total].depth, 1);
    expect(tree.parentIndex(documentsIndex + 1), documentsIndex);
    expect(tree.parentIndex(documentsIndex), 0);
    expect(tree.parentIndex(0), isNull);

    tree.collapse(entries[documentsIndex]);
    expect(tree.length, before);
  });

  test('nested expansions are counted at every level', () async {
    var entries = await lines();
    final documents = entries.firstWhere((e) => e.id == fake.landmarks.documents);
    tree.expand(documents);
    entries = await lines();
    final child = entries.firstWhere((e) => e.depth == 2 && e.expandable);
    final childIndex = entries.indexOf(child);
    final beforeGrand = tree.length;
    tree.expand(child);
    entries = await lines();
    expect(entries[childIndex].expanded, isTrue);
    expect(entries[childIndex + 1].depth, 3);
    expect(entries[childIndex + 1].ancestors, [...child.ancestors, child.id]);
    expect(tree.length, greaterThan(beforeGrand));
    // Collapsing the outer folder forgets the inner expansion as well.
    tree.collapse(entries.firstWhere((e) => e.id == fake.landmarks.documents));
    tree.expand((await lines()).firstWhere((e) => e.id == fake.landmarks.documents));
    entries = await lines();
    expect(entries.where((e) => e.depth == 3), isEmpty);
  });

  test('a folder with thousands of subfolders is paged, not loaded at once', () async {
    tree.dispose();
    await fake.dispose();
    fake = FakeApp(mainNodes: 100000);
    await fake.start();
    await fake.scan();
    tree = FolderTree(app().catalog, app().browse);
    expect(fake.landmarks.manyFoldersCount, greaterThan(1000));
    final entries = await lines();
    final many = entries.firstWhere((e) => e.id == fake.landmarks.manyFolders);
    fake.engine.requests.clear();
    tree.expand(many);
    await fake.settle();
    final requests = fake.engine.requests.where((r) => r.command == 'list_children').toList();
    expect(requests, hasLength(1), reason: 'only the first page, to learn the size');
    expect(requests.single.args['limit'], 200);
    expect(tree.length, entries.length + fake.landmarks.manyFoldersCount);
    // A line far down is a placeholder until its page arrives.
    final far = entries.indexOf(many) + fake.landmarks.manyFoldersCount; // its last subfolder
    expect(tree.entryAt(far).loading, isTrue);
    await fake.settle();
    expect(tree.entryAt(far).loading, isFalse);
    expect(tree.entryAt(far).depth, 2);
  });

  test('a new epoch starts over with the main file system expanded', () async {
    final entries = await lines();
    tree.expand(entries.firstWhere((e) => e.id == fake.landmarks.documents));
    tree.expand(entries.lastWhere((e) => e.isRoot));
    await lines();
    var notified = 0;
    tree.addListener(() => notified++);
    final older = app().catalog.info!.instances.indexWhere((i) => i.kind == InstanceKind.older);
    await fake.run(() => app().catalog.setInstanceKind(instance: older, kind: InstanceOverride.main));
    expect(notified, greaterThan(0));
    final after = await lines();
    expect(after.first.instance!.kind, InstanceKind.main);
    expect(after.first.expanded, isTrue);
    expect(after.where((e) => e.expanded), hasLength(1));
  });

  test('showing deleted folders keeps the open file systems and forgets deeper expansions', () async {
    var entries = await lines();
    tree.expand(entries.firstWhere((e) => e.id == fake.landmarks.documents));
    tree.expand(entries.lastWhere((e) => e.isRoot));
    await lines();
    app().browse.filters = app().browse.filters.copyWith(showDeleted: true);
    entries = await lines();
    expect(entries.where((e) => e.isRoot && e.expanded), hasLength(2));
    expect(entries.where((e) => !e.isRoot && e.expanded), isEmpty);
    // The deleted-only folder is listed now.
    expect(entries.any((e) => e.id == fake.landmarks.deletedOnlyFolder), isTrue);
  });

  test('a listing that changed under an expansion drops that expansion instead of showing wrong folders', () async {
    var entries = await lines();
    tree.expand(entries.firstWhere((e) => e.id == fake.landmarks.documents));
    await lines();
    // Carving keeps the epoch but adds [Carved files] to the main root, so
    // every folder after it moves down by one position.
    await fake.run(() => app().carve.start());
    await fake.finishJob();
    entries = await lines();
    await fake.settle();
    entries = await lines();
    final carved = app().catalog.info!.mainInstance!.virtual.carved;
    expect(carved, isNotNull);
    expect(entries.where((e) => e.id == carved), hasLength(1));
    // No line claims to be the expanded Documents unless it really is.
    for (final entry in entries.where((e) => e.expanded && !e.isRoot)) {
      final children = await fake.run(
        () => app().api.listChildren(epoch: app().catalog.epoch!, node: entry.id!, dirsOnly: true),
      );
      final index = entries.indexOf(entry);
      if (children.total > 0) expect(entries[index + 1].ancestors.last, entry.id);
    }
    // Each folder appears once.
    final ids = [for (final e in entries) e.id];
    expect(ids.toSet(), hasLength(ids.length));
  });

  test('a failed listing is a line with the error, and retry loads it', () async {
    final entries = await lines();
    fake.engine.failNext('list_children', 'io_error', message: 'read failed');
    tree.expand(entries.firstWhere((e) => e.id == fake.landmarks.documents));
    await fake.settle();
    final index = entries.indexWhere((e) => e.id == fake.landmarks.documents) + 1;
    expect(tree.entryAt(index).loading, isTrue);
    expect(tree.entryAt(index).error?.kind, EngineErrorCode.ioError);
    tree.retry();
    await lines();
    expect(tree.entryAt(index).error, isNull);
    expect(tree.entryAt(index).loading, isFalse);
  });
}
