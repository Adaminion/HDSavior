import 'package:material_ui/material_ui.dart';

import '../../state/state.dart';
import 'screen_placeholder.dart';

/// Screen 5, Image: copy the source sector by sector to a file. Placeholder.
class ImageScreen extends StatelessWidget {
  const ImageScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final image = app.image;
    return ScreenPlaceholder(
      title: 'Image',
      purpose:
          'Copies the whole disk to a file on another disk, so that the '
          'rescue can continue from the copy.',
      listenable: image,
      state: () => [
        'Imaging: ${image.phase.name}',
        if (image.status != null) 'Result: ${image.status!.wire}',
      ],
    );
  }
}
