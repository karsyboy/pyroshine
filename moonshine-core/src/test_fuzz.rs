//! Seeded, bounded input mutation for protocol and parser tests.
//!
//! Each fuzz test derives its inputs from valid fixtures with a fixed seed, so
//! a failure reproduces exactly. `MOONSHINE_FUZZ_ITERATIONS` scales every test
//! (for a longer local campaign) without changing the default suite's runtime;
//! `MOONSHINE_FUZZ_SEED` picks another reproducible input sequence.

/// Bytes that commonly change how a text or binary parser branches.
const INTERESTING: [u8; 14] = [
	0, 1, 0x7f, 0x80, 0xff, b'\r', b'\n', b':', b' ', b'-', b'0', b'9', b'=', b'/',
];

pub(crate) struct Mutator(u64);

impl Mutator {
	/// A generator for test `name`: the same name and seed give the same inputs.
	pub(crate) fn new(name: &str) -> Self {
		let seed = std::env::var("MOONSHINE_FUZZ_SEED")
			.ok()
			.and_then(|seed| seed.parse::<u64>().ok())
			.unwrap_or(0x9e37_79b9_7f4a_7c15);
		// FNV-1a of the name keeps tests' sequences independent.
		let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
			(hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
		});
		Self((seed ^ hash) | 1)
	}

	pub(crate) fn next(&mut self) -> u64 {
		self.0 ^= self.0 << 13;
		self.0 ^= self.0 >> 7;
		self.0 ^= self.0 << 17;
		self.0
	}

	/// A value in `0..bound` (`bound` > 0).
	pub(crate) fn below(&mut self, bound: usize) -> usize {
		(self.next() % bound as u64) as usize
	}

	pub(crate) fn bytes(&mut self, len: usize) -> Vec<u8> {
		(0..len).map(|_| self.next() as u8).collect()
	}

	/// One to four structural mutations of a corpus entry: bit flips,
	/// interesting bytes, insertions, deletions, duplication, truncation and
	/// splicing with another entry.
	pub(crate) fn mutate(&mut self, corpus: &[Vec<u8>]) -> Vec<u8> {
		let mut data = corpus[self.below(corpus.len())].clone();
		for _ in 0..=self.below(4) {
			let len = data.len();
			match self.below(8) {
				0 if len > 0 => {
					let at = self.below(len);
					data[at] ^= 1 << self.below(8);
				},
				1 if len > 0 => {
					let at = self.below(len);
					data[at] = INTERESTING[self.below(INTERESTING.len())];
				},
				2 => {
					let at = self.below(len + 1);
					let count = 1 + self.below(16);
					let inserted = self.bytes(count);
					data.splice(at..at, inserted);
				},
				3 if len > 0 => {
					let at = self.below(len);
					let end = (at + 1 + self.below(16)).min(len);
					data.drain(at..end);
				},
				4 if len > 0 => {
					let at = self.below(len);
					let end = (at + 1 + self.below(32)).min(len);
					let copy = data[at..end].to_vec();
					let to = self.below(len + 1);
					data.splice(to..to, copy);
				},
				5 => data.truncate(self.below(len + 1)),
				6 => {
					let other = &corpus[self.below(corpus.len())];
					let at = self.below(len + 1);
					let from = self.below(other.len() + 1);
					data.truncate(at);
					data.extend_from_slice(&other[from..]);
				},
				_ => {
					let count = self.below(8);
					let tail = self.bytes(count);
					data.extend(tail);
				},
			}
		}
		data
	}

	/// Split `data` into reads of random sizes (at least one, possibly empty).
	pub(crate) fn chunks<'a>(&mut self, data: &'a [u8]) -> Vec<&'a [u8]> {
		let mut chunks = Vec::new();
		let mut rest = data;
		loop {
			let take = match self.below(4) {
				0 => 1,
				1 => self.below(8),
				_ => self.below(rest.len() + 1),
			}
			.min(rest.len());
			let (chunk, tail) = rest.split_at(take);
			chunks.push(chunk);
			rest = tail;
			if rest.is_empty() {
				return chunks;
			}
		}
	}
}

/// Iterations for a fuzz test, scaled by `MOONSHINE_FUZZ_ITERATIONS` (a
/// multiplier, default 1).
pub(crate) fn iterations(default: usize) -> usize {
	let scale = std::env::var("MOONSHINE_FUZZ_ITERATIONS")
		.ok()
		.and_then(|scale| scale.parse::<usize>().ok())
		.unwrap_or(1);
	default.saturating_mul(scale.max(1))
}
