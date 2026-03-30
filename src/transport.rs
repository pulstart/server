use st_protocol::packet::{AudioRedundancyMeta, AUDIO_REDUNDANCY_HEADER_SIZE, HEADER_SIZE};
use st_protocol::reliable_udp::PunchedSocket;
use st_protocol::tunnel::{CryptoContext, CRYPTO_OVERHEAD};
use st_protocol::{FrameSlicer, FrameTimingMeta, PacketHeader, PayloadType};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
#[cfg(unix)]
use std::{mem, os::fd::AsRawFd};

const LOOPBACK_MAX_UDP: usize = 1400;
const SAFE_NETWORK_MAX_UDP: usize = 1200;
#[cfg(unix)]
const DEFAULT_UDP_SEND_BUFFER: i32 = 1024 * 1024;
#[cfg(target_os = "linux")]
const DEFAULT_UDP_SO_PRIORITY: i32 = 5;

#[cfg(unix)]
fn configured_udp_send_buffer() -> i32 {
    std::env::var("ST_UDP_SNDBUF")
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_UDP_SEND_BUFFER)
}

#[cfg(unix)]
fn configured_udp_dscp() -> Option<u8> {
    std::env::var("ST_UDP_DSCP")
        .ok()
        .and_then(|raw| raw.parse::<u8>().ok())
        .filter(|value| *value <= 63)
}

fn audio_redundancy_enabled() -> bool {
    std::env::var("ST_AUDIO_REDUNDANCY")
        .map(|raw| raw != "0")
        .unwrap_or(true)
}

#[cfg(target_os = "linux")]
fn configured_udp_priority() -> Option<i32> {
    Some(
        std::env::var("ST_UDP_SO_PRIORITY")
            .ok()
            .and_then(|raw| raw.parse::<i32>().ok())
            .filter(|value| *value >= 0)
            .unwrap_or(DEFAULT_UDP_SO_PRIORITY),
    )
}

#[cfg(unix)]
fn set_udp_socket_int_opt(
    socket: &UdpSocket,
    level: libc::c_int,
    optname: libc::c_int,
    value: libc::c_int,
) -> std::io::Result<()> {
    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            optname,
            &value as *const _ as *const _,
            mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn configure_direct_udp_socket(socket: &UdpSocket, client_addr: SocketAddr) {
    #[cfg(unix)]
    {
        let _ = set_udp_socket_int_opt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            configured_udp_send_buffer(),
        );

        if let Some(dscp) = configured_udp_dscp() {
            let tos = i32::from(dscp) << 2;
            let (level, optname) = match client_addr.ip() {
                IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
                    (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
                }
                _ => (libc::IPPROTO_IP, libc::IP_TOS),
            };
            let _ = set_udp_socket_int_opt(socket, level, optname, tos);
        }
    }

    #[cfg(target_os = "linux")]
    if let Some(priority) = configured_udp_priority() {
        let _ = set_udp_socket_int_opt(socket, libc::SOL_SOCKET, libc::SO_PRIORITY, priority);
    }

    #[cfg(not(unix))]
    let _ = (socket, client_addr);
}

pub struct EncodedVideoFrame {
    pub data: Vec<u8>,
    pub capture_micros: u64,
    pub source_seq: u64,
    pub is_recovery: bool,
}

pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub is_recovery: bool,
}

/// Backend for sending UDP data: either a direct socket or a punched socket.
enum SendBackend {
    /// Direct connection: raw UDP socket + optional encryption.
    Direct {
        socket: UdpSocket,
        crypto: Option<Arc<CryptoContext>>,
    },
    /// Punched connection: all media goes through PunchedSocket::send_media().
    Punched(Arc<PunchedSocket>),
}

pub struct UdpSender {
    backend: SendBackend,
    slicer: FrameSlicer,
    max_datagram_size: usize,
    frame_id: u32,
    audio_seq: u16,
    audio_redundancy: bool,
    audio_buf: Vec<u8>,
    previous_audio: Vec<u8>,
    encrypt_buf: Vec<u8>,
}

impl UdpSender {
    pub fn new(client_addr: SocketAddr, crypto: Option<Arc<CryptoContext>>) -> Result<Self, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind UDP: {e}"))?;
        configure_direct_udp_socket(&socket, client_addr);
        socket
            .connect(client_addr)
            .map_err(|e| format!("connect UDP: {e}"))?;
        let overhead = if crypto.is_some() { CRYPTO_OVERHEAD } else { 0 };
        let max_udp = select_max_udp_packet_size(client_addr).saturating_sub(overhead);
        if std::env::var_os("ST_TRACE").is_some()
            || max_udp != LOOPBACK_MAX_UDP
            || crypto.is_some()
        {
            eprintln!(
                "[transport] UDP max payload {} bytes for {} (encrypted={})",
                max_udp, client_addr, crypto.is_some()
            );
        }
        Ok(Self {
            backend: SendBackend::Direct { socket, crypto },
            slicer: FrameSlicer::with_max_udp(max_udp),
            max_datagram_size: max_udp,
            frame_id: 0,
            audio_seq: 0,
            audio_redundancy: audio_redundancy_enabled(),
            audio_buf: Vec::with_capacity(1500),
            previous_audio: Vec::with_capacity(1500),
            encrypt_buf: Vec::with_capacity(1500 + CRYPTO_OVERHEAD),
        })
    }

    /// Create a sender that uses a punched socket for media delivery.
    pub fn from_punched(punched: Arc<PunchedSocket>) -> Self {
        // Punched connections always use the safe (public internet) MTU
        // minus crypto overhead (handled inside PunchedSocket) minus channel prefix.
        let max_udp = SAFE_NETWORK_MAX_UDP
            .saturating_sub(CRYPTO_OVERHEAD)
            .saturating_sub(st_protocol::reliable_udp::PUNCHED_MEDIA_OVERHEAD);
        eprintln!(
            "[transport] Punched UDP max payload {} bytes for {}",
            max_udp,
            punched.peer()
        );
        Self {
            backend: SendBackend::Punched(punched),
            slicer: FrameSlicer::with_max_udp(max_udp),
            max_datagram_size: max_udp,
            frame_id: 0,
            audio_seq: 0,
            audio_redundancy: audio_redundancy_enabled(),
            audio_buf: Vec::with_capacity(1500),
            previous_audio: Vec::with_capacity(1500),
            encrypt_buf: Vec::with_capacity(1500 + CRYPTO_OVERHEAD),
        }
    }

    /// Send raw bytes through the backend, encrypting first if a CryptoContext is present.
    fn send_bytes_with(
        backend: &SendBackend,
        encrypt_buf: &mut Vec<u8>,
        plaintext: &[u8],
    ) -> Result<(), String> {
        match backend {
            SendBackend::Direct { socket, crypto } => {
                if let Some(ref crypto) = crypto {
                    encrypt_buf.clear();
                    encrypt_buf.resize(plaintext.len() + CRYPTO_OVERHEAD, 0);
                    let n = crypto.encrypt_into(plaintext, encrypt_buf);
                    socket
                        .send(&encrypt_buf[..n])
                        .map_err(|e| format!("send: {e}"))?;
                } else {
                    socket
                        .send(plaintext)
                        .map_err(|e| format!("send: {e}"))?;
                }
            }
            SendBackend::Punched(punched) => {
                punched.send_media(plaintext)?;
            }
        }
        Ok(())
    }

    /// Send a single NAL unit as sliced UDP packets (video).
    pub fn send_frame(
        &mut self,
        frame: &EncodedVideoFrame,
        send_micros: u64,
    ) -> Result<(), String> {
        let backend = &self.backend;
        let encrypt_buf = &mut self.encrypt_buf;
        let slicer = &mut self.slicer;
        let frame_id = self.frame_id;
        let (packets, parity) = slicer.slice_with_meta_parts(
            &frame.data,
            frame_id,
            FrameTimingMeta {
                capture_ts_micros: frame.capture_micros,
                send_ts_micros: send_micros,
            },
        );
        self.frame_id = self.frame_id.wrapping_add(1);

        let resend_first_packet = packets.len() > 1;
        for pkt in packets.iter() {
            Self::send_bytes_with(backend, encrypt_buf, pkt)?;
        }
        if let Some(parity) = parity {
            Self::send_bytes_with(backend, encrypt_buf, parity)?;
        }
        if resend_first_packet {
            Self::send_bytes_with(backend, encrypt_buf, &packets[0])?;
        }
        Ok(())
    }

    /// Send a single Opus audio packet.
    pub fn send_audio(&mut self, opus_data: &[u8]) -> Result<(), String> {
        let backend = &self.backend;
        let encrypt_buf = &mut self.encrypt_buf;
        let header = PacketHeader {
            seq: self.audio_seq,
            frame_id: 0,
            payload_type: PayloadType::Audio,
        };
        self.audio_seq = self.audio_seq.wrapping_add(1);

        let redundant_len = if self.audio_redundancy
            && !self.previous_audio.is_empty()
            && HEADER_SIZE
                + AUDIO_REDUNDANCY_HEADER_SIZE
                + opus_data.len()
                + self.previous_audio.len()
                <= self.max_datagram_size
        {
            self.previous_audio.len().min(u16::MAX as usize) as u16
        } else {
            0
        };

        self.audio_buf.clear();
        self.audio_buf.resize(
            HEADER_SIZE + AUDIO_REDUNDANCY_HEADER_SIZE + opus_data.len() + redundant_len as usize,
            0,
        );
        header.serialize(&mut self.audio_buf[..HEADER_SIZE]);
        AudioRedundancyMeta { redundant_len }.serialize(
            &mut self.audio_buf[HEADER_SIZE..HEADER_SIZE + AUDIO_REDUNDANCY_HEADER_SIZE],
        );
        let primary_start = HEADER_SIZE + AUDIO_REDUNDANCY_HEADER_SIZE;
        let primary_end = primary_start + opus_data.len();
        self.audio_buf[primary_start..primary_end].copy_from_slice(opus_data);
        if redundant_len > 0 {
            self.audio_buf[primary_end..primary_end + redundant_len as usize]
                .copy_from_slice(&self.previous_audio[..redundant_len as usize]);
        }

        let send_result = Self::send_bytes_with(backend, encrypt_buf, &self.audio_buf);
        self.previous_audio.clear();
        self.previous_audio.extend_from_slice(opus_data);
        send_result
    }
}

fn select_max_udp_packet_size(client_addr: SocketAddr) -> usize {
    if let Some(from_env) = std::env::var("ST_MAX_UDP_PACKET")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
    {
        let min_udp = HEADER_SIZE + st_protocol::packet::FRAME_START_HEADER_SIZE + 1;
        return from_env.max(min_udp);
    }

    if prefers_safe_udp_path(client_addr.ip()) {
        SAFE_NETWORK_MAX_UDP
    } else {
        LOOPBACK_MAX_UDP
    }
}

fn prefers_safe_udp_path(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback(),
        IpAddr::V6(v6) => !v6.is_loopback(),
    }
}
