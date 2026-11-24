/// Helpers for widget tests of the screens: the whole app on the fake
/// engine, at a chosen window size and brightness, with real fonts when
/// pictures are taken.
library;

import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/foundation.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/app.dart';
import 'package:material_ui/material_ui.dart';

import '../support/fake_app.dart';

export '../support/fake_app.dart';

/// The largest window of the screenshots.
const Size wideWindow = Size(1400, 900);

/// The client area of the smallest window: the Windows runner allows
/// 960 x 600 for the window *including* its frame (WM_GETMINMAXINFO), which
/// leaves about 944 x 561 for the app.
const Size minimumWindow = Size(944, 560);

/// Lets the fake engine answer (it does so within microtasks) and rebuilds
/// the widgets.
Future<void> settle(WidgetTester tester) async {
  await tester.pump(const Duration(milliseconds: 10));
  await tester.pump(const Duration(milliseconds: 10));
}

/// Waits until [finder] finds something (rows arrive page by page).
Future<void> pumpUntil(WidgetTester tester, Finder finder, {int tries = 50}) async {
  for (var i = 0; i < tries && finder.evaluate().isEmpty; i++) {
    await tester.pump(const Duration(milliseconds: 20));
  }
}

final GlobalKey _boundary = GlobalKey(debugLabel: 'screenshot');

/// Starts the app on a fake engine in a window of [size]; the engine has
/// started and listed its disks when this returns.
Future<FakeApp> pumpApp(
  WidgetTester tester, {
  Size size = wideWindow,
  Brightness brightness = Brightness.light,
  int mainNodes = 3000,
  bool elevated = true,
  String os = 'windows',
  int pageSize = 200,
  void Function(FakeApp fake)? prepare,
  bool start = true,
}) async {
  tester.view.physicalSize = size;
  tester.view.devicePixelRatio = 1;
  tester.platformDispatcher.platformBrightnessTestValue = brightness;
  addTearDown(tester.view.reset);
  addTearDown(tester.platformDispatcher.clearPlatformBrightnessTestValue);
  final fake = FakeApp(mainNodes: mainNodes, elevated: elevated, pageSize: pageSize, settle: () => settle(tester));
  prepare?.call(fake);
  await tester.pumpWidget(
    RepaintBoundary(
      key: _boundary,
      child: HdSaviorApp(controller: fake.app),
    ),
  );
  if (start) await settle(tester);
  return fake;
}

/// Unmounts the app and shuts the fake engine down.
Future<void> closeApp(WidgetTester tester, FakeApp fake) async {
  await tester.pumpWidget(const SizedBox());
  await fake.dispose();
}

/// Where screenshots go: the directory named by `HDSAVIOR_SCREENSHOTS`;
/// `null` when pictures are not wanted (the normal test run).
final String? screenshotDirectory = Platform.environment['HDSAVIOR_SCREENSHOTS'];

bool _fontsLoaded = false;

/// Loads Roboto (as the app's font on every platform, including the
/// `Segoe UI` that the Windows typography asks for), the Material icons and
/// a monospace font, so that screenshots show real text instead of the
/// test font's boxes.
Future<void> loadRealFonts() async {
  if (_fontsLoaded) return;
  _fontsLoaded = true;
  final root = Platform.environment['FLUTTER_ROOT'] ?? '/opt/flutter';
  final dir = '$root/bin/cache/artifacts/material_fonts';
  Future<ByteData> read(String path) async => ByteData.sublistView(await File(path).readAsBytes());
  for (final family in ['Roboto', 'Segoe UI']) {
    final loader = FontLoader(family);
    for (final file in ['Regular', 'Medium', 'Bold', 'Italic', 'MediumItalic', 'Light']) {
      loader.addFont(read('$dir/Roboto-$file.ttf'));
    }
    await loader.load();
  }
  await (FontLoader('MaterialIcons')..addFont(read('$dir/MaterialIcons-Regular.otf'))).load();
  const mono = '/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf';
  if (File(mono).existsSync()) await (FontLoader('Consolas')..addFont(read(mono))).load();
}

/// Writes the window as `<name>.png` into [screenshotDirectory] (nothing
/// when it is not set).
Future<void> capture(WidgetTester tester, String name) async {
  final directory = screenshotDirectory;
  if (directory == null) return;
  // Let dialogs, labels and buttons finish their animations.
  for (var i = 0; i < 10; i++) {
    await tester.pump(const Duration(milliseconds: 50));
  }
  final boundary = _boundary.currentContext!.findRenderObject()! as RenderRepaintBoundary;
  await tester.runAsync(() async {
    final image = await boundary.toImage(pixelRatio: 1);
    final png = await image.toByteData(format: ui.ImageByteFormat.png);
    image.dispose();
    await Directory(directory).create(recursive: true);
    await File('$directory/$name.png').writeAsBytes(png!.buffer.asUint8List());
  });
  debugPrint('screenshot: $directory/$name.png');
}
