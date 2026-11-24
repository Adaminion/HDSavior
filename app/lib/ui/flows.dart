/// Flows that more than one screen starts: scanning (with the choice of the
/// session file), opening an image or a saved session, attaching the disk
/// of a session, saving a session elsewhere.
library;

import 'dart:async';

import 'package:flutter/services.dart';
import 'package:material_ui/material_ui.dart';

import '../engine/engine.dart';
import '../state/state.dart';
import 'format.dart';
import 'strings.dart';
import 'theme.dart';
import 'widgets/common.dart';
import 'widgets/destination.dart';
import 'widgets/errors.dart';

/// Extensions of raw disk images.
const List<String> imageExtensions = ['img', 'dd', 'raw', 'bin', '001', 'ima'];

/// Extension of HDSavior session files.
const List<String> sessionExtensions = ['hdsc'];

/// Starts a scan of [range] of the open source (`null`: the whole source).
///
/// The session file is chosen automatically in the app's session directory
/// and checked by the destination guard (purpose `session`); if the guard
/// refuses it, the user is asked for a folder (with the consents of S3), or
/// can scan without saving. A refusal of `scan_start` itself leads back to
/// that question. Shows the Scan screen once the scan is accepted.
Future<void> startScan(BuildContext context, AppController app, {ByteRange? range}) async {
  if (app.catalog.hasCatalog) {
    final replace = await confirm(context, title: S.newScanTitle, body: S.newScanBody, confirmLabel: S.newScanConfirm);
    if (!replace || !context.mounted) return;
  }
  final plan = await attempt(context, () => app.source.prepareSessionFile(range: range));
  if (plan == null || !context.mounted) return;
  SessionChoice? choice = plan.check.isOk
      ? SessionChoice(path: plan.path)
      : await askSessionLocation(context, app, plan: plan, refusal: plan.check);
  while (choice != null && context.mounted) {
    app.screen.value = AppScreen.scan;
    try {
      await app.scan.start(
        range: range,
        sessionPath: choice.path,
        allowSameDisk: choice.consents.sameDisk,
        allowUnknownDisk: choice.consents.unknownDisk,
      );
      return;
    } on EngineException catch (e) {
      if (!context.mounted) return;
      final refusal = e.destinationCheck;
      if (refusal == null) {
        await showErrorDialog(context, e);
        return;
      }
      choice = await askSessionLocation(context, app, plan: plan, refusal: refusal);
    }
  }
}

/// Continues the loaded session's scan (`scan_start` with `resume`). If the
/// engine refuses the session file's location now (for example after the
/// warning `session_file_at_risk`), the user is asked for another place.
Future<void> resumeScan(BuildContext context, AppController app) async {
  app.screen.value = AppScreen.scan;
  SessionChoice? choice;
  while (context.mounted) {
    try {
      await app.scan.resume(
        sessionPath: choice?.path,
        allowSameDisk: choice?.consents.sameDisk ?? false,
        allowUnknownDisk: choice?.consents.unknownDisk ?? false,
      );
      return;
    } on EngineException catch (e) {
      if (!context.mounted) return;
      final refusal = e.destinationCheck;
      final suggested = app.source.sessionPath ?? app.source.suggestSessionPath();
      if (refusal == null || suggested == null) {
        await showErrorDialog(context, e, title: S.resumeFailed);
        return;
      }
      choice = await askSessionLocation(
        context,
        app,
        plan: SessionFilePlan(path: suggested, check: refusal),
        refusal: refusal,
        // A resumed scan continues a session file; it cannot drop it.
        allowUnsaved: false,
      );
      if (choice == null) return;
    }
  }
}

/// Where the scan saves its progress, and the user's consents for it.
@immutable
final class SessionChoice {
  const SessionChoice({required this.path, this.consents = Consents.none});

  /// `null`: the scan is not saved.
  final String? path;
  final Consents consents;
}

/// Asks where the scan's session file goes after the guard refused the
/// usual place ([refusal]). Returns `null` if the user cancels.
Future<SessionChoice?> askSessionLocation(
  BuildContext context,
  AppController app, {
  required SessionFilePlan plan,
  required DestinationCheck refusal,
  bool allowUnsaved = true,
}) => showDialog<SessionChoice>(
  context: context,
  builder: (context) => _SessionLocationDialog(app: app, plan: plan, refusal: refusal, allowUnsaved: allowUnsaved),
);

class _SessionLocationDialog extends StatefulWidget {
  const _SessionLocationDialog({
    required this.app,
    required this.plan,
    required this.refusal,
    required this.allowUnsaved,
  });

  final AppController app;
  final SessionFilePlan plan;
  final DestinationCheck refusal;

  /// Whether "Scan without saving" is offered.
  final bool allowUnsaved;

  @override
  State<_SessionLocationDialog> createState() => _SessionLocationDialogState();
}

class _SessionLocationDialogState extends State<_SessionLocationDialog> {
  late final DestinationChecker _checker = DestinationChecker(widget.app.api, purpose: DestinationPurpose.session);
  Consents _consents = Consents.none;

  String _join(String folder) {
    final separator = widget.app.platform.pathSeparator;
    final base = folder.endsWith(separator) || folder.endsWith('/') ? folder : '$folder$separator';
    return '$base${widget.plan.fileName}';
  }

  void _check(String folder) {
    setState(() => _consents = Consents.none);
    _checker.checkPath(_join(folder)).ignore();
  }

  @override
  void dispose() {
    _checker.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final refusal = widget.refusal;
    return AlertDialog(
      title: const Text(S.sessionLocationTitle),
      content: SizedBox(
        width: 560,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              const Text(S.sessionLocationIntro),
              const SizedBox(height: 12),
              Notice(
                level: NoticeLevel.caution,
                title: S.sessionLocationRefused(S.verdictHeadline(refusal.verdict, refusal.reason)),
                body: refusal.message,
                dense: true,
              ),
              const SizedBox(height: 16),
              DestinationField(
                initial: null,
                label: S.sessionFolderLabel,
                hint: S.sessionFolderHint,
                onCheck: _check,
                onChanged: (_) => setState(() => _consents = Consents.none),
                pick: () => widget.app.pickers.pickFolder(),
              ),
              const SizedBox(height: 12),
              VerdictView(
                checker: _checker,
                consents: _consents,
                onConsents: (consents) => setState(() => _consents = consents),
                hasCatalog: widget.app.catalog.hasCatalog,
              ),
            ],
          ),
        ),
      ),
      actions: [
        if (widget.allowUnsaved)
          TextButton(
            onPressed: () async {
              final yes = await confirm(
                context,
                title: S.scanWithoutSavingTitle,
                body: S.scanWithoutSavingBody,
                confirmLabel: S.scanWithoutSavingConfirm,
              );
              if (yes && context.mounted) Navigator.of(context).pop(const SessionChoice(path: null));
            },
            child: const Text(S.scanWithoutSaving),
          ),
        TextButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.cancel)),
        ListenableBuilder(
          listenable: _checker,
          builder: (context, _) {
            final check = _checker.check;
            final path = _checker.path;
            final usable = check != null && path != null && _consents.allow(check);
            return FilledButton(
              onPressed: usable
                  ? () => Navigator.of(context).pop(SessionChoice(path: path, consents: _consents))
                  : null,
              child: const Text(S.sessionLocationConfirm),
            );
          },
        ),
      ],
    );
  }
}

/// Asks a yes/no question. The safe answer ("no") is the default.
Future<bool> confirm(
  BuildContext context, {
  required String title,
  required String body,
  required String confirmLabel,
  String cancelLabel = S.cancel,
  bool dangerous = false,
}) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(title),
      content: ConstrainedBox(constraints: const BoxConstraints(maxWidth: 480), child: Text(body)),
      actions: [
        TextButton(autofocus: true, onPressed: () => Navigator.of(context).pop(false), child: Text(cancelLabel)),
        FilledButton(onPressed: () => Navigator.of(context).pop(true), child: Text(confirmLabel)),
      ],
    ),
  );
  return result ?? false;
}

/// Lets the user pick a disk image file and opens it as the source.
Future<void> openImageFile(BuildContext context, AppController app) async {
  final path = await app.pickers.pickFile(label: S.imageFilesLabel, extensions: imageExtensions);
  if (path == null || !context.mounted) return;
  await attempt(context, () => app.source.openImage(path), errorTitle: S.openImageFailed);
}

/// Lets the user pick a saved scan and opens it; shows the results.
Future<void> openSessionFile(BuildContext context, AppController app, {String? path}) async {
  if (app.catalog.hasCatalog) {
    final replace = await confirm(
      context,
      title: S.openSessionReplaceTitle,
      body: S.openSessionReplaceBody,
      confirmLabel: S.openSessionReplaceConfirm,
    );
    if (!replace || !context.mounted) return;
  }
  final chosen = path ?? await app.pickers.pickFile(label: S.sessionFilesLabel, extensions: sessionExtensions);
  if (chosen == null || !context.mounted) return;
  final result = await attempt(context, () => app.source.loadSession(chosen), errorTitle: S.openSessionFailed);
  if (result != null) app.screen.value = AppScreen.results;
}

/// Saves the session to a place the user chooses (`session_save`), for
/// example after the warning `session_file_at_risk`.
Future<void> saveSessionElsewhere(BuildContext context, AppController app) async {
  final path = await app.pickers.pickSavePath(
    label: S.sessionFilesLabel,
    extensions: sessionExtensions,
    suggestedName: app.source.suggestSessionPath()?.split(RegExp(r'[\\/]')).last,
  );
  if (path == null || !context.mounted) return;
  var consents = Consents.none;
  while (context.mounted) {
    try {
      await app.source.saveSession(path, allowSameDisk: consents.sameDisk, allowUnknownDisk: consents.unknownDisk);
      if (context.mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(S.sessionSaved(path))));
      }
      return;
    } on EngineException catch (e) {
      if (!context.mounted) return;
      final check = e.destinationCheck;
      final bool again;
      if (check?.verdict == DestinationVerdict.sameDisk && !consents.sameDisk) {
        again = await askSameDiskConsent(context, hasCatalog: app.catalog.hasCatalog);
        consents = Consents(sameDisk: again, unknownDisk: consents.unknownDisk);
      } else if (check?.verdict == DestinationVerdict.unknownDisk && !consents.unknownDisk) {
        again = await askUnknownDiskConsent(context);
        consents = Consents(sameDisk: consents.sameDisk, unknownDisk: again);
      } else {
        await showErrorDialog(context, e, title: S.saveSessionFailed);
        return;
      }
      if (!again) return;
    } on Object catch (e) {
      if (context.mounted) await showErrorDialog(context, e, title: S.saveSessionFailed);
      return;
    }
  }
}

/// Lets the user connect the disk (or image) of a loaded session
/// (`session_attach_source`). [preferPath] is preselected (the recorded
/// source). Returns whether a source is attached afterwards.
Future<bool> attachSourceFlow(BuildContext context, AppController app) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => _AttachDialog(app: app),
  );
  return result ?? false;
}

class _AttachDialog extends StatefulWidget {
  const _AttachDialog({required this.app});

  final AppController app;

  @override
  State<_AttachDialog> createState() => _AttachDialogState();
}

class _AttachDialogState extends State<_AttachDialog> {
  String? _diskPath;
  EngineException? _error;
  bool _working = false;

  AppController get app => widget.app;

  @override
  void initState() {
    super.initState();
    final recorded = app.source.current?.info;
    if (recorded?.kind == SourceKind.disk) _diskPath = recorded!.path;
    // Not during this build: the refresh notifies its listeners at once.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) unawaited(app.source.refreshDisks());
    });
  }

  Future<void> _attach(SourceKind kind, String path, {bool force = false}) async {
    setState(() {
      _working = true;
      _error = null;
    });
    try {
      await app.source.attachSource(kind: kind, path: path, force: force);
      if (mounted) Navigator.of(context).pop(true);
    } on EngineException catch (e) {
      if (mounted) setState(() => _error = e);
    } finally {
      if (mounted) setState(() => _working = false);
    }
  }

  Future<void> _image() async {
    final path = await app.pickers.pickFile(label: S.imageFilesLabel, extensions: imageExtensions);
    if (path != null && mounted) await _attach(SourceKind.image, path);
  }

  @override
  Widget build(BuildContext context) {
    final formats = Formats.of(context);
    final recorded = app.source.current?.info;
    return AlertDialog(
      title: const Text(S.attachTitle),
      content: SizedBox(
        width: 560,
        child: ListenableBuilder(
          listenable: app.source,
          builder: (context, _) {
            final disks = app.source.disks ?? const <Disk>[];
            return SingleChildScrollView(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Text(recorded == null ? S.attachIntroUnknown : S.attachIntro(describeSource(recorded, formats))),
                  const SizedBox(height: 12),
                  if (app.source.isLoadingDisks) const LinearProgressIndicator(),
                  RadioGroup<String>(
                    groupValue: _diskPath,
                    onChanged: (value) => setState(() => _diskPath = value),
                    child: Column(
                      children: [
                        for (final disk in disks)
                          RadioListTile<String>(
                            value: disk.path,
                            enabled: !_working,
                            title: Text(S.diskTitle(disk.model, disk.number)),
                            subtitle: Text(S.diskSubtitle(disk.number, formats.bytes(disk.size), disk.bus)),
                            dense: true,
                          ),
                      ],
                    ),
                  ),
                  if (disks.isEmpty && !app.source.isLoadingDisks) const Text(S.attachNoDisks),
                  if (_error != null) ...[
                    const SizedBox(height: 12),
                    ErrorNotice(
                      error: _error!,
                      actions: [
                        if (_error!.kind == EngineErrorCode.sourceMismatch && _diskPath != null)
                          TextButton(
                            onPressed: _working ? null : () => _attach(SourceKind.disk, _diskPath!, force: true),
                            child: const Text(S.attachForce),
                          ),
                      ],
                    ),
                  ],
                ],
              ),
            );
          },
        ),
      ),
      actions: [
        TextButton(onPressed: _working ? null : _image, child: const Text(S.attachImage)),
        TextButton(onPressed: () => Navigator.of(context).pop(false), child: const Text(S.cancel)),
        FilledButton(
          onPressed: _working || _diskPath == null ? null : () => _attach(SourceKind.disk, _diskPath!),
          child: const Text(S.attachConfirm),
        ),
      ],
    );
  }
}

/// One line that names a source: `WDC WD40EZRZ (disk 1, 3.63 TB)`.
String describeSource(SourceInfo info, Formats formats) => info.kind == SourceKind.disk
    ? S.diskSource(info.model, info.diskNumber, formats.bytes(info.size))
    : S.imageSource(info.path, formats.bytes(info.size));

/// Shows a folder in the file manager or opens a file. Never fails: if no
/// program can be started, the path is shown with a way to copy it.
Future<void> openPath(BuildContext context, AppController app, String path, {required bool folder}) async {
  bool opened;
  try {
    opened = folder ? await app.platform.openFolder(path) : await app.platform.openFile(path);
  } on Object {
    opened = false;
  }
  if (opened || !context.mounted) return;
  ScaffoldMessenger.of(context).showSnackBar(
    SnackBar(
      content: Text(S.openFailed(path)),
      duration: const Duration(seconds: 10),
      action: SnackBarAction(
        label: S.copyPath,
        onPressed: () => Clipboard.setData(ClipboardData(text: path)),
      ),
    ),
  );
}
