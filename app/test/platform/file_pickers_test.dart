// The file dialogs: what the plugin-backed implementation asks the platform
// for, and the scripted implementation that tests use instead.
import 'package:file_selector_platform_interface/file_selector_platform_interface.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/platform/file_pickers.dart';

import '../support/fake_platform.dart';

/// Stands in for the native dialogs and records how they were opened.
final class RecordingFileSelector extends FileSelectorPlatform {
  final List<String> calls = [];
  String? answer;

  static String _describe(List<XTypeGroup>? groups) => [
    for (final group in groups ?? const <XTypeGroup>[]) '${group.label}:${group.extensions?.join(',') ?? '*'}',
  ].join('|');

  @override
  Future<XFile?> openFile({
    List<XTypeGroup>? acceptedTypeGroups,
    String? initialDirectory,
    String? confirmButtonText,
  }) async {
    calls.add('openFile [${_describe(acceptedTypeGroups)}] in $initialDirectory');
    return answer == null ? null : XFile(answer!);
  }

  @override
  Future<FileSaveLocation?> getSaveLocation({
    List<XTypeGroup>? acceptedTypeGroups,
    SaveDialogOptions options = const SaveDialogOptions(),
  }) async {
    calls.add(
      'getSaveLocation [${_describe(acceptedTypeGroups)}] in ${options.initialDirectory} '
      'as ${options.suggestedName} mkdir ${options.canCreateDirectories}',
    );
    return answer == null ? null : FileSaveLocation(answer!);
  }

  @override
  Future<String?> getDirectoryPathWithOptions(FileDialogOptions options) async {
    calls.add('getDirectoryPath in ${options.initialDirectory} mkdir ${options.canCreateDirectories}');
    return answer;
  }
}

void main() {
  group('SystemFilePickers', () {
    late RecordingFileSelector platform;
    late FileSelectorPlatform original;
    const pickers = SystemFilePickers();

    setUp(() {
      original = FileSelectorPlatform.instance;
      FileSelectorPlatform.instance = platform = RecordingFileSelector();
    });

    tearDown(() => FileSelectorPlatform.instance = original);

    test('pickFile offers the given type and always "All files"', () async {
      platform.answer = r'D:\images\disk.img';
      final path = await pickers.pickFile(
        label: 'Disk images',
        extensions: ['img', 'dd', 'raw'],
        initialDirectory: r'D:\images',
      );
      expect(path, r'D:\images\disk.img');
      expect(platform.calls, [r'openFile [Disk images:img,dd,raw|All files:*] in D:\images']);
    });

    test('pickFile without extensions accepts any file; a cancelled dialog is null', () async {
      expect(await pickers.pickFile(label: 'Any file'), isNull);
      expect(platform.calls, ['openFile [] in null']);
    });

    test('pickSavePath proposes the name and lets the user create folders', () async {
      platform.answer = r'D:\sessions\scan.hdsc';
      final path = await pickers.pickSavePath(
        label: 'Scan sessions',
        extensions: ['hdsc'],
        suggestedName: 'Disk 1 20261124.hdsc',
      );
      expect(path, r'D:\sessions\scan.hdsc');
      expect(platform.calls, [
        'getSaveLocation [Scan sessions:hdsc|All files:*] in null as Disk 1 20261124.hdsc mkdir true',
      ]);
      platform.answer = null;
      expect(await pickers.pickSavePath(label: 'Scan sessions'), isNull);
    });

    test('pickFolder', () async {
      platform.answer = r'F:\Rescued';
      expect(await pickers.pickFolder(initialDirectory: r'F:\'), r'F:\Rescued');
      expect(platform.calls, [r'getDirectoryPath in F:\ mkdir true']);
      platform.answer = null;
      expect(await pickers.pickFolder(), isNull);
    });
  });

  test('ScriptedFilePickers answers from its list and records what was asked', () async {
    final pickers = ScriptedFilePickers()..answers.addAll([r'D:\a.img', null, r'F:\Rescued']);
    expect(await pickers.pickFile(label: 'Disk images', extensions: ['img']), r'D:\a.img');
    expect(await pickers.pickSavePath(label: 'Sessions', extensions: ['hdsc'], suggestedName: 's.hdsc'), isNull);
    expect(await pickers.pickFolder(), r'F:\Rescued');
    expect(await pickers.pickFolder(), isNull, reason: 'no answer left: cancelled');
    expect(pickers.requests, [
      'pickFile Disk images [img]',
      'pickSavePath Sessions [hdsc] s.hdsc',
      'pickFolder',
      'pickFolder',
    ]);
  });
}
