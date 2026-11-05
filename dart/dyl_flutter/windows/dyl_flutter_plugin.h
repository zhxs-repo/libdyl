#ifndef DYL_FLUTTER_PLUGIN_H_
#define DYL_FLUTTER_PLUGIN_H_

#include <flutter/encodable_value.h>
#include <flutter/method_channel.h>
#include <flutter/plugin_registrar_windows.h>

#include <cstdint>
#include <map>
#include <memory>

#include "dyl_surface_window.h"

namespace dyl_flutter {

/// MethodChannel `dev.dylplayer.dyl_flutter` host side on Windows.
///
/// The plugin owns one child HWND per Flutter surface request. Dart drives the
/// geometry in physical pixels and then hands the HWND string to the Rust
/// engine as a `windows_hwnd` surface descriptor. The kernel creates its own
/// Vulkan/GL swapchain on that HWND - no Flutter texture sharing.
class DylFlutterPlugin : public flutter::Plugin {
 public:
  static void RegisterWithRegistrar(flutter::PluginRegistrarWindows* registrar);

  explicit DylFlutterPlugin(flutter::PluginRegistrarWindows* registrar);
  ~DylFlutterPlugin() override;

  DylFlutterPlugin(const DylFlutterPlugin&) = delete;
  DylFlutterPlugin& operator=(const DylFlutterPlugin&) = delete;

  void HandleMethodCall(
      const flutter::MethodCall<flutter::EncodableValue>& method_call,
      std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result);

  /// Takes ownership of the channel and starts dispatching calls to
  /// [HandleMethodCall].
  void AttachChannel(
      std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>> channel);

 private:
  void CreateSurface(const flutter::EncodableMap& args,
                     std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>>
                         result);
  void ResizeSurface(const flutter::EncodableMap& args,
                     std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>>
                         result);
  void DestroySurface(const flutter::EncodableMap& args,
                      std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>>
                          result);
  void PickFile(std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>>
                    result);

  /// Tells Dart that the child window needs its frozen frame re-presented.
  void NotifySurfaceInvalidated(int64_t view_id);

  /// Parent for the surface HWNDs: the Flutter view's own window.
  HWND ParentWindow() const;

  flutter::PluginRegistrarWindows* registrar_;
  std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>> channel_;
  std::map<int64_t, std::unique_ptr<DylSurfaceWindow>> surfaces_;
};

}  // namespace dyl_flutter

#endif  // DYL_FLUTTER_PLUGIN_H_
