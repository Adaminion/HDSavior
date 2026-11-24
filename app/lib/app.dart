import 'dart:async';
import 'dart:ui' show AppExitResponse;

import 'package:material_ui/material_ui.dart';

import 'state/state.dart';
import 'ui/app_shell.dart';

/// The application: one window with five screens on top of an
/// [AppController].
///
/// The widget starts the engine when it is first built and shuts it down
/// cleanly when the user closes the window (the engine then saves a running
/// scan's session and removes an unfinished output file).
class HdSaviorApp extends StatefulWidget {
  const HdSaviorApp({super.key, required this.controller});

  final AppController controller;

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

  static ThemeData _theme(Brightness brightness) => ThemeData(
    colorScheme: ColorScheme.fromSeed(
      seedColor: const Color(0xFF2F6F8F),
      brightness: brightness,
    ),
    visualDensity: VisualDensity.compact,
  );

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'HDSavior',
      debugShowCheckedModeBanner: false,
      theme: _theme(Brightness.light),
      darkTheme: _theme(Brightness.dark),
      home: AppShell(app: widget.controller, closing: _closing),
    );
  }
}
