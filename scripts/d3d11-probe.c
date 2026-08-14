/* d3d11-probe — a Direct3D 11 program rendering through DXVK on shared_vulkan.
 *
 * WHY IT HAS NO WINDOW
 *
 * Not because presentation is missing — docs/design.md §10 stage 2 is built and this
 * ICD has a `VK_KHR_swapchain` — but because a `IDXGISwapChain` would put Wine's
 * window management between the test and the thing being tested. What this
 * program measures is the *forwarder*, and a window adds an `HWND`, a
 * `VK_KHR_win32_surface` mapped onto a headless one, and a whole second
 * subsystem's worth of ways to fail. `scripts/sv-present.sh` measures
 * presentation directly and byte for byte, which is a better instrument for it.
 *
 * D3D11 does not need a window to render. `D3D11CreateDevice` with no swap
 * chain, a render-target texture, and a staging texture read back on the CPU is
 * the whole API surface a forwarder has to get right: adapter enumeration,
 * device and immediate-context creation, buffer and texture creation, shader
 * compilation and binding, a draw, and a resource copy back to host memory. That
 * is the same shape as `gl-probe`, for the same reason.
 *
 * WHAT IT PROVES, AND WHAT IT DOES NOT
 *
 * It proves that DXVK can create a `VkDevice` on our forwarded physical device
 * and drive a real frame through it. It does not prove that a *game* runs: a
 * game presents through DXGI, and this program never touches DXGI's swap chain.
 * Those are separate claims and this program is careful to make only the first.
 *
 * The HLSL is compiled at run time through `D3DCompile`, resolved by name from
 * `d3dcompiler_47.dll` rather than linked, so that a Wine build without one
 * says so instead of failing to start.
 *
 * Build (from the dev shell, which has the mingw cross compiler):
 *   x86_64-w64-mingw32-gcc -O1 -o d3d11-probe.exe scripts/d3d11-probe.c \
 *       -ld3d11 -ldxgi -luuid -lole32
 */
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <stdio.h>
#include <stdlib.h>

#define W 256
#define H 256

/* The smallest pair that still exercises both stages, the input assembler and
 * the interpolators.
 *
 * `vs_4_0`/`ps_4_0` rather than 5_0: the compiler here is Wine's, which is
 * vkd3d-shader's HLSL front end, and its shader-model 5 output is not what fxc
 * would emit. Feature level 11 accepts a 4_0 shader, and what this program is
 * measuring is the forwarder rather than the compiler. */
static const char *HLSL =
    "struct VSIn  { float4 pos : POSITION; float4 col : COLOR; };\n"
    "struct VSOut { float4 pos : SV_POSITION; float4 col : COLOR; };\n"
    "VSOut vs_main(VSIn i) { VSOut o; o.pos = i.pos; o.col = i.col; return o; }\n"
    "float4 ps_main(VSOut i) : SV_TARGET { return float4(i.col.rgb, 1.0); }\n";

typedef HRESULT(WINAPI *PFN_D3DCompile)(LPCVOID, SIZE_T, LPCSTR, const void *, void *, LPCSTR,
                                        LPCSTR, UINT, UINT, ID3DBlob **, ID3DBlob **);
static PFN_D3DCompile p_D3DCompile;

static ID3DBlob *compile(const char *entry, const char *target) {
    ID3DBlob *code = NULL, *err = NULL;
    HRESULT hr = p_D3DCompile(HLSL, strlen(HLSL), "probe.hlsl", NULL, NULL, entry, target, 0, 0,
                              &code, &err);
    if (FAILED(hr)) {
        printf("D3DCompile(%s) failed 0x%08lx: %s\n", entry, (unsigned long)hr,
               err ? (const char *)ID3D10Blob_GetBufferPointer(err) : "");
        return NULL;
    }
    return code;
}

static const char *hr_name(HRESULT hr) {
    switch ((unsigned)hr) {
    case 0x00000000: return "S_OK";
    case 0x887a0004: return "DXGI_ERROR_UNSUPPORTED";
    case 0x887a0005: return "DXGI_ERROR_DEVICE_REMOVED";
    case 0x887a0001: return "DXGI_ERROR_INVALID_CALL";
    case 0x80004005: return "E_FAIL";
    case 0x80070057: return "E_INVALIDARG";
    case 0x8007000e: return "E_OUTOFMEMORY";
    case 0x80004001: return "E_NOTIMPL";
    default: return "?";
    }
}

int main(void) {
    HRESULT hr;

    HMODULE compiler = LoadLibraryA("d3dcompiler_47.dll");
    if (!compiler)
        compiler = LoadLibraryA("d3dcompiler_43.dll");
    p_D3DCompile = compiler ? (PFN_D3DCompile)(void *)GetProcAddress(compiler, "D3DCompile") : NULL;
    if (!p_D3DCompile) {
        printf("SKIP: no d3dcompiler with D3DCompile\n");
        return 7;
    }

    /* Which adapter DXVK found, printed before anything else, because if the
     * ICD is not reachable this is where it shows. */
    IDXGIFactory1 *factory = NULL;
    hr = CreateDXGIFactory1(&IID_IDXGIFactory1, (void **)&factory);
    if (FAILED(hr)) {
        printf("CreateDXGIFactory1 failed 0x%08lx (%s)\n", (unsigned long)hr, hr_name(hr));
        return 1;
    }
    for (UINT i = 0;; i++) {
        IDXGIAdapter1 *ad = NULL;
        if (IDXGIFactory1_EnumAdapters1(factory, i, &ad) != S_OK)
            break;
        DXGI_ADAPTER_DESC1 d;
        IDXGIAdapter1_GetDesc1(ad, &d);
        printf("adapter %u: %ls (vendor 0x%04x device 0x%04x, %llu MiB)\n", i, d.Description,
               (unsigned)d.VendorId, (unsigned)d.DeviceId,
               (unsigned long long)(d.DedicatedVideoMemory >> 20));
        IDXGIAdapter1_Release(ad);
    }

    static const D3D_FEATURE_LEVEL want[] = {
        D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0,
    };
    ID3D11Device *dev = NULL;
    ID3D11DeviceContext *ctx = NULL;
    D3D_FEATURE_LEVEL got = 0;
    hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, want,
                           sizeof want / sizeof *want, D3D11_SDK_VERSION, &dev, &got, &ctx);
    if (FAILED(hr)) {
        printf("D3D11CreateDevice failed 0x%08lx (%s)\n", (unsigned long)hr, hr_name(hr));
        return 2;
    }
    printf("feature level %x.%x\n", (unsigned)(got >> 12) & 0xf, (unsigned)(got >> 8) & 0xf);

    /* The render target, and a staging texture to read it back through. */
    D3D11_TEXTURE2D_DESC td = {0};
    td.Width = W;
    td.Height = H;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    ID3D11Texture2D *rt = NULL;
    hr = ID3D11Device_CreateTexture2D(dev, &td, NULL, &rt);
    if (FAILED(hr)) {
        printf("CreateTexture2D(rt) failed 0x%08lx\n", (unsigned long)hr);
        return 3;
    }
    ID3D11RenderTargetView *rtv = NULL;
    hr = ID3D11Device_CreateRenderTargetView(dev, (ID3D11Resource *)rt, NULL, &rtv);
    if (FAILED(hr)) {
        printf("CreateRenderTargetView failed 0x%08lx\n", (unsigned long)hr);
        return 3;
    }

    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D *stage = NULL;
    hr = ID3D11Device_CreateTexture2D(dev, &td, NULL, &stage);
    if (FAILED(hr)) {
        printf("CreateTexture2D(staging) failed 0x%08lx\n", (unsigned long)hr);
        return 3;
    }

    /* A triangle, in a vertex buffer, with a colour per vertex so that a frame
     * with the interpolators broken looks different from one without. */
    struct V {
        float x, y, z, w;
        float r, g, b, a;
    };
    /* THE WINDING IS LOAD-BEARING, and it was wrong.
     *
     * D3D11's default rasterizer state is `CullMode = D3D11_CULL_BACK` with
     * `FrontCounterClockwise = FALSE`, and D3D11's viewport transform puts y
     * *down* in screen space. Listed bottom-left, bottom-right, top-centre the
     * triangle is counter-clockwise on screen, so it is back-facing and every
     * pixel of it is culled — on any implementation, including RADV with no
     * forwarding at all, which is how this was finally established. The frame
     * comes back as the clear colour and nothing reports an error.
     *
     * Swapping the first two vertices makes it clockwise and therefore front-
     * facing. This is a bug in this program that the previous stage could not
     * have found: DXVK never reached a draw, because it refused the device over
     * `VK_KHR_swapchain` (docs/design.md §0.5). */
    static const struct V verts[3] = {
        {0.8f, -0.7f, 0.f, 1.f, 0.1f, 1.f, 0.2f, 1.f},
        {-0.8f, -0.7f, 0.f, 1.f, 1.f, 0.15f, 0.1f, 1.f},
        {0.0f, 0.85f, 0.f, 1.f, 0.15f, 0.2f, 1.f, 1.f},
    };
    D3D11_BUFFER_DESC bd = {0};
    bd.ByteWidth = sizeof verts;
    bd.Usage = D3D11_USAGE_IMMUTABLE;
    bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
    D3D11_SUBRESOURCE_DATA srd = {verts, 0, 0};
    ID3D11Buffer *vb = NULL;
    hr = ID3D11Device_CreateBuffer(dev, &bd, &srd, &vb);
    if (FAILED(hr)) {
        printf("CreateBuffer failed 0x%08lx\n", (unsigned long)hr);
        return 3;
    }

    ID3DBlob *vsb = compile("vs_main", "vs_4_0");
    ID3DBlob *psb = compile("ps_main", "ps_4_0");
    if (!vsb || !psb)
        return 4;
    const void *vsc = ID3D10Blob_GetBufferPointer(vsb);
    SIZE_T vsn = ID3D10Blob_GetBufferSize(vsb);
    ID3D11VertexShader *vs = NULL;
    hr = ID3D11Device_CreateVertexShader(dev, vsc, vsn, NULL, &vs);
    if (FAILED(hr)) {
        printf("CreateVertexShader failed 0x%08lx\n", (unsigned long)hr);
        return 4;
    }
    ID3D11PixelShader *ps = NULL;
    hr = ID3D11Device_CreatePixelShader(dev, ID3D10Blob_GetBufferPointer(psb),
                                        ID3D10Blob_GetBufferSize(psb), NULL, &ps);
    if (FAILED(hr)) {
        printf("CreatePixelShader failed 0x%08lx\n", (unsigned long)hr);
        return 4;
    }
    static const D3D11_INPUT_ELEMENT_DESC il[] = {
        {"POSITION", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
        {"COLOR", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 16, D3D11_INPUT_PER_VERTEX_DATA, 0},
    };
    ID3D11InputLayout *layout = NULL;
    hr = ID3D11Device_CreateInputLayout(dev, il, 2, vsc, vsn, &layout);
    if (FAILED(hr)) {
        printf("CreateInputLayout failed 0x%08lx\n", (unsigned long)hr);
        return 4;
    }

    D3D11_VIEWPORT vp = {0.f, 0.f, (float)W, (float)H, 0.f, 1.f};
    ID3D11DeviceContext_RSSetViewports(ctx, 1, &vp);
    ID3D11DeviceContext_OMSetRenderTargets(ctx, 1, &rtv, NULL);
    /* Not black: the clear is the control. If the corner of the frame carries
     * this colour the render target, the copy and the readback all work and any
     * failure below is the draw; if it does not, the failure is earlier than the
     * draw and saying which saves an afternoon. */
    const float clear[4] = {0.05f, 0.02f, 0.12f, 1.f};
    ID3D11DeviceContext_ClearRenderTargetView(ctx, rtv, clear);
    ID3D11DeviceContext_IASetInputLayout(ctx, layout);
    UINT stride = sizeof(struct V), offset = 0;
    ID3D11DeviceContext_IASetVertexBuffers(ctx, 0, 1, &vb, &stride, &offset);
    ID3D11DeviceContext_IASetPrimitiveTopology(ctx, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ID3D11DeviceContext_VSSetShader(ctx, vs, NULL, 0);
    ID3D11DeviceContext_PSSetShader(ctx, ps, NULL, 0);
    ID3D11DeviceContext_Draw(ctx, 3, 0);

    ID3D11DeviceContext_CopyResource(ctx, (ID3D11Resource *)stage, (ID3D11Resource *)rt);
    ID3D11DeviceContext_Flush(ctx);

    D3D11_MAPPED_SUBRESOURCE map = {0};
    hr = ID3D11DeviceContext_Map(ctx, (ID3D11Resource *)stage, 0, D3D11_MAP_READ, 0, &map);
    if (FAILED(hr)) {
        printf("Map failed 0x%08lx\n", (unsigned long)hr);
        return 5;
    }

    /* The same kind of judgement gl-probe makes: a triangle covers a knowable
     * fraction of the frame and the interpolated colours make many of them. A
     * forwarder that drops the draw leaves a black frame and passes every check
     * that is not about pixels. */
    {
        const unsigned char *c = (const unsigned char *)map.pData;
        const unsigned char *m =
            (const unsigned char *)map.pData + (size_t)(H * 3 / 4) * map.RowPitch + 4 * (W / 2);
        printf("corner %u,%u,%u  centre %u,%u,%u  (clear is 13,5,31)\n", c[0], c[1], c[2], m[0],
               m[1], m[2]);
    }
    long cover = 0;
    int seen[32768];
    memset(seen, 0, sizeof seen);
    long colours = 0;
    for (int y = 0; y < H; y++) {
        const unsigned char *row = (const unsigned char *)map.pData + (size_t)y * map.RowPitch;
        for (int x = 0; x < W; x++) {
            const unsigned char *p = row + 4 * x;
            if (abs((int)p[0] - 13) > 10 || abs((int)p[1] - 5) > 10 || abs((int)p[2] - 31) > 10) {
                cover++;
                int k = ((p[0] >> 3) << 10) | ((p[1] >> 3) << 5) | (p[2] >> 3);
                if (!seen[k]) {
                    seen[k] = 1;
                    colours++;
                }
            }
        }
    }
    const char *out = getenv("D3D11_PROBE_PPM");
    if (out) {
        FILE *f = fopen(out, "wb");
        if (f) {
            fprintf(f, "P6\n%d %d\n255\n", W, H);
            for (int y = 0; y < H; y++) {
                const unsigned char *row =
                    (const unsigned char *)map.pData + (size_t)y * map.RowPitch;
                for (int x = 0; x < W; x++)
                    fwrite(row + 4 * x, 1, 3, f);
            }
            fclose(f);
        }
    }
    ID3D11DeviceContext_Unmap(ctx, (ID3D11Resource *)stage, 0);

    double pct = 100.0 * cover / (W * H);
    printf("coverage %.1f%% of %dx%d\n", pct, W, H);
    printf("colours  %ld distinct (5 bits per channel)\n", colours);

    /* The triangle's area in clip space is 0.8*1.55 = 1.24 of the 4.0 the frame
     * covers, so 31%. Anything between a fifth and a half of the frame is the
     * triangle; a black frame and a full frame both fail. */
    if (pct < 20.0 || pct > 45.0) {
        printf("FAIL: coverage %.1f%% is not a triangle\n", pct);
        return 6;
    }
    if (colours < 64) {
        printf("FAIL: %ld colours means the interpolators did nothing\n", colours);
        return 6;
    }
    printf("PASS: Direct3D 11 rendered a frame and it was read back\n");
    return 0;
}
