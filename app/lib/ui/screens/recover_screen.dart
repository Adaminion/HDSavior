import 'package:material_ui/material_ui.dart';

import '../../state/state.dart';
import 'screen_placeholder.dart';

/// Screen 4, Recover: destination, options, progress, summary. Placeholder.
class RecoverScreen extends StatelessWidget {
  const RecoverScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final recover = app.recover;
    return ScreenPlaceholder(
      title: 'Recover',
      purpose: 'Copies the ticked files to another disk.',
      listenable: recover,
      state: () => [
        'Recovery: ${recover.phase.name}',
        if (recover.summary != null) 'Files to recover: ${recover.summary!.files}',
        if (recover.status != null) 'Result: ${recover.status!.wire}',
      ],
    );
  }
}
