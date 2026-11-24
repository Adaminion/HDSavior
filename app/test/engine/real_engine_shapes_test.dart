// The fake engine must speak like the real one. docs/protocol-examples.jsonl
// is recorded from the real engine; here the fake engine has the same
// conversation (the same commands, in the same order, with its own ids) and
// every result, event and error object of both is reduced to its shape: the
// member paths it has and the JSON types they hold. A member one engine sends
// and the other does not, or a member whose types do not overlap, is a
// mismatch between the fake the app's tests run against and the engine the
// app ships with.
//
// With HDSAVIOR_ENGINE and HDSAVIOR_TEST_IMAGE set (see
// engine_conformance_test.dart), the same conversation is also had with the
// real engine binary and compared the same way.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

import '../fake_engine/fake_engine.dart';
import '../fake_engine/fake_transport.dart';
import '../support/replay_transport.dart';
import 'protocol_transcript_test.dart' show transcriptPath, unknownEnumValues;

/// Kind of message -> member path -> JSON types seen there.
typedef Shapes = Map<String, Map<String, Set<String>>>;

String _type(Object? value) => switch (value) {
  null => 'null',
  bool() => 'boolean',
  num() => 'number',
  String() => 'string',
  List<Object?>() => 'array',
  Map<Object?, Object?>() => 'object',
  _ => '${value.runtimeType}',
};

void _shape(Object? value, String path, Map<String, Set<String>> into) {
  (into[path] ??= {}).add(_type(value));
  if (value is Map<String, Object?>) {
    for (final MapEntry(:key, :value) in value.entries) {
      _shape(value, '$path.$key', into);
    }
  } else if (value is List<Object?>) {
    for (final element in value) {
      _shape(element, '$path[]', into);
    }
  }
}

/// The shapes of everything an engine wrote in [transcript]: results by
/// command (`result:<cmd>`), errors (`error`) and events (`event:<name>`).
Shapes shapesOf(Iterable<String> transcript) {
  final shapes = <String, Map<String, Set<String>>>{};
  final commands = <Object?, String>{};
  for (final line in transcript) {
    if (line.trim().isEmpty) continue;
    final message = jsonDecode(line) as Map<String, Object?>;
    if (message['cmd'] case final String command) {
      commands[message['id']] = command;
    } else if (message['event'] case final String name) {
      _shape(message['data'], '', shapes['event:$name'] ??= {});
    } else if (message['ok'] == true) {
      _shape(message['result'] ?? const <String, Object?>{}, '', shapes['result:${commands[message['id']]}'] ??= {});
    } else {
      _shape(message['error'], '', shapes['error'] ??= {});
    }
  }
  return shapes;
}

/// Differences between the shapes [expected] (the real engine's) and
/// [actual]: for every kind of message both have, members only one side
/// sends, and members whose non-null types do not overlap. Kinds of message
/// [expected] has and [actual] lacks are reported as well.
List<String> compareShapes(Shapes expected, Shapes actual, {String expectedName = 'real', String actualName = 'fake'}) {
  final problems = <String>[];
  for (final MapEntry(key: kind, value: want) in expected.entries) {
    final have = actual[kind];
    if (have == null) {
      problems.add('$kind: $actualName never sent it');
      continue;
    }
    // A member below a value the other side only sent as null (or never
    // sent), or an element of an array the other side only sent empty,
    // cannot be compared.
    bool comparable(String path, Map<String, Set<String>> other) {
      if (path.endsWith('[]')) return false;
      for (var i = path.indexOf('.', 1); i > 0; i = path.indexOf('.', i + 1)) {
        if (!(other[path.substring(0, i)]?.contains('object') ?? false)) return false;
      }
      return true;
    }

    for (final MapEntry(key: path, value: types) in want.entries) {
      final other = have[path];
      if (other == null) {
        if (comparable(path, have)) problems.add('$kind$path: only $expectedName sends it (${types.join('|')})');
        continue;
      }
      final a = types.difference({'null'});
      final b = other.difference({'null'});
      if (a.isNotEmpty && b.isNotEmpty && a.intersection(b).isEmpty) {
        problems.add('$kind$path: $expectedName ${a.join('|')}, $actualName ${b.join('|')}');
      }
    }
    for (final path in have.keys) {
      if (!want.containsKey(path) && comparable(path, want)) {
        problems.add('$kind$path: only $actualName sends it (${have[path]!.join('|')})');
      }
    }
  }
  return problems..sort();
}

/// The commands of docs/protocol-examples.jsonl, had with any engine:
/// open [imagePath], scan it, browse, read, search, summarise, recover into
/// [temp], carve, save the session, image the first MiB, close, load the
/// session again and shut down; in addition `geometry_set` (the geometry in
/// effect), a stale request, state B after `close_source` and
/// `session_attach_source`. Returns the transcript.
Future<List<String>> transcriptConversation(EngineTransport transport, String imagePath, Directory temp) async {
  final recording = RecordingTransport(transport);
  final client = EngineClient(recording);
  final api = EngineApi(client);
  final protocolErrors = <String>[];
  client.protocolErrors.listen(protocolErrors.add);
  await client.start();
  try {
    final out = '${temp.path}${Platform.pathSeparator}out';
    final session = '${temp.path}${Platform.pathSeparator}basic.hds';
    await api.hello();
    await api.listDisks();
    await api.openSource(kind: SourceKind.image, path: imagePath);
    await api.checkDestination(path: out, purpose: DestinationPurpose.recover, needBytes: 1 << 20);
    final scanned = client.on<ScanDone>().first;
    await api.scanStart(sessionPath: session);
    final catalog = (await scanned.timeout(const Duration(minutes: 5))).catalog!;
    final info = await api.catalogInfo();
    final epoch = info.epoch;
    final top = await api.listChildren(epoch: epoch, node: catalog.mainInstance!.root, limit: 50);
    final folder = top.rows.firstWhere((r) => r.isFolder && r.kind == NodeKind.dir);
    var rows = (await api.listChildren(epoch: epoch, node: folder.id, limit: 50)).rows;
    while (!rows.any((r) => r.isFile && r.size > 0)) {
      rows = (await api.listChildren(epoch: epoch, node: rows.firstWhere((r) => r.isFolder).id, limit: 50)).rows;
    }
    final file = rows.firstWhere((r) => r.isFile && r.size > 0);
    await api.nodeInfo(epoch: epoch, node: file.id);
    await api.readFile(epoch: epoch, node: file.id, length: 64);
    await api.search(epoch: epoch, filter: const FileFilter(query: 'a'), limit: 5);
    await api.selectionSummary(epoch: epoch, include: [folder.id], exclude: []);
    await api.checkDestination(path: out, purpose: DestinationPurpose.recover);
    final recovered = client.on<RecoverDone>().first;
    await api.recoverStart(epoch: epoch, include: [folder.id], exclude: [], dest: out);
    await recovered.timeout(const Duration(minutes: 5));
    final carved = client.on<CarveDone>().first;
    await api.carveStart();
    final carveDone = await carved.timeout(const Duration(minutes: 5));
    // The geometry in effect, set again: the epoch and the carved files stay.
    final main = carveDone.catalog!.mainInstance!;
    final same = await api.geometrySet(
      epoch: epoch,
      instance: main.id,
      clusterSize: main.geometry.clusterSize,
      volumeOffset: main.geometry.volumeOffset,
    );
    expect(same.epoch, epoch);
    expect(same.carved, carveDone.catalog!.carved);
    await api.sessionSave(path: '${temp.path}${Platform.pathSeparator}copy.hds');
    final imaged = client.on<ImageDone>().first;
    await api.imageStart(start: 0, end: 1 << 20, dest: '${temp.path}${Platform.pathSeparator}head.img');
    await imaged.timeout(const Duration(minutes: 5));
    await api.jobCancel();
    // Errors: every engine answers these the same way.
    await expectLater(api.nodeInfo(epoch: epoch + 1000, node: 0), throwsA(isA<EngineException>()));
    // close_source keeps the session and its catalog (state B).
    await api.closeSource();
    expect((await api.catalogInfo()).sourceAttached, isFalse);
    await expectLater(
      api.checkDestination(path: out, purpose: DestinationPurpose.recover),
      throwsA(isA<EngineException>().having((e) => e.kind, 'kind', EngineErrorCode.noSource)),
    );
    final attached = await api.sessionAttachSource(kind: SourceKind.image, path: imagePath);
    expect(attached.catalog.sourceAttached, isTrue);
    expect(attached.catalog.epoch, isNot(epoch), reason: 'the catalog is rebuilt');
    await api.sessionLoad(path: session, source: SourceRef(kind: SourceKind.image, path: imagePath));
    await client.shutdown();
  } finally {
    await client.dispose();
  }
  expect(protocolErrors, isEmpty, reason: 'every line the engine wrote was understood');
  expect(unknownEnumValues(recording.lines), isEmpty);
  return recording.lines;
}

void main() {
  late Directory temp;
  setUp(() async => temp = await Directory.systemTemp.createTemp('hdsavior-shapes-'));
  tearDown(() async {
    if (temp.existsSync()) await temp.delete(recursive: true);
  });

  test('the shape comparison finds missing members, extra members and type changes', () {
    final real = shapesOf([
      '{"id": 1, "cmd": "catalog_info"}',
      '{"id": 1, "ok": true, "result": {"epoch": 1, "warnings": [{"code": "x", "message": "m"}], "range": null}}',
      '{"event": "log", "data": {"level": "info", "message": "hi"}}',
    ]);
    final fake = shapesOf([
      '{"id": 7, "cmd": "catalog_info"}',
      '{"id": 7, "ok": true, "result": {"epoch": "1", "warnings": [{"code": "x"}], "range": {"start": 0}, "extra": 1}}',
    ]);
    expect(compareShapes(real, fake), [
      'event:log: fake never sent it',
      'result:catalog_info.epoch: real number, fake string',
      'result:catalog_info.extra: only fake sends it (number)',
      'result:catalog_info.warnings[].message: only real sends it (string)',
    ]);
    expect(compareShapes(real, real), isEmpty);
  });

  test('the fake engine answers the commands of the real transcript with the same members', () async {
    final file = File(transcriptPath);
    if (!file.existsSync()) {
      markTestSkipped('$transcriptPath does not exist yet (the engine records it)');
      return;
    }
    final real = shapesOf(file.readAsLinesSync());
    final fake = shapesOf(
      await transcriptConversation(
        FakeEngineTransport(options: const FakeEngineOptions(tick: Duration.zero, os: 'linux')),
        '${temp.path}${Platform.pathSeparator}basic.img',
        temp,
      ),
    );
    final problems = compareShapes(real, fake);
    expect(problems, isEmpty, reason: problems.join('\n'));
  });

  test('the real engine binary has the same conversation with the same members', () async {
    final engine = Platform.environment['HDSAVIOR_ENGINE'];
    final image = Platform.environment['HDSAVIOR_TEST_IMAGE'];
    if (engine == null || engine.isEmpty || image == null || image.isEmpty) {
      markTestSkipped('set HDSAVIOR_ENGINE and HDSAVIOR_TEST_IMAGE to compare with the real engine');
      return;
    }
    final real = shapesOf(
      await transcriptConversation(ProcessEngineTransport(executable: engine), File(image).absolute.path, temp),
    );
    final fake = shapesOf(
      await transcriptConversation(
        FakeEngineTransport(options: const FakeEngineOptions(tick: Duration.zero, os: 'linux')),
        '${temp.path}${Platform.pathSeparator}fake.img',
        temp,
      ),
    );
    final problems = compareShapes(real, fake);
    expect(problems, isEmpty, reason: problems.join('\n'));
    // The checked-in transcript was recorded from this engine too.
    final file = File(transcriptPath);
    if (file.existsSync()) expect(compareShapes(shapesOf(file.readAsLinesSync()), real, actualName: 'binary'), isEmpty);
  }, timeout: const Timeout(Duration(minutes: 12)));
}
