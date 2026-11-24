import 'dart:async';

import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import '../flows.dart';
import '../format.dart';
import '../results/details_pane.dart';
import '../results/file_table.dart';
import '../results/file_types.dart';
import '../results/geometry_dialog.dart';
import '../results/results_model.dart';
import '../results/tree_view.dart';
import '../strings.dart';
import '../theme.dart';
import '../widgets/common.dart';
import '../widgets/errors.dart';

/// Screen 3, Results: browse what the scan found, tick what to recover.
///
/// Left the file systems and their folders, in the middle the files of the
/// chosen folder (or the search results below it), right the details of the
/// file in focus. Everything is paged through the engine; the screen holds
/// no file data.
class ResultsScreen extends StatelessWidget {
  const ResultsScreen({super.key, required this.app, required this.model});

  final AppController app;
  final ResultsModel model;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.catalog, app.scan]),
      builder: (context, _) {
        final info = app.catalog.info;
        if (info == null) {
          if (app.scan.isActive) {
            return EmptyState(
              icon: Icons.radar_outlined,
              title: S.resultsScanningTitle,
              text: S.resultsScanningText,
              actions: [
                FilledButton(onPressed: () => app.screen.value = AppScreen.scan, child: const Text(S.showScan)),
              ],
            );
          }
          return EmptyState(
            icon: Icons.folder_open_outlined,
            title: S.noCatalogTitle,
            text: S.noCatalogText,
            actions: [
              FilledButton(onPressed: () => app.screen.value = AppScreen.source, child: const Text(S.chooseSource)),
            ],
          );
        }
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _CatalogNotices(app: app, info: info),
            _Toolbar(app: app, model: model),
            _CarveStatus(app: app),
            const Divider(),
            Expanded(
              child: _Panes(app: app, model: model),
            ),
            const Divider(),
            SelectionBar(app: app),
          ],
        );
      },
    );
  }
}

/// Tree, table and details, sized to the window.
class _Panes extends StatelessWidget {
  const _Panes({required this.app, required this.model});

  final AppController app;
  final ResultsModel model;

  /// From this width on the details sit beside the table; below it they
  /// open over it.
  static const double wide = 1100;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final width = constraints.maxWidth;
        final isWide = width >= wide;
        final treeWidth = width >= 1300 ? 300.0 : (isWide ? 260.0 : 220.0);
        final detailsWidth = width >= 1300 ? 360.0 : 320.0;
        return ListenableBuilder(
          listenable: model,
          builder: (context, _) {
            final table = Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                _Location(app: app, model: model, isWide: isWide),
                const Divider(),
                Expanded(
                  child: FileTable(app: app, model: model),
                ),
              ],
            );
            return Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                SizedBox(
                  width: treeWidth,
                  child: FolderTreeView(app: app, model: model),
                ),
                const VerticalDivider(width: 1),
                Expanded(
                  child: isWide
                      ? table
                      : Stack(
                          children: [
                            Positioned.fill(child: table),
                            if (model.detailsOverlay)
                              Positioned(
                                top: 0,
                                bottom: 0,
                                right: 0,
                                width: detailsWidth.clamp(0, constraints.maxWidth - treeWidth - 24).toDouble(),
                                child: Material(
                                  elevation: 6,
                                  color: Theme.of(context).colorScheme.surfaceContainerLow,
                                  child: DetailsPane(
                                    app: app,
                                    model: model,
                                    onClose: () => model.detailsOverlay = false,
                                  ),
                                ),
                              ),
                          ],
                        ),
                ),
                if (isWide && model.detailsOpen) ...[
                  const VerticalDivider(width: 1),
                  SizedBox(
                    width: detailsWidth,
                    child: DetailsPane(app: app, model: model, onClose: () => model.detailsOpen = false),
                  ),
                ],
              ],
            );
          },
        );
      },
    );
  }
}

/// Where the table is: the path of the folder (each part clickable), or
/// what is being searched.
class _Location extends StatefulWidget {
  const _Location({required this.app, required this.model, required this.isWide});

  final AppController app;
  final ResultsModel model;
  final bool isWide;

  @override
  State<_Location> createState() => _LocationState();
}

class _LocationState extends State<_Location> {
  NodeInfo? _info;
  int? _for;
  CancelToken? _request;

  AppController get app => widget.app;

  @override
  void initState() {
    super.initState();
    app.browse.addListener(_update);
    _update();
  }

  @override
  void dispose() {
    app.browse.removeListener(_update);
    _request?.cancel();
    super.dispose();
  }

  void _update() {
    final folder = app.browse.folder;
    final id = folder?.id;
    final epoch = app.catalog.epoch;
    if (id == null || epoch == null) return;
    final key = id ^ (epoch << 40);
    if (key == _for) return;
    _for = key;
    _request?.cancel();
    final token = _request = CancelToken();
    app.catalog.nodeInfo(id, cancel: token).then<void>((info) {
      if (mounted && identical(_request, token)) setState(() => _info = info);
    }, onError: (Object _) {});
  }

  String _name(NodeRef ref, List<Instance> instances) {
    if (ref.name.isNotEmpty) return ref.name;
    final instance = instances.where((i) => i.root == ref.id).firstOrNull;
    return instance == null ? S.unnamed : S.instanceName(instance);
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final folder = app.browse.folder;
    final info = _info;
    final instances = app.catalog.info?.instances ?? const <Instance>[];
    final current = info != null && info.row.id == folder?.id ? info : null;
    final rows = app.browse.rows;
    final segments = <Widget>[];
    if (current != null) {
      final chain = [...current.ancestors, NodeRef(id: current.row.id, name: current.row.name)];
      for (final (i, ref) in chain.indexed) {
        final last = i == chain.length - 1;
        if (i > 0) segments.add(Icon(Icons.chevron_right, size: 16, color: theme.colorScheme.onSurfaceVariant));
        segments.add(
          last
              ? Text(_name(ref, instances), style: theme.textTheme.titleSmall, overflow: TextOverflow.ellipsis)
              : InkWell(
                  onTap: () => app.browse.open(FolderLocation(ref.id, [for (final a in chain.take(i)) a.id])),
                  child: Text(
                    _name(ref, instances),
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.primary),
                  ),
                ),
        );
      }
    }
    final parent = folder != null && folder.ancestors.isNotEmpty
        ? FolderLocation(folder.ancestors.last, folder.ancestors.sublist(0, folder.ancestors.length - 1))
        : null;
    return SizedBox(
      height: 40,
      child: Row(
        children: [
          IconButton(
            tooltip: S.upOneLevel,
            onPressed: parent == null ? null : () => app.browse.open(parent),
            icon: const Icon(Icons.arrow_upward, size: 18),
          ),
          Expanded(
            child: ListenableBuilder(
              listenable: rows ?? app.browse,
              builder: (context, _) {
                final total = rows?.total;
                return Row(
                  children: [
                    if (app.browse.isSearch) ...[
                      Icon(Icons.search, size: 16, color: theme.colorScheme.onSurfaceVariant),
                      const SizedBox(width: 4),
                      Flexible(
                        child: Text(
                          total == null ? S.searching : S.searchResults(formats.count(total), total),
                          overflow: TextOverflow.ellipsis,
                          style: theme.textTheme.titleSmall,
                        ),
                      ),
                      const SizedBox(width: 8),
                    ],
                    Flexible(
                      child: SingleChildScrollView(
                        scrollDirection: Axis.horizontal,
                        reverse: true,
                        child: Row(children: segments),
                      ),
                    ),
                    if (!app.browse.isSearch && total != null) ...[
                      const SizedBox(width: 12),
                      Text(
                        S.items(formats.count(total), total),
                        style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                      ),
                    ],
                  ],
                );
              },
            ),
          ),
          ListenableBuilder(
            listenable: widget.model,
            builder: (context, _) {
              final open = widget.isWide ? widget.model.detailsOpen : widget.model.detailsOverlay;
              return IconButton(
                tooltip: open ? S.detailsHide : S.detailsShow,
                isSelected: open,
                onPressed: () => widget.isWide ? widget.model.detailsOpen = !open : widget.model.detailsOverlay = !open,
                icon: const Icon(Icons.info_outline),
                selectedIcon: const Icon(Icons.info),
              );
            },
          ),
          const SizedBox(width: 4),
        ],
      ),
    );
  }
}

/// Search, filters, the disk layout, and the search by signature.
class _Toolbar extends StatefulWidget {
  const _Toolbar({required this.app, required this.model});

  final AppController app;
  final ResultsModel model;

  @override
  State<_Toolbar> createState() => _ToolbarState();
}

class _ToolbarState extends State<_Toolbar> {
  late final TextEditingController _search = TextEditingController(text: widget.model.search);
  Timer? _debounce;

  /// The pause after the last key before the search is sent.
  static const Duration searchPause = Duration(milliseconds: 350);

  AppController get app => widget.app;

  @override
  void dispose() {
    _debounce?.cancel();
    _search.dispose();
    super.dispose();
  }

  void _searchChanged(String text) {
    widget.model.search = text;
    _debounce?.cancel();
    _debounce = Timer(searchPause, () => _applySearch(text));
  }

  void _applySearch(String text) {
    app.browse.filters = app.browse.filters.copyWith(query: text.trim());
  }

  void _setGroup(Set<TypeGroup> groups, TypeGroup group, bool selected) {
    final next = {...groups};
    selected ? next.add(group) : next.remove(group);
    app.browse.filters = app.browse.filters.copyWith(exts: () => TypeGroup.extensionsOf(next));
  }

  void _clearSearch() {
    _debounce?.cancel();
    _search.clear();
    widget.model.search = '';
    _applySearch('');
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.browse, app.catalog, app.jobs, app.carve]),
      builder: (context, _) {
        final filters = app.browse.filters;
        final groups = TypeGroup.groupsOf(filters.exts);
        final main = app.catalog.info?.mainInstance;
        return Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 16, 8),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              LayoutBuilder(
                builder: (context, constraints) {
                  final compact = constraints.maxWidth < 1000;
                  return Row(
                    children: [
                      Expanded(
                        child: ConstrainedBox(
                          constraints: const BoxConstraints(maxWidth: 480),
                          child: TextField(
                            controller: _search,
                            onChanged: _searchChanged,
                            onSubmitted: (text) {
                              _debounce?.cancel();
                              _applySearch(text);
                            },
                            decoration: InputDecoration(
                              prefixIcon: const Icon(Icons.search),
                              hintText: S.searchHint,
                              suffixIcon: _search.text.isEmpty
                                  ? null
                                  : IconButton(
                                      tooltip: S.searchClear,
                                      onPressed: _clearSearch,
                                      icon: const Icon(Icons.clear),
                                    ),
                            ),
                          ),
                        ),
                      ),
                      const SizedBox(width: 12),
                      if (main != null) _GeometryChip(app: app, instance: main, compact: compact),
                      const SizedBox(width: 8),
                      _CarveButton(app: app, compact: compact),
                    ],
                  );
                },
              ),
              const SizedBox(height: 8),
              LayoutBuilder(
                builder: (context, constraints) {
                  // Five type chips need room; a narrow window gets one menu.
                  final chips = constraints.maxWidth >= 1000;
                  return Wrap(
                    spacing: 6,
                    runSpacing: 6,
                    crossAxisAlignment: WrapCrossAlignment.center,
                    children: [
                      FilterChip(
                        label: const Text(S.filterDeleted),
                        tooltip: S.filterDeletedTip,
                        selected: filters.showDeleted,
                        onSelected: (value) {
                          app.browse.filters = filters.copyWith(showDeleted: value);
                          // Seeing deleted files and ticking them means wanting them.
                          app.recover.options = app.recover.options.copyWith(includeDeleted: value);
                        },
                      ),
                      const SizedBox(height: 24, child: VerticalDivider(width: 12)),
                      if (chips)
                        for (final group in TypeGroup.values)
                          FilterChip(
                            avatar: Icon(group.icon, size: 16),
                            label: Text(group.label),
                            selected: groups.contains(group),
                            onSelected: (value) => _setGroup(groups, group, value),
                          )
                      else
                        _TypeMenu(groups: groups, onChanged: (group, value) => _setGroup(groups, group, value)),
                      const SizedBox(height: 24, child: VerticalDivider(width: 12)),
                      _ConditionMenu(app: app),
                    ],
                  );
                },
              ),
            ],
          ),
        );
      },
    );
  }
}

/// Which conditions (ratings) the table shows.
enum ConditionFilter {
  all(null),
  hideDamaged([Rating.good, Rating.uncertain, Rating.unsupported]),
  goodAndUncertain([Rating.good, Rating.uncertain]),
  goodOnly([Rating.good]);

  const ConditionFilter(this.ratings);

  final List<Rating>? ratings;

  static ConditionFilter of(List<Rating>? ratings) {
    for (final value in values) {
      final mine = value.ratings;
      if (mine == null && (ratings == null || ratings.isEmpty)) return value;
      if (mine != null && ratings != null && mine.length == ratings.length && mine.every(ratings.contains)) {
        return value;
      }
    }
    return all;
  }
}

/// The type filter as one menu, for narrow windows.
class _TypeMenu extends StatelessWidget {
  const _TypeMenu({required this.groups, required this.onChanged});

  final Set<TypeGroup> groups;
  final void Function(TypeGroup group, bool selected) onChanged;

  @override
  Widget build(BuildContext context) {
    return MenuAnchor(
      menuChildren: [
        for (final group in TypeGroup.values)
          CheckboxMenuButton(
            value: groups.contains(group),
            onChanged: (value) => onChanged(group, value ?? false),
            closeOnActivate: false,
            child: Text(group.label),
          ),
      ],
      builder: (context, controller, _) => FilterChip(
        avatar: const Icon(Icons.category_outlined, size: 16),
        label: Text(
          S.typeLabel([
            for (final g in TypeGroup.values)
              if (groups.contains(g)) g.label,
          ]),
        ),
        selected: groups.isNotEmpty,
        showCheckmark: false,
        tooltip: S.typeTip,
        onSelected: (_) => controller.isOpen ? controller.close() : controller.open(),
      ),
    );
  }
}

class _ConditionMenu extends StatelessWidget {
  const _ConditionMenu({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    final current = ConditionFilter.of(app.browse.filters.ratings);
    return MenuAnchor(
      menuChildren: [
        for (final value in ConditionFilter.values)
          MenuItemButton(
            leadingIcon: Icon(value == current ? Icons.radio_button_checked : Icons.radio_button_unchecked, size: 18),
            onPressed: () => app.browse.filters = app.browse.filters.copyWith(ratings: () => value.ratings),
            child: Text(S.conditionFilter(value.index)),
          ),
      ],
      builder: (context, controller, _) => FilterChip(
        avatar: const Icon(Icons.tune, size: 16),
        label: Text(S.conditionLabel(S.conditionFilter(current.index))),
        selected: current != ConditionFilter.all,
        showCheckmark: false,
        tooltip: S.conditionTip,
        onSelected: (_) => controller.isOpen ? controller.close() : controller.open(),
      ),
    );
  }
}

class _GeometryChip extends StatelessWidget {
  const _GeometryChip({required this.app, required this.instance, this.compact = false});

  final AppController app;
  final Instance instance;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final validated = instance.geometry.validated;
    final status = StatusColors.from(context);
    final (background, foreground) = status.forLevel(validated ? NoticeLevel.good : NoticeLevel.caution);
    return Tooltip(
      message: S.geometryChipTip,
      child: ActionChip(
        avatar: Icon(validated ? Icons.verified_outlined : Icons.warning_amber_rounded, size: 18, color: foreground),
        backgroundColor: background,
        side: BorderSide.none,
        label: Text(switch ((validated, compact)) {
          (true, false) => S.geometryChipValidated,
          (false, false) => S.geometryChipUnverified,
          (true, true) => S.geometryChipValidatedShort,
          (false, true) => S.geometryChipUnverifiedShort,
        }, style: TextStyle(color: foreground)),
        onPressed: () => showGeometryDialog(context, app),
      ),
    );
  }
}

class _CarveButton extends StatelessWidget {
  const _CarveButton({required this.app, this.compact = false});

  final AppController app;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final String? why;
    if (!app.carve.hasCarveHits) {
      why = S.carveNoHits;
    } else if (!app.catalog.sourceAttached) {
      why = S.carveNoSource;
    } else if (app.jobs.isBusy) {
      why = S.carveBusy;
    } else {
      why = null;
    }
    return Tooltip(
      message: why ?? S.carveTip,
      child: FilledButton.tonalIcon(
        onPressed: why == null ? () => _start(context) : null,
        icon: const Icon(Icons.manage_search),
        label: Text(compact ? S.carveActionShort : S.carveAction),
      ),
    );
  }

  Future<void> _start(BuildContext context) async {
    final kinds = await showDialog<List<String>?>(
      context: context,
      builder: (context) => _CarveDialog(kinds: app.engine.hello?.carveKinds ?? const []),
    );
    if (kinds == null || !context.mounted) return;
    await attempt(context, () => app.carve.start(kinds: kinds.isEmpty ? null : kinds), errorTitle: S.carveFailed);
  }
}

/// Explains carving and lets the user narrow the file types. Returns the
/// kinds (empty: all of them), or `null` if cancelled.
class _CarveDialog extends StatefulWidget {
  const _CarveDialog({required this.kinds});

  final List<String> kinds;

  @override
  State<_CarveDialog> createState() => _CarveDialogState();
}

class _CarveDialogState extends State<_CarveDialog> {
  late final Set<String> _chosen = {...widget.kinds};

  @override
  Widget build(BuildContext context) {
    final all = _chosen.length == widget.kinds.length;
    return AlertDialog(
      title: const Text(S.carveTitle),
      content: SizedBox(
        width: 520,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              const Text(S.carveBody),
              const SizedBox(height: 12),
              Text(S.carveKinds, style: Theme.of(context).textTheme.titleSmall),
              const SizedBox(height: 6),
              Wrap(
                spacing: 6,
                runSpacing: 6,
                children: [
                  for (final kind in widget.kinds)
                    FilterChip(
                      label: Text(kind.toUpperCase()),
                      selected: _chosen.contains(kind),
                      onSelected: (value) => setState(() => value ? _chosen.add(kind) : _chosen.remove(kind)),
                    ),
                ],
              ),
            ],
          ),
        ),
      ),
      actions: [
        TextButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.cancel)),
        FilledButton(
          onPressed: _chosen.isEmpty
              ? null
              : () => Navigator.of(context).pop(
                  all ? <String>[] : _chosen.toList()
                    ..sort(),
                ),
          child: const Text(S.carveStart),
        ),
      ],
    );
  }
}

/// The search by signature while it runs, and its outcome.
class _CarveStatus extends StatelessWidget {
  const _CarveStatus({required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: app.carve,
      builder: (context, _) {
        final carve = app.carve;
        final formats = Formats.of(context);
        if (carve.isActive) {
          final progress = carve.progress;
          return Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: Row(
              children: [
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Text(
                        progress == null
                            ? S.carveStarting
                            : S.carveProgress(
                                formats.count(progress.candidatesDone),
                                formats.count(progress.candidatesTotal),
                                formats.count(progress.found),
                                formats.bytes(progress.bytesFound),
                                progress.etaSeconds == null ? null : formats.duration(progress.etaSeconds!),
                              ),
                        maxLines: 2,
                        overflow: TextOverflow.ellipsis,
                      ),
                      const SizedBox(height: 4),
                      LinearProgressIndicator(value: progress?.fraction),
                    ],
                  ),
                ),
                const SizedBox(width: 12),
                OutlinedButton(
                  onPressed: carve.phase == JobPhase.running ? () => unawaited(carve.cancel()) : null,
                  child: Text(carve.phase == JobPhase.cancelling ? S.stopping : S.carveStop),
                ),
              ],
            ),
          );
        }
        if (carve.phase != JobPhase.done) return const SizedBox.shrink();
        final result = carve.result;
        final failure = carve.failure;
        final dismiss = TextButton(onPressed: carve.reset, child: const Text(S.dismiss));
        if (failure != null) {
          return Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: ErrorNotice(error: failure, title: S.carveFailed, actions: [dismiss]),
          );
        }
        final carvedRoot = app.catalog.info?.mainInstance;
        final carvedId = carvedRoot?.virtual.carved;
        return Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
          child: Notice(
            level: NoticeLevel.good,
            dense: true,
            title: S.carveDone(
              formats.count(result?.found ?? 0),
              formats.bytes(result?.bytesFound ?? 0),
              cancelled: result?.status == JobStatus.cancelled,
            ),
            actions: [
              if (carvedId != null && carvedRoot != null && (result?.found ?? 0) > 0)
                TextButton(
                  onPressed: () => app.browse.open(FolderLocation(carvedId, [carvedRoot.root])),
                  child: const Text(S.carveShow),
                ),
              dismiss,
            ],
          ),
        );
      },
    );
  }
}

/// The catalog's warnings, where the user will see them: one line each,
/// the most important first, with what to do.
class _CatalogNotices extends StatelessWidget {
  const _CatalogNotices({required this.app, required this.info});

  final AppController app;
  final CatalogInfo info;

  @override
  Widget build(BuildContext context) {
    final formats = Formats.of(context);
    final busy = app.jobs.isBusy;
    final items = <_NoticeItem>[];
    if (!info.sourceAttached) {
      items.add(
        _NoticeItem(
          level: NoticeLevel.caution,
          icon: Icons.link_off,
          title: S.browseOnlyTitle,
          body: S.browseOnlyBody,
          action: (S.attachAction, busy ? null : () => attachSourceFlow(context, app)),
        ),
      );
    }
    final warnings = {for (final w in info.warnings) w.kind: w};
    if (!info.scanComplete || warnings.containsKey(WarningCode.scanIncomplete)) {
      items.add(
        _NoticeItem(
          level: NoticeLevel.caution,
          title: S.incompleteTitle(formats.bytes(info.scannedBytes), formats.bytes(info.range.length)),
          body: S.warningAdvice(WarningCode.scanIncomplete),
          details: warnings[WarningCode.scanIncomplete]?.message,
          action: info.sourceAttached ? (S.resumeScan, busy ? null : () => resumeScan(context, app)) : null,
        ),
      );
    }
    if (info.outOfRangeFiles > 0 || warnings.containsKey(WarningCode.outOfRangeFiles)) {
      items.add(
        _NoticeItem(
          level: NoticeLevel.caution,
          title: S.outOfRangeTitle(formats.count(info.outOfRangeFiles), formats.bytes(info.outOfRangeBytes)),
          body: S.warningAdvice(WarningCode.outOfRangeFiles),
          details: warnings[WarningCode.outOfRangeFiles]?.message,
          action: info.sourceAttached && app.source.current?.info != null
              ? (S.scanWholeDisk, busy ? null : () => startScan(context, app))
              : null,
        ),
      );
    }
    for (final warning in info.warnings) {
      if (warning.kind == WarningCode.scanIncomplete || warning.kind == WarningCode.outOfRangeFiles) continue;
      items.add(
        _NoticeItem(
          level: NoticeLevel.ofWarning(warning.kind),
          title: S.warningHeadline(warning.kind),
          body: S.warningAdvice(warning.kind),
          details: warning.message,
          action: warning.kind == WarningCode.sessionFileAtRisk || warning.kind == WarningCode.sessionFileNotSaved
              ? (S.saveSessionElsewhere, busy ? null : () => saveSessionElsewhere(context, app))
              : null,
        ),
      );
    }
    if (info.badBytes > 0) {
      items.add(
        _NoticeItem(
          level: NoticeLevel.caution,
          title: S.badSectorsTitle(formats.bytes(info.badBytes)),
          body: S.badSectorsCatalog,
        ),
      );
    }
    if (items.isEmpty) return const SizedBox.shrink();
    items.sort((a, b) => b.level.index - a.level.index);
    const shown = 2;
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 8, 16, 0),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          for (final item in items.take(shown))
            Padding(
              padding: const EdgeInsets.only(bottom: 4),
              child: _CompactNotice(item: item),
            ),
          if (items.length > shown)
            Align(
              alignment: Alignment.centerLeft,
              child: TextButton(
                onPressed: () => _showAll(context, items),
                child: Text(S.moreNotices(items.length - shown)),
              ),
            ),
        ],
      ),
    );
  }

  void _showAll(BuildContext context, List<_NoticeItem> items) {
    showDialog<void>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text(S.noticesTitle),
        content: SizedBox(
          width: 560,
          child: SingleChildScrollView(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                for (final item in items)
                  Padding(padding: const EdgeInsets.only(bottom: 8), child: item.toNotice(context, closeFirst: true)),
              ],
            ),
          ),
        ),
        actions: [TextButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.close))],
      ),
    );
  }
}

final class _NoticeItem {
  const _NoticeItem({required this.level, required this.title, this.body, this.details, this.action, this.icon});

  final NoticeLevel level;
  final String title;
  final String? body;
  final String? details;
  final IconData? icon;

  /// A label and what it does (`null`: disabled right now).
  final (String, VoidCallback?)? action;

  Notice toNotice(BuildContext context, {bool closeFirst = false}) => Notice(
    level: level,
    icon: icon,
    title: title,
    body: body,
    details: details,
    dense: true,
    actions: [
      if (action case (final label, final onPressed))
        OutlinedButton(
          onPressed: onPressed == null
              ? null
              : () {
                  if (closeFirst) Navigator.of(context).pop();
                  onPressed();
                },
          child: Text(label),
        ),
    ],
  );
}

/// One line: icon, headline, the action, and "More" for the explanation.
class _CompactNotice extends StatelessWidget {
  const _CompactNotice({required this.item});

  final _NoticeItem item;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final (background, foreground) = StatusColors.from(context).forLevel(item.level);
    final hasMore = (item.body?.isNotEmpty ?? false) || (item.details?.isNotEmpty ?? false);
    return Material(
      color: background,
      borderRadius: BorderRadius.circular(8),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 2),
        child: Row(
          children: [
            Icon(item.icon ?? Notice.iconFor(item.level), size: 18, color: foreground),
            const SizedBox(width: 10),
            Expanded(
              child: Text(
                item.title,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: theme.textTheme.bodyMedium?.copyWith(color: foreground, fontWeight: FontWeight.w500),
              ),
            ),
            if (hasMore)
              TextButton(
                style: TextButton.styleFrom(foregroundColor: foreground),
                onPressed: () => showDialog<void>(
                  context: context,
                  builder: (context) => AlertDialog(
                    content: SizedBox(width: 520, child: item.toNotice(context, closeFirst: true)),
                    actions: [TextButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.close))],
                  ),
                ),
                child: const Text(S.moreAbout),
              ),
            if (item.action case (final label, final onPressed))
              TextButton(
                style: TextButton.styleFrom(foregroundColor: foreground),
                onPressed: onPressed,
                child: Text(label),
              ),
          ],
        ),
      ),
    );
  }
}

/// Always visible at the bottom: what is ticked, and the way to recover it.
class SelectionBar extends StatelessWidget {
  const SelectionBar({super.key, required this.app});

  final AppController app;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([app.selection, app.recover]),
      builder: (context, _) {
        final theme = Theme.of(context);
        final formats = Formats.of(context);
        final status = StatusColors.from(context);
        final empty = app.selection.isEmpty;
        final summary = app.recover.summary;
        final error = app.recover.summaryError;
        Widget text;
        if (empty) {
          text = Text(S.selectionEmpty, style: theme.textTheme.bodyMedium);
        } else if (error != null) {
          text = Row(
            children: [
              Flexible(child: Text(S.selectionFailed, style: theme.textTheme.bodyMedium)),
              TextButton(onPressed: app.recover.refreshSummary, child: const Text(S.retry)),
            ],
          );
        } else if (summary == null) {
          text = Text(S.selectionCounting, style: theme.textTheme.bodyMedium);
        } else {
          text = Wrap(
            spacing: 10,
            runSpacing: 4,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              Text(
                S.selectionSummary(formats.count(summary.files), summary.files, formats.bytes(summary.bytes)),
                style: theme.textTheme.titleSmall,
              ),
              if (summary.damaged > 0)
                StatusChip(
                  label: S.countDamaged(formats.count(summary.damaged)),
                  colors: status.forRating(Rating.damaged),
                ),
              if (summary.uncertain > 0)
                StatusChip(
                  label: S.countUncertain(formats.count(summary.uncertain)),
                  colors: status.forRating(Rating.uncertain),
                ),
              if (summary.unsupported > 0)
                StatusChip(
                  label: S.countUnsupported(formats.count(summary.unsupported)),
                  colors: status.forRating(Rating.unsupported),
                  tooltip: S.ratingExplanation(Rating.unsupported),
                ),
            ],
          );
        }
        return Material(
          color: theme.colorScheme.surfaceContainer,
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
            child: Row(
              children: [
                Icon(Icons.check_box_outlined, color: theme.colorScheme.primary),
                const SizedBox(width: 12),
                Expanded(child: text),
                if (!empty) TextButton(onPressed: app.selection.clear, child: const Text(S.selectionClear)),
                const SizedBox(width: 8),
                FilledButton.icon(
                  onPressed: empty ? null : () => app.screen.value = AppScreen.recover,
                  icon: const Icon(Icons.save_alt),
                  label: const Text(S.recoverAction),
                ),
              ],
            ),
          ),
        );
      },
    );
  }
}
