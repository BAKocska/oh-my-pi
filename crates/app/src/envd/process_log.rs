//! Durable bounded logs for envd-owned named processes.

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Read as _, Seek as _, SeekFrom, Write as _},
	path::{Path, PathBuf},
};

/// Maximum size retained in the current output file before rotation.
pub const MAX_LOG_BYTES: u64 = 25 * 1024 * 1024;
/// Maximum bytes returned by one historical log read.
pub const MAX_LOG_READ_BYTES: u64 = 2 * 1024 * 1024;

const OUTPUT_LOG: &str = "output.log";
const PREVIOUS_LOG: &str = "output.previous.log";

/// One newly observed range in a process log.
#[derive(Debug)]
pub struct LogChunk {
	/// Absolute cursor of the first byte.
	pub offset:    u64,
	/// Newly observed bytes.
	pub data:      Vec<u8>,
	/// Bytes before this chunk were skipped to preserve the read bound.
	pub truncated: bool,
}

/// Rotation-aware file log with monotonic absolute cursors.
#[derive(Debug)]
pub struct ProcessLog {
	dir:            PathBuf,
	observed_size:  u64,
	start_offset:   u64,
	end_offset:     u64,
	rotations:      u32,
	previous_bytes: u64,
}

impl ProcessLog {
	/// Opens a new generation, rotating any prior current-generation log.
	pub fn create(dir: impl Into<PathBuf>, end_offset: u64, rotations: u32) -> io::Result<Self> {
		let dir = dir.into();
		fs::create_dir_all(&dir)?;
		let current = dir.join(OUTPUT_LOG);
		let previous = dir.join(PREVIOUS_LOG);
		let mut previous_bytes = 0;
		if current.exists() {
			let _ = fs::remove_file(&previous);
			fs::rename(&current, &previous)?;
			if fs::metadata(&previous)?.len() > MAX_LOG_BYTES {
				retain_tail(&previous, MAX_LOG_BYTES)?;
			}
			previous_bytes = fs::metadata(&previous)?.len();
		}
		OpenOptions::new()
			.create(true)
			.append(true)
			.open(&current)?
			.sync_all()?;
		Ok(Self {
			dir,
			observed_size: 0,
			start_offset: end_offset.saturating_sub(previous_bytes),
			end_offset,
			rotations: rotations.saturating_add(u32::from(previous_bytes > 0)),
			previous_bytes,
		})
	}

	/// Reopens an existing generation at a persisted absolute cursor.
	pub fn reopen(
		dir: impl Into<PathBuf>,
		start_offset: u64,
		end_offset: u64,
		rotations: u32,
	) -> io::Result<Self> {
		let dir = dir.into();
		fs::create_dir_all(&dir)?;
		let actual_size = fs::metadata(dir.join(OUTPUT_LOG)).map_or(0, |meta| meta.len());
		let previous = dir.join(PREVIOUS_LOG);
		if fs::metadata(&previous).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
			retain_tail(&previous, MAX_LOG_BYTES)?;
		}
		let previous_bytes = fs::metadata(previous).map_or(0, |meta| meta.len());
		let observed_size = end_offset
			.saturating_sub(start_offset.saturating_add(previous_bytes))
			.min(actual_size);
		Ok(Self { dir, observed_size, start_offset, end_offset, rotations, previous_bytes })
	}

	/// Opens the current output file for direct child stdout/stderr inheritance.
	pub fn child_output(&self) -> io::Result<File> {
		OpenOptions::new()
			.create(true)
			.append(true)
			.open(self.dir.join(OUTPUT_LOG))
	}

	/// Appends bytes produced by an attached pipe or PTY generation and returns
	/// its absolute starting cursor.
	pub fn append(&mut self, data: &[u8]) -> io::Result<u64> {
		if self.observed_size > 0
			&& self.observed_size.saturating_add(data.len() as u64) > MAX_LOG_BYTES
		{
			self.rotate()?;
		}
		let offset = self.end_offset;
		let mut file = OpenOptions::new()
			.create(true)
			.append(true)
			.open(self.dir.join(OUTPUT_LOG))?;
		file.write_all(data)?;
		file.flush()?;
		self.observed_size = self.observed_size.saturating_add(data.len() as u64);
		self.end_offset = self.end_offset.saturating_add(data.len() as u64);
		Ok(offset)
	}

	/// Reads bytes appended directly by a detached child since the previous
	/// poll.
	pub fn poll_external(&mut self) -> io::Result<Option<LogChunk>> {
		let current = self.dir.join(OUTPUT_LOG);
		let size = fs::metadata(&current).map_or(0, |meta| meta.len());
		if size < self.observed_size {
			self.observed_size = size;
		}
		if size == self.observed_size {
			return Ok(None);
		}
		let delta = size - self.observed_size;
		let retained = delta.min(MAX_LOG_READ_BYTES);
		let skipped = delta - retained;
		let mut file = File::open(&current)?;
		file.seek(SeekFrom::Start(self.observed_size + skipped))?;
		let mut data = Vec::with_capacity(retained as usize);
		file.take(retained).read_to_end(&mut data)?;
		let offset = self.end_offset.saturating_add(skipped);
		self.observed_size = size;
		self.end_offset = self.end_offset.saturating_add(delta);
		let chunk = LogChunk { offset, data, truncated: skipped > 0 };
		if self.observed_size > MAX_LOG_BYTES {
			self.rotate_external()?;
		}
		Ok(Some(chunk))
	}

	/// Reads a bounded retained range strictly after `cursor`.
	pub fn read_after(&self, cursor: u64, max_bytes: u64) -> io::Result<(Vec<u8>, bool)> {
		let max_bytes = max_bytes.clamp(1, MAX_LOG_READ_BYTES);
		let requested = cursor.max(self.start_offset).min(self.end_offset);
		let mut remaining = max_bytes;
		let mut output = Vec::with_capacity(max_bytes as usize);
		let previous_end = self.start_offset.saturating_add(self.previous_bytes);
		if requested < previous_end && remaining > 0 {
			let local = requested.saturating_sub(self.start_offset);
			read_range(
				&self.dir.join(PREVIOUS_LOG),
				local,
				(previous_end - requested).min(remaining),
				&mut output,
			)?;
			remaining = max_bytes.saturating_sub(output.len() as u64);
		}
		if remaining > 0 {
			let current_start = previous_end;
			let current_cursor = requested.max(current_start);
			read_range(
				&self.dir.join(OUTPUT_LOG),
				current_cursor.saturating_sub(current_start),
				remaining,
				&mut output,
			)?;
		}
		Ok((output, cursor < self.start_offset))
	}

	/// Absolute first retained byte.
	pub const fn start_offset(&self) -> u64 {
		self.start_offset
	}

	/// Absolute byte after the newest observed byte.
	pub const fn end_offset(&self) -> u64 {
		self.end_offset
	}

	/// Number of completed rotations.
	pub const fn rotations(&self) -> u32 {
		self.rotations
	}

	fn rotate(&mut self) -> io::Result<()> {
		let current = self.dir.join(OUTPUT_LOG);
		let previous = self.dir.join(PREVIOUS_LOG);
		let _ = fs::remove_file(&previous);
		if current.exists() {
			fs::rename(&current, &previous)?;
			self.previous_bytes = fs::metadata(&previous)?.len().min(MAX_LOG_BYTES);
			if fs::metadata(&previous)?.len() > MAX_LOG_BYTES {
				retain_tail(&previous, MAX_LOG_BYTES)?;
			}
		}
		OpenOptions::new()
			.create(true)
			.append(true)
			.open(&current)?;
		self.observed_size = 0;
		self.start_offset = self.end_offset.saturating_sub(self.previous_bytes);
		self.rotations = self.rotations.saturating_add(1);
		Ok(())
	}

	fn rotate_external(&mut self) -> io::Result<()> {
		let current = self.dir.join(OUTPUT_LOG);
		let previous = self.dir.join(PREVIOUS_LOG);
		let _ = fs::remove_file(&previous);
		fs::copy(&current, &previous)?;
		if fs::metadata(&previous)?.len() > MAX_LOG_BYTES {
			retain_tail(&previous, MAX_LOG_BYTES)?;
		}
		self.previous_bytes = fs::metadata(&previous)?.len();
		OpenOptions::new().write(true).open(&current)?.set_len(0)?;
		self.observed_size = 0;
		self.start_offset = self.end_offset.saturating_sub(self.previous_bytes);
		self.rotations = self.rotations.saturating_add(1);
		Ok(())
	}
}

fn read_range(path: &Path, offset: u64, length: u64, output: &mut Vec<u8>) -> io::Result<()> {
	let mut file = match File::open(path) {
		Ok(file) => file,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(error),
	};
	file.seek(SeekFrom::Start(offset))?;
	file.take(length).read_to_end(output)?;
	Ok(())
}

fn retain_tail(path: &Path, bytes: u64) -> io::Result<()> {
	let mut file = File::open(path)?;
	let len = file.metadata()?.len();
	file.seek(SeekFrom::Start(len.saturating_sub(bytes)))?;
	let mut tail = Vec::with_capacity(bytes as usize);
	file.read_to_end(&mut tail)?;
	let mut output = OpenOptions::new().write(true).truncate(true).open(path)?;
	output.write_all(&tail)?;
	output.sync_all()
}
