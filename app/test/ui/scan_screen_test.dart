// Screen 2, Scan: progress, phases, counters, bad sectors and warnings as
// they arrive, "Stop and show results", and the way back after the disk
// was lost.
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';
import 'package:hdsavior/ui/format.dart';
import 'package:hdsavior/ui/screens/scan_screen.dart';
import 'package:hdsavior/ui/strings.dart';
import 'package:material_ui/material_ui.dart';

import 'ui_harness.dart';

void main() {
  Future<FakeApp> scanning(WidgetTester tester, {void Function(FakeScript script)? script}) async {
    final fake = await pumpApp(tester);
    script?.call(fake.script);
    await fake.openDisk();
    await fake.run(() => fake.app.scan.start(sessionPath: r'C:\sessions\disk1.hdsc'));
    fake.app.screen.value = AppScreen.scan;
    await tester.pump();
    return fake;
  }

  testWidgets('progress, speed, time left, counters and phases follow the engine', (tester) async {
    final fake = await scanning(tester);
    await fake.step();
    final progress = fake.app.scan.progress!;
    expect(progress.phase, ScanPhase.mftProbe);
    expect(find.text(S.phaseTitle(ScanPhase.mftProbe)), findsOneWidget);
    expect(find.text('12 %'), findsOneWidget);
    expect(find.text('150 MB/s'), findsOneWidget);
    expect(find.text('${en.bytes(progress.bytesDone)} of 3.63 TB'), findsOneWidget);
    expect(find.text(en.duration(progress.etaSeconds!)), findsOneWidget);
    expect(find.text(en.count(progress.files)), findsOneWidget);
    expect(find.text(en.count(progress.dirs)), findsOneWidget);
    expect(find.text(S.scanSavedTo(r'C:\sessions\disk1.hdsc')), findsOneWidget);
    expect(find.text(S.sweepHintTitle), findsNothing);
    // The steps: the first is current.
    final steps = tester.widget<PhaseSteps>(find.byType(PhaseSteps));
    expect(steps.phase, ScanPhase.mftProbe);

    await fake.step(2);
    expect(find.text(S.phaseTitle(ScanPhase.sweep)), findsOneWidget);
    expect(find.text(S.sweepHintTitle), findsOneWidget, reason: 'the cue that most of the tree is found');
    await closeApp(tester, fake);
  });

  testWidgets('bad sectors and warnings are shown as they arrive, each warning once', (tester) async {
    final fake = await scanning(
      tester,
      script: (script) => script
        ..scanBadBytesPerStep = 4096
        ..scanWarningsFromStep = 2
        ..scanWarnings = [
          {'code': 'source_reads_zero', 'message': 'The first gigabyte holds only zeros.'},
        ],
    );
    await fake.step();
    expect(find.text(S.badSectorsTitle('4.00 KB')), findsOneWidget);
    expect(find.text(S.warningHeadline(WarningCode.sourceReadsZero)), findsNothing);
    await fake.step(3);
    expect(find.text(S.badSectorsTitle('16.0 KB')), findsOneWidget);
    expect(find.text(S.warningHeadline(WarningCode.sourceReadsZero)), findsOneWidget);
    expect(find.text('The first gigabyte holds only zeros.'), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('"Stop and show results" stops the scan and shows what was found', (tester) async {
    final fake = await scanning(tester);
    await fake.step(3);
    expect(find.text(S.stopExplanation), findsOneWidget);
    await tester.tap(find.text(S.stopAndShow));
    await settle(tester);
    expect(fake.commands.last, 'job_cancel');
    expect(find.text(S.stopping), findsOneWidget);
    expect(tester.widget<FilledButton>(find.widgetWithText(FilledButton, S.stopping)).onPressed, isNull);
    await fake.step();
    // Rebuilding the folders cannot be stopped.
    expect(fake.app.scan.progress!.phase, ScanPhase.analyzing);
    expect(find.text(S.stopNotPossible), findsOneWidget);
    await fake.finishJob();
    expect(fake.app.scan.status, JobStatus.stopped);
    expect(fake.app.screen.value, AppScreen.results);
    await tester.pump();
    // Back on the Scan screen: the outcome, and the way to continue.
    fake.app.screen.value = AppScreen.scan;
    await tester.pump();
    expect(find.text(S.scanStoppedTitle), findsOneWidget);
    await tester.tap(find.text(S.resumeScan));
    await settle(tester);
    expect(fake.lastArgs('scan_start')['resume'], isTrue);
    await closeApp(tester, fake);
  });

  testWidgets('a lost disk: explained, then reconnected and resumed', (tester) async {
    final fake = await scanning(tester, script: (script) => script.scanFailAtStep = 3);
    await fake.finishJob();
    expect(fake.app.screen.value, AppScreen.scan, reason: 'a failure stays where it can be explained');
    expect(find.text(S.sourceLostTitle), findsOneWidget);
    expect(find.text(S.sourceLostBody), findsOneWidget);
    await tester.tap(find.text(S.reattachAndResume));
    await settleDialog(tester);
    expect(find.text(S.attachTitle), findsOneWidget);
    await tester.tap(find.text(S.attachConfirm));
    await settleDialog(tester);
    expect(fake.lastArgs('session_attach_source')['path'], r'\\.\PhysicalDrive1');
    final resume = fake.lastArgs('scan_start');
    expect(resume['resume'], isTrue);
    expect(fake.app.scan.isActive, isTrue);
    await fake.finishJob();
    expect(fake.app.scan.status, JobStatus.completed);
    await closeApp(tester, fake);
  });

  testWidgets('another failure is explained with its code on demand', (tester) async {
    final fake = await scanning(
      tester,
      script: (script) => script
        ..scanFailAtStep = 2
        ..scanFailCode = 'io_error',
    );
    await fake.finishJob();
    expect(find.text(S.scanFailedTitle), findsOneWidget);
    expect(find.textContaining(S.errorHeadline(EngineErrorCode.ioError)), findsOneWidget);
    expect(find.text(S.technicalDetails), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('without a source the screen says what to do first', (tester) async {
    final fake = await pumpApp(tester);
    fake.app.screen.value = AppScreen.scan;
    await tester.pump();
    expect(find.text(S.scanNoSourceTitle), findsOneWidget);
    await tester.tap(find.text(S.chooseSource));
    await tester.pump();
    expect(fake.app.screen.value, AppScreen.source);
    await fake.openDisk();
    fake.app.screen.value = AppScreen.scan;
    await tester.pump();
    expect(find.text(S.scanReadyTitle), findsOneWidget);
    await closeApp(tester, fake);
  });

  testWidgets('a stopped scan from a saved session can be continued', (tester) async {
    final fake = await pumpApp(tester);
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\partial.hdsc'));
    fake.app.screen.value = AppScreen.scan;
    await tester.pump();
    expect(find.text(S.scanPausedTitle), findsOneWidget);
    await tester.tap(find.text(S.resumeScan));
    await settle(tester);
    expect(fake.lastArgs('scan_start'), containsPair('resume', true));
    expect(fake.app.scan.isActive, isTrue);
    await closeApp(tester, fake);
  });
}

/// The formats the app uses in tests.
final Formats en = Formats('en_US');
