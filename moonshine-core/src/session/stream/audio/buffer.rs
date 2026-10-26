use std::{collections::VecDeque, io};

use byteorder::{BigEndian as BE, LittleEndian as LE, ReadBytesExt as _};
use pulseaudio::protocol::{self as pulse, ChannelPosition};
use rubato::Resampler as _;
use rubato::audioadapter_buffers::direct::InterleavedSlice;

/// Output frames per resampler call until the first drain sets the clock's
/// tick size: 5 ms at the 48 kHz sink rate.
const INITIAL_RESAMPLE_FRAMES: usize = 240;

/// Raw bytes go in; frames in the sink's speaker layout, at the sink's rate,
/// come out.
pub(crate) enum PlaybackBuffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	Passthrough(Buffer<F>),
	Resampling(Box<Resampling<F>>),
}

/// A stream at another rate than the sink: its frames are routed to the
/// sink's speakers first, then converted with rubato's asynchronous sinc
/// resampler. (dasp's 32-tap sinc converter, used before, reached only about
/// 21 dB SNR for a 44.1 kHz stream, which made dialog sound robotic.)
pub(crate) struct Resampling<F>
where
	F: dasp::Frame<Sample = f32>,
{
	buffer: Buffer<F>,
	output_rate: u32,
	resampler: rubato::Async<f32>,
	/// Interleaved scratch reused by every call: remixed input and resampled output.
	input: Vec<f32>,
	output: Vec<f32>,
}

impl<F> PlaybackBuffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	/// Fails when rubato cannot convert between the two rates, so the stream
	/// can be refused instead of the audio thread failing later.
	pub fn new(
		sample_spec: pulse::SampleSpec,
		channel_map: pulse::ChannelMap,
		output_spec: pulse::SampleSpec,
	) -> Result<Self, String> {
		assert_eq!(output_spec.channels as usize, F::CHANNELS);

		let buffer = Buffer::new(sample_spec, channel_map);
		if sample_spec.sample_rate == output_spec.sample_rate {
			return Ok(Self::Passthrough(buffer));
		}
		let resampler = new_resampler(
			sample_spec.sample_rate,
			output_spec.sample_rate,
			INITIAL_RESAMPLE_FRAMES,
			F::CHANNELS,
		)
		.map_err(|e| {
			format!(
				"cannot resample {} Hz to {} Hz: {e}",
				sample_spec.sample_rate, output_spec.sample_rate
			)
		})?;
		Ok(Self::Resampling(Box::new(Resampling {
			buffer,
			output_rate: output_spec.sample_rate,
			resampler,
			input: Vec::new(),
			output: Vec::new(),
		})))
	}

	pub fn buffer(&self) -> &Buffer<F> {
		match self {
			PlaybackBuffer::Passthrough(buffer) => buffer,
			PlaybackBuffer::Resampling(resampling) => &resampling.buffer,
		}
	}

	fn buffer_mut(&mut self) -> &mut Buffer<F> {
		match self {
			PlaybackBuffer::Passthrough(buffer) => buffer,
			PlaybackBuffer::Resampling(resampling) => &mut resampling.buffer,
		}
	}

	pub fn len_bytes(&self) -> usize {
		self.buffer().len_bytes()
	}

	#[cfg(test)]
	pub fn is_empty(&self) -> bool {
		self.buffer().len_frames() == 0
	}

	/// Whole frames at the output rate that the buffered input yields.
	pub fn available_frames(&self) -> usize {
		match self {
			PlaybackBuffer::Passthrough(buffer) => buffer.len_frames(),
			PlaybackBuffer::Resampling(resampling) => {
				let buffer = &resampling.buffer;
				buffer.len_frames() * resampling.output_rate as usize / buffer.sample_spec.sample_rate as usize
			},
		}
	}

	pub fn write(&mut self, payload: &[u8]) {
		let _ = io::Write::write_all(&mut self.buffer_mut().inner, payload);
	}

	/// Mix `num_frames` frames at the output rate into the interleaved
	/// `output`, scaled by the per-channel `vol`. Returns `false` on underrun,
	/// leaving the buffer untouched. With `tail`, fewer frames (or none) are
	/// mixed if that is all there is; the rest of `output` is left silent.
	pub fn drain_and_mix(&mut self, num_frames: usize, output: &mut [f32], vol: &[f32], tail: bool) -> bool {
		let mix = if tail {
			num_frames.min(self.available_frames())
		} else {
			num_frames
		};
		match self {
			PlaybackBuffer::Passthrough(buffer) => {
				if buffer.len_frames() < mix {
					return false;
				}
				for out_frame in output.chunks_exact_mut(F::CHANNELS).take(mix) {
					let frame = buffer.read_frame().unwrap_or(F::EQUILIBRIUM);
					for (ch, (out, sample)) in out_frame.iter_mut().zip(frame.channels()).enumerate() {
						*out += sample * vol[ch % vol.len()];
					}
				}
				true
			},
			PlaybackBuffer::Resampling(resampling) => resampling.drain_and_mix(num_frames, mix, output, vol, tail),
		}
	}

	/// Drop all queued audio, including what the resampler's filter still holds.
	pub fn clear(&mut self) {
		self.buffer_mut().inner.clear();
		if let PlaybackBuffer::Resampling(resampling) = self {
			resampling.resampler.reset();
		}
	}
}

impl<F> Resampling<F>
where
	F: dasp::Frame<Sample = f32>,
{
	/// Resample one tick of `num_frames` output frames and mix the first `mix`.
	///
	/// The resampler yields exactly `num_frames` per call, consuming a varying
	/// number of input frames (220 or 221 for a 44.1 kHz stream at 240). A
	/// tail call feeds what is left and lets rubato pad it with silence.
	fn drain_and_mix(&mut self, num_frames: usize, mix: usize, output: &mut [f32], vol: &[f32], tail: bool) -> bool {
		let channels = F::CHANNELS;
		if num_frames == 0 || mix == 0 {
			return tail || num_frames == 0;
		}
		if self.resampler.output_frames_max() != num_frames {
			// The clock's tick size, fixed for a stream unless the packet duration changes.
			match new_resampler(
				self.buffer.sample_spec.sample_rate,
				self.output_rate,
				num_frames,
				channels,
			) {
				Ok(resampler) => self.resampler = resampler,
				Err(err) => {
					tracing::error!("Failed to rebuild the audio resampler: {err}");
					return false;
				},
			}
		}

		let needed = self.resampler.input_frames_next();
		let available = self.buffer.len_frames();
		if available < needed && !tail {
			return false;
		}
		let read = needed.min(available);
		self.input.resize(needed * channels, 0.0);
		self.output.resize(num_frames * channels, 0.0);
		for frame_input in self.input.chunks_exact_mut(channels).take(read) {
			let frame = self.buffer.read_frame().unwrap_or(F::EQUILIBRIUM);
			for (sample, value) in frame_input.iter_mut().zip(frame.channels()) {
				*sample = value;
			}
		}

		let input = InterleavedSlice::new(self.input.as_slice(), channels, needed).expect("input sized above");
		let mut out =
			InterleavedSlice::new_mut(self.output.as_mut_slice(), channels, num_frames).expect("output sized above");
		let indexing = (read < needed).then(|| rubato::Indexing::new().partial_len(read));
		if let Err(err) = self.resampler.process_into_buffer(&input, &mut out, indexing.as_ref()) {
			tracing::error!("Failed to resample audio: {err}");
			return false;
		}

		for (i, (out, sample)) in output.iter_mut().zip(&self.output).take(mix * channels).enumerate() {
			*out += sample * vol[(i % channels) % vol.len()];
		}
		true
	}
}

fn new_resampler(
	input_rate: u32,
	output_rate: u32,
	output_frames: usize,
	channels: usize,
) -> Result<rubato::Async<f32>, rubato::ResamplerConstructionError> {
	rubato::Async::<f32>::new_sinc(
		f64::from(output_rate) / f64::from(input_rate),
		1.0, // The ratio never changes.
		&rubato::SincInterpolationParameters::default(),
		output_frames,
		channels,
		rubato::FixedAsync::Output,
	)
}

pub(crate) struct Buffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	inner: VecDeque<u8>,
	pub sample_spec: pulse::SampleSpec,
	pub(super) channel_map: pulse::ChannelMap,
	mixer: ChannelMixer,
	bpp: usize,
	_phantom: std::marker::PhantomData<F>,
}

impl<F> Buffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	pub fn new(sample_spec: pulse::SampleSpec, channel_map: pulse::ChannelMap) -> Self {
		let mixer = ChannelMixer::new(
			&channel_map,
			sample_spec.channels as usize,
			output_positions(F::CHANNELS),
		);
		Self {
			inner: VecDeque::new(),
			channel_map,
			sample_spec,
			mixer,
			bpp: sample_spec.format.bytes_per_sample(),
			_phantom: std::marker::PhantomData,
		}
	}

	fn len_bytes(&self) -> usize {
		self.inner.len()
	}

	fn len_frames(&self) -> usize {
		let input_channels = self.sample_spec.channels as usize;
		self.inner.len() / (input_channels * self.bpp)
	}

	fn read_frame(&mut self) -> Option<F> {
		if self.len_frames() == 0 {
			return None;
		}

		let input_channels = self.sample_spec.channels as usize;

		// Read all input samples for this frame.
		let mut samples = [0.0f32; 32];
		for s in samples.iter_mut().take(input_channels) {
			*s = self.read_sample().unwrap();
		}

		Some(self.mixer.mix(&samples[..input_channels]))
	}

	fn read_sample(&mut self) -> Option<F::Sample> {
		use dasp::Sample;

		match self.sample_spec.format {
			pulse::SampleFormat::Float32Le => self.inner.read_f32::<LE>().ok(),
			pulse::SampleFormat::Float32Be => self.inner.read_f32::<BE>().ok(),
			pulse::SampleFormat::S16Le => self.inner.read_i16::<LE>().ok().map(Sample::from_sample),
			pulse::SampleFormat::S16Be => self.inner.read_i16::<BE>().ok().map(Sample::from_sample),
			pulse::SampleFormat::U8 => self.inner.read_u8().ok().map(Sample::from_sample),
			pulse::SampleFormat::S32Le => self.inner.read_i32::<LE>().ok().map(Sample::from_sample),
			pulse::SampleFormat::S32Be => self.inner.read_i32::<BE>().ok().map(Sample::from_sample),
			pulse::SampleFormat::S24Le => self.inner.read_i24::<LE>().ok().map(Sample::from_sample),
			pulse::SampleFormat::S24Be => self.inner.read_i24::<BE>().ok().map(Sample::from_sample),
			_ => unreachable!("unsupported sample format {:?}", self.sample_spec.format),
		}
	}
}

/// Speaker positions of the sink's output channels, in the interleaved order
/// sent to the encoder (the Opus/GameStream channel order).
pub(crate) fn output_positions(channels: usize) -> &'static [ChannelPosition] {
	use ChannelPosition::*;
	match channels {
		6 => &[FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight],
		8 => &[
			FrontLeft,
			FrontRight,
			FrontCenter,
			Lfe,
			RearLeft,
			RearRight,
			SideLeft,
			SideRight,
		],
		_ => &[FrontLeft, FrontRight],
	}
}

const GAIN_CENTER: f32 = std::f32::consts::FRAC_1_SQRT_2; // 1/√2 ≈ 0.707

/// Routes a stream's channels to the sink's speakers by the positions in the
/// stream's channel map, like PulseAudio's remapping: a channel that the sink
/// has goes to that speaker, whatever its index, and other channels fold into
/// the nearest speakers.
///
/// Two layouts keep index order: a mono stream, which plays on both front
/// speakers, and a map that names no speaker position (all `Aux*`, or not
/// matching the channel count), where positions carry no information.
struct ChannelMixer {
	/// Row-major `[output][input]` gains; `None` for an identity route.
	gains: Option<Vec<f32>>,
	inputs: usize,
}

impl ChannelMixer {
	fn new(channel_map: &pulse::ChannelMap, inputs: usize, outputs: &[ChannelPosition]) -> Self {
		let positions: Vec<ChannelPosition> = channel_map.into_iter().collect();
		let positional = positions.len() == inputs && positions.iter().any(|position| !is_aux(*position));
		let mut gains = vec![0.0f32; outputs.len() * inputs];
		let mut set = |output: usize, input: usize, gain: f32| gains[output * inputs + input] += gain;
		if inputs == 1 {
			set(0, 0, 1.0);
			set(1, 0, 1.0);
		} else if !positional {
			for channel in 0..inputs.min(outputs.len()) {
				set(channel, channel, 1.0);
			}
		} else if outputs.len() == 2 {
			for (input, position) in positions.iter().enumerate() {
				let (left, right) = stereo_gains(*position);
				set(0, input, left);
				set(1, input, right);
			}
		} else {
			let output = |position| outputs.iter().position(|p| *p == position);
			// A folded channel shares its speaker with the stream's own channel
			// for it, if any, at reduced gain.
			let fold = |position: ChannelPosition| {
				if positions.contains(&position) {
					GAIN_CENTER
				} else {
					1.0
				}
			};
			for (input, position) in positions.iter().enumerate() {
				if let Some(o) = output(*position) {
					set(o, input, 1.0);
					continue;
				}
				use ChannelPosition::*;
				let targets: &[(ChannelPosition, f32)] = match position {
					Mono | TopCenter | TopFrontCenter => &[(FrontCenter, 1.0)],
					SideLeft => &[(RearLeft, fold(RearLeft))],
					SideRight => &[(RearRight, fold(RearRight))],
					RearCenter | TopRearCenter => &[(RearLeft, GAIN_CENTER), (RearRight, GAIN_CENTER)],
					FrontLeftOfCenter => &[(FrontLeft, GAIN_CENTER), (FrontCenter, GAIN_CENTER)],
					FrontRightOfCenter => &[(FrontRight, GAIN_CENTER), (FrontCenter, GAIN_CENTER)],
					TopFrontLeft => &[(FrontLeft, GAIN_CENTER)],
					TopFrontRight => &[(FrontRight, GAIN_CENTER)],
					TopRearLeft => &[(RearLeft, GAIN_CENTER)],
					TopRearRight => &[(RearRight, GAIN_CENTER)],
					_ => &[(FrontLeft, 0.5), (FrontRight, 0.5)],
				};
				for (target, gain) in targets {
					if let Some(o) = output(*target) {
						set(o, input, *gain);
					}
				}
			}
		}
		let identity = inputs == outputs.len()
			&& (0..outputs.len()).all(|o| (0..inputs).all(|i| gains[o * inputs + i] == if o == i { 1.0 } else { 0.0 }));
		Self {
			gains: (!identity).then_some(gains),
			inputs,
		}
	}

	fn mix<F: dasp::Frame<Sample = f32>>(&self, samples: &[f32]) -> F {
		match &self.gains {
			None => F::from_fn(|channel| samples[channel]),
			Some(gains) => F::from_fn(|channel| {
				gains[channel * self.inputs..(channel + 1) * self.inputs]
					.iter()
					.zip(samples)
					.map(|(gain, sample)| gain * sample)
					.sum()
			}),
		}
	}
}

fn is_aux(position: ChannelPosition) -> bool {
	(ChannelPosition::Aux0 as u32..=ChannelPosition::Aux31 as u32).contains(&(position as u32))
}

/// Left/right gains of a channel in a stereo downmix, after ITU-R BS.775:
/// center, LFE and the opposite-free surround channels at 1/√2.
fn stereo_gains(position: ChannelPosition) -> (f32, f32) {
	use ChannelPosition::*;
	match position {
		Mono | FrontCenter | Lfe => (GAIN_CENTER, GAIN_CENTER),
		FrontLeft => (1.0, 0.0),
		FrontRight => (0.0, 1.0),
		RearLeft | SideLeft => (GAIN_CENTER, 0.0),
		RearRight | SideRight => (0.0, GAIN_CENTER),
		RearCenter => (0.5, 0.5),
		FrontLeftOfCenter => (1.0, GAIN_CENTER),
		FrontRightOfCenter => (GAIN_CENTER, 1.0),
		// Unknown position: mix equally into both channels at reduced gain.
		_ => (0.5, 0.5),
	}
}

#[cfg(test)]
mod epoch_tests {
	use super::*;

	fn spec(format: pulse::SampleFormat, sample_rate: u32) -> pulse::SampleSpec {
		pulse::SampleSpec {
			format,
			channels: 2,
			sample_rate,
		}
	}

	fn stereo(sample_rate: u32) -> PlaybackBuffer<[f32; 2]> {
		PlaybackBuffer::new(
			spec(pulse::SampleFormat::Float32Le, sample_rate),
			pulse::ChannelMap::stereo(),
			spec(pulse::SampleFormat::Float32Le, 48_000),
		)
		.unwrap()
	}

	/// Single-tone SNR of the left channel in dB; higher is cleaner.
	fn snr_db(interleaved: &[f32], freq: f64, rate: f64) -> f64 {
		let left: Vec<f64> = interleaved.iter().step_by(2).map(|&x| f64::from(x)).collect();
		let n = left.len() as f64;
		let (mut re, mut im) = (0.0f64, 0.0f64);
		for (i, &s) in left.iter().enumerate() {
			let phase = 2.0 * std::f64::consts::PI * freq * (i as f64) / rate;
			re += s * phase.cos();
			im += s * phase.sin();
		}
		let fundamental = 2.0 * (re * re + im * im) / (n * n);
		let total = left.iter().map(|&s| s * s).sum::<f64>() / n;
		10.0 * (fundamental / (total - fundamental).max(1e-12)).log10()
	}

	#[test]
	fn clearing_pcm_also_clears_resampler_history() {
		for rate in [48_000, 44_100] {
			let mut buffer = stereo(rate);
			buffer.write(&0.8f32.to_le_bytes().repeat(2000));
			let mut out = vec![0.0f32; 240 * 2];
			assert!(buffer.drain_and_mix(240, &mut out, &[1.0, 1.0], false));
			buffer.clear();
			assert!(buffer.is_empty());
			buffer.write(&0.0f32.to_le_bytes().repeat(2000));
			out.fill(0.0);
			assert!(buffer.drain_and_mix(240, &mut out, &[1.0, 1.0], false));
			assert!(
				out.iter().all(|s| s.abs() < 1e-6),
				"{rate} Hz: audio from before the flush leaked through: {:?}",
				&out[..8]
			);
		}
	}

	/// Resampling 44.1 kHz to 48 kHz stays high fidelity. dasp's sinc
	/// converter reached about 21 dB SNR here, which sounded robotic.
	#[test]
	fn resampling_is_high_fidelity() {
		let rate = 44_100;
		let mut buffer = PlaybackBuffer::<[f32; 2]>::new(
			spec(pulse::SampleFormat::S16Le, rate),
			pulse::ChannelMap::stereo(),
			spec(pulse::SampleFormat::Float32Le, 48_000),
		)
		.unwrap();
		let mut bytes = Vec::new();
		for i in 0..rate * 2 {
			let x = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / rate as f32).sin() * 0.6;
			for _ in 0..2 {
				bytes.extend_from_slice(&((x * 32767.0) as i16).to_le_bytes());
			}
		}
		buffer.write(&bytes);
		let mut tick = vec![0.0f32; 240 * 2];
		let mut out = Vec::new();
		loop {
			tick.fill(0.0);
			if !buffer.drain_and_mix(240, &mut tick, &[1.0, 1.0], false) {
				break;
			}
			out.extend_from_slice(&tick);
		}
		assert!(out.len() > 20_000 * 2, "not enough output produced");
		// Skip the resampler's startup delay before measuring.
		let snr = snr_db(&out[9600 * 2..], 1000.0, 48_000.0);
		assert!(snr > 60.0, "resampled SNR too low: {snr:.1} dB");
	}

	/// Over a long run, a tick consumes the input its duration covers.
	#[test]
	fn resampling_consumes_input_at_the_stream_rate() {
		let mut buffer = stereo(44_100);
		buffer.write(&0.25f32.to_le_bytes().repeat(44_100 * 2));
		let before = buffer.len_bytes();
		let mut tick = vec![0.0f32; 240 * 2];
		for _ in 0..100 {
			assert!(buffer.drain_and_mix(240, &mut tick, &[1.0, 1.0], false));
		}
		// 100 ticks of 5 ms are 0.5 s, 22050 frames of 8 bytes, give or take
		// the frames the resampler holds back for its filter.
		let consumed = (before - buffer.len_bytes()) / 8;
		assert!((22_050..22_050 + 512).contains(&consumed), "consumed {consumed} frames");
	}

	/// A tail drain plays what is left of a short final write, then nothing
	/// output-rate frame remains to be played.
	#[test]
	fn a_tail_drain_plays_the_rest_of_a_resampled_stream() {
		let mut buffer = stereo(44_100);
		buffer.write(&0.5f32.to_le_bytes().repeat(100 * 2));
		let mut tick = vec![0.0f32; 240 * 2];
		assert!(
			!buffer.drain_and_mix(240, &mut tick, &[1.0, 1.0], false),
			"short of a full tick"
		);
		assert!(buffer.drain_and_mix(240, &mut tick, &[1.0, 1.0], true));
		assert_eq!(buffer.available_frames(), 0);
		assert!(
			tick[108 * 2..].iter().all(|s| *s == 0.0),
			"only the tail's frames are mixed"
		);
		assert!(buffer.drain_and_mix(240, &mut tick, &[1.0, 1.0], true));
	}

	/// Every rate the Pulse server accepts (1 Hz to `PA_RATE_MAX`) can be
	/// resampled, so validated streams are never refused here.
	#[test]
	fn accepted_stream_rates_build_a_resampler() {
		for rate in [1, 8_000, 44_100, 96_000, 48_000 * 16] {
			assert!(
				PlaybackBuffer::<[f32; 2]>::new(
					spec(pulse::SampleFormat::Float32Le, rate),
					pulse::ChannelMap::stereo(),
					spec(pulse::SampleFormat::Float32Le, 48_000),
				)
				.is_ok(),
				"{rate} Hz"
			);
		}
	}
}

#[cfg(test)]
mod channel_map_tests {
	use super::*;
	use ChannelPosition::*;

	/// Play one frame whose channel `i` carries `i + 1`, through a stream with
	/// `positions` into an `F` sink.
	fn route<F: dasp::Frame<Sample = f32>>(positions: &[ChannelPosition]) -> F {
		let spec = pulse::SampleSpec {
			format: pulse::SampleFormat::Float32Le,
			channels: positions.len() as u8,
			sample_rate: 48_000,
		};
		let output = pulse::SampleSpec {
			channels: F::CHANNELS as u8,
			..spec
		};
		let mut buffer =
			PlaybackBuffer::<F>::new(spec, pulse::ChannelMap::new(positions.iter().copied()), output).unwrap();
		let bytes: Vec<u8> = (1..=positions.len()).flat_map(|v| (v as f32).to_le_bytes()).collect();
		buffer.write(&bytes);
		let mut out = vec![0.0f32; F::CHANNELS];
		assert!(buffer.drain_and_mix(1, &mut out, &vec![1.0; F::CHANNELS], false));
		F::from_samples(&mut out.into_iter()).unwrap()
	}

	const G: f32 = GAIN_CENTER;

	#[test]
	fn surround_channels_reach_their_speakers_in_any_order() {
		// ALSA's 5.1 order puts the rears before the center and LFE.
		let out: [f32; 6] = route(&[FrontLeft, FrontRight, RearLeft, RearRight, FrontCenter, Lfe]);
		assert_eq!(out, [1.0, 2.0, 5.0, 6.0, 3.0, 4.0]);
		let out: [f32; 8] = route(&[
			FrontLeft,
			FrontRight,
			SideLeft,
			SideRight,
			FrontCenter,
			Lfe,
			RearLeft,
			RearRight,
		]);
		assert_eq!(out, [1.0, 2.0, 5.0, 6.0, 7.0, 8.0, 3.0, 4.0]);
		let out: [f32; 2] = route(&[FrontRight, FrontLeft]);
		assert_eq!(out, [2.0, 1.0]);
		// The sink's own layout is routed unchanged.
		let out: [f32; 6] = route(output_positions(6));
		assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
	}

	#[test]
	fn fewer_channels_fill_matching_speakers() {
		// Quadraphonic rears are rears, not center and LFE.
		let out: [f32; 6] = route(&[FrontLeft, FrontRight, RearLeft, RearRight]);
		assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 3.0, 4.0]);
		let out: [f32; 8] = route(&[FrontLeft, FrontRight]);
		assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
		// Mono plays on both front speakers.
		let out: [f32; 6] = route(&[Mono]);
		assert_eq!(out, [1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
		let out: [f32; 2] = route(&[FrontCenter]);
		assert_eq!(out, [1.0, 1.0]);
	}

	#[test]
	fn missing_speakers_fold_into_their_neighbours() {
		// 7.1 into 5.1: sides join the rears they share with.
		let out: [f32; 6] = route(output_positions(8));
		assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0 + 7.0 * G, 6.0 + 8.0 * G]);
		// Sides without rears take the rear speakers.
		let out: [f32; 6] = route(&[FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight]);
		assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
		// 5.1 into stereo keeps the ITU-R BS.775 downmix.
		let out: [f32; 2] = route(output_positions(6));
		assert_eq!(
			out,
			[1.0 + 3.0 * G + 4.0 * G + 5.0 * G, 2.0 + 3.0 * G + 4.0 * G + 6.0 * G]
		);
	}

	#[test]
	fn maps_without_speaker_positions_keep_index_order() {
		let aux = [Aux0, Aux1, Aux2, Aux3, Aux4, Aux5];
		let out: [f32; 6] = route(&aux);
		assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
		let out: [f32; 2] = route(&aux[..4]);
		assert_eq!(out, [1.0, 2.0]);
	}
}
