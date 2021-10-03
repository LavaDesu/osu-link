use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use itertools::Itertools;
use libosu::{beatmap::Beatmap, db::DbBeatmap};
use rand::{thread_rng, Rng};
use rayon::iter::{IntoParallelRefIterator, ParallelBridge, ParallelIterator};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
};

use crate::{State, FAKE_HASH};

pub mod context {
    use std::path::PathBuf;

    use libosu::{beatmap::Beatmap, db::DbBeatmap};

    pub struct BeatmapProcessed {
        pub db_beatmap: DbBeatmap,
        pub beatmap: Beatmap,
        pub is_main: bool,
    }

    pub struct HashRequest {
        pub beatmap_id: u32,
        pub beatmapset_id: u32,
        pub folder_name: String,
        pub file_name: String,

        pub beatmapset_info_id: i64,
        pub stripped_path: PathBuf,
        pub full_path: PathBuf,
    }

    pub struct HashProcessed {
        pub request: HashRequest,

        pub hash: String,
    }
}

use context::{BeatmapProcessed, HashProcessed, HashRequest};

pub struct BeatmapProcessor {
    bar: ProgressBar,
    insert_bar: ProgressBar,
    length_unchanging_style: ProgressStyle,
    stable_songs_path: PathBuf,
}

impl BeatmapProcessor {
    pub fn new(state: &State) -> Self {
        Self {
            bar: state.progress_bars.beatmap.clone(),
            insert_bar: state.progress_bars.beatmap_insert.clone(),
            length_unchanging_style: state.progress_styles.length_unchanging.clone(),
            stable_songs_path: state.stable_songs_path.clone(),
        }
    }

    pub fn start(self, beatmaps: Vec<DbBeatmap>, sender: Sender<BeatmapProcessed>) {
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

            self.bar.finish_with_message("Done.");
            self.insert_bar
                .set_style(self.length_unchanging_style.clone());
        });
    }

    fn process(
        &self,
        sender: &Sender<BeatmapProcessed>,
        db_beatmap: &DbBeatmap,
        is_main: bool,
    ) -> Result<()> {
        let mut path = self.stable_songs_path.clone();
        path.push(&db_beatmap.folder_name);
        path.push(&db_beatmap.beatmap_file_name);

        let fd = File::open(path)?;
        let beatmap = Beatmap::parse(fd)?;
        sender.send(BeatmapProcessed {
            db_beatmap: db_beatmap.clone(),
            is_main,
            beatmap,
        })?;

        Ok(())
    }
}

pub struct HashProcessor {
    bar: ProgressBar,
    insert_bar: ProgressBar,
}

impl HashProcessor {
    pub fn new(state: &State) -> Self {
        Self {
            bar: state.progress_bars.hash.clone(),
            insert_bar: state.progress_bars.hash_insert.clone(),
        }
    }

    pub fn start(self, sender: Sender<HashProcessed>, receiver: Receiver<HashRequest>) {
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
                    match HashProcessor::hash_file(&request.full_path) {
                        Ok(hash) => {
                            sender.send(HashProcessed { request, hash }).unwrap();
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
}
