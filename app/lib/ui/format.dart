/// How numbers, sizes, dates and durations are written on screen.
///
/// The app's text is English, but dates and numbers follow the user's own
/// regional format (a German Windows shows `24.11.2026 03:15` and
/// `12.345 files`), as Windows itself does. Sizes are in binary units with
/// the names Windows uses (1 KB = 1024 bytes), so that what HDSavior shows
/// matches Explorer.
library;

import 'package:flutter/widgets.dart';
import 'package:intl/date_symbol_data_local.dart';
import 'package:intl/intl.dart';

/// Formatting for one locale. Obtain it with [Formats.of].
final class Formats {
  Formats(String locale) : locale = _supported(locale) {
    _number = NumberFormat.decimalPattern(this.locale);
    _decimal1 = NumberFormat('0.0', this.locale);
    _decimal2 = NumberFormat('0.00', this.locale);
    _date = DateFormat.yMd(this.locale);
    _dateTime = DateFormat.yMd(this.locale).add_jm();
  }

  /// Formats for the system's locale (for example `de_DE`), with the date
  /// symbols loaded. Falls back to `en_US`. (The plain constructor knows
  /// `en_US` only, until this has run once.)
  static Future<Formats> forLocale(String? systemLocale) async {
    // The symbols of all locales are compiled in; loading them is cheap and
    // must happen before a locale can even be looked up.
    await initializeDateFormatting();
    return Formats(systemLocale ?? 'en_US');
  }

  static String _supported(String locale) {
    final canonical = Intl.canonicalizedLocale(locale.replaceAll('-', '_'));
    for (final candidate in [canonical, Intl.shortLocale(canonical)]) {
      if (DateFormat.localeExists(candidate) && NumberFormat.localeExists(candidate)) {
        return candidate;
      }
    }
    return 'en_US';
  }

  /// The locale actually used (`en_US` if the requested one is unknown).
  final String locale;

  late final NumberFormat _number;
  late final NumberFormat _decimal1;
  late final NumberFormat _decimal2;
  late final DateFormat _date;
  late final DateFormat _dateTime;

  /// The formats of the nearest [FormatsScope]; `en_US` without one.
  static Formats of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<FormatsScope>()?.formats ?? _fallback;

  static final Formats _fallback = Formats('en_US');

  /// A count with digit grouping: `12,345`.
  String count(int value) => _number.format(value);

  static const List<String> _units = ['KB', 'MB', 'GB', 'TB', 'PB', 'EB'];

  /// A size in binary units as Windows Explorer shows it: three significant
  /// digits, cut off rather than rounded up (Windows never shows a file as
  /// bigger than it is): `0 bytes`, `1 byte`, `512 bytes`, `1.00 KB`,
  /// `12.3 MB`, `3.63 TB`.
  String bytes(int value) {
    if (value < 1024) return value == 1 ? '1 byte' : '${count(value)} bytes';
    var unit = 0;
    var scaled = value / 1024;
    while (scaled >= 1000 && unit < _units.length - 1) {
      // Windows switches to the next unit at 1000, not 1024 ("0.97 MB").
      scaled /= 1024;
      unit++;
    }
    final String text;
    if (scaled >= 100) {
      text = count(scaled.floor());
    } else if (scaled >= 10) {
      text = _decimal1.format((scaled * 10).floor() / 10);
    } else {
      text = _decimal2.format((scaled * 100).floor() / 100);
    }
    return '$text ${_units[unit]}';
  }

  /// A transfer speed: `120 MB/s`.
  String speed(int bytesPerSecond) => '${bytes(bytesPerSecond)}/s';

  /// A local date: `11/24/2026`.
  String date(DateTime? time) => time == null ? '' : _date.format(time.toLocal());

  /// A local date and time: `11/24/2026 3:15 AM`.
  String dateTime(DateTime? time) => time == null ? '' : _dateTime.format(time.toLocal());

  /// A duration for "time left" and "elapsed": `45 s`, `3 min 20 s`,
  /// `2 h 05 min`, `3 d 4 h`.
  String duration(int seconds) {
    if (seconds < 0) seconds = 0;
    if (seconds < 60) return '$seconds s';
    final minutes = seconds ~/ 60;
    if (minutes < 60) return '$minutes min ${(seconds % 60).toString().padLeft(2, '0')} s';
    final hours = minutes ~/ 60;
    if (hours < 48) return '$hours h ${(minutes % 60).toString().padLeft(2, '0')} min';
    return '${hours ~/ 24} d ${hours % 24} h';
  }

  /// A fraction as a whole percentage, never showing 100 % before the end:
  /// `42 %`.
  String percent(double fraction) {
    final value = (fraction.clamp(0, 1) * 100).floor();
    return '${count(value)} %';
  }

  /// A byte offset as an exact count with its size in binary units, for the
  /// expert views: `1,048,576 (1.00 MB)`.
  String offset(int value) => value < 1024 ? count(value) : '${count(value)} (${bytes(value)})';
}

/// Makes [formats] available to the widgets below ([Formats.of]).
class FormatsScope extends InheritedWidget {
  const FormatsScope({super.key, required this.formats, required super.child});

  final Formats formats;

  @override
  bool updateShouldNotify(FormatsScope oldWidget) => oldWidget.formats.locale != formats.locale;
}
