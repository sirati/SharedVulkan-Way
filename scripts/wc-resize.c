/* Test tool (§102): resize or maximise waycompose's toplevel windows, as a
 * person dragging a border or double-clicking the caption would. Wine without
 * a window manager gives an X resize no WM_SIZE, so the VM test asks Win32.
 *   wc-resize.exe max        maximise every visible WaycomposeToplevel
 *   wc-resize.exe W H        set their client area to W x H              */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
static int argc_; static char **argv_; static int n;
static BOOL CALLBACK each(HWND h, LPARAM l) {
  char cls[64];
  (void)l;
  if (!IsWindowVisible(h) || !GetClassNameA(h, cls, sizeof cls) || strcmp(cls, "WaycomposeToplevel")) return TRUE;
  if (argc_ == 2 && !strcmp(argv_[1], "max")) {
    ShowWindow(h, SW_MAXIMIZE);
  } else if (argc_ == 3) {
    RECT r = {0, 0, atoi(argv_[1]), atoi(argv_[2])};
    if (IsZoomed(h)) ShowWindow(h, SW_RESTORE);
    AdjustWindowRectEx(&r, (DWORD)GetWindowLongPtrA(h, GWL_STYLE), FALSE, (DWORD)GetWindowLongPtrA(h, GWL_EXSTYLE));
    SetWindowPos(h, NULL, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
  }
  n++;
  return TRUE;
}
int main(int argc, char **argv) {
  argc_ = argc; argv_ = argv;
  EnumWindows(each, 0);
  printf("wc-resize: %d window(s)\n", n);
  return n ? 0 : 1;
}
