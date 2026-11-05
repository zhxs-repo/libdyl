#ifndef DYL_SURFACE_WINDOW_H_
#define DYL_SURFACE_WINDOW_H_

#include <windows.h>

#include <functional>
#include <memory>

namespace dyl_flutter {

/// A plain child window that the Rust renderer presents into via
/// `dyl_surface_attach` (`windows_hwnd`).
///
/// The window belongs to this plugin: Rust only borrows the handle while a
/// surface is attached. Callers must detach the surface before destroying it.
class DylSurfaceWindow {
 public:
  /// Fired from `WM_PAINT`. The swapchain owns every pixel, so a paint request
  /// means the presented frame may have been erased (the host repainted, the
  /// window was restored, ...). The owner asks the kernel to present again.
  using InvalidatedCallback = std::function<void()>;

  static std::unique_ptr<DylSurfaceWindow> Create(HWND parent,
                                                     int width,
                                                     int height);
  ~DylSurfaceWindow();

  DylSurfaceWindow(const DylSurfaceWindow&) = delete;
  DylSurfaceWindow& operator=(const DylSurfaceWindow&) = delete;

  HWND hwnd() const { return hwnd_; }

  /// Move/resize in physical pixels relative to the parent's client area.
  void SetFrame(int x, int y, int width, int height, bool visible);

  void GetSize(int* width, int* height) const;

  /// Must be set before the window is shown; the callback runs on the platform
  /// thread, inside the paint handler.
  void SetInvalidatedCallback(InvalidatedCallback callback);

 private:
  explicit DylSurfaceWindow(HWND hwnd) : hwnd_(hwnd) {}

  static void EnsureWindowClass();

  static LRESULT CALLBACK WindowProc(HWND hwnd,
                                     UINT message,
                                     WPARAM wparam,
                                     LPARAM lparam);

  HWND hwnd_ = nullptr;
  InvalidatedCallback invalidated_callback_;
};

}  // namespace dyl_flutter

#endif  // DYL_SURFACE_WINDOW_H_
