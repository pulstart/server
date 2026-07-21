use st_protocol::{VideoChromaSampling, VideoCodec, VideoCodecSupport};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientVideoCapabilities {
    pub supported_codecs: VideoCodecSupport,
    pub hardware_codecs: VideoCodecSupport,
    pub supported_yuv444_codecs: VideoCodecSupport,
    pub hardware_yuv444_codecs: VideoCodecSupport,
    pub hdr_display: bool,
    pub requested_fps: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AggregateVideoCapabilities {
    pub supported_codecs: VideoCodecSupport,
    pub hardware_codecs: VideoCodecSupport,
    pub supported_yuv444_codecs: VideoCodecSupport,
    pub hardware_yuv444_codecs: VideoCodecSupport,
    pub hdr_display: bool,
    pub requested_fps: Option<u32>,
}

impl AggregateVideoCapabilities {
    pub fn from_clients(
        clients: impl IntoIterator<Item = ClientVideoCapabilities>,
    ) -> Result<Self, String> {
        let mut clients = clients.into_iter();
        let first = clients
            .next()
            .ok_or_else(|| "no video clients to aggregate".to_string())?;
        let mut aggregate = Self {
            supported_codecs: first.supported_codecs,
            hardware_codecs: first.hardware_codecs,
            supported_yuv444_codecs: first.supported_yuv444_codecs,
            hardware_yuv444_codecs: first.hardware_yuv444_codecs,
            hdr_display: first.hdr_display,
            requested_fps: first.requested_fps,
        };
        for client in clients {
            aggregate.supported_codecs =
                intersect_support(aggregate.supported_codecs, client.supported_codecs);
            aggregate.hardware_codecs =
                intersect_support(aggregate.hardware_codecs, client.hardware_codecs);
            aggregate.supported_yuv444_codecs = intersect_support(
                aggregate.supported_yuv444_codecs,
                client.supported_yuv444_codecs,
            );
            aggregate.hardware_yuv444_codecs = intersect_support(
                aggregate.hardware_yuv444_codecs,
                client.hardware_yuv444_codecs,
            );
            aggregate.hdr_display &= client.hdr_display;
            aggregate.requested_fps =
                minimum_optional_fps(aggregate.requested_fps, client.requested_fps);
        }
        if aggregate.supported_codecs.is_empty() {
            return Err("No video codec is supported by every connected client".into());
        }
        Ok(aggregate)
    }

    pub fn preferred_codec(self, order: [VideoCodec; 3]) -> Option<VideoCodec> {
        order
            .into_iter()
            .find(|codec| self.supported_codecs.supports(*codec))
    }

    /// Prefer a codec every client can decode in hardware before following the
    /// normal preference order through software-only common codecs.
    pub fn preferred_codec_hardware_first(self, order: [VideoCodec; 3]) -> Option<VideoCodec> {
        order
            .into_iter()
            .find(|codec| self.hardware_codecs.supports(*codec))
            .or_else(|| self.preferred_codec(order))
    }

    pub fn preferred_chroma(self, codec: VideoCodec, hdr: bool) -> VideoChromaSampling {
        if !hdr && codec != VideoCodec::Av1 && self.supported_yuv444_codecs.supports(codec) {
            VideoChromaSampling::Yuv444
        } else {
            VideoChromaSampling::Yuv420
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MembershipState {
    Tentative,
    Active,
}

#[derive(Default)]
pub struct VideoCapabilityRegistry {
    next_id: u64,
    revision: u64,
    members: BTreeMap<u64, (ClientVideoCapabilities, MembershipState)>,
}

impl VideoCapabilityRegistry {
    pub fn insert_tentative(&mut self, capabilities: ClientVideoCapabilities) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.members
            .insert(id, (capabilities, MembershipState::Tentative));
        self.advance_revision();
        id
    }

    pub fn activate_if_revision(&mut self, id: u64, expected_revision: u64) -> bool {
        if self.revision != expected_revision {
            return false;
        }
        let Some((_, state)) = self.members.get_mut(&id) else {
            return false;
        };
        if *state != MembershipState::Tentative {
            return false;
        }
        *state = MembershipState::Active;
        self.advance_revision();
        true
    }

    pub fn remove(&mut self, id: u64) -> Option<MembershipState> {
        let removed = self.members.remove(&id).map(|(_, state)| state);
        if removed.is_some() {
            self.advance_revision();
        }
        removed
    }

    pub fn aggregate(&self) -> Result<AggregateVideoCapabilities, String> {
        AggregateVideoCapabilities::from_clients(
            self.members.values().map(|(capabilities, _)| *capabilities),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_tentative(&self, id: u64) -> bool {
        self.members
            .get(&id)
            .is_some_and(|(_, state)| *state == MembershipState::Tentative)
    }

    fn advance_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1).max(1);
    }
}

fn intersect_support(left: VideoCodecSupport, right: VideoCodecSupport) -> VideoCodecSupport {
    let mut result = VideoCodecSupport::empty();
    for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
        if left.supports(codec) && right.supports(codec) {
            result.insert(codec);
        }
    }
    result
}

fn minimum_optional_fps(left: Option<u32>, right: Option<u32>) -> Option<u32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn support(codecs: &[VideoCodec]) -> VideoCodecSupport {
        let mut support = VideoCodecSupport::empty();
        for codec in codecs {
            support.insert(*codec);
        }
        support
    }

    fn desktop() -> ClientVideoCapabilities {
        let codecs = support(&[VideoCodec::H264, VideoCodec::Hevc]);
        ClientVideoCapabilities {
            supported_codecs: codecs,
            hardware_codecs: codecs,
            supported_yuv444_codecs: codecs,
            hardware_yuv444_codecs: codecs,
            hdr_display: true,
            requested_fps: Some(120),
        }
    }

    fn android() -> ClientVideoCapabilities {
        ClientVideoCapabilities {
            supported_codecs: VideoCodecSupport::h264_only(),
            hardware_codecs: VideoCodecSupport::h264_only(),
            supported_yuv444_codecs: VideoCodecSupport::empty(),
            hardware_yuv444_codecs: VideoCodecSupport::empty(),
            hdr_display: false,
            requested_fps: Some(60),
        }
    }

    #[test]
    fn aggregate_intersects_every_profile_dimension() {
        let aggregate = AggregateVideoCapabilities::from_clients([desktop(), android()]).unwrap();
        assert_eq!(
            aggregate.preferred_codec([VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264]),
            Some(VideoCodec::H264)
        );
        assert_eq!(
            aggregate.preferred_chroma(VideoCodec::H264, false),
            VideoChromaSampling::Yuv420
        );
        assert!(!aggregate.hdr_display);
        assert_eq!(aggregate.requested_fps, Some(60));
    }

    #[test]
    fn android_join_downgrades_and_removal_upgrades() {
        let mut registry = VideoCapabilityRegistry::default();
        let desktop_id = registry.insert_tentative(desktop());
        assert!(registry.activate_if_revision(desktop_id, registry.revision()));
        assert_eq!(
            registry.aggregate().unwrap().preferred_codec([
                VideoCodec::Av1,
                VideoCodec::Hevc,
                VideoCodec::H264
            ]),
            Some(VideoCodec::Hevc)
        );

        let android_id = registry.insert_tentative(android());
        assert_eq!(
            registry.aggregate().unwrap().preferred_codec([
                VideoCodec::Av1,
                VideoCodec::Hevc,
                VideoCodec::H264
            ]),
            Some(VideoCodec::H264)
        );
        assert_eq!(
            registry.remove(android_id),
            Some(MembershipState::Tentative)
        );
        assert_eq!(
            registry.aggregate().unwrap().preferred_codec([
                VideoCodec::Av1,
                VideoCodec::Hevc,
                VideoCodec::H264
            ]),
            Some(VideoCodec::Hevc)
        );
    }

    #[test]
    fn no_common_codec_rolls_back_tentative_member() {
        let mut registry = VideoCapabilityRegistry::default();
        let desktop_id = registry.insert_tentative(desktop());
        assert!(registry.activate_if_revision(desktop_id, registry.revision()));
        let av1_only = ClientVideoCapabilities {
            supported_codecs: support(&[VideoCodec::Av1]),
            hardware_codecs: support(&[VideoCodec::Av1]),
            supported_yuv444_codecs: VideoCodecSupport::empty(),
            hardware_yuv444_codecs: VideoCodecSupport::empty(),
            hdr_display: false,
            requested_fps: Some(30),
        };
        let joining_id = registry.insert_tentative(av1_only);
        assert!(registry.aggregate().is_err());
        assert_eq!(
            registry.remove(joining_id),
            Some(MembershipState::Tentative)
        );
        assert_eq!(
            registry.aggregate().unwrap().preferred_codec([
                VideoCodec::Av1,
                VideoCodec::Hevc,
                VideoCodec::H264
            ]),
            Some(VideoCodec::Hevc)
        );
    }

    #[test]
    fn hardware_codec_wins_over_earlier_software_only_preference() {
        let aggregate = AggregateVideoCapabilities {
            supported_codecs: support(&[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]),
            hardware_codecs: support(&[VideoCodec::H264, VideoCodec::Hevc]),
            supported_yuv444_codecs: VideoCodecSupport::empty(),
            hardware_yuv444_codecs: VideoCodecSupport::empty(),
            hdr_display: false,
            requested_fps: Some(60),
        };

        assert_eq!(
            aggregate.preferred_codec_hardware_first([
                VideoCodec::Av1,
                VideoCodec::Hevc,
                VideoCodec::H264
            ]),
            Some(VideoCodec::Hevc)
        );
    }

    #[test]
    fn stale_revision_cannot_activate_tentative_member() {
        let mut registry = VideoCapabilityRegistry::default();
        let first = registry.insert_tentative(desktop());
        let stale_revision = registry.revision();
        let second = registry.insert_tentative(android());

        assert!(!registry.activate_if_revision(first, stale_revision));
        assert!(registry.is_tentative(first));
        assert!(registry.activate_if_revision(second, registry.revision()));
    }
}
