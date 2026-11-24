// Screen 4, Recover: what is recovered, the options, the destination with
// the engine's live verdict and the two separate consents, the job's
// progress and outcome, opening the result.
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';
import 'package:hdsavior/ui/format.dart';
import 'package:hdsavior/ui/strings.dart';
import 'package:material_ui/material_ui.dart';

import 'ui_harness.dart';

final Formats en = Formats('en_US');
const Key destination = ValueKey('recover-destination');

void main() {
  /// Scans, ticks Documents and shows the Recover screen.
  Future<FakeApp> recovering(WidgetTester tester, {Size size = wideWindow}) async {
    final fake = await pumpApp(tester, size: size);
    await fake.scan();
    final root = fake.app.catalog.info!.mainInstance!.root;
    fake.app.selection.setChecked(fake.landmarks.documents, [root], true);
    fake.app.screen.value = AppScreen.recover;
    await settle(tester);
    return fake;
  }

  FilledButton startButton(WidgetTester tester) =>
      tester.widget<FilledButton>(find.ancestor(of: find.byIcon(Icons.save_alt), matching: find.byType(FilledButton)));

  testWidgets('without results there is nothing to recover', (tester) async {
    final fake = await pumpApp(tester);
    fake.app.screen.value = AppScreen.recover;
    await tester.pump();
    expect(find.text(S.recoverNoCatalogTitle), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('what will be recovered follows the selection and the options', (tester) async {
    final fake = await recovering(tester);
    final summary = fake.app.recover.summary!;
    expect(find.text(en.count(summary.files)), findsWidgets);
    expect(find.text(en.bytes(summary.bytes)), findsOneWidget);
    expect(startButton(tester).onPressed, isNull);
    expect(find.text(S.recoverWhyNoDestination), findsOneWidget);

    expect(summary.damaged, greaterThan(0));
    await tester.tap(find.text(S.optionSkipDamaged));
    await settle(tester);
    expect(fake.lastArgs('selection_summary'), containsPair('skip_damaged', true));
    expect(fake.app.recover.summary!.bytes, lessThan(summary.bytes), reason: 'damaged files are not written');
    expect(find.text(en.bytes(fake.app.recover.summary!.bytes)), findsOneWidget);
    await tester.tap(find.text(S.optionDeleted));
    await settle(tester);
    expect(fake.lastArgs('selection_summary'), containsPair('include_deleted', true));
    expect(fake.app.recover.summary!.files, greaterThanOrEqualTo(summary.files));
    await closeApp(tester, fake);
  });

  testWidgets('a good destination: checked live, then recovered with the chosen options', (tester) async {
    final fake = await recovering(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'F:\Rescued');
    final check = fake.lastArgs('check_destination');
    expect(check['path'], r'F:\Rescued');
    expect(check['purpose'], 'recover');
    expect(check['need_bytes'], fake.app.recover.summary!.bytes);
    expect(find.text(S.verdictHeadline(DestinationVerdict.ok, null)), findsOneWidget);
    expect(find.textContaining('free · NTFS'), findsOneWidget);
    expect(find.text(S.recoverReady), findsOneWidget);
    // "If a file is already there": replace.
    await tester.tap(find.byType(DropdownMenu<OnExists>));
    await settle(tester);
    await tester.tap(find.text(S.onExists(OnExists.overwrite)).last);
    await settle(tester);
    expect(startButton(tester).onPressed, isNotNull);
    await tester.tap(find.ancestor(of: find.byIcon(Icons.save_alt), matching: find.byType(FilledButton)));
    await settle(tester);
    final args = fake.lastArgs('recover_start');
    expect(args['dest'], r'F:\Rescued');
    expect(args['include'], [fake.landmarks.documents]);
    expect(args['on_exists'], 'overwrite');
    expect(args['allow_same_disk'], isFalse);
    expect(args['allow_unknown_disk'], isFalse);

    // Progress: current file and counts by outcome.
    await fake.step(2);
    final progress = fake.app.recover.progress!;
    expect(find.text(S.recoverCurrent(progress.current!)), findsOneWidget);
    expect(find.text(S.recoverDestinationLine(r'F:\Rescued')), findsOneWidget);
    expect(find.text(S.outcomeOk(en.count(progress.ok))), findsOneWidget);
    expect(find.text(S.outcomeWarnings(en.count(progress.warnings))), findsOneWidget);
    await fake.finishJob();
    final done = fake.app.recover.result!;
    expect(find.text(S.recoverCompleted), findsOneWidget);
    expect(find.text(done.reportPath!), findsOneWidget);
    await tester.tap(find.text(S.openFolder));
    await settle(tester);
    await tester.tap(find.text(S.openReport));
    await settle(tester);
    expect(fake.platform.openedFolders, [r'F:\Rescued']);
    expect(fake.platform.openedFiles, [done.reportPath]);
    await closeApp(tester, fake);
  });

  testWidgets('a file manager that cannot be started shows the path instead of failing', (tester) async {
    final fake = await recovering(tester);
    fake.platform.canOpen = false;
    await fake.run(() => fake.app.recover.start(dest: r'F:\Rescued'));
    await fake.finishJob();
    await tester.tap(find.text(S.openFolder));
    await settle(tester);
    expect(find.text(S.openFailed(r'F:\Rescued')), findsOneWidget);
    expect(find.text(S.copyPath), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('refusals: the scanned area, the lost volume, a wrong path; never startable', (tester) async {
    final fake = await recovering(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'E:\Rescued');
    expect(
      find.text(S.verdictHeadline(DestinationVerdict.overlapsSource, ProtectionReason.sessionRange)),
      findsOneWidget,
    );
    expect(startButton(tester).onPressed, isNull);
    expect(find.text(S.recoverWhyRefused), findsOneWidget);
    expect(find.text(S.useAnyway), findsNothing, reason: 'no override for the protected ranges');

    await typeAndSubmit(tester, find.byKey(destination), r'L:\Rescued');
    expect(
      find.text(S.verdictHeadline(DestinationVerdict.overlapsSource, ProtectionReason.lostVolume)),
      findsOneWidget,
    );
    final check = fake.app.recover.destinationCheck!;
    expect(check.lostFiles, greaterThan(0));
    expect(find.textContaining(S.lostFilesHere(en.count(check.lostFiles), en.bytes(check.lostBytes))), findsOneWidget);

    await typeAndSubmit(tester, find.byKey(destination), 'Rescued');
    expect(find.text(S.verdictHeadline(DestinationVerdict.invalidPath, null)), findsOneWidget);
    await typeAndSubmit(tester, find.byKey(destination), r'Q:\Nope');
    expect(find.text(S.verdictHeadline(DestinationVerdict.notFound, null)), findsOneWidget);
    expect(startButton(tester).onPressed, isNull);
    await closeApp(tester, fake);
  });

  testWidgets('same disk and unknown disk need separate consents, void for another path', (tester) async {
    final fake = await recovering(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'S:\Rescued');
    expect(find.text(S.verdictHeadline(DestinationVerdict.sameDisk, null)), findsOneWidget);
    expect(startButton(tester).onPressed, isNull);
    expect(find.text(S.recoverWhyConsent), findsOneWidget);
    // Cancelling the consent changes nothing.
    await tapVisible(tester, find.text(S.useAnyway));
    await settleDialog(tester);
    expect(find.text(S.sameDiskConsentTitle), findsOneWidget);
    expect(find.textContaining('No scan has finished yet'), findsNothing, reason: 'a catalog exists');
    await tester.tap(find.text(S.chooseAnother));
    await settleDialog(tester);
    expect(fake.app.recover.options.allowSameDisk, isFalse);
    // Given.
    await tapVisible(tester, find.text(S.useAnyway));
    await settleDialog(tester);
    await tester.tap(find.text(S.sameDiskConsentCheck));
    await tester.pump();
    await tester.tap(find.text(S.sameDiskConsentConfirm));
    await settleDialog(tester);
    expect(fake.app.recover.options.allowSameDisk, isTrue);
    expect(fake.app.recover.options.allowUnknownDisk, isFalse);
    expect(startButton(tester).onPressed, isNotNull);

    // Another path: the consent is gone.
    await typeAndSubmit(tester, find.byKey(destination), r'U:\Rescued');
    expect(fake.app.recover.options.allowSameDisk, isFalse);
    expect(find.text(S.verdictHeadline(DestinationVerdict.unknownDisk, null)), findsOneWidget);
    await tapVisible(tester, find.text(S.useAnyway));
    await settleDialog(tester);
    expect(find.text(S.unknownDiskConsentTitle), findsOneWidget, reason: 'its own dialog and wording');
    await tester.tap(find.text(S.unknownDiskConsentCheck));
    await tester.pump();
    await tester.tap(find.text(S.unknownDiskConsentConfirm));
    await settleDialog(tester);
    expect(fake.app.recover.options.allowUnknownDisk, isTrue);
    expect(fake.app.recover.options.allowSameDisk, isFalse);
    await tester.tap(find.ancestor(of: find.byIcon(Icons.save_alt), matching: find.byType(FilledButton)));
    await settle(tester);
    expect(fake.lastArgs('recover_start'), containsPair('allow_unknown_disk', true));
    expect(fake.lastArgs('recover_start'), containsPair('allow_same_disk', false));
    await closeApp(tester, fake);
  });

  testWidgets('too little space and a FAT32 size limit are said before starting', (tester) async {
    final fake = await recovering(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'T:\Rescued');
    expect(find.textContaining('Not enough free space'), findsOneWidget);
    expect(startButton(tester).onPressed, isNull);
    expect(find.text(S.recoverWhySpace), findsOneWidget);
    await typeAndSubmit(tester, find.byKey(destination), r'K:\Rescued');
    expect(find.text(S.fileSizeLimit('FAT32', '3.99 GB')), findsOneWidget);
    expect(startButton(tester).onPressed, isNotNull);
    await closeApp(tester, fake);
  });

  testWidgets('the folder dialog fills in the path and checks it', (tester) async {
    final fake = await recovering(tester);
    fake.pickers.answers.add(r'\\nas\share\Rescued');
    await tester.tap(find.text(S.browse));
    await settle(tester);
    expect(fake.pickers.requests.single, 'pickFolder');
    expect(fake.lastArgs('check_destination')['path'], r'\\nas\share\Rescued');
    expect(find.text(S.verdictHeadline(DestinationVerdict.ok, null)), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('a destination is checked again by itself when the catalog changes', (tester) async {
    final fake = await recovering(tester);
    await typeAndSubmit(tester, find.byKey(destination), r'F:\Rescued');
    final checks = fake.engine.requests.where((r) => r.command == 'check_destination').length;
    final main = fake.app.catalog.info!.mainInstance!;
    await fake.run(
      () => fake.app.catalog.setGeometry(
        instance: main.id,
        clusterSize: main.geometry.clusterSize,
        volumeOffset: main.geometry.volumeOffset,
      ),
    );
    await settle(tester);
    expect(fake.engine.requests.where((r) => r.command == 'check_destination').length, greaterThan(checks));
    expect(find.text(S.verdictHeadline(DestinationVerdict.ok, null)), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('stopping asks first, and the outcome says what was kept', (tester) async {
    final fake = await recovering(tester);
    await fake.run(() => fake.app.recover.start(dest: r'F:\Rescued'));
    await fake.step();
    await tester.tap(find.text(S.recoverCancel));
    await settleDialog(tester);
    expect(find.text(S.recoverCancelTitle), findsOneWidget);
    await tester.tap(find.text(S.recoverContinue));
    await settleDialog(tester);
    expect(fake.commands.last, isNot('job_cancel'));
    await tester.tap(find.text(S.recoverCancel));
    await settleDialog(tester);
    await tester.tap(find.widgetWithText(FilledButton, S.recoverCancelConfirm));
    await settleDialog(tester);
    expect(fake.commands.last, 'job_cancel');
    await fake.finishJob();
    expect(find.text(S.recoverCancelled), findsOneWidget);
    await tester.tap(find.text(S.recoverBackToForm));
    await settle(tester);
    expect(find.byKey(destination), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('a recovery that fails is explained: a full destination', (tester) async {
    final fake = await recovering(tester);
    fake.script.recoverFailAtStep = 2;
    await fake.run(() => fake.app.recover.start(dest: r'F:\Rescued'));
    await fake.finishJob();
    expect(find.text(S.recoverFailed), findsOneWidget);
    expect(find.textContaining(S.errorHeadline(EngineErrorCode.noSpace)), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('without its disk a session cannot recover: the field waits, the banner offers to attach', (
    tester,
  ) async {
    final fake = await pumpApp(tester);
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\detached.hdsc'));
    fake.app.selection.setChecked(fake.app.catalog.info!.mainInstance!.root, const [], true);
    fake.app.screen.value = AppScreen.recover;
    await settle(tester);
    expect(find.text(S.browseOnlyTitle), findsOneWidget);
    expect(tester.widget<TextField>(find.byKey(destination)).enabled, isFalse);
    expect(find.text(S.recoverNeedsSource), findsWidgets);
    await closeApp(tester, fake);
  });

  testWidgets('the start button stays in view at the smallest window', (tester) async {
    final fake = await recovering(tester, size: minimumWindow);
    final button = find.ancestor(of: find.byIcon(Icons.save_alt), matching: find.byType(FilledButton));
    final rect = tester.getRect(button);
    expect(rect.bottom, lessThanOrEqualTo(minimumWindow.height));
    expect(rect.top, greaterThan(0));
    await closeApp(tester, fake);
  });
}
