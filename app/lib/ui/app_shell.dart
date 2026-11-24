import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:material_ui/material_ui.dart';

import '../engine/engine.dart';
import '../state/state.dart';
import 'format.dart';
import 'results/results_model.dart';
import 'screens/image_screen.dart';
import 'screens/recover_screen.dart';
import 'screens/results_screen.dart';
import 'screens/scan_screen.dart';
import 'screens/source_screen.dart';
import 'strings.dart';
import 'widgets/common.dart';

/// The frame around the five screens: navigation, the permanent safety
/// notice, the progress of long requests, and what the user sees while the
/// engine is not available.
///
/// It also moves the user along when something finishes: a scan that ends
/// with results shows the Results screen.
class AppShell extends StatefulWidget {
  const AppShell({super.key, required this.app, required this.closing});

  final AppController app;

  /// `true` while the app shuts the engine down before it closes.
  final ValueListenable<bool> closing;

  @override
  State<AppShell> createState() => _AppShellState();
}

class _AppShellState extends State<AppShell> {
  late final ResultsModel _results = ResultsModel(widget.app);
  JobPhase _scanPhase = JobPhase.idle;

  AppController get app => widget.app;

  static const List<(AppScreen, IconData, IconData, String)> _destinations = [
    (AppScreen.source, Icons.storage_outlined, Icons.storage, S.navSource),
    (AppScreen.scan, Icons.radar_outlined, Icons.radar, S.navScan),
    (AppScreen.results, Icons.folder_open_outlined, Icons.folder_open, S.navResults),
    (AppScreen.recover, Icons.save_alt_outlined, Icons.save_alt, S.navRecover),
    (AppScreen.image, Icons.album_outlined, Icons.album, S.navImage),
  ];

  @override
  void initState() {
    super.initState();
    _scanPhase = app.scan.phase;
    app.scan.addListener(_onScanChanged);
  }

  @override
  void dispose() {
    app.scan.removeListener(_onScanChanged);
    _results.dispose();
    super.dispose();
  }

  /// A scan that ends with results (completed, or stopped by the user) shows
  /// them. A failed scan stays on the Scan screen, which explains it.
  void _onScanChanged() {
    final phase = app.scan.phase;
    final ended = phase == JobPhase.done && _scanPhase != JobPhase.done;
    _scanPhase = phase;
    if (!ended) return;
    final screen = app.screen.value;
    if (app.scan.result?.catalog != null && (screen == AppScreen.scan || screen == AppScreen.source)) {
      app.screen.value = AppScreen.results;
    }
  }

  Widget _screen(AppScreen screen) => switch (screen) {
    AppScreen.source => SourceScreen(app: app),
    AppScreen.scan => ScanScreen(app: app),
    AppScreen.results => ResultsScreen(app: app, model: _results),
    AppScreen.recover => RecoverScreen(app: app),
    AppScreen.image => ImageScreen(app: app),
  };

  /// The job that runs on behalf of [screen], for the badge on its
  /// navigation entry.
  static AppScreen? _screenOf(JobKind? job) => switch (job) {
    JobKind.scan => AppScreen.scan,
    JobKind.recover => AppScreen.recover,
    JobKind.carve => AppScreen.results,
    JobKind.image => AppScreen.image,
    _ => null,
  };

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: Column(
        children: [
          const SafetyNotice(),
          Expanded(
            child: ListenableBuilder(
              listenable: Listenable.merge([app.engine, app.screen, widget.closing, app.jobs]),
              builder: (context, _) {
                if (widget.closing.value) {
                  return const EmptyState(
                    icon: Icons.hourglass_bottom,
                    progress: true,
                    title: S.closingTitle,
                    text: S.closingText,
                  );
                }
                if (!app.engine.isReady) return EngineStatusView(app: app);
                final busy = _screenOf(app.jobs.value);
                return Stack(
                  children: [
                    Row(
                      children: [
                        FocusTraversalGroup(
                          child: NavigationRail(
                            selectedIndex: app.screen.value.index,
                            onDestinationSelected: (index) => app.screen.value = AppScreen.values[index],
                            labelType: NavigationRailLabelType.all,
                            minWidth: 80,
                            destinations: [
                              for (final (screen, icon, selected, label) in _destinations)
                                NavigationRailDestination(
                                  icon: Badge(isLabelVisible: busy == screen, smallSize: 8, child: Icon(icon)),
                                  selectedIcon: Badge(
                                    isLabelVisible: busy == screen,
                                    smallSize: 8,
                                    child: Icon(selected),
                                  ),
                                  label: Text(label),
                                ),
                            ],
                          ),
                        ),
                        const VerticalDivider(width: 1),
                        Expanded(
                          child: FocusTraversalGroup(
                            child: KeyedSubtree(key: ValueKey(app.screen.value), child: _screen(app.screen.value)),
                          ),
                        ),
                      ],
                    ),
                    TaskOverlay(task: app.task),
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

  static const String text = S.safetyNotice;

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
              child: Text(text, style: TextStyle(color: colors.onSecondaryContainer)),
            ),
          ],
        ),
      ),
    );
  }
}

/// While a long plain request runs (`session_load`, `geometry_set`) the
/// engine answers nothing else: a modal card with its progress keeps the
/// user from clicking into a screen that cannot react.
class TaskOverlay extends StatelessWidget {
  const TaskOverlay({super.key, required this.task});

  final ValueListenable<TaskProgress?> task;

  @override
  Widget build(BuildContext context) {
    return ValueListenableBuilder(
      valueListenable: task,
      builder: (context, progress, _) {
        if (progress == null) return const SizedBox.shrink();
        final theme = Theme.of(context);
        return Positioned.fill(
          child: ColoredBox(
            color: theme.colorScheme.scrim.withValues(alpha: 0.35),
            child: Center(
              child: Card(
                color: theme.colorScheme.surfaceContainerHigh,
                child: Padding(
                  padding: const EdgeInsets.all(24),
                  child: ConstrainedBox(
                    constraints: const BoxConstraints(maxWidth: 420),
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(S.taskTitle(progress.task), style: theme.textTheme.titleMedium),
                        const SizedBox(height: 12),
                        LinearProgressIndicator(value: progress.fraction.clamp(0, 1)),
                        const SizedBox(height: 8),
                        Text('${Formats.of(context).percent(progress.fraction)} · ${S.taskText}'),
                      ],
                    ),
                  ),
                ),
              ),
            ),
          ),
        );
      },
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
        return const EmptyState(
          icon: Icons.hourglass_empty,
          progress: true,
          title: S.startingTitle,
          text: S.startingText,
        );
      case EngineStatus.incompatible:
        return EmptyState(
          icon: Icons.error_outline,
          title: S.incompatibleTitle,
          text: S.incompatibleText(engine.engineDescription, engine.hello?.protocol),
        );
      case EngineStatus.failed:
        final exit = engine.lastExit;
        return EmptyState(
          icon: Icons.error_outline,
          title: S.engineFailedTitle,
          text: [
            engine.error?.message ?? S.engineFailedFallback,
            if (exit != null && exit.stderrTail.isNotEmpty) S.engineLastLog(exit.stderrTail.last),
            S.engineFailedReassurance,
            if (app.source.sessionPath != null) S.engineFailedSession,
          ].join('\n\n'),
          actions: [FilledButton(onPressed: () => unawaited(app.restartEngine()), child: const Text(S.engineRestart))],
        );
    }
  }
}
