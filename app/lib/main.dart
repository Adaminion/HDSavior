import 'package:material_ui/material_ui.dart';

import 'app.dart';
import 'engine/engine.dart';
import 'platform/platform_services.dart';
import 'state/state.dart';

/// Starts HDSavior with the real engine: `hdsavior-engine serve` next to
/// the app's executable, or what the environment variable `HDSAVIOR_ENGINE`
/// names.
void main() {
  WidgetsFlutterBinding.ensureInitialized();
  final controller = AppController(
    transport: ProcessEngineTransport(executable: locateEngineExecutable()),
    platform: IoPlatformServices(),
  );
  runApp(HdSaviorApp(controller: controller));
}
