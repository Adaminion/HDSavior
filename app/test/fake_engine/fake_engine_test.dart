// The fake engine must behave like docs/PROTOCOL.md says the real one does,
// because every other test (and the screens) trust it. These tests drive it
// directly, line by line.
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';

import 'fake_content.dart';
import 'fake_engine.dart';

const String disk1 = r'\\.\PhysicalDrive1';

/// Talks to a [FakeEngine] the way the client would, but synchronously.
final class Harness {
  Harness({int mainNodes = 3000, bool elevated = true, String os = 'windows', FakeWorld? world})
    : engine = FakeEngine(
        options: FakeEngineOptions(mainNodes: mainNodes, tick: null, elevated: elevated, os: os),
        world: world,
      ) {
    engine.output.listen((line) {
      try {
        lines.add(jsonDecode(line) as Map<String, Object?>);
      } on FormatException {
        lines.add({'raw': line}); // a line cut short by a simulated crash
      }
    });
  }

  final FakeEngine engine;
  final List<Map<String, Object?>> lines = [];
  int _id = 0;

  /// Sends a request and returns its id.
  int send(String command, [Map<String, Object?>? args]) {
    final id = ++_id;
    engine.handleLine(jsonEncode({'id': id, 'cmd': command, 'args': ?args}));
    return id;
  }

  Map<String, Object?>? responseTo(int id) {
    for (final line in lines) {
      if (line['id'] == id) return line;
    }
    return null;
  }

  /// The result of a request that must succeed at once.
  Map<String, Object?> ok(String command, [Map<String, Object?>? args]) {
    final response = responseTo(send(command, args));
    expect(response, isNotNull, reason: '$command was not answered');
    expect(response!['ok'], isTrue, reason: '$command failed: ${response['error']}');
    return response['result']! as Map<String, Object?>;
  }

  /// The error of a request that must fail at once.
  Map<String, Object?> error(String command, [Map<String, Object?>? args]) {
    final response = responseTo(send(command, args));
    expect(response, isNotNull, reason: '$command was not answered');
    expect(response!['ok'], isFalse, reason: '$command unexpectedly succeeded');
    return response['error']! as Map<String, Object?>;
  }

  /// The error code of a request, or 'ok'.
  String outcome(String command, [Map<String, Object?>? args]) {
    final response = responseTo(send(command, args))!;
    return response['ok'] == true ? 'ok' : (response['error']! as Map<String, Object?>)['code']! as String;
  }

  List<Map<String, Object?>> events(String name) => [
    for (final line in lines)
      if (line['event'] == name) line['data']! as Map<String, Object?>,
  ];

  /// Steps the running job until it has ended.
  void finishJob() {
    for (var i = 0; i < 1000 && engine.runningJob != null; i++) {
      engine.advance();
    }
    expect(engine.runningJob, isNull);
  }

  /// Opens disk 1 (state O).
  void open() => ok('open_source', {'kind': 'disk', 'path': disk1});

  /// Opens disk 1 and scans its partition completely (state C). Returns the
  /// catalog.
  Map<String, Object?> scan({String? sessionPath}) {
    open();
    ok('scan_start', {'start': 1048576, 'end': 4000786153472, 'session_path': ?sessionPath});
    finishJob();
    return events('scan_done').last['catalog']! as Map<String, Object?>;
  }

  int get epoch => engine.catalog!.epoch;

  FakeLandmarks get landmarks => engine.catalog!.tree.landmarks;

  int get root => engine.catalog!.mainRoot;

  List<Map<String, Object?>> children(int node, [Map<String, Object?> extra = const {}]) {
    final rows = <Map<String, Object?>>[];
    while (true) {
      final page = ok('list_children', {'epoch': epoch, 'node': node, 'offset': rows.length, 'limit': 1000, ...extra});
      rows.addAll((page['rows']! as List<Object?>).cast());
      if (rows.length >= (page['total']! as int)) return rows;
    }
  }

  List<Map<String, Object?>> search([Map<String, Object?> extra = const {}]) {
    final rows = <Map<String, Object?>>[];
    while (true) {
      final page = ok('search', {'epoch': epoch, 'filter': <String, Object?>{}, 'offset': rows.length, 'limit': 1000, ...extra});
      rows.addAll((page['rows']! as List<Object?>).cast());
      if (rows.length >= (page['total']! as int)) return rows;
    }
  }
}

void main() {
  group('the generated tree', () {
    test('has exactly the requested number of nodes and is deterministic', () {
      for (final nodes in [400, 1000, 3000, 20000]) {
        final tree = FakeTree.generate(seed: 5, mainNodes: nodes);
        expect(tree.instances.first.endNode, nodes);
        expect(tree.count, nodes + 440);
      }
      final a = FakeTree.generate(seed: 9, mainNodes: 2000);
      final names = [for (var i = 0; i < a.count; i++) a.nameOf(i)];
      // (A different seed gives a different tree.)
      final b = FakeTree.generate(seed: 10, mainNodes: 2000);
      expect([for (var i = 0; i < b.count; i++) b.nameOf(i)], isNot(names));
      expect(() => FakeTree.generate(mainNodes: 399), throwsArgumentError);
    });

    test('a few hundred thousand nodes are built quickly', () {
      final watch = Stopwatch()..start();
      final tree = FakeTree.generate(seed: 77, mainNodes: 300000);
      expect(watch.elapsed, lessThan(const Duration(seconds: 10)));
      expect(tree.count, 300440);
      expect(tree.landmarks.hugeFolderFiles, 50000);
      expect(tree.childCount[tree.landmarks.hugeFolder], 50000);
      expect(tree.landmarks.manyFoldersCount, 4000);
    });

    test('every child has a larger id than its parent, and children are contiguous', () {
      final tree = FakeTree.generate(mainNodes: 3000);
      final seen = List<bool>.filled(tree.count, false);
      for (var id = 0; id < tree.count; id++) {
        if (tree.parent[id] >= 0) expect(tree.parent[id], lessThan(id));
        if (!tree.isFolder(id)) continue;
        for (var c = tree.firstChild[id]; c < tree.firstChild[id] + tree.childCount[id]; c++) {
          expect(tree.parent[c], id);
          expect(seen[c], isFalse);
          seen[c] = true;
        }
      }
      // Everything but the five instance roots is somebody's child.
      expect(seen.where((s) => !s), hasLength(5));
    });

    test('names are unique within a folder, ignoring case', () {
      final tree = FakeTree.generate(mainNodes: 3000);
      for (var id = 0; id < tree.count; id++) {
        if (!tree.isFolder(id)) continue;
        final names = <String>{};
        for (var c = tree.firstChild[id]; c < tree.firstChild[id] + tree.childCount[id]; c++) {
          expect(names.add(tree.nameOf(c).toLowerCase()), isTrue, reason: tree.nameOf(c));
        }
      }
    });
  });

  group('transport rules', () {
    test('a line that is no request is answered with id null and bad_request', () {
      final h = Harness();
      for (final line in ['not json', '[1]', '{"cmd": "hello"}', '{"id": "7", "cmd": "hello"}', '{"id": 1.5, "cmd": "hello"}']) {
        h.lines.clear();
        h.engine.handleLine(line);
        expect(h.lines.single['id'], isNull, reason: line);
        expect(h.lines.single['ok'], isFalse);
        expect((h.lines.single['error']! as Map)['code'], 'bad_request');
      }
    });

    test('unknown commands, omitted args, wrong argument types', () {
      final h = Harness();
      expect(h.outcome('frobnicate'), 'unknown_command');
      expect(h.outcome('hello'), 'ok');
      expect(h.outcome('open_source', {'kind': 'disk'}), 'bad_request');
      expect(h.outcome('open_source', {'kind': 'disk', 'path': 5}), 'bad_request');
      expect(h.outcome('open_source', {'kind': 'tape', 'path': 'x'}), 'bad_request');
    });

    test('the response to X_start comes before the events; X_done is last and the slot is free', () {
      final h = Harness()..open();
      final id = h.send('scan_start');
      h.engine.advance(100);
      final index = h.lines.indexWhere((l) => l['id'] == id);
      final firstEvent = h.lines.indexWhere((l) => l['event'] != null);
      expect(index, lessThan(firstEvent));
      expect(h.lines.last['event'], 'scan_done');
      expect(h.lines.where((l) => l['event'] == 'scan_done'), hasLength(1));
      // "...written after the job slot is free and the resulting state can be queried."
      expect(h.outcome('catalog_info'), 'ok');
      expect(h.outcome('carve_start'), 'ok');
    });

    test('requests are answered in arrival order', () {
      final h = Harness();
      final ids = [h.send('hello'), h.send('list_disks'), h.send('catalog_info'), h.send('hello')];
      expect([for (final line in h.lines) line['id']], ids);
    });

    test('a long request emits task_progress and later requests wait for it', () {
      final h = Harness()..engine.script.taskTicks = 3;
      final load = h.send('session_load', {'path': r'C:\sessions\old.hdsc'});
      final hello = h.send('hello');
      expect(h.responseTo(load), isNull);
      expect(h.responseTo(hello), isNull, reason: 'later requests wait');
      h.engine.advance(2);
      expect(h.responseTo(load), isNull);
      h.engine.advance();
      expect(h.responseTo(load)!['ok'], isTrue);
      expect(h.responseTo(hello)!['ok'], isTrue);
      expect(h.events('task_progress').map((e) => e['fraction']), [closeTo(1 / 3, 1e-9), closeTo(2 / 3, 1e-9), 1.0]);
      expect(h.events('task_progress').first['task'], 'session_load');
      final order = [for (final line in h.lines) line['id'] ?? line['event']];
      expect(order, ['task_progress', 'task_progress', 'task_progress', load, hello]);
    });

    test('shutdown answers and exits; end of input exits; a scan saves its session first', () async {
      final h = Harness();
      expect(h.outcome('shutdown'), 'ok');
      expect(await h.engine.exitCode, 0);

      final world = FakeWorld();
      final scanning = Harness(world: world)..open();
      scanning.ok('scan_start', {'session_path': r'C:\s\run.hdsc'});
      scanning.engine.advance(3);
      scanning.engine.endOfInput();
      expect(await scanning.engine.exitCode, 0);
      expect(scanning.events('scan_done'), isEmpty, reason: 'no catalog is built');
      expect(world.savedSessions[r'C:\s\run.hdsc']!.bytesDone, greaterThan(0));
      // The next engine process can load and resume it.
      final next = Harness(world: world);
      final loaded = next.ok('session_load', {'path': r'C:\s\run.hdsc'});
      expect((loaded['catalog']! as Map)['scan_complete'], isFalse);
    });
  });

  group('errors by state (the table of PROTOCOL section 1)', () {
    /// Builds a harness in each of the six states.
    Map<String, Harness> states() {
      final scanning = Harness()..open();
      scanning.ok('scan_start');
      final browse = Harness();
      browse.ok('session_load', {'path': r'C:\sessions\detached.hdsc'});
      final job = Harness()..scan();
      job.ok('image_start', {'dest': r'D:\disk.img'});
      return {
        'N': Harness(),
        'O': Harness()..open(),
        'S': scanning,
        'C': Harness()..scan(),
        'B': browse,
        'J': job,
      };
    }

    void expectByState(String command, Map<String, Object?> Function(Harness h) args, Map<String, String> expected) {
      final all = states();
      for (final MapEntry(key: state, value: h) in all.entries) {
        expect(h.outcome(command, args(h)), expected[state], reason: '$command in state $state');
      }
    }

    int epochOf(Harness h) => h.engine.catalog?.epoch ?? 1;

    test('hello, list_disks, job_cancel are always answered', () {
      for (final command in ['hello', 'list_disks', 'job_cancel']) {
        expectByState(command, (_) => {}, {'N': 'ok', 'O': 'ok', 'S': 'ok', 'C': 'ok', 'B': 'ok', 'J': 'ok'});
      }
    });

    test('open_source, close_source, session_load, session_attach_source, geometry_set, instance_set, session_save: busy in S and J', () {
      expectByState('open_source', (_) => {'kind': 'disk', 'path': disk1},
          {'N': 'ok', 'O': 'ok', 'S': 'busy', 'C': 'ok', 'B': 'ok', 'J': 'busy'});
      expectByState('close_source', (_) => {}, {'N': 'ok', 'O': 'ok', 'S': 'busy', 'C': 'ok', 'B': 'ok', 'J': 'busy'});
      expectByState('session_load', (_) => {'path': r'C:\s\x.hdsc'},
          {'N': 'ok', 'O': 'ok', 'S': 'busy', 'C': 'ok', 'B': 'ok', 'J': 'busy'});
      expectByState('session_attach_source', (_) => {'kind': 'disk', 'path': disk1},
          {'N': 'no_session', 'O': 'no_session', 'S': 'busy', 'C': 'ok', 'B': 'ok', 'J': 'busy'});
      expectByState('geometry_set', (_) => {'instance': 0, 'cluster_size': 4096, 'volume_offset': 1048576},
          {'N': 'no_catalog', 'O': 'no_catalog', 'S': 'busy', 'C': 'ok', 'B': 'ok', 'J': 'busy'});
      expectByState('instance_set', (_) => {'instance': 1, 'kind': 'other'},
          {'N': 'no_catalog', 'O': 'no_catalog', 'S': 'busy', 'C': 'ok', 'B': 'ok', 'J': 'busy'});
      expectByState('session_save', (_) => {'path': r'C:\s\copy.hdsc'},
          {'N': 'no_session', 'O': 'no_session', 'S': 'busy', 'C': 'ok', 'B': 'ok', 'J': 'busy'});
    });

    test('scan_start', () {
      expectByState('scan_start', (_) => {}, {'N': 'no_source', 'O': 'ok', 'S': 'busy', 'C': 'ok', 'B': 'no_source', 'J': 'busy'});
      final h = Harness()..open();
      expect(h.outcome('scan_start', {'resume': true}), 'no_session');
      expect(h.outcome('scan_start', {'start': 100}), 'bad_request', reason: 'not sector-aligned');
      expect(h.outcome('scan_start', {'start': 1024, 'end': 512}), 'bad_request');
      expect(h.outcome('scan_start', {'end': 4000787030016 + 512}), 'bad_request', reason: 'beyond the source');
      final refused = h.error('scan_start', {'session_path': r'E:\on-the-source.hdsc'});
      expect(refused['code'], 'destination_refused');
      expect((refused['data']! as Map)['verdict'], 'overlaps_source');
    });

    test('catalog queries', () {
      for (final command in ['catalog_info', 'list_children', 'node_info', 'search', 'selection_summary']) {
        expectByState(
          command,
          (h) => {'epoch': epochOf(h), 'node': 0, 'filter': <String, Object?>{}, 'include': [0], 'exclude': <int>[]},
          {'N': 'no_catalog', 'O': 'no_catalog', 'S': 'no_catalog', 'C': 'ok', 'B': 'ok', 'J': 'ok'},
        );
      }
      final h = Harness()..scan();
      for (final command in ['list_children', 'node_info', 'search', 'selection_summary']) {
        final args = {'epoch': h.epoch + 1, 'node': 0, 'filter': <String, Object?>{}, 'include': [0], 'exclude': <int>[]};
        expect(h.outcome(command, args), 'stale_catalog', reason: command);
        expect(h.outcome(command, {...args, 'epoch': null}), 'bad_request', reason: '$command requires epoch');
      }
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': 99999999}), 'not_found');
      expect(h.outcome('node_info', {'epoch': h.epoch, 'node': -1}), 'not_found');
      expect(h.outcome('search', {'epoch': h.epoch, 'under': 99999999, 'filter': <String, Object?>{}}), 'not_found');
      expect(h.outcome('selection_summary', {'epoch': h.epoch, 'include': [99999999], 'exclude': <int>[]}), 'not_found');
    });

    test('check_destination', () {
      expectByState('check_destination', (_) => {'path': r'D:\Rescued', 'purpose': 'recover'},
          {'N': 'no_source', 'O': 'ok', 'S': 'ok', 'C': 'ok', 'B': 'no_source', 'J': 'ok'});
    });

    test('recover_start and carve_start', () {
      expectByState(
        'recover_start',
        (h) => {'epoch': epochOf(h), 'include': [0], 'exclude': <int>[], 'dest': r'D:\Rescued'},
        {'N': 'no_catalog', 'O': 'no_catalog', 'S': 'no_catalog', 'C': 'ok', 'B': 'no_source', 'J': 'busy'},
      );
      expectByState('carve_start', (_) => {},
          {'N': 'no_catalog', 'O': 'no_catalog', 'S': 'no_catalog', 'C': 'ok', 'B': 'no_source', 'J': 'busy'});

      final h = Harness()..scan();
      Map<String, Object?> recover(Map<String, Object?> extra) =>
          {'epoch': h.epoch, 'include': [h.root], 'exclude': <int>[], 'dest': r'D:\Rescued', ...extra};
      expect(h.outcome('recover_start', recover({'include': <int>[]})), 'bad_request', reason: 'empty selection');
      expect(h.outcome('recover_start', recover({'include': <int>[], 'exclude': [h.root]})), 'bad_request');
      expect(h.outcome('recover_start', recover({'epoch': h.epoch + 5})), 'stale_catalog');
      final refused = h.error('recover_start', recover({'dest': r'E:\Rescued'}));
      expect(refused['code'], 'destination_refused');
      expect((refused['data']! as Map)['verdict'], 'overlaps_source');
      expect(h.outcome('recover_start', recover({'dest': r'E:\Rescued', 'allow_same_disk': true, 'allow_unknown_disk': true})),
          'destination_refused', reason: 'there is no override for the session range');
      expect(h.outcome('recover_start', recover({'dest': r'S:\Rescued'})), 'destination_refused');
      expect(h.outcome('recover_start', recover({'dest': r'U:\Rescued'})), 'destination_refused');
      expect(h.outcome('recover_start', recover({'dest': r'U:\Rescued', 'allow_same_disk': true})), 'destination_refused',
          reason: 'the two consents are separate');
      expect(h.outcome('recover_start', recover({'dest': 'Rescued'})), 'destination_refused');
      expect(h.outcome('recover_start', recover({'dest': r'T:\Rescued'})), 'no_space');
      expect(h.outcome('recover_start', recover({'dest': r'S:\Rescued', 'allow_same_disk': true})), 'ok');

      final noHits = Harness()..open();
      noHits.ok('scan_start', {'collect_carve_hits': false});
      noHits.finishJob();
      expect(noHits.outcome('carve_start'), 'unsupported');
      final kinds = Harness()..scan();
      expect(kinds.outcome('carve_start', {'kinds': <String>[]}), 'bad_request');
      expect(kinds.outcome('carve_start', {'kinds': ['holograms']}), 'bad_request');
    });

    test('image_start', () {
      expectByState('image_start', (_) => {'dest': r'D:\img\disk.img'},
          {'N': 'no_source', 'O': 'ok', 'S': 'busy', 'C': 'ok', 'B': 'no_source', 'J': 'busy'});
      final h = Harness()..open();
      expect(h.outcome('image_start', {'dest': r'E:\disk.img'}), 'destination_refused');
      expect(h.outcome('image_start', {'dest': r'T:\disk.img'}), 'no_space');
      expect(h.outcome('image_start', {'dest': r'K:\disk.img'}), 'no_space', reason: 'larger than a FAT32 file can be');
      expect(h.outcome('image_start', {'dest': r'D:\already-exists.img'}), 'exists');
      expect(h.outcome('image_start', {'dest': r'D:\already-exists.img', 'resume': true}), 'ok');
      h.finishJob();
      expect(h.outcome('image_start', {'dest': r'D:\already-exists.img'}), 'exists');
      expect(h.outcome('image_start', {'dest': r'D:\x.img', 'start': 1}), 'bad_request');
    });

    test('read_file', () {
      expectByState('read_file', (h) => {'epoch': epochOf(h), 'node': h.engine.catalog?.tree.landmarks.sampleText ?? 0},
          {'N': 'no_catalog', 'O': 'no_catalog', 'S': 'no_catalog', 'C': 'ok', 'B': 'no_source', 'J': 'ok'});
      final h = Harness()..scan();
      expect(h.outcome('read_file', {'epoch': h.epoch, 'node': h.landmarks.previewSamples}), 'bad_request', reason: 'a folder');
      expect(h.outcome('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleCompressed}), 'unsupported');
      expect(h.outcome('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleText, 'length': 8388609}), 'bad_request');
      expect(h.outcome('read_file', {'epoch': h.epoch + 1, 'node': h.landmarks.sampleText}), 'stale_catalog');
    });

    test('session_save and session_attach_source', () {
      final h = Harness()..scan();
      final refused = h.error('session_save', {'path': r'E:\copy.hdsc'});
      expect(refused['code'], 'destination_refused');
      expect((refused['data']! as Map)['verdict'], 'overlaps_source');
      expect(h.ok('session_save', {'path': r'C:\s\copy.hdsc'})['path'], r'C:\s\copy.hdsc');

      final mismatch = h.error('session_attach_source', {'kind': 'disk', 'path': r'\\.\PhysicalDrive2-mismatch'});
      expect(mismatch['code'], 'source_mismatch');
      expect(mismatch['data'], isNotNull, reason: 'it names the failed checks');
      expect(h.outcome('session_attach_source', {'kind': 'image', 'path': r'D:\fewsamples.img'}), 'source_mismatch');
      expect(h.outcome('session_attach_source', {'kind': 'image', 'path': r'D:\fewsamples.img', 'force': true}), 'ok');
      expect(h.outcome('session_attach_source', {'kind': 'image', 'path': r'D:\mismatch.img', 'force': true}), 'source_mismatch',
          reason: 'a mismatching source is always refused');
    });

    test('access_denied is the sign of a missing elevation', () {
      final h = Harness(elevated: false);
      expect(h.ok('hello')['elevated'], isFalse);
      expect(h.outcome('open_source', {'kind': 'disk', 'path': disk1}), 'access_denied');
      expect(h.outcome('open_source', {'kind': 'image', 'path': r'D:\disk.img'}), 'ok');
    });
  });

  group('sources', () {
    test('hello and list_disks', () {
      final h = Harness();
      final hello = h.ok('hello');
      expect(hello['protocol'], 1);
      expect(hello['carve_kinds'], hasLength(17));
      final disks = (h.ok('list_disks')['disks']! as List<Object?>).cast<Map<String, Object?>>();
      expect(disks.map((d) => d['number']), [0, 1, 2, 3, 4, 5, 6]);
      expect(disks.map((d) => d['media']).toSet(), {'hdd', 'ssd', 'unknown'});
      expect(disks.map((d) => d['trim']).toSet(), {true, false, null});
      expect(disks.map((d) => d['table']).toSet(), {'mbr', 'gpt', 'none'});
      expect(disks.expand((d) => d['flags']! as List<Object?>).toSet(), {'virtual', 'spaces', 'dynamic', 'no_media'});
      final partitions = disks.expand((d) => (d['partitions']! as List<Object?>).cast<Map<String, Object?>>());
      // The volumes Windows exposes, read without a single sector: the
      // mounted file system (null for a raw volume), no type, no NTFS
      // details; partitions without a volume are missing.
      expect(partitions.map((p) => p['fs']).toSet(), {'ntfs', 'fat', 'exfat', null});
      expect(partitions.map((p) => (p['type'], p['type_id'], p['ntfs'])).toSet(), {('', '', null)});
      expect(disks.where((d) => d['is_system'] == true), hasLength(1));
      final disk2 = disks.firstWhere((d) => d['number'] == 2);
      expect((disk2['partitions']! as List<Object?>).map((p) => (p! as Map)['index']), [1, 2, 3, 4, 6],
          reason: 'the Linux partition has no volume; Windows numbers have gaps');
      expect(((disks.firstWhere((d) => d['number'] == 6))['partitions']! as List), isEmpty,
          reason: 'a Storage Spaces member exposes no volume');

      // open_source reads the table itself: types, NTFS details, every partition.
      final opened = h.ok('open_source', {'kind': 'disk', 'path': disk2['path']});
      final table = (opened['partitions']! as List<Object?>).cast<Map<String, Object?>>();
      expect(table.map((p) => p['index']), [1, 2, 3, 4, 5, 6]);
      expect(table.map((p) => p['fs']).toSet(), {'ntfs', 'exfat', 'fat', 'bitlocker', 'other', null});
      expect(table.first['type'], isNotEmpty);
      expect(table.first['ntfs'], isNotNull);
      expect((Harness(os: 'linux').ok('list_disks')['disks']! as List), isEmpty, reason: 'empty on non-Windows hosts');
    });

    test('every partition lies inside its disk, in offset order, without overlaps', () {
      for (final disk in FakeWorld().disks) {
        final size = disk['size']! as int;
        var end = 0;
        for (final p in (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>()) {
          final offset = p['offset']! as int;
          expect(offset, greaterThanOrEqualTo(end), reason: '${disk['path']} partition ${p['index']}');
          end = offset + (p['size']! as int);
          expect(end, lessThanOrEqualTo(size), reason: '${disk['path']} partition ${p['index']}');
          expect(offset % 512 + (p['size']! as int) % 512, 0, reason: 'sector-aligned');
        }
      }
    });

    test('open_source: disks, warnings, images, errors', () {
      final h = Harness();
      final system = h.ok('open_source', {'kind': 'disk', 'path': r'\\.\PhysicalDrive0'});
      expect((system['warnings']! as List<Object?>).map((w) => (w! as Map)['code']).toSet(), {
        'trim_supported', 'mounted_volume', 'system_disk', 'page_file_on_source', 'app_on_source', //
      });
      final victim = h.ok('open_source', {'kind': 'disk', 'path': r'\\.\physicaldrive1'});
      expect((victim['source']! as Map)['disk_number'], 1);
      expect((victim['warnings']! as List<Object?>).map((w) => (w! as Map)['code']), ['mounted_volume']);
      expect(victim['table'], 'gpt');

      expect(h.outcome('open_source', {'kind': 'disk', 'path': r'E:'}), 'bad_request');
      expect(h.outcome('open_source', {'kind': 'disk', 'path': r'\\.\PhysicalDrive9'}), 'not_found');
      expect(h.outcome('open_source', {'kind': 'disk', 'path': r'\\.\PhysicalDrive5'}), 'io_error', reason: 'no medium');
      expect(h.outcome('open_source', {'kind': 'image', 'path': r'D:\missing.img'}), 'not_found');
      expect(h.outcome('open_source', {'kind': 'image', 'path': r'D:\denied.img'}), 'access_denied');

      final image = h.ok('open_source', {'kind': 'image', 'path': r'D:\backup.img', 'sector_size': 4096});
      expect((image['source']! as Map)['model'], isNull);
      expect((image['source']! as Map)['sector_size'], 4096);
      expect(image['table'], 'mbr');
      final nopart = h.ok('open_source', {'kind': 'image', 'path': '/data/nopart.img'});
      expect(nopart['table'], 'none');
      expect(((nopart['partitions']! as List<Object?>).single! as Map)['index'], 0);
      expect((h.ok('open_source', {'kind': 'image', 'path': '/data/gpt.img'})['partitions']! as List), hasLength(2));
    });

    test('open_source drops the session and the catalog', () {
      final h = Harness()..scan();
      h.open();
      expect(h.outcome('catalog_info'), 'no_catalog');
      expect(h.outcome('session_save', {'path': r'C:\x.hdsc'}), 'no_session');
    });

    test('destinations by name', () {
      final h = Harness()..open();
      String verdict(String path) => h.ok('check_destination', {'path': path, 'purpose': 'recover'})['verdict']! as String;
      expect(verdict(r'D:\Rescued'), 'ok');
      expect(verdict(r'\\nas\share\Rescued'), 'ok');
      expect(verdict('/home/me/rescued'), 'ok');
      expect(verdict(r'E:\Rescued'), 'overlaps_source');
      expect(verdict(r'e:\Rescued'), 'overlaps_source');
      expect(verdict('/overlap/x'), 'overlaps_source');
      expect(verdict(r'S:\Rescued'), 'same_disk');
      expect(verdict('/same-disk/x'), 'same_disk');
      expect(verdict(r'U:\Rescued'), 'unknown_disk');
      expect(verdict(r'\\localhost\c$\Rescued'), 'unknown_disk');
      expect(verdict(r'Q:\Rescued'), 'not_found');
      expect(verdict(r'R:\Rescued'), 'not_writable');
      expect(verdict('Rescued'), 'invalid_path');
      expect(verdict(r'..\Rescued'), 'invalid_path');

      final sized = h.ok('check_destination', {'path': r'T:\x', 'purpose': 'recover', 'need_bytes': 5000000});
      expect(sized['enough_space'], isFalse);
      expect(h.ok('check_destination', {'path': r'T:\x', 'purpose': 'recover'})['enough_space'], isNull);
      final fat = h.ok('check_destination', {'path': r'K:\x', 'purpose': 'image', 'need_bytes': 1});
      expect(fat['fs'], 'FAT32');
      expect(fat['max_file_bytes'], 4294967295);
      expect(fat['enough_space'], isTrue);
      expect(h.outcome('check_destination', {'path': r'D:\x', 'purpose': 'fun'}), 'bad_request');
    });
  });

  group('catalog epochs', () {
    test('every scan_done, session_load and session_attach_source starts a new epoch', () {
      final h = Harness();
      final epochs = <int>[];
      epochs.add(h.scan()['epoch']! as int);
      h.ok('scan_start', {'start': 1048576, 'end': 4000786153472});
      h.finishJob();
      epochs.add((h.events('scan_done').last['catalog']! as Map)['epoch']! as int);
      epochs.add(((h.ok('session_load', {'path': r'C:\s\a.hdsc'})['catalog']! as Map)['epoch']! as int));
      epochs.add(((h.ok('session_attach_source', {'kind': 'disk', 'path': disk1})['catalog']! as Map)['epoch']! as int));
      expect(epochs.toSet(), hasLength(4));
      expect(h.outcome('list_children', {'epoch': epochs[2], 'node': 0}), 'stale_catalog');
      expect(h.outcome('list_children', {'epoch': epochs[3], 'node': 0}), 'ok');
    });

    test('scan_start ends the epoch at once', () {
      final h = Harness()..scan();
      final old = h.epoch;
      h.ok('scan_start', {'resume': true});
      expect(h.outcome('list_children', {'epoch': old, 'node': 0}), 'no_catalog');
      expect(h.outcome('catalog_info'), 'no_catalog');
    });

    test('geometry_set and carve_start keep the epoch; discarded carved ids answer not_found', () {
      final h = Harness()..scan();
      final epoch = h.epoch;
      h.ok('carve_start', {'kinds': ['jpeg', 'png']});
      h.finishJob();
      final afterCarve = h.events('carve_done').last['catalog']! as Map<String, Object?>;
      expect(afterCarve['epoch'], epoch);
      final carvedRoot = h.engine.catalog!.carvedRoot!;
      final kindFolders = h.children(carvedRoot);
      expect(kindFolders.map((r) => r['name']).toSet(), {'jpg', 'png'});
      final jpgFiles = h.children(kindFolders.firstWhere((r) => r['name'] == 'jpg')['id']! as int);
      final pngFiles = h.children(kindFolders.firstWhere((r) => r['name'] == 'png')['id']! as int);
      final oldJpg = jpgFiles.first['id']! as int;
      final oldPng = pngFiles.first['id']! as int;

      // Carving jpeg again replaces the jpeg nodes (new ids) and keeps png.
      h.ok('carve_start', {'kinds': ['jpeg']});
      expect(h.outcome('node_info', {'epoch': epoch, 'node': oldJpg}), 'not_found', reason: 'replaced when the run starts');
      h.finishJob();
      expect(h.epoch, epoch);
      expect(h.outcome('node_info', {'epoch': epoch, 'node': oldJpg}), 'not_found');
      expect(h.outcome('node_info', {'epoch': epoch, 'node': oldPng}), 'ok');
      final newJpg = h.children(carvedRoot).firstWhere((r) => r['name'] == 'jpg')['id']! as int;
      expect(h.children(newJpg).map((r) => r['id']), isNot(contains(oldJpg)), reason: 'ids are never reused');

      // A new geometry of the main instance discards all carved nodes.
      final info = h.ok('geometry_set', {'instance': 0, 'cluster_size': 8192, 'volume_offset': 1048576});
      expect(info['epoch'], epoch);
      expect(info['carved'], 0);
      expect((((info['instances']! as List<Object?>).first! as Map)['virtual']! as Map)['carved'], isNull);
      expect(h.outcome('node_info', {'epoch': epoch, 'node': oldPng}), 'not_found');
      expect(h.outcome('list_children', {'epoch': epoch, 'node': carvedRoot}), 'not_found');
      expect(h.outcome('list_children', {'epoch': epoch, 'node': h.root}), 'ok', reason: 'the tree stays');
    });

    test('bumpEpoch makes old ids stale', () {
      final h = Harness()..scan();
      final old = h.epoch;
      h.engine.bumpEpoch();
      expect(h.outcome('list_children', {'epoch': old, 'node': 0}), 'stale_catalog');
      expect(h.ok('catalog_info')['epoch'], isNot(old));
    });
  });

  group('catalog_info', () {
    test('instances of every kind, the main one first, each a tree of its own', () {
      final h = Harness();
      final catalog = h.scan();
      final instances = (catalog['instances']! as List<Object?>).cast<Map<String, Object?>>();
      expect(instances.map((i) => i['kind']), ['main', 'older', 'other', 'embedded', 'strays']);
      expect(instances.first['label'], '');
      expect(instances.first['root'], 0);
      expect(instances.map((i) => i['root']).toSet(), hasLength(5));
      for (final instance in instances) {
        final row = h.ok('node_info', {'epoch': h.epoch, 'node': instance['root']})['row']! as Map<String, Object?>;
        expect(row['parent'], isNull, reason: 'no instance root is a child of another');
        expect(row['name'], instance['label']);
      }
      expect(instances[3]['embedded_in'], 'Backups/backup.vhd');
      expect((instances[0]['virtual']! as Map)['lost'], isNotNull);
      expect((instances[0]['virtual']! as Map)['orphans'], isNotNull);
      expect((instances[0]['virtual']! as Map)['carved'], isNull);
      expect(catalog['scan_complete'], isTrue);
      expect(catalog['source_attached'], isTrue);
      expect(catalog['nodes'], 3440);
    });

    test('counts of an instance match its tree', () {
      final h = Harness();
      final catalog = h.scan();
      final main = (catalog['instances']! as List<Object?>).first! as Map<String, Object?>;
      final all = h.search({'show_deleted': true});
      final live = all.where((r) => r['deleted'] == false);
      expect(main['files'], live.length);
      expect(main['deleted_files'], all.length - live.length);
      expect(main['bytes'], live.fold<int>(0, (sum, r) => sum + (r['size']! as int)));
      final rootRow = h.ok('node_info', {'epoch': h.epoch, 'node': h.root})['row']! as Map<String, Object?>;
      expect(rootRow['files'], main['files']);
      expect(rootRow['size'], main['bytes']);
      expect(rootRow['deleted_files'], main['deleted_files']);
    });
  });

  group('list_children', () {
    late Harness h;
    setUp(() => h = Harness()..scan());

    test('pages add up to the listing, without gaps or duplicates', () {
      final all = h.children(h.landmarks.hugeFolder);
      expect(all, hasLength(h.landmarks.hugeFolderFiles - all.where((r) => false).length - _deletedIn(h, h.landmarks.hugeFolder)));
      final paged = <Object?>[];
      for (var offset = 0; ; offset += 37) {
        final page = h.ok('list_children', {'epoch': h.epoch, 'node': h.landmarks.hugeFolder, 'offset': offset, 'limit': 37});
        expect(page['total'], all.length);
        final rows = page['rows']! as List<Object?>;
        if (rows.isEmpty) break;
        paged.addAll(rows.map((r) => (r! as Map)['id']));
      }
      expect(paged, all.map((r) => r['id']));
      expect(paged.toSet(), hasLength(paged.length));
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': h.root, 'limit': 1001}), 'bad_request');
    });

    test('folders sort before files; ties by name without case, then id; null values last', () {
      for (final sort in ['name', 'size', 'modified', 'rating', 'ext']) {
        for (final desc in [false, true]) {
          final rows = h.children(h.landmarks.documents, {'sort': sort, 'desc': desc, 'show_deleted': true});
          expect(rows.length, greaterThan(3));
          final firstFile = rows.indexWhere((r) => r['kind'] == 'file');
          expect(rows.skip(firstFile).every((r) => r['kind'] == 'file'), isTrue, reason: '$sort desc=$desc: folders first');
          for (final group in [rows.take(firstFile).toList(), rows.skip(firstFile).toList()]) {
            for (var i = 1; i < group.length; i++) {
              final a = group[i - 1];
              final b = group[i];
              Object? key(Map<String, Object?> r) => switch (sort) {
                'name' => (r['name']! as String).toLowerCase(),
                'size' => r['size'],
                'modified' => r['modified'],
                'rating' => r['rating'] == null ? null : fakeRatingNames.indexOf(r['rating']! as String),
                _ => r['ext'],
              };
              final ka = key(a);
              final kb = key(b);
              if (ka == null || kb == null) {
                expect(ka == null && kb != null, isFalse, reason: '$sort desc=$desc: null values last');
                if (ka != null) continue;
              }
              var order = ka == null ? 0 : Comparable.compare(ka as Comparable<Object?>, kb! as Comparable<Object?>);
              if (desc) order = -order;
              expect(order, lessThanOrEqualTo(0), reason: '$sort desc=$desc at $i');
              if (order == 0) {
                final names = (a['name']! as String).toLowerCase().compareTo((b['name']! as String).toLowerCase());
                expect(names, lessThanOrEqualTo(0), reason: '$sort desc=$desc: ties by name');
                if (names == 0) expect(a['id']! as int, lessThan(b['id']! as int));
              }
            }
          }
        }
      }
    });

    test('show_deleted: deleted files and deleted folders without live files are hidden', () {
      final hidden = h.children(h.root).map((r) => r['name']).toList();
      final shown = h.children(h.root, {'show_deleted': true}).map((r) => r['name']).toList();
      expect(hidden, isNot(contains('Deleted folder')));
      expect(hidden, contains('Mixed deleted'), reason: 'a deleted folder with live files stays');
      expect(shown, contains('Deleted folder'));
      expect(h.children(h.landmarks.mixedDeletedFolder), hasLength(3));
      expect(h.children(h.landmarks.mixedDeletedFolder, {'show_deleted': true}), hasLength(6));
      expect(h.children(h.landmarks.deletedOnlyFolder), isEmpty);
      expect(h.children(h.landmarks.deletedOnlyFolder, {'show_deleted': true}), hasLength(8));
    });

    test('dirs_only, and exts / ratings filter files while folders are always listed', () {
      final all = h.children(h.landmarks.documents);
      final folders = all.where((r) => r['kind'] != 'file').toList();
      expect(folders, isNotEmpty);
      expect(h.children(h.landmarks.documents, {'dirs_only': true}), folders);

      final jpg = h.children(h.landmarks.documents, {'exts': ['jpg', 'pdf']});
      expect(jpg.where((r) => r['kind'] != 'file'), folders);
      expect(jpg.where((r) => r['kind'] == 'file').map((r) => r['ext']).toSet().difference({'jpg', 'pdf'}), isEmpty);
      expect(jpg.where((r) => r['kind'] == 'file'), all.where((r) => r['kind'] == 'file' && ['jpg', 'pdf'].contains(r['ext'])));

      final good = h.children(h.landmarks.flagSamples, {'ratings': ['damaged', 'unsupported'], 'show_deleted': true});
      expect(good.map((r) => r['rating']).toSet(), {'damaged', 'unsupported'});
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': h.root, 'exts': <String>[]}), 'bad_request');
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': h.root, 'ratings': ['perfect']}), 'bad_request');
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': h.landmarks.sampleText}), 'bad_request', reason: 'not a folder');
    });

    test('has_children says what a dirs_only listing would return', () {
      for (final showDeleted in [false, true]) {
        final rows = h.children(h.root, {'show_deleted': showDeleted});
        for (final row in rows.where((r) => r['kind'] != 'file')) {
          final sub = h.children(row['id']! as int, {'dirs_only': true, 'show_deleted': showDeleted});
          expect(row['has_children'], sub.isNotEmpty, reason: '${row['name']} show_deleted=$showDeleted');
        }
        expect(rows.where((r) => r['kind'] == 'file').every((r) => r['has_children'] == false), isTrue);
      }
    });

    test('folder aggregates are recursive and independent of the request filters', () {
      for (final folder in [h.landmarks.documents, h.landmarks.mixedDeletedFolder, h.landmarks.manyFolders]) {
        final below = h.search({'under': folder, 'show_deleted': true});
        final live = below.where((r) => r['deleted'] == false);
        final gone = below.where((r) => r['deleted'] == true);
        for (final extra in [<String, Object?>{}, {'show_deleted': true, 'exts': ['zip']}]) {
          final parent = h.engine.catalog!.tree.parent[folder];
          final row = h.children(parent, {'show_deleted': true, ...extra}).firstWhere((r) => r['id'] == folder);
          expect(row['files'], live.length);
          expect(row['size'], live.fold<int>(0, (s, r) => s + (r['size']! as int)));
          expect(row['deleted_files'], gone.length);
          expect(row['deleted_size'], gone.fold<int>(0, (s, r) => s + (r['size']! as int)));
          expect(row['rating'], isNull);
        }
      }
    });

    test('row kinds: dir, file, lost_dir, virtual', () {
      final rootRows = h.children(h.root);
      final lost = rootRows.firstWhere((r) => r['name'] == '[Lost folders]');
      expect(lost['kind'], 'virtual');
      expect(rootRows.firstWhere((r) => r['name'] == '[Orphaned files]')['kind'], 'virtual');
      final lostDirs = h.children(lost['id']! as int);
      expect(lostDirs.map((r) => r['kind']).toSet(), {'lost_dir'});
      expect(lostDirs.every((r) => RegExp(r'^Folder #\d+$').hasMatch(r['name']! as String)), isTrue);
      expect(rootRows.firstWhere((r) => r['name'] == 'Documents')['kind'], 'dir');
    });
  });

  group('search', () {
    late Harness h;
    setUp(() => h = Harness()..scan());

    test('returns only files, with path and root-first ancestor ids', () {
      final rows = h.search({'filter': {'query': 'IMG_'}});
      expect(rows, isNotEmpty);
      for (final row in rows.take(40)) {
        expect(row['kind'], 'file');
        expect((row['name']! as String).toLowerCase(), contains('img_'));
        final info = h.ok('node_info', {'epoch': h.epoch, 'node': row['id']});
        expect(row['path'], info['path']);
        expect(row['ancestor_ids'], (info['ancestors']! as List<Object?>).map((a) => (a! as Map)['id']));
        expect((row['ancestor_ids']! as List).first, h.root);
        expect((row['ancestor_ids']! as List).last, row['parent']);
      }
    });

    test('query is a case-insensitive substring; exts, sizes and ratings combine', () {
      final upper = h.search({'filter': {'query': 'REPORT'}});
      final lower = h.search({'filter': {'query': 'report'}});
      expect(upper.map((r) => r['id']), lower.map((r) => r['id']));
      expect(lower, isNotEmpty);

      final all = h.search({'show_deleted': true});
      bool matches(Map<String, Object?> r) =>
          ['pdf', 'docx'].contains(r['ext']) && (r['size']! as int) >= 5000 && (r['size']! as int) <= 5000000 && r['rating'] == 'good';
      final filtered = h.search({
        'show_deleted': true,
        'filter': {'exts': ['pdf', 'docx'], 'min_size': 5000, 'max_size': 5000000, 'ratings': ['good']},
      });
      expect(filtered.map((r) => r['id']).toSet(), all.where(matches).map((r) => r['id']).toSet());
      expect(filtered, isNotEmpty);
      expect(h.outcome('search', {'epoch': h.epoch, 'filter': {'exts': <String>[]}}), 'bad_request');
    });

    test('under limits the search to a subtree; the default is the main root', () {
      final inDocuments = h.search({'under': h.landmarks.documents});
      expect(inDocuments, isNotEmpty);
      expect(inDocuments.every((r) => (r['ancestor_ids']! as List).contains(h.landmarks.documents)), isTrue);
      final everything = h.search();
      expect(everything.length, greaterThan(inDocuments.length));
      expect(everything.every((r) => (r['ancestor_ids']! as List).first == h.root), isTrue, reason: 'other instances are not searched');
      final older = h.engine.catalog!.tree.instances[1].root;
      expect(h.search({'under': older}).every((r) => (r['flags']! as List).contains('older_generation')), isTrue);
    });

    test('deleted files only with show_deleted', () {
      expect(h.search().any((r) => r['deleted'] == true), isFalse);
      expect(h.search({'show_deleted': true}).any((r) => r['deleted'] == true), isTrue);
    });
  });

  group('ratings and flags', () {
    late Harness h;
    setUp(() => h = Harness()..scan());

    test('every flag and every rating occurs', () {
      final samples = h.children(h.landmarks.flagSamples, {'show_deleted': true});
      expect(samples.map((r) => (r['flags']! as List).single).toSet(), fakeFlagNames.toSet());
      for (final row in samples) {
        expect(row['name'], '${(row['flags']! as List).single}.pdf');
      }
      final everything = h.search({'show_deleted': true});
      expect(everything.map((r) => r['rating']).toSet(), {'good', 'uncertain', 'damaged', 'unsupported'});
      expect(everything.expand((r) => r['flags']! as List<Object?>).toSet(), containsAll(fakeFlagNames.where((f) => !['older_generation', 'embedded', 'stray'].contains(f))));
    });

    test('the rating follows from the flags (ALGORITHMS G3)', () {
      String expected(List<Object?> flags) {
        bool any(List<String> names) => names.any(flags.contains);
        if (any(['compressed', 'encrypted', 'placeholder', 'embedded'])) return 'unsupported';
        if (any(['missing_fragments', 'out_of_bounds', 'overwritten', 'bad_sectors', 'signature_mismatch'])) return 'damaged';
        if (any(['deleted', 'overlap', 'torn_record', 'no_data', 'older_generation', 'stray'])) return 'uncertain';
        return 'good';
      }

      final tree = h.engine.catalog!.tree;
      for (final instance in tree.instances) {
        final rows = h.search({'under': instance.root, 'show_deleted': true});
        expect(rows, isNotEmpty);
        // The geometry of "older", "embedded" and "strays" is not validated:
        // their files are rated at most uncertain.
        final validated = instance.kind == 'main' || instance.kind == 'other';
        for (final row in rows) {
          var want = expected(row['flags']! as List<Object?>);
          if (!validated && want == 'good') want = 'uncertain';
          expect(row['rating'], want, reason: '${row['name']} ${row['flags']}');
          expect(row['deleted'], (row['flags']! as List).contains('deleted'));
        }
      }
      expect(h.search({'under': tree.instances[3].root}).every((r) => r['rating'] == 'unsupported'), isTrue);
    });

    test('a geometry that is not validated lowers good files to uncertain', () {
      final before = h.search({'filter': {'ratings': ['good']}}).length;
      expect(before, greaterThan(0));
      final info = h.ok('geometry_set', {'instance': 0, 'cluster_size': 512, 'volume_offset': 2097152});
      final geometry = ((info['instances']! as List<Object?>).first! as Map)['geometry']! as Map;
      expect(geometry['cluster_source'], 'manual');
      expect(geometry['offset_source'], 'manual');
      expect(geometry['validated'], isFalse);
      expect(h.search({'filter': {'ratings': ['good']}}), isEmpty);
      // Choosing a candidate again restores it.
      final back = h.ok('geometry_set', {'instance': 0, 'cluster_size': 4096, 'volume_offset': 1048576});
      expect((((back['instances']! as List<Object?>).first! as Map)['geometry']! as Map)['offset_source'], 'index');
      expect(h.search({'filter': {'ratings': ['good']}}), hasLength(before));
      expect(h.outcome('geometry_set', {'instance': 0, 'cluster_size': 1000, 'volume_offset': 0}), 'bad_request');
      expect(h.outcome('geometry_set', {'instance': 0, 'cluster_size': 4096, 'volume_offset': 100}), 'bad_request');
      expect(h.outcome('geometry_set', {'instance': 9, 'cluster_size': 4096, 'volume_offset': 0}), 'not_found');
    });
  });

  group('selection_summary', () {
    late Harness h;
    late List<Map<String, Object?>> allFiles;
    setUp(() {
      h = Harness()..scan();
      final tree = h.engine.catalog!.tree;
      allFiles = [for (final instance in tree.instances) ...h.search({'under': instance.root, 'show_deleted': true})];
    });

    /// The definition of the document, applied file by file.
    Map<String, int> bruteForce(List<int> include, List<int> exclude, {bool includeDeleted = false, bool skipDamaged = false, bool Function(Map<String, Object?>)? filter}) {
      final counts = {'files': 0, 'bytes': 0, 'good': 0, 'uncertain': 0, 'damaged': 0, 'unsupported': 0};
      for (final file in allFiles) {
        // Nearest ancestor-or-self that appears in include or exclude.
        final chain = [...(file['ancestor_ids']! as List<Object?>).cast<int>(), file['id']! as int];
        bool? selected;
        for (final id in chain.reversed) {
          if (include.contains(id)) {
            selected = true;
            break;
          }
          if (exclude.contains(id)) {
            selected = false;
            break;
          }
        }
        if (selected != true) continue;
        if (!includeDeleted && file['deleted'] == true) continue;
        if (filter != null && !filter(file)) continue;
        final rating = file['rating']! as String;
        counts['files'] = counts['files']! + 1;
        counts[rating] = counts[rating]! + 1;
        if (rating != 'unsupported' && !(rating == 'damaged' && skipDamaged)) {
          counts['bytes'] = counts['bytes']! + (file['size']! as int);
        }
      }
      return counts;
    }

    Map<String, Object?> summary(List<int> include, List<int> exclude, [Map<String, Object?> extra = const {}]) =>
        h.ok('selection_summary', {'epoch': h.epoch, 'include': include, 'exclude': exclude, ...extra});

    void expectSame(Map<String, Object?> actual, Map<String, int> expected) {
      for (final key in expected.keys) {
        expect(actual[key], expected[key], reason: key);
      }
      expect((actual['good']! as int) + (actual['uncertain']! as int) + (actual['damaged']! as int) + (actual['unsupported']! as int),
          actual['files'], reason: 'the four rating counts add up to files');
    }

    test('the nearest marked ancestor decides, at any nesting depth', () {
      final lm = h.landmarks;
      final docs = h.children(lm.documents, {'show_deleted': true});
      final subFolder = docs.firstWhere((r) => r['kind'] == 'dir')['id']! as int;
      final deepFile = h.search({'under': subFolder, 'show_deleted': true}).first['id']! as int;
      final cases = <(List<int>, List<int>)>[
        ([h.root], []),
        ([h.root], [lm.documents]),
        ([h.root], [lm.documents, lm.hugeFolder]),
        ([h.root, subFolder], [lm.documents]),
        ([h.root, deepFile], [lm.documents, subFolder]),
        ([lm.documents], [subFolder]),
        ([deepFile], []),
        ([subFolder], [deepFile]),
        ([lm.flagSamples, lm.previewSamples], []),
        ([h.engine.catalog!.tree.instances[1].root, lm.documents], []),
      ];
      for (final (include, exclude) in cases) {
        expectSame(summary(include, exclude), bruteForce(include, exclude));
      }
    });

    test('include_deleted, skip_damaged and filter apply to every selected file', () {
      final include = [h.root];
      final exclude = [h.landmarks.hugeFolder];
      expectSame(summary(include, exclude, {'include_deleted': true}), bruteForce(include, exclude, includeDeleted: true));
      expectSame(summary(include, exclude, {'skip_damaged': true}), bruteForce(include, exclude, skipDamaged: true));
      expectSame(
        summary(include, exclude, {'include_deleted': true, 'skip_damaged': true, 'filter': {'exts': ['jpg'], 'min_size': 1000}}),
        bruteForce(include, exclude, includeDeleted: true, skipDamaged: true, filter: (f) => f['ext'] == 'jpg' && (f['size']! as int) >= 1000),
      );
      final withDeleted = summary(include, exclude, {'include_deleted': true});
      final without = summary(include, exclude);
      expect(withDeleted['files']! as int, greaterThan(without['files']! as int));
    });

    test('dirs counts the folders that will be created', () {
      final lm = h.landmarks;
      // A flat folder: itself.
      expect(summary([lm.flagSamples], [])['dirs'], 1);
      // One file deep in the tree: its ancestors below the root.
      final file = h.search({'under': lm.documents}).first;
      expect(summary([file['id']! as int], [])['dirs'], (file['ancestor_ids']! as List).length - 1);
      // A selected folder counts even when it is empty or nothing in it matches.
      expect(summary([lm.deletedOnlyFolder], [])['dirs'], 1);
      expect(summary([lm.deletedOnlyFolder], [])['files'], 0);
      // Files of another file system go below a folder named after it.
      final strays = h.engine.catalog!.tree.instances[4].root;
      expect(summary([strays], [])['dirs'], 1);
      expect(summary([], [])['dirs'], 0);
      expect(summary([], [])['files'], 0);
    });
  });

  group('node_info', () {
    test('ancestors, path, record, data and flag details', () {
      final h = Harness()..scan();
      final file = h.search({'under': h.landmarks.documents}).firstWhere((r) => (r['flags']! as List).isNotEmpty);
      final info = h.ok('node_info', {'epoch': h.epoch, 'node': file['id']});
      final row = info['row']! as Map<String, Object?>;
      expect(row['id'], file['id']);
      expect(row['name'], file['name']);
      final ancestors = (info['ancestors']! as List<Object?>).cast<Map<String, Object?>>();
      expect(ancestors.first, {'id': h.root, 'name': ''});
      expect(info['path'], [...ancestors.skip(1).map((a) => a['name']), file['name']].join('/'));
      expect(info['instance'], 0);
      expect((info['record']! as Map).keys.toSet(), {'number', 'seq', 'disk_offset', 'extension_records'});
      expect((info['data']! as Map)['real_size'], file['size']);
      expect((info['flag_details']! as List<Object?>).map((d) => (d! as Map)['flag']), file['flags']);
      expect((info['flag_details']! as List<Object?>).every((d) => ((d! as Map)['text']! as String).length > 10), isTrue);

      final folder = h.ok('node_info', {'epoch': h.epoch, 'node': h.landmarks.documents});
      expect(folder['data'], isNull);
      expect(folder['record'], isNotNull);
      final virtual = h.ok('node_info', {'epoch': h.epoch, 'node': h.engine.catalog!.tree.instances.first.lost});
      expect(virtual['record'], isNull);
      expect(virtual['data'], isNull);
      final older = h.engine.catalog!.tree.instances[1];
      final olderFile = h.search({'under': older.root}).first;
      final olderInfo = h.ok('node_info', {'epoch': h.epoch, 'node': olderFile['id']});
      expect(olderInfo['instance'], 1);
      expect(olderInfo['path'], startsWith('[Older file system]/'));
    });
  });

  group('read_file', () {
    late Harness h;
    setUp(() => h = Harness()..scan());

    Uint8List read(int node, {int offset = 0, int length = 65536}) =>
        base64.decode(h.ok('read_file', {'epoch': h.epoch, 'node': node, 'offset': offset, 'length': length})['data_b64']! as String);

    test('size, eof and kind', () {
      final text = h.ok('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleText, 'offset': 0, 'length': 100});
      expect(base64.decode(text['data_b64']! as String), hasLength(100));
      expect(text['size'], 6400);
      expect(text['eof'], isFalse);
      expect(text['kind'], isNull, reason: 'plain text has no signature');
      final tail = h.ok('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleText, 'offset': 6300, 'length': 1000});
      expect(base64.decode(tail['data_b64']! as String), hasLength(100));
      expect(tail['eof'], isTrue);
      final beyond = h.ok('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleText, 'offset': 7000, 'length': 10});
      expect(beyond['data_b64'], '');
      expect(beyond['eof'], isTrue);
      final empty = h.ok('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleEmpty});
      expect(empty['data_b64'], '');
      expect(empty['size'], 0);
      expect(empty['eof'], isTrue);

      final jpeg = h.ok('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleJpeg});
      expect(jpeg['kind'], 'jpeg');
      expect(jpeg['eof'], isTrue);
      final middle = h.ok('read_file', {'epoch': h.epoch, 'node': h.landmarks.sampleJpeg, 'offset': 10, 'length': 10});
      expect(middle['kind'], isNull, reason: 'the kind is only sniffed at offset 0');
    });

    test('any range of a file equals that range of the whole', () {
      for (final node in [h.landmarks.sampleText, h.landmarks.sampleBinary, h.landmarks.samplePng, h.landmarks.sampleTruncatedJpeg]) {
        final whole = read(node, length: 20000);
        for (final (offset, length) in [(0, 1), (1, 63), (63, 2), (64, 64), (100, 999), (4095, 4098), (7777, 11111)]) {
          if (offset >= whole.length) continue;
          final end = offset + length > whole.length ? whole.length : offset + length;
          expect(read(node, offset: offset, length: length), whole.sublist(offset, end), reason: 'node $node $offset+$length');
        }
      }
    });

    test('the sample pictures are real pictures of the announced size', () {
      final jpeg = read(h.landmarks.sampleJpeg);
      expect(jpeg, hasLength(jpegSampleSize));
      expect(jpeg.sublist(0, 3), [0xFF, 0xD8, 0xFF]);
      expect(jpeg.sublist(jpeg.length - 2), [0xFF, 0xD9]);

      final bmp = read(h.landmarks.sampleBmp);
      expect(bmp, hasLength(bmpSampleSize));
      final header = ByteData.sublistView(bmp);
      expect(String.fromCharCodes(bmp.sublist(0, 2)), 'BM');
      expect(header.getUint32(2, Endian.little), bmpSampleSize);
      expect(header.getUint32(10, Endian.little), 54);
      expect(header.getInt32(18, Endian.little), 64);
      expect(header.getInt32(22, Endian.little), 48);
      expect(header.getUint16(28, Endian.little), 24);

      final png = read(h.landmarks.samplePng);
      expect(png, hasLength(pngSampleSize));
      expect(png.sublist(0, 8), [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
      // Walk the chunks: IHDR, IDAT, IEND, and inflate the image data.
      final view = ByteData.sublistView(png);
      final chunks = <String, Uint8List>{};
      for (var at = 8; at < png.length;) {
        final length = view.getUint32(at);
        chunks[ascii.decode(png.sublist(at + 4, at + 8))] = png.sublist(at + 8, at + 8 + length);
        at += 12 + length;
      }
      expect(chunks.keys, ['IHDR', 'IDAT', 'IEND']);
      expect(ByteData.sublistView(chunks['IHDR']!).getUint32(0), 64);
      expect(ByteData.sublistView(chunks['IHDR']!).getUint32(4), 48);
      expect(zlib.decode(chunks['IDAT']!), hasLength(48 * (1 + 64 * 3)), reason: 'a valid zlib stream with the right checksum');

      // The sizes in the listing are the sizes of the content.
      final rows = h.children(h.landmarks.previewSamples);
      expect(rows.firstWhere((r) => r['name'] == 'picture.jpg')['size'], jpegSampleSize);
      expect(rows.firstWhere((r) => r['name'] == 'picture.png')['size'], pngSampleSize);
      expect(rows.firstWhere((r) => r['name'] == 'picture.bmp')['size'], bmpSampleSize);
    });
  });

  group('jobs', () {
    test('scan: phases in order, counters grow, the catalog arrives with scan_done', () {
      final h = Harness()..open();
      final start = h.ok('scan_start', {'start': 1048576, 'end': 4000786153472, 'session_path': r'C:\s\a.hdsc'});
      expect(start, {'job': 'scan', 'bytes_total': 4000785104896});
      expect(h.engine.runningJob, 'scan');
      h.finishJob();
      final progress = h.events('scan_progress');
      final phases = progress.map((p) => p['phase']! as String).toList();
      expect(phases.toSet(), {'mft_probe', 'sweep', 'analyzing'});
      expect(phases, [...phases.where((p) => p == 'mft_probe'), ...phases.where((p) => p == 'sweep'), ...phases.where((p) => p == 'analyzing')]);
      final reading = progress.where((p) => p['phase'] != 'analyzing').toList();
      for (var i = 1; i < reading.length; i++) {
        expect(reading[i]['bytes_done']! as int, greaterThan(reading[i - 1]['bytes_done']! as int));
        expect(reading[i]['records']! as int, greaterThanOrEqualTo(reading[i - 1]['records']! as int));
      }
      expect(reading.last['bytes_done'], 4000785104896);
      expect(reading.every((p) => (p['bytes_done']! as int) % 512 == 0 && p['bytes_total'] == 4000785104896), isTrue);
      expect(progress.where((p) => p['phase'] == 'analyzing').every((p) => p['eta_seconds'] == null), isTrue);
      final done = h.events('scan_done').single;
      expect(done['status'], 'completed');
      expect(done['error'], isNull);
      expect(done['session_path'], r'C:\s\a.hdsc');
      expect((done['catalog']! as Map)['scan_complete'], isTrue);
      expect(h.engine.world.savedSessions, contains(r'C:\s\a.hdsc'));
    });

    test('scan: stop keeps what was read; resume completes; resuming a complete session just emits scan_done', () {
      final h = Harness()..open();
      h.ok('scan_start', {'start': 1048576, 'end': 4000786153472});
      h.engine.advance(4);
      h.ok('job_cancel');
      h.finishJob();
      final stopped = h.events('scan_done').single;
      expect(stopped['status'], 'stopped');
      final partial = stopped['catalog']! as Map<String, Object?>;
      expect(partial['scan_complete'], isFalse);
      expect(partial['scanned_bytes']! as int, lessThan(4000785104896));
      expect((partial['warnings']! as List<Object?>).map((w) => (w! as Map)['code']), contains('scan_incomplete'));
      final partialNodes = partial['nodes']! as int;

      h.lines.clear();
      expect(h.outcome('scan_start', {'resume': true, 'start': 0}), 'bad_request', reason: 'the range must be the session\'s');
      h.ok('scan_start', {'resume': true, 'start': 1048576, 'end': 4000786153472});
      h.finishJob();
      expect((h.events('scan_progress').first['bytes_done']! as int), greaterThan(partial['scanned_bytes']! as int),
          reason: 'a resumed scan starts where it stopped');
      final complete = h.events('scan_done').single['catalog']! as Map<String, Object?>;
      expect(complete['scan_complete'], isTrue);
      expect(complete['nodes']! as int, greaterThan(partialNodes));
      expect(complete['epoch'], isNot(partial['epoch']));

      h.lines.clear();
      h.ok('scan_start', {'resume': true});
      h.engine.advance();
      expect(h.events('scan_progress'), isEmpty);
      expect(h.events('scan_done').single['status'], 'completed');
    });

    test('scan: job_cancel is ignored while analyzing', () {
      final h = Harness()..open();
      h.ok('scan_start');
      while (h.events('scan_progress').isEmpty || h.events('scan_progress').last['phase'] != 'analyzing') {
        h.engine.advance();
      }
      h.ok('job_cancel');
      h.finishJob();
      expect(h.events('scan_done').single['status'], 'completed');
    });

    test('scan: source_lost fails the job, keeps the session, and needs the source attached again', () {
      final h = Harness()..open();
      h.engine.script.scanFailAtStep = 3;
      h.ok('scan_start', {'session_path': r'C:\s\lost.hdsc'});
      h.finishJob();
      final done = h.events('scan_done').single;
      expect(done['status'], 'failed');
      expect((done['error']! as Map)['code'], 'source_lost');
      expect(done['catalog'], isNull);
      expect(done['session_path'], r'C:\s\lost.hdsc');
      expect(h.outcome('catalog_info'), 'no_catalog');
      expect(h.outcome('scan_start', {'resume': true}), 'no_source');
      expect(h.outcome('session_attach_source', {'kind': 'disk', 'path': disk1}), 'ok');
      h.lines.clear();
      h.ok('scan_start', {'resume': true});
      h.finishJob();
      expect(h.events('scan_done').single['status'], 'completed');
    });

    test('at most one job at a time', () {
      final h = Harness()..scan();
      h.ok('carve_start');
      expect(h.outcome('scan_start'), 'busy');
      expect(h.outcome('image_start', {'dest': r'D:\x.img'}), 'busy');
      expect(h.outcome('carve_start'), 'busy');
      expect(h.outcome('recover_start', {'epoch': h.epoch, 'include': [h.root], 'exclude': <int>[], 'dest': r'D:\R'}), 'busy');
      // Queries are answered while a job runs.
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': h.root}), 'ok');
      expect(h.outcome('job_cancel'), 'ok');
      h.finishJob();
      expect(h.outcome('job_cancel'), 'ok', reason: 'no job: still {}');
    });

    test('recover: the numbers of selection_summary, progress, and counts that add up', () {
      final h = Harness()..scan();
      final selection = {'epoch': h.epoch, 'include': [h.root], 'exclude': [h.landmarks.hugeFolder]};
      final summary = h.ok('selection_summary', selection);
      final start = h.ok('recover_start', {...selection, 'dest': r'D:\Rescued'});
      expect(start, {'job': 'recover', 'files': summary['files'], 'bytes': summary['bytes']});
      h.finishJob();
      final progress = h.events('recover_progress');
      expect(progress, isNotEmpty);
      for (var i = 1; i < progress.length; i++) {
        expect(progress[i]['files_done']! as int, greaterThanOrEqualTo(progress[i - 1]['files_done']! as int));
      }
      expect(progress.every((p) => p['files_total'] == summary['files'] && p['bytes_total'] == summary['bytes']), isTrue);
      final done = h.events('recover_done').single;
      expect(done['status'], 'completed');
      expect(done['files_total'], summary['files']);
      expect((done['ok']! as int) + (done['warnings']! as int) + (done['failed']! as int) + (done['skipped']! as int), done['files_total']);
      expect(done['skipped'], summary['unsupported'], reason: 'unsupported files are never written');
      expect(done['dest'], r'D:\Rescued');
      expect(done['report_path'], matches(r'^D:\\Rescued\\HDSavior-report-\d{8}-\d{6}\.csv$'));

      // skip_damaged moves the damaged files from warnings to skipped.
      h.lines.clear();
      h.ok('recover_start', {...selection, 'dest': r'D:\Rescued', 'skip_damaged': true});
      h.finishJob();
      expect(h.events('recover_done').single['skipped'], (summary['unsupported']! as int) + (summary['damaged']! as int));
    });

    test('recover: cancel and failures', () {
      final h = Harness()..scan();
      final args = {'epoch': h.epoch, 'include': [h.root], 'exclude': <int>[], 'dest': '/mnt/rescue'};
      h.ok('recover_start', args);
      h.engine.advance(2);
      h.ok('job_cancel');
      h.finishJob();
      final cancelled = h.events('recover_done').single;
      expect(cancelled['status'], 'cancelled');
      expect((cancelled['ok']! as int) + (cancelled['warnings']! as int) + (cancelled['failed']! as int) + (cancelled['skipped']! as int),
          lessThan(cancelled['files_total']! as int));
      expect(cancelled['report_path'], startsWith('/mnt/rescue/HDSavior-report-'));

      h.lines.clear();
      h.engine.script.recoverFailAtStep = 2;
      h.ok('recover_start', args);
      h.finishJob();
      expect(h.events('recover_done').single['status'], 'failed');
      expect((h.events('recover_done').single['error']! as Map)['code'], 'no_space');
    });

    test('carve: carved nodes appear under [Carved files]/<ext>/f<sector>.<ext>', () {
      final h = Harness()..scan();
      final start = h.ok('carve_start', {'kinds': ['jpeg', 'pdf', 'sevenzip']});
      expect(start['job'], 'carve');
      expect(start['candidates'], (h.ok('catalog_info')['carve_hits']));
      h.finishJob();
      expect(h.events('carve_progress'), isNotEmpty);
      final done = h.events('carve_done').single;
      expect(done['status'], 'completed');
      final catalog = done['catalog']! as Map<String, Object?>;
      expect(catalog['carved'], done['found']);
      expect(done['found']! as int, greaterThan(0));
      final carvedRoot = (((catalog['instances']! as List<Object?>).first! as Map)['virtual']! as Map)['carved']! as int;
      expect(h.children(h.root).map((r) => r['name']), contains('[Carved files]'));
      final folders = h.children(carvedRoot);
      expect(folders.map((r) => r['name']).toSet(), {'jpg', 'pdf', '7z'});
      var files = 0;
      var bytes = 0;
      for (final folder in folders) {
        for (final file in h.children(folder['id']! as int)) {
          expect(file['kind'], 'carved');
          expect(file['name'], matches('^f\\d{10}\\.${folder['name']}\$'));
          expect(['good', 'uncertain'], contains(file['rating']));
          files++;
          bytes += file['size']! as int;
        }
      }
      expect(files, done['found']);
      expect(bytes, done['bytes_found']);
      // Carved files are found by search and counted by the root folder.
      expect(h.search({'under': carvedRoot}), hasLength(files));
      expect((h.ok('node_info', {'epoch': h.epoch, 'node': carvedRoot})['row']! as Map)['files'], files);
    });

    test('carve: results found before a cancel are kept', () {
      final h = Harness()..scan();
      h.ok('carve_start');
      h.engine.advance(3);
      h.ok('job_cancel');
      h.finishJob();
      final done = h.events('carve_done').single;
      expect(done['status'], 'cancelled');
      expect(done['found']! as int, greaterThan(0));
      expect((done['catalog']! as Map)['carved'], done['found']);
    });

    test('image: progress, done, bad sectors, cancel', () {
      final h = Harness()..open();
      h.engine.script.imageBadBytes = 4096;
      final start = h.ok('image_start', {'start': 1048576, 'end': 1048576 + 1073741824, 'dest': r'D:\img\part.img'});
      expect(start, {'job': 'image', 'bytes': 1073741824});
      h.finishJob();
      final progress = h.events('image_progress');
      expect(progress.last['bytes_done'], 1073741824);
      expect(progress.every((p) => p['bytes_total'] == 1073741824), isTrue);
      final done = h.events('image_done').single;
      expect(done, {
        'status': 'completed',
        'error': null,
        'bytes_written': 1073741824,
        'bad_bytes': 4096,
        'path': r'D:\img\part.img',
        'bad_map_path': r'D:\img\part.img.bad.txt',
      });

      h.lines.clear();
      h.engine.script.imageBadBytes = 0;
      h.ok('image_start', {'dest': r'D:\img\whole.img'});
      h.engine.advance(2);
      h.ok('job_cancel');
      h.finishJob();
      final cancelled = h.events('image_done').single;
      expect(cancelled['status'], 'cancelled');
      expect(cancelled['bad_map_path'], isNull);
      expect(cancelled['bytes_written']! as int, lessThan(4000787030016));
    });
  });

  group('sessions', () {
    test('session_load by name', () {
      final h = Harness();
      expect(h.outcome('session_load', {'path': r'C:\s\missing.hdsc'}), 'not_found');
      expect(h.outcome('session_load', {'path': r'C:\s\corrupt.hdsc'}), 'io_error');
      expect(h.outcome('session_load', {'path': r'C:\s\newer.hdsc'}), 'unsupported');

      final attached = h.ok('session_load', {'path': r'C:\s\holiday.hdsc'});
      expect(attached['source_attached'], isTrue);
      expect(attached['source_error'], isNull);
      expect((attached['source']! as Map)['path'], disk1);
      expect((attached['catalog']! as Map)['source_attached'], isTrue);

      final detached = h.ok('session_load', {'path': r'C:\s\detached.hdsc'});
      expect(detached['source_attached'], isFalse);
      expect(detached['source_error'], isNotNull);
      expect((detached['catalog']! as Map)['source_attached'], isFalse);
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': 0}), 'ok', reason: 'the tree can be browsed');
      expect(h.outcome('carve_start'), 'no_source');
      // Attaching makes it whole again.
      final reattached = h.ok('session_attach_source', {'kind': 'disk', 'path': disk1});
      expect((reattached['catalog']! as Map)['source_attached'], isTrue);
      expect(h.outcome('carve_start'), 'ok');
      h.finishJob();

      final differs = h.ok('session_load', {'path': r'C:\s\identity.hdsc'});
      expect(((differs['catalog']! as Map)['warnings']! as List<Object?>).map((w) => (w! as Map)['code']), contains('identity_differs'));
      final partial = h.ok('session_load', {'path': r'C:\s\partial.hdsc'});
      expect((partial['catalog']! as Map)['scan_complete'], isFalse);

      // A source to open instead of the recorded one.
      final other = h.ok('session_load', {'path': r'C:\s\detached.hdsc', 'source': {'kind': 'image', 'path': r'D:\copy.img'}});
      expect(other['source_attached'], isTrue);
      expect((other['source']! as Map)['path'], r'D:\copy.img');
      expect(h.outcome('session_load', {'path': r'C:\s\x.hdsc', 'source': {'kind': 'image', 'path': r'D:\mismatch.img'}}), 'source_mismatch');
    });

    test('a saved session is found by the next engine', () {
      final world = FakeWorld();
      final first = Harness(world: world)..scan(sessionPath: r'C:\s\mine.hdsc');
      first.ok('session_save', {'path': r'C:\s\copy.hdsc'});
      final second = Harness(world: world);
      final loaded = second.ok('session_load', {'path': r'C:\s\copy.hdsc'});
      expect(((loaded['catalog']! as Map)['range']! as Map), {'start': 1048576, 'end': 4000786153472});
      expect((loaded['catalog']! as Map)['scan_complete'], isTrue);
    });
  });

  group('file systems: instance_set', () {
    List<Map<String, Object?>> instancesOf(Map<String, Object?> catalog) =>
        (catalog['instances']! as List<Object?>).cast<Map<String, Object?>>();

    /// A file of instance [root]'s tree that has only the flags of its
    /// instance (so that its rating follows from them alone).
    Map<String, Object?> plainFile(Harness h, int root, String instanceFlag) => h
        .search({'under': root})
        .firstWhere((r) => (r['flags']! as List<Object?>).every((f) => f == instanceFlag));

    test('"main": the chosen file system comes first, the previous main one becomes "other"', () {
      final h = Harness();
      final before = h.scan();
      final older = instancesOf(before)[1];
      expect(older['kind'], 'older');
      expect(older['manual'], isFalse);
      final olderRoot = older['root']! as int;
      final oldFile = plainFile(h, olderRoot, 'older_generation');
      expect(oldFile['rating'], 'uncertain');
      h.ok('carve_start', {'kinds': ['jpeg']});
      h.finishJob();
      final carvedRoot = h.engine.catalog!.carvedRoot!;
      final epoch = h.epoch;

      final after = h.ok('instance_set', {'instance': older['id'], 'kind': 'main'});
      expect(after['epoch'], isNot(epoch), reason: 'instance_set starts a new epoch');
      expect(h.outcome('node_info', {'epoch': epoch, 'node': olderRoot}), 'stale_catalog');
      final instances = instancesOf(after);
      expect(instances.first['root'], olderRoot, reason: 'instances[0] is the main instance');
      expect(instances.first['kind'], 'main');
      expect(instances.first['manual'], isTrue);
      expect(instances.first['label'], '');
      // Ids are positions in this list: they changed with the new epoch.
      expect(instances.map((i) => i['id']), [0, 1, 2, 3, 4]);
      final demoted = instances.firstWhere((i) => i['root'] == 0);
      expect(demoted['kind'], 'other');
      expect(demoted['manual'], isFalse, reason: 'the user chose the other one');
      expect(demoted['label'], isNotEmpty);
      expect(instances.where((i) => i['kind'] == 'main'), hasLength(1));

      // Roots carry their new names; the default subtree of search is the new main root.
      final roots = {for (final i in instances) i['root']! as int: i['label']};
      for (final MapEntry(key: root, value: label) in roots.entries) {
        expect((h.ok('node_info', {'epoch': h.epoch, 'node': root})['row']! as Map)['name'], label);
      }
      expect(h.root, olderRoot);
      final found = h.search({'filter': {'query': ''}});
      final first = h.ok('node_info', {'epoch': h.epoch, 'node': found.first['id']});
      expect((first['ancestors']! as List<Object?>).cast<Map<String, Object?>>().first['id'], olderRoot);

      // Its files are no longer of an older generation, and carved files are gone.
      final now = h.ok('node_info', {'epoch': h.epoch, 'node': oldFile['id']})['row']! as Map<String, Object?>;
      expect(now['flags'], isEmpty);
      expect(now['rating'], 'good');
      expect(after['carved'], 0);
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': carvedRoot}), 'not_found');

      // The id of a node's instance is the one of this catalog.
      expect(h.ok('node_info', {'epoch': h.epoch, 'node': oldFile['id']})['instance'], 0);
      expect(h.ok('node_info', {'epoch': h.epoch, 'node': demoted['root']})['instance'], demoted['id']);

      // "auto" withdraws the override: the engine's verdict again.
      final auto = h.ok('instance_set', {'instance': 0, 'kind': 'auto'});
      expect(instancesOf(auto).map((i) => (i['id'], i['kind'], i['manual'])), [
        for (final i in instancesOf(before)) (i['id'], i['kind'], false),
      ]);
      expect(instancesOf(auto).map((i) => i['label']), instancesOf(before).map((i) => i['label']));
      expect(h.root, 0);
    });

    test('"other" makes an embedded file system a volume of its own whose files can be read', () {
      final h = Harness();
      final before = h.scan();
      final embedded = instancesOf(before).firstWhere((i) => i['kind'] == 'embedded');
      expect(embedded['embedded_in'], isNotNull);
      final file = plainFile(h, embedded['root']! as int, 'embedded');
      expect(file['rating'], 'unsupported');
      expect(h.outcome('read_file', {'epoch': h.epoch, 'node': file['id']}), 'unsupported');

      final after = h.ok('instance_set', {'instance': embedded['id'], 'kind': 'other'});
      final changed = instancesOf(after).firstWhere((i) => i['root'] == embedded['root']);
      expect(changed['kind'], 'other');
      expect(changed['manual'], isTrue);
      expect(changed['embedded_in'], isNull);
      expect(instancesOf(after).first['root'], 0, reason: 'the main instance stays');
      final row = h.ok('node_info', {'epoch': h.epoch, 'node': file['id']})['row']! as Map<String, Object?>;
      expect(row['flags'], isEmpty);
      expect(row['rating'], isNot('unsupported'));
      expect(h.outcome('read_file', {'epoch': h.epoch, 'node': file['id']}), 'ok');
      // It is read with its own geometry now, not with the main one's.
      expect((changed['geometry']! as Map)['offset_source'], isNot('inherited'));
    });

    test('what cannot be changed', () {
      final h = Harness();
      final instances = instancesOf(h.scan());
      final strays = instances.firstWhere((i) => i['kind'] == 'strays')['id'];
      final epoch = h.epoch;
      expect(h.outcome('instance_set', {'instance': strays, 'kind': 'main'}), 'bad_request');
      expect(h.epoch, epoch, reason: 'a refused change changes nothing');
      expect(h.outcome('instance_set', {'instance': strays, 'kind': 'other'}), 'bad_request');
      expect(h.outcome('instance_set', {'instance': 0, 'kind': 'other'}), 'bad_request',
          reason: 'choose another main file system instead');
      final other = instances.firstWhere((i) => i['kind'] == 'other')['id'];
      expect(h.outcome('instance_set', {'instance': other, 'kind': 'other'}), 'bad_request',
          reason: '"other" is for embedded and older file systems');
      // The previous main one cannot be pushed aside for good either: there
      // is always a main file system.
      h.ok('instance_set', {'instance': 1, 'kind': 'main'});
      final demoted = instancesOf(h.ok('catalog_info')).firstWhere((i) => i['root'] == 0)['id'];
      expect(h.outcome('instance_set', {'instance': demoted, 'kind': 'other'}), 'bad_request');
      h.ok('instance_set', {'instance': 0, 'kind': 'auto'});
      expect(instancesOf(h.ok('catalog_info')).first['root'], 0);
      expect(h.outcome('instance_set', {'instance': 1, 'kind': 'embedded'}), 'bad_request');
      expect(h.outcome('instance_set', {'instance': 99, 'kind': 'main'}), 'not_found');
      expect(h.outcome('instance_set', {'kind': 'main'}), 'bad_request');
    });
  });

  group('protected ranges and the destination guard', () {
    List<Map<String, Object?>> protectedOf(Map<String, Object?> catalog) =>
        (catalog['protected']! as List<Object?>).cast<Map<String, Object?>>();
    Map<String, Object?> check(Harness h, String path, [String purpose = 'recover']) =>
        h.ok('check_destination', {'path': path, 'purpose': purpose});
    List<String> warnings(Map<String, Object?> catalog) => [
      for (final w in (catalog['warnings']! as List<Object?>).cast<Map<String, Object?>>()) w['code']! as String,
    ];

    test('the session range first, then the lost volume of every file system of its own with validated geometry', () {
      final h = Harness();
      final catalog = h.scan();
      final protected = protectedOf(catalog);
      expect(protected.first, {'start': 1048576, 'end': 4000786153472, 'reason': 'session_range'});
      final instances = (catalog['instances']! as List<Object?>).cast<Map<String, Object?>>();
      final own = instances.where((i) => (i['kind'] == 'main' || i['kind'] == 'other') && (i['geometry']! as Map)['validated'] == true);
      expect(protected.skip(1), [
        for (final instance in own)
          {
            'start': (instance['geometry']! as Map)['volume_offset'],
            'end': ((instance['geometry']! as Map)['volume_offset']! as int) +
                ((instance['geometry']! as Map)['total_clusters']! as int) * ((instance['geometry']! as Map)['cluster_size']! as int),
            'reason': 'lost_volume',
          },
      ]);
      expect(protected.skip(1), hasLength(2));
      for (final range in protected) {
        expect(range['end']! as int, lessThanOrEqualTo(4000787030016), reason: 'clipped to the source');
      }

      // A geometry that is not validated protects nothing of its own.
      final changed = h.ok('geometry_set', {'instance': 0, 'cluster_size': 8192, 'volume_offset': 1048576});
      expect(protectedOf(changed), hasLength(2));
      expect(protectedOf(changed).first['reason'], 'session_range');
      expect(protectedOf(h.ok('catalog_info')), protectedOf(changed));
    });

    test('a lost volume that reaches beyond the scanned partition protects the next partition as well', () {
      final h = Harness();
      final disk = h.ok('open_source', {'kind': 'disk', 'path': r'\\.\PhysicalDrive2'});
      final partitions = (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>();
      final d = partitions.first;
      final start = d['offset']! as int;
      final end = start + (d['size']! as int);

      // Before a scan nothing is known: the fake protects the whole source.
      expect(check(h, r'F:\Rescued')['verdict'], 'overlaps_source');
      // The scan's own check uses its range: F: is outside it, on the same disk.
      final refused = h.error('scan_start', {'start': start, 'end': end, 'session_path': r'F:\s\d.hdsc'});
      expect((refused['data']! as Map)['verdict'], 'same_disk');
      h.ok('scan_start', {'start': start, 'end': end, 'session_path': r'F:\s\d.hdsc', 'allow_same_disk': true});
      h.finishJob();
      final catalog = h.events('scan_done').last['catalog']! as Map<String, Object?>;
      final lost = protectedOf(catalog).firstWhere((r) => r['reason'] == 'lost_volume');
      expect(lost['end']! as int, greaterThan(end), reason: 'the old volume was larger than the partition');

      final f = check(h, r'F:\Rescued');
      expect(f['verdict'], 'overlaps_source');
      expect(f['reason'], 'lost_volume');
      expect(f['lost_files']! as int, greaterThan(0));
      expect(f['lost_bytes']! as int, greaterThan(0));
      final inside = check(h, r'D:\Rescued');
      expect(inside['reason'], 'session_range');
      expect(inside['lost_files']! as int, greaterThan(f['lost_files']! as int));
      final g = check(h, r'G:\Rescued');
      expect(g['verdict'], 'same_disk');
      expect(g['reason'], isNull);
      expect(g['lost_files'], 0);
      expect(check(h, r'X:\Rescued'), containsPair('lost_files', 0));

      // The session file turned out to be inside the lost volume (B3).
      expect(warnings(catalog), contains('session_file_at_risk'));
      expect(h.outcome('session_save', {'path': r'G:\s\safe.hdsc'}), 'destination_refused');
      h.ok('session_save', {'path': r'X:\s\safe.hdsc'});
      expect(warnings(h.ok('catalog_info')), isNot(contains('session_file_at_risk')));
    });

    test('L: is inside the lost volume once a catalog knows it; before, nobody can tell', () {
      final h = Harness()..open();
      final before = check(h, r'L:\Rescued');
      expect(before['verdict'], 'same_disk');
      expect(before['message'], contains('nobody knows'));
      h.scan();
      final after = check(h, r'L:\Rescued');
      expect(after['verdict'], 'overlaps_source');
      expect(after['reason'], 'lost_volume');
      expect(after['lost_files']! as int, greaterThan(0));
      expect(after['message'], isNotEmpty);
    });

    test('the image file that is the source', () {
      final h = Harness();
      h.ok('open_source', {'kind': 'image', 'path': r'D:\images\Disk.img'});
      for (final path in [r'D:\images\Disk.img', r'd:\IMAGES\disk.IMG', 'D:/images/Disk.img']) {
        final same = check(h, path, 'image');
        expect(same['verdict'], 'overlaps_source', reason: path);
        expect(same['reason'], 'source_image');
      }
      expect(check(h, r'D:\images\copy.img', 'image')['verdict'], 'ok');
      expect(check(h, r'D:\images\copy.img', 'image')['reason'], isNull);
    });

    test('session and app data on the system volume need no consent, unless it is protected (S3)', () {
      final h = Harness();
      final disk = h.ok('open_source', {'kind': 'disk', 'path': r'\\.\PhysicalDrive0'});
      final recovery = (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>().last;
      final start = recovery['offset']! as int;
      h.ok('scan_start', {'start': start, 'end': start + (recovery['size']! as int)});
      h.finishJob();
      expect(check(h, r'C:\Users\me\AppData\Local\HDSavior', 'app_data')['verdict'], 'ok');
      expect(check(h, r'C:\Users\me\s.hdsc', 'session')['verdict'], 'ok');
      expect(check(h, r'C:\Rescued')['verdict'], 'same_disk');

      // The partition that holds C: itself.
      final windows = (disk['partitions']! as List<Object?>).cast<Map<String, Object?>>()[2];
      final from = windows['offset']! as int;
      h.ok('scan_start', {'start': from, 'end': from + 1073741824});
      h.finishJob();
      expect(check(h, r'C:\Users\me\AppData\Local\HDSavior', 'app_data')['verdict'], 'overlaps_source');
    });

    test('mixed_generations when two file systems could not be separated cleanly', () {
      final h = Harness();
      final loaded = h.ok('session_load', {'path': r'C:\s\mixed.hdsc'});
      expect(warnings(loaded['catalog']! as Map<String, Object?>), contains('mixed_generations'));
      final plain = h.ok('session_load', {'path': r'C:\s\plain.hdsc'});
      expect(warnings(plain['catalog']! as Map<String, Object?>), isNot(contains('mixed_generations')));

      h.ok('open_source', {'kind': 'image', 'path': r'D:\mixed-history.img'});
      h.ok('scan_start');
      h.finishJob();
      expect(warnings(h.events('scan_done').last['catalog']! as Map<String, Object?>), contains('mixed_generations'));
    });
  });

  group('failure injection', () {
    test('failNext answers with the given error, the given number of times', () {
      final h = Harness()..scan();
      h.engine.failNext('list_children', 'io_error', message: 'read failed', data: {'sector': 7}, times: 2);
      final first = h.error('list_children', {'epoch': h.epoch, 'node': h.root});
      expect(first, {'code': 'io_error', 'message': 'read failed', 'data': {'sector': 7}});
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': h.root}), 'io_error');
      expect(h.outcome('list_children', {'epoch': h.epoch, 'node': h.root}), 'ok');
    });

    test('exitOn kills the engine in the middle of a request', () async {
      final h = Harness()..scan();
      h.engine.exitOn('node_info', exitCode: 7, partialOutput: '{"id":');
      final id = h.send('node_info', {'epoch': h.epoch, 'node': h.root});
      expect(h.responseTo(id), isNull);
      expect(await h.engine.exitCode, 7);
      h.send('hello');
      expect(h.lines.where((l) => l['id'] == id + 1), isEmpty, reason: 'a dead engine answers nothing');
    });

    test('fake_control drives the same hooks over the wire', () async {
      final h = Harness()..scan();
      h.ok('fake_control', {
        'fail_next': {'cmd': 'hello', 'code': 'internal', 'message': 'boom'},
      });
      expect(h.error('hello')['message'], 'boom');
      final old = h.epoch;
      h.ok('fake_control', {'bump_epoch': true});
      expect(h.outcome('node_info', {'epoch': old, 'node': 0}), 'stale_catalog');
      h.ok('fake_control', {'exit_on': 'hello', 'exit_code': 9});
      h.send('hello');
      expect(await h.engine.exitCode, 9);
    });
  });
}

/// Deleted files directly in [folder] (hidden without show_deleted).
int _deletedIn(Harness h, int folder) {
  final tree = h.engine.catalog!.tree;
  var count = 0;
  for (var c = tree.firstChild[folder]; c < tree.firstChild[folder] + tree.childCount[folder]; c++) {
    if (tree.isDeleted(c)) count++;
  }
  return count;
}
