/* probe-drag.c -- a Windows application on the other end of a drag.
 *
 * WHY THIS EXISTS
 *
 * docs/waycompose-design.md §43.9 item 1 says, in as many words, that no drag
 * has ever entered: `IDropTarget::DragEnter` had never run outside a unit test,
 * because entering one needs "a desktop with a Windows application on it and a
 * mouse button held across two windows". The compositor cannot supply the
 * second application, and it must not be the one that says a drag happened —
 * that is the code under test. So this is the second application, and it is a
 * **separate process** that shares no code with the compositor, in the same
 * spirit as scripts/probe-clip.c for the clipboard (§29.7's table) and
 * scripts/probe-window.c for windows.
 *
 * It is both halves at once, because the requirement is both directions:
 *
 *   probe-drag.exe --mode target   register an IDropTarget and log what a drag
 *                                  entering this window offers and drops.
 *                                  This is the Windows end of "a Wayland client
 *                                  dragged out".
 *   probe-drag.exe --mode source   start a DoDragDrop from a left-button press
 *                                  in this window, offering CF_UNICODETEXT.
 *                                  This is the Windows end of "something was
 *                                  dragged into a Wayland client".
 *   probe-drag.exe --mode both     both, which is what the harness runs.
 *
 * Everything it observes goes to --log with a millisecond timestamp, because
 * the one measurement §43.9 item 2 asks for is an *interval*: how much of a
 * gesture passes between the drag entering and the Wayland client's `accept`
 * coming back across the boundary.
 *
 * WHY THE IDataObject IS HAND-ROLLED
 *
 * SHCreateDataObject and OleGetClipboard would both do, and both were refused.
 * The second puts the text on the system clipboard, and the compositor under
 * test holds a clipboard listener (§29.5's deliberate echo) — so the *source*
 * side of this probe would be poking the thing the other half of the feature
 * reads, and a drag that appeared to work could be a clipboard that did. The
 * first is a shell entry point whose absence from an import library would show
 * up as a link error at the worst moment. A hand-rolled IDataObject is ~150
 * lines of entirely mechanical C, it depends on nothing but ole32, and it is
 * the same shape the thing under test implements — which is the point.
 *
 * Usage:
 *   probe-drag.exe [--mode target|source|both] [--title T] [--log PATH]
 *                  [--text TEXT] [--x N --y N --w N --h N] [--seconds N]
 */

#define COBJMACROS
#define WINVER 0x0601
#define _WIN32_WINNT 0x0601

#include <windows.h>
#include <ole2.h>
#include <objidl.h>
#include <shlobj.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>

/* ------------------------------------------------------------------ log --- */

static FILE *g_log;
static DWORD g_t0;

static void logf_(const char *fmt, ...) {
    va_list ap;
    char line[2048];
    int n;
    n = snprintf(line, sizeof line, "[%6lu ms] ", (unsigned long)(GetTickCount() - g_t0));
    va_start(ap, fmt);
    vsnprintf(line + n, sizeof line - n, fmt, ap);
    va_end(ap);
    if (g_log) {
        fputs(line, g_log);
        fputc('\n', g_log);
        fflush(g_log);
    }
    fputs(line, stdout);
    fputc('\n', stdout);
    fflush(stdout);
}

/* A registered format's name, so the log says "HTML Format" and not "49332".
 * The system formats have no name and are spelled out by hand. */
static const char *format_name(CLIPFORMAT cf, char *buf, size_t n) {
    switch (cf) {
    case CF_TEXT:        return "CF_TEXT";
    case CF_UNICODETEXT: return "CF_UNICODETEXT";
    case CF_OEMTEXT:     return "CF_OEMTEXT";
    case CF_HDROP:       return "CF_HDROP";
    case CF_BITMAP:      return "CF_BITMAP";
    case CF_DIB:         return "CF_DIB";
    default: break;
    }
    if (GetClipboardFormatNameA(cf, buf, (int)n) > 0)
        return buf;
    snprintf(buf, n, "#%u", (unsigned)cf);
    return buf;
}

/* ------------------------------------------------------- IEnumFORMATETC --- */

typedef struct {
    IEnumFORMATETC iface;
    LONG ref;
    FORMATETC *fmts;
    ULONG count;
    ULONG at;
} Enum;

static Enum *enum_new(const FORMATETC *fmts, ULONG count, ULONG at);

static HRESULT STDMETHODCALLTYPE enum_qi(IEnumFORMATETC *me, REFIID iid, void **out) {
    if (!out) return E_INVALIDARG;
    *out = NULL;                             /* even on refusal: OLE reads it either way */
    if (IsEqualIID(iid, &IID_IUnknown) || IsEqualIID(iid, &IID_IEnumFORMATETC)) {
        *out = me;
        IEnumFORMATETC_AddRef(me);
        return S_OK;
    }
    return E_NOINTERFACE;
}
static ULONG STDMETHODCALLTYPE enum_addref(IEnumFORMATETC *me) {
    return InterlockedIncrement(&((Enum *)me)->ref);
}
static ULONG STDMETHODCALLTYPE enum_release(IEnumFORMATETC *me) {
    Enum *e = (Enum *)me;
    LONG n = InterlockedDecrement(&e->ref);
    if (n == 0) { free(e->fmts); free(e); }
    return (ULONG)n;
}
static HRESULT STDMETHODCALLTYPE enum_next(IEnumFORMATETC *me, ULONG celt,
                                           FORMATETC *out, ULONG *fetched) {
    Enum *e = (Enum *)me;
    ULONG i = 0;
    if (!out || (celt != 1 && !fetched)) return E_INVALIDARG;
    while (i < celt && e->at < e->count)
        out[i++] = e->fmts[e->at++];
    if (fetched) *fetched = i;
    return i == celt ? S_OK : S_FALSE;       /* S_FALSE when short, including zero */
}
static HRESULT STDMETHODCALLTYPE enum_skip(IEnumFORMATETC *me, ULONG celt) {
    Enum *e = (Enum *)me;
    ULONG can = e->count - e->at;
    e->at += celt < can ? celt : can;
    return celt <= can ? S_OK : S_FALSE;
}
static HRESULT STDMETHODCALLTYPE enum_reset(IEnumFORMATETC *me) {
    ((Enum *)me)->at = 0;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE enum_clone(IEnumFORMATETC *me, IEnumFORMATETC **out) {
    Enum *e = (Enum *)me;
    if (!out) return E_INVALIDARG;
    *out = NULL;
    { Enum *c = enum_new(e->fmts, e->count, e->at);
      if (!c) return E_OUTOFMEMORY;
      *out = &c->iface; }
    return S_OK;
}

static IEnumFORMATETCVtbl enum_vtbl = {
    enum_qi, enum_addref, enum_release, enum_next, enum_skip, enum_reset, enum_clone
};

static Enum *enum_new(const FORMATETC *fmts, ULONG count, ULONG at) {
    Enum *e = calloc(1, sizeof *e);
    if (!e) return NULL;
    e->iface.lpVtbl = &enum_vtbl;
    e->ref = 1;
    e->count = count;
    e->at = at;
    e->fmts = calloc(count ? count : 1, sizeof(FORMATETC));
    if (!e->fmts) { free(e); return NULL; }
    memcpy(e->fmts, fmts, count * sizeof(FORMATETC));
    return e;
}

/* ----------------------------------------------------------- IDataObject --- */

typedef struct {
    IDataObject iface;
    LONG ref;
    WCHAR *text;                             /* CF_UNICODETEXT, NUL-terminated */
} Data;

static HRESULT STDMETHODCALLTYPE data_qi(IDataObject *me, REFIID iid, void **out) {
    if (!out) return E_INVALIDARG;
    *out = NULL;
    if (IsEqualIID(iid, &IID_IUnknown) || IsEqualIID(iid, &IID_IDataObject)) {
        *out = me;
        IDataObject_AddRef(me);
        return S_OK;
    }
    return E_NOINTERFACE;
}
static ULONG STDMETHODCALLTYPE data_addref(IDataObject *me) {
    return InterlockedIncrement(&((Data *)me)->ref);
}
static ULONG STDMETHODCALLTYPE data_release(IDataObject *me) {
    Data *d = (Data *)me;
    LONG n = InterlockedDecrement(&d->ref);
    if (n == 0) { free(d->text); free(d); }
    return (ULONG)n;
}
static HRESULT STDMETHODCALLTYPE data_getdata(IDataObject *me, FORMATETC *fmt, STGMEDIUM *med) {
    Data *d = (Data *)me;
    size_t bytes;
    HGLOBAL h;
    void *p;
    if (!fmt || !med) return E_INVALIDARG;
    if (fmt->cfFormat != CF_UNICODETEXT || !(fmt->tymed & TYMED_HGLOBAL))
        return DV_E_FORMATETC;
    bytes = (wcslen(d->text) + 1) * sizeof(WCHAR);
    /* A fresh HGLOBAL every time, and pUnkForRelease NULL: the *receiver* frees
     * it, so handing back the same handle twice would be a double free. §29.8
     * lists this as one of the four silently-wrong things. */
    h = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if (!h) return E_OUTOFMEMORY;
    p = GlobalLock(h);
    memcpy(p, d->text, bytes);
    GlobalUnlock(h);
    med->tymed = TYMED_HGLOBAL;
    med->hGlobal = h;
    med->pUnkForRelease = NULL;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE data_getdatahere(IDataObject *me, FORMATETC *f, STGMEDIUM *m) {
    (void)me; (void)f; (void)m;
    return E_NOTIMPL;
}
static HRESULT STDMETHODCALLTYPE data_query(IDataObject *me, FORMATETC *fmt) {
    (void)me;
    if (!fmt) return E_INVALIDARG;
    return (fmt->cfFormat == CF_UNICODETEXT && (fmt->tymed & TYMED_HGLOBAL))
        ? S_OK : DV_E_FORMATETC;
}
static HRESULT STDMETHODCALLTYPE data_canon(IDataObject *me, FORMATETC *in, FORMATETC *out) {
    (void)me;
    if (!out) return E_INVALIDARG;
    if (in) *out = *in;
    if (out) out->ptd = NULL;
    return DATA_S_SAMEFORMATETC;
}
static HRESULT STDMETHODCALLTYPE data_setdata(IDataObject *me, FORMATETC *f, STGMEDIUM *m, BOOL own) {
    (void)me; (void)f; (void)m; (void)own;
    return E_NOTIMPL;
}
static HRESULT STDMETHODCALLTYPE data_enum(IDataObject *me, DWORD dir, IEnumFORMATETC **out) {
    FORMATETC f;
    Enum *e;
    (void)me;
    if (!out) return E_INVALIDARG;
    *out = NULL;
    if (dir != DATADIR_GET) return E_NOTIMPL;
    f.cfFormat = CF_UNICODETEXT;
    f.ptd = NULL;
    f.dwAspect = DVASPECT_CONTENT;
    f.lindex = -1;
    f.tymed = TYMED_HGLOBAL;
    e = enum_new(&f, 1, 0);
    if (!e) return E_OUTOFMEMORY;
    *out = &e->iface;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE data_dadvise(IDataObject *me, FORMATETC *f, DWORD a,
                                              IAdviseSink *s, DWORD *c) {
    (void)me; (void)f; (void)a; (void)s; (void)c;
    return OLE_E_ADVISENOTSUPPORTED;
}
static HRESULT STDMETHODCALLTYPE data_dunadvise(IDataObject *me, DWORD c) {
    (void)me; (void)c;
    return OLE_E_ADVISENOTSUPPORTED;
}
static HRESULT STDMETHODCALLTYPE data_enumadvise(IDataObject *me, IEnumSTATDATA **out) {
    (void)me;
    if (out) *out = NULL;
    return OLE_E_ADVISENOTSUPPORTED;
}

static IDataObjectVtbl data_vtbl = {
    data_qi, data_addref, data_release, data_getdata, data_getdatahere,
    data_query, data_canon, data_setdata, data_enum,
    data_dadvise, data_dunadvise, data_enumadvise
};

static Data *data_new(const WCHAR *text) {
    Data *d = calloc(1, sizeof *d);
    if (!d) return NULL;
    d->iface.lpVtbl = &data_vtbl;
    d->ref = 1;
    d->text = _wcsdup(text);
    return d;
}

/* ----------------------------------------------------------- IDropSource --- */

typedef struct { IDropSource iface; LONG ref; } Source;

static HRESULT STDMETHODCALLTYPE src_qi(IDropSource *me, REFIID iid, void **out) {
    if (!out) return E_INVALIDARG;
    *out = NULL;
    if (IsEqualIID(iid, &IID_IUnknown) || IsEqualIID(iid, &IID_IDropSource)) {
        *out = me;
        IDropSource_AddRef(me);
        return S_OK;
    }
    return E_NOINTERFACE;
}
static ULONG STDMETHODCALLTYPE src_addref(IDropSource *me) {
    return InterlockedIncrement(&((Source *)me)->ref);
}
static ULONG STDMETHODCALLTYPE src_release(IDropSource *me) {
    Source *s = (Source *)me;
    LONG n = InterlockedDecrement(&s->ref);
    if (n == 0) free(s);
    return (ULONG)n;
}
static HRESULT STDMETHODCALLTYPE src_continue(IDropSource *me, BOOL esc, DWORD keys) {
    (void)me;
    if (esc) return DRAGDROP_S_CANCEL;
    if (!(keys & MK_LBUTTON)) return DRAGDROP_S_DROP;   /* the user let go */
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE src_feedback(IDropSource *me, DWORD effect) {
    (void)me; (void)effect;
    return DRAGDROP_S_USEDEFAULTCURSORS;
}
static IDropSourceVtbl src_vtbl = { src_qi, src_addref, src_release, src_continue, src_feedback };

/* ----------------------------------------------------------- IDropTarget --- */

typedef struct { IDropTarget iface; LONG ref; } Target;

static int g_over_calls;

static HRESULT STDMETHODCALLTYPE tgt_qi(IDropTarget *me, REFIID iid, void **out) {
    if (!out) return E_INVALIDARG;
    *out = NULL;
    if (IsEqualIID(iid, &IID_IUnknown) || IsEqualIID(iid, &IID_IDropTarget)) {
        *out = me;
        IDropTarget_AddRef(me);
        return S_OK;
    }
    return E_NOINTERFACE;
}
static ULONG STDMETHODCALLTYPE tgt_addref(IDropTarget *me) {
    return InterlockedIncrement(&((Target *)me)->ref);
}
static ULONG STDMETHODCALLTYPE tgt_release(IDropTarget *me) {
    Target *t = (Target *)me;
    LONG n = InterlockedDecrement(&t->ref);
    if (n == 0) free(t);
    return (ULONG)n;
}

/* What the other side is offering. Enumerated rather than probed for a fixed
 * list, because "what a real IDataObject offers" is exactly the claim §43.9
 * item 4 says only a real drag can check. */
static void log_offered(IDataObject *obj) {
    IEnumFORMATETC *e = NULL;
    FORMATETC f;
    ULONG got = 0;
    int n = 0;
    char buf[256];
    if (!obj) { logf_("  (no IDataObject at all)"); return; }
    if (FAILED(IDataObject_EnumFormatEtc(obj, DATADIR_GET, &e)) || !e) {
        logf_("  EnumFormatEtc failed");
        return;
    }
    while (IEnumFORMATETC_Next(e, 1, &f, &got) == S_OK && got == 1) {
        HRESULT q = IDataObject_QueryGetData(obj, &f);
        logf_("  offers %s (tymed %lu) QueryGetData=%s",
              format_name(f.cfFormat, buf, sizeof buf), (unsigned long)f.tymed,
              q == S_OK ? "S_OK" : "no");
        n++;
        if (n > 32) break;
    }
    IEnumFORMATETC_Release(e);
    if (n == 0) logf_("  offers nothing");
}

static void log_text(IDataObject *obj) {
    FORMATETC f;
    STGMEDIUM m;
    f.cfFormat = CF_UNICODETEXT;
    f.ptd = NULL;
    f.dwAspect = DVASPECT_CONTENT;
    f.lindex = -1;
    f.tymed = TYMED_HGLOBAL;
    if (SUCCEEDED(IDataObject_GetData(obj, &f, &m)) && m.tymed == TYMED_HGLOBAL) {
        WCHAR *p = GlobalLock(m.hGlobal);
        char utf8[4096];
        WideCharToMultiByte(CP_UTF8, 0, p, -1, utf8, sizeof utf8, NULL, NULL);
        logf_("DROP-TEXT: <<<%s>>>", utf8);
        GlobalUnlock(m.hGlobal);
        ReleaseStgMedium(&m);              /* the medium is the source's; we free it */
    } else {
        logf_("DROP-TEXT: no CF_UNICODETEXT on the dropped object");
    }
}

static HRESULT STDMETHODCALLTYPE tgt_enter(IDropTarget *me, IDataObject *obj,
                                           DWORD keys, POINTL pt, DWORD *effect) {
    (void)me; (void)keys;
    g_over_calls = 0;
    logf_("TARGET-DragEnter at screen (%ld,%ld)", (long)pt.x, (long)pt.y);
    log_offered(obj);
    if (effect) *effect = DROPEFFECT_COPY;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE tgt_over(IDropTarget *me, DWORD keys, POINTL pt, DWORD *effect) {
    (void)me; (void)keys;
    g_over_calls++;
    if (g_over_calls <= 3 || g_over_calls % 20 == 0)
        logf_("TARGET-DragOver #%d at (%ld,%ld)", g_over_calls, (long)pt.x, (long)pt.y);
    if (effect) *effect = DROPEFFECT_COPY;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE tgt_leave(IDropTarget *me) {
    (void)me;
    logf_("TARGET-DragLeave after %d DragOver call(s)", g_over_calls);
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE tgt_drop(IDropTarget *me, IDataObject *obj,
                                          DWORD keys, POINTL pt, DWORD *effect) {
    (void)me; (void)keys;
    logf_("TARGET-Drop at screen (%ld,%ld) after %d DragOver call(s)",
          (long)pt.x, (long)pt.y, g_over_calls);
    log_offered(obj);
    log_text(obj);
    if (effect) *effect = DROPEFFECT_COPY;
    return S_OK;
}
static IDropTargetVtbl tgt_vtbl = {
    tgt_qi, tgt_addref, tgt_release, tgt_enter, tgt_over, tgt_leave, tgt_drop
};

/* ------------------------------------------------- windows, from outside --- */
/*
 * `--mode list` and `--mode place` exist because of Wine's virtual desktop.
 * Every Wine window in `wine explorer /desktop=...` lives *inside one X window*,
 * so `xdotool search` finds the desktop and nothing else: the two windows a
 * drag has to run between are Win32 windows that X cannot see separately. This
 * is the Win32-level answer, and it is in this binary rather than a second one
 * because it has to run in the same wineserver to enumerate the same desktop.
 */

static FILE *g_list;

static BOOL CALLBACK list_one(HWND w, LPARAM p) {
    RECT r;
    char title[512];
    DWORD pid = 0;
    (void)p;
    if (!IsWindowVisible(w)) return TRUE;
    if (!GetWindowRect(w, &r)) return TRUE;
    if (r.right <= r.left || r.bottom <= r.top) return TRUE;
    GetWindowThreadProcessId(w, &pid);
    title[0] = 0;
    GetWindowTextA(w, title, sizeof title);
    fprintf(g_list ? g_list : stdout, "%llu\t%lu\t%ld,%ld,%ld,%ld\t%s\n",
            (unsigned long long)(uintptr_t)w, (unsigned long)pid,
            (long)r.left, (long)r.top, (long)r.right, (long)r.bottom, title);
    return TRUE;
}

/* ---------------------------------------------------------------- window --- */

static int g_mode_source = 1, g_mode_target = 1;
static WCHAR *g_text;

static LRESULT CALLBACK wndproc(HWND w, UINT msg, WPARAM wp, LPARAM lp) {
    switch (msg) {
    case WM_LBUTTONDOWN:
        if (g_mode_source) {
            Data *d = data_new(g_text);
            Source *s = calloc(1, sizeof *s);
            DWORD effect = DROPEFFECT_NONE;
            HRESULT hr;
            s->iface.lpVtbl = &src_vtbl;
            s->ref = 1;
            logf_("SOURCE-DoDragDrop entering (button down in our window)");
            hr = DoDragDrop(&d->iface, &s->iface, DROPEFFECT_COPY | DROPEFFECT_MOVE, &effect);
            logf_("SOURCE-DoDragDrop returned %#lx, effect %#lx (%s)",
                  (unsigned long)hr, (unsigned long)effect,
                  hr == DRAGDROP_S_DROP ? "DROP" : hr == DRAGDROP_S_CANCEL ? "CANCEL" : "?");
            IDataObject_Release(&d->iface);
            IDropSource_Release(&s->iface);
        }
        return 0;
    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(w, &ps);
        RECT r;
        HBRUSH b = CreateSolidBrush(RGB(40, 90, 160));
        GetClientRect(w, &r);
        FillRect(dc, &r, b);
        DeleteObject(b);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, RGB(255, 255, 255));
        DrawTextA(dc, "probe-drag", -1, &r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        EndPaint(w, &ps);
        return 0;
    }
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    default: break;
    }
    return DefWindowProcW(w, msg, wp, lp);
}

int main(int argc, char **argv) {
    const char *title = "probe-drag";
    const char *logpath = NULL;
    const char *text = "probe-drag says hello";
    const char *mode = "both";
    const char *hwnd_arg = NULL;
    int x = 40, y = 40, cw = 360, ch = 260, seconds = 120;
    int i;
    HWND w;
    WNDCLASSEXW wc;
    Target *t;
    HRESULT hr;
    MSG msg;
    DWORD deadline;

    g_t0 = GetTickCount();
    for (i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--title") && i + 1 < argc) title = argv[++i];
        else if (!strcmp(argv[i], "--log") && i + 1 < argc) logpath = argv[++i];
        else if (!strcmp(argv[i], "--text") && i + 1 < argc) text = argv[++i];
        else if (!strcmp(argv[i], "--x") && i + 1 < argc) x = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--y") && i + 1 < argc) y = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--w") && i + 1 < argc) cw = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--h") && i + 1 < argc) ch = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--seconds") && i + 1 < argc) seconds = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--hwnd") && i + 1 < argc) hwnd_arg = argv[++i];
        else if (!strcmp(argv[i], "--mode") && i + 1 < argc) {
            mode = argv[++i];
            g_mode_source = (!strcmp(mode, "source") || !strcmp(mode, "both"));
            g_mode_target = (!strcmp(mode, "target") || !strcmp(mode, "both"));
        }
    }

    /* The two modes that do not make a window and do not return to the pump. */
    if (!strcmp(mode, "list")) {
        if (logpath) g_list = fopen(logpath, "w");
        EnumWindows(list_one, 0);
        if (g_list) fclose(g_list);
        return 0;
    }
    if (!strcmp(mode, "place")) {
        HWND target = (HWND)(uintptr_t)strtoull(hwnd_arg ? hwnd_arg : "0", NULL, 0);
        if (!target || !IsWindow(target)) { fprintf(stderr, "no such window\n"); return 2; }
        ShowWindow(target, SW_RESTORE);
        SetWindowPos(target, HWND_TOP, x, y, cw, ch, SWP_SHOWWINDOW);
        SetForegroundWindow(target);
        {   RECT r;
            GetWindowRect(target, &r);
            printf("%ld,%ld,%ld,%ld\n", (long)r.left, (long)r.top, (long)r.right, (long)r.bottom); }
        return 0;
    }

    if (logpath) g_log = fopen(logpath, "w");

    {   int n = MultiByteToWideChar(CP_UTF8, 0, text, -1, NULL, 0);
        g_text = calloc(n, sizeof(WCHAR));
        MultiByteToWideChar(CP_UTF8, 0, text, -1, g_text, n); }

    /* This is a console binary so that its log goes somewhere a harness can
     * read without a window, and Wine gives a console binary a console
     * *window* — 665x434 at the top-left, on top of whatever is there. That is
     * not cosmetic here: the window it covered was the one a drag has to land
     * on, and a drop that lands on a console is a drop the compositor never
     * sees. The compositor hides its own for the neighbouring reason (the shell
     * grouping the toplevel with it); this one is hidden because it is in the
     * way of the gesture. */
    {   HWND con = GetConsoleWindow();
        if (con) ShowWindow(con, SW_HIDE); }

    hr = OleInitialize(NULL);
    logf_("OleInitialize -> %#lx", (unsigned long)hr);

    memset(&wc, 0, sizeof wc);
    wc.cbSize = sizeof wc;
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"probe_drag";
    RegisterClassExW(&wc);

    {   WCHAR wtitle[256];
        MultiByteToWideChar(CP_UTF8, 0, title, -1, wtitle, 256);
        w = CreateWindowExW(0, L"probe_drag", wtitle, WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                            x, y, cw, ch, NULL, NULL, wc.hInstance, NULL); }
    if (!w) { logf_("FATAL: CreateWindowExW failed %lu", GetLastError()); return 2; }
    logf_("window %p at %d,%d %dx%d mode=%s%s", (void *)w, x, y, cw, ch,
          g_mode_target ? "target " : "", g_mode_source ? "source" : "");

    if (g_mode_target) {
        t = calloc(1, sizeof *t);
        t->iface.lpVtbl = &tgt_vtbl;
        t->ref = 1;
        hr = RegisterDragDrop(w, &t->iface);
        logf_("RegisterDragDrop -> %#lx (%s)", (unsigned long)hr,
              hr == S_OK ? "this window accepts drops" : "FAILED");
    }

    /* Ready, and said so with a line the harness waits for: a harness that
     * starts dragging before RegisterDragDrop has run measures nothing. */
    logf_("PROBE-READY");

    deadline = GetTickCount() + (DWORD)seconds * 1000;
    for (;;) {
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
            if (msg.message == WM_QUIT) goto done;
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if (GetTickCount() > deadline) break;
        Sleep(10);
    }
done:
    if (g_mode_target) RevokeDragDrop(w);
    logf_("PROBE-EXIT");
    OleUninitialize();
    return 0;
}
