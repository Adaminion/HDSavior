/// Choosing where HDSavior writes: a path field with a picker, the engine's
/// live verdict on it, and the two consents of docs/ARCHITECTURE.md S3.
library;

import 'dart:async';

import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../../state/state.dart';
import '../format.dart';
import '../strings.dart';
import '../theme.dart';
import 'common.dart';
import 'errors.dart';

/// The user's consents for one location: separate for `same_disk` and
/// `unknown_disk`, and void as soon as the location changes.
@immutable
final class Consents {
  const Consents({this.sameDisk = false, this.unknownDisk = false});

  static const Consents none = Consents();

  final bool sameDisk;
  final bool unknownDisk;

  /// Whether a location with [check] may be used with these consents.
  /// `overlaps_source` never can (S2).
  bool allow(DestinationCheck check) => switch (check.verdict) {
    DestinationVerdict.ok => true,
    DestinationVerdict.sameDisk => sameDisk,
    DestinationVerdict.unknownDisk => unknownDisk,
    _ => false,
  };

  @override
  bool operator ==(Object other) => other is Consents && other.sameDisk == sameDisk && other.unknownDisk == unknownDisk;

  @override
  int get hashCode => Object.hash(sameDisk, unknownDisk);
}

/// A text field for a destination path with a "Browse…" button.
///
/// The path is checked when the user presses Enter, leaves the field, picks
/// a path, or stops typing for a moment. [onChanged] is called at once on
/// every edit (the consents for the old path are void then).
class DestinationField extends StatefulWidget {
  const DestinationField({
    super.key,
    required this.initial,
    required this.label,
    required this.hint,
    required this.onCheck,
    required this.pick,
    this.onChanged,
    this.enabled = true,
    this.fieldKey,
  });

  final String? initial;
  final String label;
  final String hint;

  /// Asks the engine about a path (the owner's destination checker).
  final void Function(String path) onCheck;

  /// Shows the system dialog; `null` when cancelled.
  final Future<String?> Function() pick;
  final void Function(String path)? onChanged;
  final bool enabled;
  final Key? fieldKey;

  @override
  State<DestinationField> createState() => _DestinationFieldState();
}

class _DestinationFieldState extends State<DestinationField> {
  late final TextEditingController _text = TextEditingController(text: widget.initial ?? '');
  final FocusNode _focus = FocusNode();
  Timer? _debounce;
  String? _lastChecked;

  static const Duration _typingPause = Duration(milliseconds: 700);

  @override
  void initState() {
    super.initState();
    _lastChecked = widget.initial;
    _focus.addListener(() {
      if (!_focus.hasFocus) _checkNow();
    });
  }

  @override
  void didUpdateWidget(DestinationField oldWidget) {
    super.didUpdateWidget(oldWidget);
    // A path set from outside (a refused start, a picked file).
    final initial = widget.initial;
    if (initial != null && initial != oldWidget.initial && initial != _text.text) {
      _text.text = initial;
      _lastChecked = initial;
    }
  }

  void _checkNow() {
    _debounce?.cancel();
    final path = _text.text.trim();
    if (path.isEmpty || path == _lastChecked) return;
    _lastChecked = path;
    widget.onCheck(path);
  }

  void _edited(String value) {
    widget.onChanged?.call(value.trim());
    _debounce?.cancel();
    _debounce = Timer(_typingPause, _checkNow);
  }

  Future<void> _browse() async {
    final path = await widget.pick();
    if (path == null || !mounted) return;
    _text.text = path;
    widget.onChanged?.call(path);
    _lastChecked = null;
    _checkNow();
  }

  @override
  void dispose() {
    _debounce?.cancel();
    _text.dispose();
    _focus.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Expanded(
          child: TextField(
            key: widget.fieldKey,
            controller: _text,
            focusNode: _focus,
            enabled: widget.enabled,
            decoration: InputDecoration(labelText: widget.label, hintText: widget.hint),
            onChanged: _edited,
            onSubmitted: (_) => _checkNow(),
          ),
        ),
        const SizedBox(width: 8),
        Padding(
          padding: const EdgeInsets.only(top: 2),
          child: OutlinedButton.icon(
            onPressed: widget.enabled ? _browse : null,
            icon: const Icon(Icons.folder_open_outlined),
            label: const Text(S.browse),
          ),
        ),
      ],
    );
  }
}

/// The engine's verdict on a destination, explained, with the consent
/// buttons for `same_disk` and `unknown_disk`.
class VerdictView extends StatelessWidget {
  const VerdictView({
    super.key,
    required this.checker,
    required this.consents,
    required this.onConsents,
    required this.hasCatalog,
    this.needBytes,
  });

  final DestinationChecker checker;
  final Consents consents;
  final void Function(Consents consents) onConsents;

  /// Whether a catalog exists (the same-disk warning says more without one).
  final bool hasCatalog;

  /// The bytes the job needs, for the free-space message.
  final int? needBytes;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: checker,
      builder: (context, _) {
        final formats = Formats.of(context);
        if (checker.path == null) return const SizedBox.shrink();
        final check = checker.check;
        if (checker.isChecking || (check == null && checker.error == null)) {
          return const _Checking();
        }
        final error = checker.error;
        if (check == null) {
          return ErrorNotice(error: error ?? S.destinationCheckFailed, title: S.destinationCheckFailed);
        }
        final notes = <String>[
          if (check.lostFiles > 0) S.lostFilesHere(formats.count(check.lostFiles), formats.bytes(check.lostBytes)),
        ];
        final space = check.freeBytes == null ? null : S.freeSpace(formats.bytes(check.freeBytes!), check.fs);
        final children = <Widget>[];
        switch (check.verdict) {
          case DestinationVerdict.ok:
            children.add(Notice(level: NoticeLevel.good, title: S.verdictHeadline(check.verdict, null), body: space));
          case DestinationVerdict.sameDisk || DestinationVerdict.unknownDisk:
            final sameDisk = check.verdict == DestinationVerdict.sameDisk;
            final given = sameDisk ? consents.sameDisk : consents.unknownDisk;
            children.add(
              Notice(
                level: NoticeLevel.caution,
                title: S.verdictHeadline(check.verdict, null),
                body: [?S.verdictAdvice(check.verdict, null), ...notes].join(' '),
                details: check.message,
                actions: [
                  if (given) ...[
                    Text(S.consentGiven, style: TextStyle(color: StatusColors.from(context).onCaution)),
                    TextButton(
                      onPressed: () => onConsents(
                        sameDisk
                            ? Consents(sameDisk: false, unknownDisk: consents.unknownDisk)
                            : Consents(sameDisk: consents.sameDisk, unknownDisk: false),
                      ),
                      child: const Text(S.consentWithdraw),
                    ),
                  ] else
                    OutlinedButton(
                      onPressed: () async {
                        final yes = sameDisk
                            ? await askSameDiskConsent(context, hasCatalog: hasCatalog)
                            : await askUnknownDiskConsent(context);
                        if (!yes) return;
                        onConsents(
                          sameDisk
                              ? Consents(sameDisk: true, unknownDisk: consents.unknownDisk)
                              : Consents(sameDisk: consents.sameDisk, unknownDisk: true),
                        );
                      },
                      child: const Text(S.useAnyway),
                    ),
                ],
              ),
            );
          default:
            final reason = check.reason;
            children.add(
              Notice(
                level: NoticeLevel.danger,
                title: S.verdictHeadline(check.verdict, reason),
                body: [?S.verdictAdvice(check.verdict, reason), ...notes].join(' '),
                details: check.message,
              ),
            );
        }
        final usable = consents.allow(check);
        if (usable && check.enoughSpace == false) {
          children.add(
            Notice(
              level: NoticeLevel.danger,
              title: S.notEnoughSpace(
                needBytes == null ? '?' : formats.bytes(needBytes!),
                formats.bytes(check.freeBytes ?? 0),
              ),
              dense: true,
            ),
          );
        }
        if (usable && check.maxFileBytes != null) {
          children.add(
            Notice(
              level: NoticeLevel.caution,
              title: S.fileSizeLimit(check.fs ?? '', formats.bytes(check.maxFileBytes!)),
              dense: true,
            ),
          );
        }
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            for (final (i, child) in children.indexed) ...[if (i > 0) const SizedBox(height: 8), child],
          ],
        );
      },
    );
  }
}

class _Checking extends StatelessWidget {
  const _Checking();

  @override
  Widget build(BuildContext context) => const Padding(
    padding: EdgeInsets.symmetric(vertical: 8),
    child: Row(
      children: [
        SizedBox(width: 18, height: 18, child: CircularProgressIndicator(strokeWidth: 2)),
        SizedBox(width: 12),
        Flexible(child: Text(S.checkingDestination)),
      ],
    ),
  );
}

/// The consent for a location on the source disk outside the protected
/// ranges (verdict `same_disk`). Returns whether the user agreed.
Future<bool> askSameDiskConsent(BuildContext context, {required bool hasCatalog}) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => _ConsentDialog(
      icon: Icons.warning_amber_rounded,
      title: S.sameDiskConsentTitle,
      body: S.sameDiskConsentBody(hasCatalog: hasCatalog),
      checkbox: S.sameDiskConsentCheck,
      confirm: S.sameDiskConsentConfirm,
    ),
  );
  return result ?? false;
}

/// The consent for a location whose disk cannot be determined (verdict
/// `unknown_disk`). Returns whether the user agreed.
Future<bool> askUnknownDiskConsent(BuildContext context) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => const _ConsentDialog(
      icon: Icons.help_outline,
      title: S.unknownDiskConsentTitle,
      body: S.unknownDiskConsentBody,
      checkbox: S.unknownDiskConsentCheck,
      confirm: S.unknownDiskConsentConfirm,
    ),
  );
  return result ?? false;
}

/// A consent the user has to confirm with a check box: the safe choice is
/// the default button.
class _ConsentDialog extends StatefulWidget {
  const _ConsentDialog({
    required this.icon,
    required this.title,
    required this.body,
    required this.checkbox,
    required this.confirm,
  });

  final IconData icon;
  final String title;
  final String body;
  final String checkbox;
  final String confirm;

  @override
  State<_ConsentDialog> createState() => _ConsentDialogState();
}

class _ConsentDialogState extends State<_ConsentDialog> {
  bool _understood = false;

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      icon: Icon(widget.icon),
      title: Text(widget.title),
      content: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 520),
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(widget.body),
              const SizedBox(height: 12),
              CheckboxListTile(
                value: _understood,
                onChanged: (value) => setState(() => _understood = value ?? false),
                title: Text(widget.checkbox),
                controlAffinity: ListTileControlAffinity.leading,
                contentPadding: EdgeInsets.zero,
              ),
            ],
          ),
        ),
      ),
      actions: [
        TextButton(onPressed: _understood ? () => Navigator.of(context).pop(true) : null, child: Text(widget.confirm)),
        FilledButton(
          autofocus: true,
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text(S.chooseAnother),
        ),
      ],
    );
  }
}
