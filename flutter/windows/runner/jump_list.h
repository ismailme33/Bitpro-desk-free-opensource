#ifndef RUNNER_JUMP_LIST_H_
#define RUNNER_JUMP_LIST_H_

#include <flutter/binary_messenger.h>

// Registers the "bitpro/jump_list" method channel, which lets Dart publish
// pinned and recent peers to the Windows taskbar Jump List.
void RegisterJumpListChannel(flutter::BinaryMessenger* messenger);

#endif  // RUNNER_JUMP_LIST_H_
