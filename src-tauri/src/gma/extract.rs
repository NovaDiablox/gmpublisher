use std::{
	fs::{self, File},
	io::{Read, BufWriter, Cursor, SeekFrom},
	path::{Path, PathBuf},
	sync::atomic::{AtomicUsize, Ordering},
};

use crate::{app_data, transactions::Transaction};

use super::{GMAEntry, GMAError, GMAFile, GMAMetadata, GMAReader, whitelist};

use lazy_static::lazy_static;
use rayon::{
	iter::{IntoParallelRefIterator, ParallelIterator},
	ThreadPool, ThreadPoolBuilder,
};
use serde::{Deserialize, Serialize};

lazy_static! {
	pub static ref THREAD_POOL: ThreadPool = ThreadPoolBuilder::new().build().unwrap();
}

/*#[derive(Clone, Copy)]
pub enum AfterExtract {
	None,
	Open,
	UserPreference,
	Future(&'static (dyn Fn() -> bool + Send + Sync + 'static)),
}
impl Into<bool> for AfterExtract {
    fn into(self) -> bool {
        match self {
            AfterExtract::None => false,
            AfterExtract::Open => true,
			AfterExtract::UserPreference => app_data!().settings.read().open_gma_after_extract,
            AfterExtract::Future(f) => (f)()
        }
    }
}*/

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExtractDestination {
	Temp,
	Downloads,
	Addons,
	/// path/to/addon/*
	Directory(PathBuf),
	/// path/to/addon/addon_name_123456790/*
	NamedDirectory(PathBuf),
}
impl ExtractDestination {
	fn into<S: AsRef<str>>(self, extracted_name: S) -> PathBuf {
		use ExtractDestination::*;

		let push_extracted_name = |mut path: PathBuf| {
			path.push(extracted_name.as_ref());
			Some(path)
		};

		match self {
			Temp => None,

			Directory(path) => Some(path),

			Addons => app_data!().gmod_dir().and_then(|mut path| {
				path.push("GarrysMod");
				path.push("addons");
				Some(path)
			}),

			Downloads => app_data!().downloads_dir().to_owned().and_then(push_extracted_name),

			NamedDirectory(path) => push_extracted_name(path),
		}
		.unwrap_or_else(|| push_extracted_name(app_data!().temp_dir().to_owned()).unwrap())
	}
}
impl Default for ExtractDestination {
	fn default() -> Self {
		ExtractDestination::Temp
	}
}

impl GMAFile {
	pub fn decompress<P: AsRef<Path>>(path: P) -> Result<GMAFile, GMAError> {
		main_thread_forbidden!();

		// TODO somehow, in some really unsafe and stupid way, monitor the progress of decompression?

		let input = File::open(path.as_ref())?;

		let lzma_decoder =  xz2::stream::Stream::new_lzma_decoder(u64::MAX).map_err(|_| GMAError::LZMA)?;
		let mut xz_decoder = xz2::read::XzDecoder::new_stream(input, lzma_decoder);

		let mut output = Vec::new();
		if let Err(err) = xz_decoder.read_to_end(&mut output) {
			if !matches!(err.kind(), std::io::ErrorKind::Other) {
				return Err(GMAError::LZMA);
			}
		}

		Ok(GMAFile::read_header(GMAReader::MemBuffer(Cursor::new(output.into())), path)?)
	}

	pub fn extract(&mut self, dest: ExtractDestination, transaction: Transaction, open_after_extract: bool) -> Result<PathBuf, GMAError> {
		main_thread_forbidden!();

		THREAD_POOL.install(move || {
			self.entries()?;

			let dest_path = dest.into(&self.extracted_name);
			let entries_start = self.pointers.entries;

			let entries = self.entries.as_ref().unwrap();
			let entries_len_f = entries.len() as f64;
			let entries_len_i = entries.len();

			self.read()?; // Don't waste time with the threads if the file fails to open

			let i = AtomicUsize::new(0);

			let finished = |mut dest_path: PathBuf| {
				if i.fetch_add(1, Ordering::AcqRel) > entries_len_i { return; }

				if open_after_extract {
					ignore! { crate::path::open(&dest_path) };
				}

				transaction.finished(Some(dest_path.to_owned()));

				let metadata = self.metadata.as_ref().unwrap();
				if let GMAMetadata::Standard { .. } = metadata {
					if let Ok(json) = serde_json::ser::to_string_pretty(metadata) {
						dest_path.push("addon.json");
						ignore! { fs::write(dest_path, json.as_bytes()) };
						//dest_path.pop();
					}
				}
			};

			match entries.par_iter().try_for_each(
				|(entry_path, entry)| -> Result<(), GMAError> {
					let mut handle = self.read()?;

					if whitelist::check(entry_path) {
						// FIXME count errors, check if errors == number of entries, return an error instead of finished
						ignore! { GMAFile::stream_entry_bytes(&mut handle, entries_start, &dest_path.join(entry_path), entry) };

						let i = i.fetch_add(1, Ordering::AcqRel) + 1;
						transaction.progress((i as f64) / entries_len_f);

						if i == entries_len_i {
							(finished)(dest_path.to_owned());
						}
					} else {
						transaction.error("ERR_WHITELIST", entry_path.clone());
					}

					Ok(())
				},
			)
			{
				Ok(_) => {
					(finished)(dest_path.to_owned());
					Ok(dest_path)
				},

				Err(err) => {
					transaction.error(err.to_string(), turbonone!());
					Err(err)
				},
			}
		})
	}

	pub fn extract_entry(&mut self, entry_path: String) -> Result<PathBuf, GMAError> {
		main_thread_forbidden!();

		let mut handle = match self.entries()? {
			Some(handle) => handle,
			None => self.read()?,
		};

		let entry = self.entries.as_ref().unwrap().get(&entry_path).ok_or(GMAError::EntryNotFound)?;
		debug_assert_ne!(entry.index, 0);

		let mut path = std::env::temp_dir();
		path.push("gmpublisher");
		path.push(&self.extracted_name);
		path.push(&entry_path);

		GMAFile::stream_entry_bytes(&mut handle, self.pointers.entries, &path, entry)?;

		Ok(path)
	}

	fn stream_entry_bytes(
		handle: &mut GMAReader,
		entries_start: u64,
		entry_path: &PathBuf,
		entry: &GMAEntry,
	) -> Result<(), GMAError> {
		use std::io::Write;

		fs::create_dir_all(&entry_path.with_file_name(""))?;
		let f = File::create(&entry_path)?;

		handle.seek(SeekFrom::Start(entries_start + entry.index))?;

		let mut w = BufWriter::new(f);
		crate::stream_bytes(&mut **handle, &mut w, entry.size as usize)?;

		w.flush()?;

		Ok(())
	}
}
