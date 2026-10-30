import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:get/get.dart';
import 'package:http/http.dart' as http;

import '../common.dart';
import '../models/platform_model.dart';

/// Bitpro Desk updates, published from the Bitpro admin app.
///
/// The admin app writes Firestore document `updates/bitprodesk` with the
/// same fields as the ERP's `updates/latest`. The `updates` collection is
/// publicly readable (`allow get: if true`), so no login or SDK is needed.
const String _kUpdateDocUrl =
    'https://firestore.googleapis.com/v1/projects/licensemanager-46a48/databases/(default)/documents/updates/bitprodesk';
const Duration _kCheckInterval = Duration(hours: 6);

/// Only installers from the developer's own GitHub account or website are
/// downloaded and run (GitHub then redirects to its file servers).
const String _kTrustedGithubPrefix = '/ismailme33/';
const String _kTrustedDomain = 'bitproglobal.com';

class BitproUpdate {
  final String latestVersion;
  final String minVersion;
  final String downloadUrl;
  final String notes;
  final bool mandatory;

  BitproUpdate(this.latestVersion, this.minVersion, this.downloadUrl,
      this.notes, this.mandatory);
}

/// The update to offer, or null when up to date.
final Rx<BitproUpdate?> bitproUpdate = Rx<BitproUpdate?>(null);
Timer? _checkTimer;

bool _isTrustedUrl(String url) {
  final uri = Uri.tryParse(url);
  if (uri == null || uri.scheme != 'https') return false;
  if (uri.host == 'github.com') {
    return uri.path.startsWith(_kTrustedGithubPrefix);
  }
  return uri.host == _kTrustedDomain ||
      uri.host.endsWith('.$_kTrustedDomain');
}

/// Starts checking now and every few hours (Windows desktop only).
void startBitproUpdateCheck() {
  if (!isWindows || _checkTimer != null) return;
  checkBitproUpdate();
  _checkTimer = Timer.periodic(_kCheckInterval, (_) => checkBitproUpdate());
}

Future<void> checkBitproUpdate() async {
  try {
    final resp = await http
        .get(Uri.parse(_kUpdateDocUrl))
        .timeout(const Duration(seconds: 15));
    if (resp.statusCode != 200) return; // no document published yet
    final fields = (jsonDecode(resp.body)['fields'] ?? {}) as Map;
    String field(String k) =>
        ((fields[k] ?? {})['stringValue'] as String? ?? '').trim();
    final latest = field('latestVersion');
    final min = field('mandatoryMinVersion');
    final url = field('downloadUrl');
    if (latest.isEmpty || !_isTrustedUrl(url)) return;

    final current = await bind.mainGetVersion();
    if (versionCmp(latest, current) <= 0) {
      bitproUpdate.value = null;
      return;
    }
    final mandatory = min.isNotEmpty && versionCmp(current, min) < 0;
    bitproUpdate.value =
        BitproUpdate(latest, min, url, field('releaseNotes'), mandatory);
  } catch (e) {
    debugPrint('checkBitproUpdate: $e');
  }
}

/// "Update available" banner for the home screen.
class BitproUpdateBanner extends StatefulWidget {
  const BitproUpdateBanner({Key? key}) : super(key: key);

  @override
  State<BitproUpdateBanner> createState() => _BitproUpdateBannerState();
}

class _BitproUpdateBannerState extends State<BitproUpdateBanner> {
  final _dismissed = false.obs;
  final _progress = (-1.0).obs; // -1 idle, 0..1 downloading
  final _error = ''.obs;

  Future<void> _downloadAndInstall(BitproUpdate u) async {
    _error.value = '';
    _progress.value = 0;
    try {
      final req = http.Request('GET', Uri.parse(u.downloadUrl));
      final resp = await http.Client().send(req);
      if (resp.statusCode != 200) {
        throw 'HTTP ${resp.statusCode}';
      }
      final total = resp.contentLength ?? 0;
      final file = File(
          '${Directory.systemTemp.path}\\BitproDesk-Setup-${u.latestVersion}.exe');
      final sink = file.openWrite();
      var received = 0;
      await for (final chunk in resp.stream) {
        sink.add(chunk);
        received += chunk.length;
        if (total > 0) _progress.value = received / total;
      }
      await sink.close();
      // The installer replaces the app and keeps the ID and settings.
      await Process.start(file.path, [], mode: ProcessStartMode.detached);
    } catch (e) {
      _progress.value = -1;
      _error.value = translate('Download failed, please try again.');
      debugPrint('update download: $e');
    }
  }

  @override
  Widget build(BuildContext context) {
    return Obx(() {
      final u = bitproUpdate.value;
      final dismissed = _dismissed.value;
      final progress = _progress.value;
      final error = _error.value;
      if (u == null || (dismissed && !u.mandatory)) {
        return const SizedBox.shrink();
      }
      final downloading = progress >= 0;
      final textColor = Theme.of(context).textTheme.titleLarge?.color;
      final muted = textColor?.withOpacity(0.6);
      final title = u.mandatory
          ? '${translate('Required update')}: Bitpro Desk ${u.latestVersion}'
          : 'Bitpro Desk ${u.latestVersion} ${translate('is available')}';
      final subtitle = error.isNotEmpty
          ? error
          : (u.notes.isNotEmpty
              ? u.notes
              : translate('Get the latest improvements and fixes.'));
      // Slim notification bar, aligned with the cards below.
      return Align(
        alignment: Alignment.centerLeft,
        child: Container(
          constraints: const BoxConstraints(maxWidth: 856),
          margin: const EdgeInsets.only(top: 16),
          padding: const EdgeInsets.fromLTRB(14, 10, 10, 10),
          decoration: BoxDecoration(
            color: u.mandatory
                ? MyTheme.accent.withOpacity(0.08)
                : MyTheme.panel(context),
            border: Border.all(
                color: u.mandatory
                    ? MyTheme.accent.withOpacity(0.5)
                    : MyTheme.panelBorder(context)),
            borderRadius: BorderRadius.circular(10),
          ),
          child: Row(
            children: [
              Container(
                width: 34,
                height: 34,
                decoration: BoxDecoration(
                  color: MyTheme.accent.withOpacity(0.12),
                  borderRadius: BorderRadius.circular(9),
                ),
                child: const Icon(Icons.arrow_circle_down_rounded,
                    color: MyTheme.accent, size: 19),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Row(
                      children: [
                        Flexible(
                          child: Text(
                            title,
                            overflow: TextOverflow.ellipsis,
                            style: const TextStyle(
                                fontSize: 13.5, fontWeight: FontWeight.w600),
                          ),
                        ),
                        if (!u.mandatory)
                          Container(
                            margin: const EdgeInsets.only(left: 8),
                            padding: const EdgeInsets.symmetric(
                                horizontal: 6, vertical: 1),
                            decoration: BoxDecoration(
                              color: MyTheme.accent,
                              borderRadius: BorderRadius.circular(4),
                            ),
                            child: Text(
                              translate('NEW'),
                              style: const TextStyle(
                                  color: Colors.white,
                                  fontSize: 9.5,
                                  fontWeight: FontWeight.w700,
                                  letterSpacing: 0.6),
                            ),
                          ),
                      ],
                    ),
                    const SizedBox(height: 3),
                    if (downloading)
                      ClipRRect(
                        borderRadius: BorderRadius.circular(3),
                        child: LinearProgressIndicator(
                          value: progress > 0 ? progress : null,
                          minHeight: 4,
                          backgroundColor: MyTheme.panelBorder(context),
                          valueColor:
                              const AlwaysStoppedAnimation(MyTheme.accent),
                        ),
                      ).marginOnly(top: 4, right: 8)
                    else
                      Text(
                        subtitle,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(
                            fontSize: 12.5,
                            color: error.isNotEmpty ? MyTheme.accent : muted),
                      ),
                  ],
                ),
              ),
              const SizedBox(width: 12),
              if (downloading)
                Text(
                  progress > 0 ? '${(progress * 100).round()}%' : '',
                  style: TextStyle(
                      color: muted, fontSize: 12.5, fontWeight: FontWeight.w600),
                ).marginOnly(right: 6)
              else ...[
                if (!u.mandatory)
                  TextButton(
                    onPressed: () => _dismissed.value = true,
                    style: TextButton.styleFrom(
                      foregroundColor: muted,
                      minimumSize: const Size(0, 32),
                      padding: const EdgeInsets.symmetric(horizontal: 12),
                      textStyle: const TextStyle(
                          fontSize: 13, fontWeight: FontWeight.w500),
                    ),
                    child: Text(translate('Later')),
                  ),
                const SizedBox(width: 4),
                ElevatedButton.icon(
                  onPressed: () => _downloadAndInstall(u),
                  icon: const Icon(Icons.download_rounded, size: 16),
                  label: Text(translate('Update now')),
                  style: ElevatedButton.styleFrom(
                    elevation: 0,
                    minimumSize: const Size(0, 32),
                    padding: const EdgeInsets.symmetric(horizontal: 14),
                    textStyle: const TextStyle(
                        fontSize: 13, fontWeight: FontWeight.w600),
                  ),
                ),
              ],
            ],
          ),
        ),
      );
    });
  }
}
