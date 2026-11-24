import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:material_ui/material_ui.dart';

import '../state/state.dart';
import 'screens/image_screen.dart';
import 'screens/recover_screen.dart';
import 'screens/results_screen.dart';
import 'screens/scan_screen.dart';
import 'screens/source_screen.dart';

/// The frame around the five screens: navigation, the permanent safety
/// notice, and what the user sees while the engine is not available.
class AppShell extends StatelessWidget {
  const AppShell({super.key, required this.app, required this.closing});

  final AppController app;

  /// `true` while the app shuts the engine down before it closes.
  final ValueListenable<bool> closing;

  static const List<(AppScreen, IconData, String)> _destinations = [
    (AppScreen.source, Icons.storage_outlined, 'Source'),
    (AppScreen.scan, Icons.radar_outlined, 'Scan'),
    (AppScreen.results, Icons.folder_open_outlined, 'Results'),
    (AppScreen.recover, Icons.save_alt_outlined, 'Recover'),
    (AppScreen.image, Icons.album_outlined, 'Image'),
  ];

  Widget _screen(AppScreen screen) => switch (screen) {
    AppScreen.source => SourceScreen(app: app),
    AppScreen.scan => ScanScreen(app: app),
    AppScreen.results => ResultsScreen(app: app),
    AppScreen.recover => RecoverScreen(app: app),
    AppScreen.image => ImageScreen(app: app),
  };

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: Column(
        children: [
          const SafetyNotice(),
          Expanded(
            child: ListenableBuilder(
              listenable: Listenable.merge([app.engine, app.screen, closing]),
              builder: (context, _) {
                if (closing.value) {
                  return const _Centered(
                    icon: Icons.hourglass_bottom,
                    title: 'Closing HDSavior…',
                    text: 'Saving the scan and stopping the engine.',
                  );
                }
                if (!app.engine.isReady) return EngineStatusView(app: app);
                return Row(
                  children: [
                    NavigationRail(
                      selectedIndex: app.screen.value.index,
                      onDestinationSelected: (index) =>
                          app.screen.value = AppScreen.values[index],
                      labelType: NavigationRailLabelType.all,
                      destinations: [
                        for (final (_, icon, label) in _destinations)
                          NavigationRailDestination(
                            icon: Icon(icon),
                            label: Text(label),
                          ),
                      ],
                    ),
                    const VerticalDivider(width: 1),
                    Expanded(child: _screen(app.screen.value)),
                  ],
                );
              },
            ),
          ),
        ],
      ),
    );
  }
}

/// The permanent notice of docs/ARCHITECTURE.md (sections 2 and 6): the disk
/// being rescued is never written to, and the user should stop using it.
class SafetyNotice extends StatelessWidget {
  const SafetyNotice({super.key});

  static const String text =
      'HDSavior only reads from the disk you are rescuing and never writes '
      'to it. Do not use that disk for anything else until your files are '
      'safe: every new file on it can destroy the old ones for good.';

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return Material(
      color: colors.secondaryContainer,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
        child: Row(
          children: [
            Icon(Icons.shield_outlined, color: colors.onSecondaryContainer),
            const SizedBox(width: 12),
            Expanded(
              child: Text(
                text,
                style: TextStyle(color: colors.onSecondaryContainer),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// Shown instead of the screens while the engine is starting, has failed or
/// is incompatible.
class EngineStatusView extends StatelessWidget {
  const EngineStatusView({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final engine = app.engine;
    switch (engine.status) {
      case EngineStatus.stopped:
      case EngineStatus.starting:
      case EngineStatus.ready:
        return const _Centered(
          progress: true,
          title: 'Starting…',
          text: 'HDSavior is starting its disk engine.',
        );
      case EngineStatus.incompatible:
        return _Centered(
          icon: Icons.error_outline,
          title: 'The disk engine does not match this app',
          text:
              'The engine (${engine.engineDescription}) speaks protocol '
              'version ${engine.hello?.protocol}, which this version of '
              'HDSavior does not understand. Install HDSavior again.',
        );
      case EngineStatus.failed:
        final exit = engine.lastExit;
        return _Centered(
          icon: Icons.error_outline,
          title: 'The disk engine is not running',
          text: [
            engine.error?.message ?? 'The engine could not be started.',
            if (exit != null && exit.stderrTail.isNotEmpty)
              exit.stderrTail.last,
            'Nothing on your disk was changed.',
          ].join('\n\n'),
          action: FilledButton(
            onPressed: () => unawaited(app.restartEngine()),
            child: const Text('Start the engine again'),
          ),
        );
    }
  }
}

class _Centered extends StatelessWidget {
  const _Centered({
    this.icon,
    this.progress = false,
    required this.title,
    required this.text,
    this.action,
  });

  final IconData? icon;
  final bool progress;
  final String title;
  final String text;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 560),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (progress) const CircularProgressIndicator(),
            if (icon != null) Icon(icon, size: 48, color: theme.colorScheme.primary),
            const SizedBox(height: 16),
            Text(title, style: theme.textTheme.headlineSmall, textAlign: TextAlign.center),
            const SizedBox(height: 8),
            Text(text, textAlign: TextAlign.center),
            if (action != null) ...[const SizedBox(height: 24), action!],
          ],
        ),
      ),
    );
  }
}
