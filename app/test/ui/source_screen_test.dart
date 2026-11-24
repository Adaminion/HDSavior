// Screen 1, Source: the disk cards, opening a disk with its warnings and
// partition table, starting a scan with an automatic or chosen session
// file, images, saved sessions, attaching a disk, and the empty and error
// states.
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';
import 'package:hdsavior/ui/screens/source_screen.dart';
import 'package:hdsavior/ui/strings.dart';
import 'package:hdsavior/ui/widgets/errors.dart';
import 'package:material_ui/material_ui.dart';

import 'ui_harness.dart';

Finder card(String model) => find.widgetWithText(DiskCard, model);

Finder inCard(String model, Finder what) => find.descendant(of: card(model), matching: what);

void main() {
  testWidgets('disks are cards with their volumes and risk badges, the system disk after the others', (tester) async {
    final fake = await pumpApp(tester);
    expect(find.byType(DiskCard), findsNWidgets(7));
    final order = [for (final w in tester.widgetList<DiskCard>(find.byType(DiskCard))) w.disk.number];
    expect(order, [1, 2, 3, 4, 6, 0, 5], reason: 'ordinary disks by number, then Windows, then empty readers');

    expect(inCard('WDC WD40EZRZ-00GXCB0', find.text('E:')), findsOneWidget);
    expect(inCard('WDC WD40EZRZ-00GXCB0', find.textContaining('New Volume · NTFS · 3.63 TB')), findsOneWidget);
    expect(inCard('WDC WD40EZRZ-00GXCB0', find.textContaining('Disk 1 · 3.63 TB · USB · Hard disk')), findsOneWidget);
    expect(inCard('Samsung SSD 980 PRO 500GB', find.text(S.badgeSystem)), findsOneWidget);
    expect(inCard('Samsung SSD 980 PRO 500GB', find.text(S.badgeTrim)), findsOneWidget);
    expect(inCard('WDC WD40EZRZ-00GXCB0', find.text(S.badgeSystem)), findsNothing);
    expect(inCard('TOSHIBA DT01ACA200', find.text(S.badgeSpaces)), findsOneWidget);
    expect(inCard('TOSHIBA DT01ACA200', find.text(S.badgeDynamic)), findsOneWidget);
    expect(inCard('Generic STORAGE DEVICE', find.text(S.badgeNoMedia)), findsOneWidget);
    expect(inCard('Generic STORAGE DEVICE', find.text(S.noVolumes)), findsOneWidget);
    final noMedia = inCard('Generic STORAGE DEVICE', find.widgetWithText(FilledButton, S.chooseDisk));
    await tester.ensureVisible(noMedia);
    expect(tester.widget<FilledButton>(noMedia).onPressed, isNull, reason: 'a reader without a card cannot be opened');
    await closeApp(tester, fake);
  });

  testWidgets('choosing a disk opens it read-only and shows its real table and the warnings in plain words', (
    tester,
  ) async {
    final fake = await pumpApp(tester);
    await tapVisible(tester, inCard('WDC WD40EZRZ-00GXCB0', find.text(S.chooseDisk)));
    expect(fake.lastArgs('open_source'), {'kind': 'disk', 'path': r'\\.\PhysicalDrive1'});
    expect(find.byType(DiskCard), findsNothing);
    expect(find.textContaining('WDC WD40EZRZ-00GXCB0 (disk 1, 3.63 TB)'), findsOneWidget);
    // The warning: our headline and advice, the engine's own words below.
    expect(find.text(S.warningHeadline(WarningCode.mountedVolume)), findsOneWidget);
    expect(find.text(S.warningAdvice(WarningCode.mountedVolume)!), findsOneWidget);
    expect(find.textContaining('Windows is using a volume on this disk (E:)'), findsOneWidget);
    // The partition table read from the disk, with what can be scanned.
    expect(find.text('Partition 1 · E: · New Volume'), findsOneWidget);
    expect(find.textContaining('Basic data · starts at 1,048,576'), findsOneWidget);
    expect(find.text(S.scanPartition), findsOneWidget);
    expect(find.text(S.scanWhole(true)), findsOneWidget);

    await tapVisible(tester, find.text(S.chooseOtherSource));
    expect(fake.commands.sublist(fake.commands.length - 2), ['close_source', 'close_session']);
    expect(find.byType(DiskCard), findsNWidgets(7));
    await closeApp(tester, fake);
  });

  testWidgets('the system disk carries every warning, the gravest first', (tester) async {
    final fake = await pumpApp(tester);
    await tapVisible(tester, inCard('Samsung SSD 980 PRO 500GB', find.text(S.chooseDisk)));
    for (final code in [
      WarningCode.trimSupported,
      WarningCode.mountedVolume,
      WarningCode.systemDisk,
      WarningCode.pageFileOnSource,
      WarningCode.appOnSource,
    ]) {
      expect(find.text(S.warningHeadline(code)), findsOneWidget, reason: code.wire);
    }
    final trim = tester.getTopLeft(find.text(S.warningHeadline(WarningCode.trimSupported))).dy;
    final mounted = tester.getTopLeft(find.text(S.warningHeadline(WarningCode.mountedVolume))).dy;
    expect(trim, lessThan(mounted), reason: 'red before amber');
    await closeApp(tester, fake);
  });

  testWidgets('scanning a partition: the session file is named and checked automatically', (tester) async {
    final fake = await pumpApp(tester);
    await fake.openDisk();
    await tester.pump();
    await tapVisible(tester, find.text(S.scanPartition));
    final check = fake.engine.requests.lastWhere(
      (r) => r.command == 'check_destination' && r.args['purpose'] == 'session',
    );
    final path = check.args['path']! as String;
    expect(
      path,
      startsWith('${fake.platform.defaultSessionDirectory}\\WDC WD40EZRZ-00GXCB0 WD-WCC7K0000000 at 1048576 '),
    );
    expect(path, endsWith('.hdsc'));
    expect(fake.platform.createdDirectories, contains(fake.platform.defaultSessionDirectory));
    final partition = fake.app.source.current!.partitions.single;
    expect(fake.lastArgs('scan_start'), {
      'start': partition.offset,
      'end': partition.end,
      'session_path': path,
      'resume': false,
      'collect_carve_hits': true,
      'allow_same_disk': false,
      'allow_unknown_disk': false,
    });
    expect(fake.app.screen.value, AppScreen.scan);
    await closeApp(tester, fake);
  });

  testWidgets('a refused session place leads to a folder of the user, with the same-disk consent', (tester) async {
    // The usual place is on the very disk being rescued.
    final fake = await pumpApp(tester, sessionDirectory: r'E:\HDSavior\Sessions');
    await fake.openDisk();
    await tester.pump();
    await tapVisible(tester, find.text(S.scanWhole(true)));
    expect(fake.commands, isNot(contains('scan_start')));
    expect(fake.platform.createdDirectories, isEmpty, reason: 'nothing is created where the guard refused');
    expect(find.text(S.sessionLocationTitle), findsOneWidget);
    expect(
      find.textContaining(S.verdictHeadline(DestinationVerdict.overlapsSource, ProtectionReason.sessionRange)),
      findsOneWidget,
    );
    final confirm = find.widgetWithText(FilledButton, S.sessionLocationConfirm);
    expect(tester.widget<FilledButton>(confirm).onPressed, isNull);

    // Another volume of the same disk: allowed only with consent.
    await typeAndSubmit(tester, find.byType(TextField), r'S:\Scans');
    final check = fake.engine.requests.lastWhere((r) => r.command == 'check_destination');
    expect(check.args['purpose'], 'session');
    expect(check.args['path'], matches(r'^S:\\Scans\\WDC WD40EZRZ-00GXCB0 .*\.hdsc$'));
    expect(find.text(S.verdictHeadline(DestinationVerdict.sameDisk, null)), findsOneWidget);
    expect(tester.widget<FilledButton>(confirm).onPressed, isNull);
    await tester.tap(find.text(S.useAnyway));
    await settle(tester);
    expect(find.text(S.sameDiskConsentTitle), findsOneWidget);
    expect(find.textContaining('No scan has finished yet'), findsOneWidget, reason: 'no catalog yet: say so');
    // The dangerous button needs the tick; the safe one is the default.
    final writeAnyway = find.widgetWithText(TextButton, S.sameDiskConsentConfirm);
    expect(tester.widget<TextButton>(writeAnyway).onPressed, isNull);
    await tester.tap(find.text(S.sameDiskConsentCheck));
    await tester.pump();
    await tester.tap(writeAnyway);
    await settle(tester);
    expect(find.text(S.consentGiven), findsOneWidget);
    await tester.tap(confirm);
    await settle(tester);
    final args = fake.lastArgs('scan_start');
    expect(args['session_path'], check.args['path']);
    expect(args['allow_same_disk'], isTrue);
    expect(args['allow_unknown_disk'], isFalse);
    await closeApp(tester, fake);
  });

  testWidgets('scanning without saving is possible after a clear warning', (tester) async {
    final fake = await pumpApp(tester, sessionDirectory: r'E:\HDSavior\Sessions');
    await fake.openDisk();
    await tester.pump();
    await tapVisible(tester, find.text(S.scanWhole(true)));
    await tester.tap(find.text(S.scanWithoutSaving));
    await settle(tester);
    expect(find.text(S.scanWithoutSavingBody), findsOneWidget);
    await tester.tap(find.widgetWithText(FilledButton, S.scanWithoutSavingConfirm));
    await settle(tester);
    expect(fake.lastArgs('scan_start'), isNot(contains('session_path')));
    await closeApp(tester, fake);
  });

  testWidgets('a scan refused for its session file asks again instead of failing', (tester) async {
    final fake = await pumpApp(tester);
    await fake.openDisk();
    await tester.pump();
    fake.engine.failNext(
      'scan_start',
      'destination_refused',
      message: 'refused',
      data: {'verdict': 'unknown_disk', 'message': 'The disk of this location cannot be determined.'},
    );
    await tapVisible(tester, find.text(S.scanPartition));
    expect(find.text(S.sessionLocationTitle), findsOneWidget);
    expect(find.text('The disk of this location cannot be determined.'), findsOneWidget);
    await tester.tap(find.text(S.cancel));
    await settle(tester);
    expect(fake.app.scan.isActive, isFalse);
    await closeApp(tester, fake);
  });

  testWidgets('without administrator rights: the reason, and the engine refusal explained', (tester) async {
    final fake = await pumpApp(tester, elevated: false);
    expect(find.text(S.notElevatedTitle), findsOneWidget);
    await tapVisible(tester, inCard('WDC WD40EZRZ-00GXCB0', find.text(S.chooseDisk)));
    expect(find.text(S.openDiskFailed), findsOneWidget);
    expect(find.text(S.errorHeadline(EngineErrorCode.accessDenied)), findsOneWidget);
    await tester.tap(find.text(S.technicalDetails));
    await settle(tester);
    expect(find.textContaining('Code: access_denied'), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('no disks: an explanation and the way to an image instead', (tester) async {
    final fake = await pumpApp(tester, os: 'linux');
    expect(find.text(S.noDisksTitle), findsOneWidget);
    expect(find.text(S.noDisksNotWindows), findsOneWidget);
    fake.pickers.answers.add('/images/disk.img');
    await tapVisible(tester, find.widgetWithText(OutlinedButton, S.openImage).last);
    expect(fake.pickers.requests.single, startsWith('pickFile'));
    expect(fake.lastArgs('open_source'), {'kind': 'image', 'path': '/images/disk.img'});
    expect(find.textContaining('Disk image disk.img'), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('a failed disk list is shown with a way to try again', (tester) async {
    final fake = await pumpApp(tester, prepare: (fake) {});
    fake.engine.failNext('list_disks', 'internal', message: 'enumeration failed');
    await tester.tap(find.byTooltip(S.refreshDisks));
    await settle(tester);
    expect(find.text(S.disksFailed), findsOneWidget);
    expect(find.textContaining('enumeration failed'), findsOneWidget);
    await tester.tap(find.text(S.retry));
    await settle(tester);
    expect(find.text(S.disksFailed), findsNothing);
    expect(find.byType(DiskCard), findsNWidgets(7));
    await closeApp(tester, fake);
  });

  testWidgets('an image that cannot be opened is explained; cancelling the dialog does nothing', (tester) async {
    final fake = await pumpApp(tester);
    await tester.tap(find.text(S.openImage));
    await settle(tester);
    expect(fake.commands, isNot(contains('open_source')), reason: 'the dialog was cancelled');
    fake.pickers.answers.add(r'D:\images\missing.img');
    await tester.tap(find.text(S.openImage));
    await settle(tester);
    expect(find.text(S.openImageFailed), findsOneWidget);
    expect(find.text(S.errorHeadline(EngineErrorCode.notFound)), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('opening an image over unsaved results asks first, like the other ways to another source', (
    tester,
  ) async {
    final fake = await pumpApp(tester);
    await fake.scan(); // without a session file
    expect(fake.app.source.sessionPath, isNull);
    fake.app.screen.value = AppScreen.source;
    await settle(tester);
    final opens = fake.commands.where((c) => c == 'open_source').length;
    fake.pickers.answers.add(r'D:\images\other.img');
    await tester.tap(find.text(S.openImage).first);
    await settle(tester);
    expect(find.text(S.closeSourceTitle), findsOneWidget);
    expect(find.text(S.closeSourceBodyUnsaved), findsOneWidget);
    await tester.tap(find.text(S.cancel));
    await settle(tester);
    expect(fake.pickers.requests, isEmpty, reason: 'nothing is picked before the user agrees');
    expect(fake.commands.where((c) => c == 'open_source'), hasLength(opens));
    expect(fake.app.catalog.hasCatalog, isTrue, reason: 'the scan is kept');

    await tester.tap(find.text(S.openImage).first);
    await settle(tester);
    await tester.tap(find.text(S.closeSourceConfirm));
    await settle(tester);
    expect(fake.commands.where((c) => c == 'open_source'), hasLength(opens + 1));
    expect(fake.lastArgs('open_source')['path'], r'D:\images\other.img');
    await closeApp(tester, fake);
  });

  testWidgets('a saved scan opens on the Results screen', (tester) async {
    final fake = await pumpApp(tester);
    fake.pickers.answers.add(r'D:\scans\disk1.hdsc');
    await tester.tap(find.text(S.openSession));
    await settle(tester);
    expect(fake.pickers.requests.single, contains('[hdsc]'));
    expect(fake.lastArgs('session_load'), {'path': r'D:\scans\disk1.hdsc'});
    expect(fake.app.screen.value, AppScreen.results);
    expect(fake.app.catalog.hasCatalog, isTrue);
    await closeApp(tester, fake);
  });

  testWidgets('a saved scan without its disk: browse only, then attach the disk', (tester) async {
    final fake = await pumpApp(tester);
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\detached.hdsc'));
    await tester.pump();
    expect(find.text(S.browseOnlyTitle), findsOneWidget);
    await tester.tap(find.text(S.attachAction));
    await settle(tester);
    expect(find.text(S.attachTitle), findsOneWidget);
    // The recorded disk is preselected.
    final radio = tester.widget<RadioGroup<String>>(find.byType(RadioGroup<String>));
    expect(radio.groupValue, r'\\.\PhysicalDrive1');
    await tester.tap(find.text(S.attachConfirm));
    await settleDialog(tester);
    expect(fake.lastArgs('session_attach_source'), {'kind': 'disk', 'path': r'\\.\PhysicalDrive1', 'force': false});
    expect(find.text(S.attachTitle), findsNothing);
    expect(find.text(S.browseOnlyTitle), findsNothing);
    expect(fake.app.catalog.sourceAttached, isTrue);
    await closeApp(tester, fake);
  });

  testWidgets('attaching the wrong disk is refused with the reason; "attach anyway" forces it', (tester) async {
    final fake = await pumpApp(tester);
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\detached.hdsc'));
    await tester.pump();
    await tester.tap(find.text(S.attachAction));
    await settle(tester);
    // As the real engine says it (docs/protocol: session_load source_error).
    fake.engine.failNext(
      'session_attach_source',
      'source_mismatch',
      message: 'Too few records could be compared.',
      data: {'failed': ['too_few_samples'], 'samples': 3, 'matched': 3, 'mismatched': 0, 'unreadable': 0},
    );
    await tester.tap(find.text(S.attachConfirm));
    await settle(tester);
    expect(find.text(S.errorHeadline(EngineErrorCode.sourceMismatch)), findsOneWidget);
    expect(find.text('Too few records could be compared.'), findsOneWidget);
    await tester.tap(find.text(S.attachForce));
    await settleDialog(tester);
    expect(fake.lastArgs('session_attach_source')['force'], isTrue);
    expect(find.text(S.attachTitle), findsNothing);
    await closeApp(tester, fake);
  });

  testWidgets('a source whose records differ cannot be attached anyway', (tester) async {
    final fake = await pumpApp(tester);
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\detached.hdsc'));
    await tester.pump();
    await tester.tap(find.text(S.attachAction));
    await settle(tester);
    // force only overrules too_few_samples; the engine refuses this one again.
    fake.engine.failNext(
      'session_attach_source',
      'source_mismatch',
      message: '7 of 8 compared records differ',
      data: {'failed': ['records'], 'samples': 8, 'matched': 1, 'mismatched': 7, 'unreadable': 0},
    );
    await tester.tap(find.text(S.attachConfirm));
    await settle(tester);
    expect(find.text(S.errorHeadline(EngineErrorCode.sourceMismatch)), findsOneWidget);
    expect(find.text(S.attachForce), findsNothing);
    await tester.tap(find.text(S.cancel));
    await settleDialog(tester);
    await closeApp(tester, fake);
  });

  testWidgets('after the engine died, the last session can be opened again', (tester) async {
    final fake = await pumpApp(tester);
    await fake.openDisk();
    await fake.run(() => fake.app.scan.start(sessionPath: r'C:\sessions\last.hdsc'));
    await fake.step(2);
    fake.engine.crash();
    await settle(tester);
    expect(find.textContaining(S.engineFailedSession), findsOneWidget);
    await tester.tap(find.text(S.engineRestart));
    await settle(tester);
    fake.app.screen.value = AppScreen.source;
    await tester.pump();
    expect(find.text(S.lastSessionTitle), findsOneWidget);
    expect(find.text(r'C:\sessions\last.hdsc'), findsOneWidget);
    await closeApp(tester, fake);
  });

  test('orderDisks keeps ordinary disks first, by number', () {
    Disk disk(int number, {bool system = false, int size = 1}) => Disk.fromJson(
      JsonObject.root({
        'kind': 'disk', 'path': 'p$number', 'size': size, 'sector_size': 512, 'physical_sector_size': 512, //
        'model': null, 'serial': null, 'disk_number': number, 'number': number, 'bus': '', 'removable': false,
        'is_system': system, 'media': 'hdd', 'trim': false, 'flags': <Object?>[], 'table': 'gpt',
        'partitions': <Object?>[],
      }, 'Disk'),
    );
    final ordered = orderDisks([disk(3), disk(0, system: true), disk(2, size: 0), disk(1)]);
    expect([for (final d in ordered) d.number], [1, 3, 0, 2]);
  });

  testWidgets('the technical details of an error can be copied', (tester) async {
    final fake = await pumpApp(tester);
    fake.pickers.answers.add(r'D:\images\denied.img');
    await tester.tap(find.text(S.openImage));
    await settle(tester);
    expect(find.byType(TechnicalDetailsButton), findsOneWidget);
    await closeApp(tester, fake);
  });
}
