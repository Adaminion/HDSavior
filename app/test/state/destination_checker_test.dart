// The DestinationChecker on its own: overtaking, re-checks, invalidation and
// refusals at job start, against the fake engine.
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';

import '../support/fake_app.dart';

void main() {
  late FakeApp fake;
  late DestinationChecker checker;
  late List<String> notes;

  setUp(() async {
    fake = FakeApp();
    await fake.start();
    await fake.openDisk();
    checker = DestinationChecker(fake.app.api, purpose: DestinationPurpose.recover);
    notes = [];
    checker.addListener(() => notes.add('${checker.isChecking}/${checker.check?.verdict.wire}'));
  });

  tearDown(() async {
    checker.dispose();
    await fake.dispose();
  });

  List<Map<String, Object?>> checksSent() => [
    for (final request in fake.engine.requests)
      if (request.command == 'check_destination' && request.args['purpose'] == 'recover') request.args,
  ];

  test('a check: unknown while asked, then the verdict; the arguments are the documented ones', () async {
    expect(checker.path, isNull);
    final future = checker.checkPath(r'D:\Rescued', needBytes: 5000);
    expect(checker.isChecking, isTrue);
    expect(checker.check, isNull);
    final check = await future;
    expect(check.isOk, isTrue);
    expect(checker.check, same(check));
    expect(checker.path, r'D:\Rescued');
    expect(checker.needBytes, 5000);
    expect(checker.isChecking, isFalse);
    expect(checksSent().single, {'path': r'D:\Rescued', 'purpose': 'recover', 'need_bytes': 5000});
    expect(notes, ['true/null', 'false/ok']);
  });

  test('a check of another path overtakes: the older one fails as cancelled and its answer is dropped', () async {
    final first = checker.checkPath(r'E:\typing');
    final second = checker.checkPath(r'D:\typed');
    await expectLater(first, throwsA(isA<RequestCancelledException>()));
    expect((await second).isOk, isTrue);
    expect(checker.path, r'D:\typed');
    expect(checker.check!.isOk, isTrue);
  });

  test('a re-check of the same path answers for the check it overtook', () async {
    final first = checker.checkPath(r'S:\Rescued', needBytes: 1);
    checker.recheck(needBytes: 2);
    final check = await first;
    expect(checker.check, same(check), reason: 'the newer verdict');
    expect(checker.needBytes, 2);
    expect(checksSent().map((a) => a['need_bytes']), [1, 2]);
  });

  test('recheck asks again with the stored path; without a path it does nothing', () async {
    checker.recheck();
    await pumpEventQueue();
    expect(checksSent(), isEmpty);
    await checker.checkPath(r'D:\Rescued', needBytes: 10);
    checker.recheck();
    expect(checker.check, isNull);
    await pumpEventQueue();
    expect(checker.check!.isOk, isTrue);
    expect(checksSent().map((a) => a['need_bytes']), [10, 10]);
  });

  test('failures: an explicit check rethrows, a re-check keeps the error', () async {
    fake.engine.failNext('check_destination', 'io_error', message: 'cannot open the volume');
    await expectLater(checker.checkPath(r'D:\Rescued'), throwsA(engineError(EngineErrorCode.ioError)));
    expect(checker.check, isNull);
    expect(checker.error!.message, 'cannot open the volume');
    fake.engine.failNext('check_destination', 'internal');
    checker.recheck();
    await pumpEventQueue();
    expect(checker.error!.kind, EngineErrorCode.internal);
    checker.recheck();
    await pumpEventQueue();
    expect(checker.error, isNull, reason: 'a new answer replaces the error');
    expect(checker.check!.isOk, isTrue);
  });

  test('invalidate forgets the verdict and cancels the check on its way, without waiting for anything', () async {
    await checker.checkPath(r'D:\Rescued');
    final pending = checker.checkPath(r'D:\Elsewhere');
    checker.invalidate();
    await expectLater(pending, throwsA(isA<RequestCancelledException>()));
    expect(checker.check, isNull);
    expect(checker.isChecking, isFalse);
    expect(checker.path, r'D:\Elsewhere', reason: 'kept for the next re-check');
    final count = notes.length;
    checker.invalidate();
    expect(notes, hasLength(count), reason: 'nothing changed, nobody is told');
  });

  test('a refusal at job start becomes the verdict and ends a check on its way', () async {
    final pending = checker.checkPath(r'D:\Rescued');
    const refusal = DestinationCheck(
      verdict: DestinationVerdict.overlapsSource,
      reasonName: 'session_range',
      message: 'inside the scanned range',
    );
    checker.adoptRefusal(r'E:\Rescued', refusal);
    await expectLater(pending, throwsA(isA<RequestCancelledException>()));
    expect(checker.path, r'E:\Rescued');
    expect(checker.check, same(refusal));
    expect(checker.isChecking, isFalse);
    await pumpEventQueue();
    expect(checker.check, same(refusal), reason: 'the late answer of the cancelled check is dropped');
  });

  test('dispose while a check is on its way: no notification afterwards', () async {
    final local = DestinationChecker(fake.app.api, purpose: DestinationPurpose.image);
    var notified = 0;
    local.addListener(() => notified++);
    final pending = local.checkPath(r'D:\disk.img');
    final before = notified;
    local.dispose();
    await expectLater(pending, throwsA(isA<RequestCancelledException>()));
    await pumpEventQueue();
    expect(notified, before);
  });
}
