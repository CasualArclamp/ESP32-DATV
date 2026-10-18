/// Flashes an ESP32-C3 or ESP32-S3 through its ROM serial bootloader (a small subset of
/// esptool: SLIP framing, SYNC, chip check, SPI attach, compressed writes, MD5 verification).
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:archive/archive.dart';
import 'package:crypto/crypto.dart';

import 'transport.dart';

class FlashError implements Exception {
  FlashError(this.message);
  final String message;
  @override
  String toString() => message;
}

class FlashImage {
  const FlashImage(this.name, this.offset, this.data, {this.md5});
  final String name;
  final int offset;
  final Uint8List data;

  /// Expected MD5 (hex) of [data], if known (from the manifest).
  final String? md5;
}

/// The images for one chip (one bundled manifest).
class FlashTarget {
  const FlashTarget(this.chip, this.flashSize, this.images);

  /// Chip id as in the manifest ("esp32c3", "esp32s3").
  final String chip;
  final int flashSize;
  final List<FlashImage> images;
}

/// Parses a manifest flash size ("4MB", "16MB").
int parseFlashSize(String? s) {
  final m = RegExp(r'^(\d+)MB$').firstMatch(s ?? '');
  return m == null ? 4 * 1024 * 1024 : int.parse(m[1]!) * 1024 * 1024;
}

class EspChip {
  const EspChip(this.id, this.name, this.magic, this.chipId);
  final String id, name;

  /// Values of the chip magic register.
  final List<int> magic;

  /// Chip id from GET_SECURITY_INFO (newer ROMs).
  final int chipId;
}

/// ESP32-C3 chip magic values (ECO0..ECO3+), from esptool.
const List<int> esp32c3Magic = [0x6921506F, 0x1B31506F, 0x4881606F, 0x4361606F];

const EspChip esp32c3 = EspChip('esp32c3', 'ESP32-C3', esp32c3Magic, 5);
const EspChip esp32s3 = EspChip('esp32s3', 'ESP32-S3', [0x9], 9);
const List<EspChip> knownChips = [esp32c3, esp32s3];

typedef FlashProgress = void Function(String stage, double fraction);

// ROM loader opcodes
const int _opSync = 0x08;
const int _opWriteReg = 0x09;
const int _opReadReg = 0x0A;
const int _opSpiSetParams = 0x0B;
const int _opSpiAttach = 0x0D;
const int _opDeflBegin = 0x10;
const int _opDeflData = 0x11;
const int _opDeflEnd = 0x12;
const int _opMd5 = 0x13;
const int _opSecurityInfo = 0x14;

const int _chipMagicReg = 0x40001000;

// ESP32-S3 RTC watchdogs (esptool's esp32s3.py). They keep running in the ROM loader when
// it was entered over USB-Serial-JTAG and would reset the chip during a long flash.
const int _s3UartDevBufNo = 0x3FCEF14C;
const int _s3UartUsbJtagSerial = 4;
const int _s3RtcWdtConfig0 = 0x60008098;
const int _s3RtcWdtProtect = 0x600080B0;
const int _s3RtcSwdConf = 0x600080B4;
const int _s3RtcSwdProtect = 0x600080B8;
const int _rtcWdtKey = 0x50D83AA1;
const int _rtcSwdKey = 0x8F1D312A;

/// The ROM's write block size.
const int _blockSize = 0x400;

Future<void> _sleep(int ms) => Future<void>.delayed(Duration(milliseconds: ms));

Uint8List slipEncode(Uint8List data) {
  final b = BytesBuilder()..addByte(0xC0);
  for (final x in data) {
    if (x == 0xC0) {
      b.add(const [0xDB, 0xDC]);
    } else if (x == 0xDB) {
      b.add(const [0xDB, 0xDD]);
    } else {
      b.addByte(x);
    }
  }
  b.addByte(0xC0);
  return b.takeBytes();
}

/// Incremental SLIP decoder.
class SlipDecoder {
  final List<int> _cur = [];
  bool _in = false, _esc = false;

  List<Uint8List> add(Uint8List data) {
    final out = <Uint8List>[];
    for (final x in data) {
      if (!_in) {
        if (x == 0xC0) {
          _in = true;
          _cur.clear();
        }
        continue;
      }
      if (_esc) {
        _esc = false;
        _cur.add(x == 0xDC ? 0xC0 : (x == 0xDD ? 0xDB : x));
      } else if (x == 0xDB) {
        _esc = true;
      } else if (x == 0xC0) {
        if (_cur.isNotEmpty) {
          out.add(Uint8List.fromList(_cur));
          _in = false;
        }
        // an empty frame: treat the 0xC0 as a new start
        _cur.clear();
      } else {
        _cur.add(x);
      }
    }
    return out;
  }
}

class _Resp {
  _Resp(this.op, this.value, this.data);
  final int op, value;
  final Uint8List data;
}

class EspFlasher {
  EspFlasher(this.t);

  final ByteTransport t;
  final SlipDecoder _slip = SlipDecoder();
  final List<_Resp> _pending = [];

  // One line at a time, in esptool's order: on Windows the two lines cannot change
  // together, and the order decides which states the chip sees in between.
  Future<void> _dtr(bool v) => t.setSignals(dtr: v);
  Future<void> _rts(bool v) => t.setSignals(rts: v);

  /// USB-Serial-JTAG reset into the ROM download mode (esptool's USBJTAGSerialReset).
  /// DTR=1 RTS=0 asks for download mode, DTR=0 RTS=1 resets; the way from one to the other
  /// must go through DTR=1 RTS=1, because DTR=0 RTS=0 cancels the download request.
  Future<void> resetToBootloader() async {
    await _rts(false);
    await _dtr(false); // idle
    await _sleep(100);
    await _dtr(true); // boot pin low
    await _rts(false);
    await _sleep(100);
    await _rts(true); // through (1,1), not (0,0)
    await _dtr(false); // reset
    await _rts(true);
    await _sleep(100);
    await _dtr(false);
    await _rts(false); // out of reset into the ROM loader
  }

  /// Classic reset for boards with a USB-UART bridge (DTR drives IO9/IO0, RTS drives EN).
  Future<void> classicResetToBootloader() async {
    await _dtr(false);
    await _rts(true); // EN low
    await _sleep(100);
    await _dtr(true); // IO low
    await _rts(false); // EN high
    await _sleep(50);
    await _dtr(false);
  }

  /// Hard reset into the application (RTS pulses the chip's reset).
  Future<void> hardReset() async {
    await _dtr(false);
    await _rts(true);
    await _sleep(200);
    await _rts(false);
    await _sleep(200);
  }

  Future<void> _send(int op, Uint8List data, {int checksum = 0}) async {
    final p = Uint8List(8 + data.length);
    final bd = ByteData.sublistView(p);
    p[0] = 0x00;
    p[1] = op;
    bd.setUint16(2, data.length, Endian.little);
    bd.setUint32(4, checksum, Endian.little);
    p.setRange(8, 8 + data.length, data);
    await t.write(slipEncode(p), timeout: const Duration(seconds: 5));
  }

  Future<_Resp?> _recv(int op, Duration timeout) async {
    final sw = Stopwatch()..start();
    while (true) {
      for (var i = 0; i < _pending.length; i++) {
        if (_pending[i].op == op) return _pending.removeAt(i);
      }
      _pending.clear();
      if (sw.elapsed > timeout) return null;
      final d = await t.read(timeout: const Duration(milliseconds: 10));
      for (final f in _slip.add(d)) {
        if (f.length < 8 || f[0] != 0x01) continue;
        final bd = ByteData.sublistView(f);
        final size = bd.getUint16(2, Endian.little);
        final end = (8 + size).clamp(8, f.length);
        _pending.add(_Resp(f[1], bd.getUint32(4, Endian.little), Uint8List.sublistView(f, 8, end)));
      }
    }
  }

  Future<_Resp> _command(int op, Uint8List data,
      {int checksum = 0, Duration timeout = const Duration(seconds: 3), int statusAt = 0}) async {
    await _send(op, data, checksum: checksum);
    final r = await _recv(op, timeout);
    if (r == null) throw FlashError('No answer from the ROM loader (command 0x${op.toRadixString(16)})');
    if (r.data.length >= statusAt + 2 && r.data[statusAt] != 0) {
      throw FlashError('ROM loader error 0x${r.data[statusAt + 1].toRadixString(16)} '
          '(command 0x${op.toRadixString(16)})');
    }
    return r;
  }

  /// Synchronises with the ROM loader; true on success.
  Future<bool> sync({int attempts = 7}) async {
    final payload = Uint8List(36);
    payload.setAll(0, const [0x07, 0x07, 0x12, 0x20]);
    payload.fillRange(4, 36, 0x55);
    for (var i = 0; i < attempts; i++) {
      await t.flushInput();
      try {
        await _send(_opSync, payload);
      } on TransportError {
        await _sleep(100);
        continue;
      }
      final r = await _recv(_opSync, const Duration(milliseconds: 300));
      if (r != null) {
        // the ROM answers SYNC several times: drain the rest
        await _sleep(100);
        await t.flushInput();
        _pending.clear();
        return true;
      }
    }
    return false;
  }

  Future<int> readReg(int addr) async {
    final d = Uint8List(4);
    ByteData.sublistView(d).setUint32(0, addr, Endian.little);
    return (await _command(_opReadReg, d)).value;
  }

  Future<void> writeReg(int addr, int value) async {
    final d = Uint8List(16);
    final bd = ByteData.sublistView(d);
    bd.setUint32(0, addr, Endian.little);
    bd.setUint32(4, value, Endian.little);
    bd.setUint32(8, 0xFFFFFFFF, Endian.little); // mask
    bd.setUint32(12, 0, Endian.little); // delay
    await _command(_opWriteReg, d);
  }

  /// Identifies the chip by its magic register, or by GET_SECURITY_INFO on ROMs whose
  /// magic esptool does not list.
  Future<EspChip> _identify() async {
    final magic = await readReg(_chipMagicReg);
    for (final c in knownChips) {
      if (c.magic.contains(magic)) return c;
    }
    try {
      // flags(4) flash_crypt_cnt(1) key_purposes(7) chip_id(4) api_version(4), then the status
      final r = await _command(_opSecurityInfo, Uint8List(0), statusAt: 20);
      if (r.data.length >= 22) {
        final id = ByteData.sublistView(r.data).getUint32(12, Endian.little);
        for (final c in knownChips) {
          if (c.chipId == id) return c;
        }
      }
    } on FlashError {
      // old ROM without the command
    }
    throw FlashError('Not an ESP32-C3 or ESP32-S3 (chip magic 0x${magic.toRadixString(16)})');
  }

  Future<void> _disableWatchdogs(EspChip chip) async {
    if (chip != esp32s3 || (await readReg(_s3UartDevBufNo)) & 0xFF != _s3UartUsbJtagSerial) return;
    await writeReg(_s3RtcWdtProtect, _rtcWdtKey);
    await writeReg(_s3RtcWdtConfig0, 0);
    await writeReg(_s3RtcWdtProtect, 0);
    await writeReg(_s3RtcSwdProtect, _rtcSwdKey);
    await writeReg(_s3RtcSwdConf, (await readReg(_s3RtcSwdConf)) | (1 << 31)); // auto feed
    await writeReg(_s3RtcSwdProtect, 0);
  }

  /// Resets into the bootloader, syncs and identifies the chip.
  Future<EspChip> connect({FlashProgress? progress}) async {
    progress?.call('Resetting into the bootloader', 0);
    // 0: the board may already wait in download mode (BOOT held while plugging in or resetting);
    // a reset now could take it out again, so try without one first.
    // 1, 2: USB-Serial-JTAG reset (native USB boards). 3: classic reset (USB-UART bridges).
    for (var attempt = 0; attempt < 4; attempt++) {
      if (attempt == 1 || attempt == 2) {
        await resetToBootloader();
        await _sleep(300);
      } else if (attempt == 3) {
        await classicResetToBootloader();
        await _sleep(300);
      }
      if (await sync(attempts: attempt == 0 ? 2 : 7)) {
        final chip = await _identify();
        await _disableWatchdogs(chip);
        return chip;
      }
    }
    throw FlashError('The ESP32 ROM loader does not answer. Put the board in download mode by hand: '
        'hold BOOT, tap RESET (or unplug and plug in again while holding BOOT), release BOOT, then '
        'click Install firmware again.');
  }

  Future<void> _attachAndParams(int flashSize) async {
    await _command(_opSpiAttach, Uint8List(8));
    final p = Uint8List(24);
    final bd = ByteData.sublistView(p);
    bd.setUint32(0, 0, Endian.little); // flash id
    bd.setUint32(4, flashSize, Endian.little); // total size
    bd.setUint32(8, 64 * 1024, Endian.little); // block
    bd.setUint32(12, 4 * 1024, Endian.little); // sector
    bd.setUint32(16, 256, Endian.little); // page
    bd.setUint32(20, 0xFFFF, Endian.little); // status mask
    await _command(_opSpiSetParams, p);
  }

  Future<String> flashMd5(int offset, int size) async {
    final p = Uint8List(16);
    final bd = ByteData.sublistView(p);
    bd.setUint32(0, offset, Endian.little);
    bd.setUint32(4, size, Endian.little);
    final r = await _command(_opMd5, p, timeout: Duration(seconds: 8 + size ~/ 100000), statusAt: 32);
    return ascii.decode(r.data.sublist(0, 32), allowInvalid: true).toLowerCase();
  }

  Future<void> _writeImage(FlashImage img, void Function(double) prog) async {
    final comp = Uint8List.fromList(const ZLibEncoder().encode(img.data, level: 9));
    final numBlocks = (comp.length + _blockSize - 1) ~/ _blockSize;
    final p = Uint8List(20);
    final bd = ByteData.sublistView(p);
    bd.setUint32(0, img.data.length, Endian.little); // the ROM erases the uncompressed size
    bd.setUint32(4, numBlocks, Endian.little);
    bd.setUint32(8, _blockSize, Endian.little);
    bd.setUint32(12, img.offset, Endian.little);
    bd.setUint32(16, 0, Endian.little); // not encrypted
    final eraseSecs = 3 + img.data.length ~/ (256 * 1024) * 2;
    await _command(_opDeflBegin, p, timeout: Duration(seconds: eraseSecs + 10));
    for (var seq = 0; seq < numBlocks; seq++) {
      final start = seq * _blockSize;
      final chunk = Uint8List.sublistView(comp, start, (start + _blockSize).clamp(0, comp.length));
      final d = Uint8List(16 + chunk.length);
      final hd = ByteData.sublistView(d);
      hd.setUint32(0, chunk.length, Endian.little);
      hd.setUint32(4, seq, Endian.little);
      d.setRange(16, d.length, chunk);
      var ck = 0xEF;
      for (final x in chunk) {
        ck ^= x;
      }
      await _command(_opDeflData, d, checksum: ck, timeout: const Duration(seconds: 10));
      prog((seq + 1) / numBlocks);
    }
  }

  /// Writes the images of the target that matches the chip, verifies their MD5 and resets
  /// into the new firmware. Returns the target that was written.
  Future<FlashTarget> flash(List<FlashTarget> targets, {FlashProgress? progress}) async {
    final chip = await connect(progress: progress);
    final target = targets.where((t) => t.chip == chip.id).firstOrNull;
    if (target == null) {
      await hardReset();
      throw FlashError('No bundled firmware for the ${chip.name}');
    }
    final images = target.images;
    progress?.call('Attaching flash', 0);
    await _attachAndParams(target.flashSize);
    final total = images.fold<int>(0, (a, i) => a + i.data.length);
    var done = 0;
    for (final img in images) {
      final want = md5.convert(img.data).toString();
      if (img.md5 != null && img.md5!.toLowerCase() != want) {
        throw FlashError('${img.name}: bundled file does not match its manifest MD5');
      }
      await _writeImage(img, (f) => progress?.call('Writing ${img.name}', (done + f * img.data.length) / total));
      progress?.call('Verifying ${img.name}', (done + img.data.length) / total);
      final got = await flashMd5(img.offset, img.data.length);
      if (got != want) throw FlashError('${img.name}: verification failed (flash MD5 $got, expected $want)');
      done += img.data.length;
    }
    // leave the loader without rebooting, then reset through RTS
    final e = Uint8List(4);
    ByteData.sublistView(e).setUint32(0, 1, Endian.little);
    try {
      await _command(_opDeflEnd, e);
    } on FlashError {
      // some ROM versions do not answer; the reset below is what matters
    }
    progress?.call('Restarting the board', 1);
    await hardReset();
    return target;
  }

  /// Erases nothing; returns the chip whose ROM loader answers, or null (used to recognise a
  /// board without the DATV firmware).
  Future<EspChip?> probeRom() async {
    try {
      final chip = await connect();
      await hardReset();
      return chip;
    } on FlashError {
      return null;
    }
  }
}
