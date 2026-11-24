// Model behaviour that the protocol examples do not show: 64-bit integers,
// nullable members, unknown enum values, the tolerant spots.
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

import 'protocol_examples.dart';

JsonObject decode(String text, [String name = 't']) => JsonObject.root(jsonDecode(text), name);

void main() {
  final examples = ProtocolExamples.load();

  group('integers', () {
    test('the full range up to 2^63 - 1 is exact', () {
      const max = 9223372036854775807;
      final json = decode('{"a": 9223372036854775807, "b": 4000787030016, "c": 0, "d": -1, "e": 9007199254740993}');
      expect(json.integer('a'), max);
      expect(json.integer('b'), 4000787030016);
      expect(json.integer('c'), 0);
      expect(json.integer('d'), -1);
      // 2^53 + 1: not representable as a double, so it must not pass through one.
      expect(json.integer('e'), 9007199254740993);
    });

    test('sizes beyond 32 bits survive in every model', () {
      final row = examples.object('Row')..['size'] = 9223372036854775807;
      expect(NodeRow.fromJson(JsonObject.root(row, 'Row')).size, 9223372036854775807);
      final source = examples.object('SourceInfo')..['size'] = 18000000000000;
      expect(SourceInfo.fromJson(JsonObject.root(source, 'SourceInfo')).size, 18000000000000);
    });

    test('an integral double is accepted, anything else is not', () {
      expect(decode('{"a": 4000787030016.0}').integer('a'), 4000787030016);
      expect(decode('{"a": 1e3}').integer('a'), 1000);
      expect(() => decode('{"a": 1.5}').integer('a'), throwsA(isA<ProtocolFormatException>()));
      // 2^64 does not fit: the decoder yields a double, which would silently
      // change the value.
      expect(() => decode('{"a": 18446744073709551616}').integer('a'), throwsA(isA<ProtocolFormatException>()));
      expect(() => decode('{"a": "12"}').integer('a'), throwsA(isA<ProtocolFormatException>()));
      expect(() => decode('{"a": true}').integer('a'), throwsA(isA<ProtocolFormatException>()));
    });
  });

  group('JsonObject', () {
    test('errors name the member and say what is wrong', () {
      final json = decode('{"a": null, "b": "x", "list": [1, "two"], "o": {"p": 1}}', 'thing');
      expect(
        () => json.integer('missing'),
        throwsA(isA<ProtocolFormatException>().having((e) => e.message, 'message', 'thing.missing: is missing')),
      );
      expect(
        () => json.integer('a'),
        throwsA(isA<ProtocolFormatException>().having((e) => e.message, 'message', 'thing.a: must not be null')),
      );
      expect(
        () => json.integer('b'),
        throwsA(isA<ProtocolFormatException>().having((e) => e.message, 'message', 'thing.b: expected an integer, found a string')),
      );
      expect(
        () => json.integers('list'),
        throwsA(isA<ProtocolFormatException>().having((e) => e.path, 'path', 'thing.list[1]')),
      );
      expect(
        () => json.object('o').string('p'),
        throwsA(isA<ProtocolFormatException>().having((e) => e.path, 'path', 'thing.o.p')),
      );
      expect(() => JsonObject.root([1], 'top'), throwsA(isA<ProtocolFormatException>()));
      expect(json.integerOrNull('a'), isNull);
      expect(json.integerOrNull('missing'), isNull);
      expect(json.has('a'), isFalse);
      expect(json.has('b'), isTrue);
      // A ProtocolFormatException is a FormatException.
      expect(() => json.boolean('b'), throwsFormatException);
    });
  });

  group('nullable members', () {
    test('an image source has no model, serial or disk number', () {
      final source = SourceInfo.fromJson(
        decode('{"kind": "image", "path": "D:\\\\disk.img", "size": 100, "sector_size": 512, "physical_sector_size": 512, "model": null, "serial": null, "disk_number": null}'),
      );
      expect(source.kind, SourceKind.image);
      expect(source.model, isNull);
      expect(source.serial, isNull);
      expect(source.diskNumber, isNull);
    });

    test('a partition without file system, letters or label; the pseudo-partition', () {
      final partition = Partition.fromJson(
        decode('{"index": 0, "offset": 0, "size": 100, "type": null, "type_id": null, "fs": null, "ntfs": null, "letters": [], "label": null}'),
      );
      expect(partition.index, 0);
      expect(partition.fs, isNull);
      expect(partition.ntfs, isNull);
      expect(partition.letters, isEmpty);
      expect(partition.label, isNull);
      expect(partition.type, '');
      expect(partition.typeId, '');
    });

    test('trim unknown, timestamps unknown, a folder row, an instance root', () {
      final disk = examples.object('Disk')..['trim'] = null;
      expect(Disk.fromJson(JsonObject.root(disk, 'Disk')).trim, isNull);

      final folder = examples.object('Row')
        ..['kind'] = 'dir'
        ..['parent'] = null
        ..['rating'] = null
        ..['modified'] = null
        ..['created'] = null
        ..['has_children'] = true
        ..['files'] = 12;
      final row = NodeRow.fromJson(JsonObject.root(folder, 'Row'));
      expect(row.parent, isNull);
      expect(row.rating, isNull);
      expect(row.modified, isNull);
      expect(row.modifiedTime, isNull);
      expect(row.createdTime, isNull);
      expect(row.isFolder, isTrue);
      expect(row.hasChildren, isTrue);
      expect(row.files, 12);
    });

    test('node_info of a folder and of a carved file', () {
      final info = examples.object('node_info.result')
        ..['record'] = null
        ..['data'] = null
        ..['accessed'] = null;
      final parsed = NodeInfo.fromJson(JsonObject.root(info, 'node_info'));
      expect(parsed.record, isNull);
      expect(parsed.data, isNull);
      expect(parsed.accessed, isNull);

      final resident = examples.object('node_info.result');
      (resident['data']! as Map<String, Object?>)
        ..['resident'] = true
        ..['first_lcn'] = null
        ..['first_offset'] = null;
      (resident['record']! as Map<String, Object?>)['number'] = null;
      final small = NodeInfo.fromJson(JsonObject.root(resident, 'node_info'));
      expect(small.data!.resident, isTrue);
      expect(small.data!.firstLcn, isNull);
      expect(small.data!.firstOffset, isNull);
      expect(small.record!.number, isNull);
    });

    test('a failed scan: no catalog, no session path, an error', () {
      final done = ScanDone.fromJson(
        decode('{"status": "failed", "error": {"code": "source_lost", "message": "gone", "data": null}, "session_path": null, "catalog": null}'),
      );
      expect(done.status, JobStatus.failed);
      expect(done.error!.kind, EngineErrorCode.sourceLost);
      expect(done.error!.message, 'gone');
      expect(done.sessionPath, isNull);
      expect(done.catalog, isNull);
    });

    test('check_destination for a path that is no path', () {
      final check = DestinationCheck.fromJson(
        decode('{"verdict": "invalid_path", "message": "not absolute", "free_bytes": null, "enough_space": null, "fs": null, "max_file_bytes": null, "disk_numbers": []}'),
      );
      expect(check.verdict, DestinationVerdict.invalidPath);
      expect(check.isOk, isFalse);
      expect(check.freeBytes, isNull);
      expect(check.enoughSpace, isNull);
      expect(check.fs, isNull);
      expect(check.diskNumbers, isEmpty);

      expect(check.reason, isNull);
      expect(check.lostFiles, 0, reason: 'missing counts mean none');
      expect(check.lostBytes, 0);

      final fat = examples.object('check_destination.result')..['max_file_bytes'] = 4294967295;
      expect(DestinationCheck.fromJson(JsonObject.root(fat, 'c')).maxFileBytes, 4294967295);
    });

    test('check_destination inside a lost volume: the reason and the files at risk', () {
      final refused = examples.object('check_destination.result')
        ..['verdict'] = 'overlaps_source'
        ..['reason'] = 'lost_volume'
        ..['lost_files'] = 1234
        ..['lost_bytes'] = 5678901234567;
      final check = DestinationCheck.fromJson(JsonObject.root(refused, 'c'));
      expect(check.verdict, DestinationVerdict.overlapsSource);
      expect(check.isOk, isFalse);
      expect(check.reasonName, 'lost_volume');
      expect(check.reason, ProtectionReason.lostVolume);
      expect(check.lostFiles, 1234);
      expect(check.lostBytes, 5678901234567);

      final newer = refused..['reason'] = 'cosmic_ray';
      final unknown = DestinationCheck.fromJson(JsonObject.root(newer, 'c'));
      expect(unknown.reason, ProtectionReason.unknown);
      expect(unknown.reasonName, 'cosmic_ray');
    });

    test('an instance the user chose, and protected lost volumes', () {
      final instance = examples.object('Instance')
        ..['kind'] = 'other'
        ..['manual'] = true
        ..['label'] = '[File system 3]';
      final parsed = Instance.fromJson(JsonObject.root(instance, 'Instance'));
      expect(parsed.kind, InstanceKind.other);
      expect(parsed.manual, isTrue);
      expect(parsed.label, '[File system 3]');

      final catalog = examples.object('CatalogInfo')
        ..['protected'] = [
          {'start': 1048576, 'end': 4000786153472, 'reason': 'session_range'},
          {'start': 1048576, 'end': 4100000000000, 'reason': 'lost_volume'},
        ];
      final info = CatalogInfo.fromJson(JsonObject.root(catalog, 'CatalogInfo'));
      expect(info.protected.map((r) => r.reason), [ProtectionReason.sessionRange, ProtectionReason.lostVolume]);
      expect(info.protected.last.range, const ByteRange(1048576, 4100000000000));
      expect(info.protected.last.range.length, 4100000000000 - 1048576);
    });

    test('progress without eta; recovery between files; image without bad sectors', () {
      final scan = examples.object('scan_progress')
        ..['phase'] = 'analyzing'
        ..['eta_seconds'] = null;
      final progress = ScanProgress.fromJson(JsonObject.root(scan, 's'));
      expect(progress.phase, ScanPhase.analyzing);
      expect(progress.etaSeconds, isNull);

      final recover = examples.object('recover_progress')
        ..['current'] = null
        ..['eta_seconds'] = null;
      expect(RecoverProgress.fromJson(JsonObject.root(recover, 'r')).current, isNull);

      final image = examples.object('image_done')..['bad_map_path'] = null;
      expect(ImageDone.fromJson(JsonObject.root(image, 'i')).badMapPath, isNull);
    });
  });

  group('the error member of done events and source_error', () {
    test('an error object, a bare code, a bare message, null', () {
      EngineError? error(String json) => EngineError.fromMember(decode(json), 'error');
      expect(error('{"error": null}'), isNull);
      expect(error('{}'), isNull);

      final object = error('{"error": {"code": "no_space", "message": "full", "data": {"free_bytes": 1}}}')!;
      expect(object.kind, EngineErrorCode.noSpace);
      expect(object.message, 'full');
      expect(object.data, {'free_bytes': 1});

      final code = error('{"error": "source_lost"}')!;
      expect(code.kind, EngineErrorCode.sourceLost);
      expect(code.code, 'source_lost');
      expect(code.message, '');

      final message = error('{"error": "The disk is not connected."}')!;
      expect(message.kind, EngineErrorCode.unknown);
      expect(message.message, 'The disk is not connected.');

      expect(() => error('{"error": 5}'), throwsA(isA<ProtocolFormatException>()));
      // Data that is not an object is ignored rather than fatal.
      expect(error('{"error": {"code": "internal", "message": "m", "data": [1]}}')!.data, isNull);
      expect(error('{"error": {"code": "internal"}}')!.message, '');
    });

    test('session_load reports why no source was attached', () {
      final result = examples.object('session_load.result')
        ..['source_attached'] = false
        ..['source_error'] = {'code': 'not_found', 'message': 'The disk is not connected.', 'data': null};
      final parsed = SessionLoadResult.fromJson(JsonObject.root(result, 's'));
      expect(parsed.sourceAttached, isFalse);
      expect(parsed.sourceError!.kind, EngineErrorCode.notFound);
      expect(parsed.sourceError!.message, 'The disk is not connected.');
    });

    test('source_mismatch names the failed checks; only too_few_samples can be forced', () {
      EngineError mismatch(Object? failed) => EngineError.fromJson(JsonObject.root({
        'code': 'source_mismatch',
        'message': 'm',
        'data': {'failed': failed, 'samples': 3, 'matched': 3, 'mismatched': 0, 'unreadable': 0},
      }, 'e'));
      expect(mismatch(['too_few_samples']).failedChecks, ['too_few_samples']);
      expect(mismatch(['too_few_samples']).forceMayAttach, isTrue);
      expect(mismatch(['records']).forceMayAttach, isFalse);
      expect(mismatch(['size', 'too_few_samples']).forceMayAttach, isFalse);
      expect(mismatch(null).failedChecks, isEmpty);
      expect(mismatch(null).forceMayAttach, isFalse);
      const other = EngineError(code: 'not_found', data: {'failed': ['too_few_samples']});
      expect(other.failedChecks, isEmpty, reason: 'only source_mismatch names checks');
    });
  });

  group('enums', () {
    test('every wire name of the document parses to its value and back', () {
      void check<T extends WireEnum>(List<String> wire, T Function(String) parse, T unknown) {
        for (final name in wire) {
          final value = parse(name);
          expect(value, isNot(unknown), reason: name);
          expect(value.wire, name);
        }
        expect(parse('something_new'), unknown);
      }

      check(['disk', 'image'], SourceKind.parse, SourceKind.unknown);
      check(['ntfs', 'fat', 'exfat', 'refs', 'bitlocker', 'other'], FileSystemKind.parse, FileSystemKind.unknown);
      check(['hdd', 'ssd'], MediaKind.parse, MediaKind.unknown);
      expect(MediaKind.parse('unknown'), MediaKind.unknown);
      check(['virtual', 'spaces', 'dynamic', 'no_media'], DiskFlag.parse, DiskFlag.unknown);
      check(['mbr', 'gpt', 'none'], PartitionTableKind.parse, PartitionTableKind.unknown);
      check(
        [
          'trim_supported', 'mounted_volume', 'page_file_on_source', 'system_disk', 'app_on_source', //
          'source_reads_zero', 'carve_hits_capped', 'live_volume_incomplete', 'writes_not_detectable',
          'scan_incomplete', 'out_of_range_files', 'identity_differs', 'mixed_generations',
          'session_file_at_risk', 'old_session_format', 'session_file_not_saved', 'session_incomplete',
        ],
        WarningCode.parse,
        WarningCode.unknown,
      );
      check(['attr', 'boot', 'default', 'manual'], ClusterSource.parse, ClusterSource.unknown);
      check(
        ['mft', 'index', 'boot', 'backup_boot', 'signature', 'default', 'inherited', 'manual'],
        OffsetSource.parse,
        OffsetSource.unknown,
      );
      check(['main', 'older', 'other', 'embedded', 'strays'], InstanceKind.parse, InstanceKind.unknown);
      expect(InstanceOverride.values.map((k) => k.wire), ['main', 'other', 'auto']);
      check(['session_range', 'lost_volume', 'source_image'], ProtectionReason.parse, ProtectionReason.unknown);
      check(['dir', 'file', 'lost_dir', 'virtual', 'carved'], NodeKind.parse, NodeKind.unknown);
      check(['good', 'uncertain', 'damaged', 'unsupported'], Rating.parse, Rating.unknown);
      check(
        [
          'deleted', 'torn_record', 'missing_fragments', 'size_estimated', 'out_of_bounds', 'overwritten', //
          'overlap', 'compressed', 'encrypted', 'placeholder', 'sparse', 'no_data', 'bad_sectors',
          'older_generation', 'embedded', 'stray', 'signature_ok', 'signature_mismatch',
        ],
        FileFlag.parse,
        FileFlag.unknown,
      );
      check(['mft_probe', 'sweep', 'analyzing'], ScanPhase.parse, ScanPhase.unknown);
      check(['completed', 'stopped', 'cancelled', 'failed'], JobStatus.parse, JobStatus.unknown);
      check(['scan', 'recover', 'carve', 'image'], JobKind.parse, JobKind.unknown);
      check(
        ['ok', 'overlaps_source', 'same_disk', 'unknown_disk', 'not_found', 'not_writable', 'invalid_path'],
        DestinationVerdict.parse,
        DestinationVerdict.unknown,
      );
      check(['info', 'warn', 'error'], LogLevel.parse, LogLevel.unknown);
      check(['session_load', 'geometry_set'], TaskKind.parse, TaskKind.unknown);
      expect(SortKey.values.map((k) => k.wire), ['name', 'size', 'modified', 'rating', 'ext']);
      expect(Rating.known, [Rating.good, Rating.uncertain, Rating.damaged, Rating.unsupported]);
    });

    test('folders and files', () {
      expect(NodeKind.dir.isFolder, isTrue);
      expect(NodeKind.lostDir.isFolder, isTrue);
      expect(NodeKind.virtual.isFolder, isTrue);
      expect(NodeKind.file.isFolder, isFalse);
      expect(NodeKind.carved.isFolder, isFalse);
    });

    test('a value from a newer engine does not make the message unreadable', () {
      final row = examples.object('Row')
        ..['kind'] = 'hologram'
        ..['rating'] = 'pristine'
        ..['flags'] = ['signature_ok', 'quantum_entangled'];
      final parsed = NodeRow.fromJson(JsonObject.root(row, 'Row'));
      expect(parsed.kind, NodeKind.unknown);
      expect(parsed.rating, Rating.unknown);
      expect(parsed.flagNames, ['signature_ok', 'quantum_entangled']);
      expect(parsed.flags, {FileFlag.signatureOk});

      final warning = Warning.fromJson(decode('{"code": "moon_phase", "message": "Full moon."}'));
      expect(warning.kind, WarningCode.unknown);
      expect(warning.code, 'moon_phase');
      expect(warning.message, 'Full moon.');

      final disk = examples.object('Disk')..['flags'] = ['virtual', 'levitating'];
      expect(Disk.fromJson(JsonObject.root(disk, 'Disk')).flags, {DiskFlag.virtual});
    });
  });

  group('FileFilter', () {
    test('null members are left out; an empty query is no query', () {
      expect(FileFilter.any.toJson(), isEmpty);
      expect(FileFilter.any.isAny, isTrue);
      expect(const FileFilter(query: '').toJson(), isEmpty);
      expect(const FileFilter(query: '').isAny, isTrue);
      expect(const FileFilter(minSize: 0).isAny, isFalse);
      expect(const FileFilter(minSize: 5, maxSize: 9).toJson(), {'min_size': 5, 'max_size': 9});
    });

    test('an empty list is refused, as the engine would refuse it', () {
      expect(() => const FileFilter(exts: []).toJson(), throwsArgumentError);
      expect(() => const FileFilter(ratings: []).toJson(), throwsArgumentError);
      expect(() => const FileFilter(ratings: [Rating.unknown]).toJson(), throwsArgumentError);
    });

    test('extensions typed by a user are normalised', () {
      expect(FileFilter.normalizeExts(['.JPG', ' png ', 'jpg', '', '..tar.gz', '.']), ['jpg', 'png', 'tar.gz']);
      expect(FileFilter.normalizeExts([]), isNull);
      expect(FileFilter.normalizeExts(['', ' . ']), isNull);
    });

    test('equality is by value', () {
      const a = FileFilter(query: 'x', exts: ['jpg'], ratings: [Rating.good]);
      final b = FileFilter(query: 'x', exts: ['jpg'].toList(), ratings: [Rating.good].toList());
      expect(a, b);
      expect(a.hashCode, b.hashCode);
      expect(a, isNot(const FileFilter(query: 'x', exts: ['png'], ratings: [Rating.good])));
      expect(const FileFilter(), const FileFilter(query: ''));
    });
  });

  test('timeFromMs', () {
    expect(timeFromMs(null), isNull);
    expect(timeFromMs(0), DateTime.utc(1970));
    expect(timeFromMs(1714566896000), DateTime.utc(2024, 5, 1, 12, 34, 56));
    expect(timeFromMs(-11644473600000), DateTime.utc(1601)); // the FILETIME epoch
    expect(timeFromMs(9223372036854775807), isNull, reason: 'outside the range of DateTime');
    expect(timeFromMs(1714566896000)!.isUtc, isTrue);
  });

  group('EngineException', () {
    test('toString is readable', () {
      final withMessage = EngineException(const EngineError(code: 'busy', message: 'A scan is running.'), command: 'scan_start');
      expect(withMessage.toString(), 'EngineException: busy (scan_start): A scan is running.');
      expect(EngineException(const EngineError(code: 'busy')).toString(), 'EngineException: busy');
    });

    test('destinationCheck only for destination_refused with usable data', () {
      final check = examples.object('check_destination.result')..['verdict'] = 'overlaps_source';
      expect(
        EngineException(EngineError(code: 'destination_refused', data: check)).destinationCheck!.verdict,
        DestinationVerdict.overlapsSource,
      );
      expect(EngineException(EngineError(code: 'no_space', data: check)).destinationCheck, isNull);
      expect(EngineException(const EngineError(code: 'destination_refused')).destinationCheck, isNull);
      expect(
        EngineException(const EngineError(code: 'destination_refused', data: {'verdict': 5})).destinationCheck,
        isNull,
      );
    });

    test('which codes mean that the engine is gone', () {
      expect(EngineErrorCode.values.where((c) => c.isEngineGone), {
        EngineErrorCode.engineExited,
        EngineErrorCode.engineNotRunning,
        EngineErrorCode.engineStartFailed,
      });
    });
  });

  group('locateEngineExecutable', () {
    test('next to the app executable', () {
      expect(
        locateEngineExecutable(
          environment: const {},
          appExecutable: r'C:\Program Files\HDSavior\hdsavior.exe',
          isWindows: true,
        ),
        r'C:\Program Files\HDSavior\hdsavior-engine.exe',
      );
      expect(
        locateEngineExecutable(environment: const {}, appExecutable: 'C:/apps/hd/hdsavior.exe', isWindows: true),
        'C:/apps/hd/hdsavior-engine.exe',
      );
      expect(
        locateEngineExecutable(environment: const {}, appExecutable: '/opt/hdsavior/hdsavior', isWindows: false),
        '/opt/hdsavior/hdsavior-engine',
      );
      // A backslash is an ordinary character outside Windows.
      expect(
        locateEngineExecutable(environment: const {}, appExecutable: r'/opt/odd\name/app', isWindows: false),
        r'/opt/odd\name/hdsavior-engine',
      );
      expect(locateEngineExecutable(environment: const {}, appExecutable: 'app', isWindows: false), 'hdsavior-engine');
    });

    test('HDSAVIOR_ENGINE overrides it', () {
      expect(engineOverrideVariable, 'HDSAVIOR_ENGINE');
      expect(
        locateEngineExecutable(
          environment: const {'HDSAVIOR_ENGINE': r'D:\dev\engine\target\debug\hdsavior-engine.exe'},
          appExecutable: r'C:\x\hdsavior.exe',
          isWindows: true,
        ),
        r'D:\dev\engine\target\debug\hdsavior-engine.exe',
      );
      expect(
        locateEngineExecutable(
          environment: const {'HDSAVIOR_ENGINE': '  '},
          appExecutable: '/a/b',
          isWindows: false,
        ),
        '/a/hdsavior-engine',
        reason: 'an empty value is no override',
      );
    });
  });
}
