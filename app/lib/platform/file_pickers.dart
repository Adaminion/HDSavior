/// The system's file and folder dialogs, behind an interface so that screens
/// can be tested without them.
library;

import 'package:file_selector/file_selector.dart' as selector;

/// Asks the user for paths. Every method returns `null` when the user
/// cancels.
///
/// The paths are what the user chose in the dialog; whether HDSavior may
/// write there is decided by the engine's destination guard, not here. (A
/// destination can also be typed, for example a UNC path: mapped network
/// drives are invisible to an elevated program.)
abstract interface class FilePickers {
  /// Asks for an existing file.
  ///
  /// [label] names the kind of file ("Disk images") and [extensions] lists
  /// its extensions without the dot (`['img', 'dd']`). "All files" is always
  /// offered as well. Without [extensions] any file can be chosen.
  Future<String?> pickFile({
    required String label,
    List<String> extensions = const [],
    String? initialDirectory,
  });

  /// Asks where to save a file ([suggestedName]: the proposed file name).
  /// Nothing is created.
  Future<String?> pickSavePath({
    required String label,
    List<String> extensions = const [],
    String? suggestedName,
    String? initialDirectory,
  });

  /// Asks for a folder.
  Future<String?> pickFolder({String? initialDirectory});
}

/// [FilePickers] with the native dialogs of the `file_selector` plugin.
final class SystemFilePickers implements FilePickers {
  const SystemFilePickers();

  static List<selector.XTypeGroup> _groups(String label, List<String> extensions) => [
    if (extensions.isNotEmpty) ...[
      selector.XTypeGroup(label: label, extensions: extensions),
      // Never lock the user out of a file with an unusual name.
      const selector.XTypeGroup(label: 'All files'),
    ],
  ];

  @override
  Future<String?> pickFile({
    required String label,
    List<String> extensions = const [],
    String? initialDirectory,
  }) async {
    final file = await selector.openFile(
      acceptedTypeGroups: _groups(label, extensions),
      initialDirectory: initialDirectory,
    );
    return file?.path;
  }

  @override
  Future<String?> pickSavePath({
    required String label,
    List<String> extensions = const [],
    String? suggestedName,
    String? initialDirectory,
  }) async {
    final location = await selector.getSaveLocation(
      acceptedTypeGroups: _groups(label, extensions),
      suggestedName: suggestedName,
      initialDirectory: initialDirectory,
      canCreateDirectories: true,
    );
    return location?.path;
  }

  @override
  Future<String?> pickFolder({String? initialDirectory}) => selector.getDirectoryPath(
    initialDirectory: initialDirectory,
    canCreateDirectories: true,
  );
}
