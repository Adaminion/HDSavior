/// Typed, path-aware access to decoded JSON.
///
/// Everything the engine sends is read through [JsonObject], so that a message
/// which does not have the shape documented in docs/PROTOCOL.md fails with a
/// [ProtocolFormatException] that names the offending member instead of a
/// type error somewhere in the UI. Members the app does not know are ignored,
/// as the protocol requires.
library;

/// A message from the engine does not have the documented shape: a required
/// member is missing, or a member has the wrong type.
final class ProtocolFormatException implements FormatException {
  ProtocolFormatException(this.path, this.problem);

  /// Where in the message, for example `list_children.rows[3].size`.
  final String path;

  /// What is wrong there, for example `expected an integer, found a string`.
  final String problem;

  @override
  String get message => '$path: $problem';

  @override
  Object? get source => null;

  @override
  int? get offset => null;

  @override
  String toString() => 'ProtocolFormatException: $message';
}

/// Largest magnitude at which every integer is exactly representable as a
/// double (2^53).
const double _maxExactDouble = 9007199254740992.0;

String _describe(Object? value) => switch (value) {
  null => 'null',
  bool() => 'a boolean',
  int() => 'an integer',
  double() => 'a number',
  String() => 'a string',
  List<Object?>() => 'an array',
  Map<Object?, Object?>() => 'an object',
  _ => 'a ${value.runtimeType}',
};

/// Converts a decoded JSON value to an integer.
///
/// Dart's JSON decoder yields `int` for integer literals up to 2^63 - 1, which
/// is the protocol's range for byte counts and offsets. A `double` is accepted
/// only when it is integral and small enough to be exact (an encoder that
/// writes `1.0`); anything else would silently change a size or an offset.
int _toInt(Object value, String path) {
  if (value is int) return value;
  if (value is double &&
      value.isFinite &&
      value == value.truncateToDouble() &&
      value.abs() <= _maxExactDouble) {
    return value.toInt();
  }
  throw ProtocolFormatException(
    path,
    'expected an integer, found ${value is double ? 'the number $value' : _describe(value)}',
  );
}

/// One JSON object together with its position in the message.
final class JsonObject {
  JsonObject._(this._map, this.path);

  /// Wraps [value], which must be a JSON object; [path] names it in errors.
  factory JsonObject.root(Object? value, String path) {
    if (value is Map<String, Object?>) return JsonObject._(value, path);
    throw ProtocolFormatException(
      path,
      'expected an object, found ${_describe(value)}',
    );
  }

  final Map<String, Object?> _map;

  /// Position of this object in the message (for error messages).
  final String path;

  /// The underlying map, for members the typed accessors do not cover.
  Map<String, Object?> get raw => _map;

  /// Whether [key] is present and not `null`.
  bool has(String key) => _map[key] != null;

  String _at(String key) => '$path.$key';

  Object _require(String key) {
    final value = _map[key];
    if (value == null) {
      throw ProtocolFormatException(
        _at(key),
        _map.containsKey(key) ? 'must not be null' : 'is missing',
      );
    }
    return value;
  }

  // --- scalars -------------------------------------------------------------

  int integer(String key) => _toInt(_require(key), _at(key));

  int? integerOrNull(String key) {
    final value = _map[key];
    return value == null ? null : _toInt(value, _at(key));
  }

  double number(String key) {
    final value = _require(key);
    if (value is num) return value.toDouble();
    throw ProtocolFormatException(
      _at(key),
      'expected a number, found ${_describe(value)}',
    );
  }

  String string(String key) => _string(_require(key), _at(key));

  String? stringOrNull(String key) {
    final value = _map[key];
    return value == null ? null : _string(value, _at(key));
  }

  bool boolean(String key) => _bool(_require(key), _at(key));

  bool? booleanOrNull(String key) {
    final value = _map[key];
    return value == null ? null : _bool(value, _at(key));
  }

  static String _string(Object value, String path) {
    if (value is String) return value;
    throw ProtocolFormatException(
      path,
      'expected a string, found ${_describe(value)}',
    );
  }

  static bool _bool(Object value, String path) {
    if (value is bool) return value;
    throw ProtocolFormatException(
      path,
      'expected a boolean, found ${_describe(value)}',
    );
  }

  // --- objects -------------------------------------------------------------

  JsonObject object(String key) => JsonObject.root(_require(key), _at(key));

  JsonObject? objectOrNull(String key) {
    final value = _map[key];
    return value == null ? null : JsonObject.root(value, _at(key));
  }

  // --- arrays --------------------------------------------------------------

  List<Object?> _list(Object value, String path) {
    if (value is List<Object?>) return value;
    throw ProtocolFormatException(
      path,
      'expected an array, found ${_describe(value)}',
    );
  }

  /// A required array whose elements are converted with [convert], which
  /// receives each element and its path.
  List<T> list<T>(
    String key,
    T Function(Object? element, String elementPath) convert,
  ) => _convertList(_list(_require(key), _at(key)), _at(key), convert);

  /// Like [list], but `null` when the member is missing or `null`.
  List<T>? listOrNull<T>(
    String key,
    T Function(Object? element, String elementPath) convert,
  ) {
    final value = _map[key];
    if (value == null) return null;
    return _convertList(_list(value, _at(key)), _at(key), convert);
  }

  static List<T> _convertList<T>(
    List<Object?> list,
    String path,
    T Function(Object? element, String elementPath) convert,
  ) => List<T>.generate(
    list.length,
    (i) => convert(list[i], '$path[$i]'),
    growable: false,
  );

  /// A required array of objects.
  List<T> objects<T>(String key, T Function(JsonObject json) fromJson) =>
      list(key, (element, path) => fromJson(JsonObject.root(element, path)));

  /// A required array of strings.
  List<String> strings(String key) => list(key, _stringElement);

  /// An array of strings, `null` when missing or `null`.
  List<String>? stringsOrNull(String key) => listOrNull(key, _stringElement);

  /// A required array of integers.
  List<int> integers(String key) => list(key, _intElement);

  /// An array of integers, `null` when missing or `null`.
  List<int>? integersOrNull(String key) => listOrNull(key, _intElement);

  static String _stringElement(Object? element, String path) {
    if (element is String) return element;
    throw ProtocolFormatException(
      path,
      'expected a string, found ${_describe(element)}',
    );
  }

  static int _intElement(Object? element, String path) {
    if (element == null) {
      throw ProtocolFormatException(path, 'expected an integer, found null');
    }
    return _toInt(element, path);
  }
}
