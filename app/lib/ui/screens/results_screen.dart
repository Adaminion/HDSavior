import 'package:material_ui/material_ui.dart';

import '../../state/state.dart';
import 'screen_placeholder.dart';

/// Screen 3, Results: folder tree, file table, search, filters, selection.
/// Placeholder.
class ResultsScreen extends StatelessWidget {
  const ResultsScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final catalog = app.catalog;
    return ScreenPlaceholder(
      title: 'Results',
      purpose: 'Browse the files that were found and tick the ones to recover.',
      listenable: Listenable.merge([catalog, catalog.selection]),
      state: () => [
        if (catalog.info == null) 'Nothing has been scanned yet.',
        if (catalog.info != null) ...[
          'File systems found: ${catalog.info!.instances.length}',
          'Files and folders: ${catalog.info!.nodes}',
          'Ticked for recovery: '
              '${catalog.selection.isEmpty ? 'nothing' : 'something'}',
        ],
      ],
    );
  }
}
