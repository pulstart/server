/// Opus audio encoding via FFmpeg's libopus encoder.
///
/// Matches Sunshine's audio encoding pipeline: float32 input → Opus frames.
/// Uses FFmpeg's libopus wrapper which supports multichannel (calls opus_multistream
/// internally for >2 channels).
use crate::audio::capture::AudioSamples;
use crate::encode_config::AudioConfig;
use crossbeam_channel::{Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

extern crate ffmpeg_next as ffmpeg;
extern crate ffmpeg_sys_next as ffi;

use std::ptr;

/// RAII wrapper for FFmpeg codec context used for Opus encoding.
struct OpusEncoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    samples_per_frame: u32,
    channels: u32,
}

unsafe impl Send for OpusEncoder {}

fn expected_audio_packet_loss_pct() -> u32 {
    std::env::var("ST_AUDIO_PACKET_LOSS_PCT")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(5)
        .min(100)
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

        unsafe {
            (*ctx).sample_rate = config.sample_rate as i32;
            (*ctx).bit_rate = config.bitrate as i64;
            (*ctx).sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT;
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: config.sample_rate as i32,
            };
            (*ctx).frame_size = config.samples_per_frame() as i32;

            // Set channel layout based on channel count
            Self::set_channel_layout(ctx, config.channels);

            // Opus-specific options: low delay, CBR
            let application = std::ffi::CString::new("application").unwrap();
            let restricted = std::ffi::CString::new("lowdelay").unwrap();
            ffi::av_opt_set(
                (*ctx).priv_data,
                application.as_ptr(),
                restricted.as_ptr(),
                0,
            );

            let vbr_key = std::ffi::CString::new("vbr").unwrap();
            let vbr_off = std::ffi::CString::new("off").unwrap();
            ffi::av_opt_set((*ctx).priv_data, vbr_key.as_ptr(), vbr_off.as_ptr(), 0);

            let expected_loss = expected_audio_packet_loss_pct();
            if expected_loss > 0 {
                let packet_loss_key = std::ffi::CString::new("packet_loss").unwrap();
                let packet_loss_value =
                    std::ffi::CString::new(expected_loss.to_string()).unwrap();
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
            Self::set_frame_channel_layout(frame, config.channels);

            let ret = ffi::av_frame_get_buffer(frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { frame });
                ffi::avcodec_free_context(&mut { ctx });
                return Err(format!("av_frame_get_buffer failed: {}", ffmpeg_err(ret)));
            }
        }

        println!(
            "[audio] Opus encoder initialized: {}ch, {}Hz, {}kbps, frame={}, fec={} packet_loss={}%",
            config.channels,
            config.sample_rate,
            config.bitrate / 1000,
            samples_per_frame,
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
            channels: config.channels,
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
    fn encode(&mut self, samples: &[f32], pts: i64) -> Result<Vec<Vec<u8>>, String> {
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

        // Send frame to encoder
        let ret = unsafe { ffi::avcodec_send_frame(self.ctx, self.frame) };
        if ret < 0 {
            return Err(format!(
                "avcodec_send_frame (opus) failed: {}",
                ffmpeg_err(ret)
            ));
        }

        // Receive encoded packets
        let mut packets = Vec::new();
        unsafe {
            let pkt = ffi::av_packet_alloc();
            if pkt.is_null() {
                return Err("av_packet_alloc failed".into());
            }

            loop {
                let ret = ffi::avcodec_receive_packet(self.ctx, pkt);
                if ret == -ffi::EAGAIN || ret == ffi::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    ffi::av_packet_free(&mut { pkt });
                    return Err(format!(
                        "avcodec_receive_packet (opus) failed: {}",
                        ffmpeg_err(ret)
                    ));
                }
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                packets.push(data.to_vec());
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

/// Encoded audio packet ready for transport.
pub struct EncodedAudioPacket {
    pub data: Vec<u8>,
}

/// Audio encoding thread: consumes `AudioSamples`, produces `EncodedAudioPacket`.
pub fn run_encode_thread(
    config: AudioConfig,
    sample_rx: Receiver<AudioSamples>,
    packet_tx: Sender<EncodedAudioPacket>,
    running: Arc<AtomicBool>,
) {
    let handle = thread::spawn(move || {
        let mut encoder = match OpusEncoder::new(&config) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[audio] Failed to create Opus encoder: {e}");
                return;
            }
        };

        let mut pts: i64 = 0;

        while running.load(Ordering::SeqCst) {
            let samples = match sample_rx.recv() {
                Ok(s) => s,
                Err(_) => break, // Channel closed
            };

            // Validate sample metadata matches encoder config
            if samples.channels != config.channels || samples.sample_rate != config.sample_rate {
                eprintln!(
                    "[audio] Sample mismatch: got {}ch/{}Hz, expected {}ch/{}Hz",
                    samples.channels, samples.sample_rate, config.channels, config.sample_rate
                );
                continue;
            }

            match encoder.encode(&samples.data, pts) {
                Ok(packets) => {
                    for pkt_data in packets {
                        let packet = EncodedAudioPacket { data: pkt_data };
                        if packet_tx.send(packet).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[audio] Opus encode error: {e}");
                }
            }

            pts += config.samples_per_frame() as i64;
        }

        println!("[audio] Encode thread exited");
    });

    // Detach — lifetime managed by `running` flag
    drop(handle);
}

fn ffmpeg_err(code: i32) -> String {
    let mut buf = [0u8; 256];
    unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr() as *mut i8, buf.len());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).to_string()
}
