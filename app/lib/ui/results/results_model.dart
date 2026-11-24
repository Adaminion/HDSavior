/// What the Results screen remembers while the user visits other screens:
/// the folder tree, the file the details pane shows, the search text.
library;

import 'package:flutter/foundation.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import 'folder_tree.dart';

/// The thing the details pane describes.
@immutable
sealed class DetailsTarget {
  const DetailsTarget();
}

/// A row of the file table (a file or a folder).
final class RowTarget extends DetailsTarget {
  const RowTarget(this.row, this.ancestors);

  final NodeRow row;

  /// Ids of its ancestors, root first.
  final List<int> ancestors;

  @override
  bool operator ==(Object other) => other is RowTarget && other.row.id == row.id;

  @override
  int get hashCode => row.id.hashCode;
}

/// The root of a file system (from the tree).
final class InstanceTarget extends DetailsTarget {
  const InstanceTarget(this.instance);

  final Instance instance;

  @override
  bool operator ==(Object other) => other is InstanceTarget && other.instance.root == instance.root;

  @override
  int get hashCode => instance.root.hashCode;
}

/// Lives as long as the window; the Results screen widget comes and goes.
class ResultsModel extends ChangeNotifier {
  ResultsModel(this._app) : tree = FolderTree(_app.catalog, _app.browse) {
    _seenEpochChanges = _app.catalog.epochChanges;
    _seenFolder = _app.browse.folder;
    _seenQuery = _app.browse.query;
    _app.catalog.addListener(_onCatalogChanged);
    _app.browse.addListener(_onBrowseChanged);
  }

  final AppController _app;

  /// The folder tree (left).
  final FolderTree tree;

  DetailsTarget? _details;
  int? _tableCursor;
  int? _treeCursor;
  String _search = '';
  bool _detailsOpen = true;
  bool _detailsOverlay = false;
  int _seenEpochChanges = 0;
  FolderLocation? _seenFolder;
  RowQuery? _seenQuery;

  /// What the details pane shows; `null`: the current folder.
  DetailsTarget? get details => _details;

  set details(DetailsTarget? value) {
    if (value == _details) return;
    _details = value;
    notifyListeners();
  }

  /// The highlighted row of the file table (keyboard focus).
  int? get tableCursor => _tableCursor;

  set tableCursor(int? value) {
    if (value == _tableCursor) return;
    _tableCursor = value;
    notifyListeners();
  }

  /// The highlighted entry of the tree.
  int? get treeCursor => _treeCursor;

  set treeCursor(int? value) {
    if (value == _treeCursor) return;
    _treeCursor = value;
    notifyListeners();
  }

  /// The text in the search field (the filter follows it after a pause).
  String get search => _search;

  set search(String value) {
    if (value == _search) return;
    _search = value;
    notifyListeners();
  }

  /// Whether the details pane is shown beside the table (wide windows).
  bool get detailsOpen => _detailsOpen;

  set detailsOpen(bool value) {
    if (value == _detailsOpen) return;
    _detailsOpen = value;
    notifyListeners();
  }

  /// Whether the details pane is shown over the table (narrow windows,
  /// where it does not fit beside it). Closed until the user asks.
  bool get detailsOverlay => _detailsOverlay;

  set detailsOverlay(bool value) {
    if (value == _detailsOverlay) return;
    _detailsOverlay = value;
    notifyListeners();
  }

  /// The user asked for the details (Enter or a double click on a file).
  void revealDetails() {
    if (_detailsOpen && _detailsOverlay) return;
    _detailsOpen = true;
    _detailsOverlay = true;
    notifyListeners();
  }

  void _onCatalogChanged() {
    if (_app.catalog.epochChanges == _seenEpochChanges) return;
    _seenEpochChanges = _app.catalog.epochChanges;
    // Node ids of the old epoch mean nothing any more.
    _details = null;
    _tableCursor = null;
    _treeCursor = null;
    notifyListeners();
  }

  void _onBrowseChanged() {
    final folder = _app.browse.folder;
    final query = _app.browse.query;
    if (folder == _seenFolder && query == _seenQuery) return;
    // Another listing: the highlighted position means another row now.
    _tableCursor = null;
    if (folder != _seenFolder) _details = null;
    _seenFolder = folder;
    _seenQuery = query;
    notifyListeners();
  }

  @override
  void dispose() {
    _app.catalog.removeListener(_onCatalogChanged);
    _app.browse.removeListener(_onBrowseChanged);
    tree.dispose();
    super.dispose();
  }
}
