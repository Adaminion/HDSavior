import 'dart:async';

import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import '../flows.dart';
import '../format.dart';
import '../strings.dart';
import '../theme.dart';
import '../widgets/common.dart';
import '../widgets/destination.dart';
import '../widgets/errors.dart';

/// Screen 4, Recover: copy the ticked files to a safe place.
///
/// The destination is a folder chosen with the picker or typed (UNC paths
/// included). The engine's destination guard checks it live and says why
/// it refuses; `same_disk` and `unknown_disk` need the user's separate,
/// explicit consent (docs/ARCHITECTURE.md S3).
class RecoverScreen extends StatelessWidget {
  const RecoverScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.recover, app.catalog, app.selection, app.jobs]),
      builder: (context, _) {
        final recover = app.recover;
        if (recover.isActive) {
          return ScreenFrame(
            title: S.recoverTitle,
            purpose: S.recoverPurpose,
            child: _Progress(app: app),
          );
        }
        if (recover.phase == JobPhase.done) {
          return ScreenFrame(
            title: S.recoverTitle,
            purpose: S.recoverPurpose,
            child: _Outcome(app: app),
          );
        }
        if (!app.catalog.hasCatalog) {
          return EmptyState(
            icon: Icons.save_alt_outlined,
            title: S.recoverNoCatalogTitle,
            text: S.recoverNoCatalog,
            actions: [
              FilledButton(onPressed: () => app.screen.value = AppScreen.source, child: const Text(S.chooseSource)),
            ],
          );
        }
        return ScreenFrame(
          title: S.recoverTitle,
          purpose: S.recoverPurpose,
          footer: _StartBar(app: app),
          child: _Form(app: app),
        );
      },
    );
  }
}

class _Form extends StatelessWidget {
  const _Form({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final recover = app.recover;
    final options = recover.options;
    final summary = recover.summary;
    final empty = app.selection.isEmpty;
    final attached = app.catalog.sourceAttached;

    final what = Section(
      title: S.recoverWhat,
      trailing: TextButton(onPressed: () => app.screen.value = AppScreen.results, child: const Text(S.recoverChange)),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (empty)
            const Notice(
              level: NoticeLevel.info,
              title: S.recoverNothingTicked,
              body: S.recoverNothingTickedBody,
              dense: true,
            )
          else if (recover.summaryError != null)
            ErrorNotice(
              error: recover.summaryError!,
              title: S.selectionFailed,
              actions: [TextButton(onPressed: recover.refreshSummary, child: const Text(S.retry))],
            )
          else if (summary == null)
            const Text(S.selectionCounting)
          else
            Figures(
              children: [
                Figure(label: S.recoverFiles, value: formats.count(summary.files), emphasis: true),
                Figure(
                  label: S.recoverBytes,
                  value: formats.bytes(summary.bytes),
                  emphasis: true,
                  tooltip: S.recoverBytesTip,
                ),
                Figure(label: S.recoverFolders, value: formats.count(summary.dirs)),
                Figure(label: S.rating(Rating.good), value: formats.count(summary.good)),
                Figure(label: S.rating(Rating.uncertain), value: formats.count(summary.uncertain)),
                Figure(label: S.rating(Rating.damaged), value: formats.count(summary.damaged)),
                Figure(
                  label: S.rating(Rating.unsupported),
                  value: formats.count(summary.unsupported),
                  tooltip: S.ratingExplanation(Rating.unsupported),
                ),
              ],
            ),
          const SizedBox(height: 8),
          SwitchListTile(
            contentPadding: EdgeInsets.zero,
            value: options.includeDeleted,
            onChanged: (value) => recover.options = options.copyWith(includeDeleted: value),
            title: const Text(S.optionDeleted),
            subtitle: const Text(S.optionDeletedHint),
          ),
          SwitchListTile(
            contentPadding: EdgeInsets.zero,
            value: options.skipDamaged,
            onChanged: (value) => recover.options = options.copyWith(skipDamaged: value),
            title: const Text(S.optionSkipDamaged),
            subtitle: const Text(S.optionSkipDamagedHint),
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(S.optionOnExists, style: theme.textTheme.bodyLarge),
                    Text(
                      S.optionOnExistsHint,
                      style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                    ),
                  ],
                ),
              ),
              const SizedBox(width: 12),
              DropdownMenu<OnExists>(
                initialSelection: options.onExists,
                width: 230,
                onSelected: (value) {
                  if (value != null) recover.options = options.copyWith(onExists: value);
                },
                dropdownMenuEntries: [
                  for (final value in OnExists.values) DropdownMenuEntry(value: value, label: S.onExists(value)),
                ],
              ),
            ],
          ),
        ],
      ),
    );

    final where = Section(
      title: S.recoverWhere,
      subtitle: S.recoverWhereHint,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          DestinationField(
            fieldKey: const ValueKey('recover-destination'),
            initial: recover.checkedDestination,
            label: S.recoverFolderLabel,
            hint: S.recoverFolderHint,
            enabled: attached,
            onChanged: (_) => _withdrawConsents(),
            onCheck: (path) {
              _withdrawConsents();
              recover.checkDestination(path).ignore();
            },
            pick: () => app.pickers.pickFolder(),
          ),
          const SizedBox(height: 6),
          Text(S.uncPathHint, style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant)),
          const SizedBox(height: 12),
          VerdictView(
            checker: recover.destination,
            consents: Consents(sameDisk: options.allowSameDisk, unknownDisk: options.allowUnknownDisk),
            onConsents: (consents) => recover.options = options.copyWith(
              allowSameDisk: consents.sameDisk,
              allowUnknownDisk: consents.unknownDisk,
            ),
            hasCatalog: true,
            needBytes: summary?.bytes,
          ),
        ],
      ),
    );

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (!attached) ...[
          Notice(
            level: NoticeLevel.caution,
            icon: Icons.link_off,
            title: S.browseOnlyTitle,
            body: S.recoverNeedsSource,
            actions: [
              FilledButton.tonal(
                onPressed: app.jobs.isBusy ? null : () => attachSourceFlow(context, app),
                child: const Text(S.attachAction),
              ),
            ],
          ),
          const SizedBox(height: 12),
        ],
        what,
        const SizedBox(height: 12),
        where,
      ],
    );
  }

  void _withdrawConsents() {
    final options = app.recover.options;
    if (options.allowSameDisk || options.allowUnknownDisk) {
      app.recover.options = options.copyWith(allowSameDisk: false, allowUnknownDisk: false);
    }
  }
}

/// The button that starts the recovery, always in view, and in words why
/// it cannot be pressed yet.
class _StartBar extends StatelessWidget {
  const _StartBar({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final recover = app.recover;
    final summary = recover.summary;
    final empty = app.selection.isEmpty;
    final why = _whyNot(context);
    return Row(
      children: [
        Expanded(
          child: Text(
            why ?? S.recoverReady,
            style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant),
          ),
        ),
        const SizedBox(width: 16),
        FilledButton.icon(
          onPressed: recover.readyToStart ? () => _start(context) : null,
          icon: const Icon(Icons.save_alt),
          label: Text(
            summary == null || empty
                ? S.recoverStartPlain
                : S.recoverStart(formats.count(summary.files), summary.files),
          ),
        ),
      ],
    );
  }

  /// Why the start button is disabled, in words; `null` if it is not.
  String? _whyNot(BuildContext context) {
    final recover = app.recover;
    if (recover.readyToStart) return null;
    if (app.jobs.isBusy) return S.recoverWhyBusy;
    if (!app.catalog.sourceAttached) return S.recoverNeedsSource;
    if (app.selection.isEmpty) return S.recoverWhyNothing;
    final summary = recover.summary;
    if (summary == null) return S.selectionCounting;
    if (summary.files == 0 && summary.dirs == 0) return S.recoverWhyNoFiles;
    final check = recover.destinationCheck;
    if (recover.checkedDestination == null) return S.recoverWhyNoDestination;
    if (check == null) return S.recoverWhyChecking;
    if (!recover.allows(check)) {
      return check.verdict == DestinationVerdict.sameDisk || check.verdict == DestinationVerdict.unknownDisk
          ? S.recoverWhyConsent
          : S.recoverWhyRefused;
    }
    if (check.enoughSpace == false) return S.recoverWhySpace;
    return null;
  }

  Future<void> _start(BuildContext context) async {
    final dest = app.recover.checkedDestination;
    if (dest == null) return;
    try {
      await app.recover.start(dest: dest);
    } on EngineException catch (e) {
      // A refused destination shows as its verdict; other refusals in a dialog.
      if (e.destinationCheck == null && context.mounted) {
        await showErrorDialog(context, e, title: S.recoverFailedToStart);
      }
    }
  }
}

class _Progress extends StatelessWidget {
  const _Progress({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final recover = app.recover;
    final progress = recover.progress;
    final started = recover.started;
    final status = StatusColors.from(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Card(
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: Text(
                        recover.phase == JobPhase.cancelling || recover.cancelPending
                            ? S.recoverStopping
                            : (progress == null ? S.recoverStarting : S.recoverRunning),
                        style: theme.textTheme.titleMedium,
                      ),
                    ),
                    if (progress?.fraction != null)
                      Text(formats.percent(progress!.fraction!), style: theme.textTheme.titleLarge),
                  ],
                ),
                const SizedBox(height: 10),
                ClipRRect(
                  borderRadius: BorderRadius.circular(4),
                  child: LinearProgressIndicator(minHeight: 10, value: progress?.fraction),
                ),
                const SizedBox(height: 8),
                if (progress?.current != null)
                  Text(
                    S.recoverCurrent(progress!.current!),
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                  ),
                const SizedBox(height: 12),
                if (progress != null)
                  Figures(
                    children: [
                      Figure(
                        label: S.recoverFiles,
                        value: S.ofTotal(formats.count(progress.filesDone), formats.count(progress.filesTotal)),
                      ),
                      Figure(
                        label: S.recoverBytes,
                        value: S.ofTotal(formats.bytes(progress.bytesDone), formats.bytes(progress.bytesTotal)),
                      ),
                      Figure(label: S.speed, value: formats.speed(progress.speedBps)),
                      Figure(
                        label: S.timeLeft,
                        value: progress.etaSeconds == null ? S.timeUnknown : formats.duration(progress.etaSeconds!),
                      ),
                      Figure(label: S.elapsed, value: formats.duration(progress.elapsedSeconds)),
                    ],
                  )
                else if (started != null)
                  Text(S.recoverPlanned(formats.count(started.files), formats.bytes(started.bytes))),
              ],
            ),
          ),
        ),
        const SizedBox(height: 12),
        if (progress != null)
          Section(
            title: S.recoverOutcomes,
            child: Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                StatusChip(label: S.outcomeOk(formats.count(progress.ok)), colors: status.forLevel(NoticeLevel.good)),
                StatusChip(
                  label: S.outcomeWarnings(formats.count(progress.warnings)),
                  colors: status.forLevel(NoticeLevel.caution),
                ),
                StatusChip(
                  label: S.outcomeFailed(formats.count(progress.failed)),
                  colors: status.forLevel(NoticeLevel.danger),
                ),
                StatusChip(
                  label: S.outcomeSkipped(formats.count(progress.skipped)),
                  colors: status.forLevel(NoticeLevel.info),
                ),
              ],
            ),
          ),
        const SizedBox(height: 12),
        Row(
          children: [
            Expanded(
              child: Text(S.recoverDestinationLine(recover.checkedDestination ?? ''), style: theme.textTheme.bodySmall),
            ),
            const SizedBox(width: 16),
            OutlinedButton.icon(
              onPressed: recover.phase == JobPhase.running || recover.phase == JobPhase.starting
                  ? () async {
                      final yes = await confirm(
                        context,
                        title: S.recoverCancelTitle,
                        body: S.recoverCancelBody,
                        confirmLabel: S.recoverCancelConfirm,
                        cancelLabel: S.recoverContinue,
                      );
                      if (yes) unawaited(recover.cancel());
                    }
                  : null,
              icon: const Icon(Icons.stop_circle_outlined),
              label: const Text(S.recoverCancel),
            ),
          ],
        ),
      ],
    );
  }
}

class _Outcome extends StatelessWidget {
  const _Outcome({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final recover = app.recover;
    final done = recover.result;
    final failure = recover.failure;
    final status = recover.status;
    final dest = done?.dest ?? recover.checkedDestination;
    final report = done?.reportPath;
    final Widget headline = switch (status) {
      JobStatus.completed => Notice(
        level: (done?.failed ?? 0) > 0 || (done?.warnings ?? 0) > 0 ? NoticeLevel.caution : NoticeLevel.good,
        title: S.recoverCompleted,
        body: S.recoverCompletedBody,
      ),
      JobStatus.cancelled => const Notice(
        level: NoticeLevel.caution,
        title: S.recoverCancelled,
        body: S.recoverCancelledBody,
      ),
      _ => ErrorNotice(
        error: failure ?? const EngineError(code: 'internal'),
        title: S.recoverFailed,
      ),
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        headline,
        const SizedBox(height: 12),
        if (done != null)
          Section(
            title: S.recoverSummary,
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Figures(
                  children: [
                    Figure(
                      label: S.outcomeOkLabel,
                      value: formats.count(done.ok),
                      emphasis: true,
                      tooltip: S.outcomeOkTip,
                    ),
                    Figure(
                      label: S.outcomeWarningsLabel,
                      value: formats.count(done.warnings),
                      emphasis: true,
                      tooltip: S.outcomeWarningsTip,
                    ),
                    Figure(
                      label: S.outcomeFailedLabel,
                      value: formats.count(done.failed),
                      emphasis: true,
                      tooltip: S.outcomeFailedTip,
                    ),
                    Figure(
                      label: S.outcomeSkippedLabel,
                      value: formats.count(done.skipped),
                      emphasis: true,
                      tooltip: S.outcomeSkippedTip,
                    ),
                    if (done.notAttempted > 0)
                      Figure(label: S.outcomeNotAttempted, value: formats.count(done.notAttempted)),
                    Figure(label: S.recoverWritten, value: formats.bytes(done.bytesWritten)),
                  ],
                ),
                if (done.warnings > 0 || done.failed > 0 || done.skipped > 0) ...[
                  const SizedBox(height: 12),
                  Text(S.recoverReportAdvice, style: theme.textTheme.bodyMedium),
                ],
              ],
            ),
          ),
        const SizedBox(height: 12),
        if (dest != null)
          Card(
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  FactRow(label: S.recoverDestination, value: dest),
                  if (report != null) FactRow(label: S.recoverReport, value: report),
                  const SizedBox(height: 12),
                  Wrap(
                    spacing: 8,
                    runSpacing: 8,
                    children: [
                      FilledButton.icon(
                        onPressed: () => openPath(context, app, dest, folder: true),
                        icon: const Icon(Icons.folder_open),
                        label: const Text(S.openFolder),
                      ),
                      if (report != null)
                        OutlinedButton.icon(
                          onPressed: () => openPath(context, app, report, folder: false),
                          icon: const Icon(Icons.table_view_outlined),
                          label: const Text(S.openReport),
                        ),
                    ],
                  ),
                ],
              ),
            ),
          ),
        const SizedBox(height: 16),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            OutlinedButton(
              onPressed: () {
                recover.reset();
                app.screen.value = AppScreen.results;
              },
              child: const Text(S.recoverMore),
            ),
            if (status != JobStatus.completed)
              TextButton(onPressed: recover.reset, child: const Text(S.recoverBackToForm)),
          ],
        ),
      ],
    );
  }
}
