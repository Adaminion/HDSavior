/// Small building blocks shared by the screens: the screen frame, notices,
/// empty states, sections and figures.
library;

import 'package:material_ui/material_ui.dart';

import '../theme.dart';

/// The frame of a screen: a title, a one-line purpose, optional actions on
/// the right, and the content.
///
/// With [scrollable] the content scrolls as a whole (forms, summaries);
/// otherwise it gets the remaining height (tables).
class ScreenFrame extends StatelessWidget {
  const ScreenFrame({
    super.key,
    required this.title,
    required this.purpose,
    this.actions = const [],
    required this.child,
    this.scrollable = true,
    this.maxWidth = 1100,
    this.footer,
  });

  final String title;
  final String purpose;
  final List<Widget> actions;
  final Widget child;
  final bool scrollable;

  /// Content wider than this is centred (long lines are hard to read).
  final double maxWidth;

  /// Always visible below the content, however far it is scrolled: the
  /// button that starts the screen's job.
  final Widget? footer;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final titleBlock = Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(title, style: theme.textTheme.headlineSmall),
        const SizedBox(height: 4),
        Text(purpose, style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant)),
      ],
    );
    final header = Padding(
      padding: const EdgeInsets.fromLTRB(24, 16, 24, 12),
      child: LayoutBuilder(
        builder: (context, constraints) {
          if (actions.isEmpty) return titleBlock;
          final buttons = Wrap(spacing: 8, runSpacing: 8, children: actions);
          // Side by side when there is room, otherwise the actions below.
          if (constraints.maxWidth < 820) {
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [titleBlock, const SizedBox(height: 12), buttons],
            );
          }
          return Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Expanded(child: titleBlock),
              const SizedBox(width: 16),
              buttons,
            ],
          );
        },
      ),
    );
    final body = scrollable
        ? SingleChildScrollView(
            padding: const EdgeInsets.fromLTRB(24, 4, 24, 24),
            child: Align(
              alignment: Alignment.topLeft,
              child: ConstrainedBox(
                constraints: BoxConstraints(maxWidth: maxWidth),
                child: child,
              ),
            ),
          )
        : child;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Align(
          alignment: Alignment.topLeft,
          child: ConstrainedBox(
            constraints: BoxConstraints(maxWidth: scrollable ? maxWidth + 48 : double.infinity),
            child: header,
          ),
        ),
        Expanded(child: body),
        if (footer != null) ...[
          const Divider(),
          Material(
            color: theme.colorScheme.surfaceContainerLow,
            child: Padding(
              padding: const EdgeInsets.fromLTRB(24, 10, 24, 10),
              child: Align(
                alignment: Alignment.centerLeft,
                child: ConstrainedBox(
                  constraints: BoxConstraints(maxWidth: maxWidth),
                  child: footer,
                ),
              ),
            ),
          ),
        ],
      ],
    );
  }
}

/// A notice with an icon, a headline, an explanation and actions, coloured
/// by its [level].
class Notice extends StatelessWidget {
  const Notice({
    super.key,
    required this.level,
    required this.title,
    this.body,
    this.details,
    this.actions = const [],
    this.icon,
    this.dense = false,
  });

  final NoticeLevel level;
  final String title;
  final String? body;

  /// A second paragraph (for example the engine's own message).
  final String? details;
  final List<Widget> actions;
  final IconData? icon;

  /// One line of padding less, for lists of notices.
  final bool dense;

  static IconData iconFor(NoticeLevel level) => switch (level) {
    NoticeLevel.info => Icons.info_outline,
    NoticeLevel.good => Icons.check_circle_outline,
    NoticeLevel.caution => Icons.warning_amber_rounded,
    NoticeLevel.danger => Icons.error_outline,
  };

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final (background, foreground) = StatusColors.from(context).forLevel(level);
    final text = theme.textTheme.bodyMedium?.copyWith(color: foreground);
    return Semantics(
      container: true,
      child: Material(
        color: background,
        borderRadius: BorderRadius.circular(10),
        child: Padding(
          padding: EdgeInsets.symmetric(horizontal: 14, vertical: dense ? 8 : 12),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Icon(icon ?? iconFor(level), color: foreground, size: 22),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(title, style: theme.textTheme.titleSmall?.copyWith(color: foreground)),
                    if (body != null && body!.isNotEmpty) ...[const SizedBox(height: 2), Text(body!, style: text)],
                    if (details != null && details!.isNotEmpty) ...[
                      const SizedBox(height: 4),
                      Text(details!, style: text?.copyWith(fontStyle: FontStyle.italic)),
                    ],
                    if (actions.isNotEmpty) ...[
                      const SizedBox(height: 8),
                      Wrap(spacing: 8, runSpacing: 4, children: actions),
                    ],
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// What a screen shows when there is nothing to show yet: an icon, a
/// headline, an explanation and what to do.
class EmptyState extends StatelessWidget {
  const EmptyState({
    super.key,
    required this.icon,
    required this.title,
    required this.text,
    this.actions = const [],
    this.progress = false,
  });

  final IconData icon;
  final String title;
  final String text;
  final List<Widget> actions;

  /// Shows a progress indicator instead of the icon.
  final bool progress;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return LayoutBuilder(
      builder: (context, constraints) => SingleChildScrollView(
        child: ConstrainedBox(
          constraints: BoxConstraints(minHeight: constraints.maxHeight),
          child: Center(
            child: Padding(
              padding: const EdgeInsets.all(24),
              child: ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: 560),
                child: Column(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    if (progress)
                      const SizedBox(width: 40, height: 40, child: CircularProgressIndicator())
                    else
                      Icon(icon, size: 48, color: theme.colorScheme.primary),
                    const SizedBox(height: 16),
                    Text(title, style: theme.textTheme.titleLarge, textAlign: TextAlign.center),
                    const SizedBox(height: 8),
                    Text(
                      text,
                      textAlign: TextAlign.center,
                      style: theme.textTheme.bodyMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                    ),
                    if (actions.isNotEmpty) ...[
                      const SizedBox(height: 20),
                      Wrap(spacing: 12, runSpacing: 8, alignment: WrapAlignment.center, children: actions),
                    ],
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// A titled card that groups part of a form or a summary.
class Section extends StatelessWidget {
  const Section({super.key, required this.title, this.trailing, required this.child, this.subtitle});

  final String title;
  final String? subtitle;
  final Widget? trailing;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Expanded(child: Text(title, style: theme.textTheme.titleMedium)),
                ?trailing,
              ],
            ),
            if (subtitle != null) ...[
              const SizedBox(height: 2),
              Text(subtitle!, style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant)),
            ],
            const SizedBox(height: 12),
            child,
          ],
        ),
      ),
    );
  }
}

/// A figure with its label, for progress and summaries.
class Figure extends StatelessWidget {
  const Figure({super.key, required this.label, required this.value, this.emphasis = false, this.tooltip});

  final String label;
  final String value;
  final bool emphasis;
  final String? tooltip;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final content = ConstrainedBox(
      constraints: const BoxConstraints(minWidth: 120),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(label, style: theme.textTheme.labelMedium?.copyWith(color: theme.colorScheme.onSurfaceVariant)),
          const SizedBox(height: 2),
          Text(
            value,
            style: (emphasis ? theme.textTheme.titleLarge : theme.textTheme.titleMedium)?.copyWith(
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
        ],
      ),
    );
    return tooltip == null ? content : Tooltip(message: tooltip, child: content);
  }
}

/// A row of [Figure]s that wraps when space runs out.
class Figures extends StatelessWidget {
  const Figures({super.key, required this.children});

  final List<Widget> children;

  @override
  Widget build(BuildContext context) => Wrap(spacing: 24, runSpacing: 12, children: children);
}

/// A label and its value on one line, for fact lists (details pane).
class FactRow extends StatelessWidget {
  const FactRow({super.key, required this.label, required this.value, this.labelWidth = 120});

  final String label;
  final String value;
  final double labelWidth;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 2),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: labelWidth,
            child: Text(label, style: theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.onSurfaceVariant)),
          ),
          Expanded(child: SelectableText(value, style: theme.textTheme.bodySmall)),
        ],
      ),
    );
  }
}

/// A small coloured label, for example a rating.
class StatusChip extends StatelessWidget {
  const StatusChip({super.key, required this.label, required this.colors, this.icon, this.tooltip});

  final String label;
  final (Color, Color) colors;
  final IconData? icon;
  final String? tooltip;

  @override
  Widget build(BuildContext context) {
    final (background, foreground) = colors;
    final chip = DecoratedBox(
      decoration: BoxDecoration(color: background, borderRadius: BorderRadius.circular(6)),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (icon != null) ...[Icon(icon, size: 14, color: foreground), const SizedBox(width: 4)],
            Flexible(
              child: Text(
                label,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: Theme.of(context).textTheme.labelSmall?.copyWith(color: foreground),
              ),
            ),
          ],
        ),
      ),
    );
    return tooltip == null ? chip : Tooltip(message: tooltip, child: chip);
  }
}
