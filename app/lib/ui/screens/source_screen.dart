import 'package:material_ui/material_ui.dart';

import '../../state/state.dart';
import 'screen_placeholder.dart';

/// Screen 1, Source: choose the disk, partition, image file or saved session
/// to work on. Placeholder.
class SourceScreen extends StatelessWidget {
  const SourceScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final source = app.source;
    return ScreenPlaceholder(
      title: 'Source',
      purpose:
          'Choose the disk or partition that was formatted, a disk image, or '
          'a saved scan.',
      listenable: source,
      state: () => [
        if (source.isLoadingDisks) 'Looking for disks…',
        if (source.disksError != null)
          'The disks could not be listed: ${source.disksError!.message}',
        if (source.disks != null) 'Disks found: ${source.disks!.length}',
        if (source.current != null) 'Open: ${source.current!.info.path}',
      ],
    );
  }
}
