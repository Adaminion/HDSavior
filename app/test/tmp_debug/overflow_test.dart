import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/state/state.dart';

import '../ui/ui_harness.dart';

void main() {
  testWidgets('debug overflow', (tester) async {
    final details = <FlutterErrorDetails>[];
    final fake = await pumpApp(tester, size: minimumWindow);
    final old = FlutterError.onError;
    FlutterError.onError = details.add;
    await fake.scan();
    fake.app.screen.value = AppScreen.results;
    await settle(tester);
    await settle(tester);
    FlutterError.onError = old;
    for (final d in details) {
      debugPrint(d.toString().split('\n').take(40).join('\n'));
    }
    await closeApp(tester, fake);
  });
}
