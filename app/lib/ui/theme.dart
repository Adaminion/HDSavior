/// The look of HDSavior: Material 3, light and dark following the system,
/// a restrained palette and desktop density.
library;

import 'package:material_ui/material_ui.dart';

import '../engine/engine.dart';

/// The calm blue-grey the palette is derived from.
const Color _seed = Color(0xFF2F6F8F);

/// The themes of the app.
abstract final class AppTheme {
  static ThemeData light() => _build(Brightness.light);
  static ThemeData dark() => _build(Brightness.dark);

  static ThemeData _build(Brightness brightness) {
    final colors = ColorScheme.fromSeed(
      seedColor: _seed,
      brightness: brightness,
      dynamicSchemeVariant: DynamicSchemeVariant.tonalSpot,
    );
    final base = ThemeData(
      colorScheme: colors,
      visualDensity: VisualDensity.compact,
      materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
    );
    return base.copyWith(
      extensions: [StatusColors.of(brightness)],
      scaffoldBackgroundColor: colors.surface,
      cardTheme: CardThemeData(
        elevation: 0,
        margin: EdgeInsets.zero,
        color: colors.surfaceContainerLow,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(12),
          side: BorderSide(color: colors.outlineVariant),
        ),
      ),
      dividerTheme: DividerThemeData(color: colors.outlineVariant, space: 1, thickness: 1),
      tooltipTheme: const TooltipThemeData(waitDuration: Duration(milliseconds: 400)),
      inputDecorationTheme: const InputDecorationTheme(border: OutlineInputBorder(), isDense: true),
      navigationRailTheme: NavigationRailThemeData(
        backgroundColor: colors.surfaceContainer,
        indicatorColor: colors.secondaryContainer,
      ),
      chipTheme: base.chipTheme.copyWith(showCheckmark: true),
    );
  }
}

/// Colours with a meaning: the four ratings and the three levels of a
/// notice. Each pair is a container colour and the colour of text and icons
/// on it, chosen for contrast in light and dark.
@immutable
class StatusColors extends ThemeExtension<StatusColors> {
  const StatusColors({
    required this.good,
    required this.onGood,
    required this.caution,
    required this.onCaution,
    required this.danger,
    required this.onDanger,
    required this.neutral,
    required this.onNeutral,
  });

  final Color good;
  final Color onGood;
  final Color caution;
  final Color onCaution;
  final Color danger;
  final Color onDanger;
  final Color neutral;
  final Color onNeutral;

  static StatusColors of(Brightness brightness) => brightness == Brightness.light
      ? const StatusColors(
          good: Color(0xFFD7EFD8),
          onGood: Color(0xFF1B4D20),
          caution: Color(0xFFFCE8B2),
          onCaution: Color(0xFF5A4300),
          danger: Color(0xFFFADAD6),
          onDanger: Color(0xFF7A1A12),
          neutral: Color(0xFFE3E6EA),
          onNeutral: Color(0xFF3A4048),
        )
      : const StatusColors(
          good: Color(0xFF1F3D22),
          onGood: Color(0xFFB8E6BC),
          caution: Color(0xFF443511),
          onCaution: Color(0xFFF7DE9A),
          danger: Color(0xFF4F1F1A),
          onDanger: Color(0xFFFFC9C1),
          neutral: Color(0xFF30353B),
          onNeutral: Color(0xFFD4D9DF),
        );

  /// The colours of the nearest theme.
  static StatusColors from(BuildContext context) =>
      Theme.of(context).extension<StatusColors>() ?? of(Theme.of(context).brightness);

  /// Container and content colour for [rating].
  (Color, Color) forRating(Rating? rating) => switch (rating) {
    Rating.good => (good, onGood),
    Rating.uncertain => (caution, onCaution),
    Rating.damaged => (danger, onDanger),
    _ => (neutral, onNeutral),
  };

  /// Container and content colour for [level].
  (Color, Color) forLevel(NoticeLevel level) => switch (level) {
    NoticeLevel.info => (neutral, onNeutral),
    NoticeLevel.good => (good, onGood),
    NoticeLevel.caution => (caution, onCaution),
    NoticeLevel.danger => (danger, onDanger),
  };

  @override
  StatusColors copyWith() => this;

  @override
  StatusColors lerp(StatusColors? other, double t) {
    if (other == null) return this;
    return StatusColors(
      good: Color.lerp(good, other.good, t)!,
      onGood: Color.lerp(onGood, other.onGood, t)!,
      caution: Color.lerp(caution, other.caution, t)!,
      onCaution: Color.lerp(onCaution, other.onCaution, t)!,
      danger: Color.lerp(danger, other.danger, t)!,
      onDanger: Color.lerp(onDanger, other.onDanger, t)!,
      neutral: Color.lerp(neutral, other.neutral, t)!,
      onNeutral: Color.lerp(onNeutral, other.onNeutral, t)!,
    );
  }
}

/// How serious a notice is.
enum NoticeLevel {
  info,
  good,
  caution,
  danger;

  /// The level of an engine warning. Red is kept for what can cost the
  /// user's files: a device that may have erased them, a disk Windows keeps
  /// writing to, a source that reads as empty.
  static NoticeLevel ofWarning(WarningCode code) => switch (code) {
    WarningCode.trimSupported ||
    WarningCode.pageFileOnSource ||
    WarningCode.systemDisk ||
    WarningCode.appOnSource ||
    WarningCode.sourceReadsZero => danger,
    WarningCode.identityDiffers => info,
    _ => caution,
  };
}
