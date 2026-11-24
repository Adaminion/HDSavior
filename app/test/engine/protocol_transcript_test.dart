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
  'close_session': (_) {},
  'scan_start': ScanStartResult.fromJson,
  'job_cancel': (_) {},
  'session_save': SessionSaveResult.fromJson,
  'session_load': SessionLoadResult.fromJson,
  'session_attach_source': SessionAttachResult.fromJson,
  'catalog_info': CatalogInfo.fromJson,
  'geometry_set': CatalogInfo.fromJson,
  'instance_set': CatalogInfo.fromJson,
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

Set<String> _wires(List<WireEnum> values) => {for (final v in values) if (v.wire.isNotEmpty) v.wire};

/// Which closed set of strings each member of the protocol holds, by its
/// path in a message (`result:<cmd>`, `event:<name>` or `error`, then the
/// members, `[]` for array elements; `*` stands for any command or event).
final Map<RegExp, Set<String>> enumMembers = {
  RegExp(r'^\w+:\w+(\.\w+)*\.source\.kind$'): _wires(SourceKind.values),
  RegExp(r'^result:open_source\.partitions\[\]\.fs$'): _wires(FileSystemKind.values),
  RegExp(r'^result:open_source\.table$'): _wires(PartitionTableKind.values),
  RegExp(r'^result:list_disks\.disks\[\]\.(table)$'): _wires(PartitionTableKind.values),
  RegExp(r'^result:list_disks\.disks\[\]\.media$'): _wires(MediaKind.values),
  RegExp(r'^result:list_disks\.disks\[\]\.flags\[\]$'): _wires(DiskFlag.values),
  RegExp(r'\.warnings\[\]\.code$'): _wires(WarningCode.values),
  RegExp(r'\.instances\[\]\.kind$'): _wires(InstanceKind.values),
  RegExp(r'\.cluster_source$'): _wires(ClusterSource.values),
  RegExp(r'\.offset_source$'): _wires(OffsetSource.values),
  RegExp(r'\.protected\[\]\.reason$'): _wires(ProtectionReason.values),
  RegExp(r'^result:check_destination\.reason$'): _wires(ProtectionReason.values),
  RegExp(r'^result:check_destination\.verdict$'): _wires(DestinationVerdict.values),
  RegExp(r'^(result:(list_children|search)\.rows\[\]|result:node_info\.row)\.kind$'): _wires(NodeKind.values),
  RegExp(r'^(result:(list_children|search)\.rows\[\]|result:node_info\.row)\.rating$'): _wires(Rating.values),
  RegExp(r'^(result:(list_children|search)\.rows\[\]|result:node_info\.row)\.flags\[\]$'): _wires(FileFlag.values),
  RegExp(r'^result:node_info\.flag_details\[\]\.flag$'): _wires(FileFlag.values),
  RegExp(r'^result:\w+\.job$'): _wires(JobKind.values),
  RegExp(r'^event:scan_progress\.phase$'): _wires(ScanPhase.values),
  RegExp(r'^event:\w+_done\.status$'): _wires(JobStatus.values),
  RegExp(r'^event:task_progress\.task$'): _wires(TaskKind.values),
  RegExp(r'^event:log\.level$'): _wires(LogLevel.values),
  RegExp(r'(^error|\.error|\.source_error)\.code$'): _wires(EngineErrorCode.values),
};

/// Every string value of [transcript] at a path of [enumMembers] that is not
/// one of the values the app knows, as `path: value`.
List<String> unknownEnumValues(Iterable<String> transcript) {
  final problems = <String>{};
  final commands = <Object?, String>{};
  void walk(Object? value, String path) {
    if (value is Map<String, Object?>) {
      for (final MapEntry(:key, :value) in value.entries) {
        walk(value, '$path.$key');
      }
    } else if (value is List<Object?>) {
      for (final element in value) {
        walk(element, '$path[]');
      }
    } else if (value is String) {
      for (final MapEntry(key: pattern, value: known) in enumMembers.entries) {
        if (pattern.hasMatch(path) && !known.contains(value)) problems.add('$path: "$value"');
      }
    }
  }

  for (final line in transcript) {
    if (line.trim().isEmpty) continue;
    final message = jsonDecode(line) as Map<String, Object?>;
    if (message['cmd'] case final String command) {
      commands[message['id']] = command;
    } else if (message['event'] case final String name) {
      walk(message['data'], 'event:$name');
    } else if (message['ok'] == true) {
      walk(message['result'], 'result:${commands[message['id']]}');
    } else {
      walk(message['error'], 'error');
    }
  }
  return problems.toList()..sort();
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

  test('the enum checker finds values the app does not know', () {
    expect(
      unknownEnumValues([
        '{"id": 1, "cmd": "catalog_info"}',
        '{"id": 1, "ok": true, "result": {"warnings": [{"code": "old_session_format", "message": ""}, {"code": "brand_new", "message": ""}], "instances": [{"kind": "main", "geometry": {"cluster_source": "attr", "offset_source": "guess"}}]}}',
        '{"event": "scan_done", "data": {"status": "stopped", "error": {"code": "source_lost", "message": "", "data": null}}}',
        '{"event": "recover_done", "data": {"status": "paused", "error": null}}',
        '{"id": 2, "cmd": "hello"}',
        '{"id": 2, "ok": false, "error": {"code": "teapot", "message": "", "data": null}}',
      ]),
      [
        'error.code: "teapot"',
        'event:recover_done.status: "paused"',
        'result:catalog_info.instances[].geometry.offset_source: "guess"',
        'result:catalog_info.warnings[].code: "brand_new"',
      ],
    );
  });

  test('every enum value in the recorded engine transcript is one the app knows', () {
    final file = File(transcriptPath);
    if (!file.existsSync()) {
      markTestSkipped('$transcriptPath does not exist yet (the engine records it)');
      return;
    }
    expect(unknownEnumValues(file.readAsLinesSync()), isEmpty);
  });
}
