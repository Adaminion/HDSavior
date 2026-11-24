// How numbers, sizes, dates and durations are written, and that every
// engine error, warning, flag and verdict has plain-language text.
import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/ui/format.dart';
import 'package:hdsavior/ui/strings.dart';
import 'package:hdsavior/ui/widgets/errors.dart';

void main() {
  group('Formats', () {
    final en = Formats('en_US');

    test('sizes are binary units, three significant digits, cut off like Explorer', () {
      expect(en.bytes(0), '0 bytes');
      expect(en.bytes(1), '1 byte');
      expect(en.bytes(1023), '1,023 bytes');
      expect(en.bytes(1024), '1.00 KB');
      expect(en.bytes(1536), '1.50 KB');
      expect(en.bytes(1024 * 1024 - 1), '0.99 MB', reason: 'never rounded up to 1.00 MB');
      expect(en.bytes(10 * 1024 * 1024), '10.0 MB');
      expect(en.bytes(123 * 1024 * 1024 + 900 * 1024), '123 MB');
      expect(en.bytes(4000787030016), '3.63 TB', reason: 'what Windows shows for a "4 TB" disk');
      expect(en.bytes(999 * 1024), '999 KB');
      expect(en.bytes(1000 * 1024), '0.97 MB', reason: 'Windows moves to the next unit at 1000');
      expect(en.bytes(1 << 60), '1.00 EB');
    });

    test('counts, speeds, percentages', () {
      expect(en.count(1234567), '1,234,567');
      expect(en.speed(157286400), '150 MB/s');
      expect(en.percent(0.426), '42 %');
      expect(en.percent(0.999), '99 %', reason: 'never 100 % before the end');
      expect(en.percent(1.5), '100 %');
      expect(en.offset(1048576), '1,048,576 (1.00 MB)');
      expect(en.offset(512), '512');
    });

    test('durations', () {
      expect(en.duration(-3), '0 s');
      expect(en.duration(45), '45 s');
      expect(en.duration(200), '3 min 20 s');
      expect(en.duration(7500), '2 h 05 min');
      expect(en.duration(3 * 86400 + 4 * 3600 + 59), '3 d 4 h');
    });

    test('dates follow the locale', () async {
      final time = DateTime(2026, 11, 24, 15, 5);
      expect(en.dateTime(time), '11/24/2026 3:05 PM');
      expect(en.date(time), '11/24/2026');
      expect(en.dateTime(null), '');
      final de = await Formats.forLocale('de-DE');
      expect(de.locale, startsWith('de'));
      expect(de.dateTime(time), '24.11.2026 15:05');
      expect(de.count(12345), '12.345');
      expect(de.bytes(1536), '1,50 KB');
    });

    test('an unknown locale falls back to en_US', () async {
      expect((await Formats.forLocale('xx_YY')).locale, 'en_US');
      expect((await Formats.forLocale(null)).locale, 'en_US');
      expect((await Formats.forLocale('fr')).locale, 'fr');
    });
  });

  group('plain language', () {
    test('every error code has its own headline', () {
      final headlines = {for (final code in EngineErrorCode.values) code: S.errorHeadline(code)};
      expect(headlines.values.every((h) => h.isNotEmpty), isTrue);
      expect(headlines.values.toSet(), hasLength(headlines.length));
    });

    test('an engine error is described by its code, with the engine message and the technical details', () {
      final error = EngineException(
        const EngineError(code: 'source_lost', message: 'The device stopped answering.'),
        command: 'scan_start',
      );
      final description = ErrorDescription.of(error);
      expect(description.headline, S.errorHeadline(EngineErrorCode.sourceLost));
      expect(description.message, 'The device stopped answering.');
      expect(description.technical, contains('source_lost'));
      expect(description.technical, contains('scan_start'));
      // A newer engine's code still gets a headline, and keeps its code.
      final unknown = ErrorDescription.of(const EngineError(code: 'quantum_flux', message: 'm'));
      expect(unknown.headline, S.errorHeadline(EngineErrorCode.unknown));
      expect(unknown.technical, contains('quantum_flux'));
      // Anything that is not an engine error is the app's own fault.
      expect(ErrorDescription.of(StateError('x')).headline, S.unexpectedError);
    });

    test('warnings, flags, ratings and verdicts all have text', () {
      for (final code in WarningCode.values) {
        expect(S.warningHeadline(code), isNotEmpty);
      }
      for (final flag in FileFlag.values) {
        expect(S.flag(flag), isNotEmpty);
        expect(S.flagName(flag, 'x'), isNotEmpty);
      }
      for (final rating in Rating.known) {
        expect(S.rating(rating), isNotEmpty);
        expect(S.ratingExplanation(rating), isNotEmpty);
      }
      for (final verdict in DestinationVerdict.values) {
        for (final reason in [null, ...ProtectionReason.values]) {
          expect(S.verdictHeadline(verdict, reason), isNotEmpty);
        }
      }
      // The three reasons of overlaps_source are told apart.
      expect({
        for (final r in ProtectionReason.values) S.verdictHeadline(DestinationVerdict.overlapsSource, r),
      }, hasLength(3));
    });
  });
}
