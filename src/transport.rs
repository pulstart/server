use st_protocol::packet::HEADER_SIZE;
use st_protocol::{FrameSlicer, FrameTimingMeta, PacketHeader, PayloadType};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

const LAN_MAX_UDP: usize = 1400;
const SAFE_PATH_MAX_UDP: usize = 1200;

pub struct EncodedVideoFrame {
    pub data: Vec<u8>,
    pub capture_micros: u64,
}

pub struct UdpSender {
    socket: UdpSocket,
    slicer: FrameSlicer,
    frame_id: u32,
    audio_seq: u16,
    audio_buf: Vec<u8>,
}

impl UdpSender {
    pub fn new(client_addr: SocketAddr) -> Result<Self, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind UDP: {e}"))?;
        socket
            .connect(client_addr)
            .map_err(|e| format!("connect UDP: {e}"))?;
        let max_udp = select_max_udp_packet_size(client_addr);
        if std::env::var_os("ST_TRACE").is_some() || max_udp != LAN_MAX_UDP {
            eprintln!(
                "[transport] UDP max packet size {} bytes for {}",
                max_udp, client_addr
            );
        }
        Ok(Self {
            socket,
            slicer: FrameSlicer::with_max_udp(max_udp),
            frame_id: 0,
            audio_seq: 0,
            audio_buf: Vec::with_capacity(1400),
        })
    }

    /// Send a single NAL unit as sliced UDP packets (video).
    pub fn send_frame(
        &mut self,
        frame: &EncodedVideoFrame,
        send_micros: u64,
    ) -> Result<(), String> {
        let packets = self.slicer.slice_with_meta(
            &frame.data,
            self.frame_id,
            FrameTimingMeta {
                capture_ts_micros: frame.capture_micros,
                send_ts_micros: send_micros,
            },
        );
        self.frame_id = self.frame_id.wrapping_add(1);

        let mut delayed_duplicate = None;
        for (idx, pkt) in packets.iter().enumerate() {
            if idx == 0 && packets.len() > 1 {
                delayed_duplicate = Some(pkt.clone());
            }
            self.socket.send(pkt).map_err(|e| format!("send: {e}"))?;
        }
        if let Some(parity) = self.slicer.parity_packet() {
            self.socket.send(parity).map_err(|e| format!("send: {e}"))?;
        }
        if let Some(pkt) = delayed_duplicate.as_deref() {
            self.socket.send(pkt).map_err(|e| format!("send: {e}"))?;
        }
        Ok(())
    }

    /// Send a single Opus audio packet.
    pub fn send_audio(&mut self, opus_data: &[u8]) -> Result<(), String> {
        let header = PacketHeader {
            seq: self.audio_seq,
            frame_id: 0,
            payload_type: PayloadType::Audio,
        };
        self.audio_seq = self.audio_seq.wrapping_add(1);

        self.audio_buf.clear();
        self.audio_buf.resize(HEADER_SIZE + opus_data.len(), 0);
        header.serialize(&mut self.audio_buf[..HEADER_SIZE]);
        self.audio_buf[HEADER_SIZE..].copy_from_slice(opus_data);

        self.socket
            .send(&self.audio_buf)
            .map_err(|e| format!("send: {e}"))?;
        Ok(())
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
        SAFE_PATH_MAX_UDP
    } else {
        LAN_MAX_UDP
    }
}

fn prefers_safe_udp_path(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.octets()[0] == Ipv4Addr::BROADCAST.octets()[0])
        }
        IpAddr::V6(v6) => !(v6.is_loopback()
            || v6.is_unique_local()
            || v6.is_unicast_link_local()),
    }
}
