// One conversation that every engine speaking protocol version 1 must be
// able to have: open an image, scan it, browse, summarise, read a file,
// recover. Only what docs/PROTOCOL.md guarantees is asserted.
//
// It always runs against the fake engine as a child process. To run it
// against the real engine as well, set two environment variables:
//
//   HDSAVIOR_ENGINE      path of hdsavior-engine(.exe)
//   HDSAVIOR_TEST_IMAGE  path of a raw image, e.g. fixtures-out/basic.img
//
// (The image is only read. Recovered files go to a temporary directory that
// is deleted afterwards.)
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';

import 'process_transport_test.dart' show fakeEngineProcess;

TypeMatcher<EngineException> fails(EngineErrorCode kind) =>
    isA<EngineException>().having((e) => e.kind, 'kind', kind);

Future<void> converse(EngineTransport transport, String imagePath) async {
  final client = EngineClient(transport);
  final api = EngineApi(client);
  final protocolErrors = <String>[];
  client.protocolErrors.listen(protocolErrors.add);
  final exits = <EngineExit>[];
  client.exits.listen(exits.add);
  final temp = await Directory.systemTemp.createTemp('hdsavior-conformance-');
  addTearDown(() async {
    await client.dispose();
    if (temp.existsSync()) await temp.delete(recursive: true);
  });
  await client.start();

  // --- state N: no source ---------------------------------------------------
  final hello = await api.hello();
  expect(hello.protocol, supportedProtocolVersion);
  expect(hello.carveKinds, isNotEmpty);
  await api.listDisks();
  await expectLater(api.catalogInfo(), throwsA(fails(EngineErrorCode.noCatalog)));
  await expectLater(api.scanStart(), throwsA(fails(EngineErrorCode.noSource)));
  await expectLater(client.request('no_such_command'), throwsA(fails(EngineErrorCode.unknownCommand)));
  await api.jobCancel(); // no job: still answered

  // --- state O: source open ---------------------------------------------------
  final opened = await api.openSource(kind: SourceKind.image, path: imagePath);
  expect(opened.source.kind, SourceKind.image);
  expect(opened.source.size, greaterThan(0));
  expect([512, 4096], contains(opened.source.sectorSize));
  expect(opened.source.size % opened.source.sectorSize, 0);
  for (final partition in opened.partitions) {
    expect(partition.offset + partition.size, lessThanOrEqualTo(opened.source.size));
  }
  await expectLater(api.catalogInfo(), throwsA(fails(EngineErrorCode.noCatalog)));
  final destination = await api.checkDestination(path: temp.path, purpose: DestinationPurpose.recover);
  expect(destination.verdict, isNot(DestinationVerdict.unknown));

  // --- scan ---------------------------------------------------------------------
  final progress = <ScanProgress>[];
  final progressSubscription = client.on<ScanProgress>().listen(progress.add);
  final scanDone = client.on<ScanDone>().first;
  final started = await api.scanStart();
  expect(started.job, JobKind.scan);
  expect(started.bytesTotal, opened.source.size);
  final done = await scanDone.timeout(const Duration(minutes: 5));
  await progressSubscription.cancel();
  expect(done.status, JobStatus.completed);
  expect(done.error, isNull);
  final catalog = done.catalog!;
  expect(catalog.scanComplete, isTrue);
  expect(catalog.sourceAttached, isTrue);
  expect(catalog.range, ByteRange(0, opened.source.size));
  // Phases come in order: mft_probe, then sweep, then analyzing.
  const order = [ScanPhase.mftProbe, ScanPhase.sweep, ScanPhase.analyzing];
  final phases = [for (final p in progress) order.indexOf(p.phase)];
  expect(phases, isNot(contains(-1)));
  expect(phases, List.of(phases)..sort());
  for (final p in progress.where((p) => p.phase != ScanPhase.analyzing)) {
    expect(p.bytesTotal, started.bytesTotal);
    expect(p.bytesDone, inInclusiveRange(0, p.bytesTotal));
  }

  // --- state C: catalog ---------------------------------------------------------
  final info = await api.catalogInfo();
  expect(info.epoch, catalog.epoch);
  expect(info.nodes, catalog.nodes);
  if (info.instances.isEmpty) return; // nothing found: nothing more to ask
  final epoch = info.epoch;
  final root = info.mainInstance!.root;
  expect(info.instances.map((i) => i.root).toSet(), hasLength(info.instances.length));

  await expectLater(api.listChildren(epoch: epoch + 1000, node: root), throwsA(fails(EngineErrorCode.staleCatalog)));
  await expectLater(api.nodeInfo(epoch: epoch, node: 1 << 40), throwsA(fails(EngineErrorCode.notFound)));

  final children = await api.listChildren(epoch: epoch, node: root, showDeleted: true, limit: 1000);
  expect(children.rows.length, lessThanOrEqualTo(children.total));
  expect(children.rows.every((r) => r.parent == root), isTrue);
  // Folders always sort before files.
  final firstFile = children.rows.indexWhere((r) => r.isFile);
  if (firstFile >= 0) expect(children.rows.skip(firstFile).every((r) => r.isFile), isTrue);
  expect((await api.listChildren(epoch: epoch, node: root, showDeleted: true, offset: children.total)).rows, isEmpty);
  final rootInfo = await api.nodeInfo(epoch: epoch, node: root);
  expect(rootInfo.row.parent, isNull);
  expect(rootInfo.ancestors, isEmpty);

  final found = await api.search(epoch: epoch, showDeleted: true, limit: 50);
  expect(found.rows.length, lessThanOrEqualTo(found.total));
  for (final row in found.rows) {
    expect(row.isFile, isTrue);
    expect(row.ancestorIds, isNotNull);
    expect(row.ancestorIds!.first, root);
    expect(row.ancestorIds!.last, row.parent);
    expect(row.path, endsWith(row.name));
    expect(row.rating, isNotNull);
  }
  if (found.rows.isNotEmpty) {
    final row = found.rows.first;
    final details = await api.nodeInfo(epoch: epoch, node: row.id);
    expect(details.row.id, row.id);
    expect(details.ancestorIds, row.ancestorIds);
    expect(details.path, row.path);
    expect(details.flagDetails.map((d) => d.flag), unorderedEquals(details.row.flagNames));
  }

  final summary = await api.selectionSummary(epoch: epoch, include: [root], exclude: []);
  expect(summary.good + summary.uncertain + summary.damaged + summary.unsupported, summary.files);
  final nothing = await api.selectionSummary(epoch: epoch, include: [], exclude: []);
  expect(nothing.files, 0);
  expect(nothing.bytes, 0);

  // --- read_file ------------------------------------------------------------------
  final readable = found.rows.where((r) => r.rating != Rating.unsupported && r.size > 0);
  if (readable.isNotEmpty) {
    final row = readable.first;
    final head = await api.readFile(epoch: epoch, node: row.id, length: 4096);
    expect(head.size, row.size);
    expect(head.bytes.length, row.size < 4096 ? row.size : 4096);
    expect(head.eof, row.size <= 4096);
    final content = await api.readFileContent(epoch: epoch, node: row.id, maxBytes: 1 << 20, chunkSize: 100000);
    expect(content.bytes.sublist(0, head.bytes.length), head.bytes);
    expect(content.complete, row.size <= 1 << 20);
  }
  await expectLater(api.readFile(epoch: epoch, node: root), throwsA(fails(EngineErrorCode.badRequest)));

  // --- recover ----------------------------------------------------------------------
  await expectLater(
    api.recoverStart(epoch: epoch, include: [], exclude: [], dest: temp.path),
    throwsA(fails(EngineErrorCode.badRequest)),
  );
  if (summary.files > 0 && destination.isOk) {
    final recoverDone = client.on<RecoverDone>().first;
    final recovering = await api.recoverStart(epoch: epoch, include: [root], exclude: [], dest: temp.path);
    expect(recovering.job, JobKind.recover);
    expect(recovering.files, summary.files);
    expect(recovering.bytes, summary.bytes);
    // One job at a time; queries are still answered.
    final second = await api.carveStart().then<Object?>((v) => v, onError: (Object e) => e);
    final recovered = await recoverDone.timeout(const Duration(minutes: 5));
    if (second is EngineException) {
      expect(second.kind, anyOf(EngineErrorCode.busy, EngineErrorCode.unsupported));
    } else {
      // The recovery had already ended; let the carve run end as well.
      await api.jobCancel();
      await client.on<CarveDone>().first.timeout(const Duration(minutes: 5));
    }
    expect(recovered.status, JobStatus.completed);
    expect(recovered.filesTotal, summary.files);
    expect(recovered.ok + recovered.warnings + recovered.failed + recovered.skipped, recovered.filesTotal);
    expect(recovered.dest, isNotEmpty);
  }

  // --- the end ------------------------------------------------------------------------
  await client.shutdown();
  await pumpEventQueue();
  expect(exits.single.expected, isTrue);
  expect(exits.single.exitCode, 0);
  expect(protocolErrors, isEmpty, reason: 'every line the engine wrote was understood');
}

void main() {
  test('the fake engine, as a process', () async {
    await converse(fakeEngineProcess(), '${Directory.systemTemp.path}${Platform.pathSeparator}conformance.img');
  }, timeout: const Timeout(Duration(minutes: 3)));

  test('the real engine', () async {
    final engine = Platform.environment['HDSAVIOR_ENGINE'];
    final image = Platform.environment['HDSAVIOR_TEST_IMAGE'];
    if (engine == null || engine.isEmpty || image == null || image.isEmpty) {
      markTestSkipped('set HDSAVIOR_ENGINE and HDSAVIOR_TEST_IMAGE to run the conversation with the real engine');
      return;
    }
    expect(File(engine).existsSync(), isTrue, reason: 'HDSAVIOR_ENGINE: $engine');
    expect(File(image).existsSync(), isTrue, reason: 'HDSAVIOR_TEST_IMAGE: $image');
    await converse(ProcessEngineTransport(executable: engine), File(image).absolute.path);
  }, timeout: const Timeout(Duration(minutes: 12)));
}
