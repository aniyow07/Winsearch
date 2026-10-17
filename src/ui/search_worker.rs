//! Runs queries on a background thread so typing never waits on SQLite.

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use winsearch::db::Db;
use winsearch::search::{Query, SearchEngine, SearchResults};

pub struct SearchRequest {
    pub id: u64,
    pub query: Query,
    pub limit: usize,
}

pub struct SearchResponse {
    pub id: u64,
    pub result: Result<SearchResults, String>,
}

pub struct SearchWorker {
    tx: Sender<SearchRequest>,
    pub rx: Receiver<SearchResponse>,
}

impl SearchWorker {
    pub fn start(db_path: PathBuf, notify: Arc<dyn Fn() + Send + Sync>) -> SearchWorker {
        let (tx, req_rx) = crossbeam_channel::unbounded::<SearchRequest>();
        let (resp_tx, rx) = crossbeam_channel::unbounded::<SearchResponse>();
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                let mut engine: Option<SearchEngine> = None;
                while let Ok(mut req) = req_rx.recv() {
                    // Only the newest query matters.
                    while let Ok(newer) = req_rx.try_recv() {
                        req = newer;
                    }
                    if engine.is_none() {
                        match Db::open_reader(&db_path) {
                            Ok(c) => engine = Some(SearchEngine::new(c)),
                            Err(e) => {
                                let _ = resp_tx.send(SearchResponse {
                                    id: req.id,
                                    result: Err(format!("The index is not available yet: {e}")),
                                });
                                notify();
                                continue;
                            }
                        }
                    }
                    let Some(eng) = engine.as_ref() else { continue };
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        eng.search(&req.query, req.limit).map_err(|e| format!("Search failed: {e:#}"))
                    }))
                    .unwrap_or_else(|_| Err("Search failed after an internal error (details are in the log).".into()));
                    if result.is_err() {
                        // Reopen next time (database may have been rebuilt).
                        engine = None;
                    }
                    let _ = resp_tx.send(SearchResponse { id: req.id, result });
                    notify();
                }
            })
            .expect("spawn search thread");
        SearchWorker { tx, rx }
    }

    pub fn submit(&self, req: SearchRequest) {
        let _ = self.tx.send(req);
    }
}
