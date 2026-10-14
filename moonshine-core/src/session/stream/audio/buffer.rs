use std::{collections::VecDeque, io};

use byteorder::{BigEndian as BE, LittleEndian as LE, ReadBytesExt as _};
use dasp::{interpolate::sinc::Sinc, ring_buffer, signal::interpolate::Converter};
use pulseaudio::protocol::{self as pulse, ChannelPosition};

/// Raw bytes go in, (optionally) resampled frames come out.
pub(crate) enum PlaybackBuffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	Passthrough(Buffer<F>),
	Resampling {
		converter: Converter<Buffer<F>, Sinc<[F; 32]>>,
		output_rate: u32,
	},
}

impl<F> PlaybackBuffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	pub fn new(sample_spec: pulse::SampleSpec, channel_map: pulse::ChannelMap, output_spec: pulse::SampleSpec) -> Self {
		assert_eq!(output_spec.channels as usize, F::CHANNELS);

		let buffer = Buffer::new(sample_spec, channel_map);
		if sample_spec.sample_rate == output_spec.sample_rate {
			Self::Passthrough(buffer)
		} else {
			let ringbuf = ring_buffer::Fixed::from([F::EQUILIBRIUM; 32]);
			let interpolator = Sinc::new(ringbuf);
			Self::Resampling {
				converter: dasp::Signal::from_hz_to_hz(
					buffer,
					interpolator,
					sample_spec.sample_rate as f64,
					output_spec.sample_rate as f64,
				),
				output_rate: output_spec.sample_rate,
			}
		}
	}

	pub fn buffer(&self) -> &Buffer<F> {
		match self {
			PlaybackBuffer::Passthrough(buffer) => buffer,
			PlaybackBuffer::Resampling { converter, .. } => converter.source(),
		}
	}

	fn buffer_mut(&mut self) -> &mut Buffer<F> {
		match self {
			PlaybackBuffer::Passthrough(buffer) => buffer,
			PlaybackBuffer::Resampling { converter, .. } => converter.source_mut(),
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
			PlaybackBuffer::Resampling { converter, output_rate } => {
				let buffer = converter.source();
				buffer.len_frames() * *output_rate as usize / buffer.sample_spec.sample_rate as usize
			},
		}
	}

	pub fn write(&mut self, payload: &[u8]) {
		let _ = io::Write::write_all(&mut self.buffer_mut().inner, payload);
	}

	/// Reads data from the buffer at the output sample rate, returning
	/// `num_frames` at that rate, or None if there's insufficient data.
	///
	/// Dropping the returned signal removes the remaining unread data.
	pub fn drain(&mut self, num_frames: usize) -> Option<impl dasp::Signal<Frame = F> + '_> {
		match self {
			PlaybackBuffer::Passthrough(buffer) => buffer.drain(num_frames).map(EitherSignal::Left),
			PlaybackBuffer::Resampling { converter, output_rate } => {
				let buffer = converter.source();
				let needed_frames =
					(buffer.sample_spec.sample_rate as usize * num_frames).div_ceil(*output_rate as usize);

				if buffer.len_frames() < needed_frames {
					return None;
				}

				Some(EitherSignal::Right(Drain {
					signal: converter,
					remaining: num_frames,
				}))
			},
		}
	}

	pub fn clear(&mut self) {
		let buffer = self.buffer();
		let sample_spec = buffer.sample_spec;
		let channel_map = buffer.channel_map;
		let sample_rate = match self {
			Self::Passthrough(_) => sample_spec.sample_rate,
			Self::Resampling { output_rate, .. } => *output_rate,
		};
		*self = Self::new(
			sample_spec,
			channel_map,
			pulse::SampleSpec {
				channels: F::CHANNELS as u8,
				sample_rate,
				format: pulse::SampleFormat::Float32Le,
			},
		);
	}
}

enum EitherSignal<L, R> {
	Left(L),
	Right(R),
}

impl<L, R> dasp::Signal for EitherSignal<L, R>
where
	L: dasp::Signal,
	R: dasp::Signal<Frame = L::Frame>,
{
	type Frame = L::Frame;

	fn next(&mut self) -> Self::Frame {
		match self {
			EitherSignal::Left(s) => s.next(),
			EitherSignal::Right(s) => s.next(),
		}
	}

	fn is_exhausted(&self) -> bool {
		match self {
			EitherSignal::Left(s) => s.is_exhausted(),
			EitherSignal::Right(s) => s.is_exhausted(),
		}
	}
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

impl<F> Buffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	fn drain(&mut self, num_frames: usize) -> Option<Drain<'_, Self>> {
		if self.len_frames() < num_frames {
			return None;
		}

		Some(Drain {
			signal: self,
			remaining: num_frames,
		})
	}
}

impl<F> dasp::Signal for Buffer<F>
where
	F: dasp::Frame<Sample = f32>,
{
	type Frame = F;

	fn next(&mut self) -> Self::Frame {
		self.read_frame().unwrap_or(<Self::Frame as dasp::Frame>::EQUILIBRIUM)
	}
}

struct Drain<'a, S: dasp::Signal> {
	signal: &'a mut S,
	remaining: usize,
}

impl<S: dasp::Signal> dasp::Signal for Drain<'_, S> {
	type Frame = S::Frame;

	fn is_exhausted(&self) -> bool {
		self.remaining == 0
	}

	fn next(&mut self) -> Self::Frame {
		if self.remaining == 0 {
			<Self::Frame as dasp::Frame>::EQUILIBRIUM
		} else {
			self.remaining -= 1;
			dasp::Signal::next(&mut self.signal)
		}
	}
}

impl<S: dasp::Signal> Drop for Drain<'_, S> {
	fn drop(&mut self) {
		for _ in 0..self.remaining {
			if self.signal.is_exhausted() {
				break;
			}

			let _ = dasp::Signal::next(&mut self.signal);
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
	#[test]
	fn clearing_pcm_also_clears_resampler_history() {
		for rate in [48_000, 44_100] {
			let spec = pulse::SampleSpec {
				format: pulse::SampleFormat::Float32Le,
				channels: 2,
				sample_rate: rate,
			};
			let out = pulse::SampleSpec {
				sample_rate: 48_000,
				..spec
			};
			let mut buffer = PlaybackBuffer::<[f32; 2]>::new(spec, pulse::ChannelMap::stereo(), out);
			buffer.write(&0.8f32.to_le_bytes().repeat(1000));
			let _ = dasp::Signal::next(&mut buffer.drain(100).unwrap());
			buffer.clear();
			assert!(buffer.is_empty());
			buffer.write(&0.0f32.to_le_bytes().repeat(1000));
			let mut samples = buffer.drain(100).unwrap();
			for _ in 0..100 {
				assert_eq!(dasp::Signal::next(&mut samples), [0.0, 0.0]);
			}
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
		let mut buffer = PlaybackBuffer::<F>::new(spec, pulse::ChannelMap::new(positions.iter().copied()), output);
		let bytes: Vec<u8> = (1..=positions.len()).flat_map(|v| (v as f32).to_le_bytes()).collect();
		buffer.write(&bytes);
		dasp::Signal::next(&mut buffer.drain(1).unwrap())
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
