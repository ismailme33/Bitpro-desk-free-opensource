import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/common/formatter/id_formatter.dart';
import 'package:flutter_hbb/models/peer_model.dart';
import 'package:flutter_hbb/models/platform_model.dart';

/// Local option holding the pinned recent peer IDs (JSON list, newest first).
const String kOptionPinnedPeers = 'pinned-peers';

/// Implemented by the Windows runner (windows/runner/jump_list.cpp).
const _jumpListChannel = MethodChannel('bitpro/jump_list');
const int _kJumpListRecentMax = 8;
String _lastJumpListSignature = '';

List<String> getPinnedPeerIds() {
  try {
    final v = bind.mainGetLocalOption(key: kOptionPinnedPeers);
    if (v.isEmpty) return [];
    return List<String>.from(jsonDecode(v));
  } catch (e) {
    debugPrint('getPinnedPeerIds error: $e');
    return [];
  }
}

bool isPeerPinned(String id) => getPinnedPeerIds().contains(id);

Future<void> setPeerPinned(String id, bool pinned) async {
  final ids = getPinnedPeerIds()..remove(id);
  if (pinned) ids.insert(0, id);
  await bind.mainSetLocalOption(key: kOptionPinnedPeers, value: jsonEncode(ids));
}

/// Pinned peers first (in pin order), the rest keep their recent order.
List<Peer> sortPinnedFirst(List<Peer> peers) {
  final ids = getPinnedPeerIds();
  if (ids.isEmpty) return peers;
  final pinned = peers.where((p) => ids.contains(p.id)).toList()
    ..sort((a, b) => ids.indexOf(a.id).compareTo(ids.indexOf(b.id)));
  final rest = peers.where((p) => !ids.contains(p.id));
  return [...pinned, ...rest];
}

String _jumpListTitle(Peer p) {
  final id = formatID(p.id);
  if (p.alias.isNotEmpty) return '${p.alias} ($id)';
  if (p.hostname.isNotEmpty) return '${p.hostname} ($id)';
  return id;
}

/// Publishes pinned and recent peers to the taskbar Jump List (Windows only).
/// [recentPeers] must be in recent order; unchanged lists are skipped.
Future<void> updateJumpList(List<Peer> recentPeers) async {
  if (!isWindows) return;
  final ids = getPinnedPeerIds();
  final sorted = sortPinnedFirst(recentPeers);
  final pinned = sorted.where((p) => ids.contains(p.id)).toList();
  final recent =
      sorted.where((p) => !ids.contains(p.id)).take(_kJumpListRecentMax).toList();
  item(Peer p) => {'id': p.id, 'title': _jumpListTitle(p)};
  final args = {
    'pinned': pinned.map(item).toList(),
    'recent': recent.map(item).toList(),
    'pinnedLabel': translate('Pinned'),
    'recentLabel': translate('Recent sessions'),
    'uriPrefix': bind.mainUriPrefixSync(),
  };
  final signature = jsonEncode(args);
  if (signature == _lastJumpListSignature) return;
  _lastJumpListSignature = signature;
  try {
    await _jumpListChannel.invokeMethod('update', args);
  } catch (e) {
    // Only the main window registers the channel.
    debugPrint('updateJumpList error: $e');
  }
}

/// Rebuilds the Jump List from the stored recent peers.
Future<void> refreshJumpList() async {
  try {
    final data = jsonDecode(bind.mainLoadRecentPeersSync());
    dynamic peerData = data is Map ? data['peers'] : null;
    if (peerData is String) peerData = jsonDecode(peerData);
    if (peerData is List) {
      await updateJumpList(peerData
          .map((p) => Peer.fromJson(Map<String, dynamic>.from(p)))
          .toList());
    }
  } catch (e) {
    debugPrint('refreshJumpList error: $e');
  }
}
