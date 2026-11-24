/// Where the engine executable is.
library;

import 'dart:io';

/// Environment variable that overrides the path of the engine executable
/// (development, tests, running against a fake).
const String engineOverrideVariable = 'HDSAVIOR_ENGINE';

/// File name of the engine executable on this kind of system.
String engineExecutableName({required bool isWindows}) =>
    isWindows ? 'hdsavior-engine.exe' : 'hdsavior-engine';

/// Returns the path of the engine executable: the value of the environment
/// variable `HDSAVIOR_ENGINE` if it is set, otherwise `hdsavior-engine(.exe)`
/// next to the app's own executable (where the Windows build installs it).
///
/// The file is not checked here; a missing engine is reported when it is
/// started. The parameters exist for tests and default to the running
/// process.
String locateEngineExecutable({
  Map<String, String>? environment,
  String? appExecutable,
  bool? isWindows,
}) {
  final override = (environment ?? Platform.environment)[engineOverrideVariable];
  if (override != null && override.trim().isNotEmpty) return override.trim();

  final windows = isWindows ?? Platform.isWindows;
  final executable = appExecutable ?? Platform.resolvedExecutable;
  final name = engineExecutableName(isWindows: windows);

  // Windows accepts both separators; elsewhere a backslash is an ordinary
  // character of a file name.
  var cut = executable.lastIndexOf('/');
  if (windows) {
    final backslash = executable.lastIndexOf(r'\');
    if (backslash > cut) cut = backslash;
  }
  if (cut < 0) return name;
  return executable.substring(0, cut + 1) + name;
}
