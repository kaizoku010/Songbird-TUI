use lofty::{prelude::*, probe::Probe};
use rayon::prelude::*;
use std::{path::{Path, PathBuf}, sync::atomic::{AtomicUsize, Ordering}};
use walkdir::WalkDir;

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "m4a", "aac"];

/// Represents one discovered music file and its metadata.
#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    pub bitrate: u32,
    pub codec: String,
    pub lossless: bool,
    pub year: Option<i32>,
}

/// Returns true if a file has a known music extension.
fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

/// Builds a basic track record when metadata parsing fails.
/// This keeps the library usable even if a file is damaged or unsupported.
fn fallback(path: &Path) -> Track {
    let codec = path.extension().and_then(|e| e.to_str()).unwrap_or("audio").to_uppercase();
    let lossless = matches!(codec.as_str(), "FLAC" | "WAV" | "ALAC");
    Track {
        path: path.to_path_buf(),
        title: path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown Track").to_string(),
        artist: "Unknown Artist".into(),
        album: "Unknown Album".into(),
        duration: 0.0,
        bitrate: 0,
        codec,
        lossless,
        year: None,
    }
}

/// Reads metadata from a single file using the `lofty` crate.
/// If parsing fails, we return a fallback record with filename-based metadata.
fn parse_track(path: &Path) -> Track {
    let fallback_track = fallback(path);
    let Ok(tagged_file) = Probe::open(path).and_then(|p| p.read()) else {
        return fallback_track;
    };

    let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag());
    let props = tagged_file.properties();
    let ext_codec = path.extension().and_then(|e| e.to_str()).unwrap_or("audio").to_uppercase();

    Track {
        path: path.to_path_buf(),
        title: tag.and_then(|t| t.title().map(|v| v.to_string())).unwrap_or(fallback_track.title),
        artist: tag.and_then(|t| t.artist().map(|v| v.to_string())).unwrap_or_else(|| "Unknown Artist".into()),
        album: tag.and_then(|t| t.album().map(|v| v.to_string())).unwrap_or_else(|| "Unknown Album".into()),
        duration: props.duration().as_secs_f64(),
        bitrate: props.overall_bitrate().unwrap_or(0),
        codec: ext_codec,
        lossless: matches!(path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str(), "flac" | "wav" | "alac"),
        year: tag.and_then(|t| t.year()).map(|y| y as i32),
    }
}

/// Recursively scans a folder for music files and parses track metadata.
/// The `progress` callback is fired as files are processed so the UI can show scanning state.
pub fn scan_music_folder<F>(folder: &str, progress: F) -> Vec<Track>
where
    F: Fn(usize, usize) + Send + Sync,
{
    let root = Path::new(folder);
    if !root.is_dir() {
        return Vec::new();
    }

    let files: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && is_audio(e.path()))
        .map(|e| e.into_path())
        .collect();

    let total = files.len();
    let done = AtomicUsize::new(0);
    let mut tracks: Vec<Track> = files
        .par_iter()
        .map(|file| {
            let track = parse_track(file);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(n, total);
            track
        })
        .collect();

    tracks.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    tracks
}
