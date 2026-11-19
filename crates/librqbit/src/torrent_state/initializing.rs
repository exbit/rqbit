use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::Context;

use bitvec::{order::Msb0, slice::BitSlice};
use rand::{RngExt, seq::IteratorRandom};
use size_format::SizeFormatterBinary as SF;
use tracing::{info, trace, warn};

use crate::{
    api::TorrentIdOrHash,
    bitv::BitV,
    bitv_factory::BitVFactory,
    chunk_tracker::{ChunkTracker, compute_selected_pieces},
    file_ops::FileOps,
    type_aliases::{BF, FileStorage, PeerStream},
};

const MAX_FASTRESUME_CHECKS: usize = 64;

use super::{ManagedTorrentShared, TorrentMetadata, paused::TorrentStatePaused};

/// Outcome of validating the fastresume bitfield against the files on disk.
enum FastresumeCheck {
    /// Every sampled piece could be read.
    Valid,
    /// Some files are missing or truncated on disk: only their pieces were dropped from
    /// the bitfield, and the selected ones will be downloaded again.
    FilesDropped { files: usize, pieces: usize },
    /// A read failed for another reason: the bitfield is not trusted at all.
    Broken,
}

pub struct TorrentStateInitializing {
    pub(crate) files: FileStorage,
    pub(crate) shared: Arc<ManagedTorrentShared>,
    pub(crate) metadata: Arc<TorrentMetadata>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
    pause_requested: AtomicBool,
    check_running: AtomicBool,
    // Peer stream of a start request that came while the check was running.
    pending_peer_rx: parking_lot::Mutex<Option<PeerStream>>,
    previously_errored: bool,
}

impl TorrentStateInitializing {
    pub fn new(
        shared: Arc<ManagedTorrentShared>,
        metadata: Arc<TorrentMetadata>,
        only_files: Option<Vec<usize>>,
        files: FileStorage,
        previously_errored: bool,
    ) -> Self {
        Self {
            shared,
            metadata,
            only_files,
            files,
            checked_bytes: AtomicU64::new(0),
            pause_requested: AtomicBool::new(false),
            check_running: AtomicBool::new(false),
            pending_peer_rx: parking_lot::Mutex::new(None),
            previously_errored,
        }
    }

    pub fn get_checked_bytes(&self) -> u64 {
        self.checked_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn request_pause(&self) {
        self.pause_requested.store(true, Ordering::Relaxed);
    }

    pub(crate) fn clear_pause_request(&self) {
        self.pause_requested.store(false, Ordering::Relaxed);
    }

    pub(crate) fn is_pause_requested(&self) -> bool {
        self.pause_requested.load(Ordering::Relaxed)
    }

    pub(crate) fn try_start_check(&self) -> bool {
        self.check_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn finish_check(&self) {
        self.check_running.store(false, Ordering::Release);
    }

    /// Keeps the peer stream of a start request for the running check: the check
    /// task starts the torrent with it once the check is done.
    pub(crate) fn set_pending_peer_rx(&self, peer_rx: Option<PeerStream>) {
        *self.pending_peer_rx.lock() = peer_rx;
    }

    pub(crate) fn take_pending_peer_rx(&self) -> Option<PeerStream> {
        self.pending_peer_rx.lock().take()
    }

    async fn validate_fastresume(
        &self,
        bitv_factory: &dyn BitVFactory,
        have_pieces: Option<Box<dyn BitV>>,
    ) -> Option<Box<dyn BitV>> {
        let mut hp = have_pieces?;
        let actual = hp.as_bytes().len();
        let expected = self.metadata.lengths().piece_bitfield_bytes();
        if actual != expected {
            warn!(
                actual,
                expected,
                "the bitfield loaded isn't of correct length, ignoring it, will do full check"
            );
            return None;
        }

        let outcome = self
            .shared
            .spawner
            .block_in_place_with_semaphore(|| {
                let fo = crate::file_ops::FileOps::new(
                    &self.metadata.info,
                    &self.files,
                    &self.metadata.file_infos,
                );
                let to_validate = self.pieces_to_validate(hp.as_slice());
                self.check_claimed_pieces(&fo, hp.as_slice_mut(), &to_validate)
            })
            .await;

        match outcome {
            FastresumeCheck::Valid => Some(hp),
            FastresumeCheck::FilesDropped { files, pieces } => {
                warn!(
                    id = ?self.shared.id,
                    info_hash = ?self.shared.info_hash,
                    files,
                    pieces,
                    "files missing or truncated on disk, dropped only their pieces"
                );
                if let Err(e) = hp.flush(false) {
                    warn!(id=?self.shared.id, info_hash = ?self.shared.info_hash, "error flushing bitfield: {e:#}");
                }
                Some(hp)
            }
            FastresumeCheck::Broken => {
                warn!(
                    id = ?self.shared.id,
                    info_hash = ?self.shared.info_hash,
                    "data corrupted, ignoring fastresume data"
                );
                if let Err(e) = bitv_factory.clear(self.shared.id.into()).await {
                    warn!(id=?self.shared.id, info_hash = ?self.shared.info_hash, "error clearing bitfield: {e:#}");
                }
                self.checked_bytes.store(0, Ordering::Relaxed);
                None
            }
        }
    }

    /// Pieces to validate: at least one claimed piece of each file, plus claimed pieces
    /// sampled with decreasing probability.
    fn pieces_to_validate(&self, have: &BitSlice<u8, Msb0>) -> BF {
        let mut to_validate = BF::from_boxed_slice(
            vec![0u8; self.metadata.lengths().piece_bitfield_bytes()].into_boxed_slice(),
        );
        let mut queue = have.to_owned();

        // Validate at least one piece from each file, if we claim we have it.
        for fi in self.metadata.file_infos.iter() {
            let prange = fi.piece_range_usize();
            let offset = prange.start;
            for piece_id in have
                .get(fi.piece_range_usize())
                .into_iter()
                .flat_map(|s| s.iter_ones())
                .map(|pid| pid + offset)
                .take(1)
            {
                to_validate.set(piece_id, true);
                queue.set(piece_id, false);
            }
        }

        // For all the remaining pieces we claim we have, validate them with decreasing probability.
        let queue = queue
            .iter_ones()
            .sample(&mut rand::rng(), MAX_FASTRESUME_CHECKS);

        for (tmp_id, piece_id) in queue.into_iter().enumerate() {
            let denom: u32 = (tmp_id + 1).min(50).try_into().unwrap();
            if rand::rng().random_ratio(1, denom) {
                to_validate.set(piece_id, true);
            }
        }

        to_validate
    }

    /// Reads the sampled pieces. A read error caused by a file that is missing or
    /// truncated on disk (e.g. a game client deleted it) drops only that file's pieces
    /// from `have`: the selected ones are downloaded again instead of re-checking the
    /// whole torrent. A hash mismatch is ignored, as before.
    fn check_claimed_pieces(
        &self,
        fo: &FileOps<'_>,
        have: &mut BitSlice<u8, Msb0>,
        to_validate: &BF,
    ) -> FastresumeCheck {
        let lengths = self.metadata.lengths();
        let to_validate_count = to_validate.count_ones();
        let (mut files, mut pieces) = (0, 0);

        for (id, piece_id) in to_validate
            .iter_ones()
            .filter_map(|id| lengths.validate_piece_index(id.try_into().ok()?))
            .enumerate()
        {
            let index = piece_id.get() as usize;

            // Pieces of an already dropped file are no longer claimed.
            if have[index] && fo.check_piece(piece_id).is_err() {
                let Some((dropped_files, dropped_pieces)) =
                    self.drop_missing_files(fo, have, index)
                else {
                    return FastresumeCheck::Broken;
                };
                files += dropped_files;
                pieces += dropped_pieces;
            }

            #[allow(clippy::cast_possible_truncation)]
            let progress = (lengths.total_length() as f64 / to_validate_count as f64
                * (id + 1) as f64) as u64;
            let progress = progress.min(lengths.total_length());
            self.checked_bytes.store(progress, Ordering::Relaxed);
        }

        if files == 0 {
            FastresumeCheck::Valid
        } else {
            FastresumeCheck::FilesDropped { files, pieces }
        }
    }

    /// Drops the claimed pieces of the missing or truncated files that `piece` touches.
    /// Returns the number of dropped files and pieces; `None` if all of them are in
    /// place, so the read failed for another reason.
    fn drop_missing_files(
        &self,
        fo: &FileOps<'_>,
        have: &mut BitSlice<u8, Msb0>,
        piece: usize,
    ) -> Option<(usize, usize)> {
        let (mut files, mut pieces) = (0, 0);

        for (file_id, fi) in self.metadata.file_infos.iter().enumerate() {
            let range = fi.piece_range_usize();

            if !range.contains(&piece) || !fo.is_file_missing_or_truncated(file_id) {
                continue;
            }

            pieces += have[range.clone()].count_ones();
            have[range].fill(false);
            files += 1;
        }

        (files > 0).then_some((files, pieces))
    }

    pub async fn check(&self) -> anyhow::Result<TorrentStatePaused> {
        let id: TorrentIdOrHash = self.shared.info_hash.into();
        let bitv_factory = self
            .shared
            .session
            .upgrade()
            .context("session is dead")?
            .bitv_factory
            .clone();
        let have_pieces = if self.previously_errored {
            if let Err(e) = bitv_factory.clear(id).await {
                warn!(id=?self.shared.id, info_hash = ?self.shared.info_hash, error=?e, "error clearing bitfield");
            }
            None
        } else {
            bitv_factory
                .load(id)
                .await
                .context("error loading have_pieces")?
        };

        let have_pieces = self.validate_fastresume(&*bitv_factory, have_pieces).await;

        let have_pieces = match have_pieces {
            Some(h) => h,
            None => {
                info!("Doing initial checksum validation, this might take a while...");
                let have_pieces = self
                    .shared
                    .spawner
                    .block_in_place_with_semaphore(|| {
                        FileOps::new(&self.metadata.info, &self.files, &self.metadata.file_infos)
                            .initial_check(&self.checked_bytes, &self.pause_requested)
                    })
                    .await?;
                bitv_factory
                    .store_initial_check(id, have_pieces)
                    .await
                    .context("error storing initial check bitfield")?
            }
        };

        let selected_pieces = compute_selected_pieces(
            self.metadata.lengths(),
            |idx| {
                self.only_files
                    .as_ref()
                    .map(|o| o.contains(&idx))
                    .unwrap_or(true)
            },
            &self.metadata.file_infos,
        );

        let chunk_tracker = ChunkTracker::new(
            have_pieces.into_dyn(),
            selected_pieces,
            *self.metadata.lengths(),
            &self.metadata.file_infos,
        )
        .context("error creating chunk tracker")?;

        let hns = chunk_tracker.get_hns();

        info!(
            torrent=?self.shared.id,
            "Initial check results: have {}, needed {}, total selected {}",
            SF::new(hns.have_bytes),
            SF::new(hns.needed_bytes),
            SF::new(hns.selected_bytes)
        );

        // Ensure file lengths are correct, and reopen completed files read-only.
        self.shared
            .spawner
            .block_in_place_with_semaphore(|| {
                for (idx, fi) in self.metadata.file_infos.iter().enumerate() {
                    if self
                        .only_files
                        .as_ref()
                        .map(|v| v.contains(&idx))
                        .unwrap_or(true)
                    {
                        let now = Instant::now();
                        if fi.attrs.padding {
                            continue;
                        }
                        if let Err(err) = self.files.ensure_file_length(idx, fi.len) {
                            warn!(
                                id=?self.shared.id, info_hash = ?self.shared.info_hash,
                                "Error setting length for file {:?} to {}: {:#?}",
                                fi.relative_filename, fi.len, err
                            );
                        } else {
                            trace!(
                                "Set length for file {:?} to {} in {:?}",
                                fi.relative_filename,
                                SF::new(fi.len),
                                now.elapsed()
                            );
                        }
                    }
                }

                for (idx, fi) in self.metadata.file_infos.iter().enumerate() {
                    if chunk_tracker.is_file_finished(fi)
                        && let Err(err) = self.files.on_file_completed(idx)
                    {
                        warn!(
                            id=?self.shared.id, info_hash = ?self.shared.info_hash,
                            "Error reopening completed file {:?} read-only: {:#?}",
                            fi.relative_filename, err
                        );
                    }
                }

                Ok::<_, anyhow::Error>(())
            })
            .await?;

        let paused = TorrentStatePaused {
            shared: self.shared.clone(),
            metadata: self.metadata.clone(),
            files: self.files.take()?,
            chunk_tracker,
            streams: Arc::new(Default::default()),
        };
        Ok(paused)
    }
}
