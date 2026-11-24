// State transitions of the four jobs against the fake engine, step by step.
import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';

import '../engine/protocol_examples.dart';
import '../support/fake_app.dart';
import '../support/scripted_transport.dart';

void main() {
  late FakeApp fake;
  AppController app() => fake.app;

  setUp(() async {
    fake = FakeApp();
    await fake.start();
  });

  tearDown(() => fake.dispose());

  /// Records the phases a job controller goes through.
  List<JobPhase> phasesOf(JobController<Object?, JobProgressEvent, JobDoneEvent> job) {
    final phases = <JobPhase>[job.phase];
    job.addListener(() {
      if (phases.last != job.phase) phases.add(job.phase);
    });
    return phases;
  }

  group('scan', () {
    test('idle, starting, running, done; the catalog arrives with the done event', () async {
      await fake.openDisk();
      final scan = app().scan;
      final phases = phasesOf(scan);
      expect(scan.phase, JobPhase.idle);
      expect(scan.isActive, isFalse);
      expect(app().jobs.value, isNull);

      final range = app().source.current!.partitions.single.range;
      final starting = scan.start(range: range, sessionPath: r'C:\s\scan.hdsc');
      expect(scan.phase, JobPhase.starting);
      expect(scan.isActive, isTrue);
      expect(app().jobs.value, JobKind.scan);
      expect(app().jobs.isBusy, isTrue);

      final started = await starting;
      expect(started.bytesTotal, range.length);
      expect(scan.phase, JobPhase.running);
      expect(scan.started!.bytesTotal, range.length);
      expect(scan.progress, isNull, reason: 'the fake only moves when told to');
      expect(app().source.sessionPath, r'C:\s\scan.hdsc');
      expect(fake.lastArgs('scan_start'), {
        'start': range.start,
        'end': range.end,
        'session_path': r'C:\s\scan.hdsc',
        'resume': false,
        'collect_carve_hits': true,
        'allow_same_disk': false,
        'allow_unknown_disk': false,
      });

      final seen = <ScanPhase>[];
      while (scan.isActive) {
        await fake.step();
        final phase = scan.scanPhase;
        if (phase != null && (seen.isEmpty || seen.last != phase)) seen.add(phase);
        if (scan.isActive) {
          expect(scan.canStop, scan.progress!.phase != ScanPhase.analyzing);
          expect(scan.progress!.fraction, inInclusiveRange(0, 1));
        }
      }
      expect(seen, [ScanPhase.mftProbe, ScanPhase.sweep, ScanPhase.analyzing]);
      expect(phases, [JobPhase.idle, JobPhase.starting, JobPhase.running, JobPhase.done]);
      expect(scan.status, JobStatus.completed);
      expect(scan.result!.catalog!.scanComplete, isTrue);
      expect(scan.failure, isNull);
      expect(scan.scanPhase, isNull);
      expect(scan.canStop, isFalse);
      expect(app().jobs.value, isNull);
      expect(app().catalog.info!.epoch, scan.result!.catalog!.epoch);
      expect(app().browse.folder!.id, app().catalog.info!.mainInstance!.root);

      scan.reset();
      expect(scan.phase, JobPhase.idle);
      expect(scan.result, isNull);
      expect(scan.progress, isNull);
      expect(app().catalog.hasCatalog, isTrue, reason: 'dismissing the summary keeps the results');
    });

    test('the catalog, the selection and the cached rows are dropped as soon as a scan is accepted', () async {
      await fake.scan();
      final catalog = app().catalog;
      catalog.selection.setChecked(catalog.info!.mainInstance!.root, const [], true);
      final rows = app().browse.rows!..load();
      await rows.idle;
      expect(rows.total, greaterThan(0));

      await app().scan.resume();
      expect(catalog.info, isNull);
      expect(catalog.selection.isEmpty, isTrue);
      expect(rows.isClosed, isTrue);
      expect(app().browse.rows, isNull);
      expect(app().browse.folder, isNull);
      await fake.finishJob();
      expect(catalog.info, isNotNull);
    });

    test('stop keeps what was read; resume completes it', () async {
      await fake.openDisk();
      final scan = app().scan;
      final phases = phasesOf(scan);
      await scan.start();
      await fake.step(4);
      final stopping = scan.stop();
      expect(scan.phase, JobPhase.cancelling);
      await stopping;
      expect(fake.commands.last, 'job_cancel');
      await fake.finishJob();
      expect(phases, [JobPhase.idle, JobPhase.starting, JobPhase.running, JobPhase.cancelling, JobPhase.done]);
      expect(scan.status, JobStatus.stopped);
      final partial = app().catalog.info!;
      expect(partial.scanComplete, isFalse);
      expect(partial.warnings.map((w) => w.kind), contains(WarningCode.scanIncomplete));

      await scan.resume();
      expect(fake.lastArgs('scan_start'), {'resume': true, 'allow_same_disk': false, 'allow_unknown_disk': false});
      expect(app().catalog.info, isNull);
      await fake.finishJob();
      expect(scan.status, JobStatus.completed);
      expect(app().catalog.info!.scanComplete, isTrue);
      expect(app().catalog.info!.epoch, isNot(partial.epoch));
      expect(app().catalog.info!.nodes, greaterThan(partial.nodes));
    });

    test('source_lost: the job fails, the source is no longer attached, and the scan resumes after attaching', () async {
      await fake.openDisk();
      fake.script.scanFailAtStep = 3;
      final scan = app().scan;
      await scan.start(sessionPath: r'C:\s\lost.hdsc');
      await fake.finishJob();
      expect(scan.phase, JobPhase.done);
      expect(scan.status, JobStatus.failed);
      expect(scan.failure!.kind, EngineErrorCode.sourceLost);
      expect(scan.result!.catalog, isNull);
      expect(app().catalog.info, isNull);
      expect(app().source.current!.attached, isFalse);
      expect(app().source.current!.attachError!.kind, EngineErrorCode.sourceLost);
      expect(app().source.sessionPath, r'C:\s\lost.hdsc');
      expect(app().jobs.value, isNull);

      await expectLater(scan.resume(), throwsA(engineError(EngineErrorCode.noSource)));
      expect(scan.phase, JobPhase.idle);
      expect(scan.startError!.kind, EngineErrorCode.noSource);

      await app().source.attachSource(kind: SourceKind.disk, path: disk1);
      expect(app().source.current!.attached, isTrue);
      expect(app().source.current!.partitions, hasLength(1), reason: 'the same disk: what was known about it stays');
      expect(app().source.current!.table, PartitionTableKind.gpt);
      expect(app().source.current!.warnings, isNotEmpty);
      await scan.resume();
      expect(scan.startError, isNull);
      await fake.finishJob();
      expect(scan.status, JobStatus.completed);
    });

    test('a refused start: back to idle with the reason; a refused session path carries the verdict', () async {
      final scan = app().scan;
      await expectLater(scan.start(), throwsA(engineError(EngineErrorCode.noSource)));
      expect(scan.phase, JobPhase.idle);
      expect(scan.startError!.kind, EngineErrorCode.noSource);
      expect(app().jobs.value, isNull);

      await fake.openDisk();
      await expectLater(
        scan.start(sessionPath: r'E:\session-on-the-source.hdsc'),
        throwsA(engineError(EngineErrorCode.destinationRefused)),
      );
      expect(scan.startError!.destinationCheck!.verdict, DestinationVerdict.overlapsSource);
      expect(scan.phase, JobPhase.idle);

      await expectLater(scan.start(sessionPath: r'S:\same-disk.hdsc'), throwsA(engineError(EngineErrorCode.destinationRefused)));
      expect(scan.startError!.destinationCheck!.needsSameDiskConsent, isTrue);
      await scan.start(sessionPath: r'S:\same-disk.hdsc', allowSameDisk: true);
      expect(scan.startError, isNull);
      expect(scan.phase, JobPhase.running);
    });

    test('a refused start does not touch the existing catalog', () async {
      await fake.scan();
      final epoch = app().catalog.epoch;
      app().selection.setChecked(0, const [], true);
      fake.engine.failNext('scan_start', 'busy', message: 'A job is running.');
      await expectLater(app().scan.resume(), throwsA(engineError(EngineErrorCode.busy)));
      expect(app().catalog.epoch, epoch);
      expect(app().selection.isEmpty, isFalse);
    });

    test('starting a job that is already active is a programming error; another kind answers busy', () async {
      await fake.openDisk();
      await app().scan.start();
      expect(app().scan.start, throwsStateError);
      final refused = app().image.start(dest: r'D:\x.img');
      expect(app().image.phase, JobPhase.starting);
      expect(app().jobs.value, JobKind.scan, reason: 'a start that is not answered yet does not take over the slot');
      await expectLater(refused, throwsA(engineError(EngineErrorCode.busy)));
      expect(app().image.phase, JobPhase.idle);
      expect(app().image.startError!.kind, EngineErrorCode.busy);
      expect(app().jobs.value, JobKind.scan, reason: 'the refused job does not take over the slot');
      expect(app().scan.phase, JobPhase.running);
    });

    test('the engine dying under the scan ends it with engine_exited', () async {
      await fake.openDisk();
      await app().scan.start();
      await fake.step(2);
      fake.engine.crash();
      await pumpEventQueue();
      expect(app().scan.phase, JobPhase.done);
      expect(app().scan.status, JobStatus.failed);
      expect(app().scan.failure!.kind, EngineErrorCode.engineExited);
      expect(app().scan.result, isNull);
      expect(app().jobs.value, isNull);
      expect(app().engine.status, EngineStatus.failed);
    });

    test('the engine dying before it answers the start: idle, with the reason', () async {
      await fake.openDisk();
      fake.engine.exitOn('scan_start');
      await expectLater(app().scan.start(), throwsA(engineError(EngineErrorCode.engineExited)));
      await pumpEventQueue();
      expect(app().scan.isActive, isFalse);
      expect(app().jobs.value, isNull);
      expect(
        [app().scan.startError?.kind, app().scan.failure?.kind],
        contains(EngineErrorCode.engineExited),
      );
    });

    test('cancel without a job does nothing', () async {
      await app().scan.cancel();
      expect(fake.commands, isNot(contains('job_cancel')));
    });
  });

  group('recover', () {
    late int root;
    setUp(() async {
      await fake.scan();
      root = app().catalog.info!.mainInstance!.root;
    });

    test('the summary follows the selection and the options', () async {
      final recover = app().recover;
      expect(recover.summary!.files, 0, reason: 'nothing selected: known without asking');
      expect(fake.commands, isNot(contains('selection_summary')));

      app().selection.setChecked(root, const [], true);
      expect(recover.isSummaryLoading, isTrue);
      await pumpEventQueue();
      final all = recover.summary!;
      expect(recover.isSummaryLoading, isFalse);
      expect(all.files, greaterThan(1000));
      expect(all.good + all.uncertain + all.damaged + all.unsupported, all.files);
      expect(fake.lastArgs('selection_summary'), {
        'epoch': app().catalog.epoch,
        'include': [root],
        'exclude': <int>[],
        'include_deleted': false,
        'skip_damaged': false,
      });

      app().selection.setChecked(fake.landmarks.hugeFolder, [root], false);
      await pumpEventQueue();
      expect(recover.summary!.files, lessThan(all.files));
      expect(fake.lastArgs('selection_summary')['exclude'], [fake.landmarks.hugeFolder]);

      final before = recover.summary!;
      recover.options = recover.options.copyWith(includeDeleted: true, skipDamaged: true);
      expect(recover.isSummaryLoading, isTrue);
      await pumpEventQueue();
      expect(recover.summary!.files, greaterThan(before.files));
      expect(recover.summary!.bytes, isNot(before.bytes));
      expect(fake.lastArgs('selection_summary')['include_deleted'], isTrue);
      expect(fake.lastArgs('selection_summary')['skip_damaged'], isTrue);

      recover.options = recover.options.copyWith(filter: () => const FileFilter(exts: ['jpg']));
      await pumpEventQueue();
      expect(fake.lastArgs('selection_summary')['filter'], {
        'exts': ['jpg'],
      });
      recover.options = recover.options.copyWith(filter: () => null);
      await pumpEventQueue();
      expect(fake.lastArgs('selection_summary').containsKey('filter'), isFalse);

      app().selection.clear();
      await pumpEventQueue();
      expect(recover.summary!.files, 0);
      expect(recover.summary!.bytes, 0);
    });

    test('several changes in a row become one request; an obsolete request is cancelled', () async {
      final lm = fake.landmarks;
      app().selection.setChecked(root, const [], true);
      app().selection.setChecked(lm.documents, [root], false);
      app().selection.setChecked(lm.hugeFolder, [root], false);
      app().recover.options = app().recover.options.copyWith(includeDeleted: true);
      await pumpEventQueue();
      expect(fake.commands.where((c) => c == 'selection_summary'), hasLength(1));

      // A change while the summary is on its way: only the newest answer counts.
      fake.script.taskTicks = 0;
      app().selection.setChecked(lm.documents, [root], true);
      await Future<void>.delayed(Duration.zero);
      app().selection.setChecked(lm.manyFolders, [root], false);
      await pumpEventQueue();
      final summary = app().recover.summary!;
      final truth = await app().api.selectionSummary(
        epoch: app().catalog.epoch!,
        include: app().selection.include,
        exclude: app().selection.exclude,
        includeDeleted: true,
      );
      expect(summary.files, truth.files);
      expect(summary.bytes, truth.bytes);
    });

    test('an obsolete summary request still waiting in the queue is never sent', () async {
      // A long request keeps the engine busy, so the summary waits in the queue.
      fake.script.taskTicks = 2;
      final setting = app().catalog.setGeometry(instance: 0, clusterSize: 4096, volumeOffset: 1048576);
      await pumpEventQueue();
      app().selection.setChecked(fake.landmarks.documents, [root], true);
      await pumpEventQueue();
      app().selection.setChecked(fake.landmarks.manyFolders, [root], true);
      await pumpEventQueue();
      await fake.step(2);
      await setting;
      await pumpEventQueue();
      final both = [fake.landmarks.documents, fake.landmarks.manyFolders]..sort();
      final summaries = fake.engine.requests.where((r) => r.command == 'selection_summary').toList();
      // Sent: the one for both folders, and again after the new geometry
      // (ratings may have changed). Never: the one for the first folder alone.
      expect(summaries, isNotEmpty);
      for (final request in summaries) {
        expect(request.args['include'], both, reason: 'the obsolete request was cancelled before it was sent');
      }
      expect(app().recover.summary, isNotNull);
    });

    test('an answer that comes in after the selection changed again is not shown', () async {
      final examples = ProtocolExamples.load();
      final transport = ScriptedTransport();
      final scripted = AppController(transport: transport, platform: InertPlatformServices());
      addTearDown(scripted.dispose);
      unawaited(scripted.start());
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, examples.object('hello.result'));
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, {'disks': <Object?>[]});
      await pumpEventQueue();
      scripted.catalog.adopt(CatalogInfo.fromJson(JsonObject.root(examples.object('CatalogInfo'), 'c')));
      final recover = scripted.recover;

      scripted.selection.setChecked(17, const [0], true);
      await pumpEventQueue();
      final first = transport.connection.lastRequest;
      expect(first['cmd'], 'selection_summary');
      // The answer is on its way when the selection changes again.
      transport.connection.respond(first['id']! as int, examples.object('selection_summary.result'));
      scripted.selection.setChecked(99, const [0], true);
      await pumpEventQueue();
      expect(recover.summary, isNull, reason: 'the answer for the previous selection does not count');
      expect(recover.isSummaryLoading, isTrue);
      final second = transport.connection.lastRequest;
      expect(second['cmd'], 'selection_summary');
      expect((second['args']! as Map)['include'], [17, 99]);
      transport.connection.respond(second['id']! as int, {...examples.object('selection_summary.result'), 'files': 7});
      await pumpEventQueue();
      expect(recover.summary!.files, 7);
      expect(recover.isSummaryLoading, isFalse);
    });

    test('a summary that fails says why', () async {
      fake.engine.failNext('selection_summary', 'not_found', message: 'node 123 does not exist');
      app().selection.setChecked(root, const [], true);
      await pumpEventQueue();
      expect(app().recover.summary, isNull);
      expect(app().recover.summaryError!.kind, EngineErrorCode.notFound);
      expect(app().recover.isSummaryLoading, isFalse);
      app().recover.refreshSummary();
      await pumpEventQueue();
      expect(app().recover.summaryError, isNull);
      expect(app().recover.summary!.files, greaterThan(0));
    });

    test('checking a destination', () async {
      final recover = app().recover;
      app().selection.setChecked(fake.landmarks.documents, [root], true);
      await pumpEventQueue();
      final checking = recover.checkDestination(r'D:\Rescued');
      expect(recover.isCheckingDestination, isTrue);
      expect(recover.destinationCheck, isNull);
      final check = await checking;
      expect(check.isOk, isTrue);
      expect(check.enoughSpace, isTrue);
      expect(recover.destinationCheck, same(check));
      expect(recover.checkedDestination, r'D:\Rescued');
      expect(recover.isCheckingDestination, isFalse);
      expect(recover.allows(check), isTrue);
      expect(fake.lastArgs('check_destination'), {
        'path': r'D:\Rescued',
        'purpose': 'recover',
        'need_bytes': recover.summary!.bytes,
      });

      final overlap = await recover.checkDestination(r'E:\Rescued');
      expect(overlap.verdict, DestinationVerdict.overlapsSource);
      expect(overlap.message, isNotEmpty);
      expect(recover.allows(overlap), isFalse);
      final sameDisk = await recover.checkDestination(r'S:\Rescued');
      expect(recover.allows(sameDisk), isFalse);
      final unknown = await recover.checkDestination(r'U:\Rescued');
      expect(recover.allows(unknown), isFalse);
      recover.options = recover.options.copyWith(allowSameDisk: true);
      expect(recover.allows(sameDisk), isTrue);
      expect(recover.allows(unknown), isFalse, reason: 'the two consents are separate');
      expect(recover.allows(overlap), isFalse, reason: 'there is no override');
      recover.options = recover.options.copyWith(allowUnknownDisk: true);
      expect(recover.allows(unknown), isTrue);
      for (final path in [r'Q:\x', r'R:\x', 'relative']) {
        expect(recover.allows(await recover.checkDestination(path)), isFalse, reason: path);
      }
    });

    test('a newer destination check overtakes an older one', () async {
      final recover = app().recover;
      final first = recover.checkDestination(r'E:\typing');
      final second = recover.checkDestination(r'D:\typed');
      await expectLater(first, throwsA(isA<RequestCancelledException>()));
      expect((await second).isOk, isTrue);
      expect(recover.checkedDestination, r'D:\typed');
      expect(recover.destinationCheck!.isOk, isTrue);
      expect(recover.isCheckingDestination, isFalse);
    });

    test('the verdict is asked for again when the catalog changes and when the selection needs other space', () async {
      final recover = app().recover;
      int checks() => fake.engine.requests
          .where((r) => r.command == 'check_destination' && r.args['purpose'] == 'recover')
          .length;
      app().selection.setChecked(fake.landmarks.documents, [root], true);
      await pumpEventQueue();
      final first = await recover.checkDestination(r'L:\Rescued');
      expect(first.verdict, DestinationVerdict.overlapsSource);
      expect(first.reason, ProtectionReason.lostVolume);
      expect(first.lostFiles, greaterThan(0));
      final asked = checks();

      // More is selected: the guard is asked with the space needed now.
      app().selection.setChecked(root, const [], true);
      await pumpEventQueue();
      expect(checks(), asked + 1);
      expect(fake.lastArgs('check_destination')['need_bytes'], recover.summary!.bytes);
      expect(recover.destination.needBytes, recover.summary!.bytes);
      expect(recover.destinationCheck!.reason, ProtectionReason.lostVolume);

      // A geometry that is not validated protects no lost volume any more.
      await app().catalog.setGeometry(instance: 0, clusterSize: 8192, volumeOffset: 1048576);
      expect(recover.destinationCheck, isNull, reason: 'unknown until the new answer is in');
      expect(recover.isCheckingDestination, isTrue);
      await pumpEventQueue();
      expect(recover.destinationCheck!.verdict, DestinationVerdict.sameDisk);
      expect(recover.checkedDestination, r'L:\Rescued');

      // Without a catalog there is nothing to ask; with the next one it is asked again.
      await app().scan.start();
      await pumpEventQueue();
      expect(recover.destinationCheck, isNull);
      expect(recover.isCheckingDestination, isFalse);
      final duringScan = checks();
      await fake.finishJob();
      await pumpEventQueue();
      expect(checks(), greaterThan(duringScan));
      expect(recover.destinationCheck!.reason, ProtectionReason.lostVolume);
    });

    test('a check that a re-check of the same path overtakes answers with the newer verdict', () async {
      final recover = app().recover;
      fake.script.taskTicks = 2;
      final setting = app().catalog.setGeometry(instance: 0, clusterSize: 8192, volumeOffset: 1048576);
      await pumpEventQueue();
      final checking = recover.checkDestination(r'L:\Rescued');
      await fake.step(2);
      final check = await checking;
      await setting;
      await pumpEventQueue();
      expect(check.verdict, DestinationVerdict.sameDisk);
      expect(recover.destinationCheck, same(check));
      final recoverChecks = fake.engine.requests.where(
        (r) => r.command == 'check_destination' && r.args['purpose'] == 'recover',
      );
      expect(recoverChecks, hasLength(2), reason: 'asked, then asked again');
    });

    test('readyToStart: a selection, a destination the options allow, no job', () async {
      final recover = app().recover;
      expect(recover.readyToStart, isFalse);
      app().selection.setChecked(fake.landmarks.documents, [root], true);
      await pumpEventQueue();
      expect(recover.readyToStart, isFalse, reason: 'no destination yet');
      await recover.checkDestination(r'S:\Rescued');
      expect(recover.readyToStart, isFalse, reason: 'same disk needs consent');
      final summaries = fake.commands.where((c) => c == 'selection_summary').length;
      recover.options = recover.options.copyWith(allowSameDisk: true, onExists: OnExists.skip);
      await pumpEventQueue();
      expect(fake.commands.where((c) => c == 'selection_summary'), hasLength(summaries),
          reason: 'consents and on_exists do not change what the summary counts');
      expect(recover.summary, isNotNull);
      expect(recover.readyToStart, isTrue);
      await recover.checkDestination(r'T:\tiny');
      await pumpEventQueue();
      expect(recover.summary, isNotNull);
      expect(recover.allows(recover.destinationCheck!), isTrue);
      expect(recover.destinationCheck!.enoughSpace, isFalse);
      expect(recover.readyToStart, isFalse, reason: 'not enough space');
      await recover.checkDestination(r'D:\Rescued');
      expect(recover.readyToStart, isTrue);
      await recover.start(dest: r'D:\Rescued');
      expect(recover.readyToStart, isFalse, reason: 'a job runs');
      await fake.finishJob();
      await pumpEventQueue();
      expect(recover.readyToStart, isTrue);
      app().selection.clear();
      await pumpEventQueue();
      expect(recover.readyToStart, isFalse, reason: 'nothing selected');
    });

    test('readyToStart: folders without a file to recover are not startable', () async {
      final recover = app().recover;
      // A folder that holds only deleted files, with deleted files left out.
      app().selection.setChecked(fake.landmarks.deletedOnlyFolder, [root], true);
      await recover.checkDestination(r'D:\Rescued');
      await pumpEventQueue();
      final summary = recover.summary!;
      expect(summary.files, 0);
      expect(summary.dirs, greaterThan(0), reason: 'the folder itself is counted');
      expect(recover.readyToStart, isFalse, reason: 'recover_start refuses a selection without files');
      recover.options = recover.options.copyWith(includeDeleted: true);
      await pumpEventQueue();
      expect(recover.summary!.files, greaterThan(0));
      expect(recover.readyToStart, isTrue);
    });

    test('starting, progress, completion', () async {
      final recover = app().recover;
      final phases = phasesOf(recover);
      app().selection.setChecked(root, const [], true);
      app().selection.setChecked(fake.landmarks.hugeFolder, [root], false);
      recover.options = const RecoverOptions(skipDamaged: true, onExists: OnExists.skip);
      await pumpEventQueue();
      final summary = recover.summary!;

      final started = await recover.start(dest: r'D:\Rescued');
      expect(started.files, summary.files);
      expect(started.bytes, summary.bytes);
      expect(recover.phase, JobPhase.running);
      expect(app().jobs.value, JobKind.recover);
      expect(fake.lastArgs('recover_start'), {
        'epoch': app().catalog.epoch,
        'include': [root],
        'exclude': [fake.landmarks.hugeFolder],
        'dest': r'D:\Rescued',
        'include_deleted': false,
        'skip_damaged': true,
        'allow_same_disk': false,
        'allow_unknown_disk': false,
        'on_exists': 'skip',
      });

      await fake.step(2);
      final progress = recover.progress!;
      expect(progress.filesTotal, summary.files);
      expect(progress.filesDone, inExclusiveRange(0, summary.files));
      expect(progress.fraction, inExclusiveRange(0, 1));
      expect(progress.current, isNotNull);

      await fake.finishJob();
      expect(phases, [JobPhase.idle, JobPhase.starting, JobPhase.running, JobPhase.done]);
      final done = recover.result!;
      expect(recover.status, JobStatus.completed);
      expect(done.filesTotal, summary.files);
      expect(done.ok + done.warnings + done.failed + done.skipped, done.filesTotal);
      expect(done.skipped, summary.unsupported + summary.damaged);
      expect(done.notAttempted, 0);
      expect(done.dest, r'D:\Rescued');
      expect(done.reportPath, startsWith(r'D:\Rescued\HDSavior-report-'));
      expect(app().jobs.value, isNull);
      expect(app().selection.include, [root], reason: 'the selection survives a recovery');
      expect(fake.commands.last, isNot('recover_start'), reason: 'the catalog is refreshed afterwards');
      expect(fake.commands, contains('catalog_info'));
    });

    test('queries are answered while the recovery runs', () async {
      app().selection.setChecked(root, const [], true);
      await app().recover.start(dest: r'D:\Rescued');
      final rows = app().browse.rows!..load();
      await rows.idle;
      expect(rows.total, greaterThan(0));
      expect(rows.error, isNull);
      await fake.finishJob();
    });

    test('cancel: the job ends cancelled with what was done so far', () async {
      app().selection.setChecked(root, const [], true);
      final recover = app().recover;
      await recover.start(dest: r'D:\Rescued');
      await fake.step(2);
      await recover.cancel();
      expect(recover.phase, JobPhase.cancelling);
      await fake.finishJob();
      expect(recover.status, JobStatus.cancelled);
      expect(recover.result!.notAttempted, greaterThan(0));
      expect(recover.failure, isNull);
    });

    test('a refused destination: the verdict is kept and nothing runs', () async {
      app().selection.setChecked(root, const [], true);
      final recover = app().recover;
      await expectLater(recover.start(dest: r'E:\Rescued'), throwsA(engineError(EngineErrorCode.destinationRefused)));
      expect(recover.phase, JobPhase.idle);
      expect(recover.startError!.kind, EngineErrorCode.destinationRefused);
      expect(recover.checkedDestination, r'E:\Rescued');
      expect(recover.destinationCheck!.verdict, DestinationVerdict.overlapsSource);
      expect(app().jobs.value, isNull);

      // With the user's consent the engine accepts a location on the same disk.
      await expectLater(recover.start(dest: r'S:\Rescued'), throwsA(engineError(EngineErrorCode.destinationRefused)));
      expect(recover.destinationCheck!.needsSameDiskConsent, isTrue);
      recover.options = recover.options.copyWith(allowSameDisk: true);
      await recover.start(dest: r'S:\Rescued');
      expect(recover.phase, JobPhase.running);
      expect(recover.startError, isNull);
    });

    test('no_space, an empty selection, and a destination that fills up', () async {
      final recover = app().recover;
      await expectLater(recover.start(dest: r'D:\Rescued'), throwsA(engineError(EngineErrorCode.badRequest)));
      app().selection.setChecked(root, const [], true);
      await expectLater(recover.start(dest: r'T:\tiny'), throwsA(engineError(EngineErrorCode.noSpace)));
      expect(recover.phase, JobPhase.idle);

      fake.script.recoverFailAtStep = 3;
      await recover.start(dest: r'D:\Rescued');
      await fake.finishJob();
      expect(recover.status, JobStatus.failed);
      expect(recover.failure!.kind, EngineErrorCode.noSpace);
      expect(recover.result!.status, JobStatus.failed);
      expect(recover.result!.notAttempted, greaterThan(0));
    });

    test('source_lost during recovery detaches the source', () async {
      app().selection.setChecked(root, const [], true);
      fake.script
        ..recoverFailAtStep = 2
        ..recoverFailCode = 'source_lost';
      await app().recover.start(dest: r'D:\Rescued');
      await fake.finishJob();
      expect(app().recover.failure!.kind, EngineErrorCode.sourceLost);
      expect(app().source.current!.attached, isFalse);
    });

    test('a stale catalog refuses the recovery and brings the new catalog in', () async {
      app().selection.setChecked(root, const [], true);
      await pumpEventQueue();
      final old = app().catalog.epoch;
      fake.engine.bumpEpoch();
      await expectLater(app().recover.start(dest: r'D:\Rescued'), throwsA(engineError(EngineErrorCode.staleCatalog)));
      await pumpEventQueue();
      expect(app().catalog.epoch, isNot(old));
      expect(app().selection.isEmpty, isTrue, reason: 'the ids of the old epoch are void');
      expect(app().recover.phase, JobPhase.idle);
      expect(app().recover.summary!.files, 0);
    });
  });

  group('carve', () {
    setUp(() => fake.scan());

    test('start, progress, done: the catalog gains carved files in the same epoch', () async {
      final carve = app().carve;
      final phases = phasesOf(carve);
      expect(carve.hasCarveHits, isTrue);
      final epoch = app().catalog.epoch;
      final started = await carve.start(kinds: ['jpeg', 'png']);
      expect(started.candidates, app().catalog.info!.carveHits);
      expect(app().jobs.value, JobKind.carve);
      expect(fake.lastArgs('carve_start'), {
        'kinds': ['jpeg', 'png'],
      });
      await fake.step(2);
      expect(carve.progress!.candidatesTotal, started.candidates);
      expect(carve.progress!.fraction, inExclusiveRange(0, 1));
      await fake.finishJob();
      expect(phases, [JobPhase.idle, JobPhase.starting, JobPhase.running, JobPhase.done]);
      expect(carve.status, JobStatus.completed);
      expect(carve.result!.found, greaterThan(0));
      expect(app().catalog.epoch, epoch);
      expect(app().catalog.info!.carved, carve.result!.found);
      expect(app().catalog.info!.mainInstance!.virtual.carved, isNotNull);
    });

    test('the selection survives carving, except for carved files that are replaced', () async {
      final root = app().catalog.info!.mainInstance!.root;
      await app().carve.start(kinds: ['jpeg']);
      await fake.finishJob();
      final carvedRoot = app().catalog.info!.mainInstance!.virtual.carved!;
      final kindFolder = (await app().api.listChildren(epoch: app().catalog.epoch!, node: carvedRoot)).rows.single;
      final carvedFile = (await app().api.listChildren(epoch: app().catalog.epoch!, node: kindFolder.id)).rows.first;

      app().selection
        ..setChecked(fake.landmarks.documents, [root], true)
        ..setChecked(carvedFile.id, [root, carvedRoot, kindFolder.id], true);
      expect(app().selection.include, [fake.landmarks.documents, carvedFile.id]);

      final rows = app().browse.rows!..load();
      await rows.idle;
      await app().carve.start(kinds: ['jpeg']);
      expect(app().selection.include, [fake.landmarks.documents], reason: 'the carved file is being replaced');
      expect(rows.isClosed, isFalse, reason: 'same epoch');
      expect(rows.peek(0), isNull, reason: 'but the rows are fetched again');
      await fake.finishJob();
      await pumpEventQueue();
      expect(app().recover.summaryError, isNull, reason: 'no vanished id is left in the selection');
      expect(app().recover.summary!.files, greaterThan(0));
    });

    test('cancel keeps what was found; unsupported without stored hits', () async {
      await app().carve.start();
      await fake.step(3);
      await app().carve.cancel();
      await fake.finishJob();
      expect(app().carve.status, JobStatus.cancelled);
      expect(app().catalog.info!.carved, app().carve.result!.found);
      expect(app().catalog.info!.carved, greaterThan(0));

      // A scan without signature hits cannot be carved.
      await app().scan.start(collectCarveHits: false);
      await fake.finishJob();
      expect(app().carve.hasCarveHits, isFalse);
      await expectLater(app().carve.start(), throwsA(engineError(EngineErrorCode.unsupported)));
      expect(app().carve.phase, JobPhase.idle);
      expect(app().carve.startError!.kind, EngineErrorCode.unsupported);
    });

    test('a failed run still hands the catalog back', () async {
      fake.script.carveFailAtStep = 2;
      await app().carve.start();
      await fake.finishJob();
      expect(app().carve.status, JobStatus.failed);
      expect(app().carve.failure!.kind, EngineErrorCode.sourceLost);
      expect(app().source.current!.attached, isFalse);
      expect(app().catalog.hasCatalog, isTrue);
    });
  });

  group('image', () {
    setUp(() => fake.openDisk());

    test('needs a source but no scan: check, start, progress, done', () async {
      final image = app().image;
      final phases = phasesOf(image);
      final range = app().source.current!.partitions.single.range;
      final check = await image.checkDestination(r'D:\images\disk1.img', needBytes: range.length);
      expect(check.isOk, isTrue);
      expect(check.enoughSpace, isTrue);
      expect(image.destinationCheck, same(check));
      expect(fake.lastArgs('check_destination'), {'path': r'D:\images\disk1.img', 'purpose': 'image', 'need_bytes': range.length});

      final started = await image.start(range: range, dest: r'D:\images\disk1.img');
      expect(started.bytes, range.length);
      expect(app().jobs.value, JobKind.image);
      expect(fake.lastArgs('image_start'), {
        'start': range.start,
        'end': range.end,
        'dest': r'D:\images\disk1.img',
        'resume': false,
        'allow_same_disk': false,
        'allow_unknown_disk': false,
      });
      await fake.step(3);
      expect(image.progress!.bytesTotal, range.length);
      expect(image.progress!.fraction, closeTo(0.5, 0.01));
      await fake.finishJob();
      expect(phases, [JobPhase.idle, JobPhase.starting, JobPhase.running, JobPhase.done]);
      expect(image.status, JobStatus.completed);
      expect(image.result!.bytesWritten, range.length);
      expect(image.result!.path, r'D:\images\disk1.img');
      expect(image.result!.badMapPath, isNull);
    });

    test('the verdict is asked for again for another source, and unknown without one', () async {
      final image = app().image;
      final first = await image.checkDestination(r'E:\disk.img', needBytes: 4096);
      expect(first.verdict, DestinationVerdict.overlapsSource, reason: 'E: is on disk 1');
      expect(image.allows(first, allowSameDisk: true, allowUnknownDisk: true), isFalse);

      await app().source.openDisk(app().source.disks![2]);
      expect(image.destinationCheck, isNull, reason: 'the verdict was about the previous source');
      await pumpEventQueue();
      expect(image.destinationCheck!.verdict, DestinationVerdict.ok, reason: 'E: is not on disk 2');
      expect(image.allows(image.destinationCheck!), isTrue);
      expect(fake.lastArgs('check_destination'), {'path': r'E:\disk.img', 'purpose': 'image', 'need_bytes': 4096});

      await app().source.close();
      expect(image.destinationCheck, isNull);
      expect(image.checkedDestination, r'E:\disk.img');
      expect(image.destinationError, isNull);
    });

    test('the whole source by default; unreadable sectors are reported', () async {
      fake.script.imageBadBytes = 8192;
      await app().image.start(dest: r'D:\images\whole.img');
      expect(fake.lastArgs('image_start')['start'], 0);
      expect(fake.lastArgs('image_start').containsKey('end'), isFalse);
      await fake.finishJob();
      expect(app().image.result!.badBytes, 8192);
      expect(app().image.result!.badMapPath, r'D:\images\whole.img.bad.txt');
    });

    test('an existing file is refused unless resuming; refused destinations keep their verdict', () async {
      final image = app().image;
      await expectLater(image.start(dest: r'D:\images\exists.img'), throwsA(engineError(EngineErrorCode.exists)));
      expect(image.phase, JobPhase.idle);
      expect(image.startError!.kind, EngineErrorCode.exists);
      await expectLater(image.start(dest: r'E:\on-the-source.img'), throwsA(engineError(EngineErrorCode.destinationRefused)));
      expect(image.destinationCheck!.verdict, DestinationVerdict.overlapsSource);
      expect(image.checkedDestination, r'E:\on-the-source.img');
      await expectLater(image.start(dest: r'K:\fat32.img'), throwsA(engineError(EngineErrorCode.noSpace)));

      await image.start(dest: r'D:\images\exists.img', resume: true);
      expect(fake.lastArgs('image_start')['resume'], isTrue);
      expect(image.phase, JobPhase.running);
    });

    test('cancel and source_lost', () async {
      final image = app().image;
      await image.start(dest: r'D:\images\a.img');
      await fake.step(2);
      await image.cancel();
      await fake.finishJob();
      expect(image.status, JobStatus.cancelled);
      expect(app().source.current!.attached, isTrue);

      fake.script.imageFailAtStep = 2;
      await image.start(dest: r'D:\images\b.img');
      await fake.finishJob();
      expect(image.status, JobStatus.failed);
      expect(image.failure!.kind, EngineErrorCode.sourceLost);
      expect(app().source.current!.attached, isFalse);
    });
  });

  group('whatever arrives first', () {
    // The engine writes the response to X_start before the job's events, but
    // both can reach the app in the same chunk, and then the event handlers
    // run before the code that awaited the response.
    late ScriptedTransport transport;
    late AppController scripted;
    final examples = ProtocolExamples.load();

    setUp(() async {
      transport = ScriptedTransport();
      scripted = AppController(transport: transport, platform: InertPlatformServices());
      unawaited(scripted.start());
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, examples.object('hello.result'));
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, {'disks': <Object?>[]});
      await pumpEventQueue();
      expect(scripted.engine.isReady, isTrue);
    });

    tearDown(() => scripted.dispose());

    test('a scan that is done at once (resuming a complete session)', () async {
      // An older catalog is in place.
      final old = examples.object('CatalogInfo')..['epoch'] = 1;
      scripted.catalog.adopt(CatalogInfo.fromJson(JsonObject.root(old, 'c')));
      scripted.selection.setChecked(0, const [], true);

      final starting = scripted.scan.resume();
      await pumpEventQueue();
      final id = transport.connection.lastId;
      transport.connection.emit(
        '${jsonEncode({'id': id, 'ok': true, 'result': examples.json('scan_start.result')})}\n'
        '${jsonEncode({'event': 'scan_done', 'data': examples.json('scan_done')})}\n',
      );
      await starting;
      await pumpEventQueue();

      expect(scripted.scan.phase, JobPhase.done, reason: 'the late continuation must not set it back to running');
      expect(scripted.scan.status, JobStatus.completed);
      expect(scripted.scan.started!.bytesTotal, 4000785104896);
      expect(scripted.catalog.epoch, 3, reason: 'the new catalog is in place, not cleared by the late continuation');
      expect(scripted.selection.isEmpty, isTrue);
      expect(scripted.jobs.value, isNull);
    });

    test('a done event that the controller sees before the answer to its start', () async {
      // The engine writes the answer first, and this client resumes the code
      // that awaits it before it delivers the next event. The controller must
      // not depend on that order: whatever it sees first counts, and the code
      // that runs later neither revives the job nor accepts it a second time
      // (which would clear the new catalog).
      final old = examples.object('CatalogInfo')..['epoch'] = 1;
      scripted.catalog.adopt(CatalogInfo.fromJson(JsonObject.root(old, 'c')));
      final starting = scripted.scan.resume();
      await pumpEventQueue();
      final id = transport.connection.lastId;
      transport.connection.emit(
        '${jsonEncode({'event': 'scan_done', 'data': examples.json('scan_done')})}\n'
        '${jsonEncode({'id': id, 'ok': true, 'result': examples.json('scan_start.result')})}\n',
      );
      await starting;
      await pumpEventQueue();
      expect(scripted.scan.phase, JobPhase.done);
      expect(scripted.scan.status, JobStatus.completed);
      expect(scripted.scan.started!.bytesTotal, 4000785104896);
      expect(scripted.catalog.epoch, 3);
      expect(scripted.jobs.value, isNull);
    });

    test('progress and done in one chunk with the response', () async {
      final starting = scripted.image.start(dest: r'D:\x.img');
      await pumpEventQueue();
      final id = transport.connection.lastId;
      transport.connection.emit(
        '${jsonEncode({'id': id, 'ok': true, 'result': examples.json('image_start.result')})}\n'
        '${jsonEncode({'event': 'image_progress', 'data': examples.json('image_progress')})}\n'
        '${jsonEncode({'event': 'image_done', 'data': examples.json('image_done')})}\n',
      );
      await starting;
      await pumpEventQueue();
      expect(scripted.image.phase, JobPhase.done);
      expect(scripted.image.progress!.bytesDone, 1048576);
      expect(scripted.image.result!.bytesWritten, 4000787030016);
      expect(scripted.image.started!.bytes, 4000787030016);
    });

    test('a done event that cannot be read still ends the job', () async {
      final starting = scripted.image.start(dest: r'D:\x.img');
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, examples.object('image_start.result'));
      await starting;
      expect(scripted.image.phase, JobPhase.running);
      transport.connection.event('image_done', {'status': 'completed'});
      await pumpEventQueue();
      expect(scripted.image.phase, JobPhase.done);
      expect(scripted.image.status, JobStatus.failed);
      expect(scripted.image.failure!.kind, EngineErrorCode.protocolError);
      expect(scripted.jobs.value, isNull);
    });

    test('a cancel before the start is answered waits for it, so that it cannot overtake the start', () async {
      // A request is at the engine, so scan_start waits in the client's queue.
      final slow = scripted.api.listDisks().then<Object?>((v) => v, onError: (Object e) => e);
      await pumpEventQueue();
      final starting = scripted.scan.resume();
      await scripted.scan.cancel();
      await pumpEventQueue();
      expect(scripted.scan.phase, JobPhase.starting);
      expect(scripted.scan.cancelPending, isTrue);
      expect(transport.connection.requests.map((r) => r['cmd']), isNot(contains('job_cancel')),
          reason: 'sent now, job_cancel would reach the engine before scan_start');

      transport.connection.respond(transport.connection.lastId, {'disks': <Object?>[]});
      await slow;
      await pumpEventQueue();
      expect(transport.connection.requests.last['cmd'], 'scan_start');
      transport.connection.respond(transport.connection.lastId, examples.object('scan_start.result'));
      await starting;
      await pumpEventQueue();
      expect(transport.connection.requests.last['cmd'], 'job_cancel', reason: 'sent once the job was accepted');
      expect(scripted.scan.phase, JobPhase.cancelling);
      expect(scripted.scan.cancelPending, isFalse);
      transport.connection.respond(transport.connection.lastId);
      transport.connection.event('scan_done', examples.json('scan_done'));
      await pumpEventQueue();
      expect(scripted.scan.phase, JobPhase.done);
    });

    test('a cancel before a start that is refused is forgotten', () async {
      final starting = scripted.image.start(dest: r'E:\x.img');
      await pumpEventQueue();
      await scripted.image.cancel();
      expect(scripted.image.cancelPending, isTrue);
      transport.connection.respondError(transport.connection.lastId, 'busy');
      await expectLater(starting, throwsA(isA<EngineException>()));
      expect(scripted.image.cancelPending, isFalse);
      expect(transport.connection.requests.map((r) => r['cmd']), isNot(contains('job_cancel')));
    });

    test('a malformed done event of a job that is not running is ignored', () async {
      transport.connection.event('recover_done', {'status': 'completed'});
      await pumpEventQueue();
      expect(scripted.recover.phase, JobPhase.idle);
      expect(scripted.recover.failure, isNull);
    });

    test('events of a job this controller does not follow are ignored', () async {
      transport.connection.event('recover_progress', examples.json('recover_progress'));
      transport.connection.event('scan_progress', examples.json('scan_progress'));
      await pumpEventQueue();
      expect(scripted.recover.phase, JobPhase.idle);
      expect(scripted.recover.progress, isNull);
      expect(scripted.scan.phase, JobPhase.idle);
      expect(scripted.image.phase, JobPhase.idle);
    });
  });
}
