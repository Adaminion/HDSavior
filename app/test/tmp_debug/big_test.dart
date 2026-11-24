import 'package:flutter_test/flutter_test.dart';
import '../support/fake_app.dart';
void main() {
  test('time', () async {
    final sw = Stopwatch()..start();
    final fake = FakeApp(mainNodes: 6000000);
    await fake.start();
    await fake.scan();
    print('scan ${sw.elapsedMilliseconds} ms; huge ${fake.landmarks.hugeFolderFiles}; nodes ${fake.app.catalog.info!.nodes}');
    await fake.dispose();
  });
}
