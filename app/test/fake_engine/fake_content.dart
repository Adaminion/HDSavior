/// Deterministic file contents for the fake engine's `read_file`.
///
/// Most generated files are pseudo-random bytes behind a plausible magic
/// number. The files of the "Preview samples" folder are real: a JPEG, a PNG
/// and a BMP that an image decoder accepts, and plain text.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'fake_tree.dart';

/// Sizes of the generated sample pictures.
const int jpegSampleSize = jpegSampleBytes;
const int pngSampleSize = pngSampleBytes;
const int bmpSampleSize = bmpSampleBytes;

const int _pictureWidth = 64;
const int _pictureHeight = 48;

/// A real 96x64 baseline JPEG (ffmpeg test pattern).
final Uint8List jpegSample = base64.decode(
    '/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYwLjMxLjEwMgD/2wBDAAgYGBwYHCEhISEh'
    'ISckJygoKCcnJycoKCgrKyszMzMrKysoKCsrMDAzMzc5NzQ0MzQ5OTw8PEhIRUVUVFdnZ3z/'
    'xACVAAACAwEBAQEAAAAAAAAAAAAABQcDBAYBAggBAAIDAQEAAAAAAAAAAAAAAAUGBwgEAwIQ'
    'AAEDAgQEBAUFAQEBAAAAAAECABESA0EhURMEYTIxIhRx0aKhUkKBsTPSUxUj4cERAAEDAQQH'
    'BwMFAQAAAAAAAAEAAhExAxIhcYEykUETFARhYlKhUdHwwXIjM0LhIrGi/8AAEQgAQABgAwEi'
    'AAIRAAMRAP/aAAwDAQACEQMRAD8Ai47qrhSjAThAAGZJOQHq9VHETGMVfbEa1dMc5bG2Qd9E'
    'VE7ZCexUEmSBzj8t8CnZ2qfF1bc5011UzrTh35MX1PVPtXSRhOAJmA6DOQpQZpxh1m5wD3DE'
    '0JEwSI+bEgXxHE2zClRj2QZGoIEPxHE8SswlU5T2QAAMSSID3XTGykQhQJMH7KlSmT7um2ZF'
    '9JhalFJgHrpVKoPppnow/wDW7NxuwesT9apEPSWQdENj7R4ZilZwon3neOmK8Kum3EazERzl'
    '0q4/jUGDc59NvMaghMF/VSdrbjxdW3OdNdUTrTh35NbcMbKRCFAkwfsqVKZ/9z1eIMYT+mzf'
    '+0bcvkpiLQBg40G2dXdiPgUeoFy4YTnkSewAA7kk5Aer07V6qmM6au6aaYmqqaY5zDdWTUOJ'
    'QYuLUpKoB/cpWSqkjUdoz0Dfyny+zT4+vZk1U7lVM604dXJyK/rbdroDnVA1jQgG/rDAUoB2'
    'rM2xaRoJ0g6tDifgXOru37ZhRjIEZJIIOIIyI9GIu37hhJnIk5JAAGJJyA9WzvCU2bYEKlXg'
    'nNNavCCfTXPV12Uwm9bIlUo8E5qoVKgD6aZ6PPzVpcm8ZzNL0Xq031jtRQWzy+7efneNYmM9'
    'ybeZ4yqmrOKu1uI1mIjnLPM8ZVTVnFXa3EazERzl7ZTs7VPi6tuc6a6qZ1pw78mSnZ2qfF1b'
    'c5011UzrTh35MRzNp/1Gjx61PLvJU5Gw9G6t7VFfBq63n3VHV4/9D+P0fiVvy9+4fx+jwv0B'
    'LRknG2ANo/7nf6n8y6HS/XxhIhdKHY7KX8xD2sIXEtI3LGpIObWFMNwprVNgaV2aVpSp2EtU'
    'HqD8luKMgBxmh7E3d6UKWYAnKdIAxJOQa+t9Jw6gpF1PUTQae1QSZIH4/LG2jixs5eZroqhL'
    'bF16HAgQT5UkTWiSXLa1LJA0xGjybNzT5j3fVsa6LQj0U3u6Szc4kl2JJqN+hZPJ3/o+JPuz'
    'yd/6PiT7uSl4MRi+F5/C4ktyg+sVvKtt+04HFluV0+KK3vouX/zeL/q+JH8mf5vF/wBXxI/k'
    '5/Y0rnbT0ZsPupAuhfi/xjL2fpBOD1MdtuRsvV+0eyAzkvDYuI7pieY93eLNw/b8x7uRnXA7'
    'sNy7O9tHspP5FoODjHnt/hcL5e79PzHu6di59PzHu5hXwhgkKqOkR85fNXbS7cVCJ7Zj/wCN'
    'Rt2hoJbJCxdM/oeqMWdveJJAbIa4wJwa5occN4EJUxjGgqTV368GIxetjF8b8XDjTPbNIVMO'
    'Y/Bwrume9NI+qkxjGOPlMi/HTGMd/wBK6kxjGNUU/qSmMY1ZUUX/2Q==',
);

/// The `kind` that `read_file` reports for a file with extension [ext], or
/// `null` if the fake does not "recognise" it.
String? sniffedKind(String ext) => switch (ext) {
  'jpg' || 'jpeg' => 'jpeg',
  'png' => 'png',
  'gif' => 'gif',
  'bmp' => 'bmp',
  'tif' || 'tiff' => 'tiff',
  'pdf' => 'pdf',
  'zip' || 'docx' || 'xlsx' || 'pptx' => 'zip',
  'mp4' => 'mp4',
  'heic' => 'heic',
  'cr3' => 'cr3',
  'avi' => 'avi',
  'wav' => 'wav',
  'webp' => 'webp',
  'mkv' => 'mkv',
  'mp3' => 'mp3',
  '7z' => 'sevenzip',
  'rar' => 'rar',
  _ => null,
};

List<int> _magic(String ext) => switch (ext) {
  'jpg' || 'jpeg' => const [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00],
  'png' => const [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
  'gif' => const [0x47, 0x49, 0x46, 0x38, 0x39, 0x61],
  'pdf' => const [0x25, 0x50, 0x44, 0x46, 0x2D, 0x31, 0x2E, 0x37, 0x0A],
  'zip' || 'docx' || 'xlsx' || 'pptx' => const [0x50, 0x4B, 0x03, 0x04],
  'mp3' => const [0x49, 0x44, 0x33, 0x04],
  '7z' => const [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C],
  'rar' => const [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07],
  _ => const [],
};

/// Returns `length` bytes (fewer at the end of the file) of the content of a
/// node at `offset`. `size` is the node's size; `contentKind` one of the
/// `c*` constants of fake_tree.dart.
Uint8List fakeFileBytes({
  required int id,
  required String ext,
  required int size,
  required int contentKind,
  required int offset,
  required int length,
}) {
  if (offset >= size || length <= 0) return Uint8List(0);
  final end = offset + length > size ? size : offset + length;
  switch (contentKind) {
    case cJpegSample:
      return Uint8List.sublistView(jpegSample, offset, end);
    case cPng:
      return Uint8List.sublistView(buildSamplePng(id), offset, end);
    case cBmp:
      return Uint8List.sublistView(buildSampleBmp(id), offset, end);
    case cText:
      return _text(id, offset, end);
    default:
      return _binary(id, ext, offset, end, garbageAfterMagic: true);
  }
}

/// Fixed-width lines, so that any byte range can be produced directly.
Uint8List _text(int id, int offset, int end) {
  const width = 64;
  final out = Uint8List(end - offset);
  for (var line = offset ~/ width; line * width < end; line++) {
    final text = 'line ${line + 1} of node $id: the quick brown fox'.padRight(width - 1);
    final bytes = ascii.encode('${text.substring(0, width - 1)}\n');
    for (var i = 0; i < width; i++) {
      final position = line * width + i;
      if (position >= offset && position < end) out[position - offset] = bytes[i];
    }
  }
  return out;
}

Uint8List _binary(int id, String ext, int offset, int end, {required bool garbageAfterMagic}) {
  final magic = _magic(ext);
  final out = Uint8List(end - offset);
  for (var position = offset; position < end; position++) {
    out[position - offset] =
        position < magic.length
            ? magic[position]
            : (mix64(id * 0x100000001B3 + (position >> 3)) >> ((position & 7) * 8)) & 0xFF;
  }
  return out;
}

final Map<int, Uint8List> _pictureCache = {};

/// Raw RGB rows of a gradient whose colours depend on [id].
Uint8List _pixels(int id) {
  final tint = mix64(id) & 0xFFFFFF;
  final out = Uint8List(_pictureWidth * _pictureHeight * 3);
  var i = 0;
  for (var y = 0; y < _pictureHeight; y++) {
    for (var x = 0; x < _pictureWidth; x++) {
      out[i++] = (x * 4 + (tint & 0xFF)) & 0xFF;
      out[i++] = (y * 5 + (tint >> 8 & 0xFF)) & 0xFF;
      out[i++] = ((x + y) * 2 + (tint >> 16 & 0xFF)) & 0xFF;
    }
  }
  return out;
}

/// A valid 64x48 24-bit BMP of [bmpSampleSize] bytes.
Uint8List buildSampleBmp(int id) => _pictureCache.putIfAbsent(id * 2, () {
  const rowSize = _pictureWidth * 3; // a multiple of 4: no padding
  const dataSize = rowSize * _pictureHeight;
  final out = Uint8List(54 + dataSize);
  final view = ByteData.sublistView(out);
  out[0] = 0x42;
  out[1] = 0x4D;
  view.setUint32(2, out.length, Endian.little);
  view.setUint32(10, 54, Endian.little);
  view.setUint32(14, 40, Endian.little);
  view.setInt32(18, _pictureWidth, Endian.little);
  view.setInt32(22, _pictureHeight, Endian.little);
  view.setUint16(26, 1, Endian.little);
  view.setUint16(28, 24, Endian.little);
  view.setUint32(34, dataSize, Endian.little);
  final rgb = _pixels(id);
  for (var y = 0; y < _pictureHeight; y++) {
    // BMP rows run bottom-up and store blue, green, red.
    final source = (_pictureHeight - 1 - y) * rowSize;
    final target = 54 + y * rowSize;
    for (var x = 0; x < _pictureWidth; x++) {
      out[target + x * 3] = rgb[source + x * 3 + 2];
      out[target + x * 3 + 1] = rgb[source + x * 3 + 1];
      out[target + x * 3 + 2] = rgb[source + x * 3];
    }
  }
  return out;
});

final Uint32List _crcTable = () {
  final table = Uint32List(256);
  for (var n = 0; n < 256; n++) {
    var c = n;
    for (var k = 0; k < 8; k++) {
      c = c & 1 != 0 ? 0xEDB88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c;
  }
  return table;
}();

int _crc32(List<int> bytes) {
  var c = 0xFFFFFFFF;
  for (final b in bytes) {
    c = _crcTable[(c ^ b) & 0xFF] ^ (c >>> 8);
  }
  return (c ^ 0xFFFFFFFF) & 0xFFFFFFFF;
}

int _adler32(List<int> bytes) {
  var a = 1;
  var b = 0;
  for (final v in bytes) {
    a = (a + v) % 65521;
    b = (b + a) % 65521;
  }
  return (b << 16) | a;
}

/// A valid 64x48 RGB PNG of [pngSampleSize] bytes. The image data is a zlib
/// stream with one stored (uncompressed) block, so the size is fixed and no
/// compressor is needed.
Uint8List buildSamplePng(int id) => _pictureCache.putIfAbsent(id * 2 + 1, () {
  final rgb = _pixels(id);
  const rowSize = 1 + _pictureWidth * 3;
  final raw = Uint8List(rowSize * _pictureHeight);
  for (var y = 0; y < _pictureHeight; y++) {
    raw[y * rowSize] = 0; // filter: none
    raw.setRange(
      y * rowSize + 1,
      (y + 1) * rowSize,
      rgb,
      y * _pictureWidth * 3,
    );
  }
  final zlib = BytesBuilder()
    ..add(const [0x78, 0x01])
    ..add([0x01, raw.length & 0xFF, raw.length >> 8, ~raw.length & 0xFF, (~raw.length >> 8) & 0xFF])
    ..add(raw);
  final adler = _adler32(raw);
  zlib.add([adler >> 24 & 0xFF, adler >> 16 & 0xFF, adler >> 8 & 0xFF, adler & 0xFF]);

  final out = BytesBuilder()..add(const [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
  void chunk(String type, List<int> data) {
    final body = [...ascii.encode(type), ...data];
    final crc = _crc32(body);
    out
      ..add([data.length >> 24 & 0xFF, data.length >> 16 & 0xFF, data.length >> 8 & 0xFF, data.length & 0xFF])
      ..add(body)
      ..add([crc >> 24 & 0xFF, crc >> 16 & 0xFF, crc >> 8 & 0xFF, crc & 0xFF]);
  }

  chunk('IHDR', [
    0, 0, 0, _pictureWidth, //
    0, 0, 0, _pictureHeight,
    8, // bit depth
    2, // colour type: RGB
    0, 0, 0,
  ]);
  chunk('IDAT', zlib.takeBytes());
  chunk('IEND', const []);
  return out.takeBytes();
});
