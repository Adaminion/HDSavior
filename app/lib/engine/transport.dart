/// How the client reaches an engine: an interface, so that tests (and
/// development runs without a real engine) can substitute a fake.
library;

/// Starts engine processes.
abstract interface class EngineTransport {
  /// What [start] launches, for error messages (for example the path of the
  /// executable).
  String get description;

  /// Starts one engine and returns the connection to it. Throws if the
  /// engine cannot be started.
  Future<EngineConnection> start();
}

/// The standard streams of one running engine.
abstract interface class EngineConnection {
  /// What the engine writes to its standard output: responses and events,
  /// one JSON object per line, UTF-8. Single subscription. Closes when the
  /// engine has closed its end.
  Stream<List<int>> get stdout;

  /// The engine's standard error: free-form log lines for humans.
  Stream<List<int>> get stderr;

  /// Writes [bytes] to the engine's standard input. Must not throw when the
  /// engine is already gone; the exit is reported through [exitCode].
  void write(List<int> bytes);

  /// Closes the engine's standard input. End of input makes the engine save
  /// a running scan's session, cancel other jobs and exit.
  Future<void> closeInput();

  /// Completes when the engine process has ended.
  Future<int> get exitCode;

  /// Ends the engine by force. A last resort: a recovery that is killed can
  /// leave one `~hds-*.part` file behind.
  void kill();
}
