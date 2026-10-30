// main window right pane

import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common/widgets/connection_page_title.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:get/get.dart';
import 'package:url_launcher/url_launcher_string.dart';
import 'package:window_manager/window_manager.dart';
import 'package:flutter_hbb/models/peer_model.dart';

import '../../common.dart';
import '../../common/bitpro_update.dart';
import '../../common/formatter/id_formatter.dart';
import '../../common/widgets/peer_tab_page.dart';
import '../../common/widgets/autocomplete.dart';
import '../../models/platform_model.dart';
import '../widgets/button.dart';

class OnlineStatusWidget extends StatefulWidget {
  const OnlineStatusWidget(
      {Key? key, this.onSvcStatusChanged, this.pill = false})
      : super(key: key);

  final VoidCallback? onSvcStatusChanged;
  // Compact status pill shown at the top of the connection page.
  final bool pill;

  @override
  State<OnlineStatusWidget> createState() => _OnlineStatusWidgetState();
}

/// State for the connection page.
class _OnlineStatusWidgetState extends State<OnlineStatusWidget> {
  final _svcStopped = Get.find<RxBool>(tag: 'stop-service');
  final _svcIsUsingPublicServer = true.obs;
  Timer? _updateTimer;

  double get em => 14.0;
  double? get height => bind.isIncomingOnly() ? null : em * 3;

  void onUsePublicServerGuide() {
    const url = "";
    canLaunchUrlString(url).then((can) {
      if (can) {
        launchUrlString(url);
      }
    });
  }

  @override
  void initState() {
    super.initState();
    _updateTimer = periodic_immediate(Duration(seconds: 1), () async {
      updateStatus();
    });
  }

  @override
  void dispose() {
    _updateTimer?.cancel();
    super.dispose();
  }

  Widget _buildPill(BuildContext context) {
    return Obx(() {
      widget.onSvcStatusChanged?.call();
      final stopped = _svcStopped.value;
      final status = stateGlobal.svcStatus.value;
      final Color color;
      final String title;
      final String detail;
      if (stopped) {
        color = Colors.grey;
        title = translate("Service stopped");
        detail = '';
      } else if (status == SvcStatus.connecting) {
        color = const Color(0xFFD97706);
        title = translate("Connecting...");
        detail = translate("Connecting to Bitpro Network");
      } else if (status == SvcStatus.ready) {
        color = const Color(0xFF16A34A);
        title = translate("Ready");
        detail = translate("Connected to Bitpro Network");
      } else {
        color = const Color(0xFFDC2626);
        title = translate("Not ready");
        detail = translate("Check your internet connection");
      }
      final muted =
          Theme.of(context).textTheme.titleLarge?.color?.withOpacity(0.6);
      return Container(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
        decoration: BoxDecoration(
          color: color.withOpacity(0.10),
          borderRadius: BorderRadius.circular(20),
          border: Border.all(color: color.withOpacity(0.25)),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: 8,
              height: 8,
              decoration: BoxDecoration(color: color, shape: BoxShape.circle),
            ),
            const SizedBox(width: 8),
            Text(title,
                style: TextStyle(
                    fontSize: 13, fontWeight: FontWeight.w600, color: color)),
            if (detail.isNotEmpty)
              Text('  ·  $detail',
                  style: TextStyle(fontSize: 12.5, color: muted)),
            if (stopped)
              InkWell(
                onTap: () async => await start_service(true),
                child: Text('  ·  ${translate("Start service")}',
                    style: TextStyle(
                        fontSize: 12.5,
                        color: MyTheme.accent,
                        decoration: TextDecoration.underline)),
              ),
          ],
        ),
      );
    });
  }

  @override
  Widget build(BuildContext context) {
    if (widget.pill) return _buildPill(context);
    final isIncomingOnly = bind.isIncomingOnly();
    startServiceWidget() => Offstage(
          offstage: !_svcStopped.value,
          child: InkWell(
                  onTap: () async {
                    await start_service(true);
                  },
                  child: Text(translate("Start service"),
                      style: TextStyle(
                          decoration: TextDecoration.underline, fontSize: em)))
              .marginOnly(left: em),
        );

    setupServerWidget() => Flexible(
          child: Offstage(
            offstage: !(!_svcStopped.value &&
                stateGlobal.svcStatus.value == SvcStatus.ready &&
                _svcIsUsingPublicServer.value),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.center,
              children: [
                Text(', ', style: TextStyle(fontSize: em)),
                Flexible(
                  child: InkWell(
                    onTap: onUsePublicServerGuide,
                    child: Row(
                      children: [
                        Flexible(
                          child: Text(
                            translate('setup_server_tip'),
                            style: TextStyle(
                                decoration: TextDecoration.underline,
                                fontSize: em),
                          ),
                        ),
                      ],
                    ),
                  ),
                )
              ],
            ),
          ),
        );

    basicWidget() => Row(
          crossAxisAlignment: CrossAxisAlignment.center,
          children: [
            Container(
              height: 8,
              width: 8,
              decoration: BoxDecoration(
                borderRadius: BorderRadius.circular(4),
                color: _svcStopped.value ||
                        stateGlobal.svcStatus.value == SvcStatus.connecting
                    ? kColorWarn
                    : (stateGlobal.svcStatus.value == SvcStatus.ready
                        ? Color.fromARGB(255, 50, 190, 166)
                        : Color.fromARGB(255, 224, 79, 95)),
              ),
            ).marginSymmetric(horizontal: em),
            Container(
              width: isIncomingOnly ? 226 : null,
              child: _buildConnStatusMsg(),
            ),
            // stop
            if (!isIncomingOnly) startServiceWidget(),
            // ready && public
            // No need to show the guide if is custom client.
            if (!isIncomingOnly) setupServerWidget(),
          ],
        );

    return Container(
      height: height,
      child: Obx(() => isIncomingOnly
          ? Column(
              children: [
                basicWidget(),
                Align(
                        child: startServiceWidget(),
                        alignment: Alignment.centerLeft)
                    .marginOnly(top: 2.0, left: 22.0),
              ],
            )
          : basicWidget()),
    ).paddingOnly(right: isIncomingOnly ? 8 : 0);
  }

  _buildConnStatusMsg() {
    widget.onSvcStatusChanged?.call();
    return Text(
      _svcStopped.value
          ? translate("Service is not running")
          : stateGlobal.svcStatus.value == SvcStatus.connecting
              ? translate("connecting_status")
              : stateGlobal.svcStatus.value == SvcStatus.notReady
                  ? translate("not_ready_status")
                  : translate('Ready'),
      style: TextStyle(fontSize: em),
    );
  }

  updateStatus() async {
    final status =
        jsonDecode(await bind.mainGetConnectStatus()) as Map<String, dynamic>;
    final statusNum = status['status_num'] as int;
    if (statusNum == 0) {
      stateGlobal.svcStatus.value = SvcStatus.connecting;
    } else if (statusNum == -1) {
      stateGlobal.svcStatus.value = SvcStatus.notReady;
    } else if (statusNum == 1) {
      stateGlobal.svcStatus.value = SvcStatus.ready;
    } else {
      stateGlobal.svcStatus.value = SvcStatus.notReady;
    }
    _svcIsUsingPublicServer.value = await bind.mainIsUsingPublicServer();
    try {
      stateGlobal.videoConnCount.value = status['video_conn_count'] as int;
    } catch (_) {}
  }
}

/// Connection page for connecting to a remote peer.
class ConnectionPage extends StatefulWidget {
  const ConnectionPage({Key? key}) : super(key: key);

  @override
  State<ConnectionPage> createState() => _ConnectionPageState();
}

/// State for the connection page.
class _ConnectionPageState extends State<ConnectionPage>
    with SingleTickerProviderStateMixin, WindowListener {
  /// Controller for the id input bar.
  final _idController = IDTextEditingController();

  final RxBool _idInputFocused = false.obs;

  bool isWindowMinimized = false;
  List<Peer> peers = [];

  bool isPeersLoading = false;
  bool isPeersLoaded = false;
  // https://github.com/flutter/flutter/issues/157244
  Iterable<Peer> _autocompleteOpts = [];

  @override
  void initState() {
    super.initState();
    if (_idController.text.isEmpty) {
      WidgetsBinding.instance.addPostFrameCallback((_) async {
        final lastRemoteId = await bind.mainGetLastRemoteId();
        if (lastRemoteId != _idController.id) {
          setState(() {
            _idController.id = lastRemoteId;
          });
        }
      });
    }
    Get.put<IDTextEditingController>(_idController);
    windowManager.addListener(this);
  }

  @override
  void dispose() {
    _idController.dispose();
    windowManager.removeListener(this);
    if (Get.isRegistered<IDTextEditingController>()) {
      Get.delete<IDTextEditingController>();
    }
    if (Get.isRegistered<TextEditingController>()) {
      Get.delete<TextEditingController>();
    }
    super.dispose();
  }

  @override
  void onWindowEvent(String eventName) {
    super.onWindowEvent(eventName);
    if (eventName == 'minimize') {
      isWindowMinimized = true;
    } else if (eventName == 'maximize' || eventName == 'restore') {
      if (isWindowMinimized && isWindows) {
        // windows can't update when minimized.
        Get.forceAppUpdate();
      }
      isWindowMinimized = false;
    }
  }

  @override
  void onWindowEnterFullScreen() {
    // Remove edge border by setting the value to zero.
    stateGlobal.resizeEdgeSize.value = 0;
  }

  @override
  void onWindowLeaveFullScreen() {
    // Restore edge border to default edge size.
    stateGlobal.resizeEdgeSize.value = stateGlobal.isMaximized.isTrue
        ? kMaximizeEdgeSize
        : windowResizeEdgeSize;
  }

  @override
  void onWindowClose() {
    super.onWindowClose();
    bind.mainOnMainWindowClose();
  }

  @override
  Widget build(BuildContext context) {
    final isOutgoingOnly = bind.isOutgoingOnly();
    return Column(
      children: [
        Expanded(
            child: Column(
          children: [
            // Shown only when a newer Bitpro Desk is published.
            const BitproUpdateBanner(),
            // Network status, shown at the top.
            if (!isOutgoingOnly)
              Align(
                alignment: Alignment.centerLeft,
                child: OnlineStatusWidget(pill: true),
              ).marginOnly(top: 18),
            // Support card sits next to the connect card when there is room.
            LayoutBuilder(
              builder: (context, constraints) => IntrinsicHeight(
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Flexible(child: _buildRemoteIDTextField(context)),
                    if (constraints.maxWidth >= 890) ...[
                      const SizedBox(width: 16),
                      const SizedBox(width: 340, child: _SupportCard()),
                    ],
                  ],
                ),
              ),
            ).marginOnly(top: isOutgoingOnly ? 22 : 12),
            SizedBox(height: 12),
            Divider().paddingOnly(right: 12),
            Expanded(child: PeerTabPage()),
          ],
        ).paddingOnly(left: 12.0)),
      ],
    );
  }

  /// Callback for the connect button.
  /// Connects to the selected peer.
  void onConnect({bool isFileTransfer = false}) {
    var id = _idController.id;
    connect(context, id, isFileTransfer: isFileTransfer);
  }

  Future<void> _fetchPeers() async {
    setState(() {
      isPeersLoading = true;
    });
    await Future.delayed(Duration(milliseconds: 100));
    peers = await getAllPeers();
    setState(() {
      isPeersLoading = false;
      isPeersLoaded = true;
    });
  }

  /// UI for the remote ID TextField.
  /// Search for a peer.
  Widget _buildRemoteIDTextField(BuildContext context) {
    var w = Container(
      width: 460 + 20 * 2,
      padding: const EdgeInsets.fromLTRB(20, 20, 20, 20),
      decoration: BoxDecoration(
          color: MyTheme.panel(context),
          borderRadius: const BorderRadius.all(Radius.circular(10)),
          border: Border.all(color: MyTheme.panelBorder(context))),
      child: Ink(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            getConnectionPageTitle(context, false).marginOnly(bottom: 6),
            Text(
              translate('Enter the ID of the remote device'),
              style: TextStyle(
                  fontSize: 12,
                  color: Theme.of(context)
                      .textTheme
                      .titleLarge
                      ?.color
                      ?.withOpacity(0.55)),
            ).marginOnly(bottom: 14),
            // IntrinsicHeight + stretch: the Connect button takes exactly the
            // height of the ID field.
            IntrinsicHeight(
                child: Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Expanded(
                    child: Autocomplete<Peer>(
                  optionsBuilder: (TextEditingValue textEditingValue) {
                    if (textEditingValue.text == '') {
                      _autocompleteOpts = const Iterable<Peer>.empty();
                    } else if (peers.isEmpty && !isPeersLoaded) {
                      Peer emptyPeer = Peer(
                        id: '',
                        username: '',
                        hostname: '',
                        alias: '',
                        platform: '',
                        tags: [],
                        hash: '',
                        password: '',
                        forceAlwaysRelay: false,
                        rdpPort: '',
                        rdpUsername: '',
                        loginName: '',
                      );
                      _autocompleteOpts = [emptyPeer];
                    } else {
                      String textWithoutSpaces =
                          textEditingValue.text.replaceAll(" ", "");
                      if (int.tryParse(textWithoutSpaces) != null) {
                        textEditingValue = TextEditingValue(
                          text: textWithoutSpaces,
                          selection: textEditingValue.selection,
                        );
                      }
                      String textToFind = textEditingValue.text.toLowerCase();
                      _autocompleteOpts = peers
                          .where((peer) =>
                              peer.id.toLowerCase().contains(textToFind) ||
                              peer.username
                                  .toLowerCase()
                                  .contains(textToFind) ||
                              peer.hostname
                                  .toLowerCase()
                                  .contains(textToFind) ||
                              peer.alias.toLowerCase().contains(textToFind))
                          .toList();
                    }
                    return _autocompleteOpts;
                  },
                  fieldViewBuilder: (
                    BuildContext context,
                    TextEditingController fieldTextEditingController,
                    FocusNode fieldFocusNode,
                    VoidCallback onFieldSubmitted,
                  ) {
                    fieldTextEditingController.text = _idController.text;
                    Get.put<TextEditingController>(fieldTextEditingController);
                    fieldFocusNode.addListener(() async {
                      _idInputFocused.value = fieldFocusNode.hasFocus;
                      if (fieldFocusNode.hasFocus && !isPeersLoading) {
                        _fetchPeers();
                      }
                    });
                    final textLength =
                        fieldTextEditingController.value.text.length;
                    // select all to facilitate removing text, just following the behavior of address input of chrome
                    fieldTextEditingController.selection =
                        TextSelection(baseOffset: 0, extentOffset: textLength);
                    return Obx(() => TextField(
                          autocorrect: false,
                          enableSuggestions: false,
                          keyboardType: TextInputType.visiblePassword,
                          focusNode: fieldFocusNode,
                          style: const TextStyle(
                            fontFamily: 'WorkSans',
                            fontSize: 20,
                            height: 1.4,
                          ),
                          maxLines: 1,
                          cursorColor:
                              Theme.of(context).textTheme.titleLarge?.color,
                          decoration: InputDecoration(
                              filled: false,
                              counterText: '',
                              hintText: _idInputFocused.value
                                  ? null
                                  : translate('Enter Remote ID'),
                              contentPadding: const EdgeInsets.symmetric(
                                  horizontal: 14, vertical: 11)),
                          controller: fieldTextEditingController,
                          inputFormatters: [IDTextInputFormatter()],
                          onChanged: (v) {
                            _idController.id = v;
                          },
                          onSubmitted: (_) {
                            onConnect();
                          },
                        ).workaroundFreezeLinuxMint());
                  },
                  onSelected: (option) {
                    setState(() {
                      _idController.id = option.id;
                      FocusScope.of(context).unfocus();
                    });
                  },
                  optionsViewBuilder: (BuildContext context,
                      AutocompleteOnSelected<Peer> onSelected,
                      Iterable<Peer> options) {
                    options = _autocompleteOpts;
                    double maxHeight = options.length * 50;
                    if (options.length == 1) {
                      maxHeight = 52;
                    } else if (options.length == 3) {
                      maxHeight = 146;
                    } else if (options.length == 4) {
                      maxHeight = 193;
                    }
                    maxHeight = maxHeight.clamp(0, 200);

                    return Align(
                      alignment: Alignment.topLeft,
                      child: Container(
                          decoration: BoxDecoration(
                            boxShadow: [
                              BoxShadow(
                                color: Colors.black.withOpacity(0.3),
                                blurRadius: 5,
                                spreadRadius: 1,
                              ),
                            ],
                          ),
                          child: ClipRRect(
                              borderRadius: BorderRadius.circular(5),
                              child: Material(
                                elevation: 4,
                                child: ConstrainedBox(
                                  constraints: BoxConstraints(
                                    maxHeight: maxHeight,
                                    maxWidth: 338,
                                  ),
                                  child: peers.isEmpty && isPeersLoading
                                      ? Container(
                                          height: 80,
                                          child: Center(
                                            child: CircularProgressIndicator(
                                              strokeWidth: 2,
                                            ),
                                          ))
                                      : Padding(
                                          padding:
                                              const EdgeInsets.only(top: 5),
                                          child: ListView(
                                            children: options
                                                .map((peer) =>
                                                    AutocompletePeerTile(
                                                        onSelect: () =>
                                                            onSelected(peer),
                                                        peer: peer))
                                                .toList(),
                                          ),
                                        ),
                                ),
                              ))),
                    );
                  },
                )),
                const SizedBox(width: 12),
                // File transfer stays available from each peer's menu.
                Button(
                  onTap: onConnect,
                  text: "Connect",
                  minWidth: 110,
                  textSize: 14,
                  radius: 8,
                ),
              ],
            )),
          ],
        ),
      ),
    );
    return Container(
        constraints: const BoxConstraints(maxWidth: 600), child: w);
  }
}

/// "Free project" card shown next to the connect card.
class _SupportCard extends StatelessWidget {
  const _SupportCard({Key? key}) : super(key: key);

  static const _phone = '+966570624971';
  static const _phoneDisplay = '+966 57 062 4971';
  static const _supportUrl = 'https://wa.me/966570624971';

  @override
  Widget build(BuildContext context) {
    return ClipRRect(
      borderRadius: BorderRadius.circular(10),
      child: CustomPaint(
        painter: _SupportCardTexture(),
        child: Container(
          padding: const EdgeInsets.all(20),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              // Eyebrow label.
              Container(
                padding:
                    const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
                decoration: BoxDecoration(
                  color: Colors.white.withOpacity(0.16),
                  borderRadius: BorderRadius.circular(20),
                ),
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    const Icon(Icons.favorite_rounded,
                        color: Colors.white, size: 11),
                    const SizedBox(width: 5),
                    Text(
                      translate('FREE PROJECT'),
                      style: const TextStyle(
                        color: Colors.white,
                        fontSize: 10.5,
                        fontWeight: FontWeight.w700,
                        letterSpacing: 1.2,
                      ),
                    ),
                  ],
                ),
              ),
              const SizedBox(height: 12),
              Text(
                '$appName is free',
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 18,
                  fontWeight: FontWeight.w700,
                  height: 1.2,
                ),
              ),
              const SizedBox(height: 4),
              Text(
                'Developed by Ismail Ibrahim',
                style: TextStyle(
                  color: Colors.white.withOpacity(0.8),
                  fontSize: 12.5,
                  height: 1.3,
                ),
              ),
              const SizedBox(height: 16),
              const Spacer(),
              // Footer: support button and phone number.
              Row(
                children: [
                  // Opens a WhatsApp chat with the developer.
                  _CoffeeButton(onTap: () => launchUrlString(_supportUrl)),
                  const Spacer(),
                  // Click to copy the phone number.
                  InkWell(
                    onTap: () {
                      Clipboard.setData(const ClipboardData(text: _phone));
                      showToast(translate('Copied'));
                    },
                    borderRadius: BorderRadius.circular(6),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Icon(Icons.phone_rounded,
                            color: Colors.white.withOpacity(0.85), size: 13),
                        const SizedBox(width: 5),
                        Text(
                          _phoneDisplay,
                          style: TextStyle(
                            color: Colors.white.withOpacity(0.9),
                            fontSize: 12,
                            fontWeight: FontWeight.w500,
                          ),
                        ),
                      ],
                    ).paddingSymmetric(vertical: 4),
                  ),
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Rose gradient with faint diagonal lines and soft circles.
class _SupportCardTexture extends CustomPainter {
  @override
  void paint(Canvas canvas, Size size) {
    final rect = Offset.zero & size;
    canvas.drawRect(
      rect,
      Paint()
        ..shader = const LinearGradient(
          begin: Alignment.topLeft,
          end: Alignment.bottomRight,
          colors: [Color(0xFFF43F5E), Color(0xFFBE123C)],
        ).createShader(rect),
    );
    final lines = Paint()
      ..color = Colors.white.withOpacity(0.05)
      ..strokeWidth = 1;
    for (double x = -size.height; x < size.width; x += 14) {
      canvas.drawLine(
          Offset(x, size.height), Offset(x + size.height, 0), lines);
    }
    final circles = Paint()..color = Colors.white.withOpacity(0.07);
    canvas.drawCircle(Offset(size.width - 20, 18), 58, circles);
    canvas.drawCircle(Offset(size.width + 10, size.height - 10), 70, circles);
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) => false;
}

/// "Buy me a coffee" button in the classic coffee yellow, with a gentle
/// heartbeat pulse, warm glow and cup wiggle so it gets noticed.
class _CoffeeButton extends StatefulWidget {
  final VoidCallback onTap;
  const _CoffeeButton({Key? key, required this.onTap}) : super(key: key);

  @override
  State<_CoffeeButton> createState() => _CoffeeButtonState();
}

class _CoffeeButtonState extends State<_CoffeeButton>
    with SingleTickerProviderStateMixin {
  static const _yellow = Color(0xFFFFDD00);
  static const _brown = Color(0xFF3B2416);

  late final AnimationController _ctrl = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 2400),
  )..repeat();

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
    return AnimatedBuilder(
      animation: _ctrl,
      builder: (context, child) {
        final b = _beat(_ctrl.value);
        // Cup wiggles during the beat.
        final wiggle = b * 0.25 * (_ctrl.value < 0.2 ? 1 : -1);
        return Transform.scale(
          scale: 1 + 0.05 * b,
          child: InkWell(
            onTap: widget.onTap,
            borderRadius: BorderRadius.circular(8),
            child: Container(
              height: 34,
              padding: const EdgeInsets.symmetric(horizontal: 14),
              decoration: BoxDecoration(
                color: _yellow,
                borderRadius: BorderRadius.circular(8),
                boxShadow: [
                  BoxShadow(
                    color: _yellow.withOpacity(0.35 + 0.4 * b),
                    blurRadius: 8 + 14 * b,
                    spreadRadius: 1 * b,
                  ),
                ],
              ),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Transform.rotate(
                    angle: wiggle,
                    child: const Icon(Icons.local_cafe_rounded,
                        color: _brown, size: 16),
                  ),
                  const SizedBox(width: 7),
                  const Text(
                    'Buy me a coffee',
                    style: TextStyle(
                      color: _brown,
                      fontSize: 13,
                      fontWeight: FontWeight.w700,
                    ),
                  ),
                ],
              ),
            ),
          ),
        );
      },
    );
  }
}
