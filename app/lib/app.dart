import 'dart:async';
import 'dart:ui' show AppExitResponse;

import 'package:material_ui/material_ui.dart';

import 'state/state.dart';
import 'ui/app_shell.dart';
import 'ui/format.dart';
import 'ui/strings.dart';
import 'ui/theme.dart';

/// The application: one window with five screens on top of an
/// [AppController].
///
/// The widget starts the engine when it is first built and shuts it down
/// cleanly when the user closes the window (the engine then saves a running
/// scan's session and removes an unfinished output file).
class HdSaviorApp extends StatefulWidget {
  HdSaviorApp({super.key, required this.controller, Formats? formats})
    : formats = formats ?? Formats('en_US');

  final AppController controller;

  /// How dates and numbers are written: the user's regional format
  /// (`main.dart` passes the system's locale; tests use `en_US`).
  final Formats formats;

  @override
  State<HdSaviorApp> createState() => _HdSaviorAppState();
}

class _HdSaviorAppState extends State<HdSaviorApp> {
  late final AppLifecycleListener _lifecycle;

  /// Set while the engine is being shut down before the window closes.
  final ValueNotifier<bool> _closing = ValueNotifier(false);

  @override
  void initState() {
    super.initState();
    _lifecycle = AppLifecycleListener(onExitRequested: _onExitRequested);
    unawaited(widget.controller.start());
  }

  Future<AppExitResponse> _onExitRequested() async {
    _closing.value = true;
    await widget.controller.shutdown();
    return AppExitResponse.exit;
  }

  @override
  void dispose() {
    _lifecycle.dispose();
    _closing.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return FormatsScope(
      formats: widget.formats,
      child: MaterialApp(
        title: S.appName,
        debugShowCheckedModeBanner: false,
        // Light and dark follow the system.
        themeMode: ThemeMode.system,
        theme: AppTheme.light(),
        darkTheme: AppTheme.dark(),
        home: AppShell(app: widget.controller, closing: _closing),
      ),
    );
  }
}
