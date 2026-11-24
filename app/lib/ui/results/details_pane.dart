/// The details pane of the Results screen: everything known about the file
/// or folder in focus, every flag explained, a preview, and for a file
/// system its figures and the user's choices about it.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import '../flows.dart';
import '../format.dart';
import '../strings.dart';
import '../theme.dart';
import '../widgets/common.dart';
import '../widgets/errors.dart';
import 'file_table.dart' show RatingChip;
import 'file_types.dart';
import 'geometry_dialog.dart';
import 'results_model.dart';

/// See the library documentation.
class DetailsPane extends StatelessWidget {
  const DetailsPane({super.key, required this.app, required this.model, this.onClose});

  final AppController app;
  final ResultsModel model;
  final VoidCallback? onClose;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([model, app.browse, app.catalog]),
      builder: (context, _) {
        final target = model.details;
        final info = app.catalog.info;
        final Widget content;
        if (info == null) {
          content = const SizedBox.shrink();
        } else if (target is InstanceTarget) {
          final current = info.instances.where((i) => i.root == target.instance.root).firstOrNull;
          content = current == null ? const SizedBox.shrink() : InstanceDetails(app: app, instance: current);
        } else if (target is RowTarget) {
          content = NodeDetails(key: ValueKey(('row', target.row.id, info.epoch)), app: app, row: target.row);
        } else {
          final folder = app.browse.folder;
          final root = info.instances.where((i) => i.root == folder?.id).firstOrNull;
          if (root != null) {
            content = InstanceDetails(app: app, instance: root);
          } else if (folder != null) {
            content = NodeDetails(key: ValueKey(('folder', folder.id, info.epoch)), app: app, nodeId: folder.id);
          } else {
            content = const SizedBox.shrink();
          }
        }
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 8, 4, 0),
              child: Row(
                children: [
                  Expanded(child: Text(S.detailsTitle, style: Theme.of(context).textTheme.titleSmall)),
                  if (onClose != null)
                    IconButton(tooltip: S.detailsHide, onPressed: onClose, icon: const Icon(Icons.close, size: 18)),
                ],
              ),
            ),
            Expanded(
              child: SingleChildScrollView(padding: const EdgeInsets.fromLTRB(16, 4, 16, 16), child: content),
            ),
          ],
        );
      },
    );
  }
}

/// A file or folder: `node_info`, flags, facts, preview.
class NodeDetails extends StatefulWidget {
  const NodeDetails({super.key, required this.app, this.row, this.nodeId}) : assert(row != null || nodeId != null);

  final AppController app;

  /// The row from the table (shown at once, before `node_info` answers).
  final NodeRow? row;
  final int? nodeId;

  @override
  State<NodeDetails> createState() => _NodeDetailsState();
}

class _NodeDetailsState extends State<NodeDetails> {
  final CancelToken _cancel = CancelToken();
  NodeInfo? _info;
  Object? _error;

  int get _id => widget.row?.id ?? widget.nodeId!;

  @override
  void initState() {
    super.initState();
    unawaited(_load());
  }

  Future<void> _load() async {
    try {
      final info = await widget.app.catalog.nodeInfo(_id, cancel: _cancel);
      if (mounted) setState(() => _info = info);
    } on RequestCancelledException {
      return;
    } on EngineException catch (e) {
      if (mounted) setState(() => _error = e);
    }
  }

  @override
  void dispose() {
    _cancel.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final info = _info;
    final row = info?.row ?? widget.row;
    if (row == null) {
      if (_error != null) return ErrorNotice(error: _error!);
      return const Padding(padding: EdgeInsets.all(16), child: LinearProgressIndicator());
    }
    final flagDetails = info?.flagDetails ?? const <FlagDetail>[];
    final explained = {for (final detail in flagDetails) detail.flag};
    final children = <Widget>[
      Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(iconForRow(row), size: 28, color: theme.colorScheme.primary),
          const SizedBox(width: 10),
          Expanded(child: SelectableText(S.nodeName(row), style: theme.textTheme.titleMedium)),
        ],
      ),
      if (info != null) ...[
        const SizedBox(height: 4),
        SelectableText(
          S.pathLine(info.path),
          style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant),
        ),
      ],
      const SizedBox(height: 12),
      if (row.isFile) ...[
        Row(children: [RatingChip(row: row)]),
        const SizedBox(height: 6),
        Text(S.ratingExplanation(row.rating), style: theme.textTheme.bodySmall),
        const SizedBox(height: 12),
      ],
      _FlagList(
        details: [
          ...flagDetails,
          // Flags the engine did not explain, with our own words.
          for (final name in row.flagNames)
            if (!explained.contains(name)) FlagDetail(flag: name, text: S.flag(FileFlag.parse(name))),
        ],
      ),
      _Heading(S.detailsFacts),
      if (row.isFile)
        FactRow(label: S.factSize, value: '${formats.bytes(row.size)} (${S.bytesExact(formats.count(row.size))})'),
      if (row.isFolder) ...[
        FactRow(label: S.factFiles, value: formats.count(row.files)),
        FactRow(label: S.factSize, value: formats.bytes(row.size)),
        if (row.deletedFiles > 0)
          FactRow(
            label: S.factDeleted,
            value: S.deletedFilesValue(formats.count(row.deletedFiles), formats.bytes(row.deletedSize)),
          ),
      ],
      FactRow(label: S.factModified, value: formats.dateTime(row.modifiedTime).ifEmpty(S.unknown)),
      FactRow(label: S.factCreated, value: formats.dateTime(row.createdTime).ifEmpty(S.unknown)),
      if (info != null)
        FactRow(label: S.factAccessed, value: formats.dateTime(timeFromMs(info.accessed)).ifEmpty(S.unknown)),
      FactRow(label: S.factKind, value: S.nodeKind(row.kind, row.deleted)),
      if (info != null && info.otherNames.isNotEmpty)
        FactRow(label: S.factOtherNames, value: info.otherNames.join('\n')),
      if (row.isFile) ...[_Heading(S.previewTitle), FilePreview(key: ValueKey(row.id), app: widget.app, row: row)],
      if (info != null && (info.record != null || info.data != null)) ...[
        _Heading(S.detailsRecord),
        if (info.record case final record?) ...[
          FactRow(label: S.factRecord, value: record.number == null ? S.unknown : formats.count(record.number!)),
          FactRow(label: S.factSequence, value: formats.count(record.seq)),
          FactRow(label: S.factRecordOffset, value: formats.offset(record.diskOffset)),
          FactRow(label: S.factExtensionRecords, value: formats.count(record.extensionRecords)),
        ],
        if (info.data case final data?) ...[
          FactRow(
            label: S.factStorage,
            value: data.resident ? S.storageResident : S.storageRuns(formats.count(data.runs)),
          ),
          FactRow(label: S.factRealSize, value: formats.count(data.realSize)),
          FactRow(label: S.factInitialized, value: formats.count(data.initializedSize)),
          FactRow(label: S.factAllocated, value: formats.count(data.allocatedSize)),
          if (data.firstLcn != null) FactRow(label: S.factFirstCluster, value: formats.count(data.firstLcn!)),
          if (data.firstOffset != null) FactRow(label: S.factFirstOffset, value: formats.offset(data.firstOffset!)),
        ],
      ],
      if (_error != null) ...[const SizedBox(height: 12), ErrorNotice(error: _error!)],
    ];
    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: children);
  }
}

extension on String {
  String ifEmpty(String other) => isEmpty ? other : this;
}

class _Heading extends StatelessWidget {
  const _Heading(this.text);

  final String text;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.only(top: 16, bottom: 6),
    child: Text(text, style: Theme.of(context).textTheme.titleSmall),
  );
}

class _FlagList extends StatelessWidget {
  const _FlagList({required this.details});

  final List<FlagDetail> details;

  @override
  Widget build(BuildContext context) {
    if (details.isEmpty) return const SizedBox.shrink();
    final theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final detail in details)
          Padding(
            padding: const EdgeInsets.only(bottom: 6),
            child: Text.rich(
              TextSpan(
                children: [
                  TextSpan(
                    text: '${S.flagName(detail.kind, detail.flag)}: ',
                    style: const TextStyle(fontWeight: FontWeight.w600),
                  ),
                  TextSpan(text: detail.text),
                ],
              ),
              style: theme.textTheme.bodySmall,
            ),
          ),
      ],
    );
  }
}

/// What a file looks like: a picture Flutter can decode, otherwise its
/// first kilobytes as text or as hex. Read through `read_file`, the same
/// path recovery uses.
class FilePreview extends StatefulWidget {
  const FilePreview({super.key, required this.app, required this.row});

  final AppController app;
  final NodeRow row;

  /// Pictures up to this size are read whole and shown.
  static const int maxPictureBytes = 16 * 1024 * 1024;

  /// Other files: this many bytes are shown.
  static const int headBytes = 4096;

  static const Set<String> pictureExtensions = {'jpg', 'jpeg', 'png', 'gif', 'bmp', 'webp', 'ico'};

  @override
  State<FilePreview> createState() => _FilePreviewState();
}

class _FilePreviewState extends State<FilePreview> {
  final CancelToken _cancel = CancelToken();
  FileContent? _content;
  Object? _error;
  bool _loading = false;
  bool _asked = false;

  /// The bytes of a "picture" could not be decoded.
  bool _notAPicture = false;

  bool get _picture =>
      FilePreview.pictureExtensions.contains(widget.row.ext) && widget.row.size <= FilePreview.maxPictureBytes;

  @override
  void initState() {
    super.initState();
    // Small files are previewed at once; large pictures on request.
    if (widget.row.size <= 2 * 1024 * 1024 || !_picture) unawaited(_load());
  }

  Future<void> _load() async {
    setState(() {
      _loading = true;
      _asked = true;
      _error = null;
    });
    try {
      final content = await widget.app.catalog.readContent(
        widget.row.id,
        maxBytes: _picture ? widget.row.size : FilePreview.headBytes,
        cancel: _cancel,
      );
      if (mounted) setState(() => _content = content);
    } on RequestCancelledException {
      return;
    } on Object catch (e) {
      if (mounted) setState(() => _error = e);
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  @override
  void dispose() {
    _cancel.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    if (!widget.app.catalog.sourceAttached) {
      return Text(S.previewNeedsSource, style: theme.textTheme.bodySmall);
    }
    if (widget.row.rating == Rating.unsupported) {
      return Text(S.previewUnsupported, style: theme.textTheme.bodySmall);
    }
    if (widget.row.size == 0) return Text(S.previewEmpty, style: theme.textTheme.bodySmall);
    if (!_asked) {
      return Align(
        alignment: Alignment.centerLeft,
        child: OutlinedButton.icon(
          onPressed: _load,
          icon: const Icon(Icons.visibility_outlined),
          label: const Text(S.previewLoad),
        ),
      );
    }
    if (_loading) return const LinearProgressIndicator();
    final error = _error;
    if (error != null) {
      return ErrorNotice(
        error: error,
        title: S.previewFailed,
        actions: [TextButton(onPressed: _load, child: const Text(S.retry))],
      );
    }
    final content = _content;
    if (content == null) return const SizedBox.shrink();
    if (_picture && !_notAPicture) {
      return ClipRRect(
        borderRadius: BorderRadius.circular(8),
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxHeight: 280),
          child: Image.memory(
            content.bytes,
            fit: BoxFit.contain,
            cacheWidth: 640,
            gaplessPlayback: true,
            semanticLabel: S.previewPictureLabel(widget.row.name),
            errorBuilder: (context, error, stack) {
              // Show the bytes instead, outside the picture's frame.
              WidgetsBinding.instance.addPostFrameCallback((_) {
                if (mounted) setState(() => _notAPicture = true);
              });
              return const SizedBox.shrink();
            },
          ),
        ),
      );
    }
    if (_notAPicture) {
      final head = Uint8List.sublistView(content.bytes, 0, content.bytes.length.clamp(0, FilePreview.headBytes));
      return Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(S.previewNotAPicture, style: theme.textTheme.bodySmall),
          const SizedBox(height: 6),
          _BytesView(bytes: head, more: true),
        ],
      );
    }
    return _BytesView(bytes: content.bytes, more: !content.complete);
  }
}

/// The first bytes of a file as text when they look like text, else as a
/// hex dump.
class _BytesView extends StatelessWidget {
  const _BytesView({required this.bytes, this.more = false});

  final Uint8List bytes;
  final bool more;

  /// The bytes as text, or `null` if they do not look like text: valid
  /// UTF-8 (a character cut off at the end is fine) without control
  /// characters other than tab and line breaks.
  static String? asText(Uint8List bytes) {
    if (bytes.isEmpty) return '';
    String text;
    try {
      text = const Utf8Decoder(allowMalformed: false).convert(bytes);
    } on FormatException {
      // A multi-byte character cut by the end of the preview.
      try {
        text = const Utf8Decoder(allowMalformed: false).convert(bytes, 0, bytes.length - _cutTail(bytes));
      } on FormatException {
        return null;
      }
    }
    final controls = text.runes.where((r) => r < 0x20 && r != 0x09 && r != 0x0A && r != 0x0D).length;
    return controls == 0 ? text : null;
  }

  static int _cutTail(Uint8List bytes) {
    var cut = 0;
    for (var i = bytes.length - 1; i >= 0 && i >= bytes.length - 3; i--) {
      cut++;
      if (bytes[i] & 0xC0 != 0x80) break;
    }
    return cut;
  }

  static String hexDump(Uint8List bytes, {int maxBytes = 512}) {
    final lines = <String>[];
    final end = bytes.length < maxBytes ? bytes.length : maxBytes;
    for (var offset = 0; offset < end; offset += 16) {
      final slice = bytes.sublist(offset, offset + 16 > end ? end : offset + 16);
      final hex = [for (final b in slice) b.toRadixString(16).padLeft(2, '0')].join(' ').padRight(47);
      final ascii = String.fromCharCodes([for (final b in slice) b >= 0x20 && b < 0x7F ? b : 0x2E]);
      lines.add('${offset.toRadixString(16).padLeft(4, '0')}  $hex  $ascii');
    }
    return lines.join('\n');
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final text = asText(bytes);
    final mono = theme.textTheme.bodySmall?.copyWith(
      fontFamily: 'Consolas',
      fontFamilyFallback: const ['Cascadia Mono', 'Courier New', 'DejaVu Sans Mono', 'monospace'],
    );
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(text == null ? S.previewHex : S.previewText, style: theme.textTheme.labelSmall),
        const SizedBox(height: 4),
        Container(
          constraints: const BoxConstraints(maxHeight: 240),
          decoration: BoxDecoration(
            color: theme.colorScheme.surfaceContainerHighest,
            borderRadius: BorderRadius.circular(6),
          ),
          padding: const EdgeInsets.all(8),
          child: SingleChildScrollView(
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: SelectableText(text ?? hexDump(bytes), style: mono),
            ),
          ),
        ),
        if (more)
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Text(S.previewMore, style: theme.textTheme.labelSmall),
          ),
      ],
    );
  }
}

/// Exposed for tests.
String? previewText(Uint8List bytes) => _BytesView.asText(bytes);

/// Exposed for tests.
String previewHexDump(Uint8List bytes) => _BytesView.hexDump(bytes);

/// A file system found by the scan: what it is, its figures, its disk
/// layout, and the user's say about it (docs/ALGORITHMS.md D8 and
/// `instance_set`).
class InstanceDetails extends StatelessWidget {
  const InstanceDetails({super.key, required this.app, required this.instance});

  final AppController app;
  final Instance instance;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final formats = Formats.of(context);
    final geometry = instance.geometry;
    final busy = app.jobs.isBusy;
    final canMain = instance.kind != InstanceKind.main && instance.kind != InstanceKind.strays;
    final canOther = instance.kind == InstanceKind.embedded || instance.kind == InstanceKind.older;
    final from = timeFromMs(instance.timeFrom);
    final to = timeFromMs(instance.timeTo);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(iconForInstance(instance), size: 28, color: theme.colorScheme.primary),
            const SizedBox(width: 10),
            Expanded(child: Text(S.instanceName(instance), style: theme.textTheme.titleMedium)),
          ],
        ),
        const SizedBox(height: 8),
        Text(S.instanceDescription(instance), style: theme.textTheme.bodyMedium),
        if (instance.manual) ...[
          const SizedBox(height: 8),
          const Notice(level: NoticeLevel.info, title: S.instanceManual, dense: true),
        ],
        _Heading(S.detailsFacts),
        FactRow(label: S.factFiles, value: formats.count(instance.files)),
        FactRow(label: S.factFolders, value: formats.count(instance.dirs)),
        FactRow(label: S.factSize, value: formats.bytes(instance.bytes)),
        FactRow(label: S.factDeleted, value: formats.count(instance.deletedFiles)),
        if (instance.lostDirs > 0) FactRow(label: S.factLostDirs, value: formats.count(instance.lostDirs)),
        FactRow(label: S.factRecords, value: formats.count(instance.records)),
        if (from != null || to != null)
          FactRow(label: S.factTimeRange, value: S.timeRange(formats.date(from), formats.date(to))),
        if (instance.embeddedIn != null) FactRow(label: S.factEmbeddedIn, value: instance.embeddedIn!),
        _Heading(S.geometryHeading),
        FactRow(label: S.factClusterSize, value: formats.bytes(geometry.clusterSize)),
        FactRow(label: S.factVolumeOffset, value: formats.offset(geometry.volumeOffset)),
        FactRow(label: S.factVerified, value: geometry.validated ? S.yes : S.no),
        Align(
          alignment: Alignment.centerLeft,
          child: TextButton.icon(
            onPressed: () => showGeometryDialog(context, app, instance: instance.id),
            icon: const Icon(Icons.straighten),
            label: const Text(S.geometryOpen),
          ),
        ),
        if (canMain || canOther || instance.manual) ...[
          _Heading(S.instanceChoiceHeading),
          Text(S.instanceChoiceHint, style: theme.textTheme.bodySmall),
          const SizedBox(height: 8),
          Wrap(
            spacing: 8,
            runSpacing: 8,
            children: [
              if (canMain)
                OutlinedButton(
                  onPressed: busy ? null : () => _set(context, InstanceOverride.main),
                  child: const Text(S.instanceMakeMain),
                ),
              if (canOther)
                OutlinedButton(
                  onPressed: busy ? null : () => _set(context, InstanceOverride.other),
                  child: const Text(S.instanceMakeOther),
                ),
              if (instance.manual)
                TextButton(
                  onPressed: busy ? null : () => _set(context, InstanceOverride.auto),
                  child: const Text(S.instanceMakeAuto),
                ),
            ],
          ),
        ],
      ],
    );
  }

  Future<void> _set(BuildContext context, InstanceOverride kind) async {
    final yes = await confirm(
      context,
      title: S.instanceConfirmTitle(kind),
      body: S.instanceConfirmBody(kind),
      confirmLabel: S.instanceConfirm,
    );
    if (!yes || !context.mounted) return;
    await attempt(context, () => app.catalog.setInstanceKind(instance: instance.id, kind: kind));
  }
}
