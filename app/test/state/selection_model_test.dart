// The selection model: the include/exclude sets of PROTOCOL
// selection_summary, tri-states from ancestor ids, and minimal sets,
// compared with a naive model that stores the state of every node.
import 'dart:math';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/state/selection_model.dart';

/// A random forest: `parent[i] < i`, or -1 for a root.
final class Forest {
  Forest(Random random, int size, {int roots = 2}) : parent = List.filled(size, -1) {
    for (var i = roots; i < size; i++) {
      // Mostly deep-ish trees, some wide folders.
      parent[i] = random.nextInt(4) == 0 ? random.nextInt(roots) : random.nextInt(i);
    }
    for (var i = 0; i < size; i++) {
      final chain = <int>[];
      for (var p = parent[i]; p >= 0; p = parent[p]) {
        chain.add(p);
      }
      ancestors.add(chain.reversed.toList());
      children.add([]);
    }
    for (var i = 0; i < size; i++) {
      if (parent[i] >= 0) children[parent[i]].add(i);
    }
  }

  final List<int> parent;

  /// Root first, without the node itself.
  final List<List<int>> ancestors = [];
  final List<List<int>> children = [];

  int get size => parent.length;

  Iterable<int> subtree(int node) sync* {
    yield node;
    for (final child in children[node]) {
      yield* subtree(child);
    }
  }
}

/// The naive model: one boolean per node.
final class Naive {
  Naive(this.forest) : selected = List.filled(forest.size, false);

  final Forest forest;
  final List<bool> selected;

  void setChecked(int node, bool checked) {
    for (final n in forest.subtree(node)) {
      selected[n] = checked;
    }
  }

  CheckState stateOf(int node) {
    var any = false;
    var all = true;
    for (final n in forest.subtree(node)) {
      if (selected[n]) {
        any = true;
      } else {
        all = false;
      }
    }
    return all ? CheckState.checked : (any ? CheckState.partial : CheckState.unchecked);
  }
}

/// The definition of the protocol: a node is selected when its nearest
/// ancestor-or-self that appears in include or exclude is in include.
List<bool> byDefinition(Forest forest, Iterable<int> include, Iterable<int> exclude) {
  final inc = include.toSet();
  final exc = exclude.toSet();
  return [
    for (var node = 0; node < forest.size; node++)
      () {
        for (int n = node; n >= 0; n = forest.parent[n]) {
          if (inc.contains(n)) return true;
          if (exc.contains(n)) return false;
        }
        return false;
      }(),
  ];
}

void expectConsistent(Forest forest, SelectionModel model, List<bool> expected, String context) {
  final include = model.include;
  final exclude = model.exclude;
  expect(include.toSet().intersection(exclude.toSet()), isEmpty, reason: '$context: the sets are disjoint');
  expect(include, List.of(include)..sort(), reason: '$context: sorted');
  expect(exclude, List.of(exclude)..sort(), reason: '$context: sorted');

  // The sets mean, by the protocol's definition, exactly the expected selection.
  expect(byDefinition(forest, include, exclude), expected, reason: '$context: meaning of the sets');

  // The model answers the same from ancestor ids alone.
  for (var node = 0; node < forest.size; node++) {
    expect(model.isSelected(node, forest.ancestors[node]), expected[node], reason: '$context: isSelected($node)');
  }

  // Minimal: every single entry is needed.
  for (final entry in include) {
    expect(byDefinition(forest, include.where((i) => i != entry), exclude), isNot(expected),
        reason: '$context: include entry $entry is redundant');
  }
  for (final entry in exclude) {
    expect(byDefinition(forest, include, exclude.where((i) => i != entry)), isNot(expected),
        reason: '$context: exclude entry $entry is redundant');
  }
  expect(model.isEmpty, !expected.contains(true), reason: '$context: isEmpty');
}

void main() {
  // A small tree used by the hand-written cases:
  //
  //   0 root
  //   ├─ 1 Photos
  //   │   ├─ 3 2019
  //   │   │   ├─ 6 a.jpg
  //   │   │   └─ 7 b.jpg
  //   │   └─ 4 c.jpg
  //   └─ 2 Documents
  //       └─ 5 d.pdf
  const root = <int>[];
  const inRoot = [0];
  const inPhotos = [0, 1];
  const in2019 = [0, 1, 3];
  const inDocuments = [0, 2];

  group('the sets', () {
    test('checking a folder is one entry, however much it holds', () {
      final model = SelectionModel();
      model.setChecked(1, inRoot, true);
      expect(model.include, [1]);
      expect(model.exclude, isEmpty);
      expect(model.isSelected(1, inRoot), isTrue);
      expect(model.isSelected(6, in2019), isTrue, reason: 'inherited');
      expect(model.isSelected(999999, [0, 1, 3, 555]), isTrue, reason: 'any node below, known or not');
      expect(model.isSelected(5, inDocuments), isFalse);
      expect(model.isSelected(0, root), isFalse);
    });

    test('unchecking inside a checked folder is one exclude entry', () {
      final model = SelectionModel()
        ..setChecked(0, root, true)
        ..setChecked(3, inPhotos, false);
      expect(model.include, [0]);
      expect(model.exclude, [3]);
      expect(model.isSelected(6, in2019), isFalse);
      expect(model.isSelected(4, inPhotos), isTrue);
    });

    test('nesting: include inside exclude inside include', () {
      final model = SelectionModel()
        ..setChecked(0, root, true)
        ..setChecked(1, inRoot, false)
        ..setChecked(6, in2019, true);
      expect(model.include, [0, 6]);
      expect(model.exclude, [1]);
      expect(model.isSelected(6, in2019), isTrue);
      expect(model.isSelected(7, in2019), isFalse);
      expect(model.isSelected(5, inDocuments), isTrue);
    });

    test('entries that change nothing are never stored', () {
      final model = SelectionModel();
      model.setChecked(4, inPhotos, false);
      expect(model.isEmpty, isTrue, reason: 'nothing was selected there anyway');
      model.setChecked(1, inRoot, true);
      model.setChecked(6, in2019, true);
      expect(model.include, [1], reason: 'already selected through its folder');
      model.setChecked(3, inPhotos, false);
      model.setChecked(7, in2019, false);
      expect(model.exclude, [3], reason: 'already excluded through its folder');
    });

    test('checking or unchecking a folder drops the entries below it', () {
      final model = SelectionModel()
        ..setChecked(1, inRoot, true)
        ..setChecked(6, in2019, false)
        ..setChecked(4, inPhotos, false);
      expect(model.exclude, [4, 6]);
      model.setChecked(1, inRoot, true);
      expect(model.include, [1]);
      expect(model.exclude, isEmpty);

      model.setChecked(6, in2019, false);
      model.setChecked(0, root, false);
      expect(model.isEmpty, isTrue);
      expect(model.include, isEmpty);
      expect(model.exclude, isEmpty);
    });

    test('re-checking the excluded folder restores the single entry', () {
      final model = SelectionModel()
        ..setChecked(0, root, true)
        ..setChecked(3, inPhotos, false)
        ..setChecked(3, inPhotos, true);
      expect(model.include, [0]);
      expect(model.exclude, isEmpty);
    });

    test('roots of different file systems are independent', () {
      final model = SelectionModel()
        ..setChecked(0, root, true)
        ..setChecked(100, root, true)
        ..setChecked(0, root, false);
      expect(model.include, [100]);
      expect(model.isSelected(101, [100]), isTrue);
      expect(model.isSelected(1, inRoot), isFalse);
    });
  });

  group('tri-state', () {
    test('from the ancestor ids alone', () {
      final model = SelectionModel()..setChecked(6, in2019, true);
      expect(model.stateOf(6, in2019), CheckState.checked);
      expect(model.stateOf(7, in2019), CheckState.unchecked);
      expect(model.stateOf(3, inPhotos), CheckState.partial);
      expect(model.stateOf(1, inRoot), CheckState.partial);
      expect(model.stateOf(0, root), CheckState.partial);
      expect(model.stateOf(2, inRoot), CheckState.unchecked);
      expect(model.stateOf(5, inDocuments), CheckState.unchecked);
    });

    test('a checked folder with an excluded part is partial; below the exclusion it is unchecked', () {
      final model = SelectionModel()
        ..setChecked(0, root, true)
        ..setChecked(3, inPhotos, false);
      expect(model.stateOf(0, root), CheckState.partial);
      expect(model.stateOf(1, inRoot), CheckState.partial);
      expect(model.stateOf(3, inPhotos), CheckState.unchecked);
      expect(model.stateOf(6, in2019), CheckState.unchecked);
      expect(model.stateOf(4, inPhotos), CheckState.checked);
      expect(model.stateOf(2, inRoot), CheckState.checked);
      expect(model.stateOf(5, inDocuments), CheckState.checked);
    });

    test('toggle: checked becomes unchecked; unchecked and partial become checked', () {
      final model = SelectionModel();
      model.toggle(1, inRoot);
      expect(model.stateOf(1, inRoot), CheckState.checked);
      model.toggle(6, in2019);
      expect(model.stateOf(6, in2019), CheckState.unchecked);
      expect(model.stateOf(1, inRoot), CheckState.partial);
      model.toggle(1, inRoot);
      expect(model.stateOf(1, inRoot), CheckState.checked);
      expect(model.include, [1]);
      expect(model.exclude, isEmpty);
      model.toggle(1, inRoot);
      expect(model.isEmpty, isTrue);
    });
  });

  group('notifications', () {
    test('listeners are told about changes, and only about changes', () {
      final model = SelectionModel();
      var notified = 0;
      model.addListener(() => notified++);

      model.setChecked(1, inRoot, true);
      expect(notified, 1);
      expect(model.revision, 1);
      model.setChecked(1, inRoot, true);
      model.setChecked(6, in2019, true);
      model.setChecked(5, inDocuments, false);
      expect(notified, 1, reason: 'none of these changed anything');
      expect(model.revision, 1);

      model.setChecked(6, in2019, false);
      expect(notified, 2);
      model.clear();
      expect(notified, 3);
      model.clear();
      expect(notified, 3);
      expect(model.revision, 3);
    });

    test('a snapshot does not change with the model', () {
      final model = SelectionModel()..setChecked(1, inRoot, true);
      final spec = model.spec;
      model.setChecked(6, in2019, false);
      expect(spec.include, [1]);
      expect(spec.exclude, isEmpty);
      expect(spec.isEmpty, isFalse);
      expect(spec.revision, 1);
      expect(model.spec.exclude, [6]);
      expect(() => spec.include.add(5), throwsUnsupportedError);
      expect(SelectionModel().spec.isEmpty, isTrue);
    });

    test('the model keeps its own copy of the ancestor list', () {
      final model = SelectionModel();
      final ancestors = [0, 1, 3];
      model.setChecked(6, ancestors, true);
      ancestors.clear();
      expect(model.stateOf(3, inPhotos), CheckState.partial);
      model.setChecked(1, inRoot, false);
      expect(model.isEmpty, isTrue, reason: 'the entry below was found and dropped');
    });
  });

  group('removeWhere (nodes that no longer exist)', () {
    test('entries inside the removed subtree go; the rest keeps its meaning', () {
      // 3 is [Carved files] with a carved file 6 that was ticked.
      final model = SelectionModel()
        ..setChecked(6, in2019, true)
        ..setChecked(5, inDocuments, true);
      model.removeWhere((id, ancestors) => id == 3 || ancestors.contains(3));
      expect(model.include, [5]);
      expect(model.stateOf(1, inRoot), CheckState.unchecked);
    });

    test('entries that became redundant are dropped with them', () {
      final model = SelectionModel()
        ..setChecked(0, root, true)
        ..setChecked(3, inPhotos, false)
        ..setChecked(6, in2019, true);
      // The exclusion goes away: the include below it no longer changes anything.
      model.removeWhere((id, _) => id == 3);
      expect(model.include, [0]);
      expect(model.exclude, isEmpty);

      final other = SelectionModel()
        ..setChecked(1, inRoot, true)
        ..setChecked(6, in2019, false);
      // The include goes away: the exclude below it selects nothing less.
      other.removeWhere((id, _) => id == 1);
      expect(other.isEmpty, isTrue);
      expect(other.exclude, isEmpty);
    });

    test('nothing to remove: no change, no notification', () {
      final model = SelectionModel()..setChecked(1, inRoot, true);
      var notified = 0;
      model.addListener(() => notified++);
      model.removeWhere((id, _) => id == 77);
      expect(notified, 0);
      expect(model.revision, 1);
    });
  });

  group('compared with a naive model', () {
    for (final seed in [1, 2, 3, 4, 5, 6, 7, 8]) {
      test('random operations on a random forest (seed $seed)', () {
        final random = Random(seed);
        final forest = Forest(random, 60 + random.nextInt(60), roots: 1 + random.nextInt(3));
        final naive = Naive(forest);
        final model = SelectionModel();

        for (var step = 0; step < 400; step++) {
          final node = random.nextInt(forest.size);
          final context = 'seed $seed step $step';
          final operation = random.nextInt(10);
          if (operation < 6) {
            final checked = random.nextBool();
            model.setChecked(node, forest.ancestors[node], checked);
            naive.setChecked(node, checked);
          } else if (operation < 9) {
            final before = naive.stateOf(node);
            model.toggle(node, forest.ancestors[node]);
            naive.setChecked(node, before != CheckState.checked);
          } else {
            // Some nodes vanish (as discarded carved files do): their entries
            // are forgotten and the rest must mean what it meant.
            final gone = forest.subtree(node).toSet();
            final include = model.include.where((id) => !gone.contains(id)).toList();
            final exclude = model.exclude.where((id) => !gone.contains(id)).toList();
            model.removeWhere((id, _) => gone.contains(id));
            final expected = byDefinition(forest, include, exclude);
            for (var n = 0; n < forest.size; n++) {
              naive.selected[n] = expected[n];
            }
          }

          expectConsistent(forest, model, naive.selected, context);
          for (var n = 0; n < forest.size; n++) {
            expect(model.stateOf(n, forest.ancestors[n]), naive.stateOf(n), reason: '$context: stateOf($n)');
          }
        }
      });
    }

    test('the number of entries is bounded by the number of clicks, not by the tree', () {
      final random = Random(99);
      final forest = Forest(random, 5000);
      final model = SelectionModel();
      const clicks = 40;
      for (var i = 0; i < clicks; i++) {
        final node = random.nextInt(forest.size);
        model.setChecked(node, forest.ancestors[node], random.nextBool());
        expect(model.include.length + model.exclude.length, lessThanOrEqualTo(i + 1));
      }
    });
  });

  test('checking a folder above twenty thousand individual ticks collapses them at once', () {
    final model = SelectionModel();
    for (var i = 0; i < 20000; i++) {
      model.setChecked(1000 + i, const [0, 1, 2], true);
    }
    expect(model.include, hasLength(20000));
    expect(model.stateOf(1, const [0]), CheckState.partial);

    final watch = Stopwatch()..start();
    model.setChecked(1, const [0], true);
    expect(watch.elapsed, lessThan(const Duration(seconds: 2)));
    expect(model.include, [1]);
    expect(model.stateOf(1, const [0]), CheckState.checked);
    expect(model.stateOf(2, const [0, 1]), CheckState.checked);
    expect(model.stateOf(0, const []), CheckState.partial);
  });
}
