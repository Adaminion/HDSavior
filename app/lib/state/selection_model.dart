/// What the user has checked for recovery.
library;

import 'package:flutter/foundation.dart';

/// The state of a checkbox in the tree or the table.
enum CheckState {
  /// Neither the node nor anything below it is selected.
  unchecked,

  /// The node and everything below it are selected.
  checked,

  /// The node and what is below it are not all in the same state.
  partial,
}

/// An immutable snapshot of a [SelectionModel]: the `include` and `exclude`
/// arguments of `selection_summary` and `recover_start`.
@immutable
final class SelectionSpec {
  const SelectionSpec({
    required this.include,
    required this.exclude,
    required this.revision,
  });

  /// Node ids, ascending.
  final List<int> include;

  /// Node ids, ascending.
  final List<int> exclude;

  /// The [SelectionModel.revision] this snapshot was taken at.
  final int revision;

  /// Nothing is selected.
  bool get isEmpty => include.isEmpty;
}

final class _Entry {
  _Entry(this.included, this.ancestors);

  final bool included;

  /// Ids of the ancestors, root first.
  final List<int> ancestors;
}

/// The selection as two sets of node ids, exactly as docs/PROTOCOL.md defines
/// it for `selection_summary`: *a node is selected when its nearest
/// ancestor-or-self that appears in `include` or `exclude` is in `include`*.
///
/// The model never needs to know what a folder contains. Checking a folder is
/// one entry however many files it holds; the engine resolves it. What the
/// model does need, for every node it is asked about, is the node's
/// **ancestor ids, root first, without the node itself**: the path that was
/// expanded in the tree, `ancestor_ids` of a search row, or
/// `NodeInfo.ancestorIds`. An instance root has no ancestors (`const []`).
///
/// The sets are kept minimal: an entry that would not change the state of
/// its node (an `include` below an `include`, an `exclude` where nothing is
/// selected anyway) is never stored, and entries below a node that is checked
/// or unchecked as a whole are dropped. Therefore:
///
/// * [setChecked] costs O(depth + explicit entries below the node);
/// * [stateOf] costs O(depth): the nearest marked ancestor decides whether
///   the node is selected, and a per-ancestor index says whether anything
///   below it differs.
///
/// Node ids are valid for one catalog epoch. The owner (`CatalogController`)
/// clears the model when the epoch changes.
class SelectionModel extends ChangeNotifier {
  final Map<int, _Entry> _entries = {};

  /// For every node: the ids of the include (exclude) entries strictly below
  /// it.
  final Map<int, Set<int>> _includedBelow = {};
  final Map<int, Set<int>> _excludedBelow = {};

  int _revision = 0;

  /// Increases with every change. Lets consumers tell whether a result they
  /// computed (a summary) still belongs to the current selection.
  int get revision => _revision;

  /// Nothing is selected.
  bool get isEmpty => _entries.isEmpty;

  /// The `include` set, ascending.
  List<int> get include => _ids(included: true);

  /// The `exclude` set, ascending.
  List<int> get exclude => _ids(included: false);

  /// An immutable snapshot for a request.
  SelectionSpec get spec =>
      SelectionSpec(include: include, exclude: exclude, revision: _revision);

  List<int> _ids({required bool included}) {
    final ids = [
      for (final entry in _entries.entries)
        if (entry.value.included == included) entry.key,
    ]..sort();
    return List.unmodifiable(ids);
  }

  /// The state inherited from the nearest marked ancestor (`false` if none).
  bool _inherited(List<int> ancestors) {
    for (var i = ancestors.length - 1; i >= 0; i--) {
      final entry = _entries[ancestors[i]];
      if (entry != null) return entry.included;
    }
    return false;
  }

  /// Whether node [id] itself is selected.
  bool isSelected(int id, List<int> ancestors) =>
      _entries[id]?.included ?? _inherited(ancestors);

  /// The tri-state of node [id]: its own state, and whether anything below it
  /// differs.
  CheckState stateOf(int id, List<int> ancestors) {
    if (isSelected(id, ancestors)) {
      return _excludedBelow.containsKey(id)
          ? CheckState.partial
          : CheckState.checked;
    }
    return _includedBelow.containsKey(id)
        ? CheckState.partial
        : CheckState.unchecked;
  }

  /// Selects ([checked]) or deselects node [id] together with everything
  /// below it.
  void setChecked(int id, List<int> ancestors, bool checked) {
    assert(!ancestors.contains(id), 'ancestors must not contain the node');
    var changed = false;

    // Whatever was marked below the node is overridden by its new state.
    final below = [...?_includedBelow[id], ...?_excludedBelow[id]];
    for (final entry in below) {
      _remove(entry);
      changed = true;
    }
    final own = _entries[id];
    final inherited = _inherited(ancestors);
    if (inherited == checked) {
      // The ancestors already say so: no entry needed.
      if (own != null) {
        _remove(id);
        changed = true;
      }
    } else if (own == null || own.included != checked) {
      if (own != null) _remove(id);
      _add(id, ancestors, checked);
      changed = true;
    }
    if (changed) _changed();
  }

  /// What a click on the checkbox does: a checked node becomes unchecked;
  /// an unchecked or partly checked node becomes checked.
  void toggle(int id, List<int> ancestors) => setChecked(
    id,
    ancestors,
    stateOf(id, ancestors) != CheckState.checked,
  );

  /// Deselects everything.
  void clear() {
    if (_entries.isEmpty) return;
    _entries.clear();
    _includedBelow.clear();
    _excludedBelow.clear();
    _changed();
  }

  /// Forgets the entries for which [test] returns `true`: nodes that no
  /// longer exist (carved files that were discarded). The remaining entries
  /// keep their meaning.
  void removeWhere(bool Function(int id, List<int> ancestors) test) {
    final doomed = [
      for (final entry in _entries.entries)
        if (test(entry.key, entry.value.ancestors)) entry.key,
    ];
    if (doomed.isEmpty) return;
    for (final id in doomed) {
      _remove(id);
    }
    _minimize();
    _changed();
  }

  void _add(int id, List<int> ancestors, bool included) {
    final entry = _Entry(included, List.unmodifiable(ancestors));
    _entries[id] = entry;
    final index = included ? _includedBelow : _excludedBelow;
    for (final ancestor in entry.ancestors) {
      (index[ancestor] ??= {}).add(id);
    }
  }

  void _remove(int id) {
    final entry = _entries.remove(id);
    if (entry == null) return;
    final index = entry.included ? _includedBelow : _excludedBelow;
    for (final ancestor in entry.ancestors) {
      final set = index[ancestor];
      if (set == null) continue;
      set.remove(id);
      if (set.isEmpty) index.remove(ancestor);
    }
  }

  /// Drops entries that have become redundant after entries above them were
  /// removed (only [removeWhere] can cause that). Shallow entries first, so
  /// that each entry is judged against ancestors that are already final.
  void _minimize() {
    final ids = _entries.keys.toList()
      ..sort(
        (a, b) => _entries[a]!.ancestors.length.compareTo(
          _entries[b]!.ancestors.length,
        ),
      );
    for (final id in ids) {
      final entry = _entries[id]!;
      if (_inherited(entry.ancestors) == entry.included) _remove(id);
    }
  }

  void _changed() {
    _revision++;
    notifyListeners();
  }
}
