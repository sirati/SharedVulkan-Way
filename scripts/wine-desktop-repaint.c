/* Disposable Wine fixture: repaint its own desktop background while clipping
 * every application child. This never paints or copies application pixels. */
#include <windows.h>
#include <stdio.h>
int WINAPI WinMain(HINSTANCE instance,HINSTANCE previous,LPSTR command,int show) {
  (void)instance;(void)previous;(void)command;(void)show;
  HWND desktop=GetDesktopWindow();RECT r;DWORD pid=0;
  GetWindowRect(desktop,&r);GetWindowThreadProcessId(desktop,&pid);
  HDC dc=GetDCEx(desktop,NULL,DCX_CACHE|DCX_CLIPCHILDREN);
  BOOL painted=dc?PaintDesktop(dc):FALSE;
  if(dc)ReleaseDC(desktop,dc);
  printf("PaintDesktop clip-children=%d error=%lu\n",painted,(unsigned long)GetLastError());
  BOOL ok=RedrawWindow(desktop,NULL,NULL,RDW_INVALIDATE|RDW_ERASE|RDW_ALLCHILDREN|RDW_UPDATENOW);
  printf("desktop=%p pid=%lu rect=%ld,%ld %ldx%ld redraw=%d error=%lu\n",desktop,(unsigned long)pid,r.left,r.top,r.right-r.left,r.bottom-r.top,ok,(unsigned long)GetLastError());
  fflush(stdout);return painted&&ok?0:1;
}
