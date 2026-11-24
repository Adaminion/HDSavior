/// The file table of the Results screen: a virtualized list of fixed-height
/// rows, fetched from the engine page by page, with sortable columns, tick
/// boxes and keyboard navigation.
library;

import 'package:flutter/services.dart';
import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import '../format.dart';
import '../strings.dart';
import '../theme.dart';
import '../widgets/common.dart';
import '../widgets/errors.dart';
import 'file_types.dart';
import 'results_model.dart';
import 'tree_view.dart' show checkboxValue;

/// Height of one row of the table.
const double tableRowExtent = 32;

/// Which columns fit into [width].
@immutable
final class _Columns {
  const _Columns(double width) : modified = width >= 600, status = width >= 470, size = width >= 360;

  final bool size;
  final bool modified;
  final bool status;

  static const double check = 40;
  static const double sizeWidth = 92;
  static const double modifiedWidth = 156;
  static const double statusWidth = 128;
}

/// See the library documentation.
///
/// Keys: Up/Down/Page Up/Page Down/Home/End move, Space ticks, Enter opens
/// a folder.
class FileTable extends StatefulWidget {
  const FileTable({super.key, required this.app, required this.model});

  final AppController app;
  final ResultsModel model;

  @override
  State<FileTable> createState() => _FileTableState();
}

class _FileTableState extends State<FileTable> {
  final ScrollController _scroll = ScrollController();
  final FocusNode _focus = FocusNode(debugLabel: 'file table');
  PagedRows? _shown;

  /// The last click, to tell a double click without delaying the single
  /// one (a double-tap recognizer would hold every click back).
  int? _lastClickIndex;
  DateTime? _lastClickTime;

  /// The longest pause between the two clicks of a double click.
  static const Duration doubleClickTime = Duration(milliseconds: 500);

  AppController get app => widget.app;
  ResultsModel get model => widget.model;

  @override
  void initState() {
    super.initState();
    app.browse.addListener(_onBrowseChanged);
    _shown = app.browse.rows;
  }

  @override
  void dispose() {
    app.browse.removeListener(_onBrowseChanged);
    _scroll.dispose();
    _focus.dispose();
    super.dispose();
  }

  /// Another listing starts at the top.
  void _onBrowseChanged() {
    final rows = app.browse.rows;
    if (identical(rows, _shown)) return;
    _shown = rows;
    if (_scroll.hasClients) _scroll.jumpTo(0);
  }

  void _clicked(int index, NodeRow row) {
    final now = DateTime.now();
    final last = _lastClickTime;
    final isDouble = _lastClickIndex == index && last != null && now.difference(last) <= doubleClickTime;
    _lastClickIndex = index;
    _lastClickTime = isDouble ? null : now;
    _focus.requestFocus();
    _select(index, row);
    if (isDouble) _activate(row);
  }

  void _select(int index, NodeRow row) {
    model.tableCursor = index;
    model.details = RowTarget(row, app.browse.ancestorsOf(row));
  }

  void _activate(NodeRow row) {
    final folder = app.browse.folder;
    if (row.isFolder && folder != null && !app.browse.isSearch) {
      app.browse.open(folder.child(row.id));
    } else {
      model.revealDetails();
    }
  }

  void _moveTo(PagedRows rows, int index) {
    final total = rows.total ?? 0;
    if (total == 0) return;
    final target = index.clamp(0, total - 1);
    model.tableCursor = target;
    final row = rows.rowAt(target);
    if (row != null) model.details = RowTarget(row, app.browse.ancestorsOf(row));
    if (_scroll.hasClients) {
      final top = target * tableRowExtent;
      final viewport = _scroll.position.viewportDimension;
      if (top < _scroll.offset) {
        _scroll.jumpTo(top);
      } else if (top + tableRowExtent > _scroll.offset + viewport) {
        _scroll.jumpTo(top + tableRowExtent - viewport);
      }
    }
  }

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) return KeyEventResult.ignored;
    final rows = app.browse.rows;
    if (rows == null || (rows.total ?? 0) == 0) return KeyEventResult.ignored;
    final cursor = model.tableCursor;
    final page = _scroll.hasClients ? (_scroll.position.viewportDimension / tableRowExtent).floor() : 10;
    switch (event.logicalKey) {
      case LogicalKeyboardKey.arrowDown:
        _moveTo(rows, cursor == null ? 0 : cursor + 1);
      case LogicalKeyboardKey.arrowUp:
        _moveTo(rows, (cursor ?? 0) - 1);
      case LogicalKeyboardKey.pageDown:
        _moveTo(rows, (cursor ?? 0) + page);
      case LogicalKeyboardKey.pageUp:
        _moveTo(rows, (cursor ?? 0) - page);
      case LogicalKeyboardKey.home:
        _moveTo(rows, 0);
      case LogicalKeyboardKey.end:
        _moveTo(rows, rows.total! - 1);
      case LogicalKeyboardKey.space:
        final row = cursor == null ? null : rows.rowAt(cursor);
        if (row != null) app.selection.toggle(row.id, app.browse.ancestorsOf(row));
      case LogicalKeyboardKey.enter || LogicalKeyboardKey.numpadEnter:
        final row = cursor == null ? null : rows.rowAt(cursor);
        if (row != null) _activate(row);
      default:
        return KeyEventResult.ignored;
    }
    return KeyEventResult.handled;
  }

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final columns = _Columns(constraints.maxWidth);
        return Focus(
          focusNode: _focus,
          onKeyEvent: _onKey,
          child: ListenableBuilder(
            listenable: app.browse,
            builder: (context, _) {
              final rows = app.browse.rows;
              return Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  _Header(app: app, columns: columns),
                  const Divider(),
                  Expanded(
                    child: rows == null
                        ? const SizedBox.shrink()
                        : ListenableBuilder(
                            listenable: Listenable.merge([rows, app.selection, model, _focus]),
                            builder: (context, _) => _body(context, rows, columns),
                          ),
                  ),
                ],
              );
            },
          ),
        );
      },
    );
  }

  Widget _body(BuildContext context, PagedRows rows, _Columns columns) {
    rows.load();
    final total = rows.total;
    final error = rows.error;
    if (total == null && error != null) {
      return Padding(
        padding: const EdgeInsets.all(16),
        child: Align(
          alignment: Alignment.topCenter,
          child: ErrorNotice(
            error: error,
            title: S.listFailed,
            actions: [TextButton(onPressed: rows.retry, child: const Text(S.retry))],
          ),
        ),
      );
    }
    if (total == null) {
      return const Center(child: SizedBox(width: 28, height: 28, child: CircularProgressIndicator(strokeWidth: 3)));
    }
    if (total == 0) {
      return EmptyState(
        icon: app.browse.isSearch ? Icons.search_off : Icons.folder_outlined,
        title: app.browse.isSearch ? S.noMatchesTitle : S.emptyFolderTitle,
        text: app.browse.isSearch
            ? S.noMatchesText
            : (app.browse.filters == const ViewFilters() ? S.emptyFolderText : S.emptyFolderFiltered),
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (error != null)
          Padding(
            padding: const EdgeInsets.all(8),
            child: ErrorNotice(
              error: error,
              title: S.listFailed,
              actions: [TextButton(onPressed: rows.retry, child: const Text(S.retry))],
            ),
          ),
        Expanded(
          child: Scrollbar(
            controller: _scroll,
            thumbVisibility: true,
            child: ListView.builder(
              controller: _scroll,
              itemExtent: tableRowExtent,
              itemCount: total,
              itemBuilder: (context, index) {
                final row = rows.rowAt(index);
                if (row == null) return _LoadingRow(columns: columns);
                return _TableRow(
                  row: row,
                  app: app,
                  columns: columns,
                  ancestors: app.browse.ancestorsOf(row),
                  search: app.browse.isSearch,
                  highlighted: model.tableCursor == index,
                  focused: _focus.hasFocus && model.tableCursor == index,
                  onTap: () => _clicked(index, row),
                );
              },
            ),
          ),
        ),
      ],
    );
  }
}

class _Header extends StatelessWidget {
  const _Header({required this.app, required this.columns});

  final AppController app;
  final _Columns columns;

  @override
  Widget build(BuildContext context) {
    Widget cell(String label, SortKey key, {double? width, bool end = false}) {
      final active = app.browse.sort == key;
      final theme = Theme.of(context);
      final content = InkWell(
        onTap: () => app.browse.sortBy(key),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 8),
          child: Row(
            mainAxisAlignment: end ? MainAxisAlignment.end : MainAxisAlignment.start,
            children: [
              Flexible(
                child: Text(
                  label,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: theme.textTheme.labelLarge?.copyWith(color: active ? theme.colorScheme.primary : null),
                ),
              ),
              if (active)
                Icon(
                  app.browse.descending ? Icons.arrow_downward : Icons.arrow_upward,
                  size: 14,
                  color: theme.colorScheme.primary,
                ),
            ],
          ),
        ),
      );
      final semantic = Semantics(
        button: true,
        label: S.sortBy(label),
        child: Tooltip(message: S.sortBy(label), child: content),
      );
      return width == null ? Expanded(child: semantic) : SizedBox(width: width, child: semantic);
    }

    return Row(
      children: [
        const SizedBox(width: _Columns.check),
        cell(S.columnName, SortKey.name),
        if (columns.size) cell(S.columnSize, SortKey.size, width: _Columns.sizeWidth, end: true),
        if (columns.modified) cell(S.columnModified, SortKey.modified, width: _Columns.modifiedWidth),
        if (columns.status) cell(S.columnStatus, SortKey.rating, width: _Columns.statusWidth),
      ],
    );
  }
}

class _LoadingRow extends StatelessWidget {
  const _LoadingRow({required this.columns});

  final _Columns columns;

  @override
  Widget build(BuildContext context) {
    final color = Theme.of(context).colorScheme.surfaceContainerHighest;
    Widget bar(double width) => Container(
      width: width,
      height: 10,
      decoration: BoxDecoration(color: color, borderRadius: BorderRadius.circular(4)),
    );
    return Semantics(
      label: S.loading,
      child: Row(
        children: [
          const SizedBox(width: _Columns.check + 30),
          Expanded(
            child: Align(alignment: Alignment.centerLeft, child: bar(160)),
          ),
          if (columns.size)
            SizedBox(
              width: _Columns.sizeWidth,
              child: Align(alignment: Alignment.centerRight, child: bar(48)),
            ),
          if (columns.modified)
            SizedBox(
              width: _Columns.modifiedWidth,
              child: Padding(
                padding: const EdgeInsets.only(left: 8),
                child: Align(alignment: Alignment.centerLeft, child: bar(96)),
              ),
            ),
          if (columns.status)
            SizedBox(
              width: _Columns.statusWidth,
              child: Padding(
                padding: const EdgeInsets.only(left: 8),
                child: Align(alignment: Alignment.centerLeft, child: bar(64)),
              ),
            ),
        ],
      ),
    );
  }
}

class _TableRow extends StatelessWidget {
  const _TableRow({
    required this.row,
    required this.app,
    required this.columns,
    required this.ancestors,
    required this.search,
    required this.highlighted,
    required this.focused,
    required this.onTap,
  });

  final NodeRow row;
  final AppController app;
  final _Columns columns;
  final List<int> ancestors;
  final bool search;
  final bool highlighted;
  final bool focused;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    final formats = Formats.of(context);
    final muted = row.deleted ? colors.onSurfaceVariant : null;
    final state = app.selection.stateOf(row.id, ancestors);
    final nameStyle = theme.textTheme.bodyMedium?.copyWith(
      color: muted,
      fontStyle: row.deleted ? FontStyle.italic : null,
    );
    final path = search ? row.path : null;
    return Material(
      color: highlighted ? colors.secondaryContainer : Colors.transparent,
      shape: focused ? RoundedRectangleBorder(side: BorderSide(color: colors.primary, width: 1.5)) : null,
      child: InkWell(
        onTap: onTap,
        child: Row(
          children: [
            SizedBox(
              width: _Columns.check,
              child: Checkbox(
                tristate: row.isFolder,
                value: checkboxValue(state),
                onChanged: (_) => app.selection.toggle(row.id, ancestors),
                visualDensity: VisualDensity.compact,
              ),
            ),
            Icon(
              iconForRow(row),
              size: 18,
              color: row.isFolder && !row.deleted ? colors.primary : colors.onSurfaceVariant,
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Row(
                children: [
                  Flexible(
                    flex: 3,
                    child: Text(S.nodeName(row), maxLines: 1, overflow: TextOverflow.ellipsis, style: nameStyle),
                  ),
                  if (row.deleted) ...[
                    const SizedBox(width: 6),
                    Tooltip(
                      message: S.deletedTip,
                      child: Icon(Icons.delete_outline, size: 14, color: colors.onSurfaceVariant),
                    ),
                  ],
                  if (path != null && path.isNotEmpty) ...[
                    const SizedBox(width: 10),
                    Flexible(
                      flex: 2,
                      child: Text(
                        path,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: theme.textTheme.bodySmall?.copyWith(color: colors.onSurfaceVariant),
                      ),
                    ),
                  ],
                  if (!columns.status && row.isFile) ...[const SizedBox(width: 6), _RatingDot(rating: row.rating)],
                ],
              ),
            ),
            if (columns.size)
              SizedBox(
                width: _Columns.sizeWidth,
                child: Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  child: Text(
                    formats.bytes(row.size),
                    textAlign: TextAlign.end,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodyMedium?.copyWith(
                      color: muted,
                      fontFeatures: const [FontFeature.tabularFigures()],
                    ),
                  ),
                ),
              ),
            if (columns.modified)
              SizedBox(
                width: _Columns.modifiedWidth,
                child: Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  child: Text(
                    formats.dateTime(row.modifiedTime),
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodyMedium?.copyWith(color: muted),
                  ),
                ),
              ),
            if (columns.status)
              SizedBox(
                width: _Columns.statusWidth,
                child: Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  child: Align(
                    alignment: Alignment.centerLeft,
                    child: row.isFolder
                        ? Text(
                            S.files(formats.count(row.files), row.files),
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: theme.textTheme.bodySmall?.copyWith(color: colors.onSurfaceVariant),
                          )
                        : RatingChip(row: row),
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

/// The rating of a file as a coloured label; the tooltip lists its flags.
class RatingChip extends StatelessWidget {
  const RatingChip({super.key, required this.row});

  final NodeRow row;

  @override
  Widget build(BuildContext context) {
    final rating = row.rating;
    final flags = [
      for (final name in row.flagNames)
        if (FileFlag.parse(name) != FileFlag.deleted) S.flagName(FileFlag.parse(name), name),
    ];
    return StatusChip(
      label: S.rating(rating),
      colors: StatusColors.from(context).forRating(rating),
      icon: _ratingIcon(rating),
      tooltip: [S.ratingExplanation(rating), if (flags.isNotEmpty) flags.join(' · ')].join('\n'),
    );
  }
}

class _RatingDot extends StatelessWidget {
  const _RatingDot({required this.rating});

  final Rating? rating;

  @override
  Widget build(BuildContext context) {
    final (_, foreground) = StatusColors.from(context).forRating(rating);
    return Tooltip(
      message: S.rating(rating),
      child: Icon(_ratingIcon(rating), size: 14, color: foreground),
    );
  }
}

IconData _ratingIcon(Rating? rating) => switch (rating) {
  Rating.good => Icons.check_circle_outline,
  Rating.uncertain => Icons.help_outline,
  Rating.damaged => Icons.broken_image_outlined,
  _ => Icons.block,
};
