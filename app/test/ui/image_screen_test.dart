// Screen 5, Image: what to copy, where, the guard, progress with bad
// sectors, resuming, and rescuing from the finished image.
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';
import 'package:hdsavior/ui/format.dart';
import 'package:hdsavior/ui/strings.dart';
import 'package:material_ui/material_ui.dart';

import 'ui_harness.dart';

final Formats en = Formats('en_US');
const Key destination = ValueKey('image-destination');

void main() {
  Future<FakeApp> imaging(WidgetTester tester) async {
    final fake = await pumpApp(tester);
    await fake.openDisk();
    fake.app.screen.value = AppScreen.image;
    await settle(tester);
    return fake;
  }

  Finder startButton() => find.ancestor(of: find.byIcon(Icons.album_outlined), matching: find.byType(FilledButton));

  testWidgets('without a source: what to do first', (tester) async {
    final fake = await pumpApp(tester);
    fake.app.screen.value = AppScreen.image;
    await tester.pump();
    expect(find.text(S.imageNoSourceTitle), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('a partition to a file: the guard is asked with its size, the job gets its range', (tester) async {
    final fake = await imaging(tester);
    final partition = fake.app.source.current!.partitions.single;
    expect(find.text(S.imageWhole(true, '3.63 TB')), findsOneWidget);
    await tester.tap(find.textContaining('Partition 1'));
    await settle(tester);
    fake.pickers.answers.add(r'F:\Images\disk1.img');
    await tester.tap(find.text(S.browse));
    await settle(tester);
    expect(fake.pickers.requests.single, contains('WDC WD40EZRZ-00GXCB0 WD-WCC7K0000000.img'));
    final check = fake.lastArgs('check_destination');
    expect(check, {'path': r'F:\Images\disk1.img', 'purpose': 'image', 'need_bytes': partition.size});
    expect(find.text(S.verdictHeadline(DestinationVerdict.ok, null)), findsOneWidget);
    await tester.tap(startButton());
    await settle(tester);
    expect(fake.lastArgs('image_start'), {
      'start': partition.offset,
      'end': partition.end,
      'dest': r'F:\Images\disk1.img',
      'resume': false,
      'allow_same_disk': false,
      'allow_unknown_disk': false,
    });
    await closeApp(tester, fake);
  });

  testWidgets('progress with unreadable sectors; the finished image can be rescued from', (tester) async {
    final fake = await imaging(tester);
    fake.script.imageBadBytes = 1 << 20;
    await typeAndSubmit(tester, find.byKey(destination), r'F:\Images\disk1.img');
    await tester.tap(startButton());
    await settle(tester);
    await fake.step(3);
    final progress = fake.app.image.progress!;
    expect(find.text(S.ofTotal(en.bytes(progress.bytesDone), en.bytes(progress.bytesTotal))), findsOneWidget);
    expect(find.text(S.badSectorsTitle(en.bytes(progress.badBytes))), findsOneWidget);
    expect(find.text(S.imageDestinationLine(r'F:\Images\disk1.img')), findsOneWidget);
    await fake.finishJob();
    final done = fake.app.image.result!;
    expect(find.text(S.imageCompleted), findsOneWidget);
    expect(find.text(S.imageCompletedBad(en.bytes(done.badBytes))), findsOneWidget);
    expect(find.text(done.badMapPath!), findsOneWidget);
    await tester.tap(find.text(S.openFolder));
    await settle(tester);
    expect(fake.platform.openedFolders, [r'F:\Images']);
    await tester.tap(find.text(S.imageOpenAsSource));
    await settle(tester);
    expect(fake.lastArgs('open_source'), {'kind': 'image', 'path': r'F:\Images\disk1.img'});
    expect(fake.app.screen.value, AppScreen.source);
    await closeApp(tester, fake);
  });

  testWidgets('an existing file is never overwritten; continuing it is offered', (tester) async {
    final fake = await imaging(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'F:\Images\exists.img');
    await tester.tap(startButton());
    await settleDialog(tester);
    expect(find.text(S.imageExistsTitle), findsOneWidget);
    await tester.tap(find.widgetWithText(FilledButton, S.imageExistsResume));
    await settleDialog(tester);
    expect(fake.lastArgs('image_start'), containsPair('resume', true));
    await closeApp(tester, fake);
  });

  testWidgets('the guard: the rescued disk is refused; the same-disk consent is separate', (tester) async {
    final fake = await imaging(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'E:\disk.img');
    expect(
      find.text(S.verdictHeadline(DestinationVerdict.overlapsSource, ProtectionReason.sessionRange)),
      findsOneWidget,
    );
    expect(tester.widget<FilledButton>(startButton()).onPressed, isNull);
    await typeAndSubmit(tester, find.byKey(destination), r'S:\disk.img');
    expect(tester.widget<FilledButton>(startButton()).onPressed, isNull);
    await tapVisible(tester, find.text(S.useAnyway));
    await settleDialog(tester);
    expect(find.textContaining('No scan has finished yet'), findsOneWidget);
    await tester.tap(find.text(S.sameDiskConsentCheck));
    await tester.pump();
    await tester.tap(find.text(S.sameDiskConsentConfirm));
    await settleDialog(tester);
    expect(tester.widget<FilledButton>(startButton()).onPressed, isNotNull);
    await tester.tap(startButton());
    await settle(tester);
    expect(fake.lastArgs('image_start'), containsPair('allow_same_disk', true));
    await closeApp(tester, fake);
  });

  testWidgets('a FAT32 drive cannot hold the image: said, and not startable', (tester) async {
    final fake = await imaging(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'K:\disk.img');
    expect(find.text(S.imageTooLarge('FAT32', '3.99 GB')), findsOneWidget);
    expect(tester.widget<FilledButton>(startButton()).onPressed, isNull);
    await closeApp(tester, fake);
  });

  testWidgets('stopping asks first; a stopped image can be continued later', (tester) async {
    final fake = await imaging(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'F:\Images\disk1.img');
    await tester.tap(startButton());
    await settle(tester);
    await fake.step();
    await tester.tap(find.text(S.imageCancel));
    await settleDialog(tester);
    await tester.tap(find.widgetWithText(FilledButton, S.imageCancelConfirm));
    await settleDialog(tester);
    expect(fake.commands.last, 'job_cancel');
    await fake.finishJob();
    expect(find.text(S.imageCancelled), findsOneWidget);
    await tester.tap(find.text(S.imageBackToResume));
    await settle(tester);
    await tester.tap(find.text(S.imageResume));
    await tester.pump();
    expect(find.text(S.imageContinue), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('a lost disk during imaging is explained', (tester) async {
    final fake = await imaging(tester);
    fake.script.imageFailAtStep = 2;
    await fake.run(() => fake.app.image.start(dest: r'F:\Images\disk1.img'));
    await fake.finishJob();
    expect(find.text(S.imageFailed), findsOneWidget);
    expect(find.textContaining(S.errorHeadline(EngineErrorCode.sourceLost)), findsOneWidget);
    await closeApp(tester, fake);
  });
}
