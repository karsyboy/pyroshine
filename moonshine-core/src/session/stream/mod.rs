use serde::Deserialize;
use serde::Serialize;

use crate::session::stream::audio::AudioStreamConfig;
use crate::session::stream::control::ControlStreamConfig;
use crate::session::stream::video::VideoStreamConfig;

pub mod audio;
pub mod control;
pub mod video;

#[cfg(test)]
pub(crate) mod test_support;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamConfig {
	/// Port to bind the RTSP server to.
	pub port: u16,

	/// Configuration for the video stream.
	pub video: VideoStreamConfig,

	/// Configuration for the audio stream.
	pub audio: AudioStreamConfig,

	/// Configuration for the control stream.
	pub control: ControlStreamConfig,

	/// Seconds the active client may go without a control ping before it is
	/// treated as gone. Its input is released and media delivery paused, but
	/// the session and application keep running for a later resume.
	pub timeout: u64,
}

impl Default for StreamConfig {
	fn default() -> Self {
		Self {
			port: 48010,
			video: Default::default(),
			audio: Default::default(),
			control: Default::default(),
			timeout: 60,
		}
	}
}

#[derive(Debug)]
#[repr(C)]
struct RtpHeader {
	header: u8,
	packet_type: u8,
	sequence_number: u16,
	timestamp: u32,
	ssrc: u32,
}

impl RtpHeader {}

/// Whether a client can currently receive a stream's media (review
/// 2026-10-05 PERF-001). Each packet handler clears it on every pause (client
/// detached or reconnecting) and sets it when an epoch is activated. While it is
/// clear, the video encoder requests no captures and replays nothing, and the
/// audio encoder recycles PCM without encoding it, so capture, conversion,
/// encoding and packetization stop after in-flight work drains. The compositor
/// keeps presenting to the application and the Pulse server keeps serving it.
#[derive(Clone, Debug)]
pub(crate) struct MediaDemand(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl MediaDemand {
	pub(crate) fn new() -> Self {
		Self(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)))
	}

	pub(crate) fn wanted(&self) -> bool {
		self.0.load(std::sync::atomic::Ordering::Acquire)
	}

	pub(crate) fn set(&self, wanted: bool) {
		self.0.store(wanted, std::sync::atomic::Ordering::Release);
	}
}
