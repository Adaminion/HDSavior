// The real process transport, end to end: the fake engine runs as a child
// process (`dart test/fake_engine/serve.dart serve`) and the client talks to
// it over real pipes.
import 'dart:async';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

/// The `dart` executable of the Flutter SDK that runs these tests.
String dartExecutable() {
  final name = Platform.isWindows ? 'dart.exe' : 'dart';
  final candidates = <String>[
    if (Platform.environment['FLUTTER_ROOT'] case final root?) '$root/bin/cache/dart-sdk/bin/$name',
    // flutter_tester lives in bin/cache/artifacts/engine/<platform>/.
    '${File(Platform.resolvedExecutable).parent.parent.parent.parent.path}/dart-sdk/bin/$name',
  ];
  for (final candidate in candidates) {
    if (File(candidate).existsSync()) return candidate;
  }
  fail('The dart executable was not found (looked in $candidates).');
}

/// Starts the fake engine as a process, like `hdsavior-engine serve`.
ProcessEngineTransport fakeEngineProcess() => ProcessEngineTransport(
  executable: dartExecutable(),
  arguments: const ['test/fake_engine/serve.dart', 'serve'],
);

void main() {
  late EngineClient client;
  late EngineApi api;

  setUp(() async {
    client = EngineClient(fakeEngineProcess());
    api = EngineApi(client);
    await client.start();
  });

  tearDown(() => client.dispose());

  /// Opens the fake's disk 1, scans it and returns the catalog.
  Future<CatalogInfo> scan() async {
    final disks = await api.listDisks();
    await api.openSource(kind: SourceKind.disk, path: disks[1].path);
    final done = client.on<ScanDone>().first;
    await api.scanStart();
    return (await done).catalog!;
  }

  test('requests, events and stderr travel through real pipes', () async {
    final stderrLine = client.stderrLines.first;
    final hello = await api.hello();
    expect(hello.protocol, supportedProtocolVersion);
    expect(hello.engineVersion, contains('fake'));
    expect(await stderrLine, contains('fake'));

    final progress = <ScanProgress>[];
    client.on<ScanProgress>().listen(progress.add);
    final catalog = await scan();
    expect(progress.map((p) => p.phase).toSet(), {ScanPhase.mftProbe, ScanPhase.sweep, ScanPhase.analyzing});
    final page = await api.listChildren(epoch: catalog.epoch, node: catalog.mainInstance!.root);
    expect(page.rows, isNotEmpty);
    expect(page.rows.map((r) => r.name), contains('Preview samples'));
  });

  test('a response line of more than 11 MB (read_file of 8 MiB)', () async {
    final catalog = await scan();
    final root = await api.listChildren(epoch: catalog.epoch, node: catalog.mainInstance!.root);
    final samples = root.rows.firstWhere((r) => r.name == 'Preview samples');
    final files = await api.listChildren(epoch: catalog.epoch, node: samples.id);
    final large = files.rows.firstWhere((r) => r.name == 'large archive.zip');
    expect(large.size, 20 * 1024 * 1024);

    final first = await api.readFile(epoch: catalog.epoch, node: large.id, length: maxReadFileLength);
    expect(first.bytes, hasLength(maxReadFileLength));
    expect(first.eof, isFalse);
    expect(first.kind, 'zip');
    expect(first.bytes.sublist(0, 4), [0x50, 0x4B, 0x03, 0x04]);

    // The same bytes arrive when the file is read in small pieces.
    final content = await api.readFileContent(
      epoch: catalog.epoch,
      node: large.id,
      maxBytes: 3 * 1024 * 1024,
      chunkSize: 700000,
    );
    expect(content.complete, isFalse);
    expect(content.size, large.size);
    expect(content.bytes, first.bytes.sublist(0, 3 * 1024 * 1024));
  });

  test('the engine dying in the middle of a request fails it with its exit code', () async {
    final exits = <EngineExit>[];
    client.exits.listen(exits.add);
    await client.request('fake_control', args: {'exit_on': 'list_disks', 'exit_code': 42, 'partial_output': '{"id": 2, "ok": tr'});
    await expectLater(
      api.listDisks(),
      throwsA(
        isA<EngineException>()
            .having((e) => e.kind, 'kind', EngineErrorCode.engineExited)
            .having((e) => e.message, 'message', contains('42')),
      ),
    );
    await pumpEventQueue();
    expect(client.state, EngineRunState.stopped);
    expect(exits.single.exitCode, 42);
    expect(exits.single.expected, isFalse);
    expect(exits.single.stderrTail.last, contains('simulated crash'));

    // A fresh engine can be started afterwards and knows nothing of the old one.
    await client.start();
    expect((await api.hello()).protocol, 1);
    await expectLater(
      api.catalogInfo(),
      throwsA(isA<EngineException>().having((e) => e.kind, 'kind', EngineErrorCode.noCatalog)),
    );
  });

  test('shutdown ends the process cleanly, without a kill', () async {
    final exits = <EngineExit>[];
    client.exits.listen(exits.add);
    await api.hello();
    final watch = Stopwatch()..start();
    await client.shutdown();
    expect(watch.elapsed, lessThan(const Duration(seconds: 8)), reason: 'no waiting for the kill');
    await pumpEventQueue();
    expect(exits.single.exitCode, 0);
    expect(exits.single.expected, isTrue);
    expect(client.state, EngineRunState.stopped);
  });

  test('closing the input alone makes the engine exit (the app was killed)', () async {
    // What happens to the child when the UI process dies: its stdin closes.
    final process = await Process.start(dartExecutable(), ['test/fake_engine/serve.dart', 'serve']);
    unawaited(process.stdout.drain<void>());
    unawaited(process.stderr.drain<void>());
    await process.stdin.close();
    expect(await process.exitCode.timeout(const Duration(seconds: 20)), 0);
  });

  test('a missing engine executable is engine_start_failed with the path', () async {
    final missing = EngineClient(ProcessEngineTransport(executable: '${Directory.systemTemp.path}/no-such-hdsavior-engine'));
    await expectLater(
      missing.start(),
      throwsA(
        isA<EngineException>()
            .having((e) => e.kind, 'kind', EngineErrorCode.engineStartFailed)
            .having((e) => e.message, 'message', contains('no-such-hdsavior-engine'))
            .having((e) => e.isEngineGone, 'isEngineGone', isTrue),
      ),
    );
    expect(missing.state, EngineRunState.stopped);
    await missing.dispose();
  });

  test('writing to an engine that has just died raises nothing', () async {
    await client.request('fake_control', args: {'exit_on': 'hello'});
    // Requests written while the process is dying.
    final results = await Future.wait([
      for (var i = 0; i < 20; i++)
        api.hello().then<Object?>((_) => null, onError: (Object e) => e),
    ]);
    expect(results, everyElement(isA<EngineException>()));
    await pumpEventQueue();
  });
}
