#include "dyl_surface_window.h"

#include <algorithm>
#include <mutex>

namespace dyl_flutter {
namespace {

constexpr wchar_t kSurfaceWindowClass[] = L"DylFlutterSurfaceWindow";

std::once_flag g_class_once;

}  // namespace

void DylSurfaceWindow::EnsureWindowClass() {
  std::call_once(g_class_once, [] {
    WNDCLASSEXW window_class = {};
    window_class.cbSize = sizeof(WNDCLASSEXW);
    window_class.style = CS_HREDRAW | CS_VREDRAW;
    window_class.lpfnWndProc = DylSurfaceWindow::WindowProc;
    window_class.hInstance = GetModuleHandleW(nullptr);
    window_class.hCursor = LoadCursorW(nullptr, IDC_ARROW);
    // No background brush: the swapchain paints every pixel.
    window_class.hbrBackground = nullptr;
    window_class.lpszClassName = kSurfaceWindowClass;
    RegisterClassExW(&window_class);
  });
}

std::unique_ptr<DylSurfaceWindow> DylSurfaceWindow::Create(HWND parent,
                                                                int width,
                                                                int height) {
  if (parent == nullptr) {
    return nullptr;
  }
  DylSurfaceWindow::EnsureWindowClass();

  // The Flutter view hosts an ANGLE swapchain that covers its whole client
  // area and is not clipped to child windows unless WS_CLIPCHILDREN is set.
  // Without this every Flutter frame paints over the video surface: it flickers
  // black during playback (until the next kernel present) and stays black after
  // pause/EOF, when the kernel stops presenting.
  const LONG_PTR parent_style = GetWindowLongPtrW(parent, GWL_STYLE);
  if ((parent_style & WS_CLIPCHILDREN) == 0) {
    SetWindowLongPtrW(parent, GWL_STYLE, parent_style | WS_CLIPCHILDREN);
  }

  HWND hwnd = CreateWindowExW(
      0, kSurfaceWindowClass, L"Dyl video", WS_CHILD | WS_CLIPSIBLINGS,
      0, 0, std::max(width, 1), std::max(height, 1), parent, nullptr,
      GetModuleHandleW(nullptr), nullptr);
  if (hwnd == nullptr) {
    return nullptr;
  }
  auto window = std::unique_ptr<DylSurfaceWindow>(new DylSurfaceWindow(hwnd));
  SetWindowLongPtrW(hwnd, GWLP_USERDATA,
                    reinterpret_cast<LONG_PTR>(window.get()));
  return window;
}

DylSurfaceWindow::~DylSurfaceWindow() {
  if (hwnd_ != nullptr) {
    SetWindowLongPtrW(hwnd_, GWLP_USERDATA, 0);
    DestroyWindow(hwnd_);
    hwnd_ = nullptr;
  }
}

// GDI erasing the window wipes whatever the swapchain presented. Nothing is
// drawn from GDI here: suppress the erase, validate the paint request, and tell
// the owner so the kernel can present the frozen frame again.
LRESULT CALLBACK DylSurfaceWindow::WindowProc(HWND hwnd,
                                                 UINT message,
                                                 WPARAM wparam,
                                                 LPARAM lparam) {
  auto* self = reinterpret_cast<DylSurfaceWindow*>(
      GetWindowLongPtrW(hwnd, GWLP_USERDATA));
  switch (message) {
    case WM_ERASEBKGND:
      return 1;
    // This window only displays what the kernel presents; it has no input
    // handling. Answering the hit test as transparent hands the mouse message
    // to the Flutter view underneath, so a click on the video neither gets
    // swallowed nor moves keyboard focus away from Flutter. Full-window mode
    // hides every Flutter control behind this surface, and its only exit is a
    // keyboard shortcut, so losing focus here would trap the user in it.
    case WM_NCHITTEST:
      return HTTRANSPARENT;
    case WM_PAINT: {
      PAINTSTRUCT paint = {};
      BeginPaint(hwnd, &paint);
      EndPaint(hwnd, &paint);
      if (self != nullptr && self->invalidated_callback_) {
        self->invalidated_callback_();
      }
      return 0;
    }
    default:
      return DefWindowProcW(hwnd, message, wparam, lparam);
  }
}

void DylSurfaceWindow::SetInvalidatedCallback(InvalidatedCallback callback) {
  invalidated_callback_ = std::move(callback);
}

void DylSurfaceWindow::SetFrame(int x,
                                   int y,
                                   int width,
                                   int height,
                                   bool visible) {
  if (hwnd_ == nullptr) {
    return;
  }
  UINT flags = SWP_NOACTIVATE | SWP_NOZORDER;
  flags |= visible ? SWP_SHOWWINDOW : SWP_HIDEWINDOW;
  SetWindowPos(hwnd_, nullptr, x, y, std::max(width, 1),
               std::max(height, 1), flags);
}

void DylSurfaceWindow::GetSize(int* width, int* height) const {
  RECT rect = {};
  if (hwnd_ != nullptr) {
    GetClientRect(hwnd_, &rect);
  }
  if (width != nullptr) {
    *width = rect.right - rect.left;
  }
  if (height != nullptr) {
    *height = rect.bottom - rect.top;
  }
}

}  // namespace dyl_flutter
