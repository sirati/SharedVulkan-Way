/* probe-clip.c -- a Windows application, for the clipboard's sake.
 *
 * The claim stage 13 has to support is "copy in a Linux client, paste in a
 * Windows application, and the reverse". The compositor's own log cannot make
 * that claim: it is written by the code under test, and a compositor that
 * convinced itself it had called SetClipboardData is exactly what a bug looks
 * like. So the Windows half of the evidence is a **separate process** that
 * knows nothing about waycompose and talks to the same clipboard any other
 * Windows application does.
 *
 * That is the same argument scripts/probe-window.c makes for windows and
 * scripts/probe-input.c makes for input, and it is docs/waycompose-design.md §8.5's split
 * applied to a third thing.
 *
 *   probe-clip.exe get          CF_UNICODETEXT, as UTF-8 on stdout
 *   probe-clip.exe get-html     the registered "HTML Format", raw, on stdout
 *   probe-clip.exe set TEXT     put TEXT on the clipboard as CF_UNICODETEXT
 *   probe-clip.exe set-html HTML  put HTML on it, with the CF_HTML header
 *   probe-clip.exe list         the format numbers on it now
 *   probe-clip.exe wait-for S N poll up to N s for text containing S, print it
 *   probe-clip.exe set-dib W H  put a W x H picture on it as CF_DIB (§93)
 *   probe-clip.exe wait-dib N   poll up to N s for a CF_DIB; print its size
 *                               and the size of the "PNG" beside it
 *
 * Exit code 0 on success, 1 if the format is not there, 2 on a usage or API
 * error, so a shell harness can assert without parsing.
 */

#include <windows.h>
#include <stdio.h>
#include <string.h>

/* The clipboard is one global lock and OpenClipboard fails rather than waiting.
 * Every real application retries; a harness that did not would report a race as
 * a missing feature. */
static int open_clipboard(void) {
    int i;
    for (i = 0; i < 20; i++) {
        if (OpenClipboard(NULL))
            return 1;
        Sleep(25);
    }
    fprintf(stderr, "probe-clip: OpenClipboard failed: %lu\n", GetLastError());
    return 0;
}

static int put(UINT format, const void *bytes, size_t len) {
    HGLOBAL h;
    void *p;
    if (!open_clipboard())
        return 2;
    EmptyClipboard();
    h = GlobalAlloc(GMEM_MOVEABLE, len);
    if (!h) {
        CloseClipboard();
        return 2;
    }
    p = GlobalLock(h);
    memcpy(p, bytes, len);
    GlobalUnlock(h);
    if (!SetClipboardData(format, h)) {
        fprintf(stderr, "probe-clip: SetClipboardData failed: %lu\n", GetLastError());
        GlobalFree(h);
        CloseClipboard();
        return 2;
    }
    CloseClipboard();
    return 0;
}

/* Write the bytes out raw. `_setmode(_O_BINARY)` is deliberately not used: the
 * harness compares text, and a CRLF that survives to the shell is a fact about
 * the clipboard worth seeing rather than one to hide. */
static int emit(const void *bytes, size_t len) {
    fwrite(bytes, 1, len, stdout);
    fflush(stdout);
    return 0;
}

static int get_text(void) {
    HANDLE h;
    const wchar_t *w;
    int n;
    char *utf8;
    int rc;
    if (!open_clipboard())
        return 2;
    h = GetClipboardData(CF_UNICODETEXT);
    if (!h) {
        CloseClipboard();
        fprintf(stderr, "probe-clip: no CF_UNICODETEXT on the clipboard\n");
        return 1;
    }
    w = (const wchar_t *)GlobalLock(h);
    n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    utf8 = (char *)malloc(n > 0 ? (size_t)n : 1);
    WideCharToMultiByte(CP_UTF8, 0, w, -1, utf8, n, NULL, NULL);
    GlobalUnlock(h);
    CloseClipboard();
    /* n counts the terminating NUL, which is not part of the text. */
    rc = emit(utf8, n > 0 ? (size_t)(n - 1) : 0);
    free(utf8);
    return rc;
}

static int get_format(const char *name) {
    UINT fmt = RegisterClipboardFormatW(L"HTML Format");
    HANDLE h;
    const char *p;
    size_t len;
    int rc;
    (void)name;
    if (!fmt) {
        fprintf(stderr, "probe-clip: RegisterClipboardFormatW failed\n");
        return 2;
    }
    if (!open_clipboard())
        return 2;
    h = GetClipboardData(fmt);
    if (!h) {
        CloseClipboard();
        fprintf(stderr, "probe-clip: no HTML Format on the clipboard\n");
        return 1;
    }
    p = (const char *)GlobalLock(h);
    len = strlen(p);
    rc = emit(p, len);
    GlobalUnlock(h);
    CloseClipboard();
    return rc;
}

/* Wait, inside **one** process, for the clipboard to hold text containing
 * `needle`, then print the whole of it.
 *
 * This exists because the shell doing the polling was worse than the thing it
 * was polling. Each `probe-clip.exe get` is a Wine process, a poll ran twenty
 * of them, and the compositor's log filled with pairs of WM_CLIPBOARDUPDATE
 * saying the clipboard had gone empty and come back — the churn was the
 * measurement disturbing the thing measured. One process that asks in a loop
 * touches the window station once and leaves the clipboard alone between
 * asks.
 *
 * Still a poll rather than an AddClipboardFormatListener, and deliberately: a
 * listener needs a message loop, a message loop needs a window, and a window
 * in this process is one more thing on the desktop that the compositor's own
 * window has to not be confused with. A one-second `GetClipboardSequenceNumber`
 * is enough to answer "has it changed" and needs nothing.
 */
static int wait_for(const char *needle, int seconds) {
    int i;
    for (i = 0; i < seconds; i++) {
        HANDLE h;
        if (open_clipboard()) {
            h = GetClipboardData(CF_UNICODETEXT);
            if (h) {
                const wchar_t *w = (const wchar_t *)GlobalLock(h);
                int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
                char *utf8 = (char *)malloc(n > 0 ? (size_t)n : 1);
                WideCharToMultiByte(CP_UTF8, 0, w, -1, utf8, n, NULL, NULL);
                GlobalUnlock(h);
                CloseClipboard();
                if (strstr(utf8, needle) != NULL) {
                    int rc = emit(utf8, n > 0 ? (size_t)(n - 1) : 0);
                    free(utf8);
                    return rc;
                }
                free(utf8);
            } else {
                CloseClipboard();
            }
        }
        Sleep(1000);
    }
    fprintf(stderr, "probe-clip: no CF_UNICODETEXT containing \"%s\" after %d s\n",
            needle, seconds);
    return 1;
}

static int list(void) {
    UINT fmt = 0;
    wchar_t name[128];
    if (!open_clipboard())
        return 2;
    while ((fmt = EnumClipboardFormats(fmt)) != 0) {
        int n = GetClipboardFormatNameW(fmt, name, 128);
        if (n > 0) {
            name[n] = 0;
            printf("%u %ls\n", fmt, name);
        } else {
            printf("%u\n", fmt);
        }
    }
    CloseClipboard();
    return 0;
}

/* The CF_HTML header, which is not decoration: a reader is told where the
 * fragment starts by these byte offsets and shows nothing without them. Ten
 * digits each so the header's own length does not depend on the numbers in it.
 * Written out here rather than shared with the Rust side on purpose — this
 * program is the independent witness, and a witness that used the code under
 * test would not be one. */
static int set_html(const char *fragment) {
    static const char *PREFIX = "<html>\r\n<body>\r\n<!--StartFragment-->";
    static const char *SUFFIX = "<!--EndFragment-->\r\n</body>\r\n</html>";
    char header[256];
    size_t start_html = 105, start_frag, end_frag, end_html, len;
    char *buf;
    int rc;
    start_frag = start_html + strlen(PREFIX);
    end_frag = start_frag + strlen(fragment);
    end_html = end_frag + strlen(SUFFIX);
    sprintf(header,
            "Version:0.9\r\nStartHTML:%010u\r\nEndHTML:%010u\r\n"
            "StartFragment:%010u\r\nEndFragment:%010u\r\n",
            (unsigned)start_html, (unsigned)end_html,
            (unsigned)start_frag, (unsigned)end_frag);
    if (strlen(header) != start_html) {
        fprintf(stderr, "probe-clip: header is %u bytes, expected %u\n",
                (unsigned)strlen(header), (unsigned)start_html);
        return 2;
    }
    len = end_html;
    buf = (char *)malloc(len + 1);
    strcpy(buf, header);
    strcat(buf, PREFIX);
    strcat(buf, fragment);
    strcat(buf, SUFFIX);
    rc = put(RegisterClipboardFormatW(L"HTML Format"), buf, len + 1);
    free(buf);
    return rc;
}

/* §93. A picture, the way Paint puts one there: CF_DIB, a 40-byte header,
 * 32-bit BI_RGB rows bottom-up, the fourth byte zero (BI_RGB calls it
 * reserved). Memory row r has R=200, G=80*r, B=40*x, so the image's top-left
 * pixel -- the *last* row in memory -- is (200, 80*(h-1), 0). */
static int set_dib(int w, int h) {
    size_t len = 40 + (size_t)w * h * 4;
    unsigned char *buf = (unsigned char *)calloc(1, len);
    BITMAPINFOHEADER *bih = (BITMAPINFOHEADER *)buf;
    int x, y, rc;
    bih->biSize = 40;
    bih->biWidth = w;
    bih->biHeight = h;
    bih->biPlanes = 1;
    bih->biBitCount = 32;
    bih->biCompression = BI_RGB;
    bih->biSizeImage = (DWORD)(w * h * 4);
    for (y = 0; y < h; y++)
        for (x = 0; x < w; x++) {
            unsigned char *p = buf + 40 + ((size_t)y * w + x) * 4;
            p[0] = (unsigned char)(40 * x);
            p[1] = (unsigned char)(80 * y);
            p[2] = 200;
            p[3] = 0;
        }
    rc = put(CF_DIB, buf, len);
    free(buf);
    return rc;
}

/* §93. Wait for a picture on the clipboard and describe it: the CF_DIB's
 * header, and the size of the registered "PNG" beside it (0 if none). */
static int wait_dib(int seconds) {
    UINT png = RegisterClipboardFormatW(L"PNG");
    int i;
    for (i = 0; i < seconds * 4; i++) {
        if (open_clipboard()) {
            HANDLE h = GetClipboardData(CF_DIB);
            if (h) {
                const BITMAPINFOHEADER *bih = (const BITMAPINFOHEADER *)GlobalLock(h);
                HANDLE hp = GetClipboardData(png);
                unsigned long png_len = hp ? (unsigned long)GlobalSize(hp) : 0;
                printf("dib %ldx%ld bpp %u png %lu\n", (long)bih->biWidth, (long)bih->biHeight,
                       (unsigned)bih->biBitCount, png_len);
                GlobalUnlock(h);
                CloseClipboard();
                return 0;
            }
            CloseClipboard();
        }
        Sleep(250);
    }
    fprintf(stderr, "probe-clip: no CF_DIB after %d s\n", seconds);
    return 1;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
                "usage: probe-clip.exe get | get-html | set TEXT | set-html HTML | list"
                " | wait-for SUBSTRING [SECONDS]\n");
        return 2;
    }
    if (!strcmp(argv[1], "get"))
        return get_text();
    if (!strcmp(argv[1], "get-html"))
        return get_format("HTML Format");
    if (!strcmp(argv[1], "list"))
        return list();
    if (!strcmp(argv[1], "wait-for") && argc >= 3)
        return wait_for(argv[2], argc >= 4 ? atoi(argv[3]) : 30);
    if (!strcmp(argv[1], "set") && argc >= 3) {
        int n = MultiByteToWideChar(CP_UTF8, 0, argv[2], -1, NULL, 0);
        wchar_t *w = (wchar_t *)malloc((size_t)n * sizeof(wchar_t));
        int rc;
        MultiByteToWideChar(CP_UTF8, 0, argv[2], -1, w, n);
        rc = put(CF_UNICODETEXT, w, (size_t)n * sizeof(wchar_t));
        free(w);
        return rc;
    }
    if (!strcmp(argv[1], "set-html") && argc >= 3)
        return set_html(argv[2]);
    if (!strcmp(argv[1], "set-dib") && argc >= 4)
        return set_dib(atoi(argv[2]), atoi(argv[3]));
    if (!strcmp(argv[1], "wait-dib"))
        return wait_dib(argc >= 3 ? atoi(argv[2]) : 30);
    fprintf(stderr, "probe-clip: unknown command %s\n", argv[1]);
    return 2;
}
