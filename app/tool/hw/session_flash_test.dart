// ignore_for_file: avoid_print
// Dev-only hardware check of the app's flashing path: loads every bundled firmware like the
// app does, then connects, flashes and checks INFO through DeviceSession (the link isolate).
// With --board-none the T-Deck board setting is cleared first, to check that flashing sets it.
// Usage (from app/): dart run tool/hw/session_flash_test.dart <port> [--board-none]
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:esp32_datv/core/esp/esp_link.dart';
import 'package:esp32_datv/core/esp/flasher.dart';
import 'package:esp32_datv/core/esp/transport.dart';
import 'package:esp32_datv/engine/engine.dart';
import 'package:esp32_datv/engine/transport_factory.dart';
import 'package:esp32_datv/platform/serial/linux_serial.dart';

FlashTarget _load(String dir) {
  final m = jsonDecode(File('$dir/manifest.json').readAsStringSync()) as Map<String, dynamic>;
  return FlashTarget(m['chip'] as String, parseFlashSize(m['flash_size'] as String?), [
    for (final i in (m['images'] as List).cast<Map<String, dynamic>>())
      FlashImage(i['file'] as String, i['offset'] as int, File('$dir/${i['file']}').readAsBytesSync(),
          md5: i['md5'] as String?),
  ]);
}

Future<void> main(List<String> args) async {
  final port = args[0];
  if (args.contains('--board-none')) {
    final t = LinuxSerialTransport.open(port);
    final link = EspLink(t);
    print('before: ${(await link.hello())?.line}');
    await t.write(Uint8List.fromList(ascii.encode('BOARD none\n')));
    await Future<void>.delayed(const Duration(milliseconds: 300));
    await t.close();
    await Future<void>.delayed(const Duration(milliseconds: 2000));
  }
  final targets = [_load('assets/firmware'), _load('assets/firmware/tdeck')];
  final s = await DeviceSession.open(PortInfo(id: port, name: port), TransportSpec.path(port));
  final h = await s.hello();
  print('hello: version ${h.version} tdeck ${h.tdeck} line ${h.line}');
  final sw = Stopwatch()..start();
  var last = '';
  final r = await s.flash(targets, progress: (stage, f) {
    final l = '$stage ${(f * 100).toStringAsFixed(0)}%';
    if (l != last && (f == 0 || f == 1 || (f * 100).round() % 25 == 0)) print(l);
    last = l;
  });
  print('flashed in ${sw.elapsedMilliseconds} ms: version ${r.version} tdeck ${r.tdeck} line ${r.line}');
  await s.close();
  exit(0);
}
