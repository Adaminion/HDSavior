// The app's state layer driven against the real engine binary: open an
// image, scan it into a session file, browse, read a file, select a folder,
// check the destination and recover it, then load the session again.
//
// Runs only when two environment variables are set (as for
// test/engine/engine_conformance_test.dart):
//
//   HDSAVIOR_ENGINE      path of hdsavior-engine(.exe)
//   HDSAVIOR_TEST_IMAGE  a fixture image, e.g. <fixtures>/basic.img
//
// When the image has a manifest next to it (scripts/linux/make-fixtures.sh
// writes <name>.manifest.json), the recovered files are compared with the
// files the manifest lists: same relative path, same size and, where
// sha256sum is available, the same SHA-256. The image is only read; every
// file the test writes goes to a temporary directory.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';

import '../support/fake_platform.dart';

/// Waits until [condition] holds, letting the engine's lines arrive.
Future<void> until(bool Function() condition, {Duration timeout = const Duration(minutes: 5), String? reason}) async {
  final deadline = DateTime.now().add(timeout);
  while (!condition()) {
    if (DateTime.now().isAfter(deadline)) fail('timed out waiting${reason == null ? '' : ' for $reason'}');
    await Future<void>.delayed(const Duration(milliseconds: 20));
  }
}

/// All rows of a folder, page by page.
Future<List<NodeRow>> allChildren(EngineApi api, int epoch, int node) async {
  final rows = <NodeRow>[];
  while (true) {
    final page = await api.listChildren(epoch: epoch, node: node, offset: rows.length, limit: 1000, showDeleted: true);
    rows.addAll(page.rows);
    if (page.rows.isEmpty || rows.length >= page.total) return rows;
  }
}

String? sha256Of(String path) {
  if (!Platform.isLinux) return null;
  final result = Process.runSync('sha256sum', [path]);
  return result.exitCode == 0 ? (result.stdout as String).split(' ').first : null;
}

void main() {
  final engine = Platform.environment['HDSAVIOR_ENGINE'];
  final image = Platform.environment['HDSAVIOR_TEST_IMAGE'];
  final skip = engine == null || engine.isEmpty || image == null || image.isEmpty
      ? 'set HDSAVIOR_ENGINE and HDSAVIOR_TEST_IMAGE to drive the app against the real engine'
      : null;

  test('open an image, scan, browse, recover a folder and load the session again', () async {
    final imagePath = File(image!).absolute.path;
    final temp = await Directory.systemTemp.createTemp('hdsavior-app-real-');
    final sessions = Directory('${temp.path}${Platform.pathSeparator}sessions')..createSync();
    final app = AppController(
      transport: ProcessEngineTransport(executable: engine!),
      platform: InertPlatformServices(defaultSessionDirectory: sessions.path),
    );
    final protocolErrors = <String>[];
    app.client.protocolErrors.listen(protocolErrors.add);
    addTearDown(() async {
      await app.dispose();
      if (temp.existsSync()) await temp.delete(recursive: true);
    });

    // --- start and open -----------------------------------------------------
    await app.start();
    expect(app.engine.isReady, isTrue);
    expect(app.engine.hello!.protocol, supportedProtocolVersion);
    final opened = await app.source.openImage(imagePath);
    expect(opened.source.kind, SourceKind.image);
    expect(app.source.current!.info.path, imagePath);

    // --- scan into a session file ---------------------------------------------
    // (prepareSessionFile names one in the session directory with Windows
    // separators; the app is built for Windows only.)
    final sessionPath = '${sessions.path}${Platform.pathSeparator}basic.hdsc';
    expect((await app.source.checkSessionFile(sessionPath)).verdict, DestinationVerdict.ok);
    final scanDone = app.client.on<ScanDone>().first;
    await app.scan.start(sessionPath: sessionPath);
    final done = await scanDone.timeout(const Duration(minutes: 5));
    expect(done.status, JobStatus.completed, reason: '${done.error}');
    expect(done.sessionPath, sessionPath);
    expect(File(sessionPath).existsSync(), isTrue);
    await until(() => app.catalog.hasCatalog && app.jobs.value == null, reason: 'the catalog');
    final info = app.catalog.info!;
    expect(info.sourceAttached, isTrue);
    expect(info.scanComplete, isTrue);
    expect(info.instances, isNotEmpty);
    final epoch = info.epoch;
    final root = info.mainInstance!.root;

    // --- browse ---------------------------------------------------------------------
    await until(() => app.browse.folder != null, reason: 'the browser to open the main root');
    expect(app.browse.folder!.id, root);
    final rows = app.browse.rows!..load();
    await rows.idle;
    expect(rows.total, greaterThan(0));
    final top = await allChildren(app.api, epoch, root);
    final folders = top.where((r) => r.kind == NodeKind.dir && !r.deleted).toList();
    expect(folders, isNotEmpty);
    // A folder of some size that keeps the recovery small: the smallest one
    // with at least 10 files below it, else the smallest one with any.
    folders.sort((a, b) => a.files.compareTo(b.files));
    final folder = folders.firstWhere((r) => r.files >= 10, orElse: () => folders.firstWhere((r) => r.files > 0));
    app.browse.open(FolderLocation(folder.id, [root]));
    final inside = app.browse.rows!..load();
    await inside.idle;
    expect(inside.total, greaterThan(0));

    // --- details and content of one file ------------------------------------------
    final found = await app.api.search(epoch: epoch, under: folder.id, limit: 50);
    final file = found.rows.firstWhere((r) => r.isFile && r.size > 0 && r.rating != Rating.unsupported);
    final details = await app.catalog.nodeInfo(file.id);
    expect(details.row.id, file.id);
    expect(details.ancestorIds.first, root);
    final content = await app.catalog.readContent(file.id, maxBytes: 1 << 20);
    expect(content.bytes.length, file.size < (1 << 20) ? file.size : 1 << 20);

    // --- select, check the destination, recover -----------------------------------
    app.selection.setChecked(folder.id, [root], true);
    await until(() => app.recover.summary != null && !app.recover.isSummaryLoading, reason: 'the summary');
    final summary = app.recover.summary!;
    expect(summary.files, folder.files);
    final out = '${temp.path}${Platform.pathSeparator}rescued';
    Directory(out).createSync();
    final check = await app.recover.checkDestination(out);
    expect(check.verdict, DestinationVerdict.ok, reason: check.message);
    expect(check.enoughSpace, isTrue);
    expect(app.recover.readyToStart, isTrue);
    final recoverDone = app.client.on<RecoverDone>().first;
    final started = await app.recover.start(dest: out);
    expect(started.files, summary.files);
    final recovered = await recoverDone.timeout(const Duration(minutes: 5));
    expect(recovered.status, JobStatus.completed, reason: '${recovered.error}');
    // Files whose content cannot be read (compressed, encrypted) are
    // skipped; everything else is written.
    expect(recovered.skipped, summary.unsupported);
    expect(recovered.failed, 0);
    expect(recovered.ok + recovered.warnings, summary.files - summary.unsupported);
    expect(recovered.reportPath, isNotNull);
    expect(File(recovered.reportPath!).existsSync(), isTrue);
    await until(() => app.jobs.value == null, reason: 'the job slot to free up');

    // What was written: one file per selected file, below the folder's name.
    final written = {
      for (final entity in Directory(out).listSync(recursive: true))
        if (entity is File && entity.path != recovered.reportPath)
          entity.path.substring(out.length + 1).replaceAll(Platform.pathSeparator, '/'): entity,
    };
    expect(written, hasLength(recovered.ok + recovered.warnings));
    final readBack = written.entries.firstWhere((e) => e.key.endsWith('/${file.name}') || e.key == file.name);
    expect(readBack.value.readAsBytesSync().sublist(0, content.bytes.length), content.bytes);

    final manifest = File(imagePath.replaceFirst(RegExp(r'\.img$'), '.manifest.json'));
    if (manifest.existsSync()) {
      final entries = (jsonDecode(manifest.readAsStringSync()) as Map<String, Object?>)['entries']! as List<Object?>;
      final expected = {
        for (final entry in entries.cast<Map<String, Object?>>())
          if (entry['type'] == 'file' && entry['deleted'] != true && (entry['path']! as String).startsWith('${folder.name}/'))
            entry['path']! as String: entry,
      };
      expect(expected.keys, containsAll(written.keys), reason: 'the recovered paths are the manifest paths');
      expect(expected.length, summary.files);
      var damaged = 0;
      for (final MapEntry(key: path, value: copy) in written.entries) {
        final entry = expected[path]!;
        expect(copy.lengthSync(), entry['size'], reason: path);
        // Content the fixture overwrote after the accident is recovered as
        // it is now, with a warning; everything else is the original.
        if ((entry['after'] as Map<String, Object?>?)?['content_intact'] == false) {
          damaged++;
          continue;
        }
        final hash = sha256Of(copy.path);
        if (hash != null) expect(hash, entry['sha256'], reason: path);
      }
      expect(recovered.warnings, greaterThanOrEqualTo(damaged), reason: 'overwritten content is never reported as ok');
    }

    // --- load the session again ---------------------------------------------------
    await app.source.close();
    expect(app.catalog.hasCatalog, isFalse);
    final loaded = await app.source.loadSession(sessionPath, source: SourceRef(kind: SourceKind.image, path: imagePath));
    expect(loaded.sourceAttached, isTrue, reason: '${loaded.sourceError}');
    expect(loaded.catalog.epoch, isNot(epoch));
    expect(app.catalog.info!.nodes, info.nodes);
    expect(app.catalog.sourceAttached, isTrue);

    await app.shutdown();
    expect(protocolErrors, isEmpty, reason: 'every line the engine wrote was understood');
  }, skip: skip, timeout: const Timeout(Duration(minutes: 12)));
}
