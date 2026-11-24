/// The typed exception for everything that can go wrong with a request.
library;

import 'enums.dart';
import 'json.dart';
import 'objects.dart';
import 'results.dart';

/// A request failed.
///
/// Either the engine answered with an error (`ok: false`; [code], [message]
/// and [data] are the members of the protocol's error object), or the client
/// could not get an answer at all. The second case uses codes of its own that
/// never appear on the wire: `engine_exited`, `engine_not_running`,
/// `engine_start_failed` and `protocol_error` (see [EngineErrorCode]).
final class EngineException implements Exception {
  EngineException(this.error, {this.command});

  /// A failure detected by the client itself.
  EngineException.client(
    EngineErrorCode code,
    String message, {
    this.command,
    Map<String, Object?>? data,
  }) : error = EngineError(code: code.wire, message: message, data: data);

  /// The error object.
  final EngineError error;

  /// The command that failed, if known.
  final String? command;

  /// The code as sent, for example `destination_refused`.
  String get code => error.code;

  /// The typed form of [code]; `unknown` for a code this version of the app
  /// does not know.
  EngineErrorCode get kind => error.kind;

  /// Plain-language explanation; may be empty.
  String get message => error.message;

  /// Optional details (see [destinationCheck]).
  Map<String, Object?>? get data => error.data;

  /// Whether the engine process itself is unavailable (it ended, was never
  /// started, or could not be started).
  bool get isEngineGone => kind.isEngineGone;

  /// For `destination_refused`: the `check_destination` result that explains
  /// the refusal. `null` for other errors or when the engine sent no data.
  late final DestinationCheck? destinationCheck = _destinationCheck();

  DestinationCheck? _destinationCheck() {
    final data = error.data;
    if (kind != EngineErrorCode.destinationRefused || data == null) return null;
    try {
      return DestinationCheck.fromJson(JsonObject.root(data, 'error.data'));
    } on ProtocolFormatException {
      return null;
    }
  }

  @override
  String toString() {
    final where = command == null ? '' : ' ($command)';
    return message.isEmpty
        ? 'EngineException: $code$where'
        : 'EngineException: $code$where: $message';
  }
}
