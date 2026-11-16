import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:get/get.dart';
import 'package:http/http.dart' as http;

import '../common.dart';
import '../models/platform_model.dart';

/// Feedback from users (ideas, improvements, problems, complaints).
///
/// Saved to Firestore `deskFeedback` in the Bitpro license project, where the
/// Bitpro license app shows it ("Desk Feedback"). Firestore rules only allow
/// creating documents with exactly these fields and sizes; nobody can read
/// them except admins of the license app.
const String _kFeedbackUrl =
    'https://firestore.googleapis.com/v1/projects/licensemanager-46a48/databases/(default)/documents/deskFeedback';

/// Same limits as the Firestore rules (which enforce them on the server).
const int _kTitleMin = 5, _kTitleMax = 120;
const int _kMessageMin = 20, _kMessageMax = 4000;
const int _kContactMax = 120;

/// Abuse guards for this PC: one message per minute, five per day, and no
/// sending the same message twice.
const Duration _kMinInterval = Duration(minutes: 1);
const int _kMaxPerDay = 5;
const String _kOptSendLog = 'bitpro-feedback-log'; // send times (ms), 24 h
const String _kOptLastText = 'bitpro-feedback-last-text';

final _linkRe = RegExp(r'(https?://|www\.)', caseSensitive: false);
final _repeatRe = RegExp(r'(.)\1{7,}'); // "aaaaaaaa", "!!!!!!!!"
final _letterRe = RegExp(r'\p{L}', unicode: true);
final _emailRe = RegExp(r'^[^\s@]+@[^\s@]+\.[^\s@]{2,}$');
final _phoneRe = RegExp(r'^\+?[\d\s\-()]{7,20}$');

String _normalize(String s) =>
    s.toLowerCase().replaceAll(RegExp(r'\s+'), ' ').trim();

List<int> _sendTimes() {
  final now = DateTime.now().millisecondsSinceEpoch;
  return bind
      .mainGetLocalOption(key: _kOptSendLog)
      .split(',')
      .map((s) => int.tryParse(s) ?? 0)
      .where((t) => now - t < const Duration(days: 1).inMilliseconds)
      .toList();
}

/// Why this message can't be sent, or null when it is fine.
String? _problemWith(String title, String message, String contact) {
  if (_linkRe.hasMatch(title)) {
    return translate('Please remove links from the title.');
  }
  if (_linkRe.allMatches(message).length > 2) {
    return translate('Please include at most 2 links.');
  }
  if (_repeatRe.hasMatch(title) || _repeatRe.hasMatch(message)) {
    return translate('Please avoid long runs of the same character.');
  }
  if (_letterRe.allMatches(message).length < 10) {
    return translate('Please describe it in words.');
  }
  if (contact.isNotEmpty &&
      !_emailRe.hasMatch(contact) &&
      !_phoneRe.hasMatch(contact)) {
    return translate('Please enter a valid email or phone number, or leave it empty.');
  }
  final times = _sendTimes();
  final now = DateTime.now().millisecondsSinceEpoch;
  if (times.isNotEmpty &&
      now - times.reduce((a, b) => a > b ? a : b) <
          _kMinInterval.inMilliseconds) {
    return translate('Please wait a minute before sending again.');
  }
  if (times.length >= _kMaxPerDay) {
    return translate(
        'You have sent the maximum of 5 messages today. Thank you – please try again tomorrow.');
  }
  final text = _normalize('$title\n$message');
  if (bind.mainGetLocalOption(key: _kOptLastText) ==
      text.substring(0, text.length < 300 ? text.length : 300)) {
    return translate('You already sent this message.');
  }
  return null;
}

Future<void> _rememberSent(String title, String message) async {
  final times = _sendTimes()..add(DateTime.now().millisecondsSinceEpoch);
  await bind.mainSetLocalOption(key: _kOptSendLog, value: times.join(','));
  final text = _normalize('$title\n$message');
  await bind.mainSetLocalOption(
      key: _kOptLastText,
      value: text.substring(0, text.length < 300 ? text.length : 300));
}

enum FeedbackType {
  idea('idea', 'Idea', Icons.lightbulb_outline_rounded),
  improvement('improvement', 'Improvement', Icons.auto_awesome_outlined),
  problem('problem', 'Problem', Icons.bug_report_outlined),
  complaint('complaint', 'Complaint', Icons.sentiment_dissatisfied_outlined),
  other('other', 'Other', Icons.chat_bubble_outline_rounded);

  final String value;
  final String label;
  final IconData icon;
  const FeedbackType(this.value, this.label, this.icon);
}

String _cut(String s, int max) => s.length <= max ? s : s.substring(0, max);

Future<void> _send({
  required FeedbackType type,
  required String title,
  required String message,
  required String contact,
}) async {
  final version = await bind.mainGetVersion();
  final deskId =
      gFFI.serverModel.serverId.text.replaceAll(RegExp(r'[^0-9]'), '');
  final fields = <String, dynamic>{
    'type': {'stringValue': type.value},
    'title': {'stringValue': title},
    'message': {'stringValue': message},
    'version': {'stringValue': _cut(version, 20)},
    'language': {'stringValue': _cut(Platform.localeName, 10)},
    'os': {'stringValue': _cut(Platform.operatingSystemVersion, 80)},
    'status': {'stringValue': 'new'},
    'createdAt': {
      'timestampValue': DateTime.now().toUtc().toIso8601String(),
    },
    if (contact.isNotEmpty) 'contact': {'stringValue': contact},
    if (deskId.isNotEmpty) 'deskId': {'stringValue': _cut(deskId, 20)},
  };
  final resp = await http
      .post(
        Uri.parse(_kFeedbackUrl),
        headers: {'Content-Type': 'application/json'},
        body: jsonEncode({'fields': fields}),
      )
      .timeout(const Duration(seconds: 20));
  if (resp.statusCode != 200) {
    debugPrint('feedback: HTTP ${resp.statusCode} ${resp.body}');
    throw 'HTTP ${resp.statusCode}';
  }
  await _rememberSent(title, message);
}

/// "Help us improve" card for the home screen, next to the connect and
/// support cards. [slim]: one-row strip used when the row has no room.
class BitproFeedbackCard extends StatelessWidget {
  final bool slim;
  const BitproFeedbackCard({Key? key, this.slim = false}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    final textColor =
        Theme.of(context).textTheme.titleLarge?.color ?? Colors.black87;
    final muted = textColor.withOpacity(0.6);
    final decoration = BoxDecoration(
      color: MyTheme.panel(context),
      borderRadius: BorderRadius.circular(10),
      border: Border.all(color: MyTheme.panelBorder(context)),
    );
    final iconTile = Container(
      width: slim ? 30 : 36,
      height: slim ? 30 : 36,
      decoration: BoxDecoration(
        color: MyTheme.accent.withOpacity(0.12),
        borderRadius: BorderRadius.circular(9),
      ),
      child: Icon(Icons.forum_outlined,
          size: slim ? 16 : 18, color: MyTheme.accent),
    );
    final titleText = Text(
      translate('Help us improve'),
      maxLines: 1,
      overflow: TextOverflow.ellipsis,
      style: TextStyle(
          color: textColor,
          fontSize: slim ? 13.5 : 16,
          fontWeight: FontWeight.w600),
    );
    final bodyText = Text(
      translate('Share an idea, suggest an improvement or report a problem.'),
      maxLines: slim ? 1 : 3,
      overflow: TextOverflow.ellipsis,
      style: TextStyle(color: muted, fontSize: 12.5, height: 1.4),
    );
    final button = OutlinedButton.icon(
      onPressed: () => showBitproFeedbackDialog(),
      icon: const Icon(Icons.edit_outlined, size: 15),
      label: Text(translate('Send feedback')),
      style: OutlinedButton.styleFrom(
        foregroundColor: MyTheme.accent,
        backgroundColor: Colors.transparent,
        side: BorderSide(color: MyTheme.accent.withOpacity(0.55)),
        minimumSize: const Size(0, 34),
        padding: const EdgeInsets.symmetric(horizontal: 14),
        textStyle: const TextStyle(fontSize: 13, fontWeight: FontWeight.w600),
        shape:
            RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
      ),
    );

    if (slim) {
      return Container(
        padding: const EdgeInsets.fromLTRB(14, 10, 10, 10),
        decoration: decoration,
        child: Row(
          children: [
            iconTile,
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [titleText, const SizedBox(height: 2), bodyText],
              ),
            ),
            const SizedBox(width: 12),
            button,
          ],
        ),
      );
    }
    return Container(
      padding: const EdgeInsets.all(20),
      decoration: decoration,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(children: [
            iconTile,
            const SizedBox(width: 12),
            Expanded(child: titleText),
          ]),
          const SizedBox(height: 12),
          bodyText,
          const Spacer(),
          SizedBox(width: double.infinity, child: button),
        ],
      ),
    );
  }
}

/// The feedback popup.
void showBitproFeedbackDialog() {
  final titleCtrl = TextEditingController();
  final messageCtrl = TextEditingController();
  final contactCtrl = TextEditingController();
  var type = FeedbackType.idea;
  var sending = false;
  var sent = false;
  String? error;

  gFFI.dialogManager.show((setState, close, context) {
    final textColor =
        Theme.of(context).textTheme.titleLarge?.color ?? Colors.black87;
    final muted = textColor.withOpacity(0.6);
    final title = titleCtrl.text.trim();
    final message = messageCtrl.text.trim();
    final valid = title.length >= _kTitleMin && message.length >= _kMessageMin;

    Future<void> submit() async {
      if (!valid || sending) return;
      final problem = _problemWith(title, message, contactCtrl.text.trim());
      if (problem != null) {
        setState(() => error = problem);
        return;
      }
      setState(() {
        sending = true;
        error = null;
      });
      try {
        await _send(
          type: type,
          title: _cut(title, _kTitleMax),
          message: _cut(message, _kMessageMax),
          contact: _cut(contactCtrl.text.trim(), _kContactMax),
        );
        setState(() {
          sending = false;
          sent = true;
        });
      } catch (e) {
        setState(() {
          sending = false;
          error = translate(
              'Could not send. Check your internet connection and try again.');
        });
      }
    }

    InputDecoration deco(String hint) => InputDecoration(
          hintText: hint,
          isDense: true,
          contentPadding:
              const EdgeInsets.symmetric(horizontal: 12, vertical: 11),
        );

    /// Live length hint under a field: how many more characters are needed,
    /// then a check mark (title) or a counter (description).
    Widget lengthHint(int length, int min, {int? max}) {
      final ok = length >= min;
      const okColor = Color(0xFF16A34A);
      final text = ok
          ? (max != null ? '$length / $max' : translate('Looks good'))
          : '${translate('Minimum')} $min ${translate('characters')} · '
              '${min - length} ${translate('more')}';
      return Padding(
        padding: const EdgeInsets.only(top: 5, left: 2),
        child: Row(
          children: [
            Icon(
              ok ? Icons.check_circle_rounded : Icons.info_outline_rounded,
              size: 13,
              color: ok ? okColor : muted,
            ),
            const SizedBox(width: 5),
            Text(
              text,
              style: TextStyle(
                  fontSize: 11.5,
                  color: ok && max == null ? okColor : muted),
            ),
          ],
        ),
      );
    }

    Widget label(String s) => Padding(
          padding: const EdgeInsets.only(bottom: 6, top: 14),
          child: Text(translate(s),
              style: TextStyle(
                  fontSize: 12.5, fontWeight: FontWeight.w600, color: muted)),
        );

    if (sent) {
      return CustomAlertDialog(
        content: SizedBox(
          width: 420,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Container(
                width: 52,
                height: 52,
                decoration: BoxDecoration(
                  color: const Color(0xFF16A34A).withOpacity(0.14),
                  shape: BoxShape.circle,
                ),
                child: const Icon(Icons.check_rounded,
                    color: Color(0xFF16A34A), size: 28),
              ),
              const SizedBox(height: 14),
              Text(translate('Thank you!'),
                  style: TextStyle(
                      color: textColor,
                      fontSize: 16,
                      fontWeight: FontWeight.w600)),
              const SizedBox(height: 6),
              Text(
                translate(
                    'Your feedback was sent. We read every message and use it to make Bitpro Desk better.'),
                textAlign: TextAlign.center,
                style: TextStyle(color: muted, fontSize: 13, height: 1.4),
              ),
            ],
          ),
        ),
        actions: [dialogButton('Close', onPressed: close)],
        onSubmit: close,
        onCancel: close,
      );
    }

    return CustomAlertDialog(
      title: Row(children: [
        const Icon(Icons.forum_outlined, color: MyTheme.accent, size: 22),
        const SizedBox(width: 10),
        Text(translate('Send feedback')),
      ]),
      content: SizedBox(
        width: 420,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              translate(
                  'Tell us what you think. Ideas, improvements, problems and complaints are all welcome.'),
              style: TextStyle(color: muted, fontSize: 13, height: 1.4),
            ),
            label('Type'),
            Wrap(
              spacing: 6,
              runSpacing: 6,
              children: FeedbackType.values.map((t) {
                final selected = t == type;
                return ChoiceChip(
                  showCheckmark: false,
                  avatar: Icon(t.icon,
                      size: 15,
                      color: selected ? Colors.white : muted),
                  label: Text(translate(t.label)),
                  selected: selected,
                  onSelected: sending ? null : (_) => setState(() => type = t),
                  selectedColor: MyTheme.accent,
                  backgroundColor: Colors.transparent,
                  side: BorderSide(
                      color: selected
                          ? MyTheme.accent
                          : MyTheme.panelBorder(context)),
                  labelStyle: TextStyle(
                      fontSize: 12.5,
                      color: selected ? Colors.white : textColor,
                      fontWeight:
                          selected ? FontWeight.w600 : FontWeight.w500),
                  shape: RoundedRectangleBorder(
                      borderRadius: BorderRadius.circular(8)),
                  visualDensity: VisualDensity.compact,
                );
              }).toList(),
            ),
            label('Title'),
            TextField(
              controller: titleCtrl,
              enabled: !sending,
              autofocus: true,
              maxLength: _kTitleMax,
              decoration: deco(translate('Short summary'))
                  .copyWith(counterText: ''),
              onChanged: (_) => setState(() => error = null),
            ),
            lengthHint(title.length, _kTitleMin),
            label('Description'),
            TextField(
              controller: messageCtrl,
              enabled: !sending,
              minLines: 4,
              maxLines: 8,
              maxLength: _kMessageMax,
              decoration: deco(translate(
                      'Describe your idea or the problem. For problems, what did you do and what happened?'))
                  .copyWith(counterText: ''),
              onChanged: (_) => setState(() => error = null),
            ),
            lengthHint(message.length, _kMessageMin, max: _kMessageMax),
            label('Contact (optional)'),
            TextField(
              controller: contactCtrl,
              enabled: !sending,
              maxLength: _kContactMax,
              decoration: deco(translate('Email or WhatsApp, if you want a reply'))
                  .copyWith(counterText: ''),
            ),
            const SizedBox(height: 10),
            Text(
              translate(
                  'Your app version and Bitpro Desk ID are included to help us look into it.'),
              style: TextStyle(color: muted, fontSize: 11.5),
            ),
            if (error != null)
              Padding(
                padding: const EdgeInsets.only(top: 10),
                child: Text(error!,
                    style: const TextStyle(
                        color: Color(0xFFDC2626), fontSize: 12.5)),
              ),
          ],
        ),
      ),
      actions: [
        dialogButton('Cancel', onPressed: sending ? null : close, isOutline: true),
        dialogButton(
          sending ? 'Sending...' : 'Send',
          icon: sending
              ? const SizedBox(
                  width: 14,
                  height: 14,
                  child: CircularProgressIndicator(
                      strokeWidth: 2,
                      valueColor: AlwaysStoppedAnimation(Colors.white)),
                ).marginOnly(right: 6)
              : null,
          onPressed: valid && !sending ? submit : null,
        ),
      ],
      onSubmit: null,
      onCancel: sending ? null : close,
    );
  }, tag: 'bitpro-feedback');
}
