//
// Copyright 2023 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::collections::HashSet;
use std::time::Duration;

use calling_common::RoomId;
use futures::{future::BoxFuture, FutureExt, TryFutureExt};
use hyper::{client::HttpConnector, Body, Client as HttpClient, Method, Request, StatusCode, Uri};
use log::*;
use serde::Serialize;
use tokio::{runtime::Handle, task::JoinHandle};

use super::UserId;

/// This is the timeout for persistence requests
const PERSISTENCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Used to throttle persistence requests
const MINIMUM_REQUEST_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FlatApprovedUsers<'a> {
    approved_users: &'a HashSet<UserId>,
}

#[cfg(test)]
type PersistenceCallback =
    fn(body: Vec<u8>) -> BoxFuture<'static, anyhow::Result<hyper::Response<Body>>>;

#[derive(Debug, Clone)]
enum PersistenceMode {
    Off,
    Uri(&'static hyper::Uri, RoomId),
    #[cfg(test)]
    Callback(PersistenceCallback),
}

impl From<Option<(&'static hyper::Uri, RoomId)>> for PersistenceMode {
    fn from(value: Option<(&'static hyper::Uri, RoomId)>) -> Self {
        match value {
            Some((uri, room_id)) => Self::Uri(uri, room_id),
            None => Self::Off,
        }
    }
}

pub struct ApprovedUsers {
    set: HashSet<UserId>,
    future: Option<JoinHandle<StatusCode>>,
    modified: bool,
    persistence_mode: PersistenceMode,
    retry_count: u8,
}

impl ApprovedUsers {
    pub fn new(
        users: impl IntoIterator<Item = UserId>,
        uri_and_room_id: Option<(&'static Uri, RoomId)>,
    ) -> Self {
        Self {
            set: HashSet::from_iter(users),
            future: None,
            modified: false,
            persistence_mode: uri_and_room_id.into(),
            retry_count: 0,
        }
    }

    pub fn contains(&self, value: &UserId) -> bool {
        self.set.contains(value)
    }
    pub fn insert(&mut self, value: UserId) -> bool {
        if self.set.insert(value) {
            self.modified();
            true
        } else {
            false
        }
    }
    pub fn remove(&mut self, value: &UserId) -> bool {
        if self.set.remove(value) {
            self.modified();
            true
        } else {
            false
        }
    }
    fn modified(&mut self) {
        match &self.future {
            Some(future) if !future.is_finished() => {
                self.modified = true;
            }
            _ => {
                self.retry_count = 0;
                self.spawn(None);
            }
        }
    }
    fn spawn(&mut self, wait: Option<Duration>) {
        if matches!(self.persistence_mode, PersistenceMode::Off) {
            return;
        }
        if Handle::try_current().is_err() {
            warn!("called outside of tokio runtime; can't persist updates");
            return;
        }
        debug!(
            "spawning future to persist approval list of {} users",
            self.set.len()
        );
        let persistence_mode = self.persistence_mode.clone();
        let body = serde_json::to_vec(&FlatApprovedUsers {
            approved_users: &self.set,
        })
        .unwrap();
        let time_to_start = wait.map(|interval| tokio::time::Instant::now() + interval);

        self.future = Some(tokio::spawn(async move {
            if let Some(time_to_start) = time_to_start {
                tokio::time::sleep_until(time_to_start).await;
            }
            let request: BoxFuture<_> = match persistence_mode {
                PersistenceMode::Off => {
                    unreachable!("checked above");
                }
                PersistenceMode::Uri(uri, room_id) => {
                    let client: HttpClient<HttpConnector> = HttpClient::builder().build_http();
                    let req = Request::builder()
                        .method(Method::PUT)
                        .uri(uri)
                        .header("X-Room-Id", room_id.as_ref())
                        .header("Content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap();
                    Box::pin(client.request(req).map_err(anyhow::Error::from))
                }
                #[cfg(test)]
                PersistenceMode::Callback(callback) => callback(body),
            };
            let timeout = tokio::time::sleep(PERSISTENCE_TIMEOUT);
            let minimum_time_taken = tokio::time::sleep(MINIMUM_REQUEST_INTERVAL);
            tokio::select!(
                _ = timeout => {
                    debug!("persisting approved users timed out locally");
                    StatusCode::REQUEST_TIMEOUT
                },
                response = request => match response {
                    Ok(r) => {
                        // Sleep an extra amount on top of however long the request took, so that we
                        // don't bother the frontend for this call again for at least that interval.
                        // This has the effect of coalescing updates if several users get approved
                        // in rapid succession.
                        minimum_time_taken.await;
                        r.status()
                    },
                    Err(err) => {
                        error!("failed to send request to persist approved users: {}", err);
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                }
            )
        }));
    }
    pub fn is_busy(&self) -> bool {
        self.future.is_some()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn tick(&mut self) {
        if let Some(future) = self.future.take() {
            if future.is_finished() {
                let needs_retry = match future.now_or_never() {
                    Some(status) => match status {
                        Ok(StatusCode::OK) => {
                            event!("calling.call.persist_approved_users.success");
                            false
                        }
                        Ok(other) => {
                            event!("calling.call.persist_approved_users.error");
                            // This will probably be logged on the frontend side too,
                            // but just in case.
                            error!("error persisting approved users: got {}", other);
                            true
                        }
                        Err(err) => {
                            error!("internal failure persisting approved users: {}", err);
                            // This implies that the background task was cancelled or panicked.
                            // We don't cancel that task, and if it panicked once it will probably
                            // panic again. So there's no point in retrying.
                            false
                        }
                    },
                    None => {
                        error!("tokio::JoinHandle reported finished, but now_or_never failed; this should never happen");
                        // This would be a bug in tokio; no point in retrying.
                        false
                    }
                };

                if self.modified {
                    self.modified = false;
                    self.retry_count = 0;
                    self.spawn(None);
                } else if needs_retry {
                    self.retry_count += 1;
                    if self.retry_count > 3 {
                        event!("calling.call.persist_approved_users.too_many_retries");
                    } else {
                        let mut wait: f64 = (1 << self.retry_count).into();
                        wait *= 1.0 + rand::random::<f64>();
                        self.spawn(Some(Duration::from_secs_f64(wait)));
                    }
                }
            } else {
                self.future = Some(future);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn happy_path() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new([], None);
        users.persistence_mode = PersistenceMode::Callback(|body| {
            CALLBACK_COUNT.fetch_add(1, SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let body_approved_users = body["approvedUsers"]
                .as_array()
                .expect("serialized as array");
            assert_eq!(
                vec!["user"],
                body_approved_users
                    .iter()
                    .map(|user| user.as_str().expect("each user ID is a string"))
                    .collect::<Vec<_>>(),
            );
            Box::pin(async { Ok(hyper::Response::builder().body(Body::empty())?) })
        });

        users.insert("user".to_string().into());
        assert!(users.is_busy());

        // yield_now is not *guaranteed* to run the spawned persistence task,
        // but in practice it will for the single-threaded tokio runtime.
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        users.tick();
        assert!(users.is_busy(), "minimum interval not respected");

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(!users.is_busy());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new([], None);

        users.persistence_mode = PersistenceMode::Callback(|_body| {
            CALLBACK_COUNT.fetch_add(1, SeqCst);
            Box::pin(futures::future::pending())
        });

        users.insert("user".to_string().into());
        assert!(users.is_busy());

        // yield_now is not *guaranteed* to run the spawned persistence task,
        // but in practice it will for the single-threaded tokio runtime.
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        users.tick();
        assert!(users.is_busy(), "minimum interval not respected");

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());

        tokio::time::advance(PERSISTENCE_TIMEOUT).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());
        assert_eq!(users.retry_count, 1);
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        // First backoff: 2..<4 seconds.
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 2);

        tokio::time::advance(PERSISTENCE_TIMEOUT).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());
        assert_eq!(users.retry_count, 2);
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 2);

        // Second backoff: 4..<8 seconds.
        tokio::time::advance(Duration::from_secs(8)).await;
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 3);

        tokio::time::advance(PERSISTENCE_TIMEOUT).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());
        assert_eq!(users.retry_count, 3);
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 3);

        // Third backoff: 8..<16 seconds.
        tokio::time::advance(Duration::from_secs(16)).await;
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 4);

        tokio::time::advance(PERSISTENCE_TIMEOUT).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(!users.is_busy());
        assert_eq!(users.retry_count, 4);
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 4);
        // We gave up.

        tokio::time::advance(PERSISTENCE_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_on_failure() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new([], None);

        users.persistence_mode = PersistenceMode::Callback(|_body| {
            let round = CALLBACK_COUNT.fetch_add(1, SeqCst);
            Box::pin(async move {
                Ok(hyper::Response::builder()
                    .status(if round == 0 {
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        StatusCode::OK
                    })
                    .body(Body::empty())?)
            })
        });

        users.insert("user".to_string().into());
        assert!(users.is_busy());

        // yield_now is not *guaranteed* to run the spawned persistence task,
        // but in practice it will for the single-threaded tokio runtime.
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        users.tick();
        assert!(users.is_busy(), "minimum interval not respected");

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());
        assert_eq!(users.retry_count, 1);
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        // First backoff: 2..<4 seconds.
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 2);
        assert!(users.is_busy());

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(!users.is_busy());
    }

    #[tokio::test(start_paused = true)]
    async fn adding_to_existing_set_persists_all_users() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new(["A".to_string().into(), "B".to_string().into()], None);
        users.persistence_mode = PersistenceMode::Callback(|body| {
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let body_approved_users = body["approvedUsers"]
                .as_array()
                .expect("serialized as array");
            assert_eq!(
                HashSet::from_iter(["A", "B", "C"]),
                body_approved_users
                    .iter()
                    .map(|user| user.as_str().expect("each user ID is a string"))
                    .collect::<HashSet<_>>(),
            );
            CALLBACK_COUNT.fetch_add(1, SeqCst);
            Box::pin(async { Ok(hyper::Response::builder().body(Body::empty())?) })
        });

        users.insert("C".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // Make sure the callback was invoked so our assertions get checked.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn removing_from_existing_set_persists_remaining_users() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new(
            [
                "A".to_string().into(),
                "B".to_string().into(),
                "C".to_string().into(),
            ],
            None,
        );

        users.persistence_mode = PersistenceMode::Callback(|body| {
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let body_approved_users = body["approvedUsers"]
                .as_array()
                .expect("serialized as array");
            assert_eq!(
                HashSet::from_iter(["A", "C"]),
                body_approved_users
                    .iter()
                    .map(|user| user.as_str().expect("each user ID is a string"))
                    .collect::<HashSet<_>>(),
            );
            CALLBACK_COUNT.fetch_add(1, SeqCst);
            Box::pin(async { Ok(hyper::Response::builder().body(Body::empty())?) })
        });

        users.remove(&"B".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // Make sure the callback was invoked so our assertions get checked.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn add_during_persist() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new(["A".to_string().into(), "B".to_string().into()], None);
        users.persistence_mode = PersistenceMode::Callback(|body| {
            let round = CALLBACK_COUNT.fetch_add(1, SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let body_approved_users = body["approvedUsers"]
                .as_array()
                .expect("serialized as array");
            assert_eq!(
                if round == 0 {
                    HashSet::from_iter(["A", "B", "C"])
                } else {
                    HashSet::from_iter(["A", "B", "C", "D"])
                },
                body_approved_users
                    .iter()
                    .map(|user| user.as_str().expect("each user ID is a string"))
                    .collect::<HashSet<_>>(),
            );
            Box::pin(async { Ok(hyper::Response::builder().body(Body::empty())?) })
        });

        users.insert("C".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // Make sure the callback was invoked so our assertions get checked.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        users.insert("D".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // We shouldn't have spawned another callback yet; we have our minimum timeout.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 2);

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(!users.is_busy());
    }

    #[tokio::test(start_paused = true)]
    async fn remove_during_persist() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new(["A".to_string().into(), "B".to_string().into()], None);
        users.persistence_mode = PersistenceMode::Callback(|body| {
            let round = CALLBACK_COUNT.fetch_add(1, SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let body_approved_users = body["approvedUsers"]
                .as_array()
                .expect("serialized as array");
            assert_eq!(
                if round == 0 {
                    HashSet::from_iter(["A", "B", "C"])
                } else {
                    HashSet::from_iter(["A", "C"])
                },
                body_approved_users
                    .iter()
                    .map(|user| user.as_str().expect("each user ID is a string"))
                    .collect::<HashSet<_>>(),
            );
            Box::pin(async { Ok(hyper::Response::builder().body(Body::empty())?) })
        });

        users.insert("C".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // Make sure the callback was invoked so our assertions get checked.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        users.remove(&"B".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // We shouldn't have spawned another callback yet; we have our minimum timeout.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 2);

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(!users.is_busy());
    }

    #[tokio::test(start_paused = true)]
    async fn add_and_remove_during_persist() {
        // We use a static here so that the callback can avoid capturing state.
        static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);
        CALLBACK_COUNT.store(0, SeqCst);

        let mut users = ApprovedUsers::new(["A".to_string().into(), "B".to_string().into()], None);
        users.persistence_mode = PersistenceMode::Callback(|body| {
            let round = CALLBACK_COUNT.fetch_add(1, SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let body_approved_users = body["approvedUsers"]
                .as_array()
                .expect("serialized as array");
            assert_eq!(
                if round == 0 {
                    HashSet::from_iter(["A", "B", "C"])
                } else {
                    HashSet::from_iter(["A", "C", "D"])
                },
                body_approved_users
                    .iter()
                    .map(|user| user.as_str().expect("each user ID is a string"))
                    .collect::<HashSet<_>>(),
            );
            Box::pin(async { Ok(hyper::Response::builder().body(Body::empty())?) })
        });

        users.insert("C".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // Make sure the callback was invoked so our assertions get checked.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        users.insert("D".to_string().into());
        users.remove(&"B".to_string().into());
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        // We shouldn't have spawned another callback yet; we have our minimum timeout.
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 1);

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(users.is_busy());
        tokio::task::yield_now().await;
        assert_eq!(CALLBACK_COUNT.load(SeqCst), 2);

        tokio::time::advance(MINIMUM_REQUEST_INTERVAL).await;
        tokio::task::yield_now().await;
        users.tick();
        assert!(!users.is_busy());
    }

    #[tokio::test(start_paused = true)]
    async fn redundant_add_is_ignored() {
        let mut users = ApprovedUsers::new(["A".to_string().into(), "B".to_string().into()], None);
        users.persistence_mode = PersistenceMode::Callback(|_body| {
            panic!("should not be called");
        });

        users.insert("B".to_string().into());
        assert!(!users.is_busy());
    }

    #[tokio::test(start_paused = true)]
    async fn redundant_remove_is_ignored() {
        let mut users = ApprovedUsers::new(["A".to_string().into(), "B".to_string().into()], None);
        users.persistence_mode = PersistenceMode::Callback(|_body| {
            panic!("should not be called");
        });

        users.remove(&"C".to_string().into());
        assert!(!users.is_busy());
    }
}
