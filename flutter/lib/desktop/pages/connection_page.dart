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
import '../../common/bitpro_rating.dart';
import '../../common/connecting_hint.dart';
import '../../common/bitpro_feedback.dart';
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
              InkWell(mouseCursor: WidgetStateMouseCursor.clickable, 
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
          child: InkWell(mouseCursor: WidgetStateMouseCursor.clickable, 
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
                  child: InkWell(mouseCursor: WidgetStateMouseCursor.clickable, 
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
/// Height of the ID field and the Connect button next to it.
const double _kIdFieldHeight = 50;

/// Shared height of the connect card and the support card side by side.
const double _kConnectRowHeight = 186;

/// Width of the feedback card when it sits in the row.
const double _kFeedbackCardWidth = 280;

class _ConnectionPageState extends State<ConnectionPage>
    with SingleTickerProviderStateMixin, WindowListener {
  /// Controller for the id input bar.
  final _idController = IDTextEditingController();

  final RxBool _idInputFocused = false.obs;
  FocusNode? _idFocusNode;

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

  // The session window took focus: the "Connecting to" hint is done.
  @override
  void onWindowBlur() => hideConnectingHint();

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
            // Occasional thank-you after a successful session.
            const BitproCoffeeBar(),
            // Network status, shown at the top.
            if (!isOutgoingOnly)
              Align(
                alignment: Alignment.centerLeft,
                child: OnlineStatusWidget(pill: true),
              ).marginOnly(top: 18),
            // Support card sits next to the connect card when there is room,
            // otherwise a slim version of it goes below. Decided from the
            // window width, not a LayoutBuilder: the ID autocomplete field
            // must not be built during layout (debug assertions).
            // The feedback card joins the row on wide windows; otherwise a
            // slim strip of it goes below the cards.
            (_hasRoomBesideConnectCard(context)
                    ? Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          SizedBox(
                            // All cards share this height (see _kConnectRowHeight).
                            height: _kConnectRowHeight,
                            child: Row(
                              crossAxisAlignment: CrossAxisAlignment.stretch,
                              children: [
                                Flexible(
                                    child: _buildRemoteIDTextField(context)),
                                const SizedBox(width: 16),
                                const SizedBox(
                                    width: 340, child: _SupportCard()),
                                if (_hasRoomForFeedbackCard(context)) ...[
                                  const SizedBox(width: 16),
                                  const SizedBox(
                                      width: _kFeedbackCardWidth,
                                      child: BitproFeedbackCard()),
                                ],
                              ],
                            ),
                          ),
                          if (!_hasRoomForFeedbackCard(context)) ...[
                            const SizedBox(height: 12),
                            const SizedBox(
                                width: 500 + 16 + 340,
                                child: BitproFeedbackCard(slim: true)),
                          ],
                        ],
                      )
                    : Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          _buildRemoteIDTextField(context),
                          const SizedBox(height: 12),
                          // Same width as the connect card.
                          const SizedBox(
                              width: 460 + 20 * 2,
                              child: _SupportCard(compact: true)),
                          const SizedBox(height: 12),
                          const SizedBox(
                              width: 460 + 20 * 2,
                              child: BitproFeedbackCard(slim: true)),
                        ],
                      ))
                .marginOnly(top: isOutgoingOnly ? 22 : 12),
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

  /// True when the support card fits next to the connect card (500 + 16 +
  /// 340 px). The home screen's left pane (248), its divider (1) and this
  /// page's left padding (12) take the rest of the window width.
  bool _hasRoomBesideConnectCard(BuildContext context) {
    const leftPaneAndPadding = 248.0 + 1 + 12 + 12;
    final available = MediaQuery.sizeOf(context).width - leftPaneAndPadding;
    return available >= 890;
  }

  /// True when the feedback card also fits in the row (890 + 16 + 280).
  bool _hasRoomForFeedbackCard(BuildContext context) {
    const leftPaneAndPadding = 248.0 + 1 + 12 + 12;
    final available = MediaQuery.sizeOf(context).width - leftPaneAndPadding;
    return available >= 890 + 16 + _kFeedbackCardWidth;
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
          // Centered when the card is stretched to the support card height.
          mainAxisAlignment: MainAxisAlignment.center,
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
            // Fixed height + stretch: the Connect button is exactly as tall as
            // the ID field. (No IntrinsicHeight: the autocomplete field and
            // the auto-size title can't report intrinsic sizes.)
            SizedBox(
                height: _kIdFieldHeight,
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
                    // This builder runs during layout (inside a LayoutBuilder),
                    // so the field must not be changed here directly: sync the
                    // ID after the frame, and only when it actually differs.
                    // Select all to make replacing the ID easy (like Chrome's
                    // address bar).
                    final text = _idController.text;
                    if (fieldTextEditingController.text != text) {
                      WidgetsBinding.instance.addPostFrameCallback((_) {
                        if (fieldTextEditingController.text == text) return;
                        fieldTextEditingController.value = TextEditingValue(
                          text: text,
                          selection: TextSelection(
                              baseOffset: 0, extentOffset: text.length),
                        );
                      });
                    }
                    Get.put<TextEditingController>(fieldTextEditingController);
                    // Listen once per focus node (builds can repeat).
                    if (!identical(_idFocusNode, fieldFocusNode)) {
                      _idFocusNode = fieldFocusNode;
                      fieldFocusNode.addListener(() async {
                        _idInputFocused.value = fieldFocusNode.hasFocus;
                        if (fieldFocusNode.hasFocus && !isPeersLoading) {
                          _fetchPeers();
                        }
                      });
                    }
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
  /// Slim one-row layout used below the connect card on narrow windows.
  final bool compact;
  const _SupportCard({Key? key, this.compact = false}) : super(key: key);

  static const _email = 'bitproglobal3@gmail.com';

  static String get _headline => '${translate('Enjoying')} $appName?';
  static const _pitch = 'A coffee keeps it free, updated and ad-free.';

  static final _contactStyle = TextStyle(
    color: Colors.white.withOpacity(0.9),
    fontSize: 12,
    fontWeight: FontWeight.w500,
  );

  /// Small white icon + text link.
  Widget _contact(IconData icon, String text, String tooltip,
      VoidCallback onTap) {
    return Tooltip(
      message: translate(tooltip),
      child: InkWell(mouseCursor: WidgetStateMouseCursor.clickable, 
        onTap: onTap,
        borderRadius: BorderRadius.circular(6),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, color: Colors.white.withOpacity(0.85), size: 13),
            const SizedBox(width: 5),
            Text(text, style: _contactStyle),
          ],
        ).paddingSymmetric(vertical: 3, horizontal: 2),
      ),
    );
  }

  /// Email is copied (not every PC has a mail app set up for mailto: links).
  Widget _emailLink() => _contact(Icons.mail_rounded, _email, 'Click to copy',
      () {
        Clipboard.setData(const ClipboardData(text: _email));
        showToast(translate('Copied'));
      });

  Widget _coffee({double height = 44}) =>
      BuyMeACoffeeButton(height: height, pulse: true);

  Widget _compactBody() {
    return Row(
      children: [
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                _headline,
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 15,
                  fontWeight: FontWeight.w700,
                ),
              ),
              const SizedBox(height: 2),
              Text(
                translate(_pitch),
                style: TextStyle(
                  color: Colors.white.withOpacity(0.8),
                  fontSize: 12,
                ),
              ),
              const SizedBox(height: 4),
              _emailLink(),
            ],
          ),
        ),
        const SizedBox(width: 12),
        _coffee(height: 40),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    return ClipRRect(
      borderRadius: BorderRadius.circular(10),
      child: CustomPaint(
        painter: _SupportCardTexture(),
        child: Container(
          padding: compact
              ? const EdgeInsets.fromLTRB(18, 12, 16, 12)
              : const EdgeInsets.all(20),
          child: compact ? _compactBody() : Column(
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
                _headline,
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 18,
                  fontWeight: FontWeight.w700,
                  height: 1.2,
                ),
              ),
              const SizedBox(height: 4),
              Text(
                translate(_pitch),
                style: TextStyle(
                  color: Colors.white.withOpacity(0.8),
                  fontSize: 12.5,
                  height: 1.3,
                ),
              ),
              const SizedBox(height: 4),
              _emailLink(),
              const Spacer(),
              _coffee(),
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

