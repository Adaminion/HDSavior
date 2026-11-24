/// The disk layout ("geometry") of a file system: how HDSavior maps the old
/// file system onto the disk, the evidence for it, the other candidates,
/// and a manual override for experts (`geometry_set`).
library;

import 'package:flutter/services.dart';
import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import '../flows.dart';
import '../format.dart';
import '../strings.dart';
import '../theme.dart';
import '../widgets/common.dart';
import '../widgets/errors.dart';

/// Shows the geometry of the file system [instance] (an `Instance.id` of
/// the current catalog; default: the main one).
Future<void> showGeometryDialog(BuildContext context, AppController app, {int? instance}) => showDialog<void>(
  context: context,
  builder: (context) => _GeometryDialog(app: app, initial: instance),
);

class _GeometryDialog extends StatefulWidget {
  const _GeometryDialog({required this.app, this.initial});

  final AppController app;
  final int? initial;

  @override
  State<_GeometryDialog> createState() => _GeometryDialogState();
}

class _GeometryDialogState extends State<_GeometryDialog> {
  /// The instance shown, by its position (ids are positions in the current
  /// catalog and change with a new epoch).
  late int _index = widget.initial ?? 0;
  bool _advanced = false;
  int _clusterSize = 4096;
  final TextEditingController _offset = TextEditingController();
  String? _offsetError;
  bool _working = false;

  AppController get app => widget.app;

  static const List<int> clusterSizes = [
    512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152, //
  ];

  @override
  void dispose() {
    _offset.dispose();
    super.dispose();
  }

  Future<void> _apply(Instance instance, int clusterSize, int volumeOffset) async {
    final main = instance.kind == InstanceKind.main;
    final yes = await confirm(
      context,
      title: S.geometryConfirmTitle,
      body: S.geometryConfirmBody(main: main),
      confirmLabel: S.geometryConfirm,
    );
    if (!yes || !mounted) return;
    setState(() => _working = true);
    await attempt(
      context,
      () => app.catalog.setGeometry(instance: instance.id, clusterSize: clusterSize, volumeOffset: volumeOffset),
      errorTitle: S.geometryFailed,
    );
    if (mounted) setState(() => _working = false);
  }

  void _applyManual(Instance instance) {
    final text = _offset.text.replaceAll(RegExp(r'[\s,._]'), '');
    final value = int.tryParse(text);
    final size = app.source.current?.info.size;
    String? error;
    if (value == null || value < 0) {
      error = S.offsetNotANumber;
    } else if (value % 512 != 0) {
      error = S.offsetNotAligned;
    } else if (size != null && value >= size) {
      error = S.offsetOutside;
    }
    setState(() => _offsetError = error);
    if (error == null) _apply(instance, _clusterSize, value!);
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.catalog, app.jobs, app.task]),
      builder: (context, _) {
        final instances = app.catalog.info?.instances ?? const <Instance>[];
        if (instances.isEmpty) {
          return AlertDialog(
            content: const Text(S.noCatalogText),
            actions: [TextButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.close))],
          );
        }
        final index = _index.clamp(0, instances.length - 1);
        final instance = instances[index];
        final geometry = instance.geometry;
        final theme = Theme.of(context);
        final formats = Formats.of(context);
        final busy = _working || app.jobs.isBusy;
        return AlertDialog(
          title: const Text(S.geometryTitle),
          content: SizedBox(
            width: 640,
            child: SingleChildScrollView(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  if (_working) ...[
                    LinearProgressIndicator(value: app.task.value?.fraction),
                    const SizedBox(height: 8),
                  ],
                  Text(S.geometryIntro, style: theme.textTheme.bodyMedium),
                  const SizedBox(height: 12),
                  if (instances.length > 1)
                    DropdownButtonFormField<int>(
                      initialValue: index,
                      isExpanded: true,
                      decoration: const InputDecoration(labelText: S.geometryInstance),
                      items: [
                        for (final (i, item) in instances.indexed)
                          DropdownMenuItem(
                            value: i,
                            child: Text(S.instanceName(item), overflow: TextOverflow.ellipsis),
                          ),
                      ],
                      onChanged: (value) => setState(() => _index = value ?? 0),
                    ),
                  const SizedBox(height: 12),
                  Notice(
                    level: geometry.validated ? NoticeLevel.good : NoticeLevel.caution,
                    title: geometry.validated ? S.geometryValidated : S.geometryNotValidated,
                    body: geometry.validated ? S.geometryValidatedBody : S.geometryNotValidatedBody,
                    dense: true,
                  ),
                  const SizedBox(height: 12),
                  _GeometryFacts(geometry: geometry),
                  const SizedBox(height: 16),
                  Text(S.geometryCandidates, style: theme.textTheme.titleSmall),
                  const SizedBox(height: 4),
                  if (instance.geometryCandidates.isEmpty)
                    Text(S.geometryNoCandidates, style: theme.textTheme.bodySmall)
                  else
                    for (final candidate in instance.geometryCandidates)
                      _CandidateLine(
                        candidate: candidate,
                        chosen:
                            candidate.clusterSize == geometry.clusterSize &&
                            candidate.volumeOffset == geometry.volumeOffset,
                        onUse: busy ? null : () => _apply(instance, candidate.clusterSize, candidate.volumeOffset),
                      ),
                  const SizedBox(height: 12),
                  ExpansionTile(
                    title: const Text(S.geometryAdvanced),
                    subtitle: const Text(S.geometryAdvancedHint),
                    initiallyExpanded: _advanced,
                    onExpansionChanged: (open) => setState(() {
                      _advanced = open;
                      _clusterSize = geometry.clusterSize;
                      _offset.text = '${geometry.volumeOffset}';
                    }),
                    tilePadding: EdgeInsets.zero,
                    childrenPadding: const EdgeInsets.only(bottom: 8),
                    children: [
                      const Notice(level: NoticeLevel.caution, title: S.geometryManualWarning, dense: true),
                      const SizedBox(height: 12),
                      Wrap(
                        spacing: 12,
                        runSpacing: 12,
                        crossAxisAlignment: WrapCrossAlignment.start,
                        children: [
                          SizedBox(
                            width: 180,
                            child: DropdownButtonFormField<int>(
                              initialValue: clusterSizes.contains(_clusterSize) ? _clusterSize : 4096,
                              decoration: const InputDecoration(labelText: S.factClusterSize),
                              items: [
                                for (final size in clusterSizes)
                                  DropdownMenuItem(value: size, child: Text(formats.bytes(size))),
                              ],
                              onChanged: (value) => setState(() => _clusterSize = value ?? 4096),
                            ),
                          ),
                          SizedBox(
                            width: 260,
                            child: TextField(
                              controller: _offset,
                              keyboardType: TextInputType.number,
                              inputFormatters: [FilteringTextInputFormatter.allow(RegExp(r'[0-9 ,._]'))],
                              decoration: InputDecoration(
                                labelText: S.geometryOffsetLabel,
                                helperText: S.geometryOffsetHelp,
                                errorText: _offsetError,
                              ),
                            ),
                          ),
                          FilledButton.tonal(
                            onPressed: busy ? null : () => _applyManual(instance),
                            child: const Text(S.geometryApply),
                          ),
                        ],
                      ),
                    ],
                  ),
                ],
              ),
            ),
          ),
          actions: [TextButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.close))],
        );
      },
    );
  }
}

class _GeometryFacts extends StatelessWidget {
  const _GeometryFacts({required this.geometry});

  final Geometry geometry;

  @override
  Widget build(BuildContext context) {
    final formats = Formats.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        FactRow(
          labelWidth: 200,
          label: S.factClusterSize,
          value: '${formats.bytes(geometry.clusterSize)} · ${S.clusterSource(geometry.clusterSource)}',
        ),
        FactRow(
          labelWidth: 200,
          label: S.factVolumeOffset,
          value: '${formats.offset(geometry.volumeOffset)} · ${S.offsetSource(geometry.offsetSource)}',
        ),
        FactRow(
          labelWidth: 200,
          label: S.evidenceDirs,
          value: S.evidenceDirsValue(formats.count(geometry.indexDirs), formats.count(geometry.indexBlocks)),
        ),
        FactRow(
          labelWidth: 200,
          label: S.evidenceSignatures,
          value: geometry.signatureChecked == 0
              ? S.evidenceNoSignatures
              : S.evidenceSignaturesValue(
                  formats.count(geometry.signatureOk),
                  formats.count(geometry.signatureChecked),
                ),
        ),
        FactRow(
          labelWidth: 200,
          label: S.factVolumeSize,
          value: formats.bytes(geometry.totalClusters * geometry.clusterSize),
        ),
      ],
    );
  }
}

class _CandidateLine extends StatelessWidget {
  const _CandidateLine({required this.candidate, required this.chosen, required this.onUse});

  final Geometry candidate;
  final bool chosen;
  final VoidCallback? onUse;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final status = StatusColors.from(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  S.candidateTitle(formats.bytes(candidate.clusterSize), formats.offset(candidate.volumeOffset)),
                  style: theme.textTheme.bodyMedium,
                ),
                Text(
                  S.candidateEvidence(
                    S.offsetSource(candidate.offsetSource),
                    formats.count(candidate.indexDirs),
                    formats.count(candidate.signatureOk),
                    formats.count(candidate.signatureChecked),
                  ),
                  style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                ),
              ],
            ),
          ),
          const SizedBox(width: 8),
          if (candidate.validated) StatusChip(label: S.candidateValidated, colors: status.forLevel(NoticeLevel.good)),
          const SizedBox(width: 8),
          if (chosen)
            const Chip(label: Text(S.candidateInUse))
          else
            OutlinedButton(onPressed: onUse, child: const Text(S.candidateUse)),
        ],
      ),
    );
  }
}
