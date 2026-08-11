#!/usr/bin/env bash
set -u
D=/mnt/datasets/builds/sv-worktrees/sv-codegen/reference/venus
VR=https://gitlab.freedesktop.org/virgl/virglrenderer/-/raw/virglrenderer-1.3.0/src/venus
for f in vn_protocol_renderer_cs.h vn_protocol_renderer_types.h vn_protocol_renderer_defines.h \
         vn_protocol_renderer_handles.h vn_protocol_renderer_buffer.h vn_protocol_renderer_structs.h \
         vn_protocol_renderer_instance.h vn_protocol_renderer_dispatches.h; do
  curl -sS -m 180 -o "$D/$f" "$VR/venus-protocol/$f"
done
for f in vkr_cs.h vkr_cs.c vkr_ring.h vkr_ring.c; do
  curl -sS -m 180 -o "$D/virgl_$f" "https://gitlab.freedesktop.org/virgl/virglrenderer/-/raw/main/src/venus/$f"
done
ls -la "$D"
