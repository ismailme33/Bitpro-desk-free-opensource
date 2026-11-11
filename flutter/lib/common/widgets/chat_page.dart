import 'package:dash_chat_2/dash_chat_2.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/models/chat_model.dart';
import 'package:get/get.dart';
import 'package:provider/provider.dart';
import '../../mobile/pages/home_page.dart';

enum ChatPageType {
  mobileMain,
  desktopCM,
}

class ChatPage extends StatelessWidget implements PageShape {
  late final ChatModel chatModel;
  final ChatPageType? type;

  ChatPage({ChatModel? chatModel, this.type}) {
    this.chatModel = chatModel ?? gFFI.chatModel;
  }

  @override
  final title = translate("Chat");

  @override
  final icon = unreadTopRightBuilder(gFFI.chatModel.mobileUnreadSum);

@override
final appBarActions = [
    PopupMenuButton<MessageKey>(
        tooltip: "",
        icon: unreadTopRightBuilder(gFFI.chatModel.mobileUnreadSum,
            icon: Icon(Icons.group)),
        itemBuilder: (context) {
          // only mobile need [appBarActions], just bind gFFI.chatModel
          final chatModel = gFFI.chatModel;
        if (chatModel.messages.isEmpty) {
          return [
            PopupMenuItem<MessageKey>(
              enabled: false, // Make it non-clickable
              child: Text(
                translate('No chats yet'),
                style: TextStyle(color: Colors.grey),
              ),
            ),
          ];
        }
          return chatModel.messages.entries.map((entry) {
            final key = entry.key;
            final user = entry.value.chatUser;
            final client = gFFI.serverModel.clients
                .firstWhereOrNull((e) => e.id == key.connId);
            final connected =
                gFFI.serverModel.clients.any((e) => e.id == key.connId);
            return PopupMenuItem<MessageKey>(
              child: Row(
                children: [
                  Icon(
                          key.isOut
                              ? Icons.call_made_rounded
                              : Icons.call_received_rounded,
                          color: MyTheme.accent)
                      .marginOnly(right: 6),
                  Text("${user.firstName}   ${user.id}"),
                  if (connected)
                    Container(
                      width: 10,
                      height: 10,
                      decoration: BoxDecoration(
                          shape: BoxShape.circle,
                          color: Color.fromARGB(255, 46, 205, 139)),
                    ).marginSymmetric(horizontal: 2),
                  if (client != null)
                    unreadMessageCountBuilder(client.unreadChatMessageCount)
                        .marginOnly(left: 4)
                ],
              ),
              value: key,
            );
          }).toList();
        },
        onSelected: (key) {
          gFFI.chatModel.changeCurrentKey(key);
        })
];

  @override
  Widget build(BuildContext context) {
    return ChangeNotifierProvider.value(
      value: chatModel,
      child: Container(
        color: Theme.of(context).scaffoldBackgroundColor,
        child: Consumer<ChatModel>(
          builder: (context, chatModel, child) {
            final readOnly = type == ChatPageType.mobileMain &&
                    (chatModel.currentKey.connId == ChatModel.clientModeID ||
                        gFFI.serverModel.clients.every((e) =>
                            e.id != chatModel.currentKey.connId ||
                            chatModel.currentUser == null)) ||
                type == ChatPageType.desktopCM &&
                    gFFI.serverModel.clients
                            .firstWhereOrNull(
                                (e) => e.id == chatModel.currentKey.connId)
                            ?.disconnected ==
                        true;
            final hasMessages = chatModel.messages[chatModel.currentKey]
                    ?.chatMessages
                    .isNotEmpty ??
                false;
            final textColor =
                Theme.of(context).textTheme.titleLarge?.color ?? Colors.black87;
            return LayoutBuilder(builder: (context, constraints) {
              final chat = DashChat(
                onSend: chatModel.send,
                currentUser: chatModel.me,
                messages:
                    chatModel.messages[chatModel.currentKey]?.chatMessages ??
                        [],
                readOnly: readOnly,
                inputOptions: InputOptions(
                  focusNode: chatModel.inputNode,
                  textController: chatModel.textController,
                  inputTextStyle: TextStyle(fontSize: 14, color: textColor),
                  inputDecoration: InputDecoration(
                    isDense: true,
                    hintText: translate('Write a message'),
                    filled: true,
                    fillColor: MyTheme.panel(context),
                    contentPadding: const EdgeInsets.symmetric(
                        horizontal: 12, vertical: 11),
                    border: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(10.0),
                      borderSide:
                          BorderSide(color: MyTheme.panelBorder(context)),
                    ),
                    enabledBorder: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(10.0),
                      borderSide:
                          BorderSide(color: MyTheme.panelBorder(context)),
                    ),
                  ),
                  sendButtonBuilder: defaultSendButton(
                    padding:
                        const EdgeInsets.symmetric(horizontal: 6, vertical: 0),
                    color: MyTheme.accent,
                    icon: Icons.send_rounded,
                  ),
                ),
                messageOptions: MessageOptions(
                  showOtherUsersAvatar: false,
                  showOtherUsersName: false,
                  maxWidth: constraints.maxWidth * 0.7,
                  messageTextBuilder: (message, _, __) {
                    final isOwnMessage = message.user.id.isBlank!;
                    final color = isOwnMessage ? Colors.white : textColor;
                    return Column(
                      crossAxisAlignment: isOwnMessage
                          ? CrossAxisAlignment.end
                          : CrossAxisAlignment.start,
                      children: <Widget>[
                        Text(message.text,
                            style: TextStyle(color: color, fontSize: 13.5)),
                        Text(
                          "${message.createdAt.hour}:${message.createdAt.minute.toString().padLeft(2, '0')}",
                          style: TextStyle(
                            color: color.withOpacity(0.6),
                            fontSize: 9.5,
                          ),
                        ).marginOnly(top: 3),
                      ],
                    );
                  },
                  messageDecorationBuilder:
                      (message, previousMessage, nextMessage) {
                    final isOwnMessage = message.user.id.isBlank!;
                    return defaultMessageDecoration(
                      color: isOwnMessage
                          ? MyTheme.accent
                          : MyTheme.panel(context),
                      borderTopLeft: 10,
                      borderTopRight: 10,
                      borderBottomRight: isOwnMessage ? 2 : 10,
                      borderBottomLeft: isOwnMessage ? 10 : 2,
                    );
                  },
                ),
              );
              return Stack(
                children: [
                  // Quiet empty state behind the (still usable) chat.
                  if (!hasMessages)
                    Positioned.fill(
                      child: IgnorePointer(
                        child: _EmptyChatHint(
                            readOnly: readOnly, textColor: textColor),
                      ),
                    ),
                  SelectionArea(child: chat),
                ],
              );
            });
          },
        ).paddingOnly(bottom: 8),
      ),
    );
  }
}

/// Shown while a conversation has no messages yet.
class _EmptyChatHint extends StatelessWidget {
  final bool readOnly;
  final Color textColor;
  const _EmptyChatHint(
      {Key? key, required this.readOnly, required this.textColor})
      : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.only(bottom: 56, left: 24, right: 24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: 52,
              height: 52,
              decoration: BoxDecoration(
                color: MyTheme.accent.withOpacity(0.10),
                shape: BoxShape.circle,
              ),
              child: const Icon(Icons.chat_bubble_outline_rounded,
                  color: MyTheme.accent, size: 24),
            ),
            const SizedBox(height: 14),
            Text(
              translate('No messages yet'),
              style: TextStyle(
                color: textColor.withOpacity(0.85),
                fontSize: 14,
                fontWeight: FontWeight.w600,
              ),
            ),
            const SizedBox(height: 4),
            Text(
              translate(readOnly
                  ? 'Chat is available while connected.'
                  : 'Send a message to start the conversation.'),
              textAlign: TextAlign.center,
              style: TextStyle(color: textColor.withOpacity(0.55), fontSize: 12.5),
            ),
          ],
        ),
      ),
    );
  }
}
