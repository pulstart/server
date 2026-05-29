use st_protocol::packet::{
    audio_redundancy_header_size, serialize_audio_redundancy_header, AUDIO_REDUNDANCY_MAX_DEPTH,
    HEADER_SIZE,
};
use st_protocol::reliable_udp::PunchedSocket;
use st_protocol::tunnel::{CryptoContext, CRYPTO_OVERHEAD};
use st_protocol::{FrameSlicer, FrameTimingMeta, PacketHeader, PayloadType};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
#[cfg(unix)]
use std::{mem, os::fd::AsRawFd};

#[cfg(target_os = "linux")]
mod linux_send {
    use std::io;
    use std::os::fd::RawFd;

    pub const SEND_BATCH_MAX: usize = 64;

    // Kernel UAPI (include/uapi/linux/udp.h). Not always exposed by libc.
    const UDP_SEGMENT: libc::c_int = 103;

    pub struct SendBatch {
        iovecs: Vec<libc::iovec>,
        msgs: Vec<libc::mmsghdr>,
        gso_supported: bool,
    }

    // msghdr holds raw pointers back into the SendBatch's own iovec vec; contents
    // are rebuilt on every send_all call and the batch is owned by a single sender
    // thread, so sending it across threads is safe.
    unsafe impl Send for SendBatch {}

    impl SendBatch {
        pub fn new() -> Self {
            Self {
                iovecs: Vec::with_capacity(SEND_BATCH_MAX),
                msgs: Vec::with_capacity(SEND_BATCH_MAX),
                gso_supported: false,
            }
        }

        /// Probe `UDP_SEGMENT` (kernel ≥ 4.18). Clears the socket-level default
        /// back to 0 afterwards — GSO is selected per-sendmsg via cmsg, not
        /// per-socket. Returns true on success.
        pub fn probe_gso(&mut self, fd: RawFd) -> bool {
            let value: libc::c_int = 0;
            let rc = unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_UDP,
                    UDP_SEGMENT,
                    &value as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            self.gso_supported = rc == 0;
            self.gso_supported
        }

        /// Attempt to send `packets` as a single UDP_SEGMENT (GSO) datagram on the
        /// connected socket. Returns `Ok(true)` if all packets were sent via GSO,
        /// `Ok(false)` if the batch is not GSO-eligible (caller falls back to
        /// `send_all`). Eligibility: GSO probe succeeded, ≥ 2 packets, every
        /// packet except the last is the same non-zero size, and the last packet
        /// is ≤ that size.
        pub fn try_send_gso(&mut self, fd: RawFd, packets: &[&[u8]]) -> io::Result<bool> {
            if !self.gso_supported || packets.len() < 2 {
                return Ok(false);
            }
            let seg_size = packets[0].len();
            if seg_size == 0 || seg_size > u16::MAX as usize {
                return Ok(false);
            }
            for p in &packets[..packets.len() - 1] {
                if p.len() != seg_size {
                    return Ok(false);
                }
            }
            let last_len = packets[packets.len() - 1].len();
            if last_len == 0 || last_len > seg_size {
                return Ok(false);
            }

            self.iovecs.clear();
            for p in packets {
                self.iovecs.push(libc::iovec {
                    iov_base: p.as_ptr() as *mut libc::c_void,
                    iov_len: p.len(),
                });
            }

            // One UDP_SEGMENT cmsg carrying a u16 segment size. 64 bytes is far
            // more than CMSG_SPACE(sizeof(u16)) on any libc.
            let mut cmsg_buf = [0u8; 64];
            let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
            hdr.msg_name = std::ptr::null_mut();
            hdr.msg_namelen = 0;
            hdr.msg_iov = self.iovecs.as_mut_ptr();
            hdr.msg_iovlen = self.iovecs.len() as _;
            hdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
            hdr.msg_controllen =
                unsafe { libc::CMSG_SPACE(std::mem::size_of::<u16>() as u32) } as _;
            hdr.msg_flags = 0;

            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&hdr);
                (*cmsg).cmsg_level = libc::IPPROTO_UDP;
                (*cmsg).cmsg_type = UDP_SEGMENT;
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<u16>() as u32) as _;
                let data_ptr = libc::CMSG_DATA(cmsg) as *mut u16;
                std::ptr::write_unaligned(data_ptr, seg_size as u16);
            }

            loop {
                let ret = unsafe { libc::sendmsg(fd, &hdr, 0) };
                if ret < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    // EIO/ENOBUFS from GSO commonly means the path can't handle
                    // the stitched datagram (PMTU, offload off). Disable GSO for
                    // the rest of the session so we stop paying the probe cost.
                    if matches!(
                        err.raw_os_error(),
                        Some(libc::EIO) | Some(libc::ENOBUFS) | Some(libc::EMSGSIZE)
                    ) {
                        self.gso_supported = false;
                    }
                    return Err(err);
                }
                return Ok(true);
            }
        }

        /// Batched sendmmsg on a connected socket. Handles partial sends and EINTR.
        pub fn send_all(&mut self, fd: RawFd, packets: &[&[u8]]) -> io::Result<()> {
            let mut cursor = 0;
            while cursor < packets.len() {
                let chunk_end = (cursor + SEND_BATCH_MAX).min(packets.len());
                let chunk = chunk_end - cursor;
                self.iovecs.clear();
                self.msgs.clear();
                for pkt in &packets[cursor..chunk_end] {
                    self.iovecs.push(libc::iovec {
                        iov_base: pkt.as_ptr() as *mut libc::c_void,
                        iov_len: pkt.len(),
                    });
                }
                for i in 0..chunk {
                    let iov_ptr = &mut self.iovecs[i] as *mut libc::iovec;
                    let mut hdr: libc::mmsghdr = unsafe { std::mem::zeroed() };
                    hdr.msg_hdr = libc::msghdr {
                        msg_name: std::ptr::null_mut(),
                        msg_namelen: 0,
                        msg_iov: iov_ptr,
                        msg_iovlen: 1,
                        msg_control: std::ptr::null_mut(),
                        msg_controllen: 0,
                        msg_flags: 0,
                    };
                    self.msgs.push(hdr);
                }
                let vlen = self.msgs.len() as libc::c_uint;
                let ret = unsafe { libc::sendmmsg(fd, self.msgs.as_mut_ptr(), vlen, 0) };
                if ret < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(err);
                }
                let sent = ret as usize;
                if sent == 0 {
                    return Err(io::Error::other("sendmmsg returned 0"));
                }
                cursor += sent;
            }
            Ok(())
        }
    }
}

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

/// Number of previous opus packets to attach to every audio datagram.
///
/// `ST_AUDIO_REDUNDANCY` accepts either a numeric depth (0 = off, up to
/// `AUDIO_REDUNDANCY_MAX_DEPTH`) or a boolean-style toggle. When unset the
/// default depth is 2, which lets the client recover any burst of up to two
/// consecutive lost audio packets without falling back to PLC.
fn audio_redundancy_depth() -> usize {
    const DEFAULT_DEPTH: usize = 2;
    let raw = match std::env::var("ST_AUDIO_REDUNDANCY") {
        Ok(value) => value,
        Err(_) => return DEFAULT_DEPTH,
    };
    let trimmed = raw.trim();
    if let Ok(n) = trimmed.parse::<usize>() {
        return n.min(AUDIO_REDUNDANCY_MAX_DEPTH);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "0" | "off" | "false" | "no" => 0,
        "" => DEFAULT_DEPTH,
        _ => DEFAULT_DEPTH,
    }
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
    audio_redundancy_depth: usize,
    audio_buf: Vec<u8>,
    previous_audio: VecDeque<Vec<u8>>,
    encrypt_buf: Vec<u8>,
    // Pooled per-packet ciphertext buffers for sendmmsg in the crypto path.
    encrypt_pool: Vec<Vec<u8>>,
    #[cfg(target_os = "linux")]
    send_batch: linux_send::SendBatch,
    #[cfg(target_os = "linux")]
    uring: Option<crate::linux_uring::UringSend>,
}

impl UdpSender {
    pub fn new(
        client_addr: SocketAddr,
        crypto: Option<Arc<CryptoContext>>,
    ) -> Result<Self, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind UDP: {e}"))?;
        configure_direct_udp_socket(&socket, client_addr);
        socket
            .connect(client_addr)
            .map_err(|e| format!("connect UDP: {e}"))?;
        let overhead = if crypto.is_some() { CRYPTO_OVERHEAD } else { 0 };
        let max_udp = select_max_udp_packet_size(client_addr).saturating_sub(overhead);
        if std::env::var_os("ST_TRACE").is_some() || max_udp != LOOPBACK_MAX_UDP || crypto.is_some()
        {
            eprintln!(
                "[transport] UDP max payload {} bytes for {} (encrypted={})",
                max_udp,
                client_addr,
                crypto.is_some()
            );
        }
        #[cfg(target_os = "linux")]
        let uring = build_uring_send();
        #[cfg(target_os = "linux")]
        let send_batch = {
            let mut batch = linux_send::SendBatch::new();
            // GSO is incompatible with per-packet AEAD (each packet needs its
            // own nonce) and pointless when the io_uring send path is live, so
            // only probe when this sender owns the plaintext path.
            if crypto.is_none() && uring.is_none() && gso_allowed() {
                let gso = batch.probe_gso(socket.as_raw_fd());
                if std::env::var_os("ST_TRACE").is_some() {
                    eprintln!(
                        "[transport] UDP_SEGMENT (GSO) {}",
                        if gso { "enabled" } else { "unavailable" }
                    );
                }
            }
            batch
        };
        Ok(Self {
            backend: SendBackend::Direct { socket, crypto },
            slicer: FrameSlicer::with_max_udp(max_udp),
            max_datagram_size: max_udp,
            frame_id: 0,
            audio_seq: 0,
            audio_redundancy_depth: audio_redundancy_depth(),
            audio_buf: Vec::with_capacity(1500),
            previous_audio: VecDeque::with_capacity(AUDIO_REDUNDANCY_MAX_DEPTH),
            encrypt_buf: Vec::with_capacity(1500 + CRYPTO_OVERHEAD),
            encrypt_pool: Vec::new(),
            #[cfg(target_os = "linux")]
            send_batch,
            #[cfg(target_os = "linux")]
            uring,
        })
    }

    #[cfg(not(target_os = "linux"))]
    const _SEND_BATCH_UNUSED: () = ();
}

/// `ST_UDP_GSO=0` forces the sendmmsg path, in case a driver reports the probe
/// as supported but corrupts stitched datagrams on real paths.
#[cfg(target_os = "linux")]
fn gso_allowed() -> bool {
    !matches!(
        std::env::var("ST_UDP_GSO").ok().as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

#[cfg(target_os = "linux")]
fn build_uring_send() -> Option<crate::linux_uring::UringSend> {
    if !crate::linux_uring::io_uring_requested() {
        return None;
    }
    match crate::linux_uring::UringSend::new() {
        Some(u) => {
            eprintln!("[transport] io_uring send path enabled");
            Some(u)
        }
        None => {
            eprintln!("[transport] io_uring requested but unavailable; falling back to sendmmsg");
            None
        }
    }
}

impl UdpSender {
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
            audio_redundancy_depth: audio_redundancy_depth(),
            audio_buf: Vec::with_capacity(1500),
            previous_audio: VecDeque::with_capacity(AUDIO_REDUNDANCY_MAX_DEPTH),
            encrypt_buf: Vec::with_capacity(1500 + CRYPTO_OVERHEAD),
            encrypt_pool: Vec::new(),
            #[cfg(target_os = "linux")]
            send_batch: linux_send::SendBatch::new(),
            #[cfg(target_os = "linux")]
            uring: None,
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
                    socket.send(plaintext).map_err(|e| format!("send: {e}"))?;
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
        let frame_id = self.frame_id;
        let (packets, parity) = self.slicer.slice_with_meta_parts(
            &frame.data,
            frame_id,
            FrameTimingMeta {
                capture_ts_micros: frame.capture_micros,
                send_ts_micros: send_micros,
            },
        );
        self.frame_id = self.frame_id.wrapping_add(1);

        let resend_first_packet = packets.len() > 1;

        // Build a flat list of plaintext packets to send: the sliced packets,
        // optionally a parity packet, optionally a duplicate of the first.
        let mut plaintexts: Vec<&[u8]> = Vec::with_capacity(packets.len() + 2);
        for pkt in packets.iter() {
            plaintexts.push(pkt);
        }
        if let Some(parity) = parity {
            plaintexts.push(parity);
        }
        if resend_first_packet {
            plaintexts.push(&packets[0]);
        }

        Self::send_plaintext_batch(
            &self.backend,
            &mut self.encrypt_buf,
            &mut self.encrypt_pool,
            #[cfg(target_os = "linux")]
            &mut self.send_batch,
            #[cfg(target_os = "linux")]
            self.uring.as_mut(),
            &plaintexts,
        )
    }

    /// Send a list of plaintext payloads. Uses a single sendmmsg on Linux for
    /// Direct-backed senders (with per-packet AEAD when crypto is active). Other
    /// platforms and the Punched backend fall back to the original per-packet
    /// path.
    fn send_plaintext_batch(
        backend: &SendBackend,
        encrypt_buf: &mut Vec<u8>,
        encrypt_pool: &mut Vec<Vec<u8>>,
        #[cfg(target_os = "linux")] send_batch: &mut linux_send::SendBatch,
        #[cfg(target_os = "linux")] uring: Option<&mut crate::linux_uring::UringSend>,
        plaintexts: &[&[u8]],
    ) -> Result<(), String> {
        if plaintexts.is_empty() {
            return Ok(());
        }

        match backend {
            SendBackend::Direct { socket, crypto } => {
                #[cfg(target_os = "linux")]
                {
                    let fd = socket.as_raw_fd();
                    if let Some(ref crypto) = crypto {
                        while encrypt_pool.len() < plaintexts.len() {
                            encrypt_pool.push(Vec::with_capacity(1500 + CRYPTO_OVERHEAD));
                        }
                        for (i, pt) in plaintexts.iter().enumerate() {
                            let buf = &mut encrypt_pool[i];
                            buf.clear();
                            buf.resize(pt.len() + CRYPTO_OVERHEAD, 0);
                            let n = crypto.encrypt_into(pt, buf);
                            buf.truncate(n);
                        }
                        let refs: Vec<&[u8]> = encrypt_pool[..plaintexts.len()]
                            .iter()
                            .map(|v| v.as_slice())
                            .collect();
                        if let Some(uring) = uring {
                            uring
                                .send_all(fd, &refs)
                                .map_err(|e| format!("uring send: {e}"))?;
                        } else {
                            send_batch
                                .send_all(fd, &refs)
                                .map_err(|e| format!("sendmmsg: {e}"))?;
                        }
                    } else if let Some(uring) = uring {
                        uring
                            .send_all(fd, plaintexts)
                            .map_err(|e| format!("uring send: {e}"))?;
                    } else {
                        // No crypto + sendmmsg path: try one GSO sendmsg first.
                        // try_send_gso returns false when the batch isn't
                        // uniform-sized; on an actual send error we still fall
                        // back so a single bad path doesn't kill the session.
                        let gso_done = match send_batch.try_send_gso(fd, plaintexts) {
                            Ok(done) => done,
                            Err(err) => {
                                eprintln!(
                                    "[transport] GSO send failed ({err}); falling back to sendmmsg"
                                );
                                false
                            }
                        };
                        if !gso_done {
                            send_batch
                                .send_all(fd, plaintexts)
                                .map_err(|e| format!("sendmmsg: {e}"))?;
                        }
                    }
                    let _ = encrypt_buf;
                    Ok(())
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = encrypt_pool;
                    for pt in plaintexts.iter() {
                        if let Some(ref crypto) = crypto {
                            encrypt_buf.clear();
                            encrypt_buf.resize(pt.len() + CRYPTO_OVERHEAD, 0);
                            let n = crypto.encrypt_into(pt, encrypt_buf);
                            socket
                                .send(&encrypt_buf[..n])
                                .map_err(|e| format!("send: {e}"))?;
                        } else {
                            socket.send(pt).map_err(|e| format!("send: {e}"))?;
                        }
                    }
                    Ok(())
                }
            }
            SendBackend::Punched(punched) => {
                let _ = (encrypt_buf, encrypt_pool);
                #[cfg(target_os = "linux")]
                let _ = uring;
                for pt in plaintexts.iter() {
                    punched.send_media(pt)?;
                }
                Ok(())
            }
        }
    }

    /// Send a single Opus audio packet, with up to `audio_redundancy_depth`
    /// previously-sent opus packets attached as redundancy. The client uses
    /// the redundant copies to recover lost packets without waiting for
    /// retransmission.
    pub fn send_audio(&mut self, opus_data: &[u8]) -> Result<(), String> {
        let backend = &self.backend;
        let encrypt_buf = &mut self.encrypt_buf;
        let header = PacketHeader {
            seq: self.audio_seq,
            frame_id: 0,
            payload_type: PayloadType::Audio,
        };
        self.audio_seq = self.audio_seq.wrapping_add(1);

        // Pick the largest k <= depth such that the resulting datagram still fits
        // inside max_datagram_size. The newest k chunks of `previous_audio` are
        // chosen, but we attach them in oldest-first order so the client can
        // index them as `seq - k .. seq - 1`.
        let primary_len = opus_data.len();
        let available_audio = self
            .previous_audio
            .iter()
            .map(|chunk| chunk.len())
            .collect::<Vec<_>>();
        let mut chunks_to_attach: usize = 0;
        let mut chunks_byte_total: usize = 0;
        let max_depth = self.audio_redundancy_depth.min(available_audio.len());
        for k in 1..=max_depth {
            let candidate_bytes: usize = available_audio[available_audio.len() - k..].iter().sum();
            let total =
                HEADER_SIZE + audio_redundancy_header_size(k) + primary_len + candidate_bytes;
            if total > self.max_datagram_size {
                break;
            }
            chunks_to_attach = k;
            chunks_byte_total = candidate_bytes;
        }

        let redundancy_header_bytes = audio_redundancy_header_size(chunks_to_attach);
        let total_size = HEADER_SIZE + redundancy_header_bytes + primary_len + chunks_byte_total;
        self.audio_buf.clear();
        self.audio_buf.resize(total_size, 0);
        header.serialize(&mut self.audio_buf[..HEADER_SIZE]);
        let chunk_start_idx = self.previous_audio.len() - chunks_to_attach;
        let chunk_lens: Vec<u16> = (chunk_start_idx..self.previous_audio.len())
            .map(|i| self.previous_audio[i].len() as u16)
            .collect();
        let written = serialize_audio_redundancy_header(
            &mut self.audio_buf[HEADER_SIZE..HEADER_SIZE + redundancy_header_bytes],
            &chunk_lens,
        );
        debug_assert_eq!(written, redundancy_header_bytes);
        let primary_start = HEADER_SIZE + redundancy_header_bytes;
        let primary_end = primary_start + primary_len;
        self.audio_buf[primary_start..primary_end].copy_from_slice(opus_data);
        let mut cursor = primary_end;
        for i in chunk_start_idx..self.previous_audio.len() {
            let chunk = &self.previous_audio[i];
            self.audio_buf[cursor..cursor + chunk.len()].copy_from_slice(chunk);
            cursor += chunk.len();
        }
        debug_assert_eq!(cursor, total_size);

        let send_result = Self::send_bytes_with(backend, encrypt_buf, &self.audio_buf);

        // Track this packet for future datagrams' redundancy, bounded by the
        // currently-configured depth. If depth is 0, drop the history entirely.
        if self.audio_redundancy_depth > 0 {
            self.previous_audio.push_back(opus_data.to_vec());
            while self.previous_audio.len() > self.audio_redundancy_depth {
                self.previous_audio.pop_front();
            }
        } else {
            self.previous_audio.clear();
        }

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

#[cfg(all(test, target_os = "linux"))]
mod gso_tests {
    use super::linux_send::SendBatch;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    /// Real-syscall regression guard for Phase 1.2. Sends 3 packets of
    /// [1000, 1000, 500] via UDP_SEGMENT on loopback and verifies the kernel
    /// hands the peer three separate datagrams with the original contents.
    /// Skips on kernels where `setsockopt(UDP_SEGMENT)` isn't accepted.
    #[test]
    fn gso_batch_splits_into_per_packet_datagrams() {
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind recv");
        recv.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("recv timeout");
        let recv_addr = recv.local_addr().expect("recv addr");

        let send = UdpSocket::bind("127.0.0.1:0").expect("bind send");
        send.connect(recv_addr).expect("connect");

        let mut batch = SendBatch::new();
        if !batch.probe_gso(send.as_raw_fd()) {
            eprintln!("[gso_test] UDP_SEGMENT unsupported on this kernel — skipping");
            return;
        }

        let a = vec![0xAA; 1000];
        let b = vec![0xBB; 1000];
        let c = vec![0xCC; 500];
        let refs: [&[u8]; 3] = [&a, &b, &c];
        let sent = batch
            .try_send_gso(send.as_raw_fd(), &refs)
            .expect("gso send");
        assert!(sent, "uniform 3-packet batch must take the GSO path");

        let mut buf = vec![0u8; 2048];
        let mut got: Vec<Vec<u8>> = Vec::new();
        for _ in 0..3 {
            let n = recv.recv(&mut buf).expect("recv datagram");
            got.push(buf[..n].to_vec());
        }
        assert_eq!(got[0], a, "first GSO segment preserved");
        assert_eq!(got[1], b, "middle GSO segment preserved");
        assert_eq!(got[2], c, "short trailing GSO segment preserved");
    }

    #[test]
    fn gso_rejects_nonuniform_batch() {
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind recv");
        let recv_addr = recv.local_addr().expect("recv addr");
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind send");
        send.connect(recv_addr).expect("connect");

        let mut batch = SendBatch::new();
        if !batch.probe_gso(send.as_raw_fd()) {
            return;
        }

        // Middle packet is a different size — kernel can't slice this as GSO.
        let a = vec![0u8; 1000];
        let b = vec![0u8; 900];
        let c = vec![0u8; 1000];
        let refs: [&[u8]; 3] = [&a, &b, &c];
        let sent = batch
            .try_send_gso(send.as_raw_fd(), &refs)
            .expect("gso reject");
        assert!(!sent, "non-uniform batch must fall back to sendmmsg");
    }

    #[test]
    fn gso_skips_single_packet_batch() {
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind send");
        send.connect(
            "127.0.0.1:0"
                .parse::<std::net::SocketAddr>()
                .unwrap_or_else(|_| {
                    // Any valid loopback; we never send here.
                    "127.0.0.1:9".parse().unwrap()
                }),
        )
        .ok();
        let mut batch = SendBatch::new();
        let _ = batch.probe_gso(send.as_raw_fd());
        let only = vec![0u8; 1000];
        let refs: [&[u8]; 1] = [&only];
        let sent = batch
            .try_send_gso(send.as_raw_fd(), &refs)
            .expect("single-packet gso");
        assert!(!sent, "1-packet batches never take the GSO path");
    }
}
