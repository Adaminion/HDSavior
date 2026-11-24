/// Engine errors in plain language, with the technical code on demand.
library;

import 'package:flutter/services.dart';
import 'package:material_ui/material_ui.dart';

import '../../engine/engine.dart';
import '../strings.dart';
import '../theme.dart';
import 'common.dart';

/// What the user is told about [error]: a plain-language headline, the
/// engine's own message when it adds something, and the technical details.
final class ErrorDescription {
  const ErrorDescription({required this.headline, this.message, this.code, this.command});

  /// Describes any error a call may throw. An [EngineException] is
  /// explained by its code; anything else is an unexpected fault of the app.
  factory ErrorDescription.of(Object error) {
    if (error is EngineException) return ErrorDescription.ofError(error.error, command: error.command);
    if (error is EngineError) return ErrorDescription.ofError(error);
    return ErrorDescription(headline: S.unexpectedError, message: '$error');
  }

  factory ErrorDescription.ofError(EngineError error, {String? command}) {
    final headline = S.errorHeadline(error.kind);
    final message = error.message.trim();
    return ErrorDescription(
      headline: headline,
      message: message.isEmpty || message == headline ? null : message,
      code: error.code,
      command: command,
    );
  }

  final String headline;

  /// The engine's explanation (already plain language), if any.
  final String? message;
  final String? code;
  final String? command;

  String get technical => [
    if (code != null) S.errorCode(code!),
    if (command != null) S.errorCommand(command!),
    if (message != null) S.errorEngineMessage(message!),
  ].join('\n');
}

/// An error as a notice: headline, the engine's message, and an expandable
/// "Technical details" with the code.
class ErrorNotice extends StatelessWidget {
  const ErrorNotice({super.key, required this.error, this.actions = const [], this.title});

  final Object error;
  final List<Widget> actions;

  /// Replaces the headline (the plain-language text then follows it).
  final String? title;

  @override
  Widget build(BuildContext context) {
    final description = ErrorDescription.of(error);
    return Notice(
      level: NoticeLevel.danger,
      title: title ?? description.headline,
      body: title == null ? description.message : [description.headline, ?description.message].join(' '),
      actions: [
        ...actions,
        if (description.code != null) TechnicalDetailsButton(description: description),
      ],
    );
  }
}

/// Opens a small dialog with the error code, the request and the engine's
/// message, which the user can copy for a support request.
class TechnicalDetailsButton extends StatelessWidget {
  const TechnicalDetailsButton({super.key, required this.description});

  final ErrorDescription description;

  @override
  Widget build(BuildContext context) {
    return TextButton(
      onPressed: () => showDialog<void>(
        context: context,
        builder: (context) => AlertDialog(
          title: const Text(S.technicalDetails),
          content: SelectableText(description.technical),
          actions: [
            TextButton(
              onPressed: () => Clipboard.setData(ClipboardData(text: description.technical)),
              child: const Text(S.copy),
            ),
            TextButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.close)),
          ],
        ),
      ),
      child: const Text(S.technicalDetails),
    );
  }
}

/// Shows [error] in a dialog. Never throws.
Future<void> showErrorDialog(BuildContext context, Object error, {String? title}) {
  if (!context.mounted) return Future.value();
  final description = ErrorDescription.of(error);
  return showDialog<void>(
    context: context,
    builder: (context) => AlertDialog(
      icon: const Icon(Icons.error_outline),
      title: Text(title ?? S.errorTitle),
      content: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 480),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(description.headline),
            if (description.message != null) ...[const SizedBox(height: 8), Text(description.message!)],
          ],
        ),
      ),
      actions: [
        if (description.code != null) TechnicalDetailsButton(description: description),
        FilledButton(onPressed: () => Navigator.of(context).pop(), child: const Text(S.ok)),
      ],
    ),
  );
}

/// Runs [action] and shows a failure in a dialog instead of throwing.
/// Returns the result, or `null` if it failed or was cancelled.
Future<T?> attempt<T>(BuildContext context, Future<T> Function() action, {String? errorTitle}) async {
  try {
    return await action();
  } on RequestCancelledException {
    return null;
  } on Object catch (error) {
    if (context.mounted) await showErrorDialog(context, error, title: errorTitle);
    return null;
  }
}
