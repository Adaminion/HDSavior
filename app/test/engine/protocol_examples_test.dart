// Model parsing against every JSON example of docs/PROTOCOL.md.
//
// The examples live in test/data/protocol_examples.txt as verbatim copies.
// These tests check that (1) the copies still match the document, (2) every
// example is accounted for, (3) every result and event example parses, with
// each documented member read and exposed with the documented value, and
// (4) the parsers ignore unknown members and answer any malformed input with
// a ProtocolFormatException and nothing else.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

import 'protocol_examples.dart';

/// Result and event examples with the parser that must accept them.
final Map<String, Object? Function(JsonObject json)> parsers = {
  'SourceInfo': SourceInfo.fromJson,
  'Partition': Partition.fromJson,
  'Disk': Disk.fromJson,
  'Warning': Warning.fromJson,
  'Geometry': Geometry.fromJson,
  'Instance': Instance.fromJson,
  'CatalogInfo': CatalogInfo.fromJson,
  'Row': NodeRow.fromJson,
  'hello.result': HelloResult.fromJson,
  'list_disks.result': (json) => json.objects('disks', Disk.fromJson),
  'open_source.result': OpenSourceResult.fromJson,
  'scan_start.result': ScanStartResult.fromJson,
  'scan_progress': ScanProgress.fromJson,
  'scan_done': ScanDone.fromJson,
  'session_save.result': SessionSaveResult.fromJson,
  'session_load.result': SessionLoadResult.fromJson,
  'session_attach_source.result': SessionAttachResult.fromJson,
  'list_children.result': RowsPage.fromJson,
  'node_info.result': NodeInfo.fromJson,
  'search.result': RowsPage.fromJson,
  'selection_summary.result': SelectionSummary.fromJson,
  'check_destination.result': DestinationCheck.fromJson,
  'recover_start.result': RecoverStartResult.fromJson,
  'recover_progress': RecoverProgress.fromJson,
  'recover_done': RecoverDone.fromJson,
  'carve_start.result': CarveStartResult.fromJson,
  'carve_progress': CarveProgress.fromJson,
  'carve_done': CarveDone.fromJson,
  'image_start.result': ImageStartResult.fromJson,
  'image_progress': ImageProgress.fromJson,
  'image_done': ImageDone.fromJson,
  'read_file.result': ReadFileResult.fromJson,
  'task_progress': TaskProgress.fromJson,
  'log': LogEvent.fromJson,
};

/// Examples that are arguments of requests. engine_api_test.dart checks that
/// the typed API sends exactly these members.
const Set<String> argumentExamples = {
  'Filter',
  'Disk.extra', // part of 'Disk'
  'open_source.args',
  'scan_start.args',
  'session_save.args',
  'session_load.args',
  'session_load.args.source',
  'session_attach_source.args',
  'geometry_set.args',
  'instance_set.args',
  'list_children.args',
  'node_info.args',
  'search.args',
  'selection_summary.args',
  'check_destination.args',
  'recover_start.args',
  'carve_start.args',
  'image_start.args',
  'read_file.args',
};

/// Examples of the empty result `{}`.
const Set<String> emptyResults = {
  'close_source.result',
  'job_cancel.result',
  'job_cancel.result.no_job',
  'shutdown.result',
};

/// The lines of the transport section. engine_client_test.dart feeds them to
/// the client.
const Set<String> transportExamples = {
  'transport.request',
  'transport.response_ok',
  'transport.response_error',
  'transport.event',
};

/// The examples that are events, with their event name.
const Map<String, String> eventExamples = {
  'scan_progress': 'scan_progress',
  'scan_done': 'scan_done',
  'recover_progress': 'recover_progress',
  'recover_done': 'recover_done',
  'carve_progress': 'carve_progress',
  'carve_done': 'carve_done',
  'image_progress': 'image_progress',
  'image_done': 'image_done',
  'task_progress': 'task_progress',
  'log': 'log',
};

T parse<T>(ProtocolExamples examples, String name, T Function(JsonObject json) fromJson) =>
    fromJson(JsonObject.root(examples.json(name), name));

/// Every way of removing one member from [value], at any depth.
Iterable<(String, Object?)> withoutOneMember(Object? value, [String path = r'$']) sync* {
  if (value is Map<String, Object?>) {
    for (final key in value.keys) {
      yield ('$path.$key', {
        for (final entry in value.entries)
          if (entry.key != key) entry.key: entry.value,
      });
      for (final (where, changed) in withoutOneMember(value[key], '$path.$key')) {
        yield (where, {...value, key: changed});
      }
    }
  } else if (value is List<Object?>) {
    for (var i = 0; i < value.length; i++) {
      for (final (where, changed) in withoutOneMember(value[i], '$path[$i]')) {
        yield (where, [...value]..[i] = changed);
      }
    }
  }
}

/// Every way of replacing one member of [value] by [replacement].
Iterable<(String, Object?)> withOneMemberReplaced(
  Object? value,
  Object? replacement, [
  String path = r'$',
]) sync* {
  if (value is Map<String, Object?>) {
    for (final key in value.keys) {
      yield ('$path.$key', {...value, key: replacement});
      for (final (where, changed) in withOneMemberReplaced(value[key], replacement, '$path.$key')) {
        yield (where, {...value, key: changed});
      }
    }
  } else if (value is List<Object?>) {
    for (var i = 0; i < value.length; i++) {
      yield ('$path[$i]', [...value]..[i] = replacement);
      for (final (where, changed) in withOneMemberReplaced(value[i], replacement, '$path[$i]')) {
        yield (where, [...value]..[i] = changed);
      }
    }
  }
}

/// [value] with an unknown member added to every object in it.
Object? withUnknownMembers(Object? value) => switch (value) {
  final Map<String, Object?> map => {
    'x_added_by_a_newer_engine': {
      'nested': [1, 'two', null],
    },
    for (final entry in map.entries) entry.key: withUnknownMembers(entry.value),
  },
  final List<Object?> list => [for (final element in list) withUnknownMembers(element)],
  _ => value,
};

void main() {
  final examples = ProtocolExamples.load();

  group('test data and document', () {
    test('every example of docs/PROTOCOL.md is in the test data, verbatim, and vice versa', () {
      final file = File(protocolDocumentPath);
      if (!file.existsSync()) {
        markTestSkipped('$protocolDocumentPath is not available next to the app directory');
        return;
      }
      final inDocument = examplesInDocument(file.readAsStringSync());
      final inTestData = examples.verbatim.values.toSet();
      expect(
        inDocument.difference(inTestData),
        isEmpty,
        reason: 'PROTOCOL.md has examples that $protocolExamplesPath lacks (or shows differently)',
      );
      expect(
        inTestData.difference(inDocument),
        isEmpty,
        reason: '$protocolExamplesPath has examples that are not (or no longer) in PROTOCOL.md',
      );
    });

    test('every example is covered by a test', () {
      final covered = {...parsers.keys, ...argumentExamples, ...emptyResults, ...transportExamples};
      // 'Disk' is assembled from 'SourceInfo' and 'Disk.extra'.
      expect(examples.names.toSet().difference(covered), isEmpty);
      expect(covered.difference({...examples.names, 'Disk'}), isEmpty);
    });

    test('every example resolves to valid JSON', () {
      for (final name in [...examples.names, 'Disk']) {
        expect(() => examples.json(name), returnsNormally, reason: name);
      }
      for (final name in emptyResults) {
        expect(examples.json(name), isEmpty, reason: name);
      }
    });
  });

  group('every result and event example', () {
    for (final MapEntry(key: name, value: fromJson) in parsers.entries) {
      test('$name parses and every documented member is read', () {
        final recording = examples.recording(name);
        fromJson(JsonObject.root(recording, name));
        expect(recording.unread(), isEmpty, reason: 'members of $name that the model ignores');
      });

      test('$name: unknown members are ignored', () {
        final extended = withUnknownMembers(examples.json(name));
        expect(() => fromJson(JsonObject.root(extended, name)), returnsNormally);
      });

      test('$name: a missing or mistyped member is a ProtocolFormatException, never another error', () {
        final original = examples.json(name);
        var rejected = 0;
        void attempt(String where, Object? changed) {
          try {
            fromJson(JsonObject.root(changed, name));
          } on ProtocolFormatException catch (e) {
            rejected++;
            // The message leads to the member.
            expect(e.message, startsWith(name), reason: where);
          } on Object catch (e) {
            fail('$name with $where changed: $e (${e.runtimeType})');
          }
        }

        for (final (where, changed) in withoutOneMember(original)) {
          attempt('$where removed', changed);
        }
        for (final replacement in <Object?>[
          null,
          true,
          12,
          1.5,
          'text',
          <Object?>[],
          <Object?>[1, 'x'],
          <String, Object?>{},
        ]) {
          for (final (where, changed) in withOneMemberReplaced(original, replacement)) {
            attempt('$where = ${jsonEncode(replacement)}', changed);
          }
        }
        expect(rejected, greaterThan(0), reason: 'some change must be rejected');
      });
    }

    test('event examples are recognised by parseEngineEvent', () {
      final types = <String, Type>{};
      for (final MapEntry(key: example, value: eventName) in eventExamples.entries) {
        final event = parseEngineEvent(eventName, examples.json(example));
        expect(event, isNot(isA<MalformedEvent>()), reason: example);
        expect(event, isNot(isA<UnknownEvent>()), reason: example);
        expect(event.name, eventName);
        types[eventName] = event.runtimeType;
      }
      expect(types, {
        'scan_progress': ScanProgress,
        'scan_done': ScanDone,
        'recover_progress': RecoverProgress,
        'recover_done': RecoverDone,
        'carve_progress': CarveProgress,
        'carve_done': CarveDone,
        'image_progress': ImageProgress,
        'image_done': ImageDone,
        'task_progress': TaskProgress,
        'log': LogEvent,
      });
    });
  });

  group('documented values', () {
    void expectSourceInfo(SourceInfo source) {
      expect(source.kind, SourceKind.disk);
      expect(source.path, r'\\.\PhysicalDrive1');
      expect(source.size, 4000787030016);
      expect(source.sectorSize, 512);
      expect(source.physicalSectorSize, 4096);
      expect(source.model, 'WDC WD40EZRZ-00GXCB0');
      expect(source.serial, 'WD-WCC7K0000000');
      expect(source.diskNumber, 1);
    }

    void expectPartition(Partition partition) {
      expect(partition.index, 1);
      expect(partition.offset, 1048576);
      expect(partition.size, 4000785104896);
      expect(partition.type, 'Basic data');
      expect(partition.typeId, 'EBD0A0A2-B9E5-4433-87C0-68B6B72699C7');
      expect(partition.fs, FileSystemKind.ntfs);
      expect(partition.ntfs!.clusterSize, 4096);
      expect(partition.ntfs!.totalSectors, 7814033407);
      expect(partition.ntfs!.serial, 'A1B2C3D4E5F60718');
      expect(partition.letters, ['E:']);
      expect(partition.label, 'New Volume');
      expect(partition.end, 1048576 + 4000785104896);
      expect(partition.range, const ByteRange(1048576, 4000786153472));
    }

    void expectWarning(Warning warning) {
      expect(warning.code, 'source_reads_zero');
      expect(warning.kind, WarningCode.sourceReadsZero);
      expect(warning.message, '...');
    }

    void expectGeometry(Geometry geometry) {
      expect(geometry.clusterSize, 4096);
      expect(geometry.volumeOffset, 1048576);
      expect(geometry.clusterSource, ClusterSource.attr);
      expect(geometry.offsetSource, OffsetSource.indexVotes);
      expect(geometry.indexDirs, 310);
      expect(geometry.indexBlocks, 412);
      expect(geometry.signatureChecked, 200);
      expect(geometry.signatureOk, 197);
      expect(geometry.validated, isTrue);
      expect(geometry.totalClusters, 976754175);
    }

    void expectInstance(Instance instance) {
      expect(instance.id, 0);
      expect(instance.kind, InstanceKind.main);
      expect(instance.manual, isFalse);
      expect(instance.label, '');
      expect(instance.root, 0);
      expect(instance.records, 812401);
      expect(instance.files, 790001);
      expect(instance.dirs, 22300);
      expect(instance.deletedFiles, 5120);
      expect(instance.lostDirs, 14);
      expect(instance.bytes, 3210987654321);
      expect(instance.timeFrom, 1420070400000);
      expect(instance.timeTo, 1763942400000);
      expect(instance.embeddedIn, isNull);
      expectGeometry(instance.geometry);
      expect(instance.geometryCandidates, hasLength(1));
      expectGeometry(instance.geometryCandidates.single);
      expect(instance.virtual.lost, 3);
      expect(instance.virtual.orphans, 4);
      expect(instance.virtual.carved, isNull);
    }

    void expectCatalog(CatalogInfo catalog) {
      expect(catalog.epoch, 3);
      expect(catalog.instances, hasLength(1));
      expectInstance(catalog.instances.single);
      expect(catalog.mainInstance, same(catalog.instances.first));
      expect(catalog.instanceOfRoot(0), same(catalog.instances.first));
      expect(catalog.instanceOfRoot(1), isNull);
      expect(catalog.nodes, 812345);
      expect(catalog.liveRecords, 41);
      expect(catalog.postFormatRecords, 6);
      expect(catalog.liveVolumes.single.offset, 1048576);
      expect(catalog.liveVolumes.single.size, 4000785104896);
      expect(catalog.liveVolumes.single.formatTime, 1763900000000);
      expect(catalog.liveVolumes.single.snapshotTime, 1763990000000);
      expect(catalog.range, const ByteRange(1048576, 4000786153472));
      expect(catalog.range.length, 4000785104896);
      expect(catalog.protected, hasLength(1));
      expect(catalog.protected.single.start, 1048576);
      expect(catalog.protected.single.end, 4000786153472);
      expect(catalog.protected.single.range, catalog.range);
      expect(catalog.protected.single.reasonName, 'session_range');
      expect(catalog.protected.single.reason, ProtectionReason.sessionRange);
      expect(catalog.scannedBytes, 4000785104896);
      expect(catalog.scanComplete, isTrue);
      expect(catalog.badBytes, 0);
      expect(catalog.outOfRangeFiles, 0);
      expect(catalog.outOfRangeBytes, 0);
      expect(catalog.carveHits, 48213);
      expect(catalog.carved, 0);
      expect(catalog.sourceAttached, isTrue);
      expectWarning(catalog.warnings.single);
    }

    void expectRow(NodeRow row) {
      expect(row.id, 4711);
      expect(row.parent, 17);
      expect(row.name, 'IMG_0042.JPG');
      expect(row.kind, NodeKind.file);
      expect(row.isFile, isTrue);
      expect(row.isFolder, isFalse);
      expect(row.ext, 'jpg');
      expect(row.size, 3456789);
      expect(row.modified, 1714566896000);
      expect(row.created, 1714566896000);
      expect(row.modifiedTime, DateTime.utc(2024, 5, 1, 12, 34, 56));
      expect(row.createdTime, DateTime.utc(2024, 5, 1, 12, 34, 56));
      expect(row.deleted, isFalse);
      expect(row.rating, Rating.good);
      expect(row.flagNames, ['signature_ok']);
      expect(row.flags, {FileFlag.signatureOk});
      expect(row.files, 0);
      expect(row.deletedFiles, 0);
      expect(row.deletedSize, 0);
      expect(row.hasChildren, isFalse);
    }

    test('SourceInfo', () => expectSourceInfo(parse(examples, 'SourceInfo', SourceInfo.fromJson)));

    test('Partition', () => expectPartition(parse(examples, 'Partition', Partition.fromJson)));

    test('Disk: the members of SourceInfo plus its own', () {
      final disk = parse(examples, 'Disk', Disk.fromJson);
      expectSourceInfo(disk);
      expect(disk.number, 1);
      expect(disk.bus, 'USB');
      expect(disk.removable, isFalse);
      expect(disk.isSystem, isFalse);
      expect(disk.media, MediaKind.hdd);
      expect(disk.trim, isFalse);
      expect(disk.flagNames, isEmpty);
      expect(disk.flags, isEmpty);
      expect(disk.table, PartitionTableKind.gpt);
      expectPartition(disk.partitions.single);
    });

    test('Warning', () => expectWarning(parse(examples, 'Warning', Warning.fromJson)));

    test('Geometry', () => expectGeometry(parse(examples, 'Geometry', Geometry.fromJson)));

    test('Instance', () => expectInstance(parse(examples, 'Instance', Instance.fromJson)));

    test('CatalogInfo', () => expectCatalog(parse(examples, 'CatalogInfo', CatalogInfo.fromJson)));

    test('Row', () {
      final row = parse(examples, 'Row', NodeRow.fromJson);
      expectRow(row);
      expect(row.path, isNull);
      expect(row.ancestorIds, isNull);
    });

    test('hello', () {
      final hello = parse(examples, 'hello.result', HelloResult.fromJson);
      expect(hello.engineVersion, '0.1.0');
      expect(hello.protocol, 1);
      expect(hello.protocol, supportedProtocolVersion);
      expect(hello.os, 'windows');
      expect(hello.elevated, isTrue);
      expect(hello.rawDiskSupport, isTrue);
      expect(hello.carveKinds, ['jpeg', 'png']);
    });

    test('list_disks', () {
      final disks = parse(examples, 'list_disks.result', (json) => json.objects('disks', Disk.fromJson));
      expect(disks, hasLength(1));
      expectSourceInfo(disks.single);
      expect(disks.single.number, 1);
    });

    test('open_source', () {
      final result = parse(examples, 'open_source.result', OpenSourceResult.fromJson);
      expectSourceInfo(result.source);
      expect(result.table, PartitionTableKind.gpt);
      expectPartition(result.partitions.single);
      expectWarning(result.warnings.single);
    });

    test('scan_start', () {
      final result = parse(examples, 'scan_start.result', ScanStartResult.fromJson);
      expect(result.job, JobKind.scan);
      expect(result.bytesTotal, 4000785104896);
    });

    test('scan_progress', () {
      final progress = parse(examples, 'scan_progress', ScanProgress.fromJson);
      expect(progress.phase, ScanPhase.mftProbe);
      expect(progress.bytesDone, 1073741824);
      expect(progress.bytesTotal, 4000785104896);
      expect(progress.position, 4294967296);
      expect(progress.speedBps, 157286400);
      expect(progress.etaSeconds, 25430);
      expect(progress.elapsedSeconds, 7);
      expect(progress.records, 120344);
      expect(progress.files, 118002);
      expect(progress.dirs, 2342);
      expect(progress.indexBlocks, 904);
      expect(progress.carveHits, 31);
      expect(progress.badBytes, 0);
      expect(progress.warnings, isEmpty);
      expect(progress.job, JobKind.scan);
      expect(progress.fraction, closeTo(1073741824 / 4000785104896, 1e-12));
    });

    test('scan_done', () {
      final done = parse(examples, 'scan_done', ScanDone.fromJson);
      expect(done.status, JobStatus.completed);
      expect(done.error, isNull);
      expect(done.sessionPath, '...');
      expectCatalog(done.catalog!);
      expect(done.job, JobKind.scan);
    });

    test('session_save', () {
      final result = parse(examples, 'session_save.result', SessionSaveResult.fromJson);
      expect(result.path, '...');
      expect(result.bytes, 12345678);
    });

    test('session_load', () {
      final result = parse(examples, 'session_load.result', SessionLoadResult.fromJson);
      expectSourceInfo(result.source);
      expect(result.sourceAttached, isTrue);
      expect(result.sourceError, isNull);
      expectCatalog(result.catalog);
    });

    test('session_attach_source', () {
      final result = parse(examples, 'session_attach_source.result', SessionAttachResult.fromJson);
      expectSourceInfo(result.source);
      expectCatalog(result.catalog);
    });

    test('list_children', () {
      final page = parse(examples, 'list_children.result', RowsPage.fromJson);
      expect(page.total, 5321);
      expectRow(page.rows.single);
    });

    test('node_info', () {
      final info = parse(examples, 'node_info.result', NodeInfo.fromJson);
      expectRow(info.row);
      expect(info.ancestors.map((a) => (a.id, a.name)), [(0, ''), (17, 'Photos')]);
      expect(info.ancestorIds, [0, 17]);
      expect(info.path, 'Photos/IMG_0042.JPG');
      expect(info.instance, 0);
      expect(info.accessed, 1714566896000);
      expect(info.otherNames, isEmpty);
      expect(info.record!.number, 91234);
      expect(info.record!.seq, 3);
      expect(info.record!.diskOffset, 3314748416);
      expect(info.record!.extensionRecords, 0);
      expect(info.data!.resident, isFalse);
      expect(info.data!.realSize, 3456789);
      expect(info.data!.initializedSize, 3456789);
      expect(info.data!.allocatedSize, 3457024);
      expect(info.data!.runs, 1);
      expect(info.data!.firstLcn, 812345);
      expect(info.data!.firstOffset, 3328413696);
      expect(info.flagDetails.single.flag, 'signature_ok');
      expect(info.flagDetails.single.kind, FileFlag.signatureOk);
      expect(info.flagDetails.single.text, 'The file starts with a valid JPEG header.');
    });

    test('search: rows carry path and ancestor ids', () {
      final page = parse(examples, 'search.result', RowsPage.fromJson);
      expect(page.total, 1234);
      expectRow(page.rows.single);
      expect(page.rows.single.path, 'Photos/IMG_0042.JPG');
      expect(page.rows.single.ancestorIds, [0, 17]);
    });

    test('selection_summary', () {
      final summary = parse(examples, 'selection_summary.result', SelectionSummary.fromJson);
      expect(summary.files, 5320);
      expect(summary.bytes, 18234567890);
      expect(summary.dirs, 12);
      expect(summary.good, 5300);
      expect(summary.uncertain, 12);
      expect(summary.damaged, 5);
      expect(summary.unsupported, 3);
      expect(summary.good + summary.uncertain + summary.damaged + summary.unsupported, summary.files);
    });

    test('check_destination', () {
      final check = parse(examples, 'check_destination.result', DestinationCheck.fromJson);
      expect(check.verdict, DestinationVerdict.ok);
      expect(check.isOk, isTrue);
      expect(check.reasonName, isNull);
      expect(check.reason, isNull);
      expect(check.message, '');
      expect(check.lostFiles, 0);
      expect(check.lostBytes, 0);
      expect(check.freeBytes, 500000000000);
      expect(check.enoughSpace, isTrue);
      expect(check.fs, 'NTFS');
      expect(check.maxFileBytes, isNull);
      expect(check.diskNumbers, [0]);
    });

    test('recover_start', () {
      final result = parse(examples, 'recover_start.result', RecoverStartResult.fromJson);
      expect(result.job, JobKind.recover);
      expect(result.files, 5320);
      expect(result.bytes, 18234567890);
    });

    test('recover_progress', () {
      final progress = parse(examples, 'recover_progress', RecoverProgress.fromJson);
      expect(progress.filesDone, 120);
      expect(progress.filesTotal, 5320);
      expect(progress.bytesDone, 734003200);
      expect(progress.bytesTotal, 18234567890);
      expect(progress.current, 'Photos/IMG_0042.JPG');
      expect(progress.speedBps, 104857600);
      expect(progress.etaSeconds, 167);
      expect(progress.elapsedSeconds, 7);
      expect(progress.ok, 118);
      expect(progress.warnings, 2);
      expect(progress.failed, 0);
      expect(progress.skipped, 0);
      expect(progress.fraction, closeTo(734003200 / 18234567890, 1e-12));
    });

    test('recover_done', () {
      final done = parse(examples, 'recover_done', RecoverDone.fromJson);
      expect(done.status, JobStatus.completed);
      expect(done.error, isNull);
      expect(done.filesTotal, 5320);
      expect(done.ok, 5298);
      expect(done.warnings, 17);
      expect(done.failed, 2);
      expect(done.skipped, 3);
      expect(done.ok + done.warnings + done.failed + done.skipped, done.filesTotal);
      expect(done.notAttempted, 0);
      expect(done.bytesWritten, 18230000000);
      expect(done.dest, r'D:\Rescued');
      expect(done.reportPath, r'D:\Rescued\HDSavior-report-20261124-031500.csv');
    });

    test('carve_start, carve_progress, carve_done', () {
      final start = parse(examples, 'carve_start.result', CarveStartResult.fromJson);
      expect(start.job, JobKind.carve);
      expect(start.candidates, 48213);

      final progress = parse(examples, 'carve_progress', CarveProgress.fromJson);
      expect(progress.candidatesDone, 100);
      expect(progress.candidatesTotal, 48213);
      expect(progress.found, 61);
      expect(progress.bytesFound, 310000000);
      expect(progress.etaSeconds, 900);
      expect(progress.fraction, closeTo(100 / 48213, 1e-12));

      final done = parse(examples, 'carve_done', CarveDone.fromJson);
      expect(done.status, JobStatus.completed);
      expect(done.error, isNull);
      expect(done.found, 913);
      expect(done.bytesFound, 4100000000);
      expectCatalog(done.catalog!);
    });

    test('image_start, image_progress, image_done', () {
      final start = parse(examples, 'image_start.result', ImageStartResult.fromJson);
      expect(start.job, JobKind.image);
      expect(start.bytes, 4000787030016);

      // The document lists the member names of these two; the values are the
      // samples of protocol_examples.dart.
      final progress = parse(examples, 'image_progress', ImageProgress.fromJson);
      expect(progress.bytesDone, 1048576);
      expect(progress.bytesTotal, 4000787030016);
      expect(progress.speedBps, 157286400);
      expect(progress.etaSeconds, 25430);
      expect(progress.elapsedSeconds, 7);
      expect(progress.badBytes, 512);

      final done = parse(examples, 'image_done', ImageDone.fromJson);
      expect(done.status, JobStatus.completed);
      expect(done.error, isNull);
      expect(done.bytesWritten, 4000787030016);
      expect(done.badBytes, 512);
      expect(done.path, r'D:\disk1.img');
      expect(done.badMapPath, r'D:\disk1.img.bad.txt');
    });

    test('read_file: the data is decoded from base64', () {
      final result = parse(examples, 'read_file.result', ReadFileResult.fromJson);
      expect(utf8.decode(result.bytes), 'HDSavior');
      expect(result.size, 3456789);
      expect(result.eof, isFalse);
      expect(result.kind, 'jpeg');
    });

    test('task_progress and log', () {
      final task = parse(examples, 'task_progress', TaskProgress.fromJson);
      expect(task.taskName, 'session_load');
      expect(task.task, TaskKind.sessionLoad);
      expect(task.fraction, 0.42);

      final log = parse(examples, 'log', LogEvent.fromJson);
      expect(log.level, LogLevel.info);
      expect(log.message, '...');
    });
  });
}
