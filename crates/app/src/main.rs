// Player engine is platform-agnostic; driven by ALSA wiring on Linux and by
// unit tests everywhere.
#[cfg(any(target_os = "linux", windows, test))]
mod player;

use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(any(target_os = "linux", windows))]
use sointty_core::{BufferConfig, PlayerCommand, QueueEntry};
use sointty_core::{DeviceId, PlayerError};

#[cfg(any(target_os = "linux", windows))]
use player::PlayerEngine;
#[cfg(target_os = "linux")]
use sointty_output_alsa::AlsaOutput;
#[cfg(windows)]
use sointty_output_wasapi::WasapiOutput;

#[cfg(target_os = "linux")]
const DEFAULT_DEVICE: &str = "hw:0,0";
#[cfg(not(target_os = "linux"))]
const DEFAULT_DEVICE: &str = "default";

struct Cli {
    device: DeviceId,
    period_frames: u32,
    buffer_frames: u32,
    list_devices: bool,
    /// Enable the library index at the default data dir.
    index: bool,
    /// Roots to (re)scan into the index; implies `index`.
    reindex: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

fn parse_cli() -> Result<Cli, PlayerError> {
    parse_cli_args(std::env::args().skip(1))
}

fn parse_cli_args(args: impl IntoIterator<Item = String>) -> Result<Cli, PlayerError> {
    let mut args = args.into_iter();
    let mut device = DEFAULT_DEVICE.to_owned();
    let mut period_frames = 2_205;
    let mut buffer_frames = 8_820;
    let mut list_devices = false;
    let mut index = false;
    let mut reindex = Vec::new();
    let mut files = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => {
                device = args.next().ok_or(PlayerError::InvalidInput(
                    "--device requires an audio device (e.g. \"default\" or an endpoint ID)",
                ))?;
            }
            "--period-frames" => {
                period_frames = args.next().and_then(|value| value.parse().ok()).ok_or(
                    PlayerError::InvalidInput("--period-frames requires a positive integer"),
                )?;
            }
            "--buffer-frames" => {
                buffer_frames = args.next().and_then(|value| value.parse().ok()).ok_or(
                    PlayerError::InvalidInput("--buffer-frames requires a positive integer"),
                )?;
            }
            "--list-devices" => {
                list_devices = true;
            }
            "--index" => {
                index = true;
            }
            "--reindex" => {
                index = true;
                let root = args.next().ok_or(PlayerError::InvalidInput(
                    "--reindex requires a directory to scan",
                ))?;
                if root.starts_with('-') {
                    return Err(PlayerError::InvalidInput(
                        "--reindex requires a directory to scan",
                    ));
                }
                reindex.push(PathBuf::from(root));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: sointty [--device DEVICE] [--period-frames N] [--buffer-frames N] [--list-devices]\n               [--index] [--reindex DIR]... [FILE|PLAYLIST]...\n\n--device DEVICE       audio output device (default: \"hw:0,0\" on Linux, \"default\" elsewhere;\n                      use --list-devices to see endpoint IDs)\n--list-devices        list audio output devices and exit\n--index               enable the library index at the default data dir\n--reindex DIR         scan DIR into the library index (repeat for more roots);\n                      prints scan stats, then plays any files given, else exits\n\nPlaylists (.m3u, .m3u8, .pls, .xspf, .cue) are expanded into tracks.\nWith no files, sointty opens the filesystem browser."
                );
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => {
                return Err(PlayerError::InvalidInput("unknown option; use --help"));
            }
            _ => files.push(PathBuf::from(arg)),
        }
    }
    Ok(Cli {
        device,
        period_frames,
        buffer_frames,
        list_devices,
        index,
        reindex,
        files,
    })
}

#[cfg(feature = "library-index")]
fn default_index_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "sointty", "sointty")
        .map(|dirs| dirs.data_dir().join("library.db"))
        .unwrap_or_else(|| PathBuf::from("sointty-index.db"))
}

#[cfg(feature = "library-index")]
fn index_error(error: sointty_library::LibraryError) -> PlayerError {
    eprintln!("sointty: library index: {error}");
    PlayerError::InvalidInput("library index operation failed (see message above)")
}

#[cfg(feature = "library-index")]
fn run_index(cli: &Cli) -> Result<(), PlayerError> {
    let db_path = default_index_path();
    // The DB is opened lazily: only reaching this path (the user enabled
    // index operations) creates or recreates it.
    let mut library = sointty_library::Library::open(&db_path).map_err(index_error)?;
    if !cli.reindex.is_empty() {
        let stats = library.scan(&cli.reindex).map_err(index_error)?;
        println!(
            "indexed {} files ({} inserted/updated, {} unchanged) into {}",
            stats.scanned,
            stats.inserted_or_updated,
            stats.skipped_unchanged,
            db_path.display()
        );
    }
    Ok(())
}

/// Handle `--index`/`--reindex` before playback or the browser starts.
/// Returns `true` when the program should exit after indexing.
fn run_index_dispatch(cli: &Cli) -> Result<bool, PlayerError> {
    if !cli.index && cli.reindex.is_empty() {
        return Ok(false);
    }
    #[cfg(feature = "library-index")]
    {
        run_index(cli)?;
        // A pure reindex (no files to play) exits Ok after scanning.
        Ok(!cli.reindex.is_empty() && cli.files.is_empty())
    }
    #[cfg(not(feature = "library-index"))]
    {
        Err(PlayerError::InvalidInput(
            "built without the library-index feature; rebuild with --features library-index",
        ))
    }
}

#[cfg(any(target_os = "linux", windows))]
fn is_playlist_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "m3u" | "m3u8" | "pls" | "xspf" | "cue"
            )
        })
}

#[cfg(target_os = "linux")]
fn run(cli: Cli) -> Result<(), PlayerError> {
    let (command_tx, command_rx) = crossbeam_channel::unbounded();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let device = cli.device.clone();
    let output = AlsaOutput::new(device.clone())?;
    let decoder_factory: player::DecoderFactory = Box::new(|path| {
        let (source, stall) = sointty_source::ReadAheadSource::open(path)?;
        let decoder = sointty_decode::open_decoder(path, Box::new(source))?;
        Ok(player::OpenedDecoder {
            decoder,
            stall: Some(stall),
        })
    });
    let engine = PlayerEngine::new(
        output,
        Box::new(move |device| AlsaOutput::new(device)),
        device,
        BufferConfig {
            period_frames: cli.period_frames,
            buffer_frames: cli.buffer_frames,
            ring_frames: cli.buffer_frames.saturating_mul(4).max(4096),
        },
        decoder_factory,
        event_tx,
        player::DEFAULT_STALL_TIMEOUT,
    );
    let player = std::thread::spawn(move || engine.run(command_rx));
    let open_browser = cli.files.is_empty();
    for file in cli.files {
        if is_playlist_path(&file) {
            match sointty_playlist::read(&file) {
                Ok(entries) => {
                    for entry in entries {
                        command_tx
                            .send(PlayerCommand::Enqueue(QueueEntry {
                                path: entry.path,
                                cue_range: entry.cue_range,
                            }))
                            .map_err(|_| PlayerError::Output)?;
                    }
                }
                Err(error) => eprintln!("sointty: {error}"),
            }
        } else {
            command_tx
                .send(PlayerCommand::Enqueue(QueueEntry::from(file)))
                .map_err(|_| PlayerError::Output)?;
        }
    }
    command_tx
        .send(PlayerCommand::Play)
        .map_err(|_| PlayerError::Output)?;
    let tui = sointty_tui::run(command_tx, event_rx, open_browser);
    let player_result = player.join().unwrap_or(Err(PlayerError::Output));
    tui.map_err(|_| PlayerError::Output)?;
    player_result
}

#[cfg(windows)]
fn run(cli: Cli) -> Result<(), PlayerError> {
    if cli.list_devices {
        for (id, name) in WasapiOutput::list_devices()? {
            println!("{id}\t{name}");
        }
        return Ok(());
    }
    let (command_tx, command_rx) = crossbeam_channel::unbounded();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let device = cli.device.clone();
    let output = WasapiOutput::new(&device)?;
    let decoder_factory: player::DecoderFactory = Box::new(|path| {
        let (source, stall) = sointty_source::ReadAheadSource::open(path)?;
        let decoder = sointty_decode::open_decoder(path, Box::new(source))?;
        Ok(player::OpenedDecoder {
            decoder,
            stall: Some(stall),
        })
    });
    let engine = PlayerEngine::new(
        output,
        Box::new(move |device| WasapiOutput::new(&device)),
        device,
        BufferConfig {
            period_frames: cli.period_frames,
            buffer_frames: cli.buffer_frames,
            ring_frames: cli.buffer_frames.saturating_mul(4).max(4096),
        },
        decoder_factory,
        event_tx,
        player::DEFAULT_STALL_TIMEOUT,
    );
    let player = std::thread::spawn(move || engine.run(command_rx));
    let open_browser = cli.files.is_empty();
    for file in cli.files {
        if is_playlist_path(&file) {
            match sointty_playlist::read(&file) {
                Ok(entries) => {
                    for entry in entries {
                        command_tx
                            .send(PlayerCommand::Enqueue(QueueEntry {
                                path: entry.path,
                                cue_range: entry.cue_range,
                            }))
                            .map_err(|_| PlayerError::Output)?;
                    }
                }
                Err(error) => eprintln!("sointty: {error}"),
            }
        } else {
            command_tx
                .send(PlayerCommand::Enqueue(QueueEntry::from(file)))
                .map_err(|_| PlayerError::Output)?;
        }
    }
    command_tx
        .send(PlayerCommand::Play)
        .map_err(|_| PlayerError::Output)?;
    let tui = sointty_tui::run(command_tx, event_rx, open_browser);
    let player_result = player.join().unwrap_or(Err(PlayerError::Output));
    tui.map_err(|_| PlayerError::Output)?;
    player_result
}

#[cfg(not(any(target_os = "linux", windows)))]
fn run(cli: Cli) -> Result<(), PlayerError> {
    let _ = (cli.device, cli.period_frames, cli.buffer_frames, cli.files);
    Err(PlayerError::InvalidInput(
        "audio output is not wired for this OS yet; use a Linux or Windows host",
    ))
}

fn main() -> ExitCode {
    let result = parse_cli().and_then(|cli| {
        if run_index_dispatch(&cli)? {
            return Ok(());
        }
        run(cli)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sointty: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, PlayerError> {
        parse_cli_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn reindex_takes_one_root_and_leaves_files() {
        // Regression: the old greedy loop swallowed a positional file as a
        // second scan root, so `sointty --reindex music track.flac` silently
        // never played track.flac.
        let cli = parse(&["--reindex", "music", "track.flac"]).unwrap();
        assert_eq!(cli.reindex, vec![PathBuf::from("music")]);
        assert_eq!(cli.files, vec![PathBuf::from("track.flac")]);
        assert!(cli.index);
    }

    #[test]
    fn repeated_reindex_collects_multiple_roots() {
        let cli = parse(&["--reindex", "a", "--reindex", "b"]).unwrap();
        assert_eq!(cli.reindex, vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert!(cli.files.is_empty());
    }

    #[test]
    fn reindex_without_root_errors() {
        assert!(matches!(
            parse(&["--reindex"]),
            Err(PlayerError::InvalidInput(_))
        ));
        assert!(matches!(
            parse(&["--reindex", "--index"]),
            Err(PlayerError::InvalidInput(_))
        ));
    }
}
