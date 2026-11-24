import 'package:material_ui/material_ui.dart';

import 'app.dart';
import 'engine/engine.dart';
import 'platform/platform_services.dart';
import 'state/state.dart';
import 'ui/format.dart';

/// Starts HDSavior with the real engine: `hdsavior-engine serve` next to
/// the app's executable, or, in a debug or profile build, what the
/// environment variable `HDSAVIOR_ENGINE` names.
Future<void> main() async {
  final binding = WidgetsFlutterBinding.ensureInitialized();
  // Dates and numbers in the user's regional format (the text is English).
  final formats = await Formats.forLocale(binding.platformDispatcher.locale.toString());
  final controller = AppController(
    transport: ProcessEngineTransport(executable: locateEngineExecutable()),
    platform: IoPlatformServices(),
  );
  runApp(HdSaviorApp(controller: controller, formats: formats));
}
