// The typed API: what each method sends, compared with the argument examples
// of docs/PROTOCOL.md, and how it turns answers into results and errors.
import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

import '../support/scripted_transport.dart';
import 'protocol_examples.dart';

void main() {
  final examples = ProtocolExamples.load();
  late ScriptedTransport transport;
  late EngineClient client;
  late EngineApi api;

  setUp(() async {
    transport = ScriptedTransport();
    client = EngineClient(transport, settleTime: const Duration(milliseconds: 30));
    api = EngineApi(client);
    await client.start();
  });

  tearDown(() => client.dispose());

  /// Makes [call], returns the request it wrote, and ends it with an error.
  Future<Map<String, Object?>> requestOf(Future<Object?> Function() call) async {
    final outcome = call().then<Object?>((value) => value, onError: (Object e) => e);
    await pumpEventQueue();
    final request = transport.connection.lastRequest;
    transport.connection.respondError(request['id']! as int, 'busy');
    expect(await outcome, isA<EngineException>());
    return request;
  }

  Map<String, Object?> argsOf(Map<String, Object?> request) =>
      (request['args'] as Map<String, Object?>?) ?? const {};

  /// Makes [call], answers it with the example [result] and returns what the
  /// call returned.
  Future<T> answered<T>(Future<T> Function() call, Object? result) async {
    final future = call();
    await pumpEventQueue();
    transport.connection.respond(transport.connection.lastId, result! as Map<String, Object?>);
    return future;
  }

  group('each method sends the documented command and exactly the documented arguments', () {
    test('hello, list_disks, close_source, catalog_info, job_cancel, shutdown take none', () async {
      final calls = <String, Future<Object?> Function()>{
        'hello': api.hello,
        'list_disks': api.listDisks,
        'close_source': api.closeSource,
        'catalog_info': api.catalogInfo,
        'job_cancel': api.jobCancel,
        'shutdown': api.shutdown,
      };
      for (final MapEntry(key: command, value: call) in calls.entries) {
        final request = await requestOf(call);
        expect(request['cmd'], command);
        expect(request.containsKey('args'), isFalse, reason: command);
      }
    });

    test('open_source', () async {
      final request = await requestOf(
        () => api.openSource(kind: SourceKind.image, path: r'D:\disk.img', sectorSize: 4096),
      );
      expect(request['cmd'], 'open_source');
      expect(argsOf(request), {'kind': 'image', 'path': r'D:\disk.img', 'sector_size': 4096});
      expect(argsOf(request).keys.toSet(), examples.object('open_source.args').keys.toSet());

      final defaults = await requestOf(() => api.openSource(kind: SourceKind.disk, path: r'\\.\PhysicalDrive1'));
      expect(argsOf(defaults), {'kind': 'disk', 'path': r'\\.\PhysicalDrive1'});
      expect(() => api.openSource(kind: SourceKind.unknown, path: 'x'), throwsArgumentError);
    });

    test('scan_start', () async {
      final request = await requestOf(
        () => api.scanStart(
          start: 1048576,
          end: 4000786153472,
          sessionPath: r'C:\s.hdsc',
          resume: true,
          collectCarveHits: false,
          allowSameDisk: true,
          allowUnknownDisk: true,
        ),
      );
      expect(request['cmd'], 'scan_start');
      expect(argsOf(request), {
        'start': 1048576,
        'end': 4000786153472,
        'session_path': r'C:\s.hdsc',
        'resume': true,
        'collect_carve_hits': false,
        'allow_same_disk': true,
        'allow_unknown_disk': true,
      });
      expect(argsOf(request).keys.toSet(), examples.object('scan_start.args').keys.toSet());

      // A resumed scan must be able to leave range and options to the session.
      final resume = await requestOf(() => api.scanStart(resume: true));
      expect(argsOf(resume), {'resume': true, 'allow_same_disk': false, 'allow_unknown_disk': false});
    });

    test('session_save, session_load, session_attach_source', () async {
      final save = await requestOf(() => api.sessionSave(path: 'p', allowSameDisk: true, allowUnknownDisk: true));
      expect(save['cmd'], 'session_save');
      expect(argsOf(save), {'path': 'p', 'allow_same_disk': true, 'allow_unknown_disk': true});
      expect(argsOf(save).keys.toSet(), examples.object('session_save.args').keys.toSet());

      final load = await requestOf(
        () => api.sessionLoad(path: 'p', source: const SourceRef(kind: SourceKind.disk, path: r'\\.\PhysicalDrive2')),
      );
      expect(load['cmd'], 'session_load');
      expect(argsOf(load), {
        'path': 'p',
        'source': {'kind': 'disk', 'path': r'\\.\PhysicalDrive2'},
      });
      expect(argsOf(load).keys.toSet(), examples.object('session_load.args').keys.toSet());
      expect(
        (argsOf(load)['source']! as Map<String, Object?>).keys.toSet(),
        examples.object('session_load.args.source').keys.toSet(),
      );
      expect(argsOf(await requestOf(() => api.sessionLoad(path: 'p'))), {'path': 'p'});

      final attach = await requestOf(() => api.sessionAttachSource(kind: SourceKind.image, path: 'i', force: true));
      expect(attach['cmd'], 'session_attach_source');
      expect(argsOf(attach), {'kind': 'image', 'path': 'i', 'force': true});
      expect(argsOf(attach).keys.toSet(), examples.object('session_attach_source.args').keys.toSet());
    });

    test('geometry_set', () async {
      final request = await requestOf(() => api.geometrySet(instance: 2, clusterSize: 8192, volumeOffset: 32256));
      expect(request['cmd'], 'geometry_set');
      expect(argsOf(request), {'instance': 2, 'cluster_size': 8192, 'volume_offset': 32256});
      expect(argsOf(request).keys.toSet(), examples.object('geometry_set.args').keys.toSet());
    });

    test('list_children', () async {
      final request = await requestOf(
        () => api.listChildren(
          epoch: 3,
          node: 17,
          offset: 400,
          limit: 1000,
          sort: SortKey.modified,
          desc: true,
          dirsOnly: true,
          showDeleted: true,
          exts: ['jpg', 'png'],
          ratings: [Rating.good, Rating.uncertain],
        ),
      );
      expect(request['cmd'], 'list_children');
      expect(argsOf(request), {
        'epoch': 3,
        'node': 17,
        'offset': 400,
        'limit': 1000,
        'sort': 'modified',
        'desc': true,
        'dirs_only': true,
        'show_deleted': true,
        'exts': ['jpg', 'png'],
        'ratings': ['good', 'uncertain'],
      });
      expect(argsOf(request).keys.toSet(), examples.object('list_children.args').keys.toSet());

      // The defaults are the document's defaults.
      final defaults = argsOf(await requestOf(() => api.listChildren(epoch: 3, node: 0)));
      final documented = examples.object('list_children.args');
      for (final key in defaults.keys) {
        expect(defaults[key], documented[key], reason: key);
      }
      expect(defaults.containsKey('exts'), isFalse, reason: 'null means any');
      expect(defaults.containsKey('ratings'), isFalse);
    });

    test('list_children and search refuse what the engine would refuse', () {
      expect(() => api.listChildren(epoch: 1, node: 0, limit: 1001), throwsRangeError);
      expect(() => api.listChildren(epoch: 1, node: 0, limit: 0), throwsRangeError);
      expect(() => api.listChildren(epoch: 1, node: 0, offset: -1), throwsRangeError);
      expect(() => api.listChildren(epoch: 1, node: 0, exts: []), throwsArgumentError);
      expect(() => api.listChildren(epoch: 1, node: 0, ratings: []), throwsArgumentError);
      expect(() => api.listChildren(epoch: 1, node: 0, ratings: [Rating.unknown]), throwsArgumentError);
      expect(() => api.search(epoch: 1, limit: 1001), throwsRangeError);
      expect(() => api.search(epoch: 1, filter: const FileFilter(exts: [])), throwsArgumentError);
      expect(() => api.carveStart(kinds: []), throwsArgumentError);
      expect(() => api.readFile(epoch: 1, node: 1, length: maxReadFileLength + 1), throwsRangeError);
    });

    test('node_info', () async {
      final request = await requestOf(() => api.nodeInfo(epoch: 3, node: 4711));
      expect(request['cmd'], 'node_info');
      expect(argsOf(request), examples.object('node_info.args'));
    });

    test('search and Filter', () async {
      const filter = FileFilter(
        query: 'img_',
        exts: ['jpg', 'png'],
        minSize: 1,
        maxSize: 1 << 40,
        ratings: [Rating.good, Rating.uncertain],
      );
      final request = await requestOf(
        () => api.search(
          epoch: 3,
          under: 17,
          filter: filter,
          showDeleted: true,
          sort: SortKey.size,
          desc: true,
          offset: 200,
          limit: 100,
        ),
      );
      expect(request['cmd'], 'search');
      expect(argsOf(request), {
        'epoch': 3,
        'under': 17,
        'filter': {
          'query': 'img_',
          'exts': ['jpg', 'png'],
          'min_size': 1,
          'max_size': 1 << 40,
          'ratings': ['good', 'uncertain'],
        },
        'show_deleted': true,
        'sort': 'size',
        'desc': true,
        'offset': 200,
        'limit': 100,
      });
      expect(argsOf(request).keys.toSet(), examples.object('search.args').keys.toSet());
      expect(filter.toJson().keys.toSet(), examples.object('Filter').keys.toSet());
      // The document's Filter example, member by member.
      final documented = examples.object('Filter');
      expect(const FileFilter(query: 'img_', exts: ['jpg', 'png'], ratings: [Rating.good, Rating.uncertain]).toJson(), {
        for (final entry in documented.entries)
          if (entry.value != null) entry.key: entry.value,
      });

      // Without a filter the member is still sent (it has no default), empty.
      final plain = argsOf(await requestOf(() => api.search(epoch: 3)));
      expect(plain['filter'], isEmpty);
      expect(plain.containsKey('under'), isFalse, reason: 'default: the main root');
    });

    test('selection_summary', () async {
      final request = await requestOf(
        () => api.selectionSummary(
          epoch: 3,
          include: [17, 99],
          exclude: [4711],
          filter: const FileFilter(exts: ['jpg']),
          includeDeleted: true,
          skipDamaged: true,
        ),
      );
      expect(request['cmd'], 'selection_summary');
      expect(argsOf(request), {
        'epoch': 3,
        'include': [17, 99],
        'exclude': [4711],
        'filter': {
          'exts': ['jpg'],
        },
        'include_deleted': true,
        'skip_damaged': true,
      });
      expect(argsOf(request).keys.toSet(), examples.object('selection_summary.args').keys.toSet());

      // With the defaults the request equals the document's example (a null
      // filter is left out).
      final defaults = argsOf(
        await requestOf(() => api.selectionSummary(epoch: 3, include: [17, 99], exclude: [4711])),
      );
      expect(defaults, {
        for (final entry in examples.object('selection_summary.args').entries)
          if (entry.value != null) entry.key: entry.value,
      });
    });

    test('check_destination', () async {
      final request = await requestOf(
        () => api.checkDestination(path: r'D:\Rescued', purpose: DestinationPurpose.appData, needBytes: 5),
      );
      expect(request['cmd'], 'check_destination');
      expect(argsOf(request), {'path': r'D:\Rescued', 'purpose': 'app_data', 'need_bytes': 5});
      expect(argsOf(request).keys.toSet(), examples.object('check_destination.args').keys.toSet());
      expect(
        argsOf(await requestOf(() => api.checkDestination(path: r'D:\Rescued', purpose: DestinationPurpose.recover))),
        {
          for (final entry in examples.object('check_destination.args').entries)
            if (entry.value != null) entry.key: entry.value,
        },
      );
      expect(DestinationPurpose.values.map((p) => p.wire), ['recover', 'image', 'session', 'app_data']);
    });

    test('recover_start', () async {
      final request = await requestOf(
        () => api.recoverStart(
          epoch: 3,
          include: [17, 99],
          exclude: [4711],
          filter: const FileFilter(query: 'x'),
          dest: r'D:\Rescued',
          includeDeleted: true,
          skipDamaged: true,
          allowSameDisk: true,
          allowUnknownDisk: true,
          onExists: OnExists.overwrite,
        ),
      );
      expect(request['cmd'], 'recover_start');
      expect(argsOf(request), {
        'epoch': 3,
        'include': [17, 99],
        'exclude': [4711],
        'filter': {'query': 'x'},
        'dest': r'D:\Rescued',
        'include_deleted': true,
        'skip_damaged': true,
        'allow_same_disk': true,
        'allow_unknown_disk': true,
        'on_exists': 'overwrite',
      });
      expect(argsOf(request).keys.toSet(), examples.object('recover_start.args').keys.toSet());

      final defaults = argsOf(
        await requestOf(() => api.recoverStart(epoch: 3, include: [17, 99], exclude: [4711], dest: r'D:\Rescued')),
      );
      expect(defaults, {
        for (final entry in examples.object('recover_start.args').entries)
          if (entry.value != null) entry.key: entry.value,
      });
      expect(OnExists.values.map((v) => v.wire), ['rename', 'skip', 'overwrite']);
    });

    test('carve_start', () async {
      final request = await requestOf(() => api.carveStart(kinds: ['jpeg', 'png']));
      expect(request['cmd'], 'carve_start');
      expect(argsOf(request), {
        'kinds': ['jpeg', 'png'],
      });
      expect(argsOf(request).keys.toSet(), examples.object('carve_start.args').keys.toSet());
      expect(argsOf(await requestOf(api.carveStart)), isEmpty, reason: 'null means all kinds');
    });

    test('image_start', () async {
      final request = await requestOf(
        () => api.imageStart(
          start: 512,
          end: 1024,
          dest: r'D:\disk1.img',
          resume: true,
          allowSameDisk: true,
          allowUnknownDisk: true,
        ),
      );
      expect(request['cmd'], 'image_start');
      expect(argsOf(request), {
        'start': 512,
        'end': 1024,
        'dest': r'D:\disk1.img',
        'resume': true,
        'allow_same_disk': true,
        'allow_unknown_disk': true,
      });
      expect(argsOf(request).keys.toSet(), examples.object('image_start.args').keys.toSet());
      expect(argsOf(await requestOf(() => api.imageStart(dest: r'D:\disk1.img'))), {
        for (final entry in examples.object('image_start.args').entries)
          if (entry.value != null) entry.key: entry.value,
      });
    });

    test('read_file', () async {
      final request = await requestOf(() => api.readFile(epoch: 3, node: 4711));
      expect(request['cmd'], 'read_file');
      expect(argsOf(request), examples.object('read_file.args'));
    });

    test('job_cancel jumps the queue; ordinary requests do not', () async {
      final slow = api.hello().then<Object?>((v) => v, onError: (Object e) => e);
      final queued = api.listDisks().then<Object?>((v) => v, onError: (Object e) => e);
      final cancel = api.jobCancel();
      await pumpEventQueue();
      expect(transport.connection.requests.map((r) => r['cmd']), ['hello', 'job_cancel']);
      transport.connection.respond(transport.connection.requests[1]['id']! as int);
      await cancel;
      transport.connection.exit(0);
      await slow;
      await queued;
    });
  });

  group('answers', () {
    test('every method returns its documented result', () async {
      expect((await answered(api.hello, examples.json('hello.result'))).engineVersion, '0.1.0');
      expect((await answered(api.listDisks, examples.json('list_disks.result'))).single.model, 'WDC WD40EZRZ-00GXCB0');
      expect(
        (await answered(
          () => api.openSource(kind: SourceKind.disk, path: 'x'),
          examples.json('open_source.result'),
        )).partitions.single.letters,
        ['E:'],
      );
      await answered(api.closeSource, examples.json('close_source.result'));
      expect((await answered(api.scanStart, examples.json('scan_start.result'))).bytesTotal, 4000785104896);
      await answered(api.jobCancel, examples.json('job_cancel.result'));
      expect((await answered(() => api.sessionSave(path: 'p'), examples.json('session_save.result'))).bytes, 12345678);
      expect((await answered(() => api.sessionLoad(path: 'p'), examples.json('session_load.result'))).catalog.epoch, 3);
      expect(
        (await answered(
          () => api.sessionAttachSource(kind: SourceKind.disk, path: 'p'),
          examples.json('session_attach_source.result'),
        )).catalog.nodes,
        812345,
      );
      expect((await answered(api.catalogInfo, examples.json('CatalogInfo'))).carveHits, 48213);
      expect(
        (await answered(
          () => api.geometrySet(instance: 0, clusterSize: 4096, volumeOffset: 0),
          examples.json('CatalogInfo'),
        )).epoch,
        3,
      );
      expect(
        (await answered(() => api.listChildren(epoch: 3, node: 0), examples.json('list_children.result'))).total,
        5321,
      );
      expect((await answered(() => api.nodeInfo(epoch: 3, node: 1), examples.json('node_info.result'))).path, 'Photos/IMG_0042.JPG');
      expect((await answered(() => api.search(epoch: 3), examples.json('search.result'))).rows.single.ancestorIds, [0, 17]);
      expect(
        (await answered(
          () => api.selectionSummary(epoch: 3, include: [1], exclude: []),
          examples.json('selection_summary.result'),
        )).files,
        5320,
      );
      expect(
        (await answered(
          () => api.checkDestination(path: 'p', purpose: DestinationPurpose.recover),
          examples.json('check_destination.result'),
        )).verdict,
        DestinationVerdict.ok,
      );
      expect(
        (await answered(
          () => api.recoverStart(epoch: 3, include: [1], exclude: [], dest: 'd'),
          examples.json('recover_start.result'),
        )).files,
        5320,
      );
      expect((await answered(api.carveStart, examples.json('carve_start.result'))).candidates, 48213);
      expect((await answered(() => api.imageStart(dest: 'd'), examples.json('image_start.result'))).bytes, 4000787030016);
      expect(
        utf8.decode((await answered(() => api.readFile(epoch: 3, node: 1), examples.json('read_file.result'))).bytes),
        'HDSavior',
      );
      await answered(api.shutdown, examples.json('shutdown.result'));
    });

    test('a result with a missing member is a protocol_error that names it', () async {
      final broken = examples.object('list_children.result');
      ((broken['rows']! as List<Object?>).single! as Map<String, Object?>).remove('size');
      final future = api.listChildren(epoch: 3, node: 0);
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, broken);
      await expectLater(
        future,
        throwsA(
          isA<EngineException>()
              .having((e) => e.kind, 'kind', EngineErrorCode.protocolError)
              .having((e) => e.command, 'command', 'list_children')
              .having((e) => e.message, 'message', contains('list_children.rows[0].size: is missing')),
        ),
      );
    });

    test('invalid base64 in read_file is a protocol_error', () async {
      final future = api.readFile(epoch: 3, node: 1);
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, {'data_b64': '...', 'size': 1, 'eof': true, 'kind': null});
      await expectLater(
        future,
        throwsA(isA<EngineException>().having((e) => e.kind, 'kind', EngineErrorCode.protocolError)),
      );
    });
  });

  group('readFileContent', () {
    /// Serves read_file requests from [content] until the call completes.
    Future<FileContent> serve(Uint8List content, Future<FileContent> call, {List<int>? lengths}) async {
      var done = false;
      final result = call.whenComplete(() => done = true);
      var answeredRequests = 0;
      while (!done) {
        await pumpEventQueue(times: 3);
        final requests = transport.connection.requests;
        if (requests.length == answeredRequests) continue;
        final request = requests[answeredRequests++];
        expect(request['cmd'], 'read_file');
        final args = request['args']! as Map<String, Object?>;
        final offset = args['offset']! as int;
        final length = args['length']! as int;
        lengths?.add(length);
        final end = offset + length > content.length ? content.length : offset + length;
        final part = offset >= content.length ? Uint8List(0) : Uint8List.sublistView(content, offset, end);
        transport.connection.respond(request['id']! as int, {
          'data_b64': base64.encode(part),
          'size': content.length,
          'eof': end >= content.length,
          'kind': offset == 0 ? 'jpeg' : null,
        });
      }
      return result;
    }

    final content = Uint8List.fromList(List.generate(1000, (i) => i % 251));

    test('reads a whole file in chunks', () async {
      final lengths = <int>[];
      final result = await serve(
        content,
        api.readFileContent(epoch: 1, node: 2, maxBytes: 5000, chunkSize: 300),
        lengths: lengths,
      );
      expect(result.bytes, content);
      expect(result.complete, isTrue);
      expect(result.size, 1000);
      expect(result.kind, 'jpeg');
      expect(lengths, [300, 300, 300, 300]);
    });

    test('stops at maxBytes and says that the file is longer', () async {
      final lengths = <int>[];
      final result = await serve(
        content,
        api.readFileContent(epoch: 1, node: 2, maxBytes: 700, chunkSize: 300),
        lengths: lengths,
      );
      expect(result.bytes, content.sublist(0, 700));
      expect(result.complete, isFalse);
      expect(result.size, 1000);
      expect(lengths, [300, 300, 100]);
    });

    test('a file that ends exactly at maxBytes is complete', () async {
      final result = await serve(content, api.readFileContent(epoch: 1, node: 2, maxBytes: 1000, chunkSize: 400));
      expect(result.bytes, content);
      expect(result.complete, isTrue);
    });

    test('an empty file', () async {
      final result = await serve(Uint8List(0), api.readFileContent(epoch: 1, node: 2, maxBytes: 100));
      expect(result.bytes, isEmpty);
      expect(result.complete, isTrue);
      expect(result.size, 0);
    });

    test('cancelling between chunks stops the reading', () async {
      final token = CancelToken();
      final call = api.readFileContent(epoch: 1, node: 2, maxBytes: 5000, chunkSize: 100, cancel: token);
      final outcome = call.then<Object?>((v) => v, onError: (Object e) => e);
      await pumpEventQueue();
      final first = transport.connection.lastRequest;
      transport.connection.respond(first['id']! as int, {
        'data_b64': base64.encode(content.sublist(0, 100)),
        'size': 1000,
        'eof': false,
        'kind': null,
      });
      await pumpEventQueue();
      token.cancel();
      expect(await outcome, isA<RequestCancelledException>());
      final sent = transport.connection.requests.length;
      await pumpEventQueue();
      expect(transport.connection.requests, hasLength(sent), reason: 'no further chunks are requested');
    });

    test('arguments are checked', () {
      expect(() => api.readFileContent(epoch: 1, node: 2, maxBytes: 0), throwsRangeError);
      expect(
        () => api.readFileContent(epoch: 1, node: 2, maxBytes: 10, chunkSize: maxReadFileLength + 1),
        throwsRangeError,
      );
    });
  });
}
