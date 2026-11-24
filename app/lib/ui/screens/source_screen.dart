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

/// Screen 1, Source: choose the disk (or partition) that was formatted, a
/// disk image, or a saved scan.
///
/// Before anything is open it lists the disks Windows reports, as cards
/// with their volumes and risk badges. Choosing a disk opens it read-only
/// (`open_source`), which reads its real partition table and the engine's
/// warnings about it; then each partition, or the whole disk, can be
/// scanned, or the disk copied to an image file.
class SourceScreen extends StatelessWidget {
  const SourceScreen({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.source, app.catalog, app.jobs]),
      builder: (context, _) {
        final source = app.source;
        final current = source.current;
        return ScreenFrame(
          title: S.sourceTitle,
          purpose: S.sourcePurpose,
          actions: [
            OutlinedButton.icon(
              onPressed: source.isBusy || app.jobs.isBusy ? null : () => openImageFile(context, app),
              icon: const Icon(Icons.insert_drive_file_outlined),
              label: const Text(S.openImage),
            ),
            OutlinedButton.icon(
              onPressed: source.isBusy || app.jobs.isBusy ? null : () => openSessionFile(context, app),
              icon: const Icon(Icons.history),
              label: const Text(S.openSession),
            ),
          ],
          child: current == null ? _DiskList(app: app) : _OpenSource(app: app, current: current),
        );
      },
    );
  }
}

/// The disks to choose from, with the empty and error states.
class _DiskList extends StatelessWidget {
  const _DiskList({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final source = app.source;
    final hello = app.engine.hello;
    final disks = source.disks == null ? null : orderDisks(source.disks!);
    final notElevated = hello != null && !hello.elevated;
    final lastSession = source.sessionPath;
    final children = <Widget>[
      if (lastSession != null)
        Notice(
          level: NoticeLevel.info,
          icon: Icons.history,
          title: S.lastSessionTitle,
          body: lastSession,
          actions: [
            FilledButton.tonal(
              onPressed: () => openSessionFile(context, app, path: lastSession),
              child: const Text(S.lastSessionOpen),
            ),
          ],
        ),
      if (notElevated) const Notice(level: NoticeLevel.danger, title: S.notElevatedTitle, body: S.notElevatedBody),
      Row(
        children: [
          Expanded(child: Text(S.disksHeading, style: Theme.of(context).textTheme.titleMedium)),
          if (source.isLoadingDisks)
            const Padding(
              padding: EdgeInsets.only(right: 8),
              child: SizedBox(width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2)),
            ),
          IconButton(
            tooltip: S.refreshDisks,
            onPressed: source.isLoadingDisks ? null : () => unawaited(source.refreshDisks()),
            icon: const Icon(Icons.refresh),
          ),
        ],
      ),
      if (source.disksError != null)
        ErrorNotice(
          error: source.disksError!,
          title: S.disksFailed,
          actions: [TextButton(onPressed: () => unawaited(source.refreshDisks()), child: const Text(S.retry))],
        ),
      if (disks != null && disks.isEmpty && source.disksError == null)
        _NoDisks(app: app, notElevated: notElevated)
      else if (disks != null)
        LayoutBuilder(
          builder: (context, constraints) {
            final columns = constraints.maxWidth >= 720 ? 2 : 1;
            final width = (constraints.maxWidth - (columns - 1) * 12) / columns;
            return Wrap(
              spacing: 12,
              runSpacing: 12,
              children: [
                for (final disk in disks)
                  SizedBox(
                    width: width,
                    child: DiskCard(app: app, disk: disk),
                  ),
              ],
            );
          },
        ),
      const SizedBox(height: 8),
      Text(S.diskListFooter, style: Theme.of(context).textTheme.bodySmall),
    ];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [for (final child in children) Padding(padding: const EdgeInsets.only(bottom: 12), child: child)],
    );
  }
}

class _NoDisks extends StatelessWidget {
  const _NoDisks({required this.app, required this.notElevated});

  final AppController app;
  final bool notElevated;

  @override
  Widget build(BuildContext context) {
    final os = app.engine.hello?.os;
    final String text;
    if (notElevated) {
      text = S.noDisksNotElevated;
    } else if (os != null && os != 'windows') {
      text = S.noDisksNotWindows;
    } else {
      text = S.noDisksText;
    }
    return Card(
      child: SizedBox(
        height: 260,
        child: EmptyState(
          icon: Icons.usb_off,
          title: S.noDisksTitle,
          text: text,
          actions: [
            FilledButton.tonal(
              onPressed: () => unawaited(app.source.refreshDisks()),
              child: const Text(S.refreshDisks),
            ),
            OutlinedButton(onPressed: () => openImageFile(context, app), child: const Text(S.openImage)),
          ],
        ),
      ),
    );
  }
}

/// One physical disk as Windows reports it: model, size, bus, the volumes
/// it carries, and badges for what makes rescuing it risky.
class DiskCard extends StatelessWidget {
  const DiskCard({super.key, required this.app, required this.disk});

  final AppController app;
  final Disk disk;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final status = StatusColors.from(context);
    final flags = disk.flags;
    final noMedia = flags.contains(DiskFlag.noMedia) || disk.size == 0;
    final badges = <Widget>[
      if (disk.isSystem)
        StatusChip(
          label: S.badgeSystem,
          colors: status.forLevel(NoticeLevel.danger),
          icon: Icons.window,
          tooltip: S.badgeSystemTip,
        ),
      if (disk.trim == true)
        StatusChip(
          label: S.badgeTrim,
          colors: status.forLevel(NoticeLevel.caution),
          icon: Icons.warning_amber_rounded,
          tooltip: S.badgeTrimTip,
        ),
      if (disk.media == MediaKind.ssd && disk.trim != true)
        StatusChip(label: S.badgeSsd, colors: status.forLevel(NoticeLevel.caution), tooltip: S.badgeTrimTip),
      if (disk.removable) StatusChip(label: S.badgeRemovable, colors: status.forLevel(NoticeLevel.info)),
      if (flags.contains(DiskFlag.virtual))
        StatusChip(label: S.badgeVirtual, colors: status.forLevel(NoticeLevel.info)),
      if (flags.contains(DiskFlag.spaces))
        StatusChip(label: S.badgeSpaces, colors: status.forLevel(NoticeLevel.caution), tooltip: S.badgeUnsupportedTip),
      if (flags.contains(DiskFlag.dynamicDisk))
        StatusChip(label: S.badgeDynamic, colors: status.forLevel(NoticeLevel.caution), tooltip: S.badgeUnsupportedTip),
      if (noMedia) StatusChip(label: S.badgeNoMedia, colors: status.forLevel(NoticeLevel.info)),
    ];
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Icon(_iconFor(disk), size: 32, color: theme.colorScheme.primary),
                const SizedBox(width: 12),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        S.diskTitle(disk.model, disk.number),
                        style: theme.textTheme.titleMedium,
                        maxLines: 2,
                        overflow: TextOverflow.ellipsis,
                      ),
                      Text(
                        S.diskSubtitle(disk.number, formats.bytes(disk.size), disk.bus, media: S.media(disk.media)),
                        style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                      ),
                    ],
                  ),
                ),
              ],
            ),
            if (badges.isNotEmpty) ...[const SizedBox(height: 8), Wrap(spacing: 6, runSpacing: 6, children: badges)],
            const SizedBox(height: 10),
            if (disk.partitions.isEmpty)
              Text(S.noVolumes, style: theme.textTheme.bodySmall)
            else
              for (final partition in disk.partitions) _VolumeLine(partition: partition),
            const SizedBox(height: 12),
            Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                FilledButton.tonal(
                  onPressed: noMedia || app.source.isBusy || app.jobs.isBusy ? null : () => _open(context),
                  child: const Text(S.chooseDisk),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }

  Future<void> _open(BuildContext context) async {
    await attempt(context, () => app.source.openDisk(disk), errorTitle: S.openDiskFailed);
  }

  static IconData _iconFor(Disk disk) {
    if (disk.flags.contains(DiskFlag.virtual)) return Icons.dns_outlined;
    if (disk.removable) return Icons.sd_card_outlined;
    if (disk.bus.toUpperCase() == 'USB') return Icons.usb;
    return Icons.storage;
  }
}

/// A volume on a disk card: drive letter, label, file system, size.
class _VolumeLine extends StatelessWidget {
  const _VolumeLine({required this.partition});

  final Partition partition;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 2),
      child: Row(
        children: [
          SizedBox(
            width: 36,
            child: Text(partition.letters.isEmpty ? '–' : partition.letters.first, style: theme.textTheme.titleSmall),
          ),
          Expanded(
            child: Text(
              S.volumeLine(partition.label, S.fileSystem(partition.fs), formats.bytes(partition.size)),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: theme.textTheme.bodyMedium,
            ),
          ),
        ],
      ),
    );
  }
}

/// The open source: what it is, the engine's warnings, and what to do with
/// it (scan a partition, the whole disk, or make an image).
class _OpenSource extends StatelessWidget {
  const _OpenSource({required this.app, required this.current});

  final AppController app;
  final OpenedSource current;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final info = current.info;
    final busy = app.source.isBusy || app.jobs.isBusy;
    final catalog = app.catalog.info;
    final fromSession = current.table == null;
    final warnings = [...current.warnings]
      ..sort((a, b) => NoticeLevel.ofWarning(b.kind).index - NoticeLevel.ofWarning(a.kind).index);

    final children = <Widget>[
      Card(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Row(
            children: [
              Icon(
                info.kind == SourceKind.disk ? Icons.storage : Icons.insert_drive_file_outlined,
                size: 32,
                color: theme.colorScheme.primary,
              ),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(describeSource(info, formats), style: theme.textTheme.titleMedium),
                    Text(
                      [
                        if (info.kind == SourceKind.disk) ?info.serial,
                        info.path,
                        S.sectorSize(info.sectorSize, info.physicalSectorSize),
                      ].where((s) => s.isNotEmpty).join(' · '),
                      style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                    ),
                    if (app.source.sessionPath != null)
                      Text(
                        S.sessionFileLine(app.source.sessionPath!),
                        style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                      ),
                  ],
                ),
              ),
              const SizedBox(width: 12),
              OutlinedButton(onPressed: busy ? null : () => _close(context), child: const Text(S.chooseOtherSource)),
            ],
          ),
        ),
      ),
      if (!current.attached)
        Notice(
          level: NoticeLevel.caution,
          icon: Icons.link_off,
          title: S.browseOnlyTitle,
          body: S.browseOnlyBody,
          details: current.attachError?.message,
          actions: [
            FilledButton.tonal(
              onPressed: busy ? null : () => attachSourceFlow(context, app),
              child: const Text(S.attachAction),
            ),
          ],
        ),
      for (final warning in warnings)
        Notice(
          level: NoticeLevel.ofWarning(warning.kind),
          title: S.warningHeadline(warning.kind),
          body: S.warningAdvice(warning.kind),
          details: warning.message,
        ),
      if (catalog != null)
        Notice(
          level: NoticeLevel.good,
          title: S.resultsReadyTitle,
          body: S.resultsReadyBody(formats.count(catalog.instances.fold(0, (sum, i) => sum + i.files))),
          actions: [
            FilledButton(onPressed: () => app.screen.value = AppScreen.results, child: const Text(S.showResults)),
          ],
        ),
      if (!fromSession && current.attached) _ScanChoices(app: app, current: current, busy: busy),
      if (current.attached)
        Card(
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: _ChoiceLine(
              icon: Icons.album_outlined,
              title: S.imageInsteadTitle,
              subtitle: S.imageInsteadBody,
              action: OutlinedButton(
                onPressed: busy ? null : () => app.screen.value = AppScreen.image,
                child: const Text(S.createImage),
              ),
            ),
          ),
        ),
    ];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [for (final child in children) Padding(padding: const EdgeInsets.only(bottom: 12), child: child)],
    );
  }

  Future<void> _close(BuildContext context) async {
    if (app.catalog.hasCatalog) {
      final yes = await confirm(
        context,
        title: S.closeSourceTitle,
        body: app.source.sessionPath == null ? S.closeSourceBodyUnsaved : S.closeSourceBody(app.source.sessionPath!),
        confirmLabel: S.closeSourceConfirm,
      );
      if (!yes || !context.mounted) return;
    }
    await attempt(context, app.source.close);
  }
}

/// What can be scanned: each partition of the real table, or the whole
/// source.
class _ScanChoices extends StatelessWidget {
  const _ScanChoices({required this.app, required this.current, required this.busy});

  final AppController app;
  final OpenedSource current;
  final bool busy;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final info = current.info;
    return Section(
      title: S.scanChoiceTitle,
      subtitle: S.scanChoiceHint,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          for (final partition in current.partitions)
            Padding(
              padding: const EdgeInsets.only(bottom: 8),
              child: _ChoiceLine(
                icon: Icons.pie_chart_outline,
                title: S.partitionTitle(partition.index, partition.letters, partition.label),
                subtitle: S.partitionSubtitle(
                  S.fileSystem(partition.fs),
                  formats.bytes(partition.size),
                  partition.type,
                  formats.offset(partition.offset),
                ),
                action: FilledButton.tonal(
                  onPressed: busy ? null : () => startScan(context, app, range: partition.range),
                  child: const Text(S.scanPartition),
                ),
              ),
            ),
          if (current.partitions.isEmpty) Text(S.noPartitions, style: theme.textTheme.bodyMedium),
          const Divider(height: 20),
          _ChoiceLine(
            icon: Icons.storage,
            title: S.scanWholeTitle(info.kind == SourceKind.disk),
            subtitle: S.scanWholeSubtitle(formats.bytes(info.size)),
            action: FilledButton.tonal(
              onPressed: busy ? null : () => startScan(context, app),
              child: Text(S.scanWhole(info.kind == SourceKind.disk)),
            ),
          ),
        ],
      ),
    );
  }
}

/// An icon, a title with a line of explanation, and the button for it.
class _ChoiceLine extends StatelessWidget {
  const _ChoiceLine({required this.icon, required this.title, required this.subtitle, required this.action});

  final IconData icon;
  final String title;
  final String subtitle;
  final Widget action;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Row(
      children: [
        Icon(icon, color: theme.colorScheme.onSurfaceVariant),
        const SizedBox(width: 12),
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(title, style: theme.textTheme.titleSmall),
              Text(subtitle, style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant)),
            ],
          ),
        ),
        const SizedBox(width: 12),
        action,
      ],
    );
  }
}

/// The disks in the order a rescuer looks for them: ordinary disks by
/// number, then the disk Windows runs from, then readers without a medium.
List<Disk> orderDisks(List<Disk> disks) {
  int rank(Disk disk) {
    if (disk.flags.contains(DiskFlag.noMedia) || disk.size == 0) return 2;
    if (disk.isSystem) return 1;
    return 0;
  }

  return [...disks]..sort((a, b) {
    final byRank = rank(a) - rank(b);
    return byRank != 0 ? byRank : a.number - b.number;
  });
}
