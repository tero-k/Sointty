//! Read-only metadata tags via lofty. Tags are best-effort: any I/O or
//! parse failure yields `None` and never blocks playback.

use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use sointty_core::TrackTags;

pub fn read_tags(path: &std::path::Path) -> Option<TrackTags> {
    let tagged_file = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag())?;
    Some(TrackTags {
        title: tag.get_string(ItemKey::TrackTitle).map(str::to_owned),
        artist: tag.get_string(ItemKey::TrackArtist).map(str::to_owned),
        album: tag.get_string(ItemKey::AlbumTitle).map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_fixture() -> Vec<u8> {
        let samples: [i16; 8] = [0, 1, -1, i16::MAX, i16::MIN, 2, -2, 3];
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&44_100_u32.to_le_bytes());
        bytes.extend_from_slice(&(44_100_u32 * 2 * 2).to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn temp_wav(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sointty-tags-test-{name}-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, wav_fixture()).unwrap();
        path
    }

    #[test]
    fn missing_file_returns_none() {
        let path = std::env::temp_dir().join("sointty-tags-test-definitely-missing.opus");
        assert_eq!(read_tags(&path), None);
    }

    #[test]
    fn untagged_wav_does_not_panic() {
        let path = temp_wav("untagged");
        let tags = read_tags(&path);
        let _ = std::fs::remove_file(&path);
        // An untagged WAV either has no tag (None) or an empty one (default).
        if let Some(tags) = tags {
            assert_eq!(tags, TrackTags::default());
        }
    }
}
