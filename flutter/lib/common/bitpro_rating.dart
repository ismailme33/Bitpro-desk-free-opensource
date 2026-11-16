import 'dart:async';
import 'dart:ffi' hide Size;
import 'dart:io';

import 'package:ffi/ffi.dart';
import 'package:flutter/material.dart';
import 'package:get/get.dart';
import 'package:url_launcher/url_launcher.dart';
import 'package:window_manager/window_manager.dart';

import '../common.dart';
import '../models/platform_model.dart';

/// "Rate Bitpro Desk" prompt for the Microsoft Store version.
///
/// Shown at most once every [_kPromptInterval] until the user picks
/// "Rate now", which opens the app's review page in the Microsoft Store.
/// The first prompt waits [_kFirstPromptDelay] after the first launch so new
/// users get to try the app before being asked.
const Duration _kPromptInterval = Duration(days: 30);
const Duration _kFirstPromptDelay = Duration(days: 3);
const Duration _kStartupDelay = Duration(seconds: 20);

const String _kOptRated = 'bitpro-store-rated';
const String _kOptFirstRun = 'bitpro-rate-first-run';
const String _kOptLastPrompt = 'bitpro-rate-last-prompt';

/// "Buy me a coffee" tip link.
const String kBuyMeACoffeeUrl = 'https://www.buymeacoffee.com/ismailme33';

void openBuyMeACoffee() {
  _noteCoffeeClicked();
  launchUrl(Uri.parse(kBuyMeACoffeeUrl), mode: LaunchMode.externalApplication);
}

/// "Buy me a coffee" button.
Widget buyMeACoffeeButton() => const BuyMeACoffeeButton();

/// The official Buy Me a Coffee button (assets/buy-me-a-coffee.png, from
/// cdn.buymeacoffee.com/buttons/v2/default-yellow.png). With [pulse] it
/// gives a gentle heartbeat and warm glow every few seconds.
class BuyMeACoffeeButton extends StatefulWidget {
  final double height;
  final bool pulse;
  const BuyMeACoffeeButton({Key? key, this.height = 36, this.pulse = false})
      : super(key: key);

  @override
  State<BuyMeACoffeeButton> createState() => _BuyMeACoffeeButtonState();
}

class _BuyMeACoffeeButtonState extends State<BuyMeACoffeeButton>
    with SingleTickerProviderStateMixin {
  static const _yellow = Color(0xFFFFDD00);
  static const _aspect = 545 / 153; // image size
  late final AnimationController _ctrl = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 2400),
  );
  bool _hover = false;

  @override
  void initState() {
    super.initState();
    if (widget.pulse) _ctrl.repeat();
  }

  @override
  void dispose() {
    _ctrl.dispose();
    super.dispose();
  }

  // Two quick beats, then rest: 0..1 over the cycle.
  double _beat(double t) {
    double bump(double c) {
      final d = (t - c).abs() / 0.08;
      return d >= 1 ? 0 : (1 - d * d);
    }

    return (bump(0.10) + 0.7 * bump(0.28)).clamp(0.0, 1.0);
  }

  @override
  Widget build(BuildContext context) {
    final h = widget.height;
    final radius = BorderRadius.circular(h * 0.22);
    final image = Image.asset(
      'assets/buy-me-a-coffee.png',
      height: h,
      width: h * _aspect,
      fit: BoxFit.contain,
      filterQuality: FilterQuality.medium,
      semanticLabel: 'Buy Me a Coffee',
    );
    return Tooltip(
      message: kBuyMeACoffeeUrl.replaceFirst('https://', ''),
      child: MouseRegion(
        cursor: SystemMouseCursors.click,
        onEnter: (_) => setState(() => _hover = true),
        onExit: (_) => setState(() => _hover = false),
        child: GestureDetector(
          onTap: openBuyMeACoffee,
          child: AnimatedBuilder(
            animation: _ctrl,
            child: image,
            builder: (context, child) {
              final b = widget.pulse ? _beat(_ctrl.value) : 0.0;
              final glow = _hover ? 1.0 : b;
              return Transform.scale(
                scale: 1 + 0.04 * b + (_hover ? 0.03 : 0),
                child: Container(
                  decoration: BoxDecoration(
                    borderRadius: radius,
                    boxShadow: [
                      BoxShadow(
                        color: _yellow.withOpacity(0.25 + 0.4 * glow),
                        blurRadius: 6 + 12 * glow,
                      ),
                    ],
                  ),
                  child: ClipRRect(borderRadius: radius, child: child),
                ),
              );
            },
          ),
        ),
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// After-session "thank you" bar.
//
// Asks right after the app has just helped, but gently: a slim bar in the
// main window (no popup, never takes focus), only after every
// [_kSessionsPerAsk]th remote session that stayed connected at least
// [kBitproGoodSessionLength], at most once per [_kCoffeeEvery], never on the
// same day as the rating popup, paused [_kCoffeeAfterClick] after someone
// clicks the coffee button, and off for good with "Don't ask again".
// ---------------------------------------------------------------------------

/// Sent from a remote window to the main window when a session ends.
const String kWindowEventBitproSessionDone = 'bitpro_session_done';
const Duration kBitproGoodSessionLength = Duration(minutes: 2);
const int _kSessionsPerAsk = 5;
const Duration _kCoffeeEvery = Duration(days: 14);
const Duration _kCoffeeAfterClick = Duration(days: 90);

const String _kOptGoodSessions = 'bitpro-good-sessions';
const String _kOptCoffeeLastAsk = 'bitpro-coffee-last-ask';
const String _kOptCoffeeClicked = 'bitpro-coffee-clicked';
const String _kOptCoffeeNever = 'bitpro-coffee-never';

/// True while the thank-you bar is shown in the main window.
final RxBool bitproCoffeeBar = false.obs;

/// Main window: a remote session that stayed connected long enough ended.
Future<void> onBitproGoodSession() async {
  try {
    if (bind.mainGetLocalOption(key: _kOptCoffeeNever) == 'Y') return;
    final n = (int.tryParse(
                bind.mainGetLocalOption(key: _kOptGoodSessions)) ??
            0) +
        1;
    await bind.mainSetLocalOption(key: _kOptGoodSessions, value: '$n');
    if (n < _kSessionsPerAsk || bitproCoffeeBar.value) return;

    final now = DateTime.now().millisecondsSinceEpoch;
    if (now - _getMs(_kOptCoffeeLastAsk) < _kCoffeeEvery.inMilliseconds) {
      return;
    }
    if (now - _getMs(_kOptCoffeeClicked) <
        _kCoffeeAfterClick.inMilliseconds) {
      return;
    }
    // Don't stack with the rating popup.
    if (now - _getMs(_kOptLastPrompt) < const Duration(days: 1).inMilliseconds) {
      return;
    }
    // Main window hidden in the tray or minimized: keep the count and try
    // again after the next session instead of showing it unseen.
    if (!await windowManager.isVisible() || await windowManager.isMinimized()) {
      return;
    }
    await _setMs(_kOptCoffeeLastAsk, now);
    await bind.mainSetLocalOption(key: _kOptGoodSessions, value: '0');
    bitproCoffeeBar.value = true;
  } catch (e) {
    debugPrint('onBitproGoodSession: $e');
  }
}

/// Remembers a click on any coffee button (pauses the thank-you bar).
void _noteCoffeeClicked() {
  bitproCoffeeBar.value = false;
  _setMs(_kOptCoffeeClicked, DateTime.now().millisecondsSinceEpoch);
}

/// Slim thank-you bar for the top of the home screen.
class BitproCoffeeBar extends StatelessWidget {
  const BitproCoffeeBar({Key? key}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Obx(() {
      if (!bitproCoffeeBar.value) return const SizedBox.shrink();
      final muted =
          Theme.of(context).textTheme.titleLarge?.color?.withOpacity(0.6);
      return Align(
        alignment: Alignment.centerLeft,
        child: Container(
          constraints: const BoxConstraints(maxWidth: 856),
          margin: const EdgeInsets.only(top: 16, right: 12),
          padding: const EdgeInsets.fromLTRB(14, 10, 6, 10),
          decoration: BoxDecoration(
            color: MyTheme.panel(context),
            border: Border.all(color: MyTheme.panelBorder(context)),
            borderRadius: BorderRadius.circular(10),
          ),
          child: Row(
            children: [
              Container(
                width: 34,
                height: 34,
                decoration: BoxDecoration(
                  color: const Color(0xFFFFDD00).withOpacity(0.25),
                  borderRadius: BorderRadius.circular(9),
                ),
                child: const Icon(Icons.local_cafe_rounded,
                    color: Color(0xFF6B4A1F), size: 18),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(
                      '${translate('Thanks for using')} $appName!',
                      overflow: TextOverflow.ellipsis,
                      style: const TextStyle(
                          fontSize: 13.5, fontWeight: FontWeight.w600),
                    ),
                    const SizedBox(height: 3),
                    Text(
                      translate(
                          'If it saved you time today, a coffee keeps it free, updated and ad-free.'),
                      maxLines: 2,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(fontSize: 12.5, color: muted),
                    ),
                  ],
                ),
              ),
              const SizedBox(width: 12),
              const BuyMeACoffeeButton(height: 34),
              const SizedBox(width: 6),
              TextButton(
                onPressed: () {
                  bitproCoffeeBar.value = false;
                  bind.mainSetLocalOption(key: _kOptCoffeeNever, value: 'Y');
                },
                style: TextButton.styleFrom(
                  foregroundColor: muted,
                  minimumSize: const Size(0, 32),
                  padding: const EdgeInsets.symmetric(horizontal: 10),
                  textStyle: const TextStyle(fontSize: 12.5),
                ),
                child: Text(translate("Don't ask again")),
              ),
              IconButton(
                tooltip: translate('Close'),
                onPressed: () => bitproCoffeeBar.value = false,
                icon: Icon(Icons.close_rounded, size: 18, color: muted),
                splashRadius: 16,
                padding: EdgeInsets.zero,
                constraints:
                    const BoxConstraints(minWidth: 32, minHeight: 32),
              ),
            ],
          ),
        ),
      );
    });
  }
}

bool _scheduled = false;

/// True when running from a Microsoft Store (MSIX) package.
bool isMsixPackage() =>
    Platform.resolvedExecutable.toLowerCase().contains('\\windowsapps\\');

/// The package family name of this MSIX package, or null if unpackaged.
String? msixPackageFamilyName() {
  try {
    final kernel32 = DynamicLibrary.open('kernel32.dll');
    final getPfn = kernel32.lookupFunction<
        Int32 Function(Pointer<Uint32>, Pointer<Utf16>),
        int Function(Pointer<Uint32>, Pointer<Utf16>)>(
      'GetCurrentPackageFamilyName',
    );
    final len = calloc<Uint32>()..value = 0;
    try {
      getPfn(len, nullptr); // ERROR_INSUFFICIENT_BUFFER, sets len
      if (len.value == 0) return null; // APPMODEL_ERROR_NO_PACKAGE
      final buf = calloc<Uint16>(len.value).cast<Utf16>();
      try {
        if (getPfn(len, buf) != 0) return null;
        return buf.toDartString();
      } finally {
        calloc.free(buf);
      }
    } finally {
      calloc.free(len);
    }
  } catch (e) {
    debugPrint('GetCurrentPackageFamilyName: $e');
    return null;
  }
}

int _getMs(String key) =>
    int.tryParse(bind.mainGetLocalOption(key: key)) ?? 0;

Future<void> _setMs(String key, int ms) =>
    bind.mainSetLocalOption(key: key, value: ms.toString());

/// Call once from the main window. Does nothing outside the Store version.
void startBitproRatingPrompt() {
  if (!isWindows || _scheduled || !isMsixPackage()) return;
  _scheduled = true;
  Timer(_kStartupDelay, _maybePrompt);
}

Future<void> _maybePrompt() async {
  if (bind.mainGetLocalOption(key: _kOptRated) == 'Y') return;
  final pfn = msixPackageFamilyName();
  if (pfn == null || pfn.isEmpty) return;

  final now = DateTime.now().millisecondsSinceEpoch;
  var firstRun = _getMs(_kOptFirstRun);
  if (firstRun == 0) {
    await _setMs(_kOptFirstRun, now);
    firstRun = now;
  }
  if (now - firstRun < _kFirstPromptDelay.inMilliseconds) return;
  if (now - _getMs(_kOptLastPrompt) < _kPromptInterval.inMilliseconds) {
    return;
  }
  // Count it as shown now, so closing the app mid-dialog doesn't re-prompt.
  await _setMs(_kOptLastPrompt, now);
  _showDialog(pfn);
}

void _showDialog(String pfn) {
  gFFI.dialogManager.show((setState, close, context) {
    Future<void> rateNow() async {
      close();
      await bind.mainSetLocalOption(key: _kOptRated, value: 'Y');
      final uri = Uri.parse('ms-windows-store://review/?PFN=$pfn');
      if (!await launchUrl(uri)) {
        // Store app missing or blocked: let them ask again next month.
        await bind.mainSetLocalOption(key: _kOptRated, value: '');
      }
    }

    return CustomAlertDialog(
      title: Row(children: [
        const Icon(Icons.star_rounded, color: Colors.amber, size: 28),
        const SizedBox(width: 8),
        Text(translate('Enjoying Bitpro Desk?')),
      ]),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            mainAxisAlignment: MainAxisAlignment.center,
            children: List.generate(
              5,
              (_) => const Icon(Icons.star_rounded,
                  color: Colors.amber, size: 36),
            ),
          ),
          const SizedBox(height: 12),
          Text(translate(
              'If Bitpro Desk is useful to you, please take a moment to rate it in the Microsoft Store. It really helps us!')),
          const SizedBox(height: 8),
          Center(child: buyMeACoffeeButton()),
        ],
      ),
      actions: [
        dialogButton('Later', onPressed: close, isOutline: true),
        dialogButton('Rate now', onPressed: rateNow),
      ],
      onSubmit: rateNow,
      onCancel: close,
    );
  }, tag: 'bitpro-rate-prompt');
}
