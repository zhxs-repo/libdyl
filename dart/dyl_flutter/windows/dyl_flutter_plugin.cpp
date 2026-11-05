#include "dyl_flutter_plugin.h"

// windows.h must come before most other Windows headers.
#include <windows.h>

// shobjidl.h provides IFileOpenDialog / IShellItem.
#include <commdlg.h>
#include <shobjidl.h>

#include <flutter/plugin_registrar_windows.h>
#include <flutter/standard_method_codec.h>

#include <cstdint>
#include <cstdio>
#include <filesystem>
#include <iterator>
#include <memory>
#include <string>
#include <utility>

#include "include/dyl_flutter/dyl_flutter_plugin.h"

namespace dyl_flutter {
namespace {

constexpr char kChannelName[] = "dev.dylplayer.dyl_flutter";

std::string WideToUtf8(const std::wstring& text) {
  if (text.empty()) {
    return std::string();
  }
  const int size =
      WideCharToMultiByte(CP_UTF8, 0, text.data(), static_cast<int>(text.size()),
                          nullptr, 0, nullptr, nullptr);
  if (size <= 0) {
    return std::string();
  }
  std::string out(static_cast<size_t>(size), '\0');
  WideCharToMultiByte(CP_UTF8, 0, text.data(), static_cast<int>(text.size()),
                      out.data(), size, nullptr, nullptr);
  return out;
}

/// `windows_hwnd` handles are strings in the contract to avoid 64-bit
/// precision loss through JSON.
std::string WindowHandleString(HWND hwnd) {
  char buffer[32] = {};
  std::snprintf(buffer, sizeof(buffer), "0x%llx",
                static_cast<unsigned long long>(
                    reinterpret_cast<uintptr_t>(hwnd)));
  return std::string(buffer);
}

const flutter::EncodableValue* FindArg(const flutter::EncodableMap& args,
                                      const char* key) {
  auto it = args.find(flutter::EncodableValue(std::string(key)));
  return it == args.end() ? nullptr : &it->second;
}

int64_t IntArg(const flutter::EncodableMap& args,
               const char* key,
               int64_t fallback) {
  const flutter::EncodableValue* value = FindArg(args, key);
  if (value == nullptr) {
    return fallback;
  }
  if (const int32_t* number = std::get_if<int32_t>(value)) {
    return *number;
  }
  if (const int64_t* number = std::get_if<int64_t>(value)) {
    return *number;
  }
  if (const double* number = std::get_if<double>(value)) {
    return static_cast<int64_t>(*number);
  }
  return fallback;
}

bool BoolArg(const flutter::EncodableMap& args,
             const char* key,
             bool fallback) {
  const flutter::EncodableValue* value = FindArg(args, key);
  if (value == nullptr) {
    return fallback;
  }
  if (const bool* flag = std::get_if<bool>(value)) {
    return *flag;
  }
  return fallback;
}

/// Resolves a system path and its byte size. Returns false on failure.
bool QueryFile(const std::wstring& path, std::wstring* name, int64_t* size) {
  WIN32_FILE_ATTRIBUTE_DATA data = {};
  if (!GetFileAttributesExW(path.c_str(), GetFileExInfoStandard, &data)) {
    return false;
  }
  if ((data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0) {
    return false;
  }
  const uint64_t bytes = (static_cast<uint64_t>(data.nFileSizeHigh) << 32) |
                         static_cast<uint64_t>(data.nFileSizeLow);
  *size = static_cast<int64_t>(bytes);
  *name = std::filesystem::path(path).filename().wstring();
  return true;
}

/// Shows the shell open dialog. Returns false when cancelled or on error.
bool ShowOpenDialog(HWND owner, std::wstring* path) {
  IFileOpenDialog* dialog = nullptr;
  HRESULT hr = CoCreateInstance(CLSID_FileOpenDialog, nullptr,
                                CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&dialog));
  if (FAILED(hr) || dialog == nullptr) {
    return false;
  }

  DWORD options = 0;
  if (SUCCEEDED(dialog->GetOptions(&options))) {
    dialog->SetOptions(options | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST |
                       FOS_PATHMUSTEXIST);
  }

  const COMDLG_FILTERSPEC filters[] = {
      {L"Media files",
       L"*.mp4;*.mkv;*.mov;*.avi;*.webm;*.m4v;*.ts;*.flv;*.ass;*.ssa;*.srt;*.vtt;"
       L"*.sub;*.xml;*.json"},
      {L"All files", L"*.*"},
  };
  dialog->SetFileTypes(static_cast<UINT>(std::size(filters)), filters);
  dialog->SetFileTypeIndex(1);
  dialog->SetTitle(L"Open media file");

  bool picked = false;
  hr = dialog->Show(owner);
  if (SUCCEEDED(hr)) {
    IShellItem* item = nullptr;
    if (SUCCEEDED(dialog->GetResult(&item)) && item != nullptr) {
      PWSTR raw_path = nullptr;
      if (SUCCEEDED(item->GetDisplayName(SIGDN_FILESYSPATH, &raw_path)) &&
          raw_path != nullptr) {
        *path = raw_path;
        CoTaskMemFree(raw_path);
        picked = true;
      }
      item->Release();
    }
  }
  dialog->Release();
  return picked;
}

}  // namespace

// static
void DylFlutterPlugin::RegisterWithRegistrar(
    flutter::PluginRegistrarWindows* registrar) {
  auto plugin = std::make_unique<DylFlutterPlugin>(registrar);
  plugin->AttachChannel(
      std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
          registrar->messenger(), kChannelName,
          &flutter::StandardMethodCodec::GetInstance()));
  registrar->AddPlugin(std::move(plugin));
}

DylFlutterPlugin::DylFlutterPlugin(
    flutter::PluginRegistrarWindows* registrar)
    : registrar_(registrar) {}

DylFlutterPlugin::~DylFlutterPlugin() {
  for (auto& entry : surfaces_) {
    entry.second->SetInvalidatedCallback(nullptr);
  }
}

void DylFlutterPlugin::AttachChannel(
    std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>> channel) {
  channel_ = std::move(channel);
  channel_->SetMethodCallHandler(
      [this](const auto& call, auto result) {
        HandleMethodCall(call, std::move(result));
      });
}

void DylFlutterPlugin::HandleMethodCall(
    const flutter::MethodCall<flutter::EncodableValue>& method_call,
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  const flutter::EncodableMap* args =
      std::get_if<flutter::EncodableMap>(method_call.arguments());
  const std::string& method = method_call.method_name();

  if (method == "createSurface") {
    if (args == nullptr) {
      result->Error("invalid_argument", "createSurface requires arguments");
      return;
    }
    CreateSurface(*args, std::move(result));
  } else if (method == "resizeSurface") {
    if (args == nullptr) {
      result->Error("invalid_argument", "resizeSurface requires arguments");
      return;
    }
    ResizeSurface(*args, std::move(result));
  } else if (method == "destroySurface") {
    if (args == nullptr) {
      result->Error("invalid_argument", "destroySurface requires arguments");
      return;
    }
    DestroySurface(*args, std::move(result));
  } else if (method == "pickFile") {
    PickFile(std::move(result));
  } else {
    result->NotImplemented();
  }
}

HWND DylFlutterPlugin::ParentWindow() const {
  if (registrar_ == nullptr) {
    return nullptr;
  }
  flutter::FlutterView* view = registrar_->GetView();
  if (view == nullptr) {
    return nullptr;
  }
  return view->GetNativeWindow();
}

void DylFlutterPlugin::CreateSurface(
    const flutter::EncodableMap& args,
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  const int64_t view_id = IntArg(args, "viewId", 0);
  const int width = static_cast<int>(IntArg(args, "width", 0));
  const int height = static_cast<int>(IntArg(args, "height", 0));
  if (view_id == 0 || width <= 0 || height <= 0) {
    result->Error("invalid_argument",
                  "createSurface needs viewId, width and height > 0");
    return;
  }

  auto existing = surfaces_.find(view_id);
  if (existing != surfaces_.end()) {
    result->Success(flutter::EncodableValue(flutter::EncodableMap{
        {flutter::EncodableValue("handle"),
         flutter::EncodableValue(
             WindowHandleString(existing->second->hwnd()))},
        {flutter::EncodableValue("width"), flutter::EncodableValue(width)},
        {flutter::EncodableValue("height"), flutter::EncodableValue(height)},
    }));
    return;
  }

  HWND parent = ParentWindow();
  if (parent == nullptr) {
    result->Error("surface", "Flutter view has no native window");
    return;
  }

  auto surface = DylSurfaceWindow::Create(parent, width, height);
  if (!surface) {
    result->Error("surface", "failed to create the native surface window");
    return;
  }
  // Hidden until Dart attaches it to the engine; showing an unattached window
  // would flash the desktop behind it.
  surface->SetFrame(0, 0, width, height, false);
  surface->SetInvalidatedCallback(
      [this, surface_view_id = view_id]() { NotifySurfaceInvalidated(surface_view_id); });

  const std::string handle = WindowHandleString(surface->hwnd());
  surfaces_.emplace(view_id, std::move(surface));

  result->Success(flutter::EncodableValue(flutter::EncodableMap{
      {flutter::EncodableValue("handle"), flutter::EncodableValue(handle)},
      {flutter::EncodableValue("width"), flutter::EncodableValue(width)},
      {flutter::EncodableValue("height"), flutter::EncodableValue(height)},
  }));
}

void DylFlutterPlugin::ResizeSurface(
    const flutter::EncodableMap& args,
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  const int64_t view_id = IntArg(args, "viewId", 0);
  auto it = surfaces_.find(view_id);
  if (it == surfaces_.end()) {
    result->Error("invalid_state", "unknown view id");
    return;
  }

  const int x = static_cast<int>(IntArg(args, "x", 0));
  const int y = static_cast<int>(IntArg(args, "y", 0));
  const int width = static_cast<int>(IntArg(args, "width", 1));
  const int height = static_cast<int>(IntArg(args, "height", 1));
  const bool visible = BoolArg(args, "visible", true);

  it->second->SetFrame(x, y, width, height, visible);

  int actual_width = 0;
  int actual_height = 0;
  it->second->GetSize(&actual_width, &actual_height);
  result->Success(flutter::EncodableValue(flutter::EncodableMap{
      {flutter::EncodableValue("width"),
       flutter::EncodableValue(actual_width)},
      {flutter::EncodableValue("height"),
       flutter::EncodableValue(actual_height)},
  }));
}

void DylFlutterPlugin::DestroySurface(
    const flutter::EncodableMap& args,
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  const int64_t view_id = IntArg(args, "viewId", 0);
  surfaces_.erase(view_id);
  result->Success();
}

void DylFlutterPlugin::NotifySurfaceInvalidated(int64_t view_id) {
  if (!channel_) {
    return;
  }
  channel_->InvokeMethod(
      "onSurfaceInvalidated",
      std::make_unique<flutter::EncodableValue>(flutter::EncodableValue(
          flutter::EncodableMap{{flutter::EncodableValue("viewId"),
                                 flutter::EncodableValue(view_id)}})));
}

void DylFlutterPlugin::PickFile(
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  HWND owner = ParentWindow();
  if (owner != nullptr) {
    owner = GetAncestor(owner, GA_ROOT);
  }

  std::wstring path;
  if (!ShowOpenDialog(owner, &path)) {
    result->Success();
    return;
  }

  std::wstring name;
  int64_t size = 0;
  if (!QueryFile(path, &name, &size)) {
    result->Error("io", "cannot read the selected file");
    return;
  }

  result->Success(flutter::EncodableValue(flutter::EncodableMap{
      {flutter::EncodableValue("path"),
       flutter::EncodableValue(WideToUtf8(path))},
      {flutter::EncodableValue("name"),
       flutter::EncodableValue(WideToUtf8(name))},
      {flutter::EncodableValue("size"), flutter::EncodableValue(size)},
  }));
}

}  // namespace dyl_flutter

void DylFlutterPluginRegisterWithRegistrar(
    FlutterDesktopPluginRegistrarRef registrar) {
  dyl_flutter::DylFlutterPlugin::RegisterWithRegistrar(
      flutter::PluginRegistrarManager::GetInstance()
          ->GetRegistrar<flutter::PluginRegistrarWindows>(registrar));
}
