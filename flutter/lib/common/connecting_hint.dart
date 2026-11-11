import 'dart:async';

import 'package:bot_toast/bot_toast.dart';
import 'package:flutter/material.dart';

import '../common.dart';
import 'formatter/id_formatter.dart';

/// "Connecting to …" indicator for the main window.
///
/// Opening a session starts a new window (its own Flutter engine), which
/// takes a few seconds. Without feedback people think the double-click did
/// nothing and click again. This shows a small non-blocking pill right away
/// and hides it as soon as the session window appears (main window loses
/// focus or the new window reports itself), or after [_kMaxShow].
const Duration _kMaxShow = Duration(seconds: 15);

CancelFunc? _cancel;
Timer? _timeout;

void showConnectingHint(String id) {
  hideConnectingHint();
  final label = formatID(id.replaceAll(' ', ''));
  _cancel = BotToast.showCustomLoading(
    toastBuilder: (_) => _ConnectingPill(label: label),
    align: const Alignment(0, 0.82),
    allowClick: true, // never blocks the window
    clickClose: false,
    ignoreContentClick: true,
    crossPage: true,
    backgroundColor: Colors.transparent,
  );
  _timeout = Timer(_kMaxShow, hideConnectingHint);
}

void hideConnectingHint() {
  _timeout?.cancel();
  _timeout = null;
  final cancel = _cancel;
  _cancel = null;
  cancel?.call();
}

class _ConnectingPill extends StatelessWidget {
  final String label;
  const _ConnectingPill({Key? key, required this.label}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    final textColor =
        Theme.of(context).textTheme.titleLarge?.color ?? Colors.black87;
    return Material(
      color: Colors.transparent,
      child: Container(
        padding: const EdgeInsets.fromLTRB(14, 10, 18, 10),
        decoration: BoxDecoration(
          color: MyTheme.panel(context),
          borderRadius: BorderRadius.circular(24),
          border: Border.all(color: MyTheme.panelBorder(context)),
          boxShadow: [
            BoxShadow(
              color: Colors.black.withOpacity(0.25),
              blurRadius: 16,
              offset: const Offset(0, 4),
            ),
          ],
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            const SizedBox(
              width: 16,
              height: 16,
              child: CircularProgressIndicator(
                strokeWidth: 2,
                valueColor: AlwaysStoppedAnimation(MyTheme.accent),
              ),
            ),
            const SizedBox(width: 12),
            Text(
              '${translate('Connecting to')} $label...',
              style: TextStyle(
                color: textColor,
                fontSize: 13.5,
                fontWeight: FontWeight.w500,
              ),
            ),
          ],
        ),
      ),
    );
  }
}
