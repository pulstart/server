/// Opus audio encoding via FFmpeg's libopus encoder.
///
/// Float32 input → Opus frames.
/// Uses FFmpeg's libopus wrapper which supports multichannel (calls opus_multistream
/// internally for >2 channels).
use crate::audio::{capture::AudioSamples, recv_with_backlog_limit};
use crate::encode_config::AudioConfig;
use crate::transport::EncodedAudioPacket;
use crossbeam_channel::{Receiver, Sender};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

extern crate ffmpeg_next as ffmpeg;
extern crate ffmpeg_sys_next as ffi;

use std::ptr;

const DEFAULT_MAX_CAPTURE_BACKLOG: usize = 2;

/// RAII wrapper for FFmpeg codec context used for Opus encoding.
struct OpusEncoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    samples_per_frame: u32,
    channels: u32,
    pending_source_seqs: VecDeque<u64>,
    failed_after_submit: bool,
}

unsafe impl Send for OpusEncoder {}

fn expected_audio_packet_loss_pct() -> u32 {
    std::env::var("ST_AUDIO_PACKET_LOSS_PCT")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(5)
        .min(100)
}

fn opus_application_value(_expected_loss: u32) -> &'static str {
    // Default to RESTRICTED_LOWDELAY (matches Sunshine). The FEC options below
    // remain useful for packet sizes/modes that can emit LBRR, but default 5 ms
    // CELT packets cannot, so transport-level verbatim redundancy is the primary
    // loss protection. Explicit ST_AUDIO_OPUS_APPLICATION still overrides.
    match std::env::var("ST_AUDIO_OPUS_APPLICATION")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "voip" => "voip",
        "audio" => "audio",
        _ => "lowdelay",
    }
}

fn invalidate_post_submit_state(
    pending_source_seqs: &mut VecDeque<u64>,
    failed_after_submit: &mut bool,
) {
    pending_source_seqs.clear();
    *failed_after_submit = true;
}

/// -3 dB (≈0.7071) attenuation used for center/surround fold-down.
const M3DB: f32 = 0.707_106_77;

/// E4 MVP: fold an interleaved N-channel frame down to interleaved stereo
/// (ITU-R BS.775 coefficients). Channel order is assumed to be the
/// PulseAudio/ALSA default we capture with:
///   6ch (5.1): FL FR RL RR FC LFE
///   8ch (7.1): FL FR RL RR FC LFE SL SR
/// LFE is dropped (the stereo client has no bass management). Front L/R pass
/// through unchanged so the dominant energy is correct regardless of layout;
/// unknown counts fold the extra channels equally into both sides.
fn downmix_to_stereo(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 2 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames * 2);
    for f in 0..frames {
        let base = f * channels;
        let ch = |i: usize| interleaved.get(base + i).copied().unwrap_or(0.0);
        let mut l = ch(0); // FL
        let mut r = ch(1); // FR
        match channels {
            6 => {
                l += M3DB * ch(4) + M3DB * ch(2); // FC + RL
                r += M3DB * ch(4) + M3DB * ch(3); // FC + RR
            }
            8 => {
                l += M3DB * ch(4) + M3DB * ch(2) + M3DB * ch(6); // FC + RL + SL
                r += M3DB * ch(4) + M3DB * ch(3) + M3DB * ch(7); // FC + RR + SR
            }
            n => {
                for i in 2..n {
                    let s = M3DB * ch(i);
                    l += s;
                    r += s;
                }
            }
        }
        // Summed channels can exceed unity; clamp to avoid hard clipping.
        out.push(l.clamp(-1.0, 1.0));
        out.push(r.clamp(-1.0, 1.0));
    }
    out
}

impl OpusEncoder {
    fn new(config: &AudioConfig) -> Result<Self, String> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

        let codec_name = std::ffi::CString::new("libopus").unwrap();
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(codec_name.as_ptr()) };
        if codec.is_null() {
            return Err("libopus encoder not found (is FFmpeg built with libopus?)".into());
        }

        let ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 failed for opus".into());
        }

        // E4: encode at the (possibly downmixed) output channel count, not the
        // capture count — the stereo-only client rejects >2ch streams.
        let out_channels = config.output_channels();

        unsafe {
            (*ctx).sample_rate = config.sample_rate as i32;
            (*ctx).bit_rate = config.bitrate as i64;
            (*ctx).sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT;
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: config.sample_rate as i32,
            };
            (*ctx).frame_size = config.samples_per_frame() as i32;

            // FFmpeg's libopus derives the Opus packet size from its private
            // `frame_duration` option (default 20 ms), NOT from avctx->frame_size —
            // it overwrites frame_size during avcodec_open2. Setting only
            // frame_size left the encoder at 20 ms (960 samples) while we fed 5 ms
            // (240) frames, so every frame was rejected with "frame_size (960) was
            // not respected for a non-last frame" → no audio. Set frame_duration
            // (ms, matching samples_per_frame) so the E1 5 ms framing actually
            // encodes. (probe ≠ correctness — the config-level unit test missed
            // this because it never ran a real avcodec_send_frame.)
            let frame_ms = config.samples_per_frame() as f64 * 1000.0 / config.sample_rate as f64;
            let frame_duration_key = std::ffi::CString::new("frame_duration").unwrap();
            let frame_duration_val = std::ffi::CString::new(format!("{frame_ms}")).unwrap();
            ffi::av_opt_set(
                (*ctx).priv_data,
                frame_duration_key.as_ptr(),
                frame_duration_val.as_ptr(),
                0,
            );

            // Set channel layout based on channel count
            Self::set_channel_layout(ctx, out_channels);

            // Opus-specific options: low delay, CBR
            let expected_loss = expected_audio_packet_loss_pct();
            let application = std::ffi::CString::new("application").unwrap();
            let application_value =
                std::ffi::CString::new(opus_application_value(expected_loss)).unwrap();
            ffi::av_opt_set(
                (*ctx).priv_data,
                application.as_ptr(),
                application_value.as_ptr(),
                0,
            );

            let vbr_key = std::ffi::CString::new("vbr").unwrap();
            let vbr_off = std::ffi::CString::new("off").unwrap();
            ffi::av_opt_set((*ctx).priv_data, vbr_key.as_ptr(), vbr_off.as_ptr(), 0);

            if expected_loss > 0 {
                let packet_loss_key = std::ffi::CString::new("packet_loss").unwrap();
                let packet_loss_value = std::ffi::CString::new(expected_loss.to_string()).unwrap();
                ffi::av_opt_set(
                    (*ctx).priv_data,
                    packet_loss_key.as_ptr(),
                    packet_loss_value.as_ptr(),
                    0,
                );

                let fec_key = std::ffi::CString::new("fec").unwrap();
                let fec_on = std::ffi::CString::new("1").unwrap();
                ffi::av_opt_set((*ctx).priv_data, fec_key.as_ptr(), fec_on.as_ptr(), 0);
            }
        }

        let ret = unsafe { ffi::avcodec_open2(ctx, codec, ptr::null_mut()) };
        if ret < 0 {
            unsafe { ffi::avcodec_free_context(&mut { ctx }) };
            return Err(format!("Failed to open opus encoder: {}", ffmpeg_err(ret)));
        }

        // Allocate frame for input samples
        let frame = unsafe { ffi::av_frame_alloc() };
        if frame.is_null() {
            unsafe { ffi::avcodec_free_context(&mut { ctx }) };
            return Err("av_frame_alloc failed for opus".into());
        }

        let samples_per_frame = config.samples_per_frame();
        unsafe {
            (*frame).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT as i32;
            (*frame).sample_rate = config.sample_rate as i32;
            (*frame).nb_samples = samples_per_frame as i32;
            Self::set_frame_channel_layout(frame, out_channels);

            let ret = ffi::av_frame_get_buffer(frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { frame });
                ffi::avcodec_free_context(&mut { ctx });
                return Err(format!("av_frame_get_buffer failed: {}", ffmpeg_err(ret)));
            }
        }

        if out_channels != config.channels {
            println!(
                "[audio] downmixing {}ch capture -> {}ch output (E4; ST_AUDIO_DOWNMIX=0 to disable)",
                config.channels, out_channels
            );
        }
        println!(
            "[audio] Opus encoder initialized: {}ch, {}Hz, {}kbps, frame={}, app={}, fec={} packet_loss={}%",
            out_channels,
            config.sample_rate,
            config.bitrate / 1000,
            samples_per_frame,
            opus_application_value(expected_audio_packet_loss_pct()),
            if expected_audio_packet_loss_pct() > 0 {
                "on"
            } else {
                "off"
            },
            expected_audio_packet_loss_pct()
        );

        Ok(Self {
            ctx,
            frame,
            samples_per_frame,
            channels: out_channels,
            pending_source_seqs: VecDeque::new(),
            failed_after_submit: false,
        })
    }

    unsafe fn set_channel_layout(ctx: *mut ffi::AVCodecContext, channels: u32) {
        // FFmpeg 6+ uses AVChannelLayout
        let layout = match channels {
            2 => ffi::AV_CH_LAYOUT_STEREO,
            6 => ffi::AV_CH_LAYOUT_5POINT1,
            8 => ffi::AV_CH_LAYOUT_7POINT1,
            _ => ffi::AV_CH_LAYOUT_STEREO,
        };
        ffi::av_channel_layout_from_mask(&mut (*ctx).ch_layout, layout);
    }

    unsafe fn set_frame_channel_layout(frame: *mut ffi::AVFrame, channels: u32) {
        let layout = match channels {
            2 => ffi::AV_CH_LAYOUT_STEREO,
            6 => ffi::AV_CH_LAYOUT_5POINT1,
            8 => ffi::AV_CH_LAYOUT_7POINT1,
            _ => ffi::AV_CH_LAYOUT_STEREO,
        };
        ffi::av_channel_layout_from_mask(&mut (*frame).ch_layout, layout);
    }

    /// Encode a frame of interleaved float32 samples. Returns encoded Opus packets.
    fn encode(
        &mut self,
        samples: &[f32],
        pts: i64,
        source_seq: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        if self.failed_after_submit {
            return Err("Opus encoder requires reset after a post-submit failure".into());
        }
        let expected = (self.samples_per_frame * self.channels) as usize;
        if samples.len() < expected {
            return Err(format!(
                "Not enough samples: got {}, expected {expected}",
                samples.len()
            ));
        }

        unsafe {
            let ret = ffi::av_frame_make_writable(self.frame);
            if ret < 0 {
                return Err(format!(
                    "av_frame_make_writable failed: {}",
                    ffmpeg_err(ret)
                ));
            }

            // Copy interleaved float32 samples into the frame's data buffer.
            // FFmpeg's libopus with AV_SAMPLE_FMT_FLT uses interleaved format.
            let dst = (*self.frame).data[0] as *mut f32;
            ptr::copy_nonoverlapping(samples.as_ptr(), dst, expected);

            (*self.frame).pts = pts;
            (*self.frame).nb_samples = self.samples_per_frame as i32;
        }

        // Allocate before submission: once FFmpeg accepts a frame, every error
        // path must invalidate the sequence queue and replace this encoder.
        let pkt = unsafe { ffi::av_packet_alloc() };
        if pkt.is_null() {
            return Err("av_packet_alloc failed".into());
        }

        // Send frame to encoder
        let ret = unsafe { ffi::avcodec_send_frame(self.ctx, self.frame) };
        if ret < 0 {
            unsafe { ffi::av_packet_free(&mut { pkt }) };
            return Err(format!(
                "avcodec_send_frame (opus) failed: {}",
                ffmpeg_err(ret)
            ));
        }
        self.pending_source_seqs.push_back(source_seq);

        // Receive encoded packets
        let mut packets = Vec::new();
        unsafe {
            loop {
                let ret = ffi::avcodec_receive_packet(self.ctx, pkt);
                if ret == -ffi::EAGAIN || ret == ffi::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    ffi::av_packet_free(&mut { pkt });
                    invalidate_post_submit_state(
                        &mut self.pending_source_seqs,
                        &mut self.failed_after_submit,
                    );
                    return Err(format!(
                        "avcodec_receive_packet (opus) failed: {}",
                        ffmpeg_err(ret)
                    ));
                }
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                let Some(packet_source_seq) = self.pending_source_seqs.pop_front() else {
                    ffi::av_packet_unref(pkt);
                    ffi::av_packet_free(&mut { pkt });
                    invalidate_post_submit_state(
                        &mut self.pending_source_seqs,
                        &mut self.failed_after_submit,
                    );
                    return Err("libopus produced a packet without a source frame".into());
                };
                packets.push((packet_source_seq, data.to_vec()));
                ffi::av_packet_unref(pkt);
            }
            ffi::av_packet_free(&mut { pkt });
        }

        Ok(packets)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.frame.is_null() {
                ffi::av_frame_free(&mut self.frame);
            }
            if !self.ctx.is_null() {
                ffi::avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

/// Audio encoding thread: consumes `AudioSamples`, produces `EncodedAudioPacket`.
pub fn run_encode_thread(
    config: AudioConfig,
    sample_rx: Receiver<AudioSamples>,
    packet_tx: Sender<EncodedAudioPacket>,
    running: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        crate::audio::set_realtime_priority("encode");
        let mut encoder = match OpusEncoder::new(&config) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[audio] Failed to create Opus encoder: {e}");
                return;
            }
        };

        let out_channels = config.output_channels();
        let mut pts: i64 = 0;
        let trace = std::env::var_os("ST_TRACE").is_some();
        let max_capture_backlog = std::env::var("ST_AUDIO_MAX_CAPTURE_BACKLOG")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .map(|value| value.clamp(1, 8))
            .unwrap_or(DEFAULT_MAX_CAPTURE_BACKLOG);
        let mut backlog_logs = 0usize;

        while running.load(Ordering::SeqCst) {
            let (samples, dropped_frames) =
                match recv_with_backlog_limit(&sample_rx, max_capture_backlog) {
                    Ok(received) => received,
                    Err(_) => break, // Channel closed
                };
            if dropped_frames > 0 {
                pts += dropped_frames as i64 * config.samples_per_frame() as i64;
                if trace && backlog_logs < 12 {
                    eprintln!(
                        "[trace][audio] encoder dropped {} stale capture frame(s)",
                        dropped_frames
                    );
                    backlog_logs += 1;
                }
            }

            // Validate sample metadata matches encoder config
            if samples.channels != config.channels || samples.sample_rate != config.sample_rate {
                eprintln!(
                    "[audio] Sample mismatch: got {}ch/{}Hz, expected {}ch/{}Hz",
                    samples.channels, samples.sample_rate, config.channels, config.sample_rate
                );
                continue;
            }

            // E4: fold surround capture down to the encoder's output channels.
            let downmixed;
            let frame_samples: &[f32] = if samples.channels != out_channels {
                downmixed = downmix_to_stereo(&samples.data, samples.channels as usize);
                &downmixed
            } else {
                &samples.data
            };

            match encoder.encode(frame_samples, pts, samples.source_seq) {
                Ok(packets) => {
                    for (source_seq, pkt_data) in packets {
                        let packet = EncodedAudioPacket {
                            source_seq,
                            data: pkt_data,
                        };
                        if packet_tx.send(packet).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[audio] Opus encode error: {e}");
                    if encoder.failed_after_submit {
                        encoder = match OpusEncoder::new(&config) {
                            Ok(encoder) => encoder,
                            Err(reset_err) => {
                                eprintln!(
                                    "[audio] Failed to reset Opus encoder after submit error: {reset_err}"
                                );
                                return;
                            }
                        };
                    }
                }
            }

            pts += config.samples_per_frame() as i64;
        }

        println!("[audio] Encode thread exited");
    })
}

fn ffmpeg_err(code: i32) -> String {
    let mut buf = [0u8; 256];
    unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr() as *mut i8, buf.len());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn downmix_stereo_passthrough() {
        let s = [0.1, -0.2, 0.3, -0.4];
        assert_eq!(downmix_to_stereo(&s, 2), s.to_vec());
    }

    #[test]
    fn encoder_backlog_drop_preserves_captured_source_gap() {
        let (tx, rx) = unbounded();
        let samples = |source_seq| AudioSamples {
            source_seq,
            data: vec![0.0; 2],
            channels: 2,
            sample_rate: 48_000,
        };

        tx.send(samples(30)).unwrap();
        let (first, dropped) = recv_with_backlog_limit(&rx, 1).unwrap();
        assert_eq!(first.source_seq, 30);
        assert_eq!(dropped, 0);

        for seq in 31..=34 {
            tx.send(samples(seq)).unwrap();
        }
        let (next, dropped) = recv_with_backlog_limit(&rx, 1).unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(next.source_seq, 33);
        assert_eq!(next.source_seq - first.source_seq, 3);
    }

    #[test]
    fn post_submit_failure_clears_pending_sequences_and_requires_reset() {
        let mut pending = VecDeque::from([10, 11, 12]);
        let mut failed = false;

        invalidate_post_submit_state(&mut pending, &mut failed);

        assert!(pending.is_empty());
        assert!(failed);
    }

    // Regression guard for the E1 5 ms framing: FFmpeg's libopus must be told the
    // frame duration via the `frame_duration` private option, else it stays at
    // its 20 ms default and rejects our shorter frames ("frame_size (960) was not
    // respected for a non-last frame"), which silently kills ALL audio. This
    // drives a real avcodec_send_frame round-trip — the thing the config-level
    // test never did, which is why the bug shipped.
    #[test]
    fn opus_encoder_accepts_configured_frame_size() {
        let config = AudioConfig::stereo();
        let mut enc = match OpusEncoder::new(&config) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("libopus unavailable ({e}); skipping");
                return;
            }
        };
        let n = config.total_samples_per_frame();
        let samples = vec![0.0f32; n];
        let mut packets = 0usize;
        for i in 0..5i64 {
            let pkts = enc
                .encode(&samples, i * config.samples_per_frame() as i64, i as u64)
                .expect("opus rejected the configured frame size (frame_duration not wired?)");
            packets += pkts.len();
        }
        assert!(
            packets > 0,
            "no opus packets produced at the configured frame size"
        );
    }

    #[test]
    fn delayed_opus_output_keeps_its_source_sequence_gap() {
        let config = AudioConfig::stereo();
        let mut enc = match OpusEncoder::new(&config) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("libopus unavailable ({e}); skipping");
                return;
            }
        };
        let samples = vec![0.0f32; config.total_samples_per_frame()];
        let mut output_seqs = Vec::new();
        for (i, source_seq) in [10, 12, 13].into_iter().enumerate() {
            output_seqs.extend(
                enc.encode(
                    &samples,
                    i as i64 * config.samples_per_frame() as i64,
                    source_seq,
                )
                .unwrap()
                .into_iter()
                .map(|(seq, _)| seq),
            );
        }

        assert!(output_seqs.len() >= 2, "libopus produced too little output");
        assert_eq!(&output_seqs[..2], &[10, 12]);
    }

    #[test]
    fn downmix_5_1_preserves_front_and_folds_center_rear() {
        // One frame, ALSA 5.1 order: FL FR RL RR FC LFE.
        let fl = 0.20;
        let fr = -0.10;
        let rl = 0.04;
        let rr = 0.06;
        let fc = 0.10;
        let lfe = 0.90; // dropped entirely
        let frame = [fl, fr, rl, rr, fc, lfe];
        let out = downmix_to_stereo(&frame, 6);
        assert_eq!(out.len(), 2);
        let exp_l = fl + M3DB * fc + M3DB * rl;
        let exp_r = fr + M3DB * fc + M3DB * rr;
        assert!((out[0] - exp_l).abs() < 1e-6, "L {} != {}", out[0], exp_l);
        assert!((out[1] - exp_r).abs() < 1e-6, "R {} != {}", out[1], exp_r);
        // LFE must not leak into either channel.
        assert!(out[0] < 0.5 && out[1] < 0.5);
    }

    #[test]
    fn downmix_7_1_folds_side_channels() {
        // ALSA 7.1 order: FL FR RL RR FC LFE SL SR.
        let frame = [0.10, 0.20, 0.01, 0.02, 0.04, 0.99, 0.03, 0.05];
        let out = downmix_to_stereo(&frame, 8);
        let exp_l = 0.10 + M3DB * 0.04 + M3DB * 0.01 + M3DB * 0.03;
        let exp_r = 0.20 + M3DB * 0.04 + M3DB * 0.02 + M3DB * 0.05;
        assert!((out[0] - exp_l).abs() < 1e-6);
        assert!((out[1] - exp_r).abs() < 1e-6);
    }

    #[test]
    fn downmix_clamps_to_unit_range() {
        // Loud surround sums must not exceed [-1, 1].
        let frame = [0.9, 0.9, 0.9, 0.9, 0.9, 0.0];
        let out = downmix_to_stereo(&frame, 6);
        assert!(out.iter().all(|&v| (-1.0..=1.0).contains(&v)));
    }

    #[test]
    fn downmix_handles_multiple_frames() {
        // Two 5.1 frames -> two stereo frames.
        let frame: Vec<f32> = [0.1f32, 0.2, 0.0, 0.0, 0.0, 0.0]
            .iter()
            .chain([0.3f32, 0.4, 0.0, 0.0, 0.0, 0.0].iter())
            .copied()
            .collect();
        let out = downmix_to_stereo(&frame, 6);
        assert_eq!(out.len(), 4);
        assert!((out[0] - 0.1).abs() < 1e-6);
        assert!((out[3] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn output_channels_downmixes_surround_by_default() {
        let cfg = AudioConfig::surround51();
        assert_eq!(cfg.output_channels(), 2);
        assert!(cfg.downmix_to_stereo_enabled());
        let stereo = AudioConfig::stereo();
        assert_eq!(stereo.output_channels(), 2);
        assert!(!stereo.downmix_to_stereo_enabled());
    }
}
