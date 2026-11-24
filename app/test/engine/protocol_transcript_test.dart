// docs/protocol-examples.jsonl is a transcript recorded from the real engine
// (docs/ARCHITECTURE.md, section 5.6). When it exists, every line of it must
// be readable by the client side: requests name commands the typed API knows,
// results parse with the model of their command, events parse as events.
//
// The engine is being written in parallel with the app; until its transcript
// is checked in, this test reports itself as skipped.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

const String transcriptPath = '../docs/protocol-examples.jsonl';

/// How the result of each command is read.
final Map<String, void Function(JsonObject result)> resultParsers = {
  'hello': HelloResult.fromJson,
  'list_disks': (json) => json.objects('disks', Disk.fromJson),
  'open_source': OpenSourceResult.fromJson,
  'close_source': (_) {},
  'scan_start': ScanStartResult.fromJson,
  'job_cancel': (_) {},
  'session_save': SessionSaveResult.fromJson,
  'session_load': SessionLoadResult.fromJson,
  'session_attach_source': SessionAttachResult.fromJson,
  'catalog_info': CatalogInfo.fromJson,
  'geometry_set': CatalogInfo.fromJson,
  'list_children': RowsPage.fromJson,
  'node_info': NodeInfo.fromJson,
  'search': RowsPage.fromJson,
  'selection_summary': SelectionSummary.fromJson,
  'check_destination': DestinationCheck.fromJson,
  'recover_start': RecoverStartResult.fromJson,
  'carve_start': CarveStartResult.fromJson,
  'image_start': ImageStartResult.fromJson,
  'read_file': ReadFileResult.fromJson,
  'shutdown': (_) {},
};

/// Checks a transcript: one JSON object per line, requests (`cmd`),
/// responses (`ok`) and events (`event`) in the order they were exchanged.
/// Returns how many lines of each kind were seen.
Map<String, int> checkTranscript(Iterable<String> lines) {
  final commands = <int, String>{};
  final seen = {'requests': 0, 'responses': 0, 'events': 0};
  var number = 0;
  for (final line in lines) {
    number++;
    if (line.trim().isEmpty) continue;
    final message = jsonDecode(line) as Map<String, Object?>;
    final where = 'line $number';
    if (message['cmd'] case final String command) {
      expect(resultParsers, contains(command), reason: '$where: a command the typed API does not know');
      commands[message['id']! as int] = command;
      seen['requests'] = seen['requests']! + 1;
    } else if (message['event'] case final String name) {
      final event = parseEngineEvent(name, message['data']);
      expect(event, isNot(isA<UnknownEvent>()), reason: '$where: unknown event $name');
      expect(event, isNot(isA<MalformedEvent>()), reason: '$where: ${event is MalformedEvent ? event.problem.message : ''}');
      seen['events'] = seen['events']! + 1;
    } else {
      final command = commands[message['id']];
      expect(command, isNotNull, reason: '$where: a response to no request');
      if (message['ok'] == true) {
        final result = JsonObject.root(message['result'] ?? const <String, Object?>{}, command!);
        expect(() => resultParsers[command]!(result), returnsNormally, reason: '$where: result of $command');
      } else {
        final error = EngineError.fromJson(JsonObject.root(message['error'], '$command.error'));
        expect(error.kind, isNot(EngineErrorCode.unknown), reason: '$where: error code ${error.code}');
      }
      seen['responses'] = seen['responses']! + 1;
    }
  }
  return seen;
}

void main() {
  test('the checker accepts a well-formed conversation and rejects a malformed one', () {
    final good = [
      '{"id": 1, "cmd": "hello"}',
      '{"id": 1, "ok": true, "result": {"engine_version": "0.1.0", "protocol": 1, "os": "linux", "elevated": false, "raw_disk_support": false, "carve_kinds": []}}',
      '{"id": 2, "cmd": "catalog_info"}',
      '{"id": 2, "ok": false, "error": {"code": "no_catalog", "message": "nothing scanned", "data": null}}',
      '{"event": "log", "data": {"level": "info", "message": "hi"}}',
    ];
    expect(checkTranscript(good), {'requests': 2, 'responses': 2, 'events': 1});
    expect(
      () => checkTranscript(['{"id": 1, "cmd": "hello"}', '{"id": 1, "ok": true, "result": {"protocol": 1}}']),
      throwsA(isA<TestFailure>()),
    );
    expect(() => checkTranscript(['{"event": "scan_done", "data": {}}']), throwsA(isA<TestFailure>()));
    expect(() => checkTranscript(['{"id": 1, "cmd": "format_disk"}']), throwsA(isA<TestFailure>()));
  });

  test('every line of the recorded engine transcript is understood', () {
    final file = File(transcriptPath);
    if (!file.existsSync()) {
      markTestSkipped('$transcriptPath does not exist yet (the engine records it)');
      return;
    }
    final seen = checkTranscript(file.readAsLinesSync());
    expect(seen['requests'], greaterThan(0));
    expect(seen['responses'], seen['requests']);
  });
}
