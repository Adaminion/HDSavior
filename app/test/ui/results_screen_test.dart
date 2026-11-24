// Screen 3, Results: the tree of file systems, the paged table, tri-state
// ticks, sorting, filters, the debounced search, the details pane with its
// preview, the disk layout, the user's say about file systems, carving,
// the notices, keyboard navigation, and the million-file paging path.
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';
import 'package:hdsavior/ui/format.dart';
import 'package:hdsavior/ui/results/details_pane.dart';
import 'package:hdsavior/ui/results/file_table.dart';
import 'package:hdsavior/ui/results/file_types.dart';
import 'package:hdsavior/ui/results/tree_view.dart';
import 'package:hdsavior/ui/screens/results_screen.dart';
import 'package:hdsavior/ui/strings.dart';
import 'package:material_ui/material_ui.dart';

import 'ui_harness.dart';

Finder inTree(Finder what) => find.descendant(of: find.byType(FolderTreeView), matching: what);
Finder inTable(Finder what) => find.descendant(of: find.byType(FileTable), matching: what);
Finder inDetails(Finder what) => find.descendant(of: find.byType(DetailsPane), matching: what);

/// The check box on the line of [name] in [scope].
Checkbox checkboxOf(WidgetTester tester, Finder scope, String name) {
  final text = find.descendant(of: scope, matching: find.text(name));
  final line = find.ancestor(of: text, matching: find.byType(InkWell)).first;
  return tester.widget<Checkbox>(find.descendant(of: line, matching: find.byType(Checkbox)));
}

void main() {
  Future<FakeApp> results(
    WidgetTester tester, {
    int mainNodes = 3000,
    Size size = wideWindow,
    bool carveHits = true,
  }) async {
    final fake = await pumpApp(tester, mainNodes: mainNodes, size: size);
    await fake.openDisk();
    final partition = fake.app.source.current!.partitions.single;
    await fake.run(() => fake.app.scan.start(range: partition.range, collectCarveHits: carveHits));
    await fake.finishJob();
    await settle(tester);
    expect(fake.app.screen.value, AppScreen.results);
    await pumpUntil(tester, inTable(find.text('Documents')));
    return fake;
  }

  int rootOf(FakeApp fake) => fake.app.catalog.info!.mainInstance!.root;

  testWidgets('the tree: every file system labelled, the main one first and open', (tester) async {
    final fake = await results(tester);
    final instances = fake.app.catalog.info!.instances;
    expect(instances.map((i) => i.kind), [
      InstanceKind.main,
      InstanceKind.older,
      InstanceKind.other,
      InstanceKind.embedded,
      InstanceKind.strays,
    ]);
    // Main is open: its folders are listed below it.
    expect(inTree(find.text('Documents')), findsOneWidget);
    final mainY = tester.getTopLeft(inTree(find.text(S.instanceName(instances[0])))).dy;
    final documentsY = tester.getTopLeft(inTree(find.text('Documents'))).dy;
    expect(mainY, lessThan(documentsY));
    // Closed, it leaves room for the other file systems, in order.
    await tester.tap(inTree(find.byIcon(Icons.expand_more)));
    await settle(tester);
    expect(inTree(find.text('Documents')), findsNothing);
    var previous = -1.0;
    for (final instance in instances) {
      final line = inTree(find.text(S.instanceName(instance)));
      expect(line, findsOneWidget, reason: instance.kind.wire);
      final y = tester.getTopLeft(line).dy;
      expect(y, greaterThan(previous));
      previous = y;
    }
    expect(S.instanceName(instances[2]), contains('another volume'));
    expect(S.instanceName(instances[3]), contains('browse only'));

    // Opening the older one lists its folders (dirs only), lazily.
    fake.engine.requests.clear();
    final older = inTree(find.text(S.instanceName(instances[1])));
    await tester.tap(
      find.descendant(
        of: find.ancestor(of: older, matching: find.byType(InkWell)).first,
        matching: find.byIcon(Icons.chevron_right),
      ),
    );
    await settle(tester);
    final listing = fake.engine.requests.firstWhere((r) => r.command == 'list_children');
    expect(listing.args['node'], instances[1].root);
    expect(listing.args['dirs_only'], isTrue);
    await closeApp(tester, fake);
  });

  testWidgets('a tick on a folder shows in the tree as partial above it, and in the selection bar', (tester) async {
    final fake = await results(tester);
    expect(find.text(S.selectionEmpty), findsOneWidget);
    final recover = find.widgetWithText(FilledButton, S.recoverAction);
    expect(tester.widget<FilledButton>(recover).onPressed, isNull);

    await tester.tap(
      find.descendant(
        of: find.ancestor(of: inTable(find.text('Documents')), matching: find.byType(InkWell)).first,
        matching: find.byType(Checkbox),
      ),
    );
    await settle(tester);
    expect(fake.app.selection.include, [fake.landmarks.documents]);
    expect(checkboxOf(tester, find.byType(FileTable), 'Documents').value, isTrue);
    final mainName = S.instanceName(fake.app.catalog.info!.mainInstance!);
    expect(checkboxOf(tester, find.byType(FolderTreeView), mainName).value, isNull, reason: 'partial');
    expect(checkboxOf(tester, find.byType(FolderTreeView), 'Documents').value, isTrue);

    final summary = fake.app.recover.summary!;
    expect(
      find.text(S.selectionSummary(en.count(summary.files), summary.files, en.bytes(summary.bytes))),
      findsOneWidget,
    );
    expect(tester.widget<FilledButton>(recover).onPressed, isNotNull);
    await tester.tap(recover);
    await tester.pump();
    expect(fake.app.screen.value, AppScreen.recover);
    fake.app.screen.value = AppScreen.results;
    await tester.pump();
    await tester.tap(find.text(S.selectionClear));
    await settle(tester);
    expect(fake.app.selection.isEmpty, isTrue);
    await closeApp(tester, fake);
  });

  testWidgets('sorting by a column; a second click reverses it', (tester) async {
    final fake = await results(tester);
    await tester.tap(inTable(find.text(S.columnSize)));
    await settle(tester);
    expect(fake.lastArgs('list_children'), containsPair('sort', 'size'));
    expect(fake.lastArgs('list_children'), containsPair('desc', false));
    await tester.tap(inTable(find.text(S.columnSize)));
    await settle(tester);
    expect(fake.lastArgs('list_children'), containsPair('desc', true));
    await tester.tap(inTable(find.text(S.columnStatus)));
    await settle(tester);
    expect(fake.lastArgs('list_children'), containsPair('sort', 'rating'));
    await closeApp(tester, fake);
  });

  testWidgets('a double click opens a folder; the path and "up" lead back', (tester) async {
    final fake = await results(tester);
    final documents = inTable(find.text('Documents'));
    await tester.tap(documents);
    await tester.pump(const Duration(milliseconds: 50));
    await tester.tap(documents);
    await settle(tester);
    expect(fake.app.browse.folder, FolderLocation(fake.landmarks.documents, [rootOf(fake)]));
    await settle(tester);
    // The breadcrumb names the way here; its first part goes back.
    final mainName = S.instanceName(fake.app.catalog.info!.mainInstance!);
    final crumb = find.descendant(of: find.byType(ResultsScreen), matching: find.text(mainName));
    expect(crumb, findsNWidgets(2), reason: 'in the tree and in the path');
    await tester.tap(find.byTooltip(S.upOneLevel));
    await settle(tester);
    expect(fake.app.browse.folder, FolderLocation(rootOf(fake)));
    await closeApp(tester, fake);
  });

  testWidgets('a single click selects at once and shows the details; the flags are explained', (tester) async {
    final fake = await results(tester);
    fake.app.browse.open(FolderLocation(fake.landmarks.flagSamples, [rootOf(fake)]));
    await settle(tester);
    await pumpUntil(tester, inTable(find.text('overwritten.pdf')));
    await tester.tap(inTable(find.text('overwritten.pdf')));
    await tester.pump();
    await settle(tester);
    expect(inDetails(find.text('overwritten.pdf')), findsOneWidget);
    // The engine's explanation of the flag, behind our name for it.
    final details = inDetails(find.byType(RichText));
    expect(
      tester
          .widgetList<RichText>(details)
          .map((t) => t.text.toPlainText())
          .where((t) => t.startsWith('${S.flagName(FileFlag.overwritten, 'overwritten')}: ')),
      isNotEmpty,
    );
    expect(inDetails(find.text(S.rating(Rating.damaged))), findsOneWidget);
    expect(inDetails(find.text(S.ratingExplanation(Rating.damaged))), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('previews: a picture, text, and hex for anything else', (tester) async {
    final fake = await results(tester);
    fake.app.browse.open(FolderLocation(fake.landmarks.previewSamples, [rootOf(fake)]));
    await settle(tester);
    await pumpUntil(tester, inTable(find.text('picture.png')));

    await tester.tap(inTable(find.text('picture.png')));
    await settle(tester);
    await settle(tester);
    expect(fake.lastArgs('read_file'), containsPair('offset', 0));
    expect(inDetails(find.byType(Image)), findsOneWidget);

    await tester.tap(inTable(find.text('notes.txt')));
    await settle(tester);
    await settle(tester);
    expect(inDetails(find.text(S.previewText)), findsOneWidget);

    await tester.tap(inTable(find.text('large archive.zip')));
    await settle(tester);
    await settle(tester);
    expect(inDetails(find.text(S.previewHex)), findsOneWidget);
    expect(fake.lastArgs('read_file')['length'], lessThanOrEqualTo(FilePreview.headBytes));

    await tester.tap(inTable(find.text('compressed.docx')));
    await settle(tester);
    expect(inDetails(find.text(S.previewUnsupported)), findsOneWidget);
    await closeApp(tester, fake);
  });

  test('the preview tells text from binary', () {
    expect(previewText(Uint8List.fromList('Hello\r\n\tworld'.codeUnits)), 'Hello\r\n\tworld');
    expect(previewText(Uint8List.fromList([0x48, 0x00, 0x49])), isNull);
    // UTF-8 cut in the middle of a character at the end of the preview.
    final euro = [0x41, 0xE2, 0x82];
    expect(previewText(Uint8List.fromList(euro)), 'A');
    expect(previewText(Uint8List.fromList([0xFF, 0xFE, 0x41])), isNull);
    expect(previewHexDump(Uint8List.fromList(List.generate(20, (i) => i + 0x41))).split('\n'), [
      '0000  41 42 43 44 45 46 47 48 49 4a 4b 4c 4d 4e 4f 50  ABCDEFGHIJKLMNOP',
      '0010  51 52 53 54                                      QRST',
    ]);
    expect(previewHexDump(Uint8List.fromList(List.generate(10, (i) => i + 0x41)), perLine: 8).split('\n'), [
      '0000  41 42 43 44 45 46 47 48  ABCDEFGH',
      '0008  49 4a                    IJ',
    ]);
  });

  testWidgets('filters: deleted files, types and condition reach the engine', (tester) async {
    final fake = await results(tester);
    await tester.tap(find.widgetWithText(FilterChip, S.filterDeleted));
    await settle(tester);
    expect(fake.lastArgs('list_children'), containsPair('show_deleted', true));
    expect(fake.app.recover.options.includeDeleted, isTrue, reason: 'seen and ticked deleted files are wanted');
    await tester.tap(find.widgetWithText(FilterChip, S.typeGroup(TypeGroup.pictures)));
    await settle(tester);
    final exts = fake.lastArgs('list_children')['exts']! as List<Object?>;
    expect(exts, containsAll(['jpg', 'png', 'heic', 'cr3']));
    expect(exts, isNot(contains('pdf')));
    await tester.tap(find.widgetWithText(FilterChip, S.typeGroup(TypeGroup.documents)));
    await settle(tester);
    expect(fake.lastArgs('list_children')['exts'], containsAll(['jpg', 'pdf', 'docx']));
    await tester.tap(find.widgetWithText(FilterChip, S.conditionFilter(0)));
    await settle(tester);
    await tester.tap(find.text(S.conditionFilter(1)).last);
    await settle(tester);
    expect(fake.lastArgs('list_children')['ratings'], ['good', 'uncertain', 'unsupported']);
    await closeApp(tester, fake);
  });

  testWidgets('a narrow window offers the types as one menu', (tester) async {
    final fake = await results(tester, size: minimumWindow);
    expect(find.widgetWithText(FilterChip, S.typeGroup(TypeGroup.pictures)), findsNothing);
    await tester.tap(find.widgetWithText(FilterChip, S.typeLabel(const [])));
    await settle(tester);
    await tester.tap(find.widgetWithText(CheckboxMenuButton, S.typeGroup(TypeGroup.music)));
    await settle(tester);
    expect(fake.lastArgs('list_children')['exts'], contains('mp3'));
    await closeApp(tester, fake);
  });

  testWidgets('search waits for a pause in typing, then lists matching files with their folders', (tester) async {
    final fake = await results(tester);
    fake.engine.requests.clear();
    final field = find.byType(TextField).first;
    await tester.enterText(field, 'i');
    await tester.pump(const Duration(milliseconds: 100));
    await tester.enterText(field, 'im');
    await tester.pump(const Duration(milliseconds: 100));
    await tester.enterText(field, 'img');
    await tester.pump(const Duration(milliseconds: 100));
    expect(fake.commands, isNot(contains('search')), reason: 'still typing');
    await tester.pump(const Duration(milliseconds: 400));
    await settle(tester);
    final searches = fake.engine.requests.where((r) => r.command == 'search').toList();
    expect(searches, hasLength(1));
    expect(searches.single.args['filter'], {'query': 'img'});
    expect(searches.single.args['under'], rootOf(fake));
    final rows = fake.app.browse.rows!;
    expect(find.textContaining(S.searchResults(en.count(rows.total!), rows.total!)), findsOneWidget);
    // A result shows its folder, and ticking it marks its folders partial.
    final first = rows.peek(0)!;
    expect(inTable(find.text(first.path!)), findsOneWidget);
    await tester.tap(
      find.descendant(
        of: find.ancestor(of: inTable(find.text(first.name)).first, matching: find.byType(InkWell)).first,
        matching: find.byType(Checkbox),
      ),
    );
    await settle(tester);
    expect(fake.app.selection.include, [first.id]);
    expect(
      fake.app.selection.stateOf(first.ancestorIds!.last, first.ancestorIds!.sublist(0, first.ancestorIds!.length - 1)),
      CheckState.partial,
    );
    // Clearing the search returns to the folder.
    await tester.tap(find.byTooltip(S.searchClear));
    await settle(tester);
    expect(fake.app.browse.isSearch, isFalse);
    await closeApp(tester, fake);
  });

  testWidgets('the disk layout: evidence, candidates, and a checked manual setting', (tester) async {
    final fake = await results(tester);
    await tester.tap(find.text(S.geometryChipValidated));
    await settleDialog(tester);
    expect(find.widgetWithText(AlertDialog, S.geometryIntro), findsOneWidget);
    expect(find.text(S.geometryValidated), findsOneWidget);
    final main = fake.app.catalog.info!.mainInstance!;
    final other = main.geometryCandidates.firstWhere(
      (g) => g.clusterSize != main.geometry.clusterSize || g.volumeOffset != main.geometry.volumeOffset,
    );
    await tester.tap(find.text(S.candidateUse).first);
    await settleDialog(tester);
    expect(find.text(S.geometryConfirmTitle), findsOneWidget);
    await tester.tap(find.text(S.geometryConfirm));
    await settleDialog(tester);
    expect(fake.lastArgs('geometry_set'), {
      'epoch': fake.app.catalog.epoch,
      'instance': main.id,
      'cluster_size': other.clusterSize,
      'volume_offset': other.volumeOffset,
    });

    // By hand: an offset that is not a multiple of 512 is refused here.
    await tapVisible(tester, find.text(S.geometryAdvanced));
    await settleDialog(tester);
    await tester.enterText(find.widgetWithText(TextField, S.geometryOffsetLabel), '1000');
    await tapVisible(tester, find.text(S.geometryApply));
    expect(find.text(S.offsetNotAligned), findsOneWidget);
    final sets = fake.engine.requests.where((r) => r.command == 'geometry_set').length;
    await tester.enterText(find.widgetWithText(TextField, S.geometryOffsetLabel), '2,097,152');
    await tapVisible(tester, find.text(S.geometryApply));
    await settleDialog(tester);
    await tester.tap(find.text(S.geometryConfirm));
    await settleDialog(tester);
    expect(fake.engine.requests.where((r) => r.command == 'geometry_set').length, sets + 1);
    expect(fake.lastArgs('geometry_set')['volume_offset'], 2097152);
    await closeApp(tester, fake);
  });

  testWidgets('overruling the engine: "this is the one I want" rebuilds the results', (tester) async {
    final fake = await results(tester);
    final older = fake.app.catalog.info!.instances[1];
    final epoch = fake.app.catalog.epoch;
    await tester.tap(inTree(find.text(S.instanceName(older))));
    await settle(tester);
    expect(inDetails(find.text(S.instanceDescription(older))), findsOneWidget);
    await tapVisible(tester, find.text(S.instanceMakeMain));
    await settleDialog(tester);
    expect(find.text(S.instanceConfirmTitle(InstanceOverride.main)), findsOneWidget);
    await tester.tap(find.text(S.instanceConfirm));
    await settleDialog(tester);
    final now = fake.app.catalog.info!;
    expect(fake.lastArgs('instance_set'), {'epoch': epoch, 'instance': older.id, 'kind': 'main'});
    expect(now.mainInstance!.manual, isTrue);
    // A new epoch: the screen is back at the root of the new main file system.
    expect(fake.app.browse.folder, FolderLocation(now.mainInstance!.root));
    await closeApp(tester, fake);
  });

  testWidgets('finding more files by signature: choice, progress, result', (tester) async {
    final fake = await results(tester);
    await tester.tap(find.widgetWithText(FilledButton, S.carveAction));
    await settleDialog(tester);
    expect(find.widgetWithText(AlertDialog, S.carveBody), findsOneWidget);
    // Narrow it to one kind.
    final kinds = fake.app.engine.hello!.carveKinds;
    for (final kind in kinds.skip(1)) {
      await tester.tap(find.widgetWithText(FilterChip, kind.toUpperCase()));
    }
    await tester.pump();
    await tester.tap(find.text(S.carveStart));
    await settleDialog(tester);
    expect(fake.lastArgs('carve_start'), {
      'kinds': [kinds.first],
    });
    await fake.step(2);
    final progress = fake.app.carve.progress!;
    expect(
      find.textContaining(
        '${en.count(progress.candidatesDone)} of ${en.count(progress.candidatesTotal)} places checked',
      ),
      findsOneWidget,
    );
    expect(
      tester.widget<FilledButton>(find.widgetWithText(FilledButton, S.carveAction)).onPressed,
      isNull,
      reason: 'one job at a time',
    );
    await fake.finishJob();
    final done = fake.app.carve.result!;
    expect(find.text(S.carveDone(en.count(done.found), en.bytes(done.bytesFound), cancelled: false)), findsOneWidget);
    await tester.tap(find.text(S.carveShow));
    await settle(tester);
    expect(fake.app.browse.folder!.id, fake.app.catalog.info!.mainInstance!.virtual.carved);
    await tester.tap(find.text(S.dismiss));
    await settle(tester);
    expect(fake.app.carve.phase, JobPhase.idle);
    await closeApp(tester, fake);
  });

  testWidgets('without signature hits the search by signature is unavailable, with the reason', (tester) async {
    final fake = await results(tester, carveHits: false);
    final button = find.widgetWithText(FilledButton, S.carveAction);
    expect(tester.widget<FilledButton>(button).onPressed, isNull);
    expect(find.byTooltip(S.carveNoHits), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('notices: a stopped scan, files outside the range, and the rest behind "show more"', (tester) async {
    final fake = await pumpApp(tester);
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\partial-mixed.hdsc'));
    fake.app.screen.value = AppScreen.results;
    await settle(tester);
    final info = fake.app.catalog.info!;
    expect(info.scanComplete, isFalse);
    expect(find.text(S.incompleteTitle(en.bytes(info.scannedBytes), en.bytes(info.range.length))), findsOneWidget);
    expect(
      find.text(S.outOfRangeTitle(en.count(info.outOfRangeFiles), en.bytes(info.outOfRangeBytes))),
      findsOneWidget,
    );
    expect(find.text(S.warningHeadline(WarningCode.mixedGenerations)), findsNothing);
    await tester.tap(find.text(S.moreNotices(1)));
    await settleDialog(tester);
    expect(find.text(S.warningHeadline(WarningCode.mixedGenerations)), findsOneWidget);
    await tester.tap(find.text(S.close));
    await settleDialog(tester);
    // Each notice explains itself on request.
    await tester.tap(find.text(S.moreAbout).first);
    await settleDialog(tester);
    expect(find.text(S.warningAdvice(WarningCode.scanIncomplete)!), findsOneWidget);
    await tester.tap(find.text(S.close));
    await settleDialog(tester);
    // And acts: continue the scan.
    await tester.tap(find.text(S.resumeScan));
    await settle(tester);
    expect(fake.lastArgs('scan_start'), containsPair('resume', true));
    expect(fake.app.screen.value, AppScreen.scan);
    await closeApp(tester, fake);
  });

  testWidgets('files outside the range: "scan the whole disk" asks first, then scans all of it', (tester) async {
    final fake = await results(tester);
    expect(fake.app.catalog.info!.outOfRangeFiles, greaterThan(0));
    await tester.tap(find.text(S.scanWholeDisk));
    await settleDialog(tester);
    expect(find.text(S.newScanTitle), findsOneWidget);
    await tester.tap(find.text(S.newScanConfirm));
    await settleDialog(tester);
    final args = fake.lastArgs('scan_start');
    expect(args.containsKey('start'), isFalse);
    expect(args.containsKey('end'), isFalse);
    expect(fake.app.screen.value, AppScreen.scan);
    await closeApp(tester, fake);
  });

  testWidgets('browse only: the banner offers to attach the disk; previews wait for it', (tester) async {
    final fake = await pumpApp(tester);
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\detached.hdsc'));
    fake.app.screen.value = AppScreen.results;
    await settle(tester);
    expect(find.text(S.browseOnlyTitle), findsOneWidget);
    fake.app.browse.open(FolderLocation(fake.landmarks.previewSamples, [rootOf(fake)]));
    await settle(tester);
    await pumpUntil(tester, inTable(find.text('picture.png')));
    await tester.tap(inTable(find.text('picture.png')));
    await settle(tester);
    expect(inDetails(find.text(S.previewNeedsSource)), findsOneWidget);
    expect(fake.commands, isNot(contains('read_file')));
    expect(tester.widget<FilledButton>(find.widgetWithText(FilledButton, S.carveAction)).onPressed, isNull);
    await closeApp(tester, fake);
  });

  testWidgets('keyboard: arrows move, Space ticks, Enter opens, in the table and in the tree', (tester) async {
    final fake = await results(tester);
    await tester.tap(inTable(find.text('Archive')));
    await settle(tester);
    final start = fake.app.browse.rows!;
    final archiveIndex = [for (var i = 0; i < start.total!; i++) start.peek(i)?.name].indexOf('Archive');
    expect(find.byType(ResultsScreen), findsOneWidget);
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown);
    await tester.pump();
    final next = start.peek(archiveIndex + 1)!;
    expect(inDetails(find.text(next.name)), findsWidgets, reason: 'the details follow the cursor');
    await tester.sendKeyEvent(LogicalKeyboardKey.space);
    await settle(tester);
    expect(fake.app.selection.include, [next.id]);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await settle(tester);
    expect(fake.app.browse.folder!.id, next.id);

    // The tree: Down to the first folder, Right opens it, Left closes it.
    final mainName = S.instanceName(fake.app.catalog.info!.mainInstance!);
    await tester.tap(inTree(find.text(mainName)));
    await settle(tester);
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown); // [Lost folders]
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown); // [Orphaned files]
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown); // Archive
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
    await settle(tester);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await settle(tester);
    expect(fake.app.browse.folder!.ancestors, [rootOf(fake)]);
    final opened = fake.app.browse.folder!.id;
    await tester.sendKeyEvent(LogicalKeyboardKey.space);
    await settle(tester);
    expect(fake.app.selection.isSelected(opened, [rootOf(fake)]), isTrue);
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowLeft);
    await settle(tester);
    await closeApp(tester, fake);
  });

  testWidgets('a million files in one folder: only the visible pages are fetched, memory stays bounded', (
    tester,
  ) async {
    final fake = await results(tester, mainNodes: 6000000);
    expect(fake.landmarks.hugeFolderFiles, 1000000);
    fake.app.browse.filters = fake.app.browse.filters.copyWith(showDeleted: true);
    fake.app.browse.open(FolderLocation(fake.landmarks.hugeFolder, [rootOf(fake)]));
    await settle(tester);
    await settle(tester);
    final rows = fake.app.browse.rows!;
    expect(rows.total, 1000000);
    expect(find.text(S.items(en.count(1000000), 1000000)), findsOneWidget);
    fake.engine.requests.clear();
    // Jump through the list as a scroll bar drag would.
    final list = find.descendant(of: find.byType(FileTable), matching: find.byType(Scrollable));
    final position = tester.state<ScrollableState>(list).position;
    for (final fraction in [0.1, 0.5, 0.9, 1.0]) {
      position.jumpTo(position.maxScrollExtent * fraction);
      await tester.pump();
      await settle(tester);
    }
    final pages = fake.engine.requests.where((r) => r.command == 'list_children').toList();
    expect(pages.length, lessThanOrEqualTo(12), reason: 'a page or two per jump, nothing in between');
    expect(pages.every((r) => r.args['limit'] == 200), isTrue);
    expect(fake.app.catalog.rows.cachedPages, lessThanOrEqualTo(64));
    // The last row is real, not a placeholder.
    final last = rows.peek(999999);
    expect(last, isNotNull);
    expect(inTable(find.text(last!.name)), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('a page that fails to load says so and can be retried', (tester) async {
    final fake = await results(tester);
    fake.engine.failNext('list_children', 'io_error', message: 'The session file could not be read.');
    fake.app.browse.open(FolderLocation(fake.landmarks.documents, [rootOf(fake)]));
    await settle(tester);
    expect(inTable(find.text(S.listFailed)), findsOneWidget);
    expect(inTable(find.textContaining('The session file could not be read.')), findsOneWidget);
    await tester.tap(inTable(find.text(S.retry)));
    await settle(tester);
    await settle(tester);
    expect(inTable(find.text(S.listFailed)), findsNothing);
    await closeApp(tester, fake);
  });

  testWidgets('a catalog rebuilt behind the app\'s back is reloaded, not shown stale', (tester) async {
    final fake = await results(tester);
    final before = fake.app.catalog.epoch;
    fake.engine.bumpEpoch();
    fake.app.browse.open(FolderLocation(fake.landmarks.documents, [rootOf(fake)]));
    await settle(tester);
    await settle(tester);
    expect(fake.app.catalog.epoch, isNot(before));
    expect(fake.app.browse.folder, FolderLocation(rootOf(fake)), reason: 'ids of the old epoch mean nothing');
    await pumpUntil(tester, inTable(find.text('Documents')));
    expect(inTable(find.text('Documents')), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('details at a narrow window open over the table and close again', (tester) async {
    final fake = await results(tester, size: minimumWindow);
    expect(find.byType(DetailsPane), findsNothing);
    await tester.tap(find.byTooltip(S.detailsShow));
    await settle(tester);
    expect(find.byType(DetailsPane), findsOneWidget);
    await tester.tap(find.byTooltip(S.detailsHide).last);
    await settle(tester);
    expect(find.byType(DetailsPane), findsNothing);
    await closeApp(tester, fake);
  });
}

final Formats en = Formats('en_US');
