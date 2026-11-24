/// The application state: everything between the engine layer and the
/// widgets.
///
/// All of it is plain `ChangeNotifier`/`ValueNotifier` from the Flutter SDK
/// (no state-management package): [AppController] creates the controllers
/// and wires them; widgets listen with `ListenableBuilder`.
library;

export 'app_controller.dart';
export 'browse_controller.dart';
export 'carve_controller.dart';
export 'catalog_controller.dart';
export 'destination_checker.dart';
export 'engine_controller.dart';
export 'image_controller.dart';
export 'job_controller.dart';
export 'recover_controller.dart';
export 'row_cache.dart';
export 'scan_controller.dart';
export 'selection_model.dart';
export 'source_controller.dart';
