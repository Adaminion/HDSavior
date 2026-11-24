/// The few things the app needs from the operating system outside the
/// engine. They sit behind this interface so that everything else runs
/// unchanged on Linux (tests, the app under Xvfb) and in widget tests.
///
/// Only `dart:io` is used: no `dart:ffi`, no Win32 calls.
library;

import 'dart:io';

/// See the library documentation.
abstract interface class PlatformServices {
  /// The directory the app proposes for scan sessions. It is not created
  /// here, and it must pass the destination guard (purpose `app_data`)
  /// before the app creates anything in it (docs/ARCHITECTURE.md, S2).
  String get defaultSessionDirectory;

  /// The path separator of this system (`\` on Windows).
  String get pathSeparator;

  /// Creates [path] and its missing parents. Call it only for a directory
  /// the destination guard has approved.
  Future<void> createDirectory(String path);

  /// Shows the folder [path] in the system's file manager. Returns whether
  /// the file manager could be started.
  Future<bool> openFolder(String path);
}

/// [PlatformServices] for the desktop systems, built on `dart:io`.
///
/// The constructor parameters replace what would be read from the running
/// process, so that the Windows behaviour can be tested on any system.
final class IoPlatformServices implements PlatformServices {
  IoPlatformServices({
    bool? isWindows,
    Map<String, String>? environment,
    Future<ProcessResult> Function(String executable, List<String> arguments)?
    run,
  }) : _isWindows = isWindows ?? Platform.isWindows,
       _environment = environment ?? Platform.environment,
       _run = run ?? Process.run;

  final bool _isWindows;
  final Map<String, String> _environment;
  final Future<ProcessResult> Function(String, List<String>) _run;

  @override
  String get pathSeparator => _isWindows ? r'\' : '/';

  String? _variable(String name) {
    final value = _environment[name];
    return value == null || value.trim().isEmpty ? null : value;
  }

  /// Windows: `%LOCALAPPDATA%\HDSavior\Sessions`, a local directory of the
  /// user on the system volume, where session files need no consent (S3).
  /// Elsewhere: `$XDG_DATA_HOME/hdsavior/sessions`, by default
  /// `~/.local/share/hdsavior/sessions`.
  @override
  String get defaultSessionDirectory {
    if (_isWindows) {
      final base =
          _variable('LOCALAPPDATA') ??
          switch (_variable('USERPROFILE')) {
            final profile? => '$profile\\AppData\\Local',
            null => r'C:\ProgramData',
          };
      return '$base\\HDSavior\\Sessions';
    }
    final base =
        _variable('XDG_DATA_HOME') ??
        '${_variable('HOME') ?? Directory.systemTemp.path}/.local/share';
    return '$base/hdsavior/sessions';
  }

  @override
  Future<void> createDirectory(String path) async {
    await Directory(path).create(recursive: true);
  }

  @override
  Future<bool> openFolder(String path) async {
    try {
      // The path is one argument of its own; no shell is involved.
      if (_isWindows) {
        // Explorer reports success through its window, not its exit code.
        await _run('explorer.exe', [path]);
        return true;
      }
      final result = await _run('xdg-open', [path]);
      return result.exitCode == 0;
    } on ProcessException {
      return false;
    }
  }
}
