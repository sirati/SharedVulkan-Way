#!/usr/bin/env bash
set -u
D=/mnt/datasets/builds/sv-worktrees/sv-codegen/reference/venus
MESA=https://gitlab.freedesktop.org/mesa/mesa/-/raw/mesa-26.2.0
VP=https://gitlab.freedesktop.org/virgl/venus-protocol/-/raw/main

for f in vn_protocol_driver.h vn_protocol_driver_defines.h vn_protocol_driver_types.h \
         vn_protocol_driver_cs.h vn_protocol_driver_handles.h vn_protocol_driver_structs.h \
         vn_protocol_driver_info.h vn_protocol_driver_transport.h vn_protocol_driver_buffer.h \
         vn_protocol_driver_device.h vn_protocol_driver_instance.h vn_protocol_driver_pipeline.h \
         vn_protocol_driver_command_buffer.h vn_protocol_driver_queue.h \
         vn_protocol_driver_descriptor_set.h vn_protocol_driver_device_memory.h; do
  curl -sS -m 180 -o "$D/$f" "$MESA/src/virtio/venus-protocol/$f"
done

for f in vn_cs.h vn_cs.c vn_ring.h vn_instance.c vn_common.h; do
  curl -sS -m 180 -o "$D/mesa_$f" "$MESA/src/virtio/vulkan/$f"
done

mkdir -p "$D/venus-protocol-templates"
for f in banner.in common.h driver.h driver_commands.h driver_cs.h driver_defines.h \
         driver_handles.h driver_info.h driver_types.h renderer.h renderer_commands.h \
         renderer_cs.h renderer_defines.h renderer_dispatches.h renderer_handles.h \
         renderer_info.h renderer_types.h renderer_util.h types.h types_chain.h \
         types_command.h types_custom.h types_handle.h types_scalar.h types_struct.h \
         types_union.h; do
  curl -sS -m 180 -o "$D/venus-protocol-templates/$f" "$VP/templates/$f"
done

curl -sS -m 180 -o "$D/VK_EXT_command_serialization.txt" "$VP/docs/VK_EXT_command_serialization.txt"
curl -sS -m 180 -o "$D/venus-protocol-README" "$VP/README"
curl -sS -m 180 -o "$D/vn_protocol.py" "$VP/vn_protocol.py"

ls -la "$D"
