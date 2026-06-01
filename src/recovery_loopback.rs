//! Real-bitstream loss-injection integration test for the A1 Reed-Solomon FEC
//! recovery path.
//!
//! The synthetic-payload fuzz in `protocol/src/frame_assembler.rs` proves that
//! RS recovery is **byte-exact** over arbitrary bytes. It cannot prove the
//! stronger property that actually matters in production: that recovering lost
//! packets of a *real encoded IDR* yields an access unit the decoder can still
//! decode. A subtle shard-padding / length-prefix bug could round-trip random
//! `0xAA` bytes yet corrupt a slice header in a way only a real decoder rejects.
//!
//! This module closes that gap end-to-end on real FFmpeg codecs:
//!   encode (libx264 / NVENC) → real IDR access unit
//!   → `FrameSlicer` (RS mode) → drop N data packets
//!   → `FrameAssembler` (RS reconstruct)
//!   → assert byte-exact recovery AND that libavcodec's H.264 decoder (the same
//!     decoder `client/src/decode.rs` drives) produces a frame from the result.
//!
//! Per CLAUDE.md "probe ≠ correctness": the software-encoder test runs in CI on
//! every box (libx264 + the h264 decoder are always linked); the NVENC test is
//! `#[ignore]`d so it does not fail on non-NVIDIA machines and is run on demand
//! (`cargo test recovery_loopback -- --ignored`) on hardware. Validated live on
//! an RTX 4080 (driver 610.43.02, 2026-06-01).

#![cfg(test)]

use ffmpeg_sys_next as ffi;
use st_protocol::frame_assembler::FrameAssembler;
use st_protocol::frame_slicer::{FecConfig, FrameSlicer};
use st_protocol::packet::{frame_type, FecMode};
use st_protocol::{FrameTimingMeta, PacketHeader, PayloadType};
use std::collections::HashSet;
use std::ffi::CString;
use std::ptr;

/// Encode one deterministic, high-detail frame with `enc_name` and return the
/// first keyframe (IDR) access unit. Returns `None` when the encoder is not
/// available (e.g. NVENC on a non-NVIDIA box) so the caller can skip cleanly.
unsafe fn encode_idr_au(enc_name: &str, w: i32, h: i32) -> Option<Vec<u8>> {
    let name = CString::new(enc_name).unwrap();
    let codec = ffi::avcodec_find_encoder_by_name(name.as_ptr());
    if codec.is_null() {
        return None;
    }
    let ctx = ffi::avcodec_alloc_context3(codec);
    if ctx.is_null() {
        return None;
    }

    (*ctx).width = w;
    (*ctx).height = h;
    (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
    (*ctx).time_base = ffi::AVRational { num: 1, den: 30 };
    (*ctx).framerate = ffi::AVRational { num: 30, den: 1 };
    (*ctx).gop_size = 120;
    (*ctx).max_b_frames = 0;
    (*ctx).bit_rate = 6_000_000;

    if ffi::avcodec_open2(ctx, codec, ptr::null_mut()) < 0 {
        // Encoder present but cannot open (no GPU / driver mismatch) ⇒ skip.
        ffi::avcodec_free_context(&mut { ctx });
        return None;
    }

    let frame = ffi::av_frame_alloc();
    if frame.is_null() {
        ffi::avcodec_free_context(&mut { ctx });
        return None;
    }
    (*frame).width = w;
    (*frame).height = h;
    (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
    if ffi::av_frame_get_buffer(frame, 0) < 0 {
        ffi::av_frame_free(&mut { frame });
        ffi::avcodec_free_context(&mut { ctx });
        return None;
    }

    // High-spatial-frequency luma so the IDR spans many packets (residual-heavy);
    // mid-range chroma with variation. Deterministic — reproducible failures.
    let ys = (*frame).linesize[0] as usize;
    let yp = (*frame).data[0];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let v = ((x.wrapping_mul(37)) ^ (y.wrapping_mul(101)) ^ (x.wrapping_mul(y))) as u8;
            *yp.add(y * ys + x) = v;
        }
    }
    let us = (*frame).linesize[1] as usize;
    let up = (*frame).data[1];
    let vs = (*frame).linesize[2] as usize;
    let vp = (*frame).data[2];
    for y in 0..(h / 2) as usize {
        for x in 0..(w / 2) as usize {
            *up.add(y * us + x) = (128 + ((x ^ y) as i32 % 40 - 20)) as u8;
            *vp.add(y * vs + x) = (128 + ((x.wrapping_add(y)) as i32 % 40 - 20)) as u8;
        }
    }
    (*frame).pts = 0;

    // Send the frame, then flush (NULL) so the IDR is fully drained.
    let mut idr: Option<Vec<u8>> = None;
    if ffi::avcodec_send_frame(ctx, frame) >= 0 {
        ffi::avcodec_send_frame(ctx, ptr::null());
        let pkt = ffi::av_packet_alloc();
        if !pkt.is_null() {
            loop {
                let r = ffi::avcodec_receive_packet(ctx, pkt);
                if r == -ffi::EAGAIN || r == ffi::AVERROR_EOF || r < 0 {
                    break;
                }
                if idr.is_none() && ((*pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0 {
                    let bytes =
                        std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize).to_vec();
                    idr = Some(bytes);
                }
                ffi::av_packet_unref(pkt);
            }
            ffi::av_packet_free(&mut { pkt });
        }
    }

    ffi::av_frame_free(&mut { frame });
    ffi::avcodec_free_context(&mut { ctx });
    idr
}

/// Feed an H.264 access unit to libavcodec's `h264` decoder (the same decoder
/// the client uses) and report whether it produced at least one frame.
unsafe fn decode_au_yields_frame(au: &[u8]) -> bool {
    let name = CString::new("h264").unwrap();
    let dec = ffi::avcodec_find_decoder_by_name(name.as_ptr());
    if dec.is_null() {
        return false;
    }
    let ctx = ffi::avcodec_alloc_context3(dec);
    if ctx.is_null() {
        return false;
    }
    if ffi::avcodec_open2(ctx, dec, ptr::null_mut()) < 0 {
        ffi::avcodec_free_context(&mut { ctx });
        return false;
    }

    let pkt = ffi::av_packet_alloc();
    let frame = ffi::av_frame_alloc();
    let mut got = false;
    if !pkt.is_null() && !frame.is_null() {
        // av_new_packet allocates a padded buffer (AV_INPUT_BUFFER_PADDING_SIZE).
        if ffi::av_new_packet(pkt, au.len() as i32) >= 0 {
            ptr::copy_nonoverlapping(au.as_ptr(), (*pkt).data, au.len());
            if ffi::avcodec_send_packet(ctx, pkt) >= 0 {
                ffi::avcodec_send_packet(ctx, ptr::null()); // flush
                loop {
                    let r = ffi::avcodec_receive_frame(ctx, frame);
                    if r == -ffi::EAGAIN || r == ffi::AVERROR_EOF || r < 0 {
                        break;
                    }
                    if (*frame).width > 0 {
                        got = true;
                    }
                    ffi::av_frame_unref(frame);
                }
            }
        }
    }
    if !frame.is_null() {
        ffi::av_frame_free(&mut { frame });
    }
    if !pkt.is_null() {
        ffi::av_packet_free(&mut { pkt });
    }
    ffi::avcodec_free_context(&mut { ctx });
    got
}

/// Slice `au` with RS FEC at a small MTU (forces a multi-packet unit), drop
/// `want_drops` of the continuation data packets, then RS-reconstruct. Returns
/// the recovered access unit and its frame type, or `None` if the unit was not
/// multi-packet (cannot exercise FEC).
fn rs_recover_with_drop(au: &[u8], want_drops: usize) -> Option<(Vec<u8>, u8)> {
    let fec = FecConfig {
        mode: FecMode::Rs,
        fec_pct: 60,
        min_parity: 3,
    };
    // Small MTU so even a modest IDR splits into several packets.
    let mut slicer = FrameSlicer::with_config(600, fec);
    let (data, parity) =
        slicer.slice_with_meta_parts(au, 1, FrameTimingMeta::default(), frame_type::IDR);
    let data: Vec<Vec<u8>> = data.to_vec();
    let parity: Vec<Vec<u8>> = parity.to_vec();

    // Indices of continuation (Data) packets — never drop the FrameStart here so
    // we exercise pure mid-unit data loss.
    let data_idx: Vec<usize> = data
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            PacketHeader::deserialize(p)
                .map(|h| h.payload_type == PayloadType::Data)
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    if data_idx.is_empty() || parity.is_empty() {
        return None; // single-packet unit ⇒ no FEC to exercise
    }
    let drops = want_drops.min(parity.len()).min(data_idx.len());
    if drops == 0 {
        return None;
    }
    let drop_set: HashSet<usize> = data_idx.iter().rev().take(drops).copied().collect();

    let mut asm = FrameAssembler::new();
    let mut completed = None;
    for (i, p) in data.iter().enumerate() {
        if drop_set.contains(&i) {
            continue;
        }
        if let Some(f) = asm.ingest(p) {
            completed = Some(f);
        }
    }
    for p in &parity {
        if let Some(f) = asm.ingest(p) {
            completed = Some(f);
        }
    }
    completed.map(|c| (c.data, c.frame_type))
}

/// Shared assertion body: encode a real IDR with `enc_name`, drop two data
/// packets, RS-recover, and require the recovered AU to be byte-exact and
/// decodable.
fn assert_rs_recovers_decodable_idr(enc_name: &str) {
    let au = match unsafe { encode_idr_au(enc_name, 640, 480) } {
        Some(a) if !a.is_empty() => a,
        _ => {
            eprintln!("skipping recovery_loopback: encoder '{enc_name}' unavailable");
            return;
        }
    };

    // Sanity: the unmodified IDR decodes.
    assert!(
        unsafe { decode_au_yields_frame(&au) },
        "{enc_name}: original IDR did not decode — test setup is wrong"
    );

    let (recovered, ftype) = rs_recover_with_drop(&au, 2).unwrap_or_else(|| {
        panic!("{enc_name}: IDR was not multi-packet or RS failed to recover dropped packets")
    });

    assert_eq!(
        recovered, au,
        "{enc_name}: RS-recovered IDR is not byte-exact"
    );
    assert_eq!(
        ftype,
        frame_type::IDR,
        "{enc_name}: recovered frame_type should be IDR"
    );
    assert!(
        unsafe { decode_au_yields_frame(&recovered) },
        "{enc_name}: RS-recovered IDR failed to decode — recovery corrupted the bitstream"
    );
}

#[test]
fn rs_recovered_real_x264_idr_is_byte_exact_and_decodable() {
    // Hardware-free: libx264 encode + libavcodec h264 decode. Runs in CI.
    assert_rs_recovers_decodable_idr("libx264");
}

#[test]
#[ignore = "requires NVIDIA NVENC hardware; run with `--ignored` on a GPU box"]
fn rs_recovered_real_nvenc_idr_is_byte_exact_and_decodable() {
    // Live hardware path: h264_nvenc encode (RTX 4080) + libavcodec h264 decode.
    assert_rs_recovers_decodable_idr("h264_nvenc");
}
