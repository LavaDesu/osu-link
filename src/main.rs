use anyhow::{anyhow, Context, Result};
use chrono::{TimeZone, Utc};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use itertools::Itertools;
use libosu::{
    beatmap::Beatmap,
    db::{Db, DbBeatmap},
    events::Event,
    prelude::{Mode, Mods},
    timing::{TimingPointKind, UninheritedTimingInfo},
};
use rand::{thread_rng, Rng};
use rayon::prelude::*;
use rfd::FileDialog;
use rusqlite::{params, Connection, Transaction};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    convert::TryInto,
    fmt::Write as FmtWrite,
    fs::File,
    io::{stdin, stdout, BufReader, Read, Write as IoWrite},
    path::{Path, PathBuf},
    sync::mpsc::{channel, Receiver, Sender},
    thread::spawn,
};
use walkdir::WalkDir;

// Uses a random 32 characters long hexadecimal string instead of calculating the sha256 of each map
// It *works*, but not recommended as it isn't what lazer expects
const FAKE_HASH: bool = false;

// The last SQLite migration ID, used for version checking
const LAST_MIGRATION_ID: &str = "20210912144011_AddSamplesMatchPlaybackRate";

// Difference between windows epoch (0001/01/01 12:00:00 UTC) to unix epoch (1970/01/01 12:00:00 UTC)
// Units are in windows ticks; 1 tick = 100ns; 10 000 ticks = 1ms
const WIN_TO_UNIX_EPOCH: u64 = 621_355_968_000_000_000;

struct BeatmapProcessedContext {
    db_beatmap: DbBeatmap,
    beatmap: Beatmap,
    is_main: bool,
}

struct HashRequestContext {
    beatmap_id: u32,
    beatmapset_id: u32,
    folder_name: String,
    file_name: String,

    beatmapset_info_id: i64,
    stripped_path: PathBuf,
    full_path: PathBuf,
}

struct HashProcessedContext {
    request: HashRequestContext,

    hash: String,
}

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

struct State {
    lazer_path: PathBuf,
    stable_path: PathBuf,
    lazer_db_path: PathBuf,
    stable_db_path: PathBuf,

    db_online_connection: Connection,
    progress_bars: ProgressBars,
    progress_styles: ProgressStyles,
}

impl State {
    fn new() -> Result<Self> {
        print!("You will be prompted to select the path to your osu lazer directory, press enter to continue");
        stdout().flush()?;
        wait_for_input()?;
        let lazer_path = FileDialog::new()
            .set_title("Select the path to your osu lazer directory")
            .set_directory(dirs::data_local_dir().unwrap_or_else(|| "/".into()))
            .pick_folder()
            .context("Failed to select the directory..?")?;

        let mut lazer_db_path = lazer_path.clone();
        lazer_db_path.push("client.db");
        if !lazer_db_path.exists() {
            return Err(anyhow!(
                "Not a valid osu lazer directory? (missing client.db)"
            ));
        };

        let mut lazer_online_db_path = lazer_path.clone();
        lazer_online_db_path.push("online.db");
        if !lazer_online_db_path.exists() {
            return Err(anyhow!(
                "Missing osu lazer online.db, try opening the game and then rerunning this tool?"
            ));
        };

        print!("You will be prompted to select the path to your osu stable directory, press enter to continue");
        stdout().flush()?;
        wait_for_input()?;
        let stable_path = FileDialog::new()
            .set_title("Select the path to your osu lazer directory")
            .set_directory(dirs::data_local_dir().unwrap_or_else(|| "/".into()))
            .pick_folder()
            .context("Failed to select the directory..?")?;

        let mut stable_db_path = stable_path.clone();
        stable_db_path.push("osu!.db");
        if !stable_db_path.exists() {
            return Err(anyhow!(
                "Not a valid osu stable directory? (missing osu!.db)"
            ));
        };

        #[cfg(target_family = "windows")]
        if let Err(_) = windows_link_check(&lazer_path, &stable_path) {
            return Err(anyhow!("Hard link test failed! On windows, both lazer and stable must be on the same disk for linking to work."));
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

struct BeatmapThread {
    bar: ProgressBar,
    insert_bar: ProgressBar,
    length_unchanging_style: ProgressStyle,
    stable_path: PathBuf,
}

impl BeatmapThread {
    fn new(state: &State) -> Self {
        Self {
            bar: state.progress_bars.beatmap.clone(),
            insert_bar: state.progress_bars.beatmap_insert.clone(),
            length_unchanging_style: state.progress_styles.length_unchanging.clone(),
            stable_path: state.stable_path.clone(),
        }
    }

    fn start(&self, beatmaps: Vec<DbBeatmap>, sender: Sender<BeatmapProcessedContext>) {
        let mut processed_sets: Vec<u32> = vec![];
        let beatmaps = beatmaps
            .into_iter()
            .map(|bm| -> (DbBeatmap, bool) {
                if processed_sets.contains(&bm.beatmap_set_id) {
                    (bm, false)
                } else {
                    processed_sets.push(bm.beatmap_set_id);
                    (bm, true)
                }
            })
            .collect_vec();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_cpus::get())
            .thread_name(|i| format!("(osu-link) beatmap thread {}", i))
            .build()
            .unwrap();
        pool.install(move || {
            beatmaps
                .par_iter()
                .for_each_with(sender, |sender, (db_beatmap, is_main)| {
                    self.bar.set_message(format!(
                        "{: <7} - {: <7}",
                        db_beatmap.beatmap_set_id, db_beatmap.beatmap_id
                    ));
                    self.bar.inc(1);
                    if let Err(e) = self.process(sender, db_beatmap, *is_main) {
                        self.bar.println(format!(
                            "Error occurred while processing {}/{}",
                            db_beatmap.folder_name, db_beatmap.beatmap_file_name
                        ));
                        self.bar.println(format!("{}", e));
                    }
                    self.insert_bar.inc_length(1);
                });
        });

        self.bar.finish_with_message("Done.");
        self.insert_bar
            .set_style(self.length_unchanging_style.clone());
    }

    fn process(
        &self,
        sender: &Sender<BeatmapProcessedContext>,
        db_beatmap: &DbBeatmap,
        is_main: bool,
    ) -> Result<()> {
        let mut path = self.stable_path.clone();
        path.push("Songs");
        path.push(&db_beatmap.folder_name);
        path.push(&db_beatmap.beatmap_file_name);

        let fd = File::open(path)?;
        let beatmap = Beatmap::parse(fd)?;
        sender.send(BeatmapProcessedContext {
            db_beatmap: db_beatmap.clone(),
            is_main,
            beatmap,
        })?;

        Ok(())
    }
}

struct HashThread {
    bar: ProgressBar,
    insert_bar: ProgressBar,
}

impl HashThread {
    fn new(state: &State) -> Self {
        Self {
            bar: state.progress_bars.hash.clone(),
            insert_bar: state.progress_bars.hash_insert.clone(),
        }
    }

    fn start(&self, sender: Sender<HashProcessedContext>, receiver: Receiver<HashRequestContext>) {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_cpus::get())
            .thread_name(|i| format!("(osu-link) hash thread {}", i))
            .build()
            .unwrap();
        pool.install(move || {
            receiver
                .into_iter()
                .par_bridge()
                .for_each_with(sender, |sender, request| {
                    self.bar.set_message(format!(
                        "{: <7} - {: <7}",
                        request.beatmapset_id, request.beatmap_id
                    ));
                    self.bar.inc(1);
                    match hash_file(&request.full_path) {
                        Ok(hash) => {
                            sender.send(HashProcessedContext { request, hash }).unwrap();
                            self.insert_bar.inc_length(1);
                        }
                        Err(e) => {
                            self.bar.println(format!(
                                "Error occurred while processing {}/{}",
                                request.folder_name, request.file_name
                            ));
                            self.bar.println(format!("{}", e));
                        }
                    }
                });
            self.bar.finish_with_message("Done.");
        });
    }
}

fn main() -> Result<()> {
    let state = State::new()?;

    let mut db_connection = Connection::open(&state.lazer_db_path)?;

    if !check_version(&db_connection)? {
        return Err(anyhow!("Database version mismatch! Please make sure you have the latest versions of both osu and osu-link"));
    }

    let (stable_len, lazer_len, beatmaps) = get_beatmaps(&state, &db_connection)?;

    println!("Stable path: {:?}", state.stable_path);
    println!("Lazer path: {:?}", state.lazer_path);
    println!("Stable beatmap count: {}", stable_len);
    println!("Lazer beatmap count: {}", lazer_len);
    print!("If this looks correct, press enter to continue and start");
    stdout().flush()?;
    wait_for_input()?;

    state.show_progress();

    let (bm_sx, bm_rx) = channel::<BeatmapProcessedContext>();
    let (hash_req_sx, hash_req_rx) = channel::<HashRequestContext>();
    let (hash_sx, hash_rx) = channel::<HashProcessedContext>();

    let b_ctx = BeatmapThread::new(&state);
    let beatmap_thread = spawn(move || {
        b_ctx.start(beatmaps, bm_sx);
    });
    let h_ctx = HashThread::new(&state);
    let hash_thread = spawn(move || {
        h_ctx.start(hash_sx, hash_req_rx);
    });

    let transaction = db_connection.transaction()?;

    insert_beatmaps(&state, &transaction, bm_rx, hash_req_sx)?;
    state
        .progress_bars
        .beatmap_insert
        .finish_with_message("Done.");

    state.progress_bars.hash_insert.disable_steady_tick();
    state
        .progress_bars
        .hash_insert
        .set_style(state.progress_styles.length_unchanging.clone());
    insert_hashes(&state, &transaction, hash_rx)?;
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

fn insert_beatmaps(
    state: &State,
    transaction: &Transaction,
    receiver: Receiver<BeatmapProcessedContext>,
    hash_sender: Sender<HashRequestContext>,
) -> Result<()> {
    for beatmap in receiver {
        state.progress_bars.beatmap_insert.set_message(format!(
            "{: <7} - {: <7}",
            beatmap.db_beatmap.beatmap_set_id, beatmap.db_beatmap.beatmap_id
        ));
        state.progress_bars.beatmap_insert.inc(1);

        let res = insert_beatmap(state, transaction, &beatmap);
        if let Err(err) = res {
            state.progress_bars.beatmap_insert.println(format!(
                "Error importing {}/{}",
                beatmap.db_beatmap.folder_name, beatmap.db_beatmap.beatmap_file_name
            ));
            state
                .progress_bars
                .beatmap_insert
                .println(format!("{}", err));
        } else {
            let res = res.unwrap();

            if !beatmap.is_main {
                continue;
            }

            let mut bms_path = state.stable_path.clone();
            bms_path.push("Songs");
            bms_path.push(&beatmap.db_beatmap.folder_name);

            for entry in WalkDir::new(&bms_path) {
                let entry = entry?;
                let path = entry.path();

                if !path.is_file() {
                    continue;
                }

                let clone = path.to_path_buf();
                let stripped_path = clone.strip_prefix(&bms_path)?;

                hash_sender
                    .send(HashRequestContext {
                        beatmap_id: beatmap.db_beatmap.beatmap_id,
                        beatmapset_id: beatmap.db_beatmap.beatmap_set_id,
                        folder_name: beatmap.db_beatmap.folder_name.clone(),
                        file_name: beatmap.db_beatmap.beatmap_file_name.clone(),
                        beatmapset_info_id: res,
                        full_path: path.to_path_buf(),
                        stripped_path: stripped_path.to_path_buf(),
                    })
                    .unwrap();

                state.progress_bars.hash.inc_length(1);
            }
        };
    }

    Ok(())
}

fn insert_hashes(
    state: &State,
    transaction: &Transaction,
    receiver: Receiver<HashProcessedContext>,
) -> Result<()> {
    for hash in receiver {
        state.progress_bars.hash_insert.set_message(format!(
            "{: <7} - {: <7}",
            hash.request.beatmapset_id, hash.request.beatmap_id
        ));

        transaction.execute(
            "INSERT OR IGNORE INTO FileInfo
                 (Hash, ReferenceCount)
             VALUES
                 (?, ?)",
            params![hash.hash, 0],
        )?;

        transaction.execute(
            "UPDATE FileInfo
             SET ReferenceCount = ReferenceCount + 1
             WHERE Hash = ?",
            params![hash.hash],
        )?;

        let file_id: i64 = transaction.query_row(
            "SELECT ID
             FROM FileInfo
             WHERE Hash = ?",
            params![hash.hash],
            |row| row.get(0),
        )?;

        transaction.execute(
            "INSERT INTO BeatmapSetFileInfo
                 (BeatmapSetInfoID, FileInfoID, Filename)
             VALUES
                 (?, ?, ?)",
            params![
                hash.request.beatmapset_info_id,
                file_id,
                hash.request.stripped_path.to_str().unwrap()
            ],
        )?;

        if hash.request.stripped_path.to_str().unwrap() == hash.request.file_name {
            transaction.execute(
                "UPDATE BeatmapInfo
                 SET Hash = ?
                 WHERE OnlineBeatmapID = ?",
                params![hash.hash, hash.request.beatmap_id],
            )?;
        }

        let mut path = state.lazer_path.clone();
        path.push("files");
        path.push(&hash.hash[..1]);
        path.push(&hash.hash[..2]);
        std::fs::create_dir_all(&path)?;
        path.push(&hash.hash);

        #[cfg(target_family = "unix")]
        {
            let read = std::fs::read_link(&path);
            if read.is_err() && !path.exists() {
                std::os::unix::fs::symlink(hash.request.full_path.clone(), path)?;
            }
        }
        #[cfg(target_family = "windows")]
        {
            if !path.exists() {
                std::fs::hard_link(hash.request.full_path.clone(), path)?;
            }
        }

        state.progress_bars.hash_insert.inc(1);
    }

    Ok(())
}

fn insert_beatmap(
    state: &State,
    transaction: &Transaction,
    beatmap_context: &BeatmapProcessedContext,
) -> Result<i64> {
    let difficulty_id = insert_beatmap_difficulty(transaction, &beatmap_context.beatmap)?;
    let metadata_id = insert_beatmap_metadata(
        transaction,
        &state.db_online_connection,
        &beatmap_context.beatmap,
    )?;
    let beatmapset_info_id = insert_beatmapset_info(
        transaction,
        &beatmap_context.db_beatmap,
        metadata_id,
        beatmap_context.is_main,
    )?;

    insert_beatmap_info(
        transaction,
        &beatmap_context.beatmap,
        &beatmap_context.db_beatmap,
        beatmapset_info_id,
        difficulty_id,
        metadata_id,
    )?;

    Ok(beatmapset_info_id)
}

fn insert_beatmap_difficulty(tx: &Transaction, beatmap: &Beatmap) -> Result<i64> {
    tx.execute(
        "INSERT INTO BeatmapDifficulty
             (ApproachRate,
              CircleSize,
              DrainRate,
              OverallDifficulty,
              SliderMultiplier,
              SliderTickRate)
         VALUES
             (?, ?, ?, ?, ?, ?)",
        params![
            beatmap.difficulty.approach_rate,
            beatmap.difficulty.circle_size,
            beatmap.difficulty.hp_drain_rate,
            beatmap.difficulty.overall_difficulty,
            beatmap.difficulty.slider_multiplier,
            beatmap.difficulty.slider_tick_rate,
        ]
    )?;

    Ok(tx.last_insert_rowid())
}

fn insert_beatmap_metadata(
    tx: &Transaction,
    online_db: &Connection,
    beatmap: &Beatmap,
) -> Result<i64> {
    let mapper_id: i64 = online_db
        .query_row(
            "SELECT user_id
             FROM osu_beatmaps
             WHERE beatmap_id = ?",
            [beatmap.beatmap_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let mut background: Option<String> = None;
    let mut video: Option<String> = None;

    for event in &beatmap.events {
        match event {
            Event::Background(bg) => {
                background = Some(bg.filename.clone());
                break;
            }
            Event::Video(vid) => {
                video = Some(vid.filename.clone());
                break;
            }
            _ => {
                continue;
            }
        }
    }

    let params = params![
        beatmap.artist,
        beatmap.artist_unicode,
        beatmap.audio_filename,
        beatmap.creator,
        background,
        beatmap.preview_time.0,
        beatmap.source,
        beatmap.tags.join(" "),
        beatmap.title,
        beatmap.title_unicode,
        video,
        mapper_id
    ];

    let res = tx.query_row(
        "SELECT ID
         FROM BeatmapMetadata
         WHERE Artist = ?
           AND ArtistUnicode = ?
           AND AudioFile = ?
           AND Author = ?
           AND BackgroundFile = ?
           AND PreviewTime = ?
           AND Source = ?
           AND Tags = ?
           AND Title = ?
           AND TitleUnicode = ?
           AND VideoFile = ?
           AND AuthorID = ?
         LIMIT 1",
        params,
        |row| row.get(0),
    );

    if let Ok(res) = res {
        Ok(res)
    } else {
        tx.execute(
            "INSERT INTO BeatmapMetadata
                 (Artist,
                  ArtistUnicode,
                  AudioFile,
                  Author,
                  BackgroundFile,
                  PreviewTime,
                  Source,
                  Tags,
                  Title,
                  TitleUnicode,
                  VideoFile,
                  AuthorID)
             VALUES
                 (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params
        )?;

        Ok(tx.last_insert_rowid())
    }
}

fn insert_beatmapset_info(
    tx: &Transaction,
    db_beatmap: &DbBeatmap,
    metadata_id: i64,
    force: bool,
) -> Result<i64> {
    let res = tx.query_row(
        "
        SELECT ID
        FROM BeatmapSetInfo
        WHERE OnlineBeatmapSetID = ?
        LIMIT 1
    ",
        [db_beatmap.beatmap_set_id],
        |row| row.get(0),
    );

    if res.is_err() || force {
        let mut random_hash: [u8; 32] = [0; 32];
        thread_rng().fill(&mut random_hash);

        let mut hash = String::with_capacity(2 * random_hash.len());
        for byte in random_hash {
            write!(hash, "{:02x}", byte)?;
        }

        tx.execute(
            "INSERT INTO BeatmapSetInfo
                (DeletePending,
                 Hash,
                 MetadataID,
                 OnlineBeatmapSetID,
                 Protected,
                 Status,
                 DateAdded)
             VALUES
                 (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT
                 (OnlineBeatmapSetID)
             DO UPDATE SET
                 DeletePending=excluded.DeletePending,
                 Hash=excluded.Hash,
                 MetadataID=excluded.MetadataID,
                 OnlineBeatmapSetID=excluded.OnlineBeatmapSetID,
                 Protected=excluded.Protected,
                 Status=excluded.Status,
                 DateAdded=excluded.DateAdded",
            params![
                false,
                hash,
                metadata_id,
                db_beatmap.beatmap_set_id,
                false,
                db_beatmap.ranked_status as i8 - 3,
                // TODO
                // the params macro supports datetimes, but i haven't checked if it would be
                // correct
                Utc.timestamp_nanos(
                    ((db_beatmap.modification_date - WIN_TO_UNIX_EPOCH) * 100).try_into()?
                )
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
                .replace("T", " "),
            ],
        )?;
    }

    if let Ok(res) = res {
        Ok(res)
    } else {
        Ok(tx.last_insert_rowid())
    }
}

fn insert_beatmap_info(
    tx: &Transaction,
    beatmap: &Beatmap,
    db_beatmap: &DbBeatmap,
    beatmapset_info_id: i64,
    difficulty_id: i64,
    metadata_id: i64,
) -> Result<()> {
    let mut bpm: f64 = 0.0;

    // HACK: should be average bpm i think
    for tp in &beatmap.timing_points {
        if let TimingPointKind::Uninherited(UninheritedTimingInfo { mpb, .. }) = tp.kind {
            bpm = 60_000.0 / mpb as f64;
            break;
        }
    }

    let star_rating: &Vec<(Mods, f64)>;

    match beatmap.mode {
        Mode::Osu => star_rating = &db_beatmap.std_star_rating,
        Mode::Taiko => star_rating = &db_beatmap.std_taiko_rating,
        Mode::Catch => star_rating = &db_beatmap.std_ctb_rating,
        Mode::Mania => star_rating = &db_beatmap.std_mania_rating,
    }

    let star_rating = star_rating
        .iter()
        .find(|t| t.0 == Mods::None)
        .map_or(0.0, |o| o.1);

    tx.execute(
        "INSERT INTO BeatmapInfo
             (AudioLeadIn,
              BaseDifficultyID,
              BeatDivisor,
              BeatmapSetInfoID,
              Countdown,
              DistanceSpacing,
              GridSize,
              Hidden,
              LetterboxInBreaks,
              MD5Hash,
              MetadataID,
              OnlineBeatmapID,
              Path,
              RulesetID,
              SpecialStyle,
              StackLeniency,
              StarDifficulty,
              StoredBookmarks,
              TimelineZoom,
              Version,
              WidescreenStoryboard,
              Status,
              BPM,
              Length,
              EpilepsyWarning,
              CountdownOffset,
              SamplesMatchPlaybackRate)
         VALUES
             (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            beatmap.audio_leadin.0,
            difficulty_id,
            beatmap.beat_divisor,
            beatmapset_info_id,
            beatmap.countdown,
            beatmap.distance_spacing,
            beatmap.grid_size,
            false,
            beatmap.letterbox_in_breaks,
            db_beatmap.hash,
            metadata_id,
            db_beatmap.beatmap_id,
            db_beatmap.beatmap_file_name,
            beatmap.mode as i8,
            // XXX: ???
            false,
            beatmap.stack_leniency,
            star_rating,
            beatmap.bookmarks.iter().join(","),
            beatmap.timeline_zoom,
            beatmap.difficulty_name,
            beatmap.widescreen_storyboard,
            db_beatmap.ranked_status as i8 - 3,
            bpm,
            db_beatmap.total_time.0,
            beatmap.epilepsy_warning,
            // XXX: ???
            false,
            // XXX: ???
            false
        ],
    )?;

    Ok(())
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

fn hash_file(path: &Path) -> Result<String> {
    if FAKE_HASH {
        let mut hash: [u8; 32] = [0; 32];
        thread_rng().fill(&mut hash);

        let mut ret = String::with_capacity(2 * hash.len());
        for byte in hash {
            write!(ret, "{:02x}", byte)?;
        }

        return Ok(ret);
    }

    let mut fd = File::open(path)?;
    let mut buf = vec![];
    fd.read_to_end(&mut buf)?;

    let mut hash = Sha256::new();
    hash.update(buf);
    let hash = hash.finalize();

    let mut ret = String::with_capacity(2 * hash.len());
    for byte in hash {
        write!(ret, "{:02x}", byte)?;
    }

    Ok(ret)
}

fn wait_for_input() -> Result<()> {
    let mut str = String::new();
    stdin().read_line(&mut str)?;
    Ok(())
}

#[cfg(target_family = "windows")]
fn windows_link_check(lazer_path: &Path, stable_path: &Path) -> Result<()> {
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
