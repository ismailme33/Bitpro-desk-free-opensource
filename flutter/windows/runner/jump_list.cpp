#include "jump_list.h"

#include <windows.h>
#include <propkey.h>
#include <propvarutil.h>
#include <shobjidl.h>
#include <wrl/client.h>

#include <flutter/method_channel.h>
#include <flutter/standard_method_codec.h>

#include <memory>
#include <set>
#include <string>
#include <vector>

using Microsoft::WRL::ComPtr;

namespace {

struct JumpItem {
  std::wstring id;
  std::wstring title;
};

std::wstring Utf8ToWide(const std::string& s) {
  if (s.empty()) return L"";
  int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), (int)s.size(), nullptr, 0);
  std::wstring w(n, L'\0');
  MultiByteToWideChar(CP_UTF8, 0, s.data(), (int)s.size(), w.data(), n);
  return w;
}

// Peer IDs are plain identifiers; drop anything else so an ID can never
// inject extra command line arguments.
std::wstring SanitizeId(const std::wstring& id) {
  std::wstring out;
  for (wchar_t c : id) {
    if (iswalnum(c) || c == L'-' || c == L'_' || c == L'.') out.push_back(c);
  }
  return out;
}

std::wstring ExePath() {
  wchar_t buf[MAX_PATH];
  DWORD n = GetModuleFileNameW(nullptr, buf, MAX_PATH);
  return std::wstring(buf, n);
}

// URL scheme of this build (e.g. "bitprodesk://"), provided by Dart.
std::wstring g_uri_prefix = L"bitprodesk://";

// Accepts only "<letters/digits>://".
bool IsValidUriPrefix(const std::wstring& prefix) {
  const size_t sep = prefix.find(L"://");
  if (sep == std::wstring::npos || sep == 0 || sep + 3 != prefix.size()) {
    return false;
  }
  for (size_t i = 0; i < sep; i++) {
    if (!iswalnum(prefix[i])) return false;
  }
  return true;
}

// The running instance forwards this link to the main window (see main.cpp),
// and a cold start handles it as the initial link.
std::wstring ArgsFor(const std::wstring& id) {
  return g_uri_prefix + L"connect/" + id;
}

HRESULT MakeLink(const std::wstring& exe, const JumpItem& item,
                 ComPtr<IShellLinkW>& out) {
  ComPtr<IShellLinkW> link;
  HRESULT hr = CoCreateInstance(CLSID_ShellLink, nullptr, CLSCTX_INPROC_SERVER,
                                IID_PPV_ARGS(&link));
  if (FAILED(hr)) return hr;
  link->SetPath(exe.c_str());
  link->SetArguments(ArgsFor(item.id).c_str());
  link->SetIconLocation(exe.c_str(), 0);
  link->SetDescription(item.title.c_str());

  ComPtr<IPropertyStore> store;
  hr = link.As(&store);
  if (FAILED(hr)) return hr;
  PROPVARIANT pv;
  hr = InitPropVariantFromString(item.title.c_str(), &pv);
  if (FAILED(hr)) return hr;
  hr = store->SetValue(PKEY_Title, pv);
  PropVariantClear(&pv);
  if (SUCCEEDED(hr)) hr = store->Commit();
  if (SUCCEEDED(hr)) out = link;
  return hr;
}

// Items the user removed from the Jump List must not be re-added, otherwise
// AppendCategory fails for the whole category.
std::set<std::wstring> RemovedArgs(IObjectArray* removed) {
  std::set<std::wstring> result;
  UINT count = 0;
  if (!removed || FAILED(removed->GetCount(&count))) return result;
  for (UINT i = 0; i < count; i++) {
    ComPtr<IShellLinkW> link;
    if (SUCCEEDED(removed->GetAt(i, IID_PPV_ARGS(&link)))) {
      wchar_t args[1024] = {0};
      if (SUCCEEDED(link->GetArguments(args, ARRAYSIZE(args)))) {
        result.insert(args);
      }
    }
  }
  return result;
}

void AppendCategory(ICustomDestinationList* list, const std::wstring& name,
                    const std::vector<JumpItem>& items, const std::wstring& exe,
                    const std::set<std::wstring>& removed) {
  ComPtr<IObjectCollection> collection;
  if (FAILED(CoCreateInstance(CLSID_EnumerableObjectCollection, nullptr,
                              CLSCTX_INPROC_SERVER,
                              IID_PPV_ARGS(&collection)))) {
    return;
  }
  UINT added = 0;
  for (const auto& item : items) {
    if (removed.count(ArgsFor(item.id))) continue;
    ComPtr<IShellLinkW> link;
    if (SUCCEEDED(MakeLink(exe, item, link)) &&
        SUCCEEDED(collection->AddObject(link.Get()))) {
      added++;
    }
  }
  if (added == 0) return;
  ComPtr<IObjectArray> array;
  if (SUCCEEDED(collection.As(&array))) {
    list->AppendCategory(name.c_str(), array.Get());
  }
}

void UpdateJumpList(const std::vector<JumpItem>& pinned,
                    const std::vector<JumpItem>& recent,
                    const std::wstring& pinned_label,
                    const std::wstring& recent_label) {
  ComPtr<ICustomDestinationList> list;
  if (FAILED(CoCreateInstance(CLSID_DestinationList, nullptr,
                              CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&list)))) {
    return;
  }
  UINT min_slots = 0;
  ComPtr<IObjectArray> removed;
  if (FAILED(list->BeginList(&min_slots, IID_PPV_ARGS(&removed)))) return;
  const auto removed_args = RemovedArgs(removed.Get());
  const auto exe = ExePath();
  AppendCategory(list.Get(), pinned_label, pinned, exe, removed_args);
  AppendCategory(list.Get(), recent_label, recent, exe, removed_args);
  list->CommitList();
}

const std::string* GetString(const flutter::EncodableMap& map,
                             const char* key) {
  auto it = map.find(flutter::EncodableValue(key));
  if (it == map.end()) return nullptr;
  return std::get_if<std::string>(&it->second);
}

std::wstring GetLabel(const flutter::EncodableMap& map, const char* key,
                      const wchar_t* fallback) {
  const auto* s = GetString(map, key);
  return (s && !s->empty()) ? Utf8ToWide(*s) : std::wstring(fallback);
}

std::vector<JumpItem> ParseItems(const flutter::EncodableMap& args,
                                 const char* key) {
  std::vector<JumpItem> out;
  auto it = args.find(flutter::EncodableValue(key));
  if (it == args.end()) return out;
  const auto* list = std::get_if<flutter::EncodableList>(&it->second);
  if (!list) return out;
  for (const auto& value : *list) {
    const auto* map = std::get_if<flutter::EncodableMap>(&value);
    if (!map) continue;
    JumpItem item;
    if (const auto* id = GetString(*map, "id")) item.id = SanitizeId(Utf8ToWide(*id));
    if (item.id.empty()) continue;
    if (const auto* title = GetString(*map, "title")) item.title = Utf8ToWide(*title);
    if (item.title.empty()) item.title = item.id;
    out.push_back(item);
  }
  return out;
}

}  // namespace

void RegisterJumpListChannel(flutter::BinaryMessenger* messenger) {
  static std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>>
      channel;
  channel = std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
      messenger, "bitpro/jump_list",
      &flutter::StandardMethodCodec::GetInstance());
  channel->SetMethodCallHandler(
      [](const flutter::MethodCall<flutter::EncodableValue>& call,
         std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>>
             result) {
        if (call.method_name() != "update") {
          result->NotImplemented();
          return;
        }
        const auto* args = std::get_if<flutter::EncodableMap>(call.arguments());
        if (!args) {
          result->Error("bad_args", "Expected a map");
          return;
        }
        const std::wstring prefix = GetLabel(*args, "uriPrefix", L"");
        if (IsValidUriPrefix(prefix)) g_uri_prefix = prefix;
        UpdateJumpList(ParseItems(*args, "pinned"), ParseItems(*args, "recent"),
                       GetLabel(*args, "pinnedLabel", L"Pinned"),
                       GetLabel(*args, "recentLabel", L"Recent"));
        result->Success();
      });
}
