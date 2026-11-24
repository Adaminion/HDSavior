// The EngineClient against a scripted engine: framing, matching, events,
// errors, cancellation, engine exit, restart and shutdown.
import 'dart:async';
import 'dart:convert';
import 'dart:math';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

import '../support/scripted_transport.dart';
import 'protocol_examples.dart';

const _settle = Duration(milliseconds: 30);

TypeMatcher<EngineException> engineError(EngineErrorCode kind, {Object? message}) => isA<EngineException>()
    .having((e) => e.kind, 'kind', kind)
    .having((e) => e.code, 'code', kind.wire)
    .having((e) => e.message, 'message', message ?? anything);

void main() {
  late ScriptedTransport transport;
  late EngineClient client;
  late List<String> protocolErrors;

  Future<void> startClient({int maxInFlight = 1}) async {
    transport = ScriptedTransport();
    client = EngineClient(transport, maxInFlight: maxInFlight, settleTime: _settle);
    protocolErrors = [];
    client.protocolErrors.listen(protocolErrors.add);
    await client.start();
  }

  ScriptedConnection engine() => transport.connection;

  tearDown(() async {
    await client.dispose();
  });

  group('requests', () {
    test('a request is one JSON line with id, cmd and args', () async {
      await startClient();
      unawaited(client.request('list_children', args: {'node': 12, 'epoch': 3}).catchError((_) => const <String, Object?>{}));
      await pumpEventQueue();
      expect(engine().writtenText, endsWith('\n'));
      expect('\n'.allMatches(engine().writtenText), hasLength(1));
      // The shape of the document's example (the id is the client's choice).
      final example = ProtocolExamples.load().object('transport.request');
      expect(engine().lastRequest.keys.toSet(), example.keys.toSet());
      expect(engine().lastRequest['id'], isA<int>());
      expect(engine().lastRequest['cmd'], 'list_children');
      expect(engine().lastRequest['args'], example['args']);
    });

    test('args are omitted when there are none', () async {
      await startClient();
      unawaited(client.request('hello').catchError((_) => const <String, Object?>{}));
      await pumpEventQueue();
      expect(engine().lastRequest.keys, ['id', 'cmd']);
    });

    test('newlines inside strings do not break the line', () async {
      await startClient();
      unawaited(client.request('open_source', args: {'path': 'a\nb\r\nc'}).catchError((_) => const <String, Object?>{}));
      await pumpEventQueue();
      expect('\n'.allMatches(engine().writtenText), hasLength(1));
      expect((engine().lastRequest['args']! as Map)['path'], 'a\nb\r\nc');
    });

    test('ids are unique and responses are matched by id, in any order', () async {
      await startClient(maxInFlight: 3);
      final a = client.request('a');
      final b = client.request('b');
      final c = client.request('c');
      await pumpEventQueue();
      final ids = [for (final r in engine().requests) r['id']! as int];
      expect(ids.toSet(), hasLength(3));
      engine().respond(ids[2], {'who': 'c'});
      engine().respond(ids[0], {'who': 'a'});
      engine().respond(ids[1], {'who': 'b'});
      expect(await a, {'who': 'a'});
      expect(await b, {'who': 'b'});
      expect(await c, {'who': 'c'});
      expect(protocolErrors, isEmpty);
    });

    test('the documented ok response completes the request', () async {
      await startClient();
      final examples = ProtocolExamples.load();
      final future = client.request('list_children', args: {'node': 12, 'epoch': 3});
      await pumpEventQueue();
      final line = examples.object('transport.response_ok')..['id'] = engine().lastId;
      engine().emitLine(line);
      expect(await future, isEmpty);
    });

    test('a missing result counts as an empty object', () async {
      await startClient();
      final future = client.request('job_cancel');
      await pumpEventQueue();
      engine().emitLine({'id': engine().lastId, 'ok': true});
      expect(await future, isEmpty);
    });

    test('requests are refused while no engine runs', () async {
      transport = ScriptedTransport();
      client = EngineClient(transport, settleTime: _settle);
      await expectLater(client.request('hello'), throwsA(engineError(EngineErrorCode.engineNotRunning)));
    });

    test('arguments that cannot be encoded fail the request', () async {
      await startClient();
      await expectLater(client.request('x', args: {'bad': Object()}), throwsArgumentError);
    });
  });

  group('one request at the engine at a time', () {
    test('later requests wait in the client until the answer is in', () async {
      await startClient();
      final a = client.request('a');
      final b = client.request('b');
      final c = client.request('c');
      await pumpEventQueue();
      expect(engine().requests.map((r) => r['cmd']), ['a']);
      expect(client.inFlightCount, 1);
      expect(client.queuedCount, 2);

      engine().respond(engine().lastId);
      await a;
      await pumpEventQueue();
      expect(engine().requests.map((r) => r['cmd']), ['a', 'b']);
      engine().respond(engine().lastId);
      await b;
      await pumpEventQueue();
      engine().respond(engine().lastId);
      await c;
      expect(engine().requests.map((r) => r['cmd']), ['a', 'b', 'c']);
      expect(client.inFlightCount, 0);
      expect(client.queuedCount, 0);
    });

    test('an urgent request is written at once', () async {
      await startClient();
      unawaited(client.request('slow').catchError((_) => const <String, Object?>{}));
      unawaited(client.request('queued').catchError((_) => const <String, Object?>{}));
      final cancel = client.request('job_cancel', urgent: true);
      await pumpEventQueue();
      expect(engine().requests.map((r) => r['cmd']), ['slow', 'job_cancel']);
      engine().respond(engine().requests[1]['id']! as int);
      expect(await cancel, isEmpty);
    });
  });

  group('framing', () {
    final line = jsonEncode({
      'id': 1,
      'ok': true,
      'result': {'name': 'Ünïcödé 写真 � 😀.jpg', 'size': 9223372036854775807},
    });

    test('a line split at every possible byte position, including inside a character', () async {
      final bytes = utf8.encode('$line\n');
      for (var cut = 1; cut < bytes.length; cut++) {
        await startClient();
        final future = client.request('x');
        await pumpEventQueue();
        final id = engine().lastId;
        final own = utf8.encode('${line.replaceFirst('"id":1', '"id":$id')}\n');
        engine().emitBytes(own.sublist(0, cut));
        await pumpEventQueue(times: 2);
        engine().emitBytes(own.sublist(cut));
        final result = await future;
        expect(result['name'], 'Ünïcödé 写真 � 😀.jpg', reason: 'cut at $cut');
        expect(result['size'], 9223372036854775807);
        expect(protocolErrors, isEmpty);
        await client.dispose();
      }
    });

    test('a line delivered one byte at a time', () async {
      await startClient();
      final future = client.request('x');
      await pumpEventQueue();
      final own = utf8.encode('${line.replaceFirst('"id":1', '"id":${engine().lastId}')}\n');
      for (final byte in own) {
        engine().emitBytes([byte]);
      }
      expect((await future)['name'], 'Ünïcödé 写真 � 😀.jpg');
    });

    test('several lines merged into one chunk, with events in between', () async {
      await startClient(maxInFlight: 2);
      final events = <EngineEvent>[];
      client.events.listen(events.add);
      final a = client.request('a');
      final b = client.request('b');
      await pumpEventQueue();
      final ids = [for (final r in engine().requests) r['id']! as int];
      engine().emit(
        [
          jsonEncode({'event': 'log', 'data': {'level': 'info', 'message': 'one'}}),
          jsonEncode({'id': ids[0], 'ok': true, 'result': {'n': 1}}),
          jsonEncode({'event': 'log', 'data': {'level': 'warn', 'message': 'two'}}),
          jsonEncode({'id': ids[1], 'ok': true, 'result': {'n': 2}}),
          '',
        ].join('\n'),
      );
      expect(await a, {'n': 1});
      expect(await b, {'n': 2});
      await pumpEventQueue();
      expect(events.cast<LogEvent>().map((e) => e.message), ['one', 'two']);
    });

    test('carriage returns and empty lines are tolerated', () async {
      await startClient();
      final future = client.request('x');
      await pumpEventQueue();
      engine().emit('\r\n\n${jsonEncode({'id': engine().lastId, 'ok': true, 'result': {'n': 1}})}\r\n\r\n');
      expect(await future, {'n': 1});
      expect(protocolErrors, isEmpty);
    });

    test('a very large line (read_file of 8 MiB), in one chunk and in pipe-sized chunks', () async {
      final random = Random(7);
      final data = Uint8List.fromList(List.generate(maxReadFileLength, (_) => random.nextInt(256)));
      final payload = base64.encode(data);
      for (final chunkSize in [0, 65536, 4096]) {
        await startClient();
        final future = client.request('read_file');
        await pumpEventQueue();
        final bytes = utf8.encode(
          '${jsonEncode({'id': engine().lastId, 'ok': true, 'result': {'data_b64': payload, 'size': data.length, 'eof': true, 'kind': null}})}\n',
        );
        expect(bytes.length, greaterThan(11000000)); // more than 11 MB in one line
        final watch = Stopwatch()..start();
        if (chunkSize == 0) {
          engine().emitBytes(bytes);
        } else {
          for (var i = 0; i < bytes.length; i += chunkSize) {
            engine().emitBytes(Uint8List.sublistView(bytes, i, min(i + chunkSize, bytes.length)));
          }
        }
        final result = ReadFileResult.fromJson(JsonObject.root(await future, 'read_file'));
        expect(result.bytes, data, reason: 'chunks of $chunkSize');
        // Reassembling a line from thousands of chunks must stay linear.
        expect(watch.elapsed, lessThan(const Duration(seconds: 20)), reason: 'chunks of $chunkSize');
        await client.dispose();
      }
    });

    test('invalid UTF-8 does not end the conversation', () async {
      await startClient();
      final first = client.request('a');
      await pumpEventQueue();
      engine().emitBytes([0xFF, 0xFE, 0x0A]);
      engine().respond(engine().lastId, {'ok': 1});
      expect(await first, {'ok': 1});
      expect(protocolErrors, hasLength(1));
    });

    test('invalid UTF-8 costs only the line it is in, even inside one chunk', () async {
      await startClient(maxInFlight: 2);
      final a = client.request('a');
      final b = client.request('b');
      await pumpEventQueue();
      final ids = [for (final r in engine().requests) r['id']! as int];
      // A good line, a line of garbage, and a line whose string value holds a
      // stray byte (a file name the engine failed to clean), all at once.
      engine().emitBytes([
        ...utf8.encode('${jsonEncode({'id': ids[0], 'ok': true, 'result': {'n': 1}})}\n'),
        0xC3, 0x28, 0xFF, 0x0A,
        ...utf8.encode('{"id": ${ids[1]}, "ok": true, "result": {"name": "bad'),
        0xFF,
        ...utf8.encode('name.jpg"}}\n'),
      ]);
      expect(await a, {'n': 1});
      expect(await b, {'name': 'bad\uFFFDname.jpg'});
      expect(protocolErrors, hasLength(1));
    });
  });

  group('events', () {
    test('events and responses are delivered in the order of the lines', () async {
      await startClient();
      final log = <String>[];
      client.events.listen((event) => log.add('event ${event.name}'));
      final start = client.request('scan_start').then((value) {
        log.add('response');
        return value;
      });
      await pumpEventQueue();
      final examples = ProtocolExamples.load();
      // The response to X_start is written before any event of the job.
      engine().emit(
        '${jsonEncode({'id': engine().lastId, 'ok': true, 'result': examples.json('scan_start.result')})}\n'
        '${jsonEncode({'event': 'scan_progress', 'data': examples.json('scan_progress')})}\n'
        '${jsonEncode({'event': 'scan_done', 'data': examples.json('scan_done')})}\n',
      );
      await start;
      await pumpEventQueue();
      expect(log, ['response', 'event scan_progress', 'event scan_done']);
    });

    test('typed streams', () async {
      await startClient();
      final examples = ProtocolExamples.load();
      final progress = <ScanProgress>[];
      final done = <JobDoneEvent>[];
      final tasks = <TaskProgress>[];
      client.on<ScanProgress>().listen(progress.add);
      client.on<JobDoneEvent>().listen(done.add);
      client.on<TaskProgress>().listen(tasks.add);
      engine().event('scan_progress', examples.json('scan_progress'));
      engine().event('task_progress', examples.json('task_progress'));
      engine().event('recover_done', examples.json('recover_done'));
      engine().event('scan_done', examples.json('scan_done'));
      await pumpEventQueue();
      expect(progress.single.phase, ScanPhase.mftProbe);
      expect(tasks.single.fraction, 0.42);
      expect(done.map((e) => e.job), [JobKind.recover, JobKind.scan]);
    });

    test('the documented event line with empty data is a malformed scan_progress', () async {
      await startClient();
      final events = <EngineEvent>[];
      client.events.listen(events.add);
      engine().emitLine(ProtocolExamples.load().object('transport.event'));
      await pumpEventQueue();
      expect(events.single, isA<MalformedEvent>().having((e) => e.name, 'name', 'scan_progress'));
      expect(protocolErrors.single, contains('scan_progress'));
    });

    test('an unknown event is passed on as such and is no protocol error', () async {
      await startClient();
      final events = <EngineEvent>[];
      client.events.listen(events.add);
      engine().event('future_event', {'x': 1});
      await pumpEventQueue();
      expect(events.single, isA<UnknownEvent>().having((e) => e.name, 'name', 'future_event').having((e) => e.data, 'data', {'x': 1}));
      expect(protocolErrors, isEmpty);
    });

    test('a malformed done event names its job', () async {
      await startClient();
      final events = <EngineEvent>[];
      client.events.listen(events.add);
      engine().event('recover_done', {'status': 'completed'});
      await pumpEventQueue();
      expect(events.single, isA<MalformedEvent>().having((e) => e.doneOf, 'doneOf', JobKind.recover));
    });
  });

  group('errors', () {
    test('the documented error response becomes an EngineException with code, message and data', () async {
      await startClient();
      final future = client.request('list_children', args: {'node': 12, 'epoch': 3});
      await pumpEventQueue();
      engine().emitLine(ProtocolExamples.load().object('transport.response_error')..['id'] = engine().lastId);
      await expectLater(
        future,
        throwsA(
          isA<EngineException>()
              .having((e) => e.code, 'code', 'not_found')
              .having((e) => e.kind, 'kind', EngineErrorCode.notFound)
              .having((e) => e.message, 'message', 'node 12 does not exist')
              .having((e) => e.data, 'data', isNull)
              .having((e) => e.command, 'command', 'list_children')
              .having((e) => e.isEngineGone, 'isEngineGone', isFalse),
        ),
      );
    });

    test('destination_refused carries the check_destination result', () async {
      await startClient();
      final future = client.request('recover_start');
      await pumpEventQueue();
      final check = ProtocolExamples.load().object('check_destination.result')
        ..['verdict'] = 'same_disk'
        ..['message'] = 'This location is on the same physical disk.';
      engine().respondError(engine().lastId, 'destination_refused', message: 'refused', data: check);
      final error = await future.then<Object?>((_) => null, onError: (Object e) => e) as EngineException;
      expect(error.kind, EngineErrorCode.destinationRefused);
      expect(error.data, check);
      expect(error.destinationCheck!.verdict, DestinationVerdict.sameDisk);
      expect(error.destinationCheck!.needsSameDiskConsent, isTrue);
      expect(error.destinationCheck!.message, 'This location is on the same physical disk.');
      expect(error.destinationCheck!.freeBytes, 500000000000);
    });

    test('every documented error code maps to its enum value', () async {
      const codes = {
        'bad_request': EngineErrorCode.badRequest,
        'unknown_command': EngineErrorCode.unknownCommand,
        'busy': EngineErrorCode.busy,
        'no_source': EngineErrorCode.noSource,
        'no_session': EngineErrorCode.noSession,
        'no_catalog': EngineErrorCode.noCatalog,
        'stale_catalog': EngineErrorCode.staleCatalog,
        'not_found': EngineErrorCode.notFound,
        'io_error': EngineErrorCode.ioError,
        'access_denied': EngineErrorCode.accessDenied,
        'destination_refused': EngineErrorCode.destinationRefused,
        'source_mismatch': EngineErrorCode.sourceMismatch,
        'source_lost': EngineErrorCode.sourceLost,
        'no_space': EngineErrorCode.noSpace,
        'exists': EngineErrorCode.exists,
        'unsupported': EngineErrorCode.unsupported,
        'cancelled': EngineErrorCode.cancelled,
        'internal': EngineErrorCode.internal,
      };
      await startClient();
      for (final MapEntry(key: code, value: kind) in codes.entries) {
        final future = client.request('x');
        await pumpEventQueue();
        engine().respondError(engine().lastId, code, message: 'm');
        await expectLater(future, throwsA(engineError(kind)));
      }
    });

    test('an unknown error code keeps its text', () async {
      await startClient();
      final future = client.request('x');
      await pumpEventQueue();
      engine().respondError(engine().lastId, 'disk_on_fire', message: 'hot', data: {'degrees': 451});
      await expectLater(
        future,
        throwsA(
          isA<EngineException>()
              .having((e) => e.kind, 'kind', EngineErrorCode.unknown)
              .having((e) => e.code, 'code', 'disk_on_fire')
              .having((e) => e.message, 'message', 'hot')
              .having((e) => e.data, 'data', {'degrees': 451}),
        ),
      );
    });

    test('unreadable responses fail the request with protocol_error', () async {
      await startClient();
      final unreadable = <Map<String, Object?>>[
        {'ok': true, 'result': 5},
        {'ok': true, 'result': []},
        {'ok': false},
        {'ok': false, 'error': 'nope'},
        {'ok': false, 'error': {'message': 'no code'}},
        {'result': <String, Object?>{}},
        {'ok': 'yes', 'result': <String, Object?>{}},
      ];
      for (final body in unreadable) {
        final future = client.request('x');
        await pumpEventQueue();
        engine().emitLine({'id': engine().lastId, ...body});
        await expectLater(future, throwsA(engineError(EngineErrorCode.protocolError)), reason: '$body');
      }
      expect(protocolErrors, hasLength(unreadable.length));
    });

    test('lines that answer nothing are reported and otherwise ignored', () async {
      await startClient();
      final future = client.request('x');
      await pumpEventQueue();
      engine().emit('this is not json\n');
      engine().emit('[1, 2, 3]\n');
      engine().emit('"text"\n');
      engine().emitLine({'id': null, 'ok': false, 'error': {'code': 'bad_request', 'message': 'what?', 'data': null}});
      engine().emitLine({'id': 424242, 'ok': true, 'result': <String, Object?>{}});
      engine().emitLine({'id': '7', 'ok': true, 'result': <String, Object?>{}});
      engine().respond(engine().lastId, {'fine': true});
      expect(await future, {'fine': true});
      expect(protocolErrors, hasLength(6));
    });
  });

  group('cancellation', () {
    test('a queued request is never sent', () async {
      await startClient();
      final first = client.request('first');
      final token = CancelToken();
      final second = client.request('second', cancel: token);
      await pumpEventQueue();
      token.cancel();
      await expectLater(second, throwsA(isA<RequestCancelledException>().having((e) => e.command, 'command', 'second')));
      engine().respond(engine().lastId);
      await first;
      await pumpEventQueue();
      expect(engine().requests.map((r) => r['cmd']), ['first']);
      expect(client.queuedCount, 0);
    });

    test('the answer to a cancelled request that was already sent is dropped, and its slot stays taken until then', () async {
      await startClient();
      final token = CancelToken();
      final cancelled = client.request('slow', cancel: token);
      final next = client.request('next');
      await pumpEventQueue();
      final slowId = engine().lastId;

      token.cancel();
      await expectLater(cancelled, throwsA(isA<RequestCancelledException>()));
      await pumpEventQueue();
      // The engine is still busy with it: nothing else is piled on.
      expect(engine().requests.map((r) => r['cmd']), ['slow']);

      engine().respond(slowId, {'late': true});
      await pumpEventQueue();
      expect(engine().requests.map((r) => r['cmd']), ['slow', 'next']);
      engine().respond(engine().lastId, {'n': 2});
      expect(await next, {'n': 2});
      expect(protocolErrors, isEmpty);
    });

    test('a token that is already cancelled prevents the request', () async {
      await startClient();
      final token = CancelToken()..cancel();
      await expectLater(client.request('x', cancel: token), throwsA(isA<RequestCancelledException>()));
      await pumpEventQueue();
      expect(engine().requests, isEmpty);
    });

    test('one token cancels all its requests; cancelling twice is harmless', () async {
      await startClient();
      final token = CancelToken();
      final futures = [for (var i = 0; i < 5; i++) client.request('r$i', cancel: token)];
      await pumpEventQueue();
      token.cancel();
      token.cancel();
      for (final future in futures) {
        await expectLater(future, throwsA(isA<RequestCancelledException>()));
      }
      expect(token.isCancelled, isTrue);
    });

    test('a completed request no longer listens to its token', () async {
      await startClient();
      final token = CancelToken();
      final future = client.request('x', cancel: token);
      await pumpEventQueue();
      engine().respond(engine().lastId, {'n': 1});
      expect(await future, {'n': 1});
      token.cancel(); // must not throw or complete anything twice
      await pumpEventQueue();
    });
  });

  group('engine exit', () {
    test('pending and queued requests fail with engine_exited; exits reports it', () async {
      await startClient();
      final exits = <EngineExit>[];
      client.exits.listen(exits.add);
      final states = <EngineRunState>[];
      client.states.listen(states.add);
      final inFlight = client.request('list_children');
      final queued = client.request('node_info');
      await pumpEventQueue();
      engine().emitStderr('thread main panicked at scan.rs:42\n');
      await pumpEventQueue();

      engine().exit(101);
      await expectLater(
        inFlight,
        throwsA(
          engineError(EngineErrorCode.engineExited, message: allOf(contains('unexpectedly'), contains('101')))
              .having((e) => e.command, 'command', 'list_children')
              .having((e) => e.isEngineGone, 'isEngineGone', isTrue),
        ),
      );
      await expectLater(queued, throwsA(engineError(EngineErrorCode.engineExited)));
      await pumpEventQueue();
      expect(client.state, EngineRunState.stopped);
      expect(states.last, EngineRunState.stopped);
      expect(exits.single.exitCode, 101);
      expect(exits.single.expected, isFalse);
      expect(exits.single.stderrTail, ['thread main panicked at scan.rs:42']);
      await expectLater(client.request('hello'), throwsA(engineError(EngineErrorCode.engineNotRunning)));
    });

    test('what the engine wrote before it died is still delivered', () async {
      await startClient(maxInFlight: 2);
      final answered = client.request('a');
      final lost = client.request('b');
      await pumpEventQueue();
      final ids = [for (final r in engine().requests) r['id']! as int];
      engine().respond(ids[0], {'made': 'it'});
      // Half a line, then death.
      engine().emit('{"id": ${ids[1]}, "ok": true, "res');
      engine().exit(1);
      expect(await answered, {'made': 'it'});
      await expectLater(lost, throwsA(engineError(EngineErrorCode.engineExited)));
    });

    test('an exit code without the output closing is settled after a moment', () async {
      await startClient();
      final pending = client.request('x');
      await pumpEventQueue();
      engine().exitKeepingOutputOpen(3);
      await expectLater(pending, throwsA(engineError(EngineErrorCode.engineExited)));
      expect(client.state, EngineRunState.stopped);
    });

    test('an engine that closes its output but lingers is killed', () async {
      await startClient();
      final pending = client.request('x');
      await pumpEventQueue();
      engine().closeOutputOnly();
      await expectLater(pending, throwsA(engineError(EngineErrorCode.engineExited)));
      expect(engine().killed, isTrue);
    });

    test('restart: a fresh engine, new ids, nothing left over', () async {
      await startClient();
      final first = client.request('before');
      await pumpEventQueue();
      final firstId = engine().lastId;
      engine().exit(9);
      await expectLater(first, throwsA(engineError(EngineErrorCode.engineExited)));
      await pumpEventQueue();

      await client.start();
      expect(transport.connections, hasLength(2));
      expect(client.state, EngineRunState.running);
      final second = client.request('after');
      await pumpEventQueue();
      expect(engine().requests.single['cmd'], 'after');
      expect(engine().lastId, isNot(firstId));
      engine().respond(engine().lastId, {'n': 2});
      expect(await second, {'n': 2});
    });

    test('restart() of a running engine shuts it down first', () async {
      await startClient();
      final old = engine();
      await client.restart();
      expect(old.requests.last['cmd'], 'shutdown');
      expect(old.inputClosed, isTrue);
      expect(transport.connections, hasLength(2));
      expect(client.state, EngineRunState.running);
    });

    test('restart(graceful: false) kills an engine that hangs', () async {
      await startClient();
      final old = engine()..exitCodeOnInputClosed = null;
      final pending = expectLater(client.request('hangs'), throwsA(engineError(EngineErrorCode.engineExited)));
      await pumpEventQueue();
      await client.restart(graceful: false);
      expect(old.killed, isTrue);
      expect(old.requests.map((r) => r['cmd']), ['hangs']);
      await pending;
      expect(client.state, EngineRunState.running);
    });
  });

  group('start', () {
    test('an engine that cannot be started is engine_start_failed, and start can be retried', () async {
      transport = ScriptedTransport()..startError = StateError('No such file or directory');
      client = EngineClient(transport, settleTime: _settle);
      await expectLater(
        client.start(),
        throwsA(engineError(EngineErrorCode.engineStartFailed, message: allOf(contains('scripted engine'), contains('No such file')))),
      );
      expect(client.state, EngineRunState.stopped);

      transport.startError = null;
      await client.start();
      expect(client.state, EngineRunState.running);
    });

    test('starting twice is a programming error', () async {
      await startClient();
      expect(client.start, throwsStateError);
    });

    test('a shutdown while the engine is still starting kills it when it appears', () async {
      transport = ScriptedTransport()..startGate = Completer<void>();
      client = EngineClient(transport, settleTime: _settle);
      final starting = client.start();
      await pumpEventQueue();
      expect(client.state, EngineRunState.starting);
      await client.shutdown();
      expect(client.state, EngineRunState.stopped);
      transport.startGate!.complete();
      await expectLater(starting, throwsA(engineError(EngineErrorCode.engineStartFailed)));
      expect(transport.connection.killed, isTrue);
      expect(client.state, EngineRunState.stopped);
    });
  });

  group('shutdown', () {
    test('sends shutdown, closes the input and waits for the exit', () async {
      await startClient();
      final exits = <EngineExit>[];
      client.exits.listen(exits.add);
      final connection = engine();
      await client.shutdown();
      expect(connection.requests.single['cmd'], 'shutdown');
      expect(connection.inputClosed, isTrue);
      expect(connection.killed, isFalse);
      expect(client.state, EngineRunState.stopped);
      await pumpEventQueue();
      expect(exits.single.expected, isTrue);
      expect(exits.single.exitCode, 0);
    });

    test('requests that have not been sent fail; the one at the engine fails when it exits', () async {
      await startClient();
      final inFlight = client.request('scan_start');
      final queued = client.request('later');
      await pumpEventQueue();
      final done = client.shutdown();
      await expectLater(queued, throwsA(engineError(EngineErrorCode.engineNotRunning, message: contains('shutting down'))));
      await expectLater(inFlight, throwsA(engineError(EngineErrorCode.engineExited, message: contains('shut down'))));
      await done;
      await expectLater(client.request('x'), throwsA(engineError(EngineErrorCode.engineNotRunning)));
    });

    test('kill is the last resort for an engine that does not exit', () async {
      await startClient();
      final connection = engine()..exitCodeOnInputClosed = null;
      final watch = Stopwatch()..start();
      await client.shutdown(grace: const Duration(milliseconds: 80));
      expect(watch.elapsedMilliseconds, greaterThanOrEqualTo(70));
      expect(connection.requests.single['cmd'], 'shutdown');
      expect(connection.inputClosed, isTrue);
      expect(connection.killed, isTrue);
      expect(client.state, EngineRunState.stopped);
    });

    test('an engine that survives even the kill does not hang the app', () async {
      await startClient();
      engine()
        ..exitCodeOnInputClosed = null
        ..exitCodeOnKill = null;
      await client.shutdown(grace: const Duration(milliseconds: 20));
      expect(client.state, EngineRunState.stopped);
    });

    test('shutdown is safe to call twice and when nothing runs', () async {
      await startClient();
      await Future.wait([client.shutdown(), client.shutdown()]);
      await client.shutdown();
      expect(transport.connection.requests.where((r) => r['cmd'] == 'shutdown'), hasLength(1));
    });
  });

  test('stderr lines are passed on and the tail is bounded', () async {
    transport = ScriptedTransport();
    client = EngineClient(transport, stderrTailLines: 3, settleTime: _settle);
    await client.start();
    final lines = <String>[];
    client.stderrLines.listen(lines.add);
    transport.connection.emitStderr('one\ntwo\nthr');
    transport.connection.emitStderr('ee\nfour\nfive\n');
    await pumpEventQueue();
    expect(lines, ['one', 'two', 'three', 'four', 'five']);
    expect(client.stderrTail, ['three', 'four', 'five']);
  });
}
