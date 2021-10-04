use anyhow::{anyhow, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use itertools::Itertools;
use libosu::db::{Db, DbBeatmap};
use rfd::FileDialog;
use rusqlite::Connection;
use std::{
    collections::HashSet,
    convert::TryInto,
    fs::File,
    io::{stdin, stdout, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::mpsc::channel,
    thread::spawn,
};

mod database;
mod processors;

use crate::processors::{
    context::{BeatmapProcessed, HashProcessed, HashRequest},
    BeatmapProcessor, HashProcessor,
};

// Uses a random 32 characters long hexadecimal string instead of calculating the sha256 of each map
// It *works*, but not recommended as it isn't what lazer expects
const FAKE_HASH: bool = false;

// The last SQLite migration ID, used for version checking
const LAST_MIGRATION_ID: &str = "20210912144011_AddSamplesMatchPlaybackRate";

// Difference between windows epoch (0001/01/01 12:00:00 UTC) to unix epoch (1970/01/01 12:00:00 UTC)
// Units are in windows ticks; 1 tick = 100ns; 10 000 ticks = 1ms
const WIN_TO_UNIX_EPOCH: u64 = 621_355_968_000_000_000;

struct ProgressBars {
    manager: MultiProgress,
    beatmap: ProgressBar,
    beatmap_insert: ProgressBar,
    hash: ProgressBar,
    hash_insert: ProgressBar,
}

struct ProgressStyles {
    length_unchanging: ProgressStyle,
    length_changing: ProgressStyle,
    waiting: ProgressStyle,
}

pub struct State {
    pub lazer_path: PathBuf,
    pub stable_path: PathBuf,
    pub lazer_db_path: PathBuf,
    pub stable_db_path: PathBuf,
    pub stable_songs_path: PathBuf,

    db_online_connection: Connection,
    progress_bars: ProgressBars,
    progress_styles: ProgressStyles,
}

impl State {
    fn new() -> Result<Self> {
        let lazer_path = get_lazer_path()?;

        let mut lazer_db_path = lazer_path.clone();
        lazer_db_path.push("client.db");
        if !lazer_db_path.exists() {
            return Err(anyhow!(
                "Not a valid osu!lazer directory? (missing client.db)"
            ));
        };

        let mut lazer_online_db_path = lazer_path.clone();
        lazer_online_db_path.push("online.db");
        if !lazer_online_db_path.exists() {
            return Err(anyhow!(
                "Missing osu!lazer online.db, try opening the game, closing it, and then rerunning this tool?"
            ));
        };

        let stable_path = get_stable_path()?;
        let stable_db_path = stable_path.join("osu!.db");
        let stable_songs_path = get_songs_directory(&stable_path)?;

        #[cfg(target_family = "windows")]
        if let Err(_) = windows_link_check(&lazer_path, &stable_path) {
            return Err(anyhow!("Hard link test failed! On Windows, both lazer and stable must be on the same disk for linking to work."));
        }

        let db_online_connection =
            Connection::open(&lazer_online_db_path).context("Failed to open online.db")?;

        let progress_styles = ProgressStyles {
            length_unchanging: ProgressStyle::default_bar()
                .template("{prefix} {msg:17} [{wide_bar}] {percent:>3}% {pos:>8}/{len:8}")
                .progress_chars("=> "),
            length_changing: ProgressStyle::default_bar()
                .template("{prefix} {msg:17} [{wide_bar}] {percent:>3}% {pos:>8}/{len:8}")
                .progress_chars("-> "),
            waiting: ProgressStyle::default_spinner()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈")
                .template("{prefix} {wide_msg} {spinner} /{len:8}"),
        };

        let manager = MultiProgress::new();
        manager.set_draw_target(ProgressDrawTarget::hidden());

        let beatmap = manager
            .add(ProgressBar::new(0))
            .with_prefix("Processing beatmaps:")
            .with_style(progress_styles.length_unchanging.clone());
        beatmap.tick();

        let beatmap_insert = manager
            .add(ProgressBar::new(0))
            .with_prefix("Inserting beatmaps: ")
            .with_style(progress_styles.length_changing.clone());
        beatmap_insert.tick();

        let hash = manager
            .add(ProgressBar::new(0))
            .with_prefix("Processing files:   ")
            .with_style(progress_styles.length_changing.clone());
        hash.tick();

        let hash_insert = manager
            .add(ProgressBar::new(0))
            .with_prefix("Inserting files:    ")
            .with_style(progress_styles.waiting.clone())
            .with_message("Waiting...");
        hash_insert.enable_steady_tick(250);

        Ok(Self {
            lazer_path,
            lazer_db_path,
            stable_path,
            stable_db_path,
            stable_songs_path,

            db_online_connection,
            progress_bars: ProgressBars {
                manager,
                beatmap,
                beatmap_insert,
                hash,
                hash_insert,
            },
            progress_styles,
        })
    }

    fn show_progress(&self) {
        self.progress_bars
            .manager
            .set_draw_target(ProgressDrawTarget::stderr());
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:?}", e);

        #[cfg(target_os = "windows")]
        {
            eprintln!();
            eprint!("Press enter to exit");
            stdout().flush().unwrap();
            wait_for_input().unwrap();
        }
    }
}

fn run() -> Result<()> {
    let state = State::new()?;

    println!("Preparing...");

    let mut db_connection = Connection::open(&state.lazer_db_path)?;

    if !check_version(&db_connection)? {
        return Err(anyhow!("Database version mismatch! Please make sure you have the latest versions of both osu! and osu-link"));
    }

    let (stable_len, lazer_len, beatmaps) = get_beatmaps(&state, &db_connection)?;

    println!("Stable path: {:?}", state.stable_path);
    println!("Stable songs path: {:?}", state.stable_songs_path);
    println!("Lazer path: {:?}", state.lazer_path);
    println!("Stable beatmap count: {}", stable_len);
    println!("Lazer beatmap count: {}", lazer_len);
    println!("Make sure both osu!stable and osu!lazer are closed!");
    println!("Also back up your osu!lazer folder before continuing!");
    print!("Press enter to continue, Ctrl+C to cancel");
    stdout().flush()?;
    wait_for_input()?;

    state.show_progress();

    let (bm_sx, bm_rx) = channel::<BeatmapProcessed>();
    let (hash_req_sx, hash_req_rx) = channel::<HashRequest>();
    let (hash_sx, hash_rx) = channel::<HashProcessed>();

    let b_ctx = BeatmapProcessor::new(&state);
    let beatmap_thread = spawn(move || {
        b_ctx.start(beatmaps, bm_sx);
    });
    let h_ctx = HashProcessor::new(&state);
    let hash_thread = spawn(move || {
        h_ctx.start(hash_sx, hash_req_rx);
    });

    let transaction = db_connection.transaction()?;

    database::insert_beatmaps(&state, &transaction, bm_rx, hash_req_sx)?;
    state
        .progress_bars
        .beatmap_insert
        .finish_with_message("Done.");

    state.progress_bars.hash_insert.disable_steady_tick();
    state
        .progress_bars
        .hash_insert
        .set_style(state.progress_styles.length_unchanging.clone());
    database::insert_hashes(&state, &transaction, hash_rx)?;
    state.progress_bars.hash_insert.finish_with_message("Done.");

    beatmap_thread.join().unwrap();
    hash_thread.join().unwrap();

    let db_progress = ProgressBar::new_spinner()
        .with_prefix("Database:           ")
        .with_message("Committing")
        .with_style(state.progress_styles.waiting);
    db_progress.tick();
    transaction.commit()?;
    db_progress.finish_with_message("Done.");

    Ok(())
}

fn get_beatmaps(
    state: &State,
    db_connection: &Connection,
) -> Result<(usize, usize, Vec<DbBeatmap>)> {
    let fd = File::open(&state.stable_db_path)?;
    let beatmaps = Db::parse(BufReader::new(fd))?.beatmaps;
    let mut stable_beatmaps: HashSet<u32> = beatmaps.iter().map(|bm| bm.beatmap_id).collect();
    let stable_len = stable_beatmaps.len();

    let mut query = db_connection.prepare(
        "
        SELECT OnlineBeatmapID
        FROM BeatmapInfo
        WHERE OnlineBeatmapID NOT NULL
    ",
    )?;

    let lazer_beatmaps = query.query_map([], |row| row.get::<_, u32>(0))?;
    let mut lazer_len = 0;

    for b in lazer_beatmaps {
        lazer_len += 1;
        stable_beatmaps.remove(&b?);
    }

    let mut beatmaps = beatmaps
        .into_iter()
        .filter(|bm| {
            stable_beatmaps.contains(&bm.beatmap_id) &&
            // TODO: unsubmitted maps
            bm.beatmap_id != 0 &&
            bm.beatmap_set_id != u32::MAX
        })
        .collect_vec();
    beatmaps.sort_unstable_by(|a, b| a.beatmap_id.cmp(&b.beatmap_id));
    state
        .progress_bars
        .beatmap
        .set_length(beatmaps.len().try_into()?);

    Ok((stable_len, lazer_len, beatmaps))
}

fn get_songs_directory(stable_path: &Path) -> Result<PathBuf> {
    let username = whoami::username();
    let mut path = stable_path.to_path_buf();
    path.push(format!("osu!.{}.cfg", username));

    let fd = File::open(path)?;
    let reader = BufReader::new(fd);

    for line in reader.lines() {
        let line = line?;

        if line.starts_with("BeatmapDirectory") {
            let parts = line.split('=').collect_vec();

            let mut path = stable_path.to_path_buf();
            path.push(parts.get(1).unwrap().trim());
            return Ok(path);
        }
    }

    let mut path = stable_path.to_path_buf();
    path.push("Songs");
    Ok(path)
}

fn check_version(conn: &Connection) -> Result<bool> {
    let last_migration: String = conn.query_row(
        "SELECT MigrationId FROM __EFMigrationsHistory
         ORDER BY MigrationId DESC
         LIMIT 1",
        [],
        |row| row.get(0),
    )?;

    Ok(last_migration == LAST_MIGRATION_ID)
}

fn check_stable_path(path: &Path) -> bool {
    path.join("osu!.db").exists()
}

fn prompt_stable_path() -> Result<PathBuf> {
    print!("You will be prompted to select the path to your osu!stable directory, press enter to continue");
    stdout().flush()?;
    wait_for_input()?;

    let path = FileDialog::new()
        .set_title("Select the path to your osu!stable directory")
        .set_directory(dirs::data_dir().unwrap_or_else(|| "/".into()))
        .pick_folder()
        .context("Failed to select the directory..?")?;

    if check_stable_path(&path) {
        Ok(path)
    } else {
        Err(anyhow!(
            "Not a valid osu!stable directory? (missing osu!.db)"
        ))
    }
}

fn get_stable_path() -> Result<PathBuf> {
    // https://osu.ppy.sh/wiki/en/osu%21_Program_Files#installation-paths
    #[cfg(target_os = "macos")]
    let path = Some(PathBuf::from(
        "/Applications/osu!.app/Contents/Resources/drive_c/osu!",
    ));

    #[cfg(target_os = "linux")]
    let path = dirs::data_local_dir().map(|p| p.join("osu!"));

    #[cfg(target_os = "windows")]
    let path = get_stable_path_from_registry().ok();

    if let Some(path) = path {
        if check_stable_path(&path) {
            return Ok(path);
        }
    }

    prompt_stable_path()
}

fn get_lazer_path() -> Result<PathBuf> {
    let path = dirs::data_dir().context("No data directory?")?.join("osu");

    let custom_storage = path.join("storage.ini");
    if custom_storage.exists() {
        let fd = File::open(custom_storage)?;

        for line in BufReader::new(fd).lines() {
            let line = line?;

            if line.starts_with("FullPath") {
                let parts = line.split('=').collect_vec();

                return Ok(PathBuf::from(parts.get(1).unwrap().trim()));
            }
        }
    }

    if path.join("client.db").exists() {
        Ok(path)
    } else {
        Err(anyhow!(
            "Can't find lazer path, do you have the game installed?"
        ))
    }
}

#[cfg(target_family = "windows")]
fn get_stable_path_from_registry() -> Result<PathBuf> {
    use winreg::{enums::HKEY_CLASSES_ROOT, RegKey};

    let root = RegKey::predef(HKEY_CLASSES_ROOT);
    let value: String = root
        .open_subkey("osu\\shell\\open\\command")?
        .get_value("")?;
    let path = value
        .split('"')
        .collect_vec()
        .get(1)
        .context("invalid regkey")?
        .trim()
        .replace("osu!.exe", "");

    let path = PathBuf::from(path);
    if path.exists() {
        Ok(path)
    } else {
        Err(anyhow!("registry path does not exist"))
    }
}

fn wait_for_input() -> Result<()> {
    let mut str = String::new();
    stdin().read_line(&mut str)?;

    Ok(())
}

#[cfg(target_family = "windows")]
fn windows_link_check(lazer_path: &std::path::Path, stable_path: &std::path::Path) -> Result<()> {
    let mut lazer_path = lazer_path.to_path_buf();
    lazer_path.push("_link_test");
    let mut stable_path = stable_path.to_path_buf();
    stable_path.push("_link_test");

    std::fs::write(&lazer_path, "hello from osu-link!")?;

    let res = std::fs::hard_link(&lazer_path, &stable_path);

    std::fs::remove_file(&lazer_path)?;
    let _ = std::fs::remove_file(&stable_path);

    res?;

    Ok(())
}
