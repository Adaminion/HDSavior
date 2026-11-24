// Recording a conversation and playing it back through the EngineClient:
// the machinery behind "the Dart fake engine replays the transcript"
// (docs/ARCHITECTURE.md, 5.6 and 7.5). A conversation with the fake engine is
// recorded here; docs/protocol-examples.jsonl, recorded from the real engine,
// is replayed as soon as it exists.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

import '../fake_engine/fake_engine.dart';
import '../fake_engine/fake_transport.dart';
import '../support/replay_transport.dart';
import 'protocol_transcript_test.dart' show checkTranscript, resultParsers, transcriptPath;

/// Sends every recorded request of [transcript] through a fresh client over
/// a [ReplayTransport] and checks that each answer reaches the caller as the
/// engine wrote it, readable by the typed models, with the events in their
/// recorded order. Returns the number of requests replayed.
Future<int> replay(List<String> transcript) async {
  final transport = ReplayTransport(transcript);
  final client = EngineClient(transport, settleTime: const Duration(milliseconds: 30));
  final protocolErrors = <String>[];
  client.protocolErrors.listen(protocolErrors.add);
  final events = <String>[];
  client.events.listen((event) {
    expect(event, isNot(isA<MalformedEvent>()), reason: 'event ${event.name}');
    events.add(event.name);
  });
  await client.start();

  final recorded = [for (final line in transcript) jsonDecode(line) as Map<String, Object?>];
  var replayed = 0;
  for (final request in transport.requests) {
    final command = request['cmd']! as String;
    if (command == 'shutdown') {
      // The client's own way to end the engine sends it.
      await client.shutdown();
      replayed++;
      continue;
    }
    final response = recorded.firstWhere((m) => m.containsKey('ok') && m['id'] == request['id']);
    Object? outcome;
    try {
      outcome = await client.request(command, args: request['args'] as Map<String, Object?>?);
    } on EngineException catch (e) {
      outcome = e;
    }
    if (response['ok'] == true) {
      expect(outcome, response['result'] ?? const <String, Object?>{}, reason: command);
      final parse = resultParsers[command];
      expect(parse, isNotNull, reason: 'a command the typed API does not know: $command');
      parse!(JsonObject.root(outcome, command));
    } else {
      final error = response['error']! as Map<String, Object?>;
      expect(outcome, isA<EngineException>().having((e) => e.code, 'code', error['code']), reason: command);
    }
    replayed++;
  }
  // A transcript without shutdown ends the replayed engine the hard way.
  if (client.isRunning) await client.kill();
  await client.dispose();

  expect(transport.mismatches, isEmpty);
  expect(transport.finished, isTrue);
  expect(protocolErrors, isEmpty, reason: 'every line was understood');
  expect(events, [
    for (final message in recorded)
      if (message['event'] case final String name) name,
  ]);
  return replayed;
}

void main() {
  test('a conversation with the fake engine, recorded and played back', () async {
    final recording = RecordingTransport(
      FakeEngineTransport(options: const FakeEngineOptions(tick: Duration.zero)),
    );
    final client = EngineClient(recording);
    final api = EngineApi(client);
    await client.start();
    await api.hello();
    final disks = await api.listDisks();
    await api.openSource(kind: SourceKind.disk, path: disks[1].path);
    final scanned = client.on<ScanDone>().first;
    await api.scanStart(start: 1048576, end: 4000786153472);
    final catalog = (await scanned).catalog!;
    final root = catalog.mainInstance!.root;
    final page = await api.listChildren(epoch: catalog.epoch, node: root);
    await api.checkDestination(path: r'E:\Rescued', purpose: DestinationPurpose.recover);
    await expectLater(api.nodeInfo(epoch: catalog.epoch + 7, node: root), throwsA(isA<EngineException>()));
    final file = page.rows.firstWhere((r) => r.isFile && r.rating != Rating.unsupported && r.size > 0);
    await api.readFile(epoch: catalog.epoch, node: file.id, length: 512);
    final changed = await api.instanceSet(instance: 1, kind: InstanceOverride.main);
    await api.selectionSummary(epoch: changed.epoch, include: [changed.mainInstance!.root], exclude: []);
    await client.shutdown();
    await client.dispose();

    final transcript = recording.lines;
    expect(checkTranscript(transcript)['requests'], greaterThanOrEqualTo(11));
    expect(transcript.where((l) => l.contains('"event":"scan_progress"')), isNotEmpty);

    final replayed = await replay(transcript);
    expect(replayed, checkTranscript(transcript)['requests']);
  });

  test('a request the transcript does not expect is answered with an error and noted', () async {
    final transport = ReplayTransport([
      jsonEncode({'id': 1, 'cmd': 'hello'}),
      jsonEncode({
        'id': 1,
        'ok': true,
        'result': {'engine_version': '0.1.0', 'protocol': 1, 'os': 'linux', 'elevated': false, 'raw_disk_support': false, 'carve_kinds': <String>[]},
      }),
    ]);
    final client = EngineClient(transport, settleTime: const Duration(milliseconds: 30));
    await client.start();
    await expectLater(
      client.request('list_disks'),
      throwsA(isA<EngineException>().having((e) => e.message, 'message', contains('expected hello'))),
    );
    expect((await EngineApi(client).hello()).protocol, 1, reason: 'the transcript continues where it was');
    await expectLater(client.request('hello'), throwsA(isA<EngineException>()));
    expect(transport.mismatches, ['expected hello, got list_disks', 'hello after the end of the transcript']);
    await client.dispose();
  });

  test('the transcript recorded from the real engine plays back through the client', () async {
    final file = File(transcriptPath);
    if (!file.existsSync()) {
      markTestSkipped('$transcriptPath does not exist yet (the engine records it)');
      return;
    }
    expect(await replay(file.readAsLinesSync().where((l) => l.trim().isNotEmpty).toList()), greaterThan(0));
  });
}
