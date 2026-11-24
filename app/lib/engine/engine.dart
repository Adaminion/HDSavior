/// The engine layer: the protocol of docs/PROTOCOL.md as Dart types.
///
/// * [EngineClient] owns the engine process and speaks JSON lines.
/// * [EngineApi] offers one typed method per command.
/// * objects.dart, results.dart and events.dart hold the model classes.
///
/// Nothing in this layer imports Flutter, so it can be used and tested as
/// plain Dart.
library;

export 'cancel.dart';
export 'engine_api.dart';
export 'engine_client.dart';
export 'engine_locator.dart';
export 'enums.dart';
export 'errors.dart';
export 'events.dart';
export 'json.dart' show JsonObject, ProtocolFormatException;
export 'objects.dart';
export 'process_transport.dart';
export 'results.dart';
export 'transport.dart';
