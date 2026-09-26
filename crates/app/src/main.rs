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
    /// CLI override; the config file and built-in default fill in when `None`.
    device: Option<DeviceId>,
    period_frames: Option<u32>,
    buffer_frames: Option<u32>,
    float_to_int: Option<bool>,
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
    let mut device = None;
    let mut period_frames = None;
    let mut buffer_frames = None;
    let mut float_to_int = None;
    let mut list_devices = false;
    let mut index = false;
    let mut reindex = Vec::new();
    let mut files = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => {
                device = Some(args.next().ok_or(PlayerError::InvalidInput(
                    "--device requires an audio device (e.g. \"default\" or an endpoint ID)",
                ))?);
            }
            "--period-frames" => {
                period_frames = Some(args.next().and_then(|value| value.parse().ok()).ok_or(
                    PlayerError::InvalidInput("--period-frames requires a positive integer"),
                )?);
            }
            "--buffer-frames" => {
                buffer_frames = Some(args.next().and_then(|value| value.parse().ok()).ok_or(
                    PlayerError::InvalidInput("--buffer-frames requires a positive integer"),
                )?);
            }
            "--allow-float-to-int" => float_to_int = Some(true),
            "--no-float-to-int" => float_to_int = Some(false),
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
                    r#"Usage: sointty [--device DEVICE] [--period-frames N] [--buffer-frames N]
               [--allow-float-to-int|--no-float-to-int] [--list-devices]
               [--index] [--reindex DIR]... [FILE|PLAYLIST]...

--device DEVICE       audio output device (default: "hw:0,0" on Linux, "default" elsewhere;
                      use --list-devices to see endpoint IDs)
--allow-float-to-int  opt in to F32-to-integer conversion if exact F32 is unavailable
                      (converted playback is NOT bit-perfect)
--no-float-to-int     force strict mode for this run
--list-devices        list audio output devices and exit
--index               enable the library index at the default data dir
--reindex DIR         scan DIR into the library index (repeat for more roots);
                      prints scan stats, then plays any files given, else exits

Settings persist in <config dir>/sointty/config.toml; CLI flags override the file.
In the TUI, press 'd' to pick output and 'c' to toggle F32 compatibility.

Keys: space play/pause, s stop, n next, left/right seek 10 s,
      b browser, d output, c F32-to-integer compatibility, q quit.
Playlists (.m3u, .m3u8, .pls, .xspf, .cue) are expanded into tracks.
With no files, sointty opens the filesystem browser."#
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
        float_to_int,
        list_devices,
        index,
        reindex,
        files,
    })
}

/// Persistent settings from `<config dir>/sointty/config.toml`; every field is
/// optional. CLI flags override the file, the file overrides the built-in
/// defaults. The TUI device picker (`d`) saves `device` here.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Config {
    device: Option<DeviceId>,
    period_frames: Option<u32>,
    buffer_frames: Option<u32>,
    float_to_int: Option<bool>,
}

fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "sointty", "sointty")
        .map(|dirs| dirs.config_dir().join("config.toml"))
}

fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str(&text) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("sointty: ignoring invalid config {}: {error}", path.display());
            Config::default()
        }
    }
}

fn save_config(config: &Config) -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(config).map_err(std::io::Error::other)?;
    std::fs::write(&path, text)
}

/// Playback settings after applying config file and CLI overrides.
struct Settings {
    device: DeviceId,
    period_frames: u32,
    buffer_frames: u32,
    float_to_int: bool,
}

fn resolve_settings(cli: &Cli, config: &Config) -> Settings {
    Settings {
        device: cli
            .device
            .clone()
            .or_else(|| config.device.clone())
            .unwrap_or_else(|| DEFAULT_DEVICE.to_owned()),
        period_frames: cli.period_frames.or(config.period_frames).unwrap_or(2_205),
        buffer_frames: cli.buffer_frames.or(config.buffer_frames).unwrap_or(8_820),
        float_to_int: cli.float_to_int.or(config.float_to_int).unwrap_or(false),
    }
}

/// Parse `/proc/asound/pcm` into selectable `hw:CARD,DEV` playback endpoints.
/// Lines look like `00-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1`;
/// entries without a playback stream are skipped.
#[cfg(any(target_os = "linux", test))]
fn parse_asound_pcm(text: &str) -> Vec<(DeviceId, String)> {
    let mut devices = Vec::new();
    for line in text.lines() {
        let Some((id, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((card, device)) = id.trim().split_once('-') else {
            continue;
        };
        let (Ok(card), Ok(device)) = (card.parse::<u32>(), device.parse::<u32>()) else {
            continue;
        };
        if !rest.contains("playback") {
            continue;
        }
        let name = rest.split(':').next().unwrap_or("").trim().to_owned();
        devices.push((format!("hw:{card},{device}"), name));
    }
    devices
}

#[cfg(target_os = "linux")]
fn list_devices() -> Result<Vec<(DeviceId, String)>, PlayerError> {
    let text = std::fs::read_to_string("/proc/asound/pcm")
        .map_err(|error| PlayerError::Io(error.kind()))?;
    Ok(parse_asound_pcm(&text))
}

#[cfg(windows)]
fn list_devices() -> Result<Vec<(DeviceId, String)>, PlayerError> {
    WasapiOutput::list_devices()
}

#[cfg(any(target_os = "linux", windows))]
fn print_devices() -> Result<(), PlayerError> {
    for (id, name) in list_devices()? {
        println!("{id}\t{name}");
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn print_devices() -> Result<(), PlayerError> {
    Err(PlayerError::InvalidInput(
        "device listing is not available on this OS",
    ))
}

#[cfg(any(target_os = "linux", windows))]
fn device_provider() -> sointty_tui::DeviceProvider {
    Box::new(|| list_devices().unwrap_or_default())
}

/// Persists a TUI device pick; other config fields are preserved.
#[cfg(any(target_os = "linux", windows))]
fn device_saver() -> sointty_tui::DeviceSaver {
    Box::new(|device| {
        let mut config = load_config();
        config.device = Some(device.clone());
        save_config(&config).map_err(|error| error.to_string())
    })
}

#[cfg(any(target_os = "linux", windows))]
fn float_to_int_saver() -> sointty_tui::FloatCompatibilitySaver {
    Box::new(|enabled| {
        let mut config = load_config();
        config.float_to_int = Some(enabled);
        save_config(&config).map_err(|error| error.to_string())
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
fn open_output(device: &DeviceId) -> Result<AlsaOutput, PlayerError> {
    AlsaOutput::new(device.clone())
}

#[cfg(windows)]
fn open_output(device: &DeviceId) -> Result<WasapiOutput, PlayerError> {
    WasapiOutput::new(device)
}

#[cfg(any(target_os = "linux", windows))]
fn run(cli: Cli, settings: Settings) -> Result<(), PlayerError> {
    let (command_tx, command_rx) = crossbeam_channel::unbounded();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let device = settings.device.clone();
    let output = open_output(&device)?;
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
        Box::new(move |device| open_output(&device)),
        device,
        BufferConfig {
            period_frames: settings.period_frames,
            buffer_frames: settings.buffer_frames,
            ring_frames: settings.buffer_frames.saturating_mul(4).max(4096),
        },
        settings.float_to_int,
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
    if !open_browser {
        command_tx
            .send(PlayerCommand::Play)
            .map_err(|_| PlayerError::Output)?;
    }
    let tui = sointty_tui::run(
        command_tx,
        event_rx,
        open_browser,
        device_provider(),
        settings.device.clone(),
        device_saver(),
        settings.float_to_int,
        float_to_int_saver(),
    );
    let player_result = player.join().unwrap_or(Err(PlayerError::Output));
    tui.map_err(|_| PlayerError::Output)?;
    player_result
}

#[cfg(not(any(target_os = "linux", windows)))]
fn run(cli: Cli, settings: Settings) -> Result<(), PlayerError> {
    let _ = (cli.files, settings.device, settings.period_frames, settings.buffer_frames, settings.float_to_int);
    Err(PlayerError::InvalidInput(
        "audio output is not wired for this OS yet; use a Linux or Windows host",
    ))
}

fn main() -> ExitCode {
    let result = parse_cli().and_then(|cli| {
        if cli.list_devices {
            return print_devices();
        }
        if run_index_dispatch(&cli)? {
            return Ok(());
        }
        let settings = resolve_settings(&cli, &load_config());
        run(cli, settings)
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

    #[test]
    fn device_and_buffer_flags_override_config() {
        let cli = parse(&[
            "--device",
            "hw:1,0",
            "--period-frames",
            "1024",
            "--buffer-frames",
            "4096",
        ])
        .unwrap();
        let config = Config {
            device: Some("hw:9,9".to_owned()),
            period_frames: Some(512),
            buffer_frames: Some(2048),
            float_to_int: Some(false),
        };
        let settings = resolve_settings(&cli, &config);
        assert_eq!(settings.device, "hw:1,0");
        assert_eq!(settings.period_frames, 1024);
        assert_eq!(settings.buffer_frames, 4096);
    }

    #[test]
    fn config_fills_in_when_cli_is_silent() {
        let cli = parse(&[]).unwrap();
        let config = Config {
            device: Some("usb-dac".to_owned()),
            period_frames: None,
            buffer_frames: Some(16_384),
            float_to_int: Some(true),
        };
        let settings = resolve_settings(&cli, &config);
        assert_eq!(settings.device, "usb-dac");
        assert_eq!(settings.period_frames, 2_205);
        assert_eq!(settings.buffer_frames, 16_384);
        assert!(settings.float_to_int);
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config {
            device: Some("hw:2,0".to_owned()),
            period_frames: Some(4_410),
            buffer_frames: None,
            float_to_int: Some(true),
        };
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.device.as_deref(), Some("hw:2,0"));
        assert_eq!(parsed.period_frames, Some(4_410));
        assert_eq!(parsed.float_to_int, Some(true));
        assert_eq!(parsed.buffer_frames, None);
    }

    #[test]
    fn float_compatibility_is_opt_in_with_cli_override() {
        let config = Config { float_to_int: Some(true), ..Config::default() };
        assert!(!resolve_settings(&parse(&[]).unwrap(), &Config::default()).float_to_int);
        assert!(resolve_settings(&parse(&[]).unwrap(), &config).float_to_int);
        assert!(!resolve_settings(&parse(&["--no-float-to-int"]).unwrap(), &config).float_to_int);
        let off = Config { float_to_int: Some(false), ..Config::default() };
        assert!(resolve_settings(&parse(&["--allow-float-to-int"]).unwrap(), &off).float_to_int);
    }

    #[test]
    fn asound_pcm_lists_only_playback_devices() {
        let text = "\
00-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1
00-02: ALC892 Alt Analog : ALC892 Alt Analog : capture 1
01-00: USB Audio : USB Audio : playback 1
";
        let devices = parse_asound_pcm(text);
        assert_eq!(
            devices,
            vec![
                ("hw:0,0".to_owned(), "ALC892 Analog".to_owned()),
                ("hw:1,0".to_owned(), "USB Audio".to_owned()),
            ]
        );
    }
}
