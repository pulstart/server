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
/// Default media DSCP class (CS5 = 40), matching Sunshine's video marking. This
/// is default-on per the CLAUDE.md auto-enable rule: media is latency-sensitive
/// and benefits from being marked above bulk traffic on any QoS-aware path, so
/// it auto-enables instead of waiting for an env opt-in. `ST_UDP_DSCP=off`
/// (or `0`/`false`/`no`) is the escape hatch.
#[cfg(unix)]
const DEFAULT_MEDIA_DSCP: u8 = 40;

#[cfg(unix)]
fn configured_udp_send_buffer() -> i32 {
    std::env::var("ST_UDP_SNDBUF")
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_UDP_SEND_BUFFER)
}

/// Resolve the DSCP value to mark media packets with. Default-on: an unset env
/// returns the media class. `ST_UDP_DSCP=off|0|false|no` disables marking; any
/// other value sets an explicit 0..=63 DSCP.
#[cfg(unix)]
fn configured_udp_dscp() -> Option<u8> {
    match std::env::var("ST_UDP_DSCP") {
        Ok(raw) => {
            let trimmed = raw.trim();
            match trimmed.to_ascii_lowercase().as_str() {
                "off" | "0" | "false" | "no" => None,
                _ => trimmed.parse::<u8>().ok().filter(|value| *value <= 63),
            }
        }
        Err(_) => Some(DEFAULT_MEDIA_DSCP),
    }
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

/// Configured (max) audio redundancy depth — the cap the adaptive controller
/// (E5) ramps toward, and the fixed depth used when adaptation is disabled.
pub fn configured_audio_redundancy_depth() -> usize {
    audio_redundancy_depth()
}

/// E5 (default-on): drive verbatim audio-redundancy depth from measured loss —
/// depth 0 on a clean LAN (single losses are still covered by Opus LBRR in-band
/// FEC, kept on under E2 RESTRICTED_LOWDELAY), ramping toward the configured cap
/// (`ST_AUDIO_REDUNDANCY`, default 2) only when burst loss is observed, decaying
/// back to 0 over sustained clean intervals. This pays for burst resilience only
/// when loss appears instead of the legacy always-on fixed depth. The wire format
/// and client reconstruction are unchanged — only the depth *value* now tracks
/// loss — so flipping default-on adds no new untested recovery path.
/// `ST_AUDIO_ADAPTIVE_REDUNDANCY=0` (`false`/`no`/`off`) restores the fixed depth.
pub fn audio_adaptive_redundancy_enabled() -> bool {
    audio_adaptive_redundancy_tristate(
        std::env::var("ST_AUDIO_ADAPTIVE_REDUNDANCY")
            .ok()
            .as_deref(),
    )
}

/// Tri-state decision for E5: unset ⇒ auto-on, `0`/`false`/`no`/`off` ⇒ off
/// (fixed legacy depth), anything else ⇒ on.
fn audio_adaptive_redundancy_tristate(var: Option<&str>) -> bool {
    !matches!(
        var.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Build the slicer FEC config from env (A1/A2). **Reed-Solomon block FEC is the
/// default** (recovers up to M losses/unit); `ST_FEC=xor` is the escape hatch to
/// the always-correct single-XOR path. `ST_FEC_PCT` is the parity floor (default
/// 0 — a clean link pays only `ST_FEC_MIN_PARITY`, default 1, ≈ XOR's single
/// parity packet) and `ST_FEC_MIN_PARITY` the minimum recovery shards; the
/// adaptive FEC controller (A2) raises `fec_pct` live on measured loss and
/// decays back to the floor over clean intervals. RS recovery is validated
/// byte-exact AND decodable on real bitstreams (sw + NVENC) in
/// `recovery_loopback.rs`, so it is safe as the default.
fn env_fec_config() -> st_protocol::FecConfig {
    let mode = std::env::var("ST_FEC")
        .ok()
        .map(|v| st_protocol::packet::FecMode::from_env_value(&v))
        // Default-on RS (A1): the slicer's single-XOR path is now the M=1
        // degenerate case the controller reaches when the link is clean.
        .unwrap_or(st_protocol::packet::FecMode::Rs);
    let fec_pct = std::env::var("ST_FEC_PCT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(0)
        .min(100);
    let min_parity = std::env::var("ST_FEC_MIN_PARITY")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(1)
        .max(1);
    st_protocol::FecConfig {
        mode,
        fec_pct,
        min_parity,
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

/// `ST_FEC_TRACE=1` enables per-frame FEC-build vs send latency logging (A5).
fn fec_trace_enabled() -> bool {
    matches!(
        std::env::var("ST_FEC_TRACE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// Log slice/FEC-build vs on-wire send latency, throttled to ~2s, so the CPU
/// cost of FEC (especially once RS lands) is observable separately from send
/// time (A5). Mirrors the `ST_URING_TRACE` style.
fn log_fec_latency(fec_us: u128, send_us: u128, packet_count: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST_LOG_SECS: AtomicU64 = AtomicU64::new(0);
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_LOG_SECS.load(Ordering::Relaxed);
    if now_secs.saturating_sub(last) >= 2
        && LAST_LOG_SECS
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        eprintln!("[fec-trace] slice+fec={fec_us}us send={send_us}us packets={packet_count}");
    }
}

/// Log the resolved media DSCP once per process (default-on auto-enable rule:
/// log once either way).
#[cfg(unix)]
fn log_dscp_once(dscp: Option<u8>) {
    use std::sync::Once;
    static LOG: Once = Once::new();
    LOG.call_once(|| match dscp {
        Some(v) => {
            eprintln!("[transport] media DSCP marking on (dscp={v}); ST_UDP_DSCP=off to disable")
        }
        None => eprintln!("[transport] media DSCP marking off (ST_UDP_DSCP)"),
    });
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

/// Read the kernel's path/route MTU for a connected UDP socket (B7). Returns the
/// usable media payload (`mtu − IP − UDP − crypto`) or `None` if the query is
/// unsupported. Used to *shrink* the slice size on small-MTU egress paths
/// (overlays/tunnels) and, with `ST_PMTU_PROBE`, to grow it when the route MTU
/// supports more than the conservative WAN default.
#[cfg(target_os = "linux")]
fn path_mtu_payload(socket: &UdpSocket, client_addr: SocketAddr, overhead: usize) -> Option<usize> {
    let (level, optname) = match client_addr.ip() {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => (libc::IPPROTO_IPV6, libc::IPV6_MTU),
        _ => (libc::IPPROTO_IP, libc::IP_MTU),
    };
    let mut mtu: libc::c_int = 0;
    let mut len = mem::size_of::<libc::c_int>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            level,
            optname,
            &mut mtu as *mut _ as *mut _,
            &mut len,
        )
    };
    if ret != 0 || mtu <= 0 {
        return None;
    }
    // IPv6 has a 40-byte fixed header; IPv4 a 20-byte minimum. UDP adds 8.
    let ip_udp = match client_addr.ip() {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => 40 + 8,
        _ => 20 + 8,
    };
    (mtu as usize)
        .checked_sub(ip_udp + overhead)
        .filter(|p| *p > 0)
}

/// B7: decide whether the kernel's route MTU may *raise* the slice size above
/// the conservative default. **Auto-enabled on a directly-reachable LAN** (a
/// private/loopback/link-local destination): there the egress interface MTU
/// equals the path MTU — a single L2 hop, nothing in between to fragment — so
/// the route MTU is the true PMTU. On public / overlay paths (incl. Tailscale's
/// `100.64/10` CGNAT range, which carries a 1280 MTU and is deliberately *not*
/// `is_private`) the route MTU is only the first-hop MTU, so the raise stays off
/// unless `ST_PMTU_PROBE=1` forces it; acked-DF probing would lift this safely.
/// `ST_PMTU_PROBE=0` (`false`/`no`/`off`) force-disables the raise everywhere.
/// The *shrink* path is always on regardless — fitting a smaller egress MTU is
/// the previously-working safe fallback.
#[cfg(target_os = "linux")]
fn pmtu_auto_raise(dest: IpAddr, env: Option<&str>) -> bool {
    match env.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        Some("1") | Some("true") | Some("yes") | Some("on") => true,
        _ => is_lan_dest(dest),
    }
}

/// True when `ip` is a directly-reachable LAN address (private/loopback/
/// link-local), where the egress interface MTU equals the path MTU. Tailscale's
/// `100.64/10` shared-address range is intentionally excluded (not `is_private`).
fn is_lan_dest(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                v4.is_private() || v4.is_loopback() || v4.is_link_local()
            } else {
                v6.is_loopback()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
            }
        }
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

        let dscp = configured_udp_dscp();
        if let Some(dscp) = dscp {
            let tos = i32::from(dscp) << 2;
            let (level, optname) = match client_addr.ip() {
                IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
                    (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
                }
                _ => (libc::IPPROTO_IP, libc::IP_TOS),
            };
            let _ = set_udp_socket_int_opt(socket, level, optname, tos);
        }
        log_dscp_once(dscp);
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

/// B5: intra-frame packet pacing. Opt-in `ST_PACING`. A 4K IDR is 30-60 UDP
/// packets that today hit the NIC as one instantaneous burst; on a thin WAN
/// uplink that momentary saturation self-induces loss → ABR oscillation. When
/// enabled (and the path is not loopback), `send_frame` emits the frame's
/// packets in `group_us`-spaced groups sized so the paced rate stays under
/// `pace_bps`, with the total spread capped at `window_us` (< one frame
/// interval) so pacing never adds a frame of latency. Default-off pending live
/// WAN validation that the spin-then-sleep loop spreads without hurting
/// latency — getting it wrong adds the very latency it aims to remove.
struct PacingConfig {
    pace_bps: u64,
    group_us: u64,
    window_us: u64,
}

fn pacing_enabled() -> bool {
    matches!(
        std::env::var("ST_PACING").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

fn env_pacing(remote: SocketAddr) -> Option<PacingConfig> {
    if !pacing_enabled() || remote.ip().is_loopback() {
        return None;
    }
    let pace_mbps = std::env::var("ST_PACING_MBPS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(100);
    let window_us = std::env::var("ST_PACING_WINDOW_US")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v >= 1000)
        .unwrap_or(12_000);
    Some(PacingConfig {
        pace_bps: pace_mbps.saturating_mul(1_000_000),
        group_us: 1000,
        window_us,
    })
}

/// Packets to emit per `group_us` tick so a frame of `total_packets` is spread
/// (not bursted): the larger of the paced byte budget and the floor that keeps
/// the number of groups within `window_us` (so total spread ≤ window). Pure +
/// unit-tested; the hot-loop timing around it can only be validated live.
fn pacing_group_size(
    total_packets: usize,
    packet_bytes: usize,
    pace_bps: u64,
    group_us: u64,
    window_us: u64,
) -> usize {
    if packet_bytes == 0 || total_packets == 0 || group_us == 0 {
        return total_packets.max(1);
    }
    let budget_bytes = (pace_bps as u128 * group_us as u128) / (8 * 1_000_000);
    let by_rate = ((budget_bytes as usize) / packet_bytes).max(1);
    let max_groups = (window_us / group_us).max(1) as usize;
    let window_floor = total_packets.div_ceil(max_groups);
    by_rate.max(window_floor)
}

/// Spin-then-sleep until `deadline`: sleep the bulk, busy-spin the final
/// <300 µs for precision the OS scheduler can't give.
fn pace_until(deadline: std::time::Instant) {
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        if remaining > std::time::Duration::from_micros(300) {
            std::thread::sleep(remaining - std::time::Duration::from_micros(200));
        } else {
            std::hint::spin_loop();
        }
    }
}

pub struct UdpSender {
    backend: SendBackend,
    slicer: FrameSlicer,
    max_datagram_size: usize,
    pacing: Option<PacingConfig>,
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
        let mut max_udp = select_max_udp_packet_size(client_addr).saturating_sub(overhead);
        // B7: consult the kernel's route MTU. Always shrink to fit a small-MTU
        // egress path; only grow past the conservative default under ST_PMTU_PROBE.
        #[cfg(target_os = "linux")]
        if std::env::var_os("ST_MAX_UDP_PACKET").is_none() {
            if let Some(path_payload) = path_mtu_payload(&socket, client_addr, overhead) {
                if path_payload < max_udp {
                    eprintln!(
                        "[transport] path MTU shrinks UDP payload {max_udp} → {path_payload} for {client_addr}"
                    );
                    max_udp = path_payload;
                } else if path_payload > max_udp
                    && pmtu_auto_raise(
                        client_addr.ip(),
                        std::env::var("ST_PMTU_PROBE").ok().as_deref(),
                    )
                {
                    let raised = path_payload.min(LOOPBACK_MAX_UDP.saturating_sub(overhead));
                    if raised > max_udp {
                        eprintln!(
                            "[transport] route MTU raises UDP payload {max_udp} → {raised} for {client_addr} (LAN auto / ST_PMTU_PROBE)"
                        );
                        max_udp = raised;
                    }
                }
            }
        }
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
            slicer: FrameSlicer::with_config(max_udp, env_fec_config()),
            max_datagram_size: max_udp,
            pacing: env_pacing(client_addr),
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
        let peer = punched.peer();
        Self {
            backend: SendBackend::Punched(punched),
            slicer: FrameSlicer::with_config(max_udp, env_fec_config()),
            max_datagram_size: max_udp,
            pacing: env_pacing(peer),
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

    /// Live-update the RS parity percentage from the adaptive FEC controller
    /// (A2). Cheap no-op when unchanged; inert on the XOR default path.
    pub fn set_fec_pct(&mut self, pct: u16) {
        if self.slicer.fec_pct() != pct {
            self.slicer.set_fec_pct(pct);
        }
    }

    /// Live-update the verbatim audio-redundancy depth (E5 adaptive redundancy).
    pub fn set_audio_redundancy_depth(&mut self, depth: usize) {
        self.audio_redundancy_depth = depth.min(AUDIO_REDUNDANCY_MAX_DEPTH);
    }

    /// B5: when pacing is engaged for a frame this large, return the per-group
    /// packet count and inter-group interval; `None` means burst as before.
    /// Takes explicit fields (not `&self`) so it can run while `self.slicer` is
    /// mutably borrowed by `slice_with_meta_parts`.
    fn paced_plan(
        pacing: Option<&PacingConfig>,
        max_datagram_size: usize,
        packet_count: usize,
    ) -> Option<(usize, std::time::Duration)> {
        let p = pacing?;
        let group_size = pacing_group_size(
            packet_count,
            max_datagram_size,
            p.pace_bps,
            p.group_us,
            p.window_us,
        );
        // Only pace when it actually splits into multiple groups.
        (packet_count > group_size)
            .then(|| (group_size, std::time::Duration::from_micros(p.group_us)))
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
        // A5: separate the slice/FEC-build cost from the on-wire send cost so RS
        // FEC's added CPU is attributable once it lands. Gated like ST_URING_TRACE.
        let fec_trace = fec_trace_enabled();
        let slice_start = fec_trace.then(std::time::Instant::now);

        let frame_id = self.frame_id;
        let (packets, parity) = self.slicer.slice_with_meta_parts(
            &frame.data,
            frame_id,
            FrameTimingMeta {
                capture_ts_micros: frame.capture_micros,
                send_ts_micros: send_micros,
            },
            st_protocol::packet::frame_type::from_is_recovery(frame.is_recovery),
        );
        self.frame_id = self.frame_id.wrapping_add(1);

        let resend_first_packet = packets.len() > 1;

        // Build a flat list of plaintext packets to send: the sliced packets,
        // zero or more FEC parity packets (XOR ⇒ ≤1, RS ⇒ 0..M), optionally a
        // duplicate of the first.
        let mut plaintexts: Vec<&[u8]> = Vec::with_capacity(packets.len() + parity.len() + 1);
        for pkt in packets.iter() {
            plaintexts.push(pkt);
        }
        for pkt in parity.iter() {
            plaintexts.push(pkt);
        }
        if resend_first_packet {
            plaintexts.push(&packets[0]);
        }
        let packet_count = plaintexts.len();

        let send_start = fec_trace.then(std::time::Instant::now);
        let result =
            match Self::paced_plan(self.pacing.as_ref(), self.max_datagram_size, packet_count) {
                // B5: spread the frame's packets over `group_interval`-spaced groups.
                Some((group_size, group_interval)) => {
                    let start = std::time::Instant::now();
                    let mut res = Ok(());
                    for (i, chunk) in plaintexts.chunks(group_size).enumerate() {
                        if i > 0 {
                            pace_until(start + group_interval * i as u32);
                        }
                        res = Self::send_plaintext_batch(
                            &self.backend,
                            &mut self.encrypt_buf,
                            &mut self.encrypt_pool,
                            #[cfg(target_os = "linux")]
                            &mut self.send_batch,
                            #[cfg(target_os = "linux")]
                            self.uring.as_mut(),
                            chunk,
                        );
                        if res.is_err() {
                            break;
                        }
                    }
                    res
                }
                None => Self::send_plaintext_batch(
                    &self.backend,
                    &mut self.encrypt_buf,
                    &mut self.encrypt_pool,
                    #[cfg(target_os = "linux")]
                    &mut self.send_batch,
                    #[cfg(target_os = "linux")]
                    self.uring.as_mut(),
                    &plaintexts,
                ),
            };

        if let (Some(slice_start), Some(send_start)) = (slice_start, send_start) {
            let fec_us = send_start.duration_since(slice_start).as_micros();
            let send_us = send_start.elapsed().as_micros();
            log_fec_latency(fec_us, send_us, packet_count);
        }
        result
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

#[cfg(test)]
mod pmtu_tests {
    use super::is_lan_dest;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn lan_dest_classifies_private_and_excludes_tailscale_and_public() {
        let lan = |s: [u8; 4]| is_lan_dest(IpAddr::V4(Ipv4Addr::new(s[0], s[1], s[2], s[3])));
        assert!(lan([192, 168, 1, 50]), "192.168/16 is LAN");
        assert!(lan([10, 0, 0, 5]), "10/8 is LAN");
        assert!(lan([172, 16, 0, 1]), "172.16/12 is LAN");
        assert!(lan([127, 0, 0, 1]), "loopback is LAN");
        assert!(lan([169, 254, 1, 1]), "link-local is LAN");
        // Tailscale CGNAT 100.64/10 is NOT is_private → must not auto-raise
        // (its tunnel MTU is 1280, below the conservative default).
        assert!(!lan([100, 64, 0, 1]), "Tailscale CGNAT must be excluded");
        assert!(!lan([8, 8, 8, 8]), "public is not LAN");
        // v4-mapped v6 follows the v4 rules.
        assert!(is_lan_dest(IpAddr::V6(
            Ipv4Addr::new(192, 168, 0, 1).to_ipv6_mapped()
        )));
        assert!(!is_lan_dest(IpAddr::V6(
            Ipv4Addr::new(8, 8, 8, 8).to_ipv6_mapped()
        )));
        // v6 unique-local / link-local / loopback.
        assert!(is_lan_dest(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_lan_dest(IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_lan_dest(IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(!is_lan_dest(IpAddr::V6(Ipv6Addr::new(
            0x2606, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn auto_raise_lan_on_unset_wan_off_unless_forced() {
        use super::pmtu_auto_raise;
        let lan = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 2));
        let wan = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        // Unset: raise on LAN, hold on WAN.
        assert!(pmtu_auto_raise(lan, None));
        assert!(!pmtu_auto_raise(wan, None));
        // Force on: raise even on WAN.
        assert!(pmtu_auto_raise(wan, Some("1")));
        // Force off: never raise, even on LAN.
        assert!(!pmtu_auto_raise(lan, Some("0")));
        assert!(!pmtu_auto_raise(lan, Some("off")));
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::pacing_group_size;

    const MTU: usize = 1200;

    #[test]
    fn small_frame_fits_one_group() {
        // 2 packets at 100 Mbps / 1 ms budget (~12500 B) → all in one group.
        let gs = pacing_group_size(2, MTU, 100_000_000, 1000, 12_000);
        assert!(gs >= 2, "small frame should not split: {gs}");
    }

    #[test]
    fn large_frame_splits_into_multiple_groups() {
        // 60-packet 4K IDR at 100 Mbps → group size well below 60.
        let gs = pacing_group_size(60, MTU, 100_000_000, 1000, 12_000);
        assert!(gs < 60, "large frame must split: {gs}");
        assert!(gs >= 1);
    }

    #[test]
    fn rate_budget_matches_expected() {
        // 100 Mbps over 1 ms = 12_500 B; /1200 = 10 packets per group.
        let gs = pacing_group_size(100, MTU, 100_000_000, 1000, 1_000_000);
        assert_eq!(gs, 10);
    }

    #[test]
    fn window_cap_bounds_group_count() {
        // Window = 4 ms / 1 ms ticks = 4 groups max. 100 packets at a tiny
        // pace rate would split into many groups, but the window floor forces
        // ceil(100/4)=25 per group so the spread stays within 4 ms.
        let gs = pacing_group_size(100, MTU, 1_000_000, 1000, 4_000);
        assert_eq!(gs, 25);
        let groups = 100usize.div_ceil(gs);
        assert!(groups <= 4, "spread exceeds window: {groups} groups");
    }

    #[test]
    fn zero_packet_size_is_safe() {
        // Defensive: never divide by zero.
        let gs = pacing_group_size(10, 0, 100_000_000, 1000, 12_000);
        assert!(gs >= 10);
    }
}

#[cfg(test)]
mod audio_redundancy_tests {
    use super::audio_adaptive_redundancy_tristate;

    #[test]
    fn adaptive_redundancy_auto_on_unless_disabled() {
        // E5 default-on: unset and unknown values enable adaptive depth.
        assert!(audio_adaptive_redundancy_tristate(None), "unset ⇒ auto-on");
        assert!(audio_adaptive_redundancy_tristate(Some("1")), "explicit on");
        assert!(audio_adaptive_redundancy_tristate(Some("yes")), "truthy on");
        assert!(
            audio_adaptive_redundancy_tristate(Some("garbage")),
            "unknown ⇒ on"
        );
        // Only the explicit off sentinels restore the fixed legacy depth.
        assert!(!audio_adaptive_redundancy_tristate(Some("0")), "0 ⇒ off");
        assert!(
            !audio_adaptive_redundancy_tristate(Some("off")),
            "off ⇒ off"
        );
        assert!(
            !audio_adaptive_redundancy_tristate(Some(" False ")),
            "trim + case-insensitive off"
        );
    }
}
