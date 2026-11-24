/// Where the engine executable is.
library;

import 'dart:io';

/// Environment variable that overrides the path of the engine executable
/// (development, tests, running against a fake). Debug and profile builds
/// only; see [locateEngineExecutable].
const String engineOverrideVariable = 'HDSAVIOR_ENGINE';

/// File name of the engine executable on this kind of system.
String engineExecutableName({required bool isWindows}) =>
    isWindows ? 'hdsavior-engine.exe' : 'hdsavior-engine';

/// Whether this is a release build (`flutter build --release`).
const bool _releaseBuild = bool.fromEnvironment('dart.vm.product');

/// Returns the path of the engine executable: `hdsavior-engine(.exe)` next
/// to the app's own executable (where the Windows build installs it). In a
/// debug or profile build, the value of the environment variable
/// `HDSAVIOR_ENGINE` wins if it is set.
///
/// A release build ignores the variable: the app runs elevated (it asks for
/// administrator rights to read disks), and any program of the user could
/// set a user environment variable without elevation and so have its own
/// executable started with the administrator token and raw disk access.
///
/// The file is not checked here; a missing engine is reported when it is
/// started. The parameters exist for tests and default to the running
/// process and build ([allowOverride]: not in a release build).
String locateEngineExecutable({
  Map<String, String>? environment,
  String? appExecutable,
  bool? isWindows,
  bool allowOverride = !_releaseBuild,
}) {
  if (allowOverride) {
    final override = (environment ?? Platform.environment)[engineOverrideVariable];
    if (override != null && override.trim().isNotEmpty) return override.trim();
  }

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
