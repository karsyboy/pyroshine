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
/// detached or reconnecting). Video demand requires an activated epoch, StartB
/// and the current client's discovered endpoint; audio enables it at activation. While it is
/// clear, the video encoder requests no captures and replays nothing, and the
/// audio encoder recycles PCM without encoding it, so capture, conversion,
/// encoding and packetization stop after in-flight work drains. The compositor
/// keeps presenting to the application and the Pulse server keeps serving it.
///
/// It also wakes an encoder that sleeps while demand is clear: changing demand,
/// and [`MediaDemand::wake`] for any other command the encoder must see (epoch
/// activation, start, stop), end [`MediaDemand::wait_for_wake`].
#[derive(Clone, Debug)]
pub(crate) struct MediaDemand(std::sync::Arc<DemandState>);

#[derive(Debug)]
struct DemandState {
	wanted: std::sync::atomic::AtomicBool,
	/// Wakes so far; a waiter compares it with the count it last saw, so a wake
	/// between its check and its wait is never lost.
	wakes: std::sync::Mutex<u64>,
	woken: std::sync::Condvar,
}

impl MediaDemand {
	pub(crate) fn new() -> Self {
		Self(std::sync::Arc::new(DemandState {
			wanted: std::sync::atomic::AtomicBool::new(true),
			wakes: std::sync::Mutex::new(0),
			woken: std::sync::Condvar::new(),
		}))
	}

	pub(crate) fn wanted(&self) -> bool {
		self.0.wanted.load(std::sync::atomic::Ordering::Acquire)
	}

	pub(crate) fn set(&self, wanted: bool) {
		if self.0.wanted.swap(wanted, std::sync::atomic::Ordering::AcqRel) != wanted {
			self.wake();
		}
	}

	/// Wake a sleeping encoder so it re-reads its commands.
	pub(crate) fn wake(&self) {
		*self.0.wakes.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
		self.0.woken.notify_all();
	}

	/// The wake count to pass to [`Self::wait_for_wake`], read before the
	/// caller checks the state it would otherwise sleep on.
	pub(crate) fn wakes(&self) -> u64 {
		*self.0.wakes.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Sleep until a wake after `seen` or `timeout`.
	pub(crate) fn wait_for_wake(&self, seen: u64, timeout: std::time::Duration) {
		let guard = self.0.wakes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let _ = self
			.0
			.woken
			.wait_timeout_while(guard, timeout, |wakes| *wakes == seen)
			.unwrap_or_else(std::sync::PoisonError::into_inner);
	}
}
