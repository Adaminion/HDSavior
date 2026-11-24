// The application shell: the permanent notice, navigation between the five
// screens, what is shown when the engine is not available, and the clean
// shutdown when the window is closed.
import 'dart:ui' show AppExitResponse;

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/app.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';
import 'package:hdsavior/ui/app_shell.dart';
import 'package:hdsavior/ui/screens/source_screen.dart';
import 'package:hdsavior/ui/strings.dart';
import 'package:material_ui/material_ui.dart';

import '../support/fake_app.dart';

void main() {
  late FakeApp fake;

  /// Lets the engine conversation run (the fake answers within microtasks)
  /// and rebuilds the widgets.
  Future<void> settle(WidgetTester tester) async {
    await tester.pump(const Duration(milliseconds: 10));
    await tester.pump(const Duration(milliseconds: 10));
  }

  Future<void> pumpApp(WidgetTester tester, {int protocol = 1, void Function(FakeApp fake)? prepare}) async {
    fake = FakeApp(protocol: protocol, settle: () => settle(tester));
    prepare?.call(fake);
    await tester.pumpWidget(HdSaviorApp(controller: fake.app));
  }

  /// The controller outlives the widgets in the real app; here both end.
  Future<void> close(WidgetTester tester) async {
    await tester.pumpWidget(const SizedBox());
    await fake.dispose();
  }

  Finder destination(String label) => find.descendant(of: find.byType(NavigationRail), matching: find.text(label));

  testWidgets('starting: the notice is there from the first frame, then the screens', (tester) async {
    await pumpApp(tester);
    expect(find.text(SafetyNotice.text), findsOneWidget);
    expect(find.text('Starting…'), findsOneWidget);
    expect(find.byType(NavigationRail), findsNothing);

    await settle(tester);
    expect(fake.app.engine.isReady, isTrue);
    expect(find.text(SafetyNotice.text), findsOneWidget);
    expect(find.text('Starting…'), findsNothing);
    final rail = tester.widget<NavigationRail>(find.byType(NavigationRail));
    expect(rail.destinations, hasLength(5));
    for (final label in ['Source', 'Scan', 'Results', 'Recover', 'Image']) {
      expect(destination(label), findsOneWidget);
    }
    // The first screen shows what the engine reported.
    expect(find.byType(DiskCard), findsNWidgets(7));
    expect(tester.widget<MaterialApp>(find.byType(MaterialApp)).title, 'HDSavior');
    await close(tester);
  });

  testWidgets('the notice tells the user not to use the disk and that it is never written to', (tester) async {
    expect(SafetyNotice.text, contains('never writes'));
    expect(SafetyNotice.text, contains('Do not use that disk'));
    await pumpApp(tester);
    await settle(tester);
    for (final screen in AppScreen.values) {
      fake.app.screen.value = screen;
      await tester.pump();
      expect(find.text(SafetyNotice.text), findsOneWidget, reason: 'on the ${screen.name} screen');
    }
    await close(tester);
  });

  testWidgets('navigation: five screens, by tap and from the controller', (tester) async {
    await pumpApp(tester);
    await settle(tester);
    // Nothing is open yet: each screen says what it is waiting for.
    final titles = {
      AppScreen.source: S.sourceTitle,
      AppScreen.scan: S.scanNoSourceTitle,
      AppScreen.results: S.noCatalogTitle,
      AppScreen.recover: S.recoverNoCatalogTitle,
      AppScreen.image: S.imageNoSourceTitle,
    };
    expect(find.text(titles[AppScreen.source]!), findsOneWidget);

    for (final (label, screen) in [
      ('Results', AppScreen.results),
      ('Image', AppScreen.image),
      ('Scan', AppScreen.scan),
      ('Recover', AppScreen.recover),
      ('Source', AppScreen.source),
    ]) {
      await tester.tap(destination(label));
      await tester.pump();
      expect(fake.app.screen.value, screen);
      expect(find.text(titles[screen]!), findsOneWidget);
      expect(
        titles.values.where((t) => find.text(t).evaluate().isNotEmpty),
        hasLength(1),
        reason: 'one screen at a time',
      );
    }

    fake.app.screen.value = AppScreen.scan;
    await tester.pump();
    expect(find.text(titles[AppScreen.scan]!), findsOneWidget);
    expect(tester.widget<NavigationRail>(find.byType(NavigationRail)).selectedIndex, AppScreen.scan.index);
    await close(tester);
  });

  testWidgets('a scan that ends with results shows the Results screen; a failed one stays', (tester) async {
    await pumpApp(tester);
    await settle(tester);
    await fake.openDisk();
    await fake.run(() => fake.app.scan.start());
    fake.app.screen.value = AppScreen.scan;
    await tester.pump();
    await fake.step(3);
    expect(find.text(S.phaseTitle(ScanPhase.sweep)), findsOneWidget);
    await fake.finishJob();
    expect(fake.app.screen.value, AppScreen.results);
    expect(find.text(S.instanceName(fake.app.catalog.info!.mainInstance!)), findsWidgets);

    // A failure is explained where the scan was watched.
    fake.script.scanFailAtStep = 2;
    await fake.run(() => fake.app.scan.start());
    fake.app.screen.value = AppScreen.scan;
    await fake.finishJob();
    expect(fake.app.screen.value, AppScreen.scan);
    expect(find.text(S.sourceLostTitle), findsOneWidget);
    await close(tester);
  });

  testWidgets('a long request shows its progress over the screens', (tester) async {
    await pumpApp(tester, prepare: (fake) => fake.script.taskTicks = 3);
    await settle(tester);
    fake.app.screen.value = AppScreen.source;
    final load = fake.app.source.loadSession(r'D:\scans\big.hdsc');
    load.ignore();
    await settle(tester);
    expect(find.text(S.taskTitle(TaskKind.sessionLoad)), findsNothing, reason: 'no progress reported yet');
    fake.engine.advance();
    await settle(tester);
    expect(find.text(S.taskTitle(TaskKind.sessionLoad)), findsOneWidget);
    expect(find.textContaining('33 %'), findsOneWidget);
    for (var i = 0; i < 5; i++) {
      fake.engine.advance();
      await settle(tester);
    }
    await load;
    await settle(tester);
    expect(find.text(S.taskTitle(TaskKind.sessionLoad)), findsNothing);
    await close(tester);
  });

  testWidgets('an engine that cannot be started: the reason, reassurance, and a way to try again', (tester) async {
    await pumpApp(
      tester,
      prepare: (fake) => fake.transport.startError = StateError('hdsavior-engine.exe: file not found'),
    );
    await settle(tester);
    expect(find.text('The disk engine is not running'), findsOneWidget);
    expect(find.textContaining('hdsavior-engine.exe: file not found'), findsOneWidget);
    expect(find.textContaining('Nothing on your disk was changed.'), findsOneWidget);
    expect(find.text(SafetyNotice.text), findsOneWidget);
    expect(find.byType(NavigationRail), findsNothing);

    fake.transport.startError = null;
    await tester.tap(find.text('Start the engine again'));
    await settle(tester);
    expect(find.byType(NavigationRail), findsOneWidget);
    expect(find.byType(DiskCard), findsNWidgets(7));
    await close(tester);
  });

  testWidgets('an engine of another protocol version is not used', (tester) async {
    await pumpApp(tester, protocol: 7);
    await settle(tester);
    expect(find.text('The disk engine does not match this app'), findsOneWidget);
    expect(find.textContaining('protocol version 7'), findsOneWidget);
    expect(find.byType(NavigationRail), findsNothing);
    await close(tester);
  });

  testWidgets('a crash replaces the screens with an explanation; a restart brings them back', (tester) async {
    await pumpApp(tester);
    await settle(tester);
    fake.engine.crash(exitCode: 101);
    await settle(tester);
    expect(find.text('The disk engine is not running'), findsOneWidget);
    expect(find.textContaining('exit code 101'), findsOneWidget);
    expect(find.textContaining('simulated crash'), findsOneWidget, reason: "the last line of the engine's log");
    expect(find.byType(NavigationRail), findsNothing);

    await tester.tap(find.text('Start the engine again'));
    await settle(tester);
    expect(find.byType(NavigationRail), findsOneWidget);
    expect(fake.transport.starts, 2);
    await close(tester);
  });

  testWidgets('closing the window shuts the engine down first', (tester) async {
    await pumpApp(tester);
    await settle(tester);
    final engine = fake.engine;
    final response = tester.binding.handleRequestAppExit();
    await settle(tester);
    expect(find.text('Closing HDSavior…'), findsOneWidget);
    expect(find.byType(NavigationRail), findsNothing);
    expect(await response, AppExitResponse.exit);
    expect(engine.requests.last.command, 'shutdown');
    expect(engine.hasExited, isTrue);
    expect(await engine.exitCode, 0, reason: 'a clean exit, not a kill');
    await close(tester);
  });

  testWidgets('closing during a scan lets the engine save the session', (tester) async {
    await pumpApp(tester);
    await settle(tester);
    await fake.openDisk();
    await fake.run(() => fake.app.scan.start(sessionPath: r'C:\sessions\interrupted.hdsc'));
    await fake.step(3);
    final response = tester.binding.handleRequestAppExit();
    await settle(tester);
    expect(await response, AppExitResponse.exit);
    expect(fake.transport.world.savedSessions[r'C:\sessions\interrupted.hdsc']!.bytesDone, greaterThan(0));
    expect(fake.app.scan.phase, JobPhase.done);
    // The engine ends the scan `stopped`, without a catalog, before it exits.
    expect(fake.app.scan.status, JobStatus.stopped);
    expect(fake.app.scan.failure, isNull);
    expect(fake.app.catalog.hasCatalog, isFalse);
    await close(tester);
  });
}
