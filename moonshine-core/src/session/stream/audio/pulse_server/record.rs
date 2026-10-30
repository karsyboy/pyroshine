//! The session's microphone as a PulseAudio source.
//!
//! When the microphone is on, the server offers one source, the default,
//! that plays the client's microphone ([`super::super::microphone`]) and is
//! silent while no client sends one. Applications record from it with
//! ordinary record streams in the format they ask for: each clock tick pulls
//! one tick of mono 48 kHz audio from the feed, converts it for every running
//! stream and sends whole fragments.

use std::ffi::CString;

use pulseaudio::protocol as pulse;
use rubato::Resampler as _;
use rubato::audioadapter_buffers::direct::InterleavedSlice;

use crate::session::stream::audio::microphone::MICROPHONE_SAMPLE_RATE;

/// The source's name, also the server's default source.
pub(super) const SOURCE_NAME: &str = "moonshine_mic";
/// The source's index (sources and sinks are numbered separately).
pub(super) const SOURCE_INDEX: u32 = 1;

/// Longest fragment a record stream may ask for.
const MAX_FRAGMENT_MS: u64 = 2000;
/// Fragment of a stream that leaves it to the server.
const DEFAULT_FRAGMENT_MS: u64 = 10;

fn source_spec() -> pulse::SampleSpec {
	pulse::SampleSpec {
		format: pulse::SampleFormat::Float32Le,
		channels: 1,
		sample_rate: MICROPHONE_SAMPLE_RATE,
	}
}

fn source_format() -> pulse::FormatInfo {
	let mut props = pulse::Props::new();
	for (prop, value) in [
		(pulse::Prop::FormatChannels, "1"),
		(pulse::Prop::FormatChannelMap, "mono"),
		(pulse::Prop::FormatSampleFormat, "float32le"),
	] {
		props.set(prop, CString::new(value).unwrap());
	}
	props.set(
		pulse::Prop::FormatRate,
		CString::new(MICROPHONE_SAMPLE_RATE.to_string()).unwrap(),
	);
	pulse::FormatInfo {
		encoding: pulse::FormatEncoding::Pcm,
		props,
	}
}

/// The microphone source as `GetSourceInfo` reports it.
pub(super) fn source_info() -> pulse::SourceInfo {
	// No ports: pulseaudio 0.3.1 writes a source's ports without the
	// protocol 34 fields (availability group, type), which libpulse rejects
	// as a protocol error.
	pulse::SourceInfo {
		index: SOURCE_INDEX,
		name: CString::new(SOURCE_NAME).unwrap(),
		description: Some(CString::new("Moonshine client microphone").unwrap()),
		sample_spec: source_spec(),
		channel_map: pulse::ChannelMap::mono(),
		cvolume: pulse::ChannelVolume::norm(1),
		base_volume: pulse::Volume::NORM,
		configured_latency: DEFAULT_FRAGMENT_MS * 1000,
		formats: vec![source_format()],
		..Default::default()
	}
}

/// Whether a record stream's source selection names the microphone.
pub(super) fn selects_source(index: Option<u32>, name: Option<&CString>) -> bool {
	match (index, name.map(|name| name.as_bytes())) {
		(Some(index), _) => index == SOURCE_INDEX,
		(None, None | Some(b"" | b"@DEFAULT_SOURCE@")) => true,
		(None, Some(name)) => name == SOURCE_NAME.as_bytes(),
	}
}

/// Negotiate record buffer attributes (only `max_length` and `fragment_size`
/// apply), whole frames throughout.
pub(super) fn configure_record_buffer(attr: &mut pulse::stream::BufferAttr, spec: &pulse::SampleSpec) {
	let frame = u64::from(spec.channels) * spec.format.bytes_per_sample() as u64;
	let frame = frame.max(1);
	let per_ms = |ms: u64| (u64::from(spec.sample_rate) * ms / 1000).max(1) * frame;
	let fragment = match attr.fragment_size {
		0 | u32::MAX => per_ms(DEFAULT_FRAGMENT_MS),
		requested => (u64::from(requested) / frame * frame).clamp(frame, per_ms(MAX_FRAGMENT_MS)),
	};
	let max_length = match attr.max_length {
		0 | u32::MAX => per_ms(MAX_FRAGMENT_MS) * 2,
		requested => (u64::from(requested) / frame * frame).clamp(fragment, per_ms(MAX_FRAGMENT_MS) * 2),
	};
	let narrow = |bytes: u64| u32::try_from(bytes).unwrap_or(u32::MAX);
	attr.fragment_size = narrow(fragment);
	attr.max_length = narrow(max_length);
	attr.target_length = u32::MAX;
	attr.pre_buffering = u32::MAX;
	attr.minimum_request_length = u32::MAX;
}

pub(super) struct RecordStream {
	pub stream_index: u32,
	pub spec: pulse::SampleSpec,
	pub channel_map: pulse::ChannelMap,
	pub buffer_attr: pulse::stream::BufferAttr,
	pub corked: bool,
	pub muted: bool,
	pub volume: f32,
	/// Converted audio not sent yet, less than one fragment.
	pending: Vec<u8>,
	/// Bytes sent to the client.
	pub sent: u64,
	/// For a stream at another rate than the source.
	resampler: Option<rubato::Async<f32>>,
	resampled: Vec<f32>,
}

impl RecordStream {
	/// A stream for an accepted `spec`; fails when its rate cannot be resampled.
	pub fn new(
		stream_index: u32,
		spec: pulse::SampleSpec,
		channel_map: pulse::ChannelMap,
		buffer_attr: pulse::stream::BufferAttr,
		corked: bool,
		tick_frames: usize,
	) -> Result<Self, String> {
		let mut stream = Self {
			stream_index,
			spec,
			channel_map,
			buffer_attr,
			corked,
			muted: false,
			volume: 1.0,
			pending: Vec::new(),
			sent: 0,
			resampler: None,
			resampled: Vec::new(),
		};
		stream.set_tick_frames(tick_frames)?;
		Ok(stream)
	}

	/// The clock's tick size, fixed unless the packet duration changes.
	pub fn set_tick_frames(&mut self, tick_frames: usize) -> Result<(), String> {
		if self.spec.sample_rate == MICROPHONE_SAMPLE_RATE {
			self.resampler = None;
			return Ok(());
		}
		let resampler = rubato::Async::<f32>::new_sinc(
			f64::from(self.spec.sample_rate) / f64::from(MICROPHONE_SAMPLE_RATE),
			1.0,
			&rubato::SincInterpolationParameters::default(),
			tick_frames,
			1,
			rubato::FixedAsync::Input,
		)
		.map_err(|e| {
			format!(
				"cannot resample {MICROPHONE_SAMPLE_RATE} Hz to {} Hz: {e}",
				self.spec.sample_rate
			)
		})?;
		self.resampled = vec![0.0; resampler.output_frames_max()];
		self.resampler = Some(resampler);
		Ok(())
	}

	/// Drop audio converted but not sent.
	pub fn flush(&mut self) {
		self.pending.clear();
	}

	pub fn pending_bytes(&self) -> usize {
		self.pending.len()
	}

	/// Convert one tick of source audio and return the bytes to send, if a
	/// whole fragment is ready.
	pub fn push(&mut self, mono: &[f32]) -> Option<Vec<u8>> {
		let gain = if self.muted { 0.0 } else { self.volume };
		let samples: &[f32] = match &mut self.resampler {
			None => mono,
			Some(resampler) => {
				let frames = mono.len();
				let output_frames = self.resampled.len();
				let input = InterleavedSlice::new(mono, 1, frames).ok()?;
				let mut output = InterleavedSlice::new_mut(self.resampled.as_mut_slice(), 1, output_frames).ok()?;
				match resampler.process_into_buffer(&input, &mut output, None) {
					Ok((_, written)) => &self.resampled[..written],
					Err(error) => {
						tracing::error!(%error, "Failed to resample the microphone");
						return None;
					},
				}
			},
		};
		let channels = usize::from(self.spec.channels);
		self.pending
			.reserve(samples.len() * channels * self.spec.format.bytes_per_sample());
		for &sample in samples {
			let sample = (sample * gain).clamp(-1.0, 1.0);
			for _ in 0..channels {
				encode(self.spec.format, sample, &mut self.pending);
			}
		}
		if self.pending.len() < self.buffer_attr.fragment_size as usize {
			return None;
		}
		self.sent += self.pending.len() as u64;
		Some(std::mem::take(&mut self.pending))
	}

	pub fn source_output_info(&self, client_id: u32) -> pulse::SourceOutputInfo {
		pulse::SourceOutputInfo {
			index: self.stream_index,
			name: CString::new(format!("record-{}", self.stream_index)).unwrap(),
			client_index: Some(client_id),
			source_index: SOURCE_INDEX,
			sample_spec: self.spec,
			channel_map: self.channel_map,
			cvolume: {
				let mut volume = pulse::ChannelVolume::empty();
				for _ in 0..self.spec.channels {
					volume.push(pulse::Volume::from_linear(self.volume));
				}
				volume
			},
			muted: self.muted,
			corked: self.corked,
			has_volume: true,
			volume_writable: true,
			..Default::default()
		}
	}
}

/// Append one sample, within -1..=1, in a supported Pulse sample format.
fn encode(format: pulse::SampleFormat, sample: f32, out: &mut Vec<u8>) {
	use pulse::SampleFormat::*;
	// Float-to-integer `as` saturates, so full scale cannot wrap.
	let int = |bits: u32| (f64::from(sample) * ((1i64 << (bits - 1)) - 1) as f64).round() as i32;
	match format {
		U8 => out.push((int(8) + 128) as u8),
		S16Le => out.extend_from_slice(&(int(16) as i16).to_le_bytes()),
		S16Be => out.extend_from_slice(&(int(16) as i16).to_be_bytes()),
		Float32Le => out.extend_from_slice(&sample.to_le_bytes()),
		Float32Be => out.extend_from_slice(&sample.to_be_bytes()),
		S32Le => out.extend_from_slice(&int(32).to_le_bytes()),
		S32Be => out.extend_from_slice(&int(32).to_be_bytes()),
		S24Le => out.extend_from_slice(&int(24).to_le_bytes()[..3]),
		S24Be => out.extend_from_slice(&int(24).to_be_bytes()[1..]),
		// Rejected when the stream is created.
		_ => {},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn spec(format: pulse::SampleFormat, channels: u8, sample_rate: u32) -> pulse::SampleSpec {
		pulse::SampleSpec {
			format,
			channels,
			sample_rate,
		}
	}

	fn stream(spec: pulse::SampleSpec, fragment_size: u32) -> RecordStream {
		let mut attr = pulse::stream::BufferAttr {
			fragment_size,
			max_length: u32::MAX,
			..Default::default()
		};
		configure_record_buffer(&mut attr, &spec);
		RecordStream::new(3, spec, pulse::ChannelMap::default(), attr, false, 480).unwrap()
	}

	#[test]
	fn the_default_and_named_sources_select_the_microphone() {
		assert!(selects_source(None, None));
		assert!(selects_source(None, Some(&CString::new("@DEFAULT_SOURCE@").unwrap())));
		assert!(selects_source(None, Some(&CString::new(SOURCE_NAME).unwrap())));
		assert!(selects_source(Some(SOURCE_INDEX), None));
		assert!(!selects_source(None, Some(&CString::new("moonshine.monitor").unwrap())));
		assert!(!selects_source(Some(7), None));
	}

	#[test]
	fn buffer_attributes_are_whole_frames_within_bounds() {
		let stereo16 = spec(pulse::SampleFormat::S16Le, 2, 44100);
		for (fragment, max) in [
			(u32::MAX, u32::MAX),
			(0, 0),
			(1, 1),
			(4099, 3),
			(u32::MAX - 1, u32::MAX - 1),
		] {
			let mut attr = pulse::stream::BufferAttr {
				fragment_size: fragment,
				max_length: max,
				..Default::default()
			};
			configure_record_buffer(&mut attr, &stereo16);
			assert_eq!(attr.fragment_size % 4, 0);
			assert_eq!(attr.max_length % 4, 0);
			assert!(attr.fragment_size >= 4 && attr.fragment_size <= attr.max_length);
			assert!(u64::from(attr.fragment_size) <= 44100 * 4 * 2);
		}
		let mut attr = pulse::stream::BufferAttr {
			fragment_size: u32::MAX,
			max_length: u32::MAX,
			..Default::default()
		};
		configure_record_buffer(&mut attr, &stereo16);
		assert_eq!(attr.fragment_size, 441 * 4);
	}

	#[test]
	fn ticks_become_fragments_in_the_streams_format() {
		// Stereo s16 at the source rate, 20 ms fragments: every second tick sends.
		let mut s16 = stream(spec(pulse::SampleFormat::S16Le, 2, 48000), 1920 * 2);
		let tick = vec![0.5f32; 480];
		assert!(s16.push(&tick).is_none());
		let sent = s16.push(&tick).unwrap();
		assert_eq!(sent.len(), 960 * 2 * 2);
		assert_eq!(i16::from_le_bytes([sent[0], sent[1]]), 16384);
		assert_eq!(&sent[0..2], &sent[2..4], "mono is copied to both channels");
		assert_eq!(s16.sent, sent.len() as u64);

		s16.muted = true;
		let silent = [s16.push(&tick), s16.push(&tick)];
		assert!(silent[1].as_ref().unwrap().iter().all(|&b| b == 0));

		for (format, bytes) in [
			(pulse::SampleFormat::U8, 1),
			(pulse::SampleFormat::S16Be, 2),
			(pulse::SampleFormat::S24Le, 3),
			(pulse::SampleFormat::S24Be, 3),
			(pulse::SampleFormat::S32Le, 4),
			(pulse::SampleFormat::S32Be, 4),
			(pulse::SampleFormat::Float32Le, 4),
			(pulse::SampleFormat::Float32Be, 4),
		] {
			let mut stream = stream(spec(format, 1, 48000), 1);
			assert_eq!(stream.push(&[1.0, -1.0, 0.0]).unwrap().len(), 3 * bytes, "{format:?}");
		}
	}

	#[test]
	fn other_rates_are_resampled() {
		let mut voice = stream(spec(pulse::SampleFormat::S16Le, 1, 16000), u32::MAX);
		let tick = vec![0.0f32; 480];
		let mut bytes = 0;
		for _ in 0..100 {
			bytes += voice.push(&tick).map_or(0, |sent| sent.len());
		}
		bytes += voice.pending_bytes();
		// One second at 16 kHz, less the resampler's delay.
		let frames = bytes / 2;
		assert!((15_000..=16_000).contains(&frames), "{frames} frames");
	}
}
