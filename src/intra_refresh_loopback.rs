//! Real-bitstream loss-injection convergence test for the A3 intra-refresh
//! recovery path.
//!
//! CLAUDE.md §9 requires that any loss-recovery feature default-on **only after**
//! a regression-guard test exercises the real failure class. For A3 that class is
//! literally "packet-loss-injection recovery **without** an IDR": with periodic
//! intra-refresh (PIR) the encoder emits a single startup IDR and then *never* a
//! mid-stream IDR — instead a vertical band of intra blocks sweeps the frame over
//! one refresh period, so a decoder that loses a frame re-converges to the correct
//! picture within that period with no full-IDR bitrate spike.
//!
//! This module proves that property end-to-end on the real software encoder +
//! the same libavcodec H.264 decoder the client drives:
//!   encode N frames of changing content with x264 `intra-refresh=1`
//!   → assert exactly ONE IDR in the whole stream (PIR really replaced IDRs)
//!   → decode the clean stream → per-frame reference luma
//!   → decode again but DROP one mid-stream P-frame (the client's recovery model:
//!     skip the lost access unit, keep feeding)
//!   → assert the lossy output (a) visibly diverges right after the loss, then
//!     (b) re-converges to the clean reference within one refresh period — with no
//!     IDR in between.
//!
//! Hardware-free: libx264 + the h264 decoder are always linked, so this runs in
//! CI on every box (no GPU needed — NVENC intra-refresh emits no `recovery_point`
//! SEI via FFmpeg and is intentionally left IDR-based, see `encode.rs`).

#![cfg(test)]

use ffmpeg_sys_next as ffi;
use std::ffi::CString;
use std::ptr;

const W: i32 = 128;
const H: i32 = 128;
/// Refresh period in frames. With x264 PIR the intra band completes one sweep
/// every `keyint` frames, so this doubles as the encoder's `gop_size`.
const PERIOD: usize = 10;
const FRAMES: usize = 40;
/// Index of the P-frame we drop to simulate loss.
const DROP_IDX: usize = 12;

/// One decoded luma plane, tightly packed `W*H` (stride removed).
type LumaPlane = Vec<u8>;

/// Fill `frame`'s YUV420P planes with a smooth pattern that *pans* horizontally
/// with `f`. This matters for the test's discriminating power: smooth content
/// that translates by whole pixels is near-perfectly inter-predictable, so x264
/// codes it almost entirely as motion vectors with very few intra macroblocks.
/// A lost reference therefore does **not** self-heal from incidental intra MBs —
/// the only thing that restores the picture is the periodic intra-refresh band.
/// (An earlier high-frequency/noisy pattern self-healed within ~6 frames even
/// with PIR disabled, because ~36% of its P-frame MBs were intra-coded, which
/// made the test pass vacuously.) Chroma is held constant (pure skip) for the
/// same reason.
unsafe fn fill_frame(frame: *mut ffi::AVFrame, f: usize) {
    let ys = (*frame).linesize[0] as usize;
    let yp = (*frame).data[0];
    let phase = f as f64 * 3.0; // pan 3 px/frame
    for y in 0..H as usize {
        // Slow vertical component so the frame is genuinely 2D, not a 1D ramp.
        let yc = (y as f64 * 0.04).sin() * 18.0;
        for x in 0..W as usize {
            let v = 128.0 + 100.0 * ((x as f64 + phase) * 0.08).sin() + yc;
            *yp.add(y * ys + x) = v.clamp(0.0, 255.0) as u8;
        }
    }
    let us = (*frame).linesize[1] as usize;
    let up = (*frame).data[1];
    let vs = (*frame).linesize[2] as usize;
    let vp = (*frame).data[2];
    for y in 0..(H / 2) as usize {
        for x in 0..(W / 2) as usize {
            *up.add(y * us + x) = 128;
            *vp.add(y * vs + x) = 128;
        }
    }
}

/// Whether an Annex-B access unit contains any IDR slice NAL (nal_unit_type 5).
///
/// FFmpeg flags the *recovery-point* AUs of an intra-refresh stream with
/// `AV_PKT_FLAG_KEY` (they are seek points), so the packet flag over-counts. The
/// real "no mid-stream IDR" property is about IDR slices in the bitstream: PIR
/// emits exactly one IDR access unit (the startup frame) and recovers via the
/// moving intra band thereafter. We test at access-unit granularity because a
/// single IDR frame may be split into several IDR-slice NALs (each type 5). The
/// scan tolerates both 3- and 4-byte start codes; H.264 emulation-prevention
/// guarantees `00 00 01` never appears inside a NAL payload.
fn contains_idr_nal(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1F == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Encode `FRAMES` frames with x264 periodic intra-refresh. Returns each frame's
/// access unit. `None` when libx264 is unavailable so CI without it skips cleanly.
unsafe fn encode_intra_refresh_sequence() -> Option<Vec<Vec<u8>>> {
    let name = CString::new("libx264").unwrap();
    let codec = ffi::avcodec_find_encoder_by_name(name.as_ptr());
    if codec.is_null() {
        return None;
    }
    let ctx = ffi::avcodec_alloc_context3(codec);
    if ctx.is_null() {
        return None;
    }

    (*ctx).width = W;
    (*ctx).height = H;
    (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
    (*ctx).time_base = ffi::AVRational { num: 1, den: 30 };
    (*ctx).framerate = ffi::AVRational { num: 30, den: 1 };
    // PIR sweep period == keyint. No B-frames, single reference: the low-latency
    // game-streaming config under which A3 ships.
    (*ctx).gop_size = PERIOD as i32;
    (*ctx).max_b_frames = 0;
    (*ctx).bit_rate = 2_000_000;

    // tune=zerolatency for 1:1 send→receive; intra-refresh=1 enables PIR.
    let preset = CString::new("preset").unwrap();
    let veryfast = CString::new("veryfast").unwrap();
    ffi::av_opt_set((*ctx).priv_data, preset.as_ptr(), veryfast.as_ptr(), 0);
    let tune = CString::new("tune").unwrap();
    let zl = CString::new("zerolatency").unwrap();
    ffi::av_opt_set((*ctx).priv_data, tune.as_ptr(), zl.as_ptr(), 0);
    let x264_params = CString::new("x264-params").unwrap();
    let params = CString::new("intra-refresh=1:scenecut=0:repeat-headers=1").unwrap();
    ffi::av_opt_set((*ctx).priv_data, x264_params.as_ptr(), params.as_ptr(), 0);

    if ffi::avcodec_open2(ctx, codec, ptr::null_mut()) < 0 {
        ffi::avcodec_free_context(&mut { ctx });
        return None;
    }

    let frame = ffi::av_frame_alloc();
    if frame.is_null() {
        ffi::avcodec_free_context(&mut { ctx });
        return None;
    }
    (*frame).width = W;
    (*frame).height = H;
    (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
    if ffi::av_frame_get_buffer(frame, 0) < 0 {
        ffi::av_frame_free(&mut { frame });
        ffi::avcodec_free_context(&mut { ctx });
        return None;
    }

    let pkt = ffi::av_packet_alloc();
    let mut aus: Vec<Vec<u8>> = Vec::with_capacity(FRAMES);

    let drain = |aus: &mut Vec<Vec<u8>>| loop {
        let r = ffi::avcodec_receive_packet(ctx, pkt);
        if r == -ffi::EAGAIN || r == ffi::AVERROR_EOF || r < 0 {
            break;
        }
        let bytes = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize).to_vec();
        aus.push(bytes);
        ffi::av_packet_unref(pkt);
    };

    for f in 0..FRAMES {
        if ffi::av_frame_make_writable(frame) < 0 {
            break;
        }
        fill_frame(frame, f);
        (*frame).pts = f as i64;
        if ffi::avcodec_send_frame(ctx, frame) < 0 {
            break;
        }
        drain(&mut aus);
    }
    ffi::avcodec_send_frame(ctx, ptr::null()); // flush
    drain(&mut aus);

    ffi::av_packet_free(&mut { pkt });
    ffi::av_frame_free(&mut { frame });
    ffi::avcodec_free_context(&mut { ctx });

    if aus.len() < FRAMES {
        return None; // encoder produced fewer AUs than frames — cannot test
    }
    Some(aus)
}

/// Copy a decoded frame's luma into a tightly packed `W*H` buffer.
unsafe fn extract_luma(frame: *mut ffi::AVFrame) -> LumaPlane {
    let stride = (*frame).linesize[0] as usize;
    let src = (*frame).data[0];
    let mut out = vec![0u8; (W * H) as usize];
    for y in 0..H as usize {
        ptr::copy_nonoverlapping(
            src.add(y * stride),
            out.as_mut_ptr().add(y * W as usize),
            W as usize,
        );
    }
    out
}

/// Decode `aus` with the libavcodec `h264` decoder, optionally skipping the AU at
/// `drop_idx` (loss). Returns `(input_index, luma)` pairs in decode order. The
/// decoder is configured to keep emitting concealed/corrupt frames after a loss
/// (`AV_CODEC_FLAG_OUTPUT_CORRUPT` + error concealment) so the divergence is
/// observable rather than the frame being silently dropped.
unsafe fn decode_luma_sequence(
    aus: &[Vec<u8>],
    drop_idx: Option<usize>,
) -> Vec<(usize, LumaPlane)> {
    let name = CString::new("h264").unwrap();
    let dec = ffi::avcodec_find_decoder_by_name(name.as_ptr());
    if dec.is_null() {
        return Vec::new();
    }
    let ctx = ffi::avcodec_alloc_context3(dec);
    if ctx.is_null() {
        return Vec::new();
    }
    (*ctx).flags |= ffi::AV_CODEC_FLAG_OUTPUT_CORRUPT as i32;
    (*ctx).error_concealment = ffi::FF_EC_GUESS_MVS | ffi::FF_EC_DEBLOCK;
    if ffi::avcodec_open2(ctx, dec, ptr::null_mut()) < 0 {
        ffi::avcodec_free_context(&mut { ctx });
        return Vec::new();
    }

    let pkt = ffi::av_packet_alloc();
    let frame = ffi::av_frame_alloc();
    let mut out: Vec<(usize, LumaPlane)> = Vec::new();

    for (idx, au) in aus.iter().enumerate() {
        if Some(idx) == drop_idx {
            continue; // simulate a lost access unit
        }
        if ffi::av_new_packet(pkt, au.len() as i32) < 0 {
            continue;
        }
        ptr::copy_nonoverlapping(au.as_ptr(), (*pkt).data, au.len());
        // Zero-latency stream: one decoded frame per fed packet, in order.
        if ffi::avcodec_send_packet(ctx, pkt) >= 0 {
            loop {
                let r = ffi::avcodec_receive_frame(ctx, frame);
                if r == -ffi::EAGAIN || r == ffi::AVERROR_EOF || r < 0 {
                    break;
                }
                if (*frame).width == W && (*frame).height == H {
                    out.push((idx, extract_luma(frame)));
                }
                ffi::av_frame_unref(frame);
            }
        }
        ffi::av_packet_unref(pkt);
    }
    // Flush.
    ffi::avcodec_send_packet(ctx, ptr::null());
    loop {
        let r = ffi::avcodec_receive_frame(ctx, frame);
        if r == -ffi::EAGAIN || r == ffi::AVERROR_EOF || r < 0 {
            break;
        }
        ffi::av_frame_unref(frame);
    }

    ffi::av_frame_free(&mut { frame });
    ffi::av_packet_free(&mut { pkt });
    ffi::avcodec_free_context(&mut { ctx });
    out
}

/// Mean absolute luma difference between two same-size planes.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let sum: u64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs() as u64)
        .sum();
    sum as f64 / a.len() as f64
}

#[test]
fn intra_refresh_recovers_within_one_period_without_idr() {
    let aus = match unsafe { encode_intra_refresh_sequence() } {
        Some(v) => v,
        None => {
            eprintln!("skipping intra_refresh_loopback: libx264 unavailable or short stream");
            return;
        }
    };

    // (1) PIR replaced IDRs: exactly one IDR access unit (the startup frame) in
    // the whole stream. If x264 silently fell back to periodic IDRs this is > 1
    // and the recovery model under test isn't actually active.
    let idr_aus = aus.iter().filter(|au| contains_idr_nal(au)).count();
    assert_eq!(
        idr_aus, 1,
        "intra-refresh must emit exactly one startup IDR access unit and recover via PIR, got {idr_aus}"
    );

    let clean = unsafe { decode_luma_sequence(&aus, None) };
    let lossy = unsafe { decode_luma_sequence(&aus, Some(DROP_IDX)) };
    assert!(
        clean.len() >= FRAMES - 1,
        "clean decode produced too few frames: {}",
        clean.len()
    );
    assert!(!lossy.is_empty(), "lossy decode produced no frames");

    // Index clean output by input frame index for aligned comparison.
    let mut clean_by_idx = std::collections::HashMap::new();
    for (idx, plane) in &clean {
        clean_by_idx.insert(*idx, plane);
    }

    // Per-input-frame divergence of the lossy decode from the clean reference,
    // for frames after the loss.
    let mut diffs: Vec<(usize, f64)> = Vec::new();
    for (idx, plane) in &lossy {
        if *idx > DROP_IDX {
            if let Some(reference) = clean_by_idx.get(idx) {
                diffs.push((*idx, mean_abs_diff(plane, reference)));
            }
        }
    }
    assert!(
        !diffs.is_empty(),
        "no post-loss frames to compare — decoder dropped everything after the loss"
    );

    // (2) The loss must visibly corrupt the picture right after it (otherwise the
    // test isn't exercising real reference damage).
    let post_loss_peak = diffs
        .iter()
        .filter(|(idx, _)| *idx <= DROP_IDX + 4)
        .map(|(_, d)| *d)
        .fold(0.0_f64, f64::max);
    assert!(
        post_loss_peak > 3.0,
        "loss did not visibly corrupt decode (peak diff {post_loss_peak:.2}); test not exercising damage"
    );

    // (3) The PIR sweep must restore the picture — and, once restored, it must
    // STAY restored through the end of the stream (a momentary dip to 0 that
    // re-diverges would not be real recovery). So locate the *last* frame that
    // still diverges and require: (a) it exists at all only because of the loss,
    // (b) recovery completes within a generous bound (two refresh periods — the
    // band sweep plus a few frames for motion-propagation to settle; exact timing
    // is encoder-version-sensitive, so we don't pin the single frame), and
    // (c) every frame after it matches the clean reference. With idr_aus == 1
    // (asserted above) this recovery provably happened with NO intervening IDR.
    const CONVERGED: f64 = 1.0;
    let recovery_bound = DROP_IDX + 2 * PERIOD;
    let last_diverged = diffs
        .iter()
        .filter(|(_, d)| *d > CONVERGED)
        .map(|(idx, _)| *idx)
        .max();
    // The negative control (PIR disabled / infinite GOP) never refreshes, so the
    // tail stays diverged and `last_diverged` runs to the final frame — caught
    // here. The positive PIR stream converges and stays converged.
    let last_diverged = last_diverged.unwrap_or_else(|| {
        panic!("loss never corrupted decode past the convergence threshold — test not exercising damage")
    });
    assert!(
        last_diverged <= recovery_bound,
        "PIR did not recover without an IDR: still diverging at frame {last_diverged} (bound {recovery_bound})"
    );
    // Sanity: there are post-recovery frames actually compared (not vacuous).
    assert!(
        diffs.iter().any(|(idx, _)| *idx > last_diverged),
        "no converged frames after recovery at {last_diverged} — cannot confirm sustained recovery"
    );
}
