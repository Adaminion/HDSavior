import 'package:material_ui/material_ui.dart';

import '../../state/state.dart';
import 'screen_placeholder.dart';

/// Screen 2, Scan: progress of the scan, "Stop and show results".
/// Placeholder.
class ScanScreen extends StatelessWidget {
  const ScanScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final scan = app.scan;
    return ScreenPlaceholder(
      title: 'Scan',
      purpose: 'Reads the disk once and collects what is left of the old files.',
      listenable: scan,
      state: () => [
        'Scan: ${scan.phase.name}',
        if (scan.progress != null)
          'Phase ${scan.progress!.phase.wire}, '
              '${((scan.progress!.fraction ?? 0) * 100).toStringAsFixed(0)} %',
        if (scan.status != null) 'Result: ${scan.status!.wire}',
      ],
    );
  }
}
