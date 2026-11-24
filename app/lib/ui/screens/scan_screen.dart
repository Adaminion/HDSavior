import 'dart:async';

import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import '../flows.dart';
import '../format.dart';
import '../strings.dart';
import '../theme.dart';
import '../widgets/common.dart';
import '../widgets/errors.dart';

/// Screen 2, Scan: the one pass over the disk, its progress, and the choice
/// to stop early and look at what was found.
class ScanScreen extends StatelessWidget {
  const ScanScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.scan, app.source, app.catalog, app.jobs]),
      builder: (context, _) {
        final scan = app.scan;
        final Widget body;
        if (scan.isActive) {
          body = _Running(app: app);
        } else if (scan.phase == JobPhase.done) {
          body = _Finished(app: app);
        } else {
          return _Idle(app: app);
        }
        return ScreenFrame(title: S.scanTitle, purpose: S.scanPurpose, child: body);
      },
    );
  }
}

/// No scan has run (in this session of the app).
class _Idle extends StatelessWidget {
  const _Idle({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final current = app.source.current;
    final catalog = app.catalog.info;
    if (current == null) {
      return EmptyState(
        icon: Icons.radar_outlined,
        title: S.scanNoSourceTitle,
        text: S.scanNoSourceText,
        actions: [
          FilledButton(onPressed: () => app.screen.value = AppScreen.source, child: const Text(S.chooseSource)),
        ],
      );
    }
    if (catalog != null && !catalog.scanComplete && current.attached) {
      return EmptyState(
        icon: Icons.pause_circle_outline,
        title: S.scanPausedTitle,
        text: S.scanPausedText,
        actions: [
          FilledButton(
            onPressed: app.jobs.isBusy ? null : () => resumeScan(context, app),
            child: const Text(S.resumeScan),
          ),
          OutlinedButton(onPressed: () => app.screen.value = AppScreen.results, child: const Text(S.showResults)),
        ],
      );
    }
    if (catalog != null) {
      return EmptyState(
        icon: Icons.task_alt,
        title: S.scanHasResultsTitle,
        text: S.scanHasResultsText,
        actions: [
          FilledButton(onPressed: () => app.screen.value = AppScreen.results, child: const Text(S.showResults)),
        ],
      );
    }
    return EmptyState(
      icon: Icons.radar_outlined,
      title: S.scanReadyTitle,
      text: S.scanReadyText,
      actions: [
        FilledButton(onPressed: () => app.screen.value = AppScreen.source, child: const Text(S.chooseWhatToScan)),
      ],
    );
  }
}

/// The scan runs (or is being started or stopped).
class _Running extends StatelessWidget {
  const _Running({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final scan = app.scan;
    final progress = scan.progress;
    final phase = progress?.phase;
    final analyzing = phase == ScanPhase.analyzing;
    final fraction = progress?.fraction;
    final status = StatusColors.from(context);

    final children = <Widget>[
      PhaseSteps(phase: phase),
      Card(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Row(
                children: [
                  Expanded(
                    child: Text(switch (scan.phase) {
                      JobPhase.starting => S.scanStarting,
                      JobPhase.cancelling => S.scanStopping,
                      _ => S.phaseTitle(phase),
                    }, style: theme.textTheme.titleMedium),
                  ),
                  if (fraction != null && !analyzing)
                    Text(formats.percent(fraction), style: theme.textTheme.titleLarge),
                ],
              ),
              const SizedBox(height: 10),
              ClipRRect(
                borderRadius: BorderRadius.circular(4),
                child: LinearProgressIndicator(minHeight: 10, value: analyzing || fraction == null ? null : fraction),
              ),
              const SizedBox(height: 12),
              if (progress != null && !analyzing)
                Figures(
                  children: [
                    Figure(
                      label: S.scanRead,
                      value: S.ofTotal(formats.bytes(progress.bytesDone), formats.bytes(progress.bytesTotal)),
                    ),
                    Figure(label: S.speed, value: formats.speed(progress.speedBps)),
                    Figure(
                      label: S.timeLeft,
                      value: progress.etaSeconds == null ? S.timeUnknown : formats.duration(progress.etaSeconds!),
                    ),
                    Figure(label: S.elapsed, value: formats.duration(progress.elapsedSeconds)),
                    Figure(label: S.scanPosition, value: formats.bytes(progress.position), tooltip: S.scanPositionTip),
                  ],
                )
              else if (analyzing)
                Text(S.analyzingText, style: theme.textTheme.bodyMedium),
              const Divider(height: 28),
              Row(
                children: [
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(analyzing ? S.stopNotPossible : S.stopExplanation, style: theme.textTheme.bodyMedium),
                        const SizedBox(height: 4),
                        Text(
                          app.source.sessionPath == null ? S.scanNotSaved : S.scanSavedTo(app.source.sessionPath!),
                          style: theme.textTheme.bodySmall?.copyWith(
                            color: app.source.sessionPath == null
                                ? status.onCaution
                                : theme.colorScheme.onSurfaceVariant,
                          ),
                        ),
                      ],
                    ),
                  ),
                  const SizedBox(width: 16),
                  FilledButton.icon(
                    onPressed: scan.canStop ? () => unawaited(scan.stop()) : null,
                    icon: const Icon(Icons.stop_circle_outlined),
                    label: Text(scan.phase == JobPhase.cancelling || scan.cancelPending ? S.stopping : S.stopAndShow),
                  ),
                ],
              ),
            ],
          ),
        ),
      ),
      if (progress != null)
        Section(
          title: S.foundSoFar,
          child: Figures(
            children: [
              Figure(label: S.foundFiles, value: formats.count(progress.files), emphasis: true),
              Figure(label: S.foundFolders, value: formats.count(progress.dirs), emphasis: true),
              Figure(label: S.foundRecords, value: formats.count(progress.records), tooltip: S.foundRecordsTip),
              Figure(label: S.foundIndexBlocks, value: formats.count(progress.indexBlocks), tooltip: S.foundIndexTip),
              Figure(label: S.foundSignatures, value: formats.count(progress.carveHits), tooltip: S.foundSignaturesTip),
              Figure(
                label: S.unreadable,
                value: progress.badBytes == 0 ? S.none : formats.bytes(progress.badBytes),
                tooltip: S.unreadableTip,
              ),
            ],
          ),
        ),
      if (progress != null && progress.badBytes > 0)
        Notice(
          level: NoticeLevel.caution,
          title: S.badSectorsTitle(formats.bytes(progress.badBytes)),
          body: S.badSectorsBody,
        ),
      if (phase == ScanPhase.sweep && scan.phase == JobPhase.running)
        const Notice(
          level: NoticeLevel.info,
          icon: Icons.lightbulb_outline,
          title: S.sweepHintTitle,
          body: S.sweepHintBody,
        ),
      for (final warning in scan.warnings)
        Notice(
          level: NoticeLevel.ofWarning(warning.kind),
          title: S.warningHeadline(warning.kind),
          body: S.warningAdvice(warning.kind),
          details: warning.message,
        ),
    ];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [for (final child in children) Padding(padding: const EdgeInsets.only(bottom: 12), child: child)],
    );
  }
}

/// The three phases of a scan as steps.
class PhaseSteps extends StatelessWidget {
  const PhaseSteps({super.key, required this.phase});

  final ScanPhase? phase;

  static const List<ScanPhase> _order = [ScanPhase.mftProbe, ScanPhase.sweep, ScanPhase.analyzing];

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final current = phase == null ? -1 : _order.indexOf(phase!);
    return Wrap(
      spacing: 8,
      runSpacing: 8,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        for (final (i, step) in _order.indexed) ...[
          if (i > 0) Icon(Icons.chevron_right, color: theme.colorScheme.outline),
          _Step(
            number: i + 1,
            label: S.phaseStep(step),
            state: i < current ? _StepState.done : (i == current ? _StepState.current : _StepState.coming),
          ),
        ],
      ],
    );
  }
}

enum _StepState { done, current, coming }

class _Step extends StatelessWidget {
  const _Step({required this.number, required this.label, required this.state});

  final int number;
  final String label;
  final _StepState state;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    final (background, foreground) = switch (state) {
      _StepState.current => (colors.primaryContainer, colors.onPrimaryContainer),
      _StepState.done => (colors.surfaceContainerHigh, colors.onSurface),
      _StepState.coming => (colors.surfaceContainerLow, colors.onSurfaceVariant),
    };
    return Semantics(
      selected: state == _StepState.current,
      child: DecoratedBox(
        decoration: BoxDecoration(color: background, borderRadius: BorderRadius.circular(20)),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              if (state == _StepState.done)
                Icon(Icons.check, size: 16, color: foreground)
              else
                Text(
                  '$number',
                  style: TextStyle(color: foreground, fontWeight: FontWeight.w600),
                ),
              const SizedBox(width: 8),
              Text(label, style: TextStyle(color: foreground)),
            ],
          ),
        ),
      ),
    );
  }
}

/// The last scan has ended: with results (normally the Results screen is
/// already showing them), or with an error.
class _Finished extends StatelessWidget {
  const _Finished({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final scan = app.scan;
    final formats = Formats.of(context);
    final status = scan.status;
    final failure = scan.failure;
    if (status == JobStatus.failed || failure != null) {
      final lost = failure?.kind == EngineErrorCode.sourceLost;
      final current = app.source.current;
      return Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (lost)
            Notice(
              level: NoticeLevel.danger,
              icon: Icons.link_off,
              title: S.sourceLostTitle,
              body: app.source.sessionPath == null ? S.sourceLostBodyUnsaved : S.sourceLostBody,
              details: failure?.message,
              actions: [
                FilledButton(
                  onPressed: app.jobs.isBusy || current == null
                      ? null
                      : () async {
                          final attached = await attachSourceFlow(context, app);
                          if (attached && context.mounted) await resumeScan(context, app);
                        },
                  child: const Text(S.reattachAndResume),
                ),
              ],
            )
          else
            ErrorNotice(
              error: failure ?? const EngineError(code: 'internal'),
              title: S.scanFailedTitle,
              actions: [
                OutlinedButton(onPressed: () => app.screen.value = AppScreen.source, child: const Text(S.chooseSource)),
              ],
            ),
        ],
      );
    }
    final catalog = scan.result?.catalog;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Notice(
          level: NoticeLevel.good,
          title: status == JobStatus.stopped ? S.scanStoppedTitle : S.scanCompletedTitle,
          body: catalog == null
              ? null
              : S.resultsReadyBody(formats.count(catalog.instances.fold(0, (sum, i) => sum + i.files))),
          actions: [
            FilledButton(onPressed: () => app.screen.value = AppScreen.results, child: const Text(S.showResults)),
            if (status == JobStatus.stopped && app.catalog.sourceAttached)
              OutlinedButton(
                onPressed: app.jobs.isBusy ? null : () => resumeScan(context, app),
                child: const Text(S.resumeScan),
              ),
          ],
        ),
      ],
    );
  }
}
