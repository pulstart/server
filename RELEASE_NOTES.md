# 0.9.21

Server-only release; no wire format change.

- Desktop latency: while a client is connected, the NVIDIA GPU's memory clock
  is held at its P3 level. An idle desktop otherwise parks the GPU in P8 with
  the PCIe link at gen1, and every sparse update (typing, hovering) paid for
  it: over a 70 s idle-desktop session, capture to receive went from p50
  16.3 / p99 40.7 ms (with 5 frame-rate rebuilds) to 6.0 / 7.0 ms (none).
  Costs ~11 W while streaming; needs the system service (root).
  `ST_GPU_CLOCK_FLOOR=0` disables it.
- Fix: the first connect after a server start cut the bitrate from 20 to
  15 Mbps. The first frame waited behind encoder setup, which the bitrate
  controller read as a congested send queue. The send backlog is now
  measured from when a frame is encoded, and the copy engine is opened
  before the first capture.

# 0.9.20

Server-only release; no wire format change.

- Fix: every cursor shape change (hovering links, text fields, window edges)
  stalled KMS capture for 11-15 ms while the 256x256 cursor image was read out
  of video memory. About once a second of desktop use a frame overran, which
  pushed adaptive fps down. The read now uses streaming loads: ~1 ms.
- Fix: KMS capture leaked a GPU buffer handle on every frame for the whole
  session, pinning each buffer it had seen (e.g. a closed game's swapchain)
  and growing kernel memory.
- Fix: a session started while the cursor was hidden (inside a game) never
  showed a remote cursor, even after leaving the game.
- Fix: adaptive fps stepped a desktop session down to 90/60/48 fps. Frames
  that arrive sparsely (typing, hovering) find the GPU idle and take ~3x
  longer (encode 2 -> 6 ms, readback 2.5 -> 9.6 ms), which the controller
  read as overload even though no frame was waiting. It now counts an
  overrun only when a newer frame was held back by it, and judges headroom
  for stepping up from encoder busy time.
- Reconnecting is ~0.65 s faster (0.9 s to 0.25 s from connect to video on
  the test box): a codec the encoder self-test found too slow (HEVC at
  1440p120 on NVIDIA) is remembered for 10 minutes instead of being tested
  again on every connect, and the copy-engine device is kept between
  sessions (first frame 196 ms to 10 ms).
- Fix: clipboard sync never worked with the system-wide service (the
  installer default) and logged an error every 5 s: a root service can't
  reach the user's display. The tray agent, which runs in the user session,
  now mirrors the clipboard to the service while a client is connected.
  Connect notifications are shown the same way, and no longer delay the
  connection while `notify-send` runs.
- Frame-rate step-down logs split the encode time into upload and encode.
  Capture overrun logs split cursor/scanout from copy time.

# 0.9.19

Server-only release; no wire format change.

- KMS capture follows the compositor's flips instead of a fixed 120 Hz tick:
  the scanout framebuffer is polled every 1 ms and captured as soon as a new
  frame is committed. Content is 0.4 ms old when sampled on average (0.9 ms
  p99) instead of 3.2 ms (6.6 ms p99). A 144 Hz desktop streamed at 120 fps
  drops 1 flip in 6 rather than sampling between flips, and a game rendering
  below the stream rate sends each of its frames exactly once.
- Fix: the frame-rate gate added in 0.9.18 could drop a lone screen update
  that arrived shortly after the previous frame, leaving the client stale until
  the next change. Early frames are now held until the encoder is ready and
  replaced by newer ones, never dropped.
- Latency stats (client overlay) now start when the frame is sampled, so they
  include the capture readback.
- Bitrate climbs fast when the picture needs it: while the encoder is using
  its whole budget on a clean link, the target rises 20% every 2 s (20 to
  90 Mbps in about 25 s), instead of creeping up ~10% per probe. Simple
  content that fits its budget still probes slowly.
- Fix: every bitrate or frame-rate change rebuilt the Vulkan encoder with a
  14-frame self-test that competed with the live encoder, stalling the stream
  (p99 88 ms) and tripping the frame-rate controller into more rebuilds.
  Rebuilds of the running codec now skip it: p99 12 ms through a full climb.
- Each bitrate cut logs the client feedback that caused it.

# 0.9.18

Server-only release; no wire format change.

- NVIDIA KMS capture reads the scanout on a GPU copy engine instead of the 3D
  engine a game saturates. Each scanout buffer is imported once into a
  transfer-only Vulkan queue, copied in row bands to cached host memory and
  converted to NV12 on the CPU (AVX2/F16C, 4 threads) while the next band is
  still copying. 1440p FP16 scanout: 2.6 ms idle and 2.6 ms at 100% GPU load
  (the GL readback took 21-24 ms under load). Real KMS capture into the
  Vulkan encoder now holds 120 fps with the GPU saturated (47 fps before).
  `ST_KMS_VK_COPY=0` returns to the GL readback, which also still handles any
  buffer the copy engine can't import.
- Fix: adaptive frame rate never slowed KMS/X11/wlroots capture. The encoder
  was rebuilt at 90/60 fps while capture kept delivering 120, so each frame got
  a smaller bit budget and the rate kept bouncing (a rebuild and keyframe every
  few seconds). Capture now follows the rate live, frames above the encoder's
  rate are dropped for backends that can't retarget, and a mild shortfall must
  repeat before the rate steps down.
- Fix: under GPU load the encoder self-test could reject the last Vulkan codec
  and fall back to NVENC, which stalls far worse. The cost gate now only picks
  between Vulkan codecs.

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
