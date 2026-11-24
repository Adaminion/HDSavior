// The small platform interface: session directory, creating it, opening a
// folder in the file manager. The Windows behaviour is tested on any system
// by passing the environment in.
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/platform/platform_services.dart';

import '../support/fake_platform.dart';

void main() {
  test('the session directory on Windows and elsewhere', () {
    expect(
      IoPlatformServices(isWindows: true, environment: const {'LOCALAPPDATA': r'C:\Users\Ann\AppData\Local'}).defaultSessionDirectory,
      r'C:\Users\Ann\AppData\Local\HDSavior\Sessions',
    );
    expect(
      IoPlatformServices(isWindows: true, environment: const {'USERPROFILE': r'C:\Users\Ann', 'LOCALAPPDATA': ' '}).defaultSessionDirectory,
      r'C:\Users\Ann\AppData\Local\HDSavior\Sessions',
    );
    expect(IoPlatformServices(isWindows: true, environment: const {}).defaultSessionDirectory, r'C:\ProgramData\HDSavior\Sessions');
    expect(
      IoPlatformServices(isWindows: false, environment: const {'HOME': '/home/ann'}).defaultSessionDirectory,
      '/home/ann/.local/share/hdsavior/sessions',
    );
    expect(
      IoPlatformServices(isWindows: false, environment: const {'HOME': '/home/ann', 'XDG_DATA_HOME': '/data'}).defaultSessionDirectory,
      '/data/hdsavior/sessions',
    );
    expect(IoPlatformServices(isWindows: true, environment: const {}).pathSeparator, r'\');
    expect(IoPlatformServices(isWindows: false, environment: const {}).pathSeparator, '/');
    // The running process: whatever it is, the directory is an absolute path.
    expect(IoPlatformServices().defaultSessionDirectory, anyOf(startsWith('/'), matches(r'^[A-Za-z]:\\')));
  });

  test('createDirectory creates the directory with its parents', () async {
    final temp = await Directory.systemTemp.createTemp('hdsavior-test-');
    addTearDown(() => temp.delete(recursive: true));
    final target = '${temp.path}${Platform.pathSeparator}a${Platform.pathSeparator}b';
    await IoPlatformServices().createDirectory(target);
    expect(Directory(target).existsSync(), isTrue);
    await IoPlatformServices().createDirectory(target); // already there: fine
  });

  group('openFolder', () {
    // Each entry: the executable followed by its arguments.
    late List<List<String>> started;
    Future<ProcessResult> Function(String, List<String>) runner(int exitCode) => (executable, arguments) async {
      started.add([executable, ...arguments]);
      return ProcessResult(1, exitCode, '', '');
    };

    setUp(() => started = []);

    test('Windows: Explorer gets the path as one argument, whatever it contains', () async {
      final platform = IoPlatformServices(isWindows: true, environment: const {}, run: runner(1));
      const path = r'D:\Rescued files & "more" (2026)';
      // Explorer exits with 1 even when it opened the window.
      expect(await platform.openFolder(path), isTrue);
      expect(started, [
        ['explorer.exe', path],
      ]);
    });

    test('elsewhere: xdg-open, and its exit code counts', () async {
      expect(await IoPlatformServices(isWindows: false, environment: const {}, run: runner(0)).openFolder('/mnt/rescued'), isTrue);
      expect(started, [
        ['xdg-open', '/mnt/rescued'],
      ]);
      expect(await IoPlatformServices(isWindows: false, environment: const {}, run: runner(4)).openFolder('/mnt/rescued'), isFalse);
    });

    test('openFile starts the same program with the file as one argument', () async {
      const report = r'D:\Rescued\HDSavior-report-20261124-031500.csv';
      expect(await IoPlatformServices(isWindows: true, environment: const {}, run: runner(1)).openFile(report), isTrue);
      expect(await IoPlatformServices(isWindows: false, environment: const {}, run: runner(0)).openFile('/r/report.csv'), isTrue);
      expect(started, [
        ['explorer.exe', report],
        ['xdg-open', '/r/report.csv'],
      ]);
    });

    test('a file manager that cannot be started is reported, not thrown', () async {
      final platform = IoPlatformServices(
        isWindows: false,
        environment: const {},
        run: (executable, arguments) => throw const ProcessException('xdg-open', [], 'No such file or directory', 2),
      );
      expect(await platform.openFolder('/mnt/rescued'), isFalse);
      expect(await platform.openFile('/mnt/rescued/report.csv'), isFalse);
    });
  });

  test('InertPlatformServices only records', () async {
    final platform = InertPlatformServices(defaultSessionDirectory: '/sessions', pathSeparator: '/');
    await platform.createDirectory('/sessions');
    expect(await platform.openFolder('/rescued'), isTrue);
    expect(platform.createdDirectories, ['/sessions']);
    expect(platform.openedFolders, ['/rescued']);
    expect(await platform.openFile('/rescued/report.csv'), isTrue);
    expect(platform.openedFiles, ['/rescued/report.csv']);
    expect(Directory('/sessions').existsSync(), isFalse);
  });
}
