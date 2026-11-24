import 'package:flutter_test/flutter_test.dart';
import '../support/fake_app.dart';

void main() {
  test('debug', () async {
    final fake = FakeApp();
    await fake.start();
    print('engine status ${fake.app.engine.status} error ${fake.app.engine.error}');
    print('disks ${fake.app.source.disks?.length} err ${fake.app.source.disksError}');
    try {
      await fake.openDisk();
    } catch (e, st) {
      print('openDisk threw $e\n$st');
    }
    print('current ${fake.app.source.current}');
    print(fake.commands);
    await fake.dispose();
  });
}
