/// The folder tree of the Results screen: the file systems found, each with
/// its folders, loaded page by page, with tri-state check boxes and
/// keyboard navigation.
library;

import 'package:flutter/services.dart';
import 'package:material_ui/material_ui.dart';

import '../../state/state.dart';
import '../format.dart';
import '../strings.dart';
import 'file_types.dart';
import 'folder_tree.dart';
import 'results_model.dart';

/// Height of one line of the tree.
const double treeRowExtent = 30;

/// The value of a tri-state check box for [state].
bool? checkboxValue(CheckState state) => switch (state) {
  CheckState.checked => true,
  CheckState.unchecked => false,
  CheckState.partial => null,
};

/// See the library documentation.
///
/// Keys: Up/Down move, Right expands (or enters), Left collapses (or goes
/// to the parent), Space ticks, Enter shows the folder in the table.
class FolderTreeView extends StatefulWidget {
  const FolderTreeView({super.key, required this.app, required this.model});

  final AppController app;
  final ResultsModel model;

  @override
  State<FolderTreeView> createState() => _FolderTreeViewState();
}

class _FolderTreeViewState extends State<FolderTreeView> {
  final ScrollController _scroll = ScrollController();
  final FocusNode _focus = FocusNode(debugLabel: 'folder tree');

  ResultsModel get model => widget.model;
  FolderTree get tree => widget.model.tree;
  AppController get app => widget.app;

  @override
  void dispose() {
    _scroll.dispose();
    _focus.dispose();
    super.dispose();
  }

  void _open(TreeEntry entry) {
    final id = entry.id;
    if (id == null) return;
    final instance = entry.instance;
    if (instance != null) {
      app.browse.openRoot(instance);
      model.details = InstanceTarget(instance);
    } else {
      app.browse.open(FolderLocation(id, entry.ancestors));
    }
  }

  void _toggleCheck(TreeEntry entry) {
    final id = entry.id;
    if (id != null) app.selection.toggle(id, entry.ancestors);
  }

  void _moveTo(int index) {
    final length = tree.length;
    if (length == 0) return;
    final target = index.clamp(0, length - 1);
    model.treeCursor = target;
    _reveal(target);
  }

  void _reveal(int index) {
    if (!_scroll.hasClients) return;
    final top = index * treeRowExtent;
    final viewport = _scroll.position.viewportDimension;
    final offset = _scroll.offset;
    if (top < offset) {
      _scroll.jumpTo(top);
    } else if (top + treeRowExtent > offset + viewport) {
      _scroll.jumpTo(top + treeRowExtent - viewport);
    }
  }

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) return KeyEventResult.ignored;
    final length = tree.length;
    if (length == 0) return KeyEventResult.ignored;
    final cursor = (model.treeCursor ?? 0).clamp(0, length - 1);
    final entry = tree.entryAt(cursor);
    final page = _scroll.hasClients ? (_scroll.position.viewportDimension / treeRowExtent).floor() : 10;
    switch (event.logicalKey) {
      case LogicalKeyboardKey.arrowDown:
        _moveTo(model.treeCursor == null ? 0 : cursor + 1);
      case LogicalKeyboardKey.arrowUp:
        _moveTo(cursor - 1);
      case LogicalKeyboardKey.pageDown:
        _moveTo(cursor + page);
      case LogicalKeyboardKey.pageUp:
        _moveTo(cursor - page);
      case LogicalKeyboardKey.home:
        _moveTo(0);
      case LogicalKeyboardKey.end:
        _moveTo(length - 1);
      case LogicalKeyboardKey.arrowRight:
        if (entry.expandable && !entry.expanded) {
          tree.expand(entry);
        } else if (entry.expanded) {
          _moveTo(cursor + 1);
        }
      case LogicalKeyboardKey.arrowLeft:
        if (entry.expanded) {
          tree.collapse(entry);
        } else {
          final parent = tree.parentIndex(cursor);
          if (parent != null) _moveTo(parent);
        }
      case LogicalKeyboardKey.space:
        _toggleCheck(entry);
      case LogicalKeyboardKey.enter || LogicalKeyboardKey.numpadEnter:
        _open(entry);
      default:
        return KeyEventResult.ignored;
    }
    return KeyEventResult.handled;
  }

  @override
  Widget build(BuildContext context) {
    return Focus(
      focusNode: _focus,
      onKeyEvent: _onKey,
      child: ListenableBuilder(
        listenable: Listenable.merge([tree, app.selection, app.browse, model, _focus]),
        builder: (context, _) {
          final length = tree.length;
          final current = app.browse.folder?.id;
          return Scrollbar(
            controller: _scroll,
            child: ListView.builder(
              controller: _scroll,
              itemExtent: treeRowExtent,
              itemCount: length,
              itemBuilder: (context, index) {
                final entry = tree.entryAt(index);
                return _TreeLine(
                  entry: entry,
                  app: app,
                  selected: entry.id != null && entry.id == current,
                  cursor: _focus.hasFocus && model.treeCursor == index,
                  onTap: () {
                    model.treeCursor = index;
                    _focus.requestFocus();
                    _open(entry);
                  },
                  onToggle: () => tree.toggle(entry),
                  onCheck: () => _toggleCheck(entry),
                  onRetry: tree.retry,
                );
              },
            ),
          );
        },
      ),
    );
  }
}

class _TreeLine extends StatelessWidget {
  const _TreeLine({
    required this.entry,
    required this.app,
    required this.selected,
    required this.cursor,
    required this.onTap,
    required this.onToggle,
    required this.onCheck,
    required this.onRetry,
  });

  final TreeEntry entry;
  final AppController app;
  final bool selected;
  final bool cursor;
  final VoidCallback onTap;
  final VoidCallback onToggle;
  final VoidCallback onCheck;
  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    final indent = 8.0 + entry.depth * 16;
    if (entry.loading) {
      return Padding(
        padding: EdgeInsets.only(left: indent + 28),
        child: Row(
          children: [
            if (entry.error != null) ...[
              Icon(Icons.error_outline, size: 16, color: colors.error),
              const SizedBox(width: 6),
              Flexible(
                child: Text(S.listFailed, overflow: TextOverflow.ellipsis, style: theme.textTheme.bodySmall),
              ),
              TextButton(onPressed: onRetry, child: const Text(S.retry)),
            ] else
              Flexible(
                child: Text(
                  S.loading,
                  overflow: TextOverflow.ellipsis,
                  style: theme.textTheme.bodySmall?.copyWith(color: colors.onSurfaceVariant),
                ),
              ),
          ],
        ),
      );
    }
    final id = entry.id!;
    final instance = entry.instance;
    final row = entry.row;
    final name = instance != null ? S.instanceName(instance) : S.nodeName(row!);
    final deleted = row?.deleted ?? false;
    final state = app.selection.stateOf(id, entry.ancestors);
    final Widget line = Material(
      color: selected ? colors.secondaryContainer : Colors.transparent,
      shape: cursor ? RoundedRectangleBorder(side: BorderSide(color: colors.primary, width: 1.5)) : null,
      child: InkWell(
        onTap: onTap,
        child: Padding(
          padding: EdgeInsets.only(left: indent, right: 8),
          child: Row(
            children: [
              SizedBox(
                width: 24,
                child: entry.expandable
                    ? InkResponse(
                        onTap: onToggle,
                        radius: 14,
                        child: Icon(entry.expanded ? Icons.expand_more : Icons.chevron_right, size: 20),
                      )
                    : null,
              ),
              SizedBox(
                width: 28,
                child: Checkbox(
                  tristate: true,
                  value: checkboxValue(state),
                  onChanged: (_) => onCheck(),
                  visualDensity: VisualDensity.compact,
                  materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
                ),
              ),
              Icon(
                instance != null ? iconForInstance(instance) : iconForRow(row!),
                size: 18,
                color: deleted ? colors.onSurfaceVariant : colors.primary,
              ),
              const SizedBox(width: 6),
              Expanded(
                child: Text(
                  name,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: (instance != null ? theme.textTheme.titleSmall : theme.textTheme.bodyMedium)?.copyWith(
                    fontStyle: deleted ? FontStyle.italic : null,
                    color: deleted ? colors.onSurfaceVariant : null,
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
    final formats = Formats.of(context);
    final tip = instance != null
        ? S.instanceTooltip(instance, formats.count(instance.files), formats.bytes(instance.bytes))
        : S.folderTooltip(name, formats.count(row!.files), formats.bytes(row.size), deleted);
    return Tooltip(message: tip, waitDuration: const Duration(milliseconds: 800), child: line);
  }
}
