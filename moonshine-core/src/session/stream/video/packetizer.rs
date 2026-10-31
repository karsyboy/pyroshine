use super::pyrowave_protocol::{FecBlockLayout, PyroWaveDialect, record_boundaries};
use aes_gcm::{
	Aes128Gcm, Key,
	aead::{AeadInOut, KeyInit, Nonce},
};
use fec_rs::ReedSolomon;
use std::collections::{HashMap, hash_map::Entry};
use std::sync::Arc;
use std::time::Instant;

use crate::session::SessionKeysReceiver;
use crate::session::keys::KeyNonces;

use crate::session::stream::video::shard_batch::{ShardBatch, ShardBuf};

/// Maximum allowed number of shards in the encoder (data + parity).
pub(crate) const MAX_SHARDS: usize = 255;
/// The GameStream FEC header allocates 10 bits to the data-shard count.
const MAX_DATA_SHARDS_WITHOUT_FEC: usize = 1023;

/// Most FEC threads the automatic setting uses. At PyroWave 4:4:4 4K120
/// (1.5 Gb/s) on a 16-thread CPU four kept packetization at 120 µs per frame,
/// as fast as eight or sixteen, at half the process CPU time; two took 160 µs
/// and one 254 µs.
const MAX_AUTO_FEC_THREADS: usize = 4;

/// Threads computing FEC parity for `configured` (`[stream.video] fec_threads`)
/// on a host with `host_threads` hardware threads: `0` selects a quarter of
/// them (at least one, at most [`MAX_AUTO_FEC_THREADS`]); an explicit count is
/// kept within the host's threads.
pub(crate) fn fec_threads_for(configured: usize, host_threads: usize) -> usize {
	let host_threads = host_threads.max(1);
	match configured {
		0 => (host_threads / 4).clamp(1, MAX_AUTO_FEC_THREADS),
		threads => threads.min(host_threads),
	}
}

/// The FEC thread pool of one video stream.
///
/// fec-rs splits a large block's parity rows over rayon (every protected 4K
/// PyroWave block). Rayon's global pool has one thread per CPU, and all of them
/// woke for every block: ~19,000 wakeups/s at 4K120 on a 16-thread CPU,
/// competing with the game's own threads, and they lived for the whole process.
/// A small named pool keeps the parallel speedup without that fan-out, and its
/// threads end with the stream. `None` (the pool could not start) computes on
/// the global pool.
pub(crate) fn fec_thread_pool(configured: usize) -> Option<Arc<rayon::ThreadPool>> {
	// Honors CPU affinity and cgroup quotas, unlike a raw core count.
	let host_threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
	let threads = fec_threads_for(configured, host_threads);
	tracing::debug!(threads, host_threads, configured, "Starting the video FEC thread pool");
	rayon::ThreadPoolBuilder::new()
		.num_threads(threads)
		.thread_name(|index| format!("video-fec-{index}"))
		.build()
		.inspect_err(|error| tracing::warn!(%error, "Could not start the FEC thread pool"))
		.ok()
		.map(Arc::new)
}

const NV_VIDEO_PACKET_SIZE: usize = 16;
const RTP_HEADER_SIZE: usize = 12;
const PADDING_SIZE: usize = 4;
/// Byte offset where the NvVideoPacket starts within a shard.
const NV_PACKET_OFFSET: usize = RTP_HEADER_SIZE + PADDING_SIZE;
/// Byte offset where the payload starts within a shard.
const PAYLOAD_OFFSET: usize = NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE;

/// Size of the per-shard encryption prefix: iv(12) + frameNumber(4) + tag(16).
pub(super) const ENC_PREFIX_SIZE: usize = 12 + 4 + 16;

#[repr(u8)]
enum RtpFlag {
	ContainsPicData = 0x1,
	EndOfFrame = 0x2,
	StartOfFrame = 0x4,
}

#[derive(Debug)]
#[repr(C)]
struct VideoFrameHeader {
	header_type: u8,
	frame_processing_latency: u16,
	frame_type: u8,
	last_payload_len: u32,
}

const VIDEO_FRAME_HEADER_SIZE: usize = 8;

/// Smallest negotiable `packetSize`: the NV packet header plus the video
/// frame header, so a frame's first shard carries its complete frame header.
pub(crate) const MIN_VIDEO_PACKET_SIZE: usize = NV_VIDEO_PACKET_SIZE + VIDEO_FRAME_HEADER_SIZE;

/// UDP payload bytes of one shard for a negotiated `packetSize`.
///
/// The client's `packetSize` covers the NV header and payload; the packetizer
/// adds the RTP header and padding, and the encryption prefix when encrypting
/// (Moonlight already subtracts that prefix from an encrypted stream's size).
pub(crate) fn wire_shard_size(packet_size: usize, encrypted: bool) -> Option<usize> {
	let prefix = if encrypted { ENC_PREFIX_SIZE } else { 0 };
	packet_size
		.checked_add(RTP_HEADER_SIZE + PADDING_SIZE)?
		.checked_add(prefix)
}

impl VideoFrameHeader {
	fn serialize(&self, buffer: &mut [u8]) {
		buffer[0] = self.header_type;
		buffer[1..3].copy_from_slice(&self.frame_processing_latency.to_le_bytes());
		buffer[3] = self.frame_type;
		buffer[4..8].copy_from_slice(&self.last_payload_len.to_le_bytes());
	}
}

/// Write an RTP header directly into a byte slice at offset 0.
fn write_rtp_header(buf: &mut [u8], sequence_number: u16, timestamp: u32) {
	buf[0] = 0x90;
	buf[1] = 0; // packet_type
	buf[2..4].copy_from_slice(&sequence_number.to_be_bytes());
	buf[4..8].copy_from_slice(&timestamp.to_be_bytes());
	buf[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc
}

/// Write an NvVideoPacket directly into a byte slice.
fn write_nv_video_packet(
	buf: &mut [u8],
	stream_packet_index: u32,
	frame_index: u32,
	flags: u8,
	multi_fec_blocks: u8,
	fec_info: u32,
) {
	buf[0..4].copy_from_slice(&stream_packet_index.to_le_bytes());
	buf[4..8].copy_from_slice(&frame_index.to_le_bytes());
	buf[8] = flags;
	buf[9] = 0; // reserved
	buf[10] = 0x10; // multi_fec_flags
	buf[11] = multi_fec_blocks;
	buf[12..16].copy_from_slice(&fec_info.to_le_bytes());
}

/// Copy bytes from the logical [header ++ encoded_data] stream into a
/// destination slice, without materializing the concatenation.
fn copy_header_and_data(
	dst: &mut [u8],
	header: &[u8; VIDEO_FRAME_HEADER_SIZE],
	encoded_data: &[u8],
	offset: usize,
	len: usize,
) {
	let total = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();
	let end = (offset + len).min(total);
	let mut written = 0;

	if offset < VIDEO_FRAME_HEADER_SIZE {
		let header_end = VIDEO_FRAME_HEADER_SIZE.min(end);
		let n = header_end - offset;
		dst[written..written + n].copy_from_slice(&header[offset..header_end]);
		written += n;
		if end > VIDEO_FRAME_HEADER_SIZE {
			let n = end - VIDEO_FRAME_HEADER_SIZE;
			dst[written..written + n].copy_from_slice(&encoded_data[..n]);
		}
	} else {
		let data_start = offset - VIDEO_FRAME_HEADER_SIZE;
		let data_end = end - VIDEO_FRAME_HEADER_SIZE;
		let n = data_end - data_start;
		dst[written..written + n].copy_from_slice(&encoded_data[data_start..data_end]);
	}
}

/// Video encryption for one packetizer epoch.
///
/// `Disabled` is only ever the negotiated plaintext mode. An epoch that
/// negotiated encryption is `Enabled` or `Unavailable`, and an `Unavailable`
/// epoch emits no shards at all; it never degrades to plaintext.
enum VideoEncryption {
	Disabled,
	Enabled {
		/// Boxed: the expanded AES key schedule dwarfs the other variants.
		cipher: Box<Aes128Gcm>,
		/// Server-owned generation of the published key (not the client key ID).
		generation: u64,
		/// Nonces belong to the key, not to this packetizer: a recreated or
		/// concurrently draining packetizer for the same key shares them.
		nonces: Arc<KeyNonces>,
	},
	/// The key's nonce space is exhausted; nothing more may be encrypted with it.
	Unavailable {
		generation: u64,
	},
}

pub(crate) struct Packetizer {
	pyrowave_dialect: Option<PyroWaveDialect>,
	/// How many FEC blocks a frame may span (see [`FecBlockLayout`]).
	fec_layout: FecBlockLayout,
	fec_encoders: HashMap<(usize, usize), ReedSolomon>,
	/// Watch channel for encryption keys — read eagerly per `packetize()` call.
	keys_rx: SessionKeysReceiver,
	encryption: VideoEncryption,
	/// Rate limit recurring layout warnings for consistently large frames.
	last_fec_warning: Option<Instant>,
	/// The stream's FEC thread pool (see [`fec_thread_pool`]).
	fec_pool: Option<Arc<rayon::ThreadPool>>,
}

impl Packetizer {
	pub fn new(encrypt: bool, keys_rx: SessionKeysReceiver) -> Self {
		let mut packetizer = Self {
			pyrowave_dialect: None,
			fec_layout: FecBlockLayout::GameStream,
			fec_encoders: HashMap::new(),
			keys_rx,
			encryption: VideoEncryption::Disabled,
			last_fec_warning: None,
			fec_pool: None,
		};
		if encrypt {
			packetizer.encryption = packetizer.current_encryption();
		}
		packetizer
	}

	/// Compute large FEC blocks on `pool` (see [`fec_thread_pool`]).
	pub(crate) fn with_fec_pool(mut self, pool: Option<Arc<rayon::ThreadPool>>) -> Self {
		self.fec_pool = pool;
		self
	}

	pub fn set_pyrowave_dialect(&mut self, dialect: Option<PyroWaveDialect>) {
		self.pyrowave_dialect = dialect;
	}

	/// Use the block layout the client negotiated. Only PyroWave clients that
	/// echoed the extended layout may receive more than four blocks.
	pub fn set_fec_layout(&mut self, layout: FecBlockLayout) {
		self.fec_layout = layout;
	}

	fn current_encryption(&self) -> VideoEncryption {
		let keys = self.keys_rx.borrow();
		let material = keys.material();
		// Key bytes were validated at the HTTP boundary; the type guarantees a
		// 16-byte AES-128 key, so cipher construction cannot fail here.
		VideoEncryption::Enabled {
			cipher: Box::new(Aes128Gcm::new(&Key::<Aes128Gcm>::from(*material.key().as_bytes()))),
			generation: material.generation(),
			nonces: material.nonces().clone(),
		}
	}

	/// Switch to a newly published key. Called eagerly by every `packetize()`.
	fn maybe_update_cipher(&mut self) {
		let current = match &self.encryption {
			VideoEncryption::Disabled => return,
			VideoEncryption::Enabled { generation, .. } | VideoEncryption::Unavailable { generation } => *generation,
		};
		if self.keys_rx.borrow().material().generation() != current {
			self.encryption = self.current_encryption();
			tracing::debug!("Video encryption key updated");
		}
	}

	/// Pre-create FEC encoders for all possible block sizes to avoid
	/// expensive ReedSolomon matrix construction during frame processing.
	///
	/// Building every encoder takes up to a few hundred milliseconds in an
	/// optimized build and many seconds in a debug one, and runs on the
	/// pipeline thread before its loop. `stopped` is checked between
	/// encoders so a session stopped meanwhile (for example before the client
	/// ever connected) is not held up by it; encoders not built here are
	/// built on first use. Returns whether the cache was completed.
	pub fn warm_up(&mut self, fec_percentage: u8, minimum_fec_packets: u32, stopped: impl Fn() -> bool) -> bool {
		for nr_data_shards in 1..MAX_SHARDS {
			if stopped() {
				tracing::debug!(
					"FEC encoder warm-up interrupted by session stop after {} entries.",
					self.fec_encoders.len()
				);
				return false;
			}
			if let Some((_, nr_parity_shards)) =
				block_fec_parameters(nr_data_shards, fec_percentage, minimum_fec_packets as usize)
				&& nr_parity_shards > 0
			{
				let _ = self.get_fec_encoder(nr_data_shards, nr_parity_shards);
			}
		}

		tracing::debug!("FEC encoder cache warmed with {} entries.", self.fec_encoders.len());
		true
	}

	/// Packetize an encoded frame into a batch of network-ready shards.
	///
	/// Returns a `ShardBatch` containing all data + parity shards packed
	/// contiguously in a single zeroed allocation per frame.
	#[allow(clippy::too_many_arguments)]
	pub fn packetize(
		&mut self,
		encoded_data: &[u8],
		is_key_frame: bool,
		requested_packet_size: usize,
		minimum_fec_packets: u32,
		fec_percentage: u8,
		frame_number: u32,
		sequence_number: &mut u32,
		rtp_timestamp: u32,
		frame_processing_latency: u16,
	) -> Result<ShardBatch, ()> {
		// Held across the FEC encoder's mutable borrow of `self`.
		let fec_pool = self.fec_pool.clone();
		let fec_layout = self.fec_layout;
		// Eagerly read current encryption key and update cipher if rotated.
		self.maybe_update_cipher();
		if let VideoEncryption::Unavailable { .. } = self.encryption {
			return Err(());
		}

		tracing::trace!(
			"Packetizing frame {}, size={}, keyframe={}",
			frame_number,
			encoded_data.len(),
			is_key_frame
		);

		let requested_shard_payload_size =
			requested_packet_size.checked_sub(NV_VIDEO_PACKET_SIZE).ok_or_else(|| {
				tracing::warn!(
					requested_packet_size,
					"Video packet size is smaller than the transport header"
				)
			})?;
		if requested_shard_payload_size == 0 {
			tracing::warn!(requested_packet_size, "Video packet size leaves no room for a payload");
			return Err(());
		}
		let packet_data_len = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();
		let last_shard_size = packet_data_len % requested_shard_payload_size;
		let last_shard_size = if last_shard_size == 0 {
			requested_shard_payload_size
		} else {
			last_shard_size
		};

		let video_frame_header = VideoFrameHeader {
			header_type: 0x01,
			frame_processing_latency,
			frame_type: if is_key_frame { 2 } else { 1 },
			last_payload_len: last_shard_size as u32,
		};

		let mut header_bytes = [0u8; VIDEO_FRAME_HEADER_SIZE];
		video_frame_header.serialize(&mut header_bytes);
		let record_mode = self.pyrowave_dialect == Some(PyroWaveDialect::RecordFramed);
		let record_starts = if record_mode {
			let (starts, critical_packets) = record_boundaries(encoded_data, requested_shard_payload_size)?;
			header_bytes[6..8].copy_from_slice(&critical_packets.to_le_bytes());
			starts
		} else {
			Vec::new()
		};

		// The total size of a shard (RTP + padding + NvVideoPacket + payload).
		let requested_shard_size = PAYLOAD_OFFSET + requested_shard_payload_size;

		// When encryption is enabled, reserve space for the per-shard prefix.
		let prefix_size = match self.encryption {
			VideoEncryption::Disabled => 0,
			_ => ENC_PREFIX_SIZE,
		};

		let nr_data_shards = packet_data_len.div_ceil(requested_shard_payload_size);
		assert!(nr_data_shards != 0);

		let layout = packet_layout(
			nr_data_shards,
			fec_percentage,
			minimum_fec_packets as usize,
			fec_layout.max_blocks(),
		)?;
		if (layout.fec_percentage < fec_percentage || (layout.disable_fec && minimum_fec_packets > 0))
			&& self
				.last_fec_warning
				.is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
		{
			if layout.disable_fec {
				tracing::warn!(
					nr_data_shards,
					requested_fec_percentage = fec_percentage,
					minimum_fec_packets,
					"No protected video FEC layout is representable; sending this frame without parity"
				);
			} else {
				tracing::warn!(
					nr_data_shards,
					requested_fec_percentage = fec_percentage,
					effective_fec_percentage = layout.fec_percentage,
					blocks = layout.blocks,
					max_blocks = fec_layout.max_blocks(),
					"Reduced video FEC to fit the negotiated FEC block limit"
				);
			}
			self.last_fec_warning = Some(Instant::now());
		}
		let nr_blocks = layout.blocks;
		let nr_data_shards_per_block = layout.data_shards_per_block;
		let requested_fec_percentage = fec_percentage;
		let fec_percentage = layout.fec_percentage;
		let disable_fec = layout.disable_fec;
		let last_block_index = nr_blocks - 1;

		tracing::trace!(
			nr_data_shards_per_block,
			fec_percentage,
			"Selected video FEC block layout"
		);
		tracing::trace!("Sending {nr_blocks} blocks of video data.");

		// Plan all block capacities before allocating; FEC and encryption operate
		// on disjoint views of this single zeroed, sender-owned frame buffer.
		let mut frame_shards = 0;
		for block in 0..nr_blocks {
			let data = ((block + 1) * nr_data_shards_per_block).min(nr_data_shards) - block * nr_data_shards_per_block;
			let parity = if disable_fec {
				0
			} else {
				block_fec_parameters(data, fec_percentage, minimum_fec_packets as usize)
					.ok_or(())?
					.1
			};
			frame_shards += data + parity;
		}
		let allocated = Instant::now();
		let mut shard_buf = ShardBuf::new(frame_shards, requested_shard_size, prefix_size);
		let mut block_start = 0;

		let total_alloc_us = allocated.elapsed().as_micros();
		let mut total_data_write_us = 0u128;
		let mut total_fec_encoder_us = 0u128;
		let mut total_fec_compute_us = 0u128;
		let mut total_fec_headers_us = 0u128;
		let mut total_parity_shards = 0usize;

		for block_index in 0..nr_blocks {
			let start = block_index * nr_data_shards_per_block;
			let end = ((block_index + 1) * nr_data_shards_per_block).min(nr_data_shards);

			let nr_data_shards = end - start;
			assert!(nr_data_shards != 0);

			let (fec_percentage, nr_parity_shards) = if disable_fec {
				(0, 0)
			} else {
				block_fec_parameters(nr_data_shards, fec_percentage, minimum_fec_packets as usize)
					.ok_or_else(|| tracing::error!(nr_data_shards, "Selected video FEC layout became invalid"))?
			};

			let t_fec_encoder = Instant::now();
			let encoder = if nr_parity_shards > 0 {
				Some(self.get_fec_encoder(nr_data_shards, nr_parity_shards)?)
			} else {
				None
			};
			total_fec_encoder_us += t_fec_encoder.elapsed().as_micros();

			tracing::trace!(
				"Sending block {block_index} with {nr_data_shards} data shards and {nr_parity_shards} parity shards."
			);

			// Select this block in the single zeroed frame allocation.
			let total_shards = nr_data_shards + nr_parity_shards;
			total_parity_shards = total_parity_shards
				.checked_add(nr_parity_shards)
				.ok_or_else(|| tracing::error!("Video parity-shard count overflow"))?;
			shard_buf.select_block(block_start, total_shards);

			let t_data_write = Instant::now();

			// Write data shards directly into the flat buffer.
			for (block_shard_index, data_shard_index) in (start..end).enumerate() {
				let payload_start = data_shard_index * requested_shard_payload_size;
				let payload_len = requested_shard_payload_size.min(packet_data_len - payload_start);

				let shard = shard_buf.shard_mut(block_shard_index);

				// Write RTP header.
				write_rtp_header(shard, *sequence_number as u16, rtp_timestamp);

				// Padding (4 bytes of zeros) is already zeroed.

				// Write NvVideoPacket header.
				let mut flags = RtpFlag::ContainsPicData as u8;
				if block_shard_index == 0 {
					flags |= RtpFlag::StartOfFrame as u8;
				}
				if block_shard_index == nr_data_shards - 1 {
					flags |= RtpFlag::EndOfFrame as u8;
				}
				write_nv_video_packet(
					&mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE],
					*sequence_number << 8,
					frame_number,
					flags,
					fec_layout.encode(block_index, last_block_index),
					(block_shard_index << 12 | nr_data_shards << 22 | usize::from(fec_percentage) << 4) as u32,
				);

				// This exact record-start metadata is included in plaintext FEC
				// and encryption. Native and conventional shard bytes stay intact.
				if record_mode
					&& (payload_start == 0
						|| payload_start >= 8 && record_starts.binary_search(&(payload_start - 8)).is_ok())
				{
					shard[NV_PACKET_OFFSET + 9] |= 0x80;
				}
				// Copy payload from [header ++ encoded_data].
				copy_header_and_data(
					&mut shard[PAYLOAD_OFFSET..],
					&header_bytes,
					encoded_data,
					payload_start,
					payload_len,
				);

				// Remaining bytes are already zero (padding for undersized last shard).

				*sequence_number += 1;
			}

			// Parity shards are already zeroed from ShardBuf::new().

			total_data_write_us += t_data_write.elapsed().as_micros();

			if let Some(encoder) = encoder {
				// Create FEC-compatible slice views into the flat buffer.
				let mut fec_slices = shard_buf.as_fec_slices();

				let t_fec_compute = Instant::now();

				match &fec_pool {
					Some(pool) => pool.install(|| encoder.encode(&mut fec_slices)),
					None => encoder.encode(&mut fec_slices),
				}
				.map_err(|e| tracing::warn!("Failed to encode packet as FEC shards: {e}"))?;

				total_fec_compute_us += t_fec_compute.elapsed().as_micros();

				let t_fec_headers = Instant::now();

				// Write headers for parity shards. FEC overwrites the entire shard
				// content, so we patch the fields Moonlight needs afterward.
				for block_shard_index in 0..nr_parity_shards {
					let shard = shard_buf.shard_mut(nr_data_shards + block_shard_index);

					// RTP header.
					shard[0] = 0x90;
					shard[1] = 0; // packet_type
					shard[2..4].copy_from_slice(&(*sequence_number as u16).to_be_bytes());

					// NvVideoPacket fields that Moonlight needs.
					let nv = &mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE];
					nv[4..8].copy_from_slice(&frame_number.to_le_bytes()); // frame_index
					nv[11] = fec_layout.encode(block_index, last_block_index); // multi_fec_blocks
					let fec_info = ((nr_data_shards + block_shard_index) << 12
						| nr_data_shards << 22
						| usize::from(fec_percentage) << 4) as u32;
					nv[12..16].copy_from_slice(&fec_info.to_le_bytes()); // fec_info

					*sequence_number += 1;
				}

				total_fec_headers_us += t_fec_headers.elapsed().as_micros();
			}

			// Encrypt each shard if video encryption is enabled.
			if let VideoEncryption::Enabled {
				cipher,
				nonces,
				generation,
			} = &self.encryption
			{
				// One reservation per block. Exhaustion is detected before any IV
				// could repeat; the partial frame is discarded and the key retired.
				let Ok(first_counter) = nonces.video.reserve(total_shards as u64) else {
					tracing::error!("Video encryption nonce space exhausted; refusing to send further video");
					self.encryption = VideoEncryption::Unavailable {
						generation: *generation,
					};
					return Err(());
				};
				for shard_index in 0..total_shards {
					// Build the 12-byte IV: bytes 0..8 = counter (LE), byte 11 = 'V'.
					let mut iv = [0u8; 12];
					iv[..8].copy_from_slice(&(first_counter + shard_index as u64).to_le_bytes());
					iv[11] = b'V';

					let nonce = Nonce::<Aes128Gcm>::from(iv);

					// Encrypt the shard data in-place, returning a detached 16-byte tag.
					let shard_data = shard_buf.shard_mut(shard_index);
					let tag = cipher
						.encrypt_inout_detached(&nonce, b"", shard_data.into())
						.map_err(|e| tracing::warn!("Failed to encrypt video shard: {e}"))?;

					// Fill the encryption prefix: iv(12) + frameNumber(4) + tag(16).
					let prefix = shard_buf.prefix_mut(shard_index);
					prefix[..12].copy_from_slice(&iv);
					prefix[12..16].copy_from_slice(&frame_number.to_le_bytes());
					prefix[16..32].copy_from_slice(&tag);
				}
			}

			block_start += total_shards;

			tracing::trace!("Finished sending frame {frame_number}.");
		}

		tracing::trace!(
			"Packetize breakdown: alloc_us={total_alloc_us} data_write_us={total_data_write_us} fec_encoder_us={total_fec_encoder_us} fec_compute_us={total_fec_compute_us} fec_headers_us={total_fec_headers_us}",
		);

		let mut all_shards = shard_buf.into_batch();
		all_shards.set_frame_metadata(
			frame_number,
			encoded_data.len(),
			nr_data_shards,
			total_parity_shards,
			nr_blocks,
		);
		all_shards.set_fec_percentages(requested_fec_percentage, fec_percentage);
		Ok(all_shards)
	}

	fn get_fec_encoder(&mut self, nr_data_shards: usize, nr_parity_shards: usize) -> Result<&mut ReedSolomon, ()> {
		Ok(match self.fec_encoders.entry((nr_data_shards, nr_parity_shards)) {
			Entry::Occupied(e) => {
				tracing::trace!("Found a FEC encoder for this combination of shards.");
				e.into_mut()
			},
			Entry::Vacant(e) => {
				tracing::trace!("No FEC encoder for this combination of shards, creating a new one.");
				let encoder = e.insert(
					ReedSolomon::new(nr_data_shards, nr_parity_shards)
						.map_err(|e| tracing::warn!("Couldn't create error correction encoder: {e}"))?,
				);
				tracing::trace!("Finished preparing FEC encoder.");

				encoder
			},
		})
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PacketLayout {
	blocks: usize,
	data_shards_per_block: usize,
	fec_percentage: u8,
	disable_fec: bool,
}

/// Moonlight derives parity as `ceil(data * percentage / 100)`. Keeping this
/// as the single sender-side rule guarantees that the percentage written to
/// `fecInfo` describes exactly the number of emitted parity shards.
fn receiver_parity_shards(nr_data_shards: usize, fec_percentage: u8) -> usize {
	(nr_data_shards * fec_percentage as usize).div_ceil(100)
}

/// Select the smallest wire percentage at or above `requested_percentage`
/// that also satisfies the client minimum. Some parity counts are not
/// representable by Moonlight's integer percentage formula, so parity is
/// always derived from the selected wire percentage rather than vice versa.
fn block_fec_parameters(
	nr_data_shards: usize,
	requested_percentage: u8,
	minimum_parity_shards: usize,
) -> Option<(u8, usize)> {
	for percentage in requested_percentage..=u8::MAX {
		let parity = receiver_parity_shards(nr_data_shards, percentage);
		if parity >= minimum_parity_shards && nr_data_shards + parity <= MAX_SHARDS {
			return Some((percentage, parity));
		}
	}
	None
}

/// Select a transport layout compatible with the negotiated block limit
/// (GameStream's four, see [`FecBlockLayout`]) and Reed-Solomon's 255-shard
/// limit. If the requested percentage does not fit, protection is reduced one
/// representable percentage point at a time. Only when no protected layout
/// exists is FEC disabled for the frame.
fn packet_layout(
	nr_data_shards: usize,
	requested_fec_percentage: u8,
	minimum_parity_shards: usize,
	max_blocks: usize,
) -> Result<PacketLayout, ()> {
	debug_assert!(nr_data_shards > 0);
	if requested_fec_percentage == 0 && minimum_parity_shards == 0 {
		let blocks = nr_data_shards.div_ceil(MAX_DATA_SHARDS_WITHOUT_FEC);
		if blocks > max_blocks {
			return Err(());
		}
		return Ok(PacketLayout {
			blocks,
			data_shards_per_block: nr_data_shards.div_ceil(blocks),
			fec_percentage: 0,
			disable_fec: true,
		});
	}

	for fec_percentage in (0..=requested_fec_percentage).rev() {
		for blocks in 1..=max_blocks {
			let data_shards_per_block = nr_data_shards.div_ceil(blocks);
			if data_shards_per_block > MAX_SHARDS {
				continue;
			}
			let valid = (0..blocks).all(|block_index| {
				let start = block_index * data_shards_per_block;
				let data = nr_data_shards.saturating_sub(start).min(data_shards_per_block);
				data > 0 && block_fec_parameters(data, fec_percentage, minimum_parity_shards).is_some()
			});
			if valid {
				return Ok(PacketLayout {
					blocks,
					data_shards_per_block,
					fec_percentage,
					disable_fec: fec_percentage == 0 && minimum_parity_shards == 0,
				});
			}
		}
	}

	let blocks = nr_data_shards.div_ceil(MAX_DATA_SHARDS_WITHOUT_FEC);
	if blocks > max_blocks {
		tracing::error!(
			nr_data_shards,
			max_blocks,
			"Encoded frame exceeds the 10-bit shard count of the negotiated FEC blocks"
		);
		return Err(());
	}
	let data_shards_per_block = nr_data_shards.div_ceil(blocks);
	Ok(PacketLayout {
		blocks,
		data_shards_per_block,
		fec_percentage: 0,
		disable_fec: true,
	})
}

#[cfg(test)]
mod tests {
	#[test]
	fn fec_threads_scale_with_the_host_and_respect_an_explicit_count() {
		use super::fec_threads_for;
		// Automatic: a quarter of the hardware threads, between one and four.
		assert_eq!(fec_threads_for(0, 1), 1);
		assert_eq!(fec_threads_for(0, 4), 1);
		assert_eq!(fec_threads_for(0, 8), 2);
		assert_eq!(fec_threads_for(0, 12), 3);
		assert_eq!(fec_threads_for(0, 16), 4);
		assert_eq!(fec_threads_for(0, 64), 4);
		// Explicit: as configured, but never more than the host has.
		assert_eq!(fec_threads_for(6, 16), 6);
		assert_eq!(fec_threads_for(32, 16), 16);
		assert_eq!(fec_threads_for(2, 0), 1);
	}

	use super::*;
	use crate::session::keys::ActiveKeys;

	#[test]
	fn native_wire_v1_uses_the_ordinary_packetizer_byte_for_byte() {
		// Opaque bytes deliberately lack a codec header: the native packetizer
		// must not inspect them, add record metadata, or choose another framing.
		for encrypt in [false, true] {
			for fec in [0, 20] {
				let data: Vec<u8> = (0..32768).map(|i| (i * 131) as u8).collect();
				let mut ordinary = packetizer_with(encrypt);
				let mut native = packetizer_with(encrypt);
				native.set_pyrowave_dialect(Some(PyroWaveDialect::NativeWireV1));
				let a = ordinary.packetize(&data, true, 1392, 0, fec, 1, &mut 0, 42, 3).unwrap();
				let b = native.packetize(&data, true, 1392, 0, fec, 1, &mut 0, 42, 3).unwrap();
				assert_eq!(a.as_bytes(), b.as_bytes());
				let mut records = packetizer();
				records.set_pyrowave_dialect(Some(PyroWaveDialect::RecordFramed));
				assert!(records.packetize(&data, true, 1392, 0, fec, 1, &mut 0, 42, 3).is_err());
			}
		}
	}

	#[test]
	fn record_metadata_survives_encryption_and_fec_loss() {
		let mut frame = Vec::new();
		frame.extend_from_slice(&(0x80000000u32 | 127 | (127 << 14)).to_le_bytes());
		frame.extend_from_slice(&32u32.to_le_bytes());
		for block in 0..32u32 {
			frame.extend_from_slice(&(2u32 << 16).to_le_bytes());
			frame.extend_from_slice(&(block << 8).to_le_bytes());
		}
		for encrypt in [false, true] {
			let mut p = packetizer_with(encrypt);
			p.set_pyrowave_dialect(Some(PyroWaveDialect::RecordFramed));
			let batch = p.packetize(&frame, true, 80, 0, 20, 1, &mut 0, 0, 0).unwrap();
			let prefix = if encrypt { ENC_PREFIX_SIZE } else { 0 };
			let cipher = Aes128Gcm::new(&Key::<Aes128Gcm>::from([0; 16]));
			let mut plain: Vec<Vec<u8>> = batch
				.as_bytes()
				.chunks_exact(batch.shard_size())
				.map(|shard| {
					let mut data = shard[prefix..].to_vec();
					if encrypt {
						cipher
							.decrypt_inout_detached(
								shard[..12].try_into().unwrap(),
								b"",
								data.as_mut_slice().into(),
								shard[16..32].try_into().unwrap(),
							)
							.unwrap();
					}
					data
				})
				.collect();
			assert_eq!(
				u16::from_le_bytes(plain[0][PAYLOAD_OFFSET + 6..PAYLOAD_OFFSET + 8].try_into().unwrap()),
				2
			);
			for data in &plain[..batch.data_shards()] {
				assert_ne!(data[NV_PACKET_OFFSET + 9] & 0x80, 0);
			}
			// Parity headers are patched on wire; reconstruct only the payload
			// region so header patching cannot invalidate the RS equation.
			let original: Vec<Vec<u8>> = plain.iter().map(|s| s[PAYLOAD_OFFSET..].to_vec()).collect();
			let mut missing: Vec<Option<Vec<u8>>> =
				plain.drain(..).map(|s| Some(s[PAYLOAD_OFFSET..].to_vec())).collect();
			missing[0] = None;
			ReedSolomon::new(batch.data_shards(), batch.parity_shards())
				.unwrap()
				.reconstruct(&mut missing)
				.unwrap();
			assert_eq!(missing[0].as_ref().unwrap(), &original[0]);
			let rebuilt: Vec<u8> = missing[..batch.data_shards()]
				.iter()
				.flat_map(|s| s.as_ref().unwrap().iter().copied())
				.collect();
			assert_eq!(&rebuilt[8..8 + frame.len()], &frame);
		}
	}

	#[test]
	fn packets_match_pre_remediation_full_batch_fingerprints() {
		use sha2::{Digest, Sha256};
		for (size, fec, encrypted, expected) in [
			(
				16000,
				0,
				false,
				"24fca8220828eae880c558db63b96e043463b08ce92cd7db43dd3270c0a95efe",
			),
			(
				16000,
				0,
				true,
				"0f31da56ab9b73cbe302117e43787c3599ffd5834af0dfcd9c54110aff2369ce",
			),
			(
				128000,
				20,
				false,
				"d7720a61045e59aab7f95622ab6e00f9a046962d1b18d493e8966d946703a18b",
			),
			(
				128000,
				20,
				true,
				"a1b5e9efc41a2b2021aa45966487c761b7fdd6270cce6ad30b4deac741a17d71",
			),
			(
				512000,
				20,
				false,
				"cb8794f145bc0d74d42037abb7bdf35600bdbc1d9e1cd8b6de05a3e0b68f6a79",
			),
			(
				512000,
				20,
				true,
				"3f3beafce8394363690b2137490156224495c15fe177bbf6180163aded0279c5",
			),
			(
				2000000,
				0,
				false,
				"03d4f811e149c6cd8d6fb89edf00c56dba2ad7c81413d7d05e65bae52c9cb62e",
			),
			(
				2000000,
				0,
				true,
				"42de8946c2120735baf3d3c6abb92cc0fbc77109da77b724e84e53c654844d6b",
			),
		] {
			let data: Vec<u8> = (0..size).map(|i| (i * 31 + i / 97) as u8).collect();
			let (_tx, rx) = tokio::sync::watch::channel(ActiveKeys::for_test([0x11; 16], 7));
			let mut packetizer = Packetizer::new(encrypted, rx);
			let first = packetizer
				.packetize(&data, true, 1024, 0, fec, 1, &mut 0, 123, 5)
				.unwrap();
			if encrypted {
				// The recorded baseline fixture is sample 20, after twenty nonce
				// reservations. Skip only counters, never re-encrypt with a reused IV.
				packetizer
					.keys_rx
					.borrow()
					.material()
					.nonces()
					.video
					.reserve(19 * first.shard_count() as u64)
					.unwrap();
			}
			let batch = packetizer
				.packetize(&data, true, 1024, 0, fec, 1, &mut 0, 123, 5)
				.unwrap();
			assert_eq!(hex::encode(Sha256::digest(batch.as_bytes())), expected);
		}
	}

	/// CPU-only, fixed-content transport baseline. Run alone with --ignored --nocapture.
	#[test]
	#[ignore]
	fn transport_packetizer_measurements() {
		use sha2::{Digest, Sha256};
		for size in [16_000, 128_000, 512_000, 2_000_000] {
			for fec in [0, 20] {
				for encrypted in [false, true] {
					let data: Vec<u8> = (0..size).map(|i| (i * 31 + i / 97) as u8).collect();
					let (_tx, rx) = tokio::sync::watch::channel(ActiveKeys::for_test([0x11; 16], 7));
					let mut packetizer = Packetizer::new(encrypted, rx);
					for sample in 0..120 {
						measurement_allocator::reset();
						let started = Instant::now();
						let batch = packetizer
							.packetize(&data, true, 1024, 0, fec, 1, &mut 0, 123, 5)
							.unwrap();
						let ns = started.elapsed().as_nanos();
						let counts = measurement_allocator::counts();
						if sample >= 20 {
							println!(
								"packetizer,{size},{fec},{encrypted},{sample},{ns},{},{},{},{},{},{},{}",
								batch.as_bytes().len(),
								batch.fec_blocks(),
								hex::encode(Sha256::digest(batch.as_bytes())),
								counts[0],
								counts[1],
								counts[2],
								counts[3]
							);
						}
					}
				}
			}
		}
	}

	/// CPU-only cost of packetizing full-budget PyroWave 4K120 frames with
	/// GameStream's and the extended FEC layout, on the stream's FEC pool.
	/// Run alone with `--release --ignored --nocapture`.
	#[test]
	#[ignore]
	fn pyrowave_high_bitrate_packetizer_measurements() {
		let pool = fec_thread_pool(0);
		println!("layout,mbps,encrypted,data_shards,parity_shards,blocks,fec_percent,p50_us,p99_us,max_us,allocations");
		for layout in [FecBlockLayout::GameStream, FecBlockLayout::Extended] {
			for mbps in [500usize, 800, 1000, 1150, 1300, 1500] {
				for encrypted in [false, true] {
					let bytes = mbps * 1_000_000 / (120 * 8);
					let data: Vec<u8> = (0..bytes).map(|i| (i * 31 + i / 97) as u8).collect();
					let (_tx, rx) = tokio::sync::watch::channel(ActiveKeys::for_test([0x11; 16], 7));
					let mut packetizer = Packetizer::new(encrypted, rx).with_fec_pool(pool.clone());
					packetizer.set_fec_layout(layout);
					packetizer.warm_up(20, 0, || false);
					let mut times = Vec::new();
					let mut last = None;
					let mut allocations = 0;
					for sample in 0..140 {
						measurement_allocator::reset();
						let started = Instant::now();
						let batch = packetizer
							.packetize(&data, true, 1392, 0, 20, sample, &mut 0, 0, 0)
							.unwrap();
						let elapsed = started.elapsed().as_micros() as u64;
						if sample >= 20 {
							times.push(elapsed);
							allocations = measurement_allocator::counts()[0];
						}
						last = Some(batch);
					}
					times.sort_unstable();
					let batch = last.unwrap();
					println!(
						"{layout:?},{mbps},{encrypted},{},{},{},{},{},{},{},{allocations}",
						batch.data_shards(),
						batch.parity_shards(),
						batch.fec_blocks(),
						batch.fec_percentage(),
						times[times.len() / 2],
						times[times.len() * 99 / 100],
						times[times.len() - 1],
					);
				}
			}
		}
	}

	/// `(iv, plaintext)` of one decrypted shard.
	type DecryptedShard = ([u8; 12], Vec<u8>);

	/// Decrypt every shard of `batch` with `key`.
	fn decrypt_shards(batch: &ShardBatch, key: &[u8; 16]) -> Result<Vec<DecryptedShard>, ()> {
		let cipher = Aes128Gcm::new(&Key::<Aes128Gcm>::from(*key));
		batch
			.as_bytes()
			.chunks_exact(batch.shard_size())
			.map(|shard| {
				let iv: [u8; 12] = shard[..12].try_into().unwrap();
				let mut data = shard[ENC_PREFIX_SIZE..].to_vec();
				cipher
					.decrypt_inout_detached(
						&Nonce::<Aes128Gcm>::from(iv),
						b"",
						data.as_mut_slice().into(),
						<&aes_gcm::Tag>::try_from(&shard[16..32]).unwrap(),
					)
					.map_err(|_| ())?;
				Ok((iv, data))
			})
			.collect()
	}

	fn frame(byte: u8, len: usize) -> Vec<u8> {
		vec![byte; len]
	}

	#[test]
	fn encrypted_epochs_never_emit_plaintext() {
		let (_tx, rx) = tokio::sync::watch::channel(ActiveKeys::for_test([0x11; 16], 7));
		let mut encrypted = Packetizer::new(true, rx.clone());
		let mut plain = Packetizer::new(false, rx);
		let data = frame(0xab, 5000);
		let batch = encrypted.packetize(&data, true, 1024, 0, 20, 1, &mut 0, 0, 0).unwrap();
		let reference = plain.packetize(&data, true, 1024, 0, 20, 1, &mut 0, 0, 0).unwrap();
		assert_eq!(batch.shard_size(), reference.shard_size() + ENC_PREFIX_SIZE);
		assert_eq!(batch.shard_count(), reference.shard_count());
		assert!(
			!batch.as_bytes().windows(32).any(|window| window == [0xab; 32]),
			"plaintext payload visible in an encrypted epoch"
		);
		let decrypted = decrypt_shards(&batch, &[0x11; 16]).unwrap();
		for ((iv, data), original) in decrypted
			.iter()
			.zip(reference.as_bytes().chunks_exact(reference.shard_size()))
		{
			assert_eq!(data.as_slice(), original);
			assert_eq!(iv[8..], [0, 0, 0, b'V'], "client-compatible IV format");
		}
		assert!(decrypt_shards(&batch, &[0x12; 16]).is_err());
	}

	/// Every IV emitted under a key across packetizer recreation, unchanged
	/// resume, key-ID-only updates, interleaved packetizers and later reuse of
	/// the key is unique, and every shard decrypts with the epoch's key.
	#[test]
	fn key_nonces_are_unique_across_packetizer_lifetimes_and_key_updates() {
		use crate::session::keys::{KeyLedger, RemoteInputKey, RemoteInputKeyId, SessionKeyData};
		use std::collections::HashSet;

		let first = [0x33; 16];
		let second = [0x44; 16];
		let mut ledger = KeyLedger::default();
		let mut publish = |key: [u8; 16], id: u32| {
			ledger.publish(SessionKeyData::new(
				RemoteInputKey::from_bytes(key),
				RemoteInputKeyId::new(id),
			))
		};
		let (tx, rx) = tokio::sync::watch::channel(publish(first, 1));
		let mut seen = HashSet::new();
		let mut record = |batch: ShardBatch, key: [u8; 16]| {
			for (iv, _) in decrypt_shards(&batch, &key).expect("shard encrypted under the epoch key") {
				assert!(seen.insert((key, iv)), "reused (key, nonce)");
			}
		};
		let mut frame_number = 0;
		let mut pack = |packetizer: &mut Packetizer| {
			frame_number += 1;
			packetizer
				.packetize(
					&frame(frame_number as u8, 3000),
					true,
					512,
					1,
					20,
					frame_number,
					&mut 0,
					0,
					0,
				)
				.unwrap()
		};

		// A draining epoch and its replacement interleave under one key.
		let mut retired = Packetizer::new(true, rx.clone());
		let mut current = Packetizer::new(true, rx.clone());
		for _ in 0..3 {
			record(pack(&mut retired), first);
			record(pack(&mut current), first);
		}
		// Unchanged resume republishes the same key.
		tx.send_replace(publish(first, 1));
		record(pack(&mut current), first);
		// Mode/codec change recreates the packetizer.
		let mut recreated = Packetizer::new(true, rx.clone());
		record(pack(&mut recreated), first);
		record(pack(&mut retired), first);
		// Only the client key ID changes.
		tx.send_replace(publish(first, i32::MIN as u32));
		record(pack(&mut recreated), first);
		// Same key ID, different key: the cipher must switch.
		tx.send_replace(publish(second, i32::MIN as u32));
		let batch = pack(&mut recreated);
		assert!(
			decrypt_shards(&batch, &first).is_err(),
			"stale key after same-ID update"
		);
		record(batch, second);
		record(pack(&mut current), second);
		// A later epoch (or session) reusing the first key continues its nonces.
		tx.send_replace(publish(first, 1));
		let mut reused = Packetizer::new(true, rx);
		record(pack(&mut reused), first);
		record(pack(&mut current), first);
		assert!(seen.len() > 100);
	}

	#[test]
	fn nonce_exhaustion_fails_before_reuse_without_emitting_shards() {
		use crate::session::keys::{KeyNonces, VIDEO_NONCE_END};

		let nonces = KeyNonces::for_test(VIDEO_NONCE_END - 10, 0);
		let (tx, rx) = tokio::sync::watch::channel(ActiveKeys::with_nonces([0x66; 16], 1, nonces.clone()));
		let mut packetizer = Packetizer::new(true, rx.clone());
		let batch = packetizer
			.packetize(&frame(1, 16), true, 1024, 0, 0, 1, &mut 0, 0, 0)
			.unwrap();
		let (iv, _) = decrypt_shards(&batch, &[0x66; 16]).unwrap()[0];
		assert_eq!(u64::from_le_bytes(iv[..8].try_into().unwrap()), VIDEO_NONCE_END - 10);

		// Needs more counters than remain: nothing is emitted, nothing wraps.
		let mut sequence = 0;
		assert!(
			packetizer
				.packetize(&frame(2, 20 * 1008), true, 1024, 0, 0, 2, &mut sequence, 0, 0)
				.is_err()
		);
		assert_eq!(nonces.video.high_water(), VIDEO_NONCE_END - 9);
		// The key is retired for this epoch, even for frames that would fit.
		assert!(
			packetizer
				.packetize(&frame(3, 16), true, 1024, 0, 0, 3, &mut 0, 0, 0)
				.is_err()
		);
		// Another packetizer may use the remaining counters, but never past the end.
		let mut other = Packetizer::new(true, rx);
		let batch = other
			.packetize(&frame(4, 8 * 1008), true, 1024, 0, 0, 4, &mut 0, 0, 0)
			.unwrap();
		assert_eq!(batch.shard_count(), 9);
		assert!(
			other
				.packetize(&frame(5, 16), true, 1024, 0, 0, 5, &mut 0, 0, 0)
				.is_err()
		);
		assert_eq!(nonces.video.high_water(), VIDEO_NONCE_END);

		// A newly published key restores service.
		tx.send_replace(ActiveKeys::for_test([0x77; 16], 1));
		let batch = packetizer
			.packetize(&frame(6, 16), true, 1024, 0, 0, 6, &mut 0, 0, 0)
			.unwrap();
		assert!(decrypt_shards(&batch, &[0x77; 16]).is_ok());
	}

	#[test]
	fn ordinary_frames_keep_fec() {
		let layout = packet_layout(400, 20, 0, 4).unwrap();
		assert_eq!(layout.fec_percentage, 20);
		assert!(!layout.disable_fec);
		assert_eq!(layout.blocks, 2);
		assert_eq!(layout.data_shards_per_block, 200);
	}

	#[test]
	fn oversized_frames_reduce_fec_instead_of_disabling_it() {
		let layout = packet_layout(934, 20, 0, 4).unwrap();
		assert_eq!(
			layout,
			PacketLayout {
				blocks: 4,
				data_shards_per_block: 234,
				fec_percentage: 8,
				disable_fec: false,
			}
		);
	}

	#[test]
	fn frames_beyond_the_header_limit_are_rejected() {
		assert!(packet_layout(MAX_DATA_SHARDS_WITHOUT_FEC * 4 + 1, 20, 0, 4).is_err());
	}

	#[test]
	fn extended_layout_keeps_the_requested_fec_beyond_four_blocks() {
		// 1392-byte packets at 120 FPS: data shards per frame at 1.15, 1.3,
		// 1.5 and 2 Gb/s; GameStream's four blocks protect at most 848 at 20%.
		for data_shards in [871, 985, 1136, 1515] {
			let gamestream = packet_layout(data_shards, 20, 0, 4).unwrap();
			assert!(gamestream.fec_percentage < 20 || gamestream.disable_fec);
			let extended = packet_layout(data_shards, 20, 0, FecBlockLayout::Extended.max_blocks()).unwrap();
			assert_eq!(extended.fec_percentage, 20);
			assert!(!extended.disable_fec);
			assert!(extended.blocks > 4 && extended.blocks <= 16);
			assert!(
				extended.data_shards_per_block + receiver_parity_shards(extended.data_shards_per_block, 20)
					<= MAX_SHARDS
			);
		}
		// The largest protected frame: 16 full blocks.
		assert_eq!(packet_layout(16 * 212, 20, 0, 16).unwrap().fec_percentage, 20);
		assert!(packet_layout(16 * 212 + 1, 20, 0, 16).unwrap().fec_percentage < 20);
		// Below five blocks the extended layout is byte-identical to GameStream's.
		for encrypt in [false, true] {
			let data: Vec<u8> = (0..800 * 1376).map(|i| (i * 7 + i / 13) as u8).collect();
			let mut gamestream = packetizer_with(encrypt);
			let mut extended = packetizer_with(encrypt);
			extended.set_fec_layout(FecBlockLayout::Extended);
			// Each test packetizer has its own key, so nonces match too.
			let a = gamestream.packetize(&data, true, 1392, 0, 20, 1, &mut 0, 7, 1).unwrap();
			let b = extended.packetize(&data, true, 1392, 0, 20, 1, &mut 0, 7, 1).unwrap();
			assert_eq!(a.fec_blocks(), 4);
			assert_eq!(a.as_bytes(), b.as_bytes());
		}
	}

	/// Data shards of every block of `batch`, recovered from the wire with
	/// up to `lose` data shards missing per block, reassembled to the frame.
	fn recover_extended_frame(batch: &ShardBatch, lose: impl Fn(usize, usize) -> bool) -> Result<Vec<u8>, ()> {
		let shards: Vec<&[u8]> = batch.as_bytes().chunks_exact(batch.shard_size()).collect();
		let mut frame = Vec::new();
		let mut index = 0;
		let mut block = 0;
		while index < shards.len() {
			let nv = &shards[index][NV_PACKET_OFFSET..];
			let fec_info = u32::from_le_bytes(nv[12..16].try_into().unwrap());
			let data = ((fec_info >> 22) & 0x3ff) as usize;
			let parity = receiver_parity_shards(data, ((fec_info >> 4) & 0xff) as u8);
			let byte = nv[11] as usize;
			assert_eq!((byte >> 4) & 3 | (byte & 3) << 2, block, "block index");
			assert_eq!(
				(byte >> 6) & 3 | ((byte >> 2) & 3) << 2,
				batch.fec_blocks() - 1,
				"last block"
			);
			let mut received: Vec<Option<Vec<u8>>> = shards[index..index + data + parity]
				.iter()
				.enumerate()
				.map(|(i, s)| (!lose(block, i)).then(|| s[PAYLOAD_OFFSET..].to_vec()))
				.collect();
			if parity > 0 {
				ReedSolomon::new(data, parity)
					.unwrap()
					.reconstruct(&mut received)
					.map_err(|_| ())?;
			}
			for shard in &received[..data] {
				frame.extend_from_slice(shard.as_ref().ok_or(())?);
			}
			index += data + parity;
			block += 1;
		}
		assert_eq!(block, batch.fec_blocks());
		Ok(frame)
	}

	#[test]
	fn extended_blocks_recover_random_and_burst_loss_up_to_their_parity() {
		// 1.3 Gb/s at 120 FPS: 985 data shards in five blocks of 197 + 40.
		let data: Vec<u8> = (0..985 * 1376 - 8).map(|i| (i * 131 + i / 7) as u8).collect();
		let mut p = packetizer();
		p.set_fec_layout(FecBlockLayout::Extended);
		let batch = p.packetize(&data, true, 1392, 0, 20, 1, &mut 0, 0, 0).unwrap();
		assert_eq!((batch.fec_blocks(), batch.fec_percentage()), (5, 20));
		assert_eq!(batch.data_shards(), 985);
		assert_eq!(batch.parity_shards(), 5 * 40);
		let check = |frame: Vec<u8>| assert_eq!(&frame[8..8 + data.len()], &data[..]);
		check(recover_extended_frame(&batch, |_, _| false).unwrap());
		// A burst of exactly the parity count in every block.
		check(recover_extended_frame(&batch, |_, i| (50..90).contains(&i)).unwrap());
		// Scattered loss, data and parity alike, up to the parity count.
		check(recover_extended_frame(&batch, |block, i| (i * 7 + block) % 6 == 0 && i < 6 * 40).unwrap());
		// One more than the parity in a single block is unrecoverable.
		assert!(recover_extended_frame(&batch, |block, i| block == 3 && i < 41).is_err());
	}

	/// The client's reassembly test (`tests/pyrowave_extended_fec.c` in
	/// Pyrolight's moonlight-common-c) feeds this exact frame through its
	/// queue, Reed-Solomon recovery and depacketizer. Rewrite the fixture with
	/// `MOONSHINE_UPDATE_FEC_FIXTURE=1` and copy it to the client.
	#[test]
	fn extended_fec_frame_matches_the_client_fixture() {
		// 24-byte packets keep the fixture small: 849 data shards, one more
		// than four 20%-protected blocks hold, so the frame needs five.
		let data: Vec<u8> = (0..849 * 8 - 8).map(|i| (i * 131 + i / 7) as u8).collect();
		let mut p = packetizer();
		p.set_fec_layout(FecBlockLayout::Extended);
		let batch = p.packetize(&data, true, 24, 0, 20, 1, &mut 0, 9000, 3).unwrap();
		assert_eq!((batch.fec_blocks(), batch.fec_percentage()), (5, 20));
		let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/protocol/pyrowave-extended-fec.bin");
		if std::env::var_os("MOONSHINE_UPDATE_FEC_FIXTURE").is_some() {
			std::fs::write(&path, batch.as_bytes()).unwrap();
		}
		assert_eq!(std::fs::read(&path).unwrap(), batch.as_bytes());
		assert_eq!(
			&recover_extended_frame(&batch, |_, i| i % 7 == 1).unwrap()[8..8 + data.len()],
			&data[..]
		);
	}

	#[test]
	fn zero_percentage_and_zero_minimum_disable_fec() {
		let layout = packet_layout(100, 0, 0, 4).unwrap();
		assert!(layout.disable_fec);
		assert_eq!(layout.fec_percentage, 0);
	}

	fn packetizer() -> Packetizer {
		packetizer_with(false)
	}

	fn packetizer_with(encrypt: bool) -> Packetizer {
		let (_tx, rx) = tokio::sync::watch::channel(ActiveKeys::for_test([0; 16], 0));
		Packetizer::new(encrypt, rx)
	}

	#[test]
	fn emitted_metadata_reconstructs_the_actual_parity_count() {
		for percentage in [5, 10, 20, 25] {
			let mut packetizer = packetizer();
			let mut sequence = 0;
			let batch = packetizer
				.packetize(&vec![0x55; 992], true, 116, 0, percentage, 1, &mut sequence, 0, 0)
				.unwrap();
			let first = &batch.as_bytes()[..batch.shard_size()];
			let fec_info = u32::from_le_bytes(first[NV_PACKET_OFFSET + 12..NV_PACKET_OFFSET + 16].try_into().unwrap());
			let data = ((fec_info >> 22) & 0x3ff) as usize;
			let wire_percentage = ((fec_info >> 4) & 0xff) as u8;
			let parity = batch.shard_count() - data;
			assert_eq!(batch.frame_number(), 1);
			assert_eq!(batch.encoded_size(), 992);
			assert_eq!(batch.data_shards(), data);
			assert_eq!(batch.parity_shards(), parity);
			assert_eq!(batch.fec_blocks(), 1);
			assert_eq!(wire_percentage, percentage);
			assert_eq!(parity, receiver_parity_shards(data, wire_percentage));
		}
	}

	#[test]
	fn minimum_parity_metadata_is_receiver_symmetric_for_a_tiny_frame() {
		let mut packetizer = packetizer();
		let mut sequence = 0;
		let batch = packetizer
			.packetize(&[0x55; 16], true, 116, 2, 20, 1, &mut sequence, 0, 0)
			.unwrap();
		let first = &batch.as_bytes()[..batch.shard_size()];
		let fec_info = u32::from_le_bytes(first[NV_PACKET_OFFSET + 12..NV_PACKET_OFFSET + 16].try_into().unwrap());
		let data = ((fec_info >> 22) & 0x3ff) as usize;
		let wire_percentage = ((fec_info >> 4) & 0xff) as u8;
		assert_eq!(data, 1);
		assert_eq!(batch.shard_count(), 3);
		assert_eq!(receiver_parity_shards(data, wire_percentage), 2);
	}

	#[test]
	fn no_fec_packetization_is_continuous_across_gso_chunk_boundaries() {
		for data_shards in [46 * 6, 46 * 7, 46 * 7 + 1, 46 * 8 - 7, 46 * 8] {
			let encoded_size = data_shards * 1376 - VIDEO_FRAME_HEADER_SIZE;
			let mut packetizer = packetizer();
			let mut sequence = 0;
			let batch = packetizer
				.packetize(&vec![0x55; encoded_size], true, 1392, 0, 0, 1, &mut sequence, 0, 0)
				.unwrap();
			assert_eq!(batch.shard_size(), 1408);
			assert_eq!(batch.data_shards(), data_shards);
			assert_eq!(batch.parity_shards(), 0);
			assert_eq!(batch.shard_count(), data_shards);
		}
	}

	#[test]
	fn reed_solomon_recovers_up_to_parity_count_including_bursts() {
		let data_shards = 50;
		let parity_shards = receiver_parity_shards(data_shards, 20);
		let encoder = ReedSolomon::new(data_shards, parity_shards).unwrap();
		let mut shards: Vec<Vec<u8>> = (0..data_shards + parity_shards)
			.map(|index| {
				if index < data_shards {
					(0..128).map(|byte| (index ^ byte) as u8).collect()
				} else {
					vec![0; 128]
				}
			})
			.collect();
		encoder.encode(&mut shards).unwrap();
		let original = shards.clone();

		let mut recoverable: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
		for shard in &mut recoverable[7..7 + parity_shards] {
			*shard = None;
		}
		encoder.reconstruct(&mut recoverable).unwrap();
		for index in 0..data_shards {
			assert_eq!(recoverable[index].as_ref().unwrap(), &original[index]);
		}

		let mut unrecoverable: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
		for shard in &mut unrecoverable[7..7 + parity_shards + 1] {
			*shard = None;
		}
		assert!(encoder.reconstruct(&mut unrecoverable).is_err());
	}

	#[test]
	fn client_minimum_uses_a_receiver_representable_percentage() {
		for data in 1..=255 {
			for requested in [0, 1, 5, 10, 20, 25] {
				for minimum in [0, 1, 2] {
					if let Some((percentage, parity)) = block_fec_parameters(data, requested, minimum) {
						assert_eq!(receiver_parity_shards(data, percentage), parity);
						assert!(parity >= minimum);
						assert!(data + parity <= MAX_SHARDS);
					}
				}
			}
		}
	}

	#[test]
	fn four_block_boundary_degrades_smoothly() {
		let before = packet_layout(848, 20, 0, 4).unwrap();
		let after = packet_layout(849, 20, 0, 4).unwrap();
		assert_eq!(before.blocks, 4);
		assert_eq!(before.fec_percentage, 20);
		assert_eq!(after.blocks, 4);
		assert!(after.fec_percentage > 0);
		assert!(after.fec_percentage <= 20);
	}

	/// A session stopped while the pipeline warms its FEC cache (seconds in a
	/// debug build) must not wait for the whole cache; what is missing is built
	/// on first use with identical output.
	#[test]
	fn fec_warm_up_stops_with_the_session_and_encoders_fill_on_demand() {
		let checks = std::cell::Cell::new(0);
		let mut interrupted = packetizer_with(false);
		assert!(!interrupted.warm_up(20, 0, || {
			checks.set(checks.get() + 1);
			checks.get() > 3
		}));
		assert_eq!(checks.get(), 4, "checked before every encoder");
		assert!(interrupted.fec_encoders.len() <= 3);

		let mut warmed = packetizer_with(false);
		assert!(warmed.warm_up(20, 0, || false));
		assert!(warmed.fec_encoders.len() > interrupted.fec_encoders.len());

		let data: Vec<u8> = (0..200_000).map(|i| (i * 7) as u8).collect();
		let lazy = interrupted
			.packetize(&data, true, 1392, 0, 20, 1, &mut 0, 9, 1)
			.unwrap();
		let eager = warmed.packetize(&data, true, 1392, 0, 20, 1, &mut 0, 9, 1).unwrap();
		assert_eq!(lazy.as_bytes(), eager.as_bytes());
	}
}

#[cfg(test)]
mod measurement_allocator {
	use std::alloc::{GlobalAlloc, Layout, System};
	use std::cell::Cell;
	thread_local! {
		static COUNTS: Cell<[usize; 4]> = const { Cell::new([0; 4]) };
	}
	pub struct MeasuredAllocator;
	fn record(index: usize, bytes: usize) {
		COUNTS.with(|counts| {
			let mut c = counts.get();
			c[index] += bytes;
			counts.set(c);
		});
	}
	// Delegates unchanged to System; thread-local counters require no allocation.
	unsafe impl GlobalAlloc for MeasuredAllocator {
		unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
			record(0, 1);
			record(1, layout.size());
			unsafe { System.alloc(layout) }
		}
		unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
			record(0, 1);
			record(1, layout.size());
			unsafe { System.alloc_zeroed(layout) }
		}
		unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
			unsafe { System.dealloc(ptr, layout) }
		}
		unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
			record(0, 1);
			record(1, size);
			record(2, layout.size());
			unsafe { System.realloc(ptr, layout, size) }
		}
	}
	#[global_allocator]
	static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;
	pub fn reset() {
		COUNTS.with(|c| c.set([0; 4]));
	}
	pub fn counts() -> [usize; 4] {
		COUNTS.with(Cell::get)
	}
}
