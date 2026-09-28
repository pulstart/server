# 0.9.17

Update hosts first; client 0.12.16 is recommended but not required (no wire
format change).

- Keep streaming while the host GPU is saturated. On NVIDIA every NVENC
  submission queues behind a GPU-bound game and no driver priority changes
  that; the new Vulkan Video encoder (H.264/HEVC) runs on the isolated encode
  engine. 1440p at 100% GPU load: 20 fps / p99 449 ms before, 48 fps / p99
  6 ms now. VAAPI (AMD/Intel) and Vulkan are tried before NVENC for every
  codec the client decodes in hardware. `ST_VULKAN_ENCODE=0` opts out.
- Media threads run realtime (capture, send, input, audio) or boosted
  (encode) on every platform; the installer grants `cap_sys_nice`. Windows also
  raises the GPU scheduling class, disables EcoQoS and uses 1 ms timers; macOS
  opts the server out of App Nap and captures on a user-interactive queue.
- KMS capture no longer busy-waits: NVIDIA's `glFinish` spun a full CPU core
  for the whole GPU wait (up to 36 ms per frame under load). NVIDIA readback
  now converts straight to NV12 on the GPU (62% less copied) into recycled
  buffers.
- Adaptive frame rate drops straight to the rate the capture sustains instead
  of rebuilding the encoder once per step.
- Fix: a client whose preferred hardware codec could not be encoded never fell
  back to the next codec (e.g. AV1 client on a pre-Ada NVIDIA GPU).
- Fix: CPU colour conversion used BT.601 while streams signal BT.709.
- Linux packages now bundle FFmpeg 8.1.

GPU-gated regression tests cover encode/decode roundtrips (IDR placement,
BT.709 colour) for H.264/HEVC, NV12 readback orientation, scanout-to-decode
through the Vulkan path, and a live KMS pipeline under load.
