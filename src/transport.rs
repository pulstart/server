use st_protocol::packet::{
    audio_redundancy_header_size, serialize_audio_redundancy_header, AUDIO_REDUNDANCY_MAX_DEPTH,
    HEADER_SIZE,
};
use st_protocol::tcp_tunnel::TunnelLink;
use st_protocol::tunnel::{CryptoContext, CRYPTO_OVERHEAD};
use st_protocol::{FrameSlicer, FrameTimingMeta, PacketHeader, PayloadType};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
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
    audio_redundancy_depth_from_var(std::env::var("ST_AUDIO_REDUNDANCY").ok().as_deref())
}

fn audio_redundancy_depth_from_var(var: Option<&str>) -> usize {
    const DEFAULT_DEPTH: usize = 2;
    let Some(raw) = var else {
        return DEFAULT_DEPTH;
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

/// Configured audio redundancy depth: fixed by default and the cap when
/// explicitly using adaptive redundancy.
pub fn configured_audio_redundancy_depth() -> usize {
    audio_redundancy_depth()
}

/// Opt-in adaptive redundancy. Default 5 ms CELT packets do not carry usable
/// Opus LBRR, so the safe default is the configured fixed verbatim depth from
/// stream start (`ST_AUDIO_REDUNDANCY`, default 2). Set
/// `ST_AUDIO_ADAPTIVE_REDUNDANCY=1` to decay that depth on a clean path and ramp
/// it back after loss. The wire format and client reconstruction are unchanged.
pub fn audio_adaptive_redundancy_enabled() -> bool {
    audio_adaptive_redundancy_tristate(
        std::env::var("ST_AUDIO_ADAPTIVE_REDUNDANCY")
            .ok()
            .as_deref(),
    )
}

/// Unset/unknown values keep fixed redundancy; only explicit truthy values opt
/// into adaptation.
fn audio_adaptive_redundancy_tristate(var: Option<&str>) -> bool {
    matches!(
        var.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
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

/// Policy for the delayed-duplicate FrameStart (cheap loss hardening for the
/// most critical packet of a multi-packet unit).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DupFirstMode {
    /// Never duplicate.
    Off,
    /// Always duplicate (legacy behavior).
    On,
    /// Duplicate only while recent loss is present (default).
    Auto,
}

/// `ST_DUP_FRAMESTART`: `off` never duplicates, `on` always (legacy), `auto`
/// (default) only while the link shows recent loss — so a clean / bandwidth-
/// constrained path pays no duplicate overhead. Sending it unconditionally was
/// pure waste on a clean link (and the original false-`late` source the ABR
/// controller misread as impairment).
fn dup_first_mode_from_env() -> DupFirstMode {
    match std::env::var("ST_DUP_FRAMESTART") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "no" => DupFirstMode::Off,
            "on" | "1" | "true" | "yes" | "always" => DupFirstMode::On,
            _ => DupFirstMode::Auto,
        },
        Err(_) => DupFirstMode::Auto,
    }
}

/// Resolve whether to send the duplicate FrameStart for this unit.
fn dup_first_effective(mode: DupFirstMode, loss_active: bool) -> bool {
    match mode {
        DupFirstMode::Off => false,
        DupFirstMode::On => true,
        DupFirstMode::Auto => loss_active,
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
    /// Internal-only encoder/config epoch. It is not serialized on the wire;
    /// per-client senders use it to discard queued output from a replaced encoder.
    pub video_epoch: u64,
}

#[derive(Debug)]
pub struct EncodedAudioPacket {
    pub source_seq: u64,
    pub data: Vec<u8>,
}

pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub is_recovery: bool,
}

/// Backend for sending media: either a direct UDP socket or a tunnel link
/// (hole-punched UDP or TCP fallback).
enum SendBackend {
    /// Direct connection: raw UDP socket + optional encryption.
    Direct {
        socket: UdpSocket,
        crypto: Option<Arc<CryptoContext>>,
    },
    /// Tunneled connection: all media goes through TunnelLink::send_media()
    /// (PunchedSocket over UDP, or TcpTunnel for the TCP fallback).
    Tunnel(Arc<dyn TunnelLink>),
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
    audio_redundancy_depth: usize,
    audio_redundancy_max_depth: usize,
    last_audio_source_seq: Option<u64>,
    // Delayed-duplicate FrameStart policy + the live Auto-mode verdict pushed by
    // the utility `DupFirstController` (only sends the duplicate while it has
    // measurably reduced frame loss).
    dup_mode: DupFirstMode,
    dup_auto_active: bool,
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
        // Mutated only on the Linux PMTU path below; non-Linux keeps the default.
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
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
        let audio_redundancy_max_depth = audio_redundancy_depth();
        Ok(Self {
            backend: SendBackend::Direct { socket, crypto },
            slicer: FrameSlicer::with_config(max_udp, env_fec_config()),
            max_datagram_size: max_udp,
            pacing: env_pacing(client_addr),
            frame_id: 0,
            audio_redundancy_depth: audio_redundancy_max_depth,
            audio_redundancy_max_depth,
            last_audio_source_seq: None,
            dup_mode: dup_first_mode_from_env(),
            dup_auto_active: false,
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
    /// Create a sender that delivers media through a tunnel link (hole-punched
    /// UDP socket or TCP fallback tunnel).
    pub fn from_tunnel(link: Arc<dyn TunnelLink>) -> Self {
        let max_udp = link.max_media_payload();
        let reliable = link.is_reliable();
        eprintln!(
            "[transport] Tunnel media payload {} bytes for {} (reliable={})",
            max_udp,
            link.peer(),
            reliable
        );
        let peer = link.peer();
        // Loss-recovery extras (FEC parity, duplicate FrameStart, audio
        // redundancy) are pure overhead on a reliable (TCP) link, so reliable
        // links disable parity construction entirely (no per-frame XOR/RS pass).
        let mut slicer = FrameSlicer::with_config(max_udp, env_fec_config());
        if reliable {
            slicer.set_parity_enabled(false);
        }
        let audio_redundancy_max_depth = if reliable {
            0
        } else {
            audio_redundancy_depth()
        };
        Self {
            backend: SendBackend::Tunnel(link),
            slicer,
            max_datagram_size: max_udp,
            pacing: env_pacing(peer),
            frame_id: 0,
            audio_redundancy_depth: audio_redundancy_max_depth,
            audio_redundancy_max_depth,
            last_audio_source_seq: None,
            dup_mode: if reliable {
                DupFirstMode::Off
            } else {
                dup_first_mode_from_env()
            },
            dup_auto_active: false,
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

    /// Live Auto-mode verdict from the `DupFirstController` (A/B utility probe):
    /// whether the delayed-duplicate FrameStart is currently earning its keep.
    /// Ignored when `ST_DUP_FRAMESTART` forces `off`/`on`.
    pub fn set_dup_first(&mut self, on: bool) {
        self.dup_auto_active = on;
    }

    /// Live-update the verbatim audio-redundancy depth (E5 adaptive redundancy).
    pub fn set_audio_redundancy_depth(&mut self, depth: usize) {
        self.audio_redundancy_depth = depth.min(self.audio_redundancy_max_depth);
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
            SendBackend::Tunnel(link) => {
                link.send_media(plaintext)?;
            }
        }
        Ok(())
    }

    /// Repoint the send socket at a new client address (B3 return-path relearn).
    /// All egress paths (plain send, sendmmsg, io_uring) operate on this one
    /// connected fd, so a single `connect()` retargets them together. No-op for
    /// the punched backend, whose peer is fixed by the hole punch.
    pub fn update_dest(&mut self, new_addr: SocketAddr) -> Result<(), String> {
        if let SendBackend::Direct { socket, .. } = &self.backend {
            socket
                .connect(new_addr)
                .map_err(|e| format!("reconnect UDP to {new_addr}: {e}"))?;
        }
        Ok(())
    }

    /// Send a header-only liveness keepalive on the media path. Used when no
    /// video is flowing (static screen → capture produces no frames) so the
    /// client can tell an idle path from a dead one. Works for both backends.
    pub fn send_keepalive(&mut self) -> Result<(), String> {
        let mut hdr = [0u8; HEADER_SIZE];
        PacketHeader {
            seq: 0,
            frame_id: 0,
            payload_type: PayloadType::Keepalive,
        }
        .serialize(&mut hdr);
        let backend = &self.backend;
        let encrypt_buf = &mut self.encrypt_buf;
        Self::send_bytes_with(backend, encrypt_buf, &hdr)
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
        // Resolve the duplicate-FrameStart decision before borrowing the slicer.
        let dup_first_allowed = dup_first_effective(self.dup_mode, self.dup_auto_active);
        let (packets, parity) = self.slicer.slice_with_meta_parts(
            &frame.data,
            frame_id,
            FrameTimingMeta {
                capture_ts_micros: frame.capture_micros,
                send_ts_micros: send_micros,
                video_epoch: frame.video_epoch,
            },
            st_protocol::packet::frame_type::from_is_recovery(frame.is_recovery),
        );
        self.frame_id = self.frame_id.wrapping_add(1);

        let resend_first_packet = packets.len() > 1 && dup_first_allowed;

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
            SendBackend::Tunnel(link) => {
                let _ = (encrypt_buf, encrypt_pool);
                #[cfg(target_os = "linux")]
                let _ = uring;
                for pt in plaintexts.iter() {
                    link.send_media(pt)?;
                }
                Ok(())
            }
        }
    }

    /// Send a single source-sequenced Opus packet, with up to
    /// `audio_redundancy_depth` contiguous previous packets attached. Source
    /// gaps clear history so stale packets are never labeled as the missing
    /// sequence slots. A gap also restores the configured redundancy cap, which
    /// lets pure audio pipeline loss re-enable protection without a wire change.
    pub fn send_audio(
        &mut self,
        packet: &EncodedAudioPacket,
        shared_depth: &AtomicU8,
    ) -> Result<(), String> {
        let source_gap = self
            .last_audio_source_seq
            .is_some_and(|last| packet.source_seq != last.wrapping_add(1));
        if source_gap {
            self.previous_audio.clear();
            self.audio_redundancy_depth = self.audio_redundancy_max_depth;
            shared_depth.store(self.audio_redundancy_max_depth as u8, Ordering::Relaxed);
        }
        self.last_audio_source_seq = Some(packet.source_seq);

        let backend = &self.backend;
        let encrypt_buf = &mut self.encrypt_buf;
        let header = PacketHeader {
            seq: packet.source_seq as u16,
            frame_id: 0,
            payload_type: PayloadType::Audio,
        };

        // Pick the largest k <= depth such that the resulting datagram still fits
        // inside max_datagram_size. The newest k chunks of `previous_audio` are
        // chosen, but we attach them in oldest-first order so the client can
        // index them as `seq - k .. seq - 1`.
        let primary_len = packet.data.len();
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
        self.audio_buf[primary_start..primary_end].copy_from_slice(&packet.data);
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
            self.previous_audio.push_back(packet.data.clone());
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
mod dup_first_tests {
    use super::{dup_first_effective, DupFirstMode};

    #[test]
    fn dup_first_auto_gates_on_recent_loss() {
        // Auto: duplicate only while loss is active — clean link pays nothing.
        assert!(!dup_first_effective(DupFirstMode::Auto, false));
        assert!(dup_first_effective(DupFirstMode::Auto, true));
        // Off never duplicates; On always does (legacy override).
        assert!(!dup_first_effective(DupFirstMode::Off, true));
        assert!(dup_first_effective(DupFirstMode::On, false));
    }
}

#[cfg(test)]
mod audio_redundancy_tests {
    use super::{audio_adaptive_redundancy_tristate, audio_redundancy_depth_from_var};

    #[test]
    fn adaptive_redundancy_requires_explicit_opt_in() {
        assert!(!audio_adaptive_redundancy_tristate(None), "unset => fixed");
        assert!(audio_adaptive_redundancy_tristate(Some("1")), "explicit on");
        assert!(audio_adaptive_redundancy_tristate(Some("yes")), "truthy on");
        assert!(
            !audio_adaptive_redundancy_tristate(Some("garbage")),
            "unknown => fixed"
        );
        assert!(!audio_adaptive_redundancy_tristate(Some("0")), "0 => fixed");
        assert!(
            !audio_adaptive_redundancy_tristate(Some("off")),
            "off => fixed"
        );
        assert!(
            !audio_adaptive_redundancy_tristate(Some(" False ")),
            "trim + case-insensitive off"
        );
    }

    #[test]
    fn verbatim_redundancy_defaults_to_depth_two() {
        assert_eq!(audio_redundancy_depth_from_var(None), 2);
        assert_eq!(audio_redundancy_depth_from_var(Some("4")), 4);
        assert_eq!(audio_redundancy_depth_from_var(Some("off")), 0);
    }
}

#[cfg(test)]
mod audio_source_sequence_tests {
    use super::{EncodedAudioPacket, UdpSender};
    use st_protocol::packet::{parse_audio_packet, HEADER_SIZE};
    use st_protocol::reliable_udp::PunchedMessage;
    use st_protocol::tcp_tunnel::TunnelLink;
    use st_protocol::PacketHeader;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    };
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingTunnel {
        packets: Mutex<Vec<Vec<u8>>>,
    }

    impl TunnelLink for RecordingTunnel {
        fn peer(&self) -> SocketAddr {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9))
        }

        fn is_reliable(&self) -> bool {
            false
        }

        fn max_media_payload(&self) -> usize {
            1_400
        }

        fn send_media(&self, data: &[u8]) -> Result<(), String> {
            self.packets.lock().unwrap().push(data.to_vec());
            Ok(())
        }

        fn send_control(&self, _data: &[u8]) -> Result<(), String> {
            Ok(())
        }

        fn try_recv(&self) -> Option<PunchedMessage> {
            None
        }

        fn try_recv_all(&self) -> Vec<PunchedMessage> {
            Vec::new()
        }

        fn recv_timeout(&self, _timeout: Duration) -> Option<PunchedMessage> {
            None
        }

        fn tick(&self) {}

        fn flush_control(&self, _timeout: Duration) -> Result<(), String> {
            Ok(())
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> Result<(), String> {
            Ok(())
        }

        fn set_read_timeout(&self, _dur: Option<Duration>) -> Result<(), String> {
            Ok(())
        }

        fn is_closed(&self) -> bool {
            false
        }
    }

    fn packet(source_seq: u64, data: u8) -> EncodedAudioPacket {
        EncodedAudioPacket {
            source_seq,
            data: vec![data],
        }
    }

    fn parse(raw: &[u8]) -> (u16, Vec<u8>, Vec<Vec<u8>>) {
        let header = PacketHeader::deserialize(raw).unwrap();
        let audio = parse_audio_packet(&raw[HEADER_SIZE..]).unwrap();
        (
            header.seq,
            audio.primary.to_vec(),
            audio.redundant.iter().map(|chunk| chunk.to_vec()).collect(),
        )
    }

    #[test]
    fn source_gap_reaches_wire_and_clears_then_restores_history() {
        let tunnel = Arc::new(RecordingTunnel::default());
        let mut sender = UdpSender::from_tunnel(tunnel.clone());
        sender.audio_redundancy_max_depth = 2;
        sender.set_audio_redundancy_depth(2);
        let shared_depth = AtomicU8::new(2);

        sender
            .send_audio(&packet(100, b'a'), &shared_depth)
            .unwrap();
        sender
            .send_audio(&packet(101, b'b'), &shared_depth)
            .unwrap();
        // Simulate adaptive depth reaching zero before a pure audio source gap.
        sender.set_audio_redundancy_depth(0);
        shared_depth.store(0, Ordering::Relaxed);
        sender
            .send_audio(&packet(104, b'd'), &shared_depth)
            .unwrap();
        assert_eq!(shared_depth.load(Ordering::Relaxed), 2);
        sender
            .send_audio(&packet(105, b'e'), &shared_depth)
            .unwrap();
        sender
            .send_audio(&packet(106, b'f'), &shared_depth)
            .unwrap();

        let packets = tunnel.packets.lock().unwrap();
        let parsed = packets.iter().map(|raw| parse(raw)).collect::<Vec<_>>();
        assert_eq!(
            parsed.iter().map(|packet| packet.0).collect::<Vec<_>>(),
            [100, 101, 104, 105, 106]
        );
        assert!(
            parsed[2].2.is_empty(),
            "source gap must clear stale history"
        );
        assert_eq!(parsed[3].2, [vec![b'd']]);
        assert_eq!(parsed[4].2, [vec![b'd'], vec![b'e']]);
    }
}
