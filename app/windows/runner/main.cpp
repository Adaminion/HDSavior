#include <flutter/dart_project.h>
#include <flutter/flutter_view_controller.h>
#include <flutter_windows.h>
#include <windows.h>

#include <algorithm>

#include "flutter_window.h"
#include "utils.h"

namespace {

// Window sizes in logical pixels (scaled by the monitor's DPI).
constexpr unsigned int kDefaultWidth = 1280;
constexpr unsigned int kDefaultHeight = 800;
// Below this the results screen (folder tree, file table, details) no longer
// fits.
constexpr unsigned int kMinimumWidth = 960;
constexpr unsigned int kMinimumHeight = 600;

// Shrinks |size| to the work area of the primary monitor where necessary and
// returns the origin that centres a window of that size there. All values are
// logical pixels, as Win32Window::Create expects.
Win32Window::Point CenterOnPrimaryMonitor(Win32Window::Size* size) {
  RECT work_area{};
  if (!::SystemParametersInfo(SPI_GETWORKAREA, 0, &work_area, 0)) {
    return Win32Window::Point(10, 10);
  }
  const POINT corner = {work_area.left, work_area.top};
  HMONITOR monitor = ::MonitorFromPoint(corner, MONITOR_DEFAULTTOPRIMARY);
  const double scale = FlutterDesktopGetDpiForMonitor(monitor) / 96.0;

  // The work area of the primary monitor never has negative coordinates.
  const auto to_logical = [scale](LONG physical) {
    return static_cast<unsigned int>(std::max<LONG>(physical, 0) / scale);
  };
  const unsigned int work_left = to_logical(work_area.left);
  const unsigned int work_top = to_logical(work_area.top);
  const unsigned int work_width = to_logical(work_area.right - work_area.left);
  const unsigned int work_height =
      to_logical(work_area.bottom - work_area.top);

  size->width = std::min(size->width, work_width);
  size->height = std::min(size->height, work_height);
  return Win32Window::Point(work_left + (work_width - size->width) / 2,
                            work_top + (work_height - size->height) / 2);
}

}  // namespace

int APIENTRY wWinMain(_In_ HINSTANCE instance, _In_opt_ HINSTANCE prev,
                      _In_ wchar_t *command_line, _In_ int show_command) {
  // Attach to console when present (e.g., 'flutter run') or create a
  // new console when running with a debugger.
  if (!::AttachConsole(ATTACH_PARENT_PROCESS) && ::IsDebuggerPresent()) {
    CreateAndAttachConsole();
  }

  // Initialize COM, so that it is available for use in the library and/or
  // plugins.
  ::CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);

  flutter::DartProject project(L"data");

  std::vector<std::string> command_line_arguments =
      GetCommandLineArguments();

  project.set_dart_entrypoint_arguments(std::move(command_line_arguments));

  FlutterWindow window(project);
  Win32Window::Size size(kDefaultWidth, kDefaultHeight);
  Win32Window::Point origin = CenterOnPrimaryMonitor(&size);
  window.SetMinimumSize(Win32Window::Size(kMinimumWidth, kMinimumHeight));
  if (!window.Create(L"HDSavior", origin, size)) {
    return EXIT_FAILURE;
  }
  window.SetQuitOnClose(true);

  ::MSG msg;
  while (::GetMessage(&msg, nullptr, 0, 0)) {
    ::TranslateMessage(&msg);
    ::DispatchMessage(&msg);
  }

  ::CoUninitialize();
  return EXIT_SUCCESS;
}
