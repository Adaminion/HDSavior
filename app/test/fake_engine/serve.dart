/// The fake engine as a process: `dart test/fake_engine/serve.dart serve`.
///
/// Speaks the protocol over its standard streams like the real
/// `hdsavior-engine serve`, so it can stand in for the engine executable
/// (set HDSAVIOR_ENGINE to a script that runs it) and lets the tests exercise
/// the real process transport.
///
/// Environment: `FAKE_ENGINE_NODES` (nodes of the main file system, default
/// 3000), `FAKE_ENGINE_TICK_MS` (milliseconds per job step, default 0).
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'fake_engine.dart';

Future<void> main(List<String> arguments) async {
  if (arguments.isEmpty || arguments.first != 'serve') {
    stderr.writeln('usage: serve.dart serve');
    exitCode = 2;
    return;
  }
  final environment = Platform.environment;
  final engine = FakeEngine(
    options: FakeEngineOptions(
      mainNodes: int.parse(environment['FAKE_ENGINE_NODES'] ?? '3000'),
      tick: Duration(milliseconds: int.parse(environment['FAKE_ENGINE_TICK_MS'] ?? '0')),
    ),
  );
  engine.stderrLines.listen(stderr.writeln);
  engine.output.listen(stdout.writeln);
  stdin
      .transform(utf8.decoder)
      .transform(const LineSplitter())
      .listen(engine.handleLine, onDone: engine.endOfInput);

  final code = await engine.exitCode;
  await stdout.flush();
  await stderr.flush();
  exit(code);
}
