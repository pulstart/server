use st_protocol::packet::HEADER_SIZE;
use st_protocol::{FrameSlicer, FrameTimingMeta, PacketHeader, PayloadType};
use std::net::{SocketAddr, UdpSocket};

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
        Ok(Self {
            socket,
            slicer: FrameSlicer::new(),
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

        for pkt in packets {
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
