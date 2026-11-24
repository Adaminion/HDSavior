// Every screen in every important state, at the largest and the smallest
// window size: nothing may overflow or throw while it is laid out (a
// RenderFlex overflow fails the test). With HDSAVIOR_SCREENSHOTS=<dir> the
// same states are rendered with real fonts, in light and dark, to PNG files
// for looking at them (see README.md, "Screenshots").
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/state/state.dart';
import 'package:material_ui/material_ui.dart';

import 'ui_harness.dart';

typedef Scenario = Future<void> Function(WidgetTester tester, FakeApp fake);

Future<void> _scanned(WidgetTester tester, FakeApp fake) async {
  await fake.scan(sessionPath: r'C:\Users\Rescuer\AppData\Local\HDSavior\Sessions\scan.hdsc');
  fake.app.screen.value = AppScreen.results;
  await settle(tester);
  await pumpUntil(tester, find.text('Documents'));
}

final Map<String, Scenario> scenarios = {
  'source-disks': (tester, fake) async {},
  'source-opened': (tester, fake) async {
    await fake.openDisk();
    await tester.pump();
  },
  'source-system-disk': (tester, fake) async {
    await fake.run(() => fake.app.source.openDisk(fake.app.source.disks![0]));
    await tester.pump();
  },
  'scan-running': (tester, fake) async {
    fake.script
      ..scanBadBytesPerStep = 4096
      ..scanWarnings = [
        {'code': 'live_volume_incomplete', 'message': 'The bitmap of the new volume could not be read.'},
      ];
    await fake.openDisk();
    await fake.run(
      () => fake.app.scan.start(sessionPath: r'C:\Users\Rescuer\AppData\Local\HDSavior\Sessions\scan.hdsc'),
    );
    fake.app.screen.value = AppScreen.scan;
    await fake.step(4);
  },
  'scan-source-lost': (tester, fake) async {
    fake.script.scanFailAtStep = 3;
    await fake.openDisk();
    await fake.run(() => fake.app.scan.start(sessionPath: r'C:\sessions\scan.hdsc'));
    fake.app.screen.value = AppScreen.scan;
    await fake.finishJob();
  },
  'results': (tester, fake) async {
    await _scanned(tester, fake);
    final root = fake.app.catalog.info!.mainInstance!.root;
    fake.app.browse.open(FolderLocation(fake.landmarks.documents, [root]));
    fake.app.selection.setChecked(fake.landmarks.flagSamples, [root], true);
    await settle(tester);
    await settle(tester);
  },
  'results-file-details': (tester, fake) async {
    await _scanned(tester, fake);
    final landmarks = fake.landmarks;
    fake.app.browse.open(FolderLocation(landmarks.previewSamples, [fake.app.catalog.info!.mainInstance!.root]));
    await settle(tester);
    await pumpUntil(tester, find.textContaining('.jpg'));
    await tester.tap(find.textContaining('.jpg').first);
    await settle(tester);
    await settle(tester);
  },
  'results-search': (tester, fake) async {
    await _scanned(tester, fake);
    await tester.enterText(find.byType(TextField).first, 'img');
    await tester.pump(const Duration(milliseconds: 400));
    await settle(tester);
  },
  'results-notices': (tester, fake) async {
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\partial-mixed.hdsc'));
    fake.app.screen.value = AppScreen.results;
    await settle(tester);
  },
  'results-browse-only': (tester, fake) async {
    await fake.run(() => fake.app.source.loadSession(r'D:\scans\detached.hdsc'));
    fake.app.screen.value = AppScreen.results;
    await settle(tester);
  },
  'results-geometry': (tester, fake) async {
    await _scanned(tester, fake);
    await tester.tap(find.textContaining('Disk layout').first);
    await settle(tester);
  },
  'recover-form': (tester, fake) async {
    await _scanned(tester, fake);
    fake.app.selection.setChecked(fake.landmarks.documents, [fake.app.catalog.info!.mainInstance!.root], true);
    fake.app.screen.value = AppScreen.recover;
    await settle(tester);
    await tester.enterText(find.byKey(const ValueKey('recover-destination')), r'F:\Rescued');
    await tester.testTextInput.receiveAction(TextInputAction.done);
    await settle(tester);
  },
  'recover-same-disk': (tester, fake) async {
    await _scanned(tester, fake);
    fake.app.selection.setChecked(fake.landmarks.documents, [fake.app.catalog.info!.mainInstance!.root], true);
    fake.app.screen.value = AppScreen.recover;
    await settle(tester);
    await tester.enterText(find.byKey(const ValueKey('recover-destination')), r'S:\Rescued');
    await tester.testTextInput.receiveAction(TextInputAction.done);
    await settle(tester);
  },
  'recover-running': (tester, fake) async {
    await _scanned(tester, fake);
    fake.app.selection.setChecked(fake.landmarks.documents, [fake.app.catalog.info!.mainInstance!.root], true);
    await settle(tester);
    await fake.run(() => fake.app.recover.start(dest: r'F:\Rescued'));
    fake.app.screen.value = AppScreen.recover;
    await fake.step(3);
  },
  'recover-done': (tester, fake) async {
    await _scanned(tester, fake);
    fake.app.selection.setChecked(fake.landmarks.documents, [fake.app.catalog.info!.mainInstance!.root], true);
    await settle(tester);
    await fake.run(() => fake.app.recover.start(dest: r'F:\Rescued'));
    fake.app.screen.value = AppScreen.recover;
    await fake.finishJob();
  },
  'image-form': (tester, fake) async {
    await fake.openDisk();
    fake.app.screen.value = AppScreen.image;
    await settle(tester);
    await tester.enterText(find.byKey(const ValueKey('image-destination')), r'F:\Images\disk1.img');
    await tester.testTextInput.receiveAction(TextInputAction.done);
    await settle(tester);
  },
  'image-running': (tester, fake) async {
    fake.script.imageBadBytes = 1 << 20;
    await fake.openDisk();
    await fake.run(() => fake.app.image.start(dest: r'F:\Images\disk1.img'));
    fake.app.screen.value = AppScreen.image;
    await fake.step(3);
  },
  'engine-crashed': (tester, fake) async {
    fake.engine.crash(exitCode: 101);
    await settle(tester);
  },
};

void main() {
  final pictures = screenshotDirectory != null;
  for (final MapEntry(key: name, value: scenario) in scenarios.entries) {
    for (final (sizeName, size) in [('wide', wideWindow), ('minimum', minimumWindow)]) {
      for (final brightness in [Brightness.light, if (pictures) Brightness.dark]) {
        testWidgets('$name, $sizeName window, ${brightness.name}', (tester) async {
          if (pictures) await tester.runAsync(loadRealFonts);
          final fake = await pumpApp(tester, size: size, brightness: brightness);
          await scenario(tester, fake);
          await settle(tester);
          await capture(tester, '$name-$sizeName-${brightness.name}');
          final problem = tester.takeException();
          expect(problem, isNull, reason: problem is FlutterError ? problem.toStringDeep() : null);
          await closeApp(tester, fake);
        });
      }
    }
  }
}
