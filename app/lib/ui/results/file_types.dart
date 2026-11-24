/// File types as the user thinks of them: groups of extensions for the type
/// filter, and an icon per row.
library;

import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../strings.dart';

/// A group of extensions offered as one filter chip.
enum TypeGroup {
  pictures([
    'jpg', 'jpeg', 'png', 'gif', 'bmp', 'tif', 'tiff', 'heic', 'heif', 'webp', 'cr2', 'cr3', 'nef', 'arw', //
    'dng', 'orf', 'rw2', 'raf', 'psd', 'svg', 'ico',
  ]),
  documents([
    'pdf', 'doc', 'docx', 'docm', 'xls', 'xlsx', 'xlsm', 'ppt', 'pptx', 'odt', 'ods', 'odp', 'rtf', 'txt', //
    'csv', 'md', 'htm', 'html', 'xml', 'json', 'pages', 'numbers', 'key', 'epub', 'one', 'pst', 'msg', 'eml',
  ]),
  music(['mp3', 'wav', 'flac', 'm4a', 'aac', 'ogg', 'oga', 'wma', 'aiff', 'aif', 'opus', 'mid', 'midi']),
  videos(['mp4', 'm4v', 'mov', 'avi', 'mkv', 'wmv', 'mpg', 'mpeg', 'mts', 'm2ts', '3gp', 'webm', 'flv', 'vob']),
  archives(['zip', '7z', 'rar', 'tar', 'gz', 'tgz', 'bz2', 'xz', 'cab', 'iso', 'vhd', 'vhdx', 'vmdk', 'img']);

  const TypeGroup(this.extensions);

  /// Lower-case extensions without the dot.
  final List<String> extensions;

  String get label => S.typeGroup(this);

  IconData get icon => switch (this) {
    TypeGroup.pictures => Icons.image_outlined,
    TypeGroup.documents => Icons.description_outlined,
    TypeGroup.music => Icons.audio_file_outlined,
    TypeGroup.videos => Icons.video_file_outlined,
    TypeGroup.archives => Icons.folder_zip_outlined,
  };

  /// The group of [ext], if any.
  static TypeGroup? of(String ext) {
    final lower = ext.toLowerCase();
    for (final group in values) {
      if (group.extensions.contains(lower)) return group;
    }
    return null;
  }

  /// The extensions of [groups] together, sorted; `null` for no groups
  /// (any type).
  static List<String>? extensionsOf(Set<TypeGroup> groups) {
    if (groups.isEmpty) return null;
    return {for (final group in groups) ...group.extensions}.toList()..sort();
  }

  /// The groups whose extensions make up exactly [exts]; `null` if [exts]
  /// is not such a union.
  static Set<TypeGroup> groupsOf(List<String>? exts) {
    if (exts == null || exts.isEmpty) return const {};
    final wanted = exts.toSet();
    return {
      for (final group in values)
        if (group.extensions.every(wanted.contains)) group,
    };
  }
}

/// The icon of a row of the tree or table.
IconData iconForRow(NodeRow row) => switch (row.kind) {
  NodeKind.dir => Icons.folder_outlined,
  NodeKind.lostDir => Icons.folder_off_outlined,
  NodeKind.virtual => Icons.folder_special_outlined,
  NodeKind.carved => Icons.find_in_page_outlined,
  _ => TypeGroup.of(row.ext)?.icon ?? Icons.insert_drive_file_outlined,
};

/// The icon of a file system (an instance root).
IconData iconForInstance(Instance instance) => switch (instance.kind) {
  InstanceKind.main => Icons.storage,
  InstanceKind.older => Icons.history,
  InstanceKind.other => Icons.dns_outlined,
  InstanceKind.embedded => Icons.inventory_2_outlined,
  InstanceKind.strays => Icons.scatter_plot_outlined,
  InstanceKind.unknown => Icons.storage,
};
