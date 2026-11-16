import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';
import 'package:flutter/foundation.dart';
import 'package:win32/win32.dart';

import '../common.dart';
import '../models/platform_model.dart';
import 'bitpro_rating.dart' show msixPackageFamilyName, isMsixPackage;

/// Desktop icon for the Microsoft Store (MSIX) version.
///
/// Store apps only get a Start menu entry. The package's own desktop7:Shortcut
/// is not used: Windows draws a blank icon for it (tested on Windows 11). So
/// the app creates the icon on first start, on Windows 10 and 11.
///
/// The icon points to the app's Start menu entry (shell:AppsFolder), exactly
/// like dragging the app from Start to the desktop: Windows shows the app's
/// logo, and it keeps working after Store updates move the app's files.
/// Created once only: if the user deletes it, it is not created again.
const String _kOptDone = 'bitpro-desktop-shortcut-v2';
const String _kAppId = 'BitproDesk'; // <Application Id> in the manifest
const String _kLinkName = 'Bitpro Desk.lnk';
const int _kStgmRead = 0x00000000; // STGM_READ
const int _kSlgpRawPath = 0x4; // SLGP_RAWPATH

final _shParseDisplayName = DynamicLibrary.open('shell32.dll').lookupFunction<
    Int32 Function(Pointer<Utf16>, Pointer, Pointer<Pointer<ITEMIDLIST>>,
        Uint32, Pointer<Uint32>),
    int Function(Pointer<Utf16>, Pointer, Pointer<Pointer<ITEMIDLIST>>, int,
        Pointer<Uint32>)>('SHParseDisplayName');

String? _desktopFolder() {
  final path = calloc<Pointer<Utf16>>();
  final id = GUIDFromString(FOLDERID_Desktop);
  try {
    if (FAILED(SHGetKnownFolderPath(id, 0, NULL, path))) return null;
    return path.value.toDartString();
  } finally {
    CoTaskMemFree(path.value);
    free(id);
    free(path);
  }
}

/// Call once from the main window. Does nothing outside the Store version.
Future<void> ensureMsixDesktopShortcut() async {
  if (!isWindows || !isMsixPackage()) return;
  if (bind.mainGetLocalOption(key: _kOptDone) == 'Y') return;
  // Remember first, so a failure never makes it retry on every start.
  await bind.mainSetLocalOption(key: _kOptDone, value: 'Y');
  // Let the main window open first; the icon is a quiet extra.
  await Future.delayed(const Duration(seconds: 4));
  try {
    _createShortcut();
  } catch (e) {
    debugPrint('desktop shortcut: $e');
  }
}

/// The file path a shortcut points to, or '' when it points to a non-file
/// item such as the app's Start menu entry.
String _shortcutTargetPath(String lnkPath) {
  final link = ShellLink.createInstance();
  final file = lnkPath.toNativeUtf16();
  final buf = wsalloc(MAX_PATH);
  try {
    // COM objects are released automatically by win32's Finalizer; calling
    // release() as well frees them twice and crashes the app.
    final persist = IPersistFile.from(link);
    final ok = SUCCEEDED(persist.load(file, _kStgmRead));
    if (!ok) return '';
    if (link.getPath(buf, MAX_PATH, nullptr, _kSlgpRawPath) != S_OK) return '';
    return buf.toDartString();
  } finally {
    free(file);
    free(buf);
  }
}

void _createShortcut() {
  final pfn = msixPackageFamilyName();
  final desktop = _desktopFolder();
  if (pfn == null || desktop == null) return;
  final lnkPath = '$desktop\\$_kLinkName';

  CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED); // ok if already done

  if (File(lnkPath).existsSync()) {
    // Version 1.46.83 had the package create this icon, which Windows draws
    // blank. That one points straight at the exe in WindowsApps: replace it.
    // Anything else with this name is left alone.
    final target = _shortcutTargetPath(lnkPath).toLowerCase();
    if (!target.contains('\\windowsapps\\')) return;
    try {
      File(lnkPath).deleteSync();
    } catch (_) {
      return;
    }
  }

  final link = ShellLink.createInstance();
  final pidl = calloc<Pointer<ITEMIDLIST>>();
  final appsFolderItem = 'shell:AppsFolder\\$pfn!$_kAppId'.toNativeUtf16();
  final description = 'Bitpro Desk – free remote desktop'.toNativeUtf16();
  final file = lnkPath.toNativeUtf16();
  try {
    if (FAILED(_shParseDisplayName(appsFolderItem, nullptr, pidl, 0, nullptr))) {
      return;
    }
    link.setIDList(pidl.value);
    link.setDescription(description);
    final persist = IPersistFile.from(link);
    persist.save(file, TRUE);
  } finally {
    if (pidl.value != nullptr) CoTaskMemFree(pidl.value);
    free(pidl);
    free(appsFolderItem);
    free(description);
    free(file);
  }
}
