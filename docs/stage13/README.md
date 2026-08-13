# Stage 13: what a person saw

`docs/design.md` §18 is the account. These are the pictures it rests on.

| file | what it is |
|---|---|
| `stage13-torn-file-arena.png` | the guest's screen, Firefox over a **file-backed arena on the virtio-fs export**. The striped block is an `xdg_popup` at 460x357; the bands are two to three rows tall, which is one 4096-byte page at 1840 bytes a row. §10.3 measured that export incoherent for two live mappings in stage 5. |
| `stage13-torn-popup-dump.png` | the *same popup's* `--dump-frames` PPM — what the compositor believed it had composited, written before GDI touches anything. Same stripes. That is what puts the fault in the transport rather than in the blit. |
| `stage13-clean-guest-ram.png` | the same client, the same compositor, the same desktop, over the **guest's own RAM**. Nothing else changed. |
| `compositor-torn-file-arena.log` | the torn run's compositor log, 3714 lines. Four popups created and mapped, none destroyed — §18.4. |

Frame counts, which say the same thing without a picture: the torn run dumped
**583 frames of which 47 were distinct**; the guest-RAM run dumped **806 of
which 462**.
