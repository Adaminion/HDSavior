/// Stand-ins for the operating system in tests and when the app runs
/// against the fake engine: nothing here touches the file system or shows a
/// dialog.
library;

import 'package:hdsavior/platform/file_pickers.dart';
import 'package:hdsavior/platform/platform_services.dart';

/// A [PlatformServices] that touches nothing and records what it was asked
/// to do.
final class InertPlatformServices implements PlatformServices {
  /// The default [defaultSessionDirectory].
  static const String windowsSessionDirectory = r'C:\Users\Rescuer\AppData\Local\HDSavior\Sessions';

  InertPlatformServices({
    this.defaultSessionDirectory = windowsSessionDirectory,
    this.pathSeparator = r'\',
    this.programDirectory = r'C:\Program Files\HDSavior',
  });

  @override
  final String? programDirectory;

  @override
  final String defaultSessionDirectory;

  @override
  final String pathSeparator;

  /// The directories [createDirectory] was asked to create.
  final List<String> createdDirectories = [];

  /// The folders [openFolder] was asked to show.
  final List<String> openedFolders = [];

  /// The files [openFile] was asked to open.
  final List<String> openedFiles = [];

  /// What [openFolder] and [openFile] answer: `false` pretends that no file
  /// manager could be started.
  bool canOpen = true;

  @override
  Future<void> createDirectory(String path) async => createdDirectories.add(path);

  @override
  Future<bool> openFolder(String path) async {
    openedFolders.add(path);
    return canOpen;
  }

  @override
  Future<bool> openFile(String path) async {
    openedFiles.add(path);
    return canOpen;
  }
}

/// [FilePickers] that answers from a list instead of showing dialogs.
final class ScriptedFilePickers implements FilePickers {
  /// The answers of the next dialogs, in order; `null` is "cancelled". When
  /// the list is empty every dialog is cancelled.
  final List<String?> answers = [];

  /// The dialogs that were asked for: `pickFile`, `pickSavePath` or
  /// `pickFolder`, followed by a description of the arguments.
  final List<String> requests = [];

  String? _next() => answers.isEmpty ? null : answers.removeAt(0);

  @override
  Future<String?> pickFile({
    required String label,
    List<String> extensions = const [],
    String? initialDirectory,
  }) async {
    requests.add('pickFile $label $extensions');
    return _next();
  }

  @override
  Future<String?> pickSavePath({
    required String label,
    List<String> extensions = const [],
    String? suggestedName,
    String? initialDirectory,
  }) async {
    requests.add('pickSavePath $label $extensions $suggestedName');
    return _next();
  }

  @override
  Future<String?> pickFolder({String? initialDirectory}) async {
    requests.add('pickFolder');
    return _next();
  }
}
