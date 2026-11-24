import 'package:material_ui/material_ui.dart';

/// Stands in for a screen that is not built yet: its name, what it will do,
/// and a few lines of live state that show the foundation at work.
///
/// The screens of docs/ARCHITECTURE.md (section 6) replace their placeholder
/// one by one; this widget disappears with the last of them.
class ScreenPlaceholder extends StatelessWidget {
  const ScreenPlaceholder({
    super.key,
    required this.title,
    required this.purpose,
    required this.listenable,
    required this.state,
  });

  final String title;

  /// What the finished screen is for.
  final String purpose;

  /// What [state] depends on.
  final Listenable listenable;

  /// Lines of live state.
  final List<String> Function() state;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.all(24),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(title, style: theme.textTheme.headlineMedium),
          const SizedBox(height: 8),
          Text(purpose, style: theme.textTheme.bodyLarge),
          const SizedBox(height: 24),
          ListenableBuilder(
            listenable: listenable,
            builder: (context, _) => Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                for (final line in state())
                  Padding(
                    padding: const EdgeInsets.only(bottom: 4),
                    child: Text(line),
                  ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}
