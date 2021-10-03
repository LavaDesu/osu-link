use anyhow::Result;
use chrono::{TimeZone, Utc};
use itertools::Itertools;
use libosu::{
    beatmap::Beatmap,
    db::DbBeatmap,
    events::Event,
    prelude::{Mode, Mods},
    timing::{TimingPointKind, UninheritedTimingInfo},
};
use rand::{thread_rng, Rng};
use rusqlite::{params, Connection, Transaction};
use std::{
    convert::TryInto,
    fmt::Write as FmtWrite,
    sync::mpsc::{Receiver, Sender},
};
use walkdir::WalkDir;

use crate::processors::context::{BeatmapProcessed, HashProcessed, HashRequest};
use crate::{State, WIN_TO_UNIX_EPOCH};

pub fn insert_beatmaps(
    state: &State,
    transaction: &Transaction,
    receiver: Receiver<BeatmapProcessed>,
    hash_sender: Sender<HashRequest>,
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
                    .send(HashRequest {
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

pub fn insert_hashes(
    state: &State,
    transaction: &Transaction,
    receiver: Receiver<HashProcessed>,
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

pub fn insert_beatmap(
    state: &State,
    transaction: &Transaction,
    beatmap_context: &BeatmapProcessed,
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

pub fn insert_beatmap_difficulty(tx: &Transaction, beatmap: &Beatmap) -> Result<i64> {
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
        ],
    )?;

    Ok(tx.last_insert_rowid())
}

pub fn insert_beatmap_metadata(
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
            params,
        )?;

        Ok(tx.last_insert_rowid())
    }
}

pub fn insert_beatmapset_info(
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

pub fn insert_beatmap_info(
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
