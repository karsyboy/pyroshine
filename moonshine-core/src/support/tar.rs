//! A minimal gzip-compressed ustar writer for support bundles: regular files
//! under one directory, nothing else.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;

const BLOCK: usize = 512;

pub(crate) struct Archive {
	encoder: GzEncoder<Vec<u8>>,
	directory: String,
	mtime: u64,
}

impl Archive {
	/// An archive whose files live under `directory/`, dated `mtime` (Unix seconds).
	pub(crate) fn new(directory: &str, mtime: u64) -> Self {
		let mut archive = Self {
			encoder: GzEncoder::new(Vec::new(), Compression::default()),
			directory: directory.to_string(),
			mtime,
		};
		archive.entry(&format!("{directory}/"), 0, b'5', 0o755);
		archive
	}

	pub(crate) fn file(&mut self, name: &str, contents: &[u8]) {
		let path = format!("{}/{name}", self.directory);
		self.entry(&path, contents.len(), b'0', 0o644);
		self.write(contents);
		self.write(&vec![0; contents.len().next_multiple_of(BLOCK) - contents.len()]);
	}

	pub(crate) fn finish(mut self) -> Vec<u8> {
		self.write(&[0; 2 * BLOCK]);
		self.encoder.finish().expect("writing to memory cannot fail")
	}

	fn write(&mut self, bytes: &[u8]) {
		self.encoder.write_all(bytes).expect("writing to memory cannot fail");
	}

	fn entry(&mut self, path: &str, size: usize, kind: u8, mode: u32) {
		assert!(path.len() < 100, "support bundle paths are short: {path}");
		let mut header = [0u8; BLOCK];
		let mut field = |offset: usize, value: &[u8]| header[offset..offset + value.len()].copy_from_slice(value);
		field(0, path.as_bytes());
		field(100, format!("{mode:07o}\0").as_bytes());
		field(108, b"0000000\0");
		field(116, b"0000000\0");
		field(124, format!("{size:011o}\0").as_bytes());
		field(136, format!("{:011o}\0", self.mtime).as_bytes());
		field(148, b"        ");
		field(156, &[kind]);
		field(257, b"ustar\0");
		field(263, b"00");
		let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
		header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
		self.write(&header);
	}
}

#[cfg(test)]
pub(crate) mod tests {
	use std::io::Read;

	use super::*;

	/// Entries as a reader sees them: name, type and contents.
	pub(crate) fn entries(archive: &[u8]) -> Vec<(String, u8, Vec<u8>)> {
		let mut tar = Vec::new();
		flate2::read::GzDecoder::new(archive).read_to_end(&mut tar).unwrap();
		let mut entries = Vec::new();
		let mut offset = 0;
		while offset + BLOCK <= tar.len() && tar[offset] != 0 {
			let header = &tar[offset..offset + BLOCK];
			let mut sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
			sum = sum - header[148..156].iter().map(|byte| u32::from(*byte)).sum::<u32>() + 8 * u32::from(b' ');
			let stored = u32::from_str_radix(std::str::from_utf8(&header[148..154]).unwrap(), 8).unwrap();
			assert_eq!(sum, stored, "checksum");
			let name = String::from_utf8(header[..100].split(|byte| *byte == 0).next().unwrap().to_vec()).unwrap();
			let size = usize::from_str_radix(std::str::from_utf8(&header[124..135]).unwrap(), 8).unwrap();
			let start = offset + BLOCK;
			entries.push((name, header[156], tar[start..start + size].to_vec()));
			offset = start + size.next_multiple_of(BLOCK);
		}
		assert_eq!(&tar[offset..], &[0; 2 * BLOCK], "two zero blocks end the archive");
		entries
	}

	#[test]
	fn writes_a_readable_archive() {
		let mut archive = Archive::new("bundle", 1_760_000_000);
		archive.file("a.txt", b"hello");
		archive.file("empty.json", b"");
		archive.file("block.bin", &[7; BLOCK]);
		let entries = entries(&archive.finish());
		let summary: Vec<_> = entries
			.iter()
			.map(|(name, kind, data)| (name.as_str(), *kind, data.len()))
			.collect();
		assert_eq!(
			summary,
			[
				("bundle/", b'5', 0),
				("bundle/a.txt", b'0', 5),
				("bundle/empty.json", b'0', 0),
				("bundle/block.bin", b'0', BLOCK)
			]
		);
		assert_eq!(entries[1].2, b"hello");
	}

	/// The system's tar reads it too.
	#[test]
	fn gnu_tar_lists_the_archive() {
		let Ok(tar) = which::which("tar") else {
			return;
		};
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("bundle.tar.gz");
		let mut archive = Archive::new("bundle", 0);
		archive.file("report.json", b"{}");
		std::fs::write(&path, archive.finish()).unwrap();
		let output = std::process::Command::new(tar).arg("-tzf").arg(&path).output().unwrap();
		assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
		assert_eq!(String::from_utf8_lossy(&output.stdout), "bundle/\nbundle/report.json\n");
	}
}
