/// HDSavior with the fake engine instead of `hdsavior-engine`: for working on
/// the screens without a disk, on any system.
///
///     flutter run -d windows -t test/fake_engine/run_app.dart
///
/// (On Linux: in a scratch copy that has a `linux/` folder, see README.md.)
/// The fake offers seven disks, scans in a few seconds and finds a tree of
/// 300,000 nodes; fake_world.dart lists the names that trigger refusals and
/// errors.
library;

import 'package:hdsavior/app.dart';
import 'package:hdsavior/platform/file_pickers.dart';
import 'package:hdsavior/state/state.dart';
import 'package:material_ui/material_ui.dart';

import '../support/fake_platform.dart';
import 'fake_engine.dart';
import 'fake_transport.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  final controller = AppController(
    transport: FakeEngineTransport(
      options: const FakeEngineOptions(mainNodes: 300000, tick: Duration(milliseconds: 250)),
      script: FakeScript()
        ..scanSteps = 24
        ..recoverSteps = 20
        ..carveSteps = 12
        ..imageSteps = 20
        ..taskTicks = 4,
    ),
    // The real dialogs: any file or folder can be picked, the fake engine
    // only looks at the names.
    pickers: const SystemFilePickers(),
    platform: InertPlatformServices(),
  );
  runApp(HdSaviorApp(controller: controller));
}
