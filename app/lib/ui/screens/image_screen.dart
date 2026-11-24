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

/// Screen 5, Image: copy the source sector by sector to a file on another
/// disk, so that the rescue can continue from the copy (and a failing disk
/// is read only once).
class ImageScreen extends StatefulWidget {
  const ImageScreen({super.key, required this.app});

  final AppController app;

  @override
  State<ImageScreen> createState() => _ImageScreenState();
}

/// One choice of what to copy.
@immutable
final class _RangeChoice {
  const _RangeChoice(this.label, this.range);

  final String label;

  /// `null`: the whole source.
  final ByteRange? range;

  @override
  bool operator ==(Object other) => other is _RangeChoice && other.range == range;

  @override
  int get hashCode => range.hashCode;
}

class _ImageScreenState extends State<ImageScreen> {
  _RangeChoice? _range;
  Consents _consents = Consents.none;
  bool _resume = false;
  int _seenSource = -1;

  AppController get app => widget.app;

  List<_RangeChoice> _choices(OpenedSource source, Formats formats) => [
    _RangeChoice(S.imageWhole(source.info.kind == SourceKind.disk, formats.bytes(source.info.size)), null),
    for (final partition in source.partitions)
      _RangeChoice(
        S.imagePartition(
          S.partitionTitle(partition.index, partition.letters, partition.label),
          formats.bytes(partition.size),
        ),
        partition.range,
      ),
    if (app.catalog.info?.range case final scanned?)
      if (!source.partitions.any((p) => p.range == scanned) && scanned != ByteRange(0, source.info.size))
        _RangeChoice(S.imageScanned(formats.bytes(scanned.length)), scanned),
  ];

  int _needBytes(OpenedSource source) => (_range?.range?.length) ?? source.info.size;

  void _check(String path, OpenedSource source) {
    setState(() => _consents = Consents.none);
    app.image.checkDestination(path, needBytes: _needBytes(source)).ignore();
  }

  String _suggestedName(SourceInfo info) {
    final base = info.kind == SourceKind.disk
        ? [?info.model, ?info.serial].where((s) => s.trim().isNotEmpty).join(' ')
        : 'Copy of ${info.path.split(RegExp(r'[\\/]')).last}';
    final safe = base.replaceAll(RegExp(r'[<>:"/\\|?*\x00-\x1f]'), ' ').replaceAll(RegExp(r'\s+'), ' ').trim();
    return '${safe.isEmpty ? 'Disk' : safe}.img';
  }

  Future<void> _start(OpenedSource source) async {
    final dest = app.image.checkedDestination;
    if (dest == null) return;
    var resume = _resume;
    while (mounted) {
      try {
        await app.image.start(
          range: _range?.range,
          dest: dest,
          resume: resume,
          allowSameDisk: _consents.sameDisk,
          allowUnknownDisk: _consents.unknownDisk,
        );
        return;
      } on EngineException catch (e) {
        if (!mounted) return;
        if (e.kind == EngineErrorCode.exists && !resume) {
          resume = await confirm(
            context,
            title: S.imageExistsTitle,
            body: S.imageExistsBody,
            confirmLabel: S.imageExistsResume,
          );
          if (!resume || !mounted) return;
          setState(() => _resume = true);
          continue;
        }
        if (e.destinationCheck == null) await showErrorDialog(context, e, title: S.imageFailedToStart);
        return;
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.image, app.source, app.catalog, app.jobs]),
      builder: (context, _) {
        final image = app.image;
        if (image.isActive) {
          return ScreenFrame(
            title: S.imageTitle,
            purpose: S.imagePurpose,
            child: _Progress(app: app),
          );
        }
        if (image.phase == JobPhase.done) {
          return ScreenFrame(
            title: S.imageTitle,
            purpose: S.imagePurpose,
            child: _Outcome(app: app),
          );
        }
        final source = app.source.current;
        if (source == null) {
          return EmptyState(
            icon: Icons.album_outlined,
            title: S.imageNoSourceTitle,
            text: S.imageNoSourceText,
            actions: [
              FilledButton(onPressed: () => app.screen.value = AppScreen.source, child: const Text(S.chooseSource)),
            ],
          );
        }
        if (_seenSource != app.source.revision) {
          // Another source: what was chosen for the previous one is void.
          _seenSource = app.source.revision;
          _range = null;
          _consents = Consents.none;
          _resume = false;
        }
        return ScreenFrame(
          title: S.imageTitle,
          purpose: S.imagePurpose,
          footer: _startBar(context, source),
          child: _form(context, source),
        );
      },
    );
  }

  /// Whether everything the screen can know of allows a start.
  bool _ready(OpenedSource source) {
    final check = app.image.destinationCheck;
    return source.attached &&
        !app.jobs.isBusy &&
        check != null &&
        app.image.allows(check, allowSameDisk: _consents.sameDisk, allowUnknownDisk: _consents.unknownDisk) &&
        check.enoughSpace != false &&
        (check.maxFileBytes == null || check.maxFileBytes! >= _needBytes(source));
  }

  Widget _startBar(BuildContext context, OpenedSource source) {
    final theme = Theme.of(context);
    final ready = _ready(source);
    return Row(
      children: [
        Expanded(
          child: Text(
            S.imageDuration,
            style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant),
          ),
        ),
        const SizedBox(width: 16),
        FilledButton.icon(
          onPressed: ready ? () => _start(source) : null,
          icon: const Icon(Icons.album_outlined),
          label: Text(_resume ? S.imageContinue : S.imageStart),
        ),
      ],
    );
  }

  Widget _form(BuildContext context, OpenedSource source) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final image = app.image;
    final choices = _choices(source, formats);
    final chosen = choices.contains(_range) ? _range! : choices.first;
    final check = image.destinationCheck;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (!source.attached) ...[
          Notice(
            level: NoticeLevel.caution,
            icon: Icons.link_off,
            title: S.browseOnlyTitle,
            body: S.imageNeedsSource,
            actions: [
              FilledButton.tonal(onPressed: () => attachSourceFlow(context, app), child: const Text(S.attachAction)),
            ],
          ),
          const SizedBox(height: 12),
        ],
        Section(
          title: S.imageWhat,
          subtitle: describeSource(source.info, formats),
          child: RadioGroup<_RangeChoice>(
            groupValue: chosen,
            onChanged: (value) {
              setState(() => _range = value);
              final path = image.checkedDestination;
              if (path != null && value != null) {
                image.checkDestination(path, needBytes: value.range?.length ?? source.info.size).ignore();
              }
            },
            child: Column(
              children: [
                for (final choice in choices)
                  RadioListTile<_RangeChoice>(
                    value: choice,
                    title: Text(choice.label),
                    contentPadding: EdgeInsets.zero,
                  ),
              ],
            ),
          ),
        ),
        const SizedBox(height: 12),
        Section(
          title: S.imageWhere,
          subtitle: S.imageWhereHint,
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              DestinationField(
                fieldKey: const ValueKey('image-destination'),
                initial: image.checkedDestination,
                label: S.imageFileLabel,
                hint: S.imageFileHint,
                enabled: source.attached,
                onChanged: (_) => setState(() => _consents = Consents.none),
                onCheck: (path) => _check(path, source),
                pick: () => app.pickers.pickSavePath(
                  label: S.imageFilesLabel,
                  extensions: const ['img'],
                  suggestedName: _suggestedName(source.info),
                ),
              ),
              const SizedBox(height: 6),
              Text(
                S.uncPathHint,
                style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
              ),
              const SizedBox(height: 12),
              VerdictView(
                checker: image.destination,
                consents: _consents,
                onConsents: (consents) => setState(() => _consents = consents),
                hasCatalog: app.catalog.hasCatalog,
                needBytes: _needBytes(source),
              ),
              if (check != null && check.maxFileBytes != null && check.maxFileBytes! < _needBytes(source)) ...[
                const SizedBox(height: 8),
                Notice(
                  level: NoticeLevel.danger,
                  title: S.imageTooLarge(check.fs ?? '', formats.bytes(check.maxFileBytes!)),
                  dense: true,
                ),
              ],
              CheckboxListTile(
                value: _resume,
                onChanged: (value) => setState(() => _resume = value ?? false),
                title: const Text(S.imageResume),
                subtitle: const Text(S.imageResumeHint),
                controlAffinity: ListTileControlAffinity.leading,
                contentPadding: EdgeInsets.zero,
              ),
            ],
          ),
        ),
      ],
    );
  }
}

class _Progress extends StatelessWidget {
  const _Progress({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final image = app.image;
    final progress = image.progress;
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
                        image.phase == JobPhase.cancelling || image.cancelPending
                            ? S.imageStopping
                            : (progress == null ? S.imageStarting : S.imageRunning),
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
                const SizedBox(height: 12),
                if (progress != null)
                  Figures(
                    children: [
                      Figure(
                        label: S.imageCopied,
                        value: S.ofTotal(formats.bytes(progress.bytesDone), formats.bytes(progress.bytesTotal)),
                      ),
                      Figure(label: S.speed, value: formats.speed(progress.speedBps)),
                      Figure(
                        label: S.timeLeft,
                        value: progress.etaSeconds == null ? S.timeUnknown : formats.duration(progress.etaSeconds!),
                      ),
                      Figure(label: S.elapsed, value: formats.duration(progress.elapsedSeconds)),
                      Figure(
                        label: S.unreadable,
                        value: progress.badBytes == 0 ? S.none : formats.bytes(progress.badBytes),
                        tooltip: S.unreadableTip,
                      ),
                    ],
                  ),
              ],
            ),
          ),
        ),
        if (progress != null && progress.badBytes > 0) ...[
          const SizedBox(height: 12),
          Notice(
            level: NoticeLevel.caution,
            title: S.badSectorsTitle(formats.bytes(progress.badBytes)),
            body: S.imageBadSectors,
          ),
        ],
        const SizedBox(height: 12),
        Row(
          children: [
            Expanded(
              child: Text(S.imageDestinationLine(image.checkedDestination ?? ''), style: theme.textTheme.bodySmall),
            ),
            const SizedBox(width: 16),
            OutlinedButton.icon(
              onPressed: image.phase == JobPhase.running || image.phase == JobPhase.starting
                  ? () async {
                      final yes = await confirm(
                        context,
                        title: S.imageCancelTitle,
                        body: S.imageCancelBody,
                        confirmLabel: S.imageCancelConfirm,
                        cancelLabel: S.imageKeepGoing,
                      );
                      if (yes) unawaited(image.cancel());
                    }
                  : null,
              icon: const Icon(Icons.stop_circle_outlined),
              label: const Text(S.imageCancel),
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
    final formats = Formats.of(context);
    final image = app.image;
    final done = image.result;
    final status = image.status;
    final path = done?.path ?? image.checkedDestination;
    final Widget headline = switch (status) {
      JobStatus.completed => Notice(
        level: (done?.badBytes ?? 0) > 0 ? NoticeLevel.caution : NoticeLevel.good,
        title: S.imageCompleted,
        body: (done?.badBytes ?? 0) > 0 ? S.imageCompletedBad(formats.bytes(done!.badBytes)) : S.imageCompletedBody,
      ),
      JobStatus.cancelled => const Notice(
        level: NoticeLevel.caution,
        title: S.imageCancelled,
        body: S.imageCancelledBody,
      ),
      _ => ErrorNotice(
        error: image.failure ?? const EngineError(code: 'internal'),
        title: S.imageFailed,
      ),
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        headline,
        const SizedBox(height: 12),
        if (path != null)
          Card(
            child: Padding(
              padding: const EdgeInsets.all(16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  FactRow(label: S.imageFile, value: path),
                  if (done != null) FactRow(label: S.imageWritten, value: formats.bytes(done.bytesWritten)),
                  if (done?.badMapPath != null) FactRow(label: S.imageBadMap, value: done!.badMapPath!),
                  const SizedBox(height: 12),
                  Wrap(
                    spacing: 8,
                    runSpacing: 8,
                    children: [
                      OutlinedButton.icon(
                        onPressed: () => openPath(context, app, _folderOf(path), folder: true),
                        icon: const Icon(Icons.folder_open),
                        label: const Text(S.openFolder),
                      ),
                      if (status == JobStatus.completed)
                        FilledButton.icon(
                          onPressed: app.jobs.isBusy ? null : () => _openImage(context, path),
                          icon: const Icon(Icons.insert_drive_file_outlined),
                          label: const Text(S.imageOpenAsSource),
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
          children: [
            if (status != JobStatus.completed)
              OutlinedButton(
                onPressed: image.reset,
                child: Text(status == JobStatus.cancelled ? S.imageBackToResume : S.imageBackToForm),
              ),
            if (status == JobStatus.completed) TextButton(onPressed: image.reset, child: const Text(S.imageAnother)),
          ],
        ),
      ],
    );
  }

  static String _folderOf(String path) {
    final cut = path.lastIndexOf(RegExp(r'[\\/]'));
    return cut <= 0 ? path : path.substring(0, cut);
  }

  Future<void> _openImage(BuildContext context, String path) async {
    if (app.catalog.hasCatalog) {
      final yes = await confirm(
        context,
        title: S.imageOpenTitle,
        body: app.source.sessionPath == null ? S.closeSourceBodyUnsaved : S.closeSourceBody(app.source.sessionPath!),
        confirmLabel: S.imageOpenConfirm,
      );
      if (!yes || !context.mounted) return;
    }
    final opened = await attempt(context, () => app.source.openImage(path), errorTitle: S.openImageFailed);
    if (opened != null) {
      app.image.reset();
      app.screen.value = AppScreen.source;
    }
  }
}
