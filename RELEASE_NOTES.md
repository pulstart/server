# 0.9.16

Deploy API server 0.1.2 first, then update hosts and clients (client 0.12.15).

- Read keys, candidates and requests from one atomic registration snapshot;
  poll every 250 ms while waiting instead of sleeping up to three seconds.
- Serialize STUN cache refresh to prevent concurrent socket readers.
- Honor macOS keyframe requests on the existing VideoToolbox session and remove
  the unconditional once-per-second keyframe burst.
- Update VideoToolbox average bitrate and optional one-second data-rate limit
  together; provide frame duration and back off after rejected updates.
- Let bitrate hysteresis follow low-bandwidth links without multi-megabit steps.

Native macOS CI exercises real VideoToolbox recovery frames and measures output
before/after a bitrate reduction. Linux unit suites cover signaling, recovery,
transport and rate control. Live NAT and macOS streaming remain separate checks.
