/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use malloc_size_of_derive::MallocSizeOf;
use uuid::Uuid;

use super::{MediaStream, MediaStreamSettings};

type RegisteredMediaStream = Arc<Mutex<dyn MediaStream>>;

static MEDIA_STREAMS_REGISTRY: LazyLock<Mutex<HashMap<MediaStreamId, RegisteredMediaStream>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Hash, Eq, PartialEq, MallocSizeOf)]
pub struct MediaStreamId(Uuid);
impl MediaStreamId {
    pub fn new() -> MediaStreamId {
        Self(Uuid::new_v4())
    }

    pub fn id(self) -> Uuid {
        self.0
    }
}

impl Default for MediaStreamId {
    fn default() -> Self {
        Self::new()
    }
}

pub fn register_stream(stream: Arc<Mutex<dyn MediaStream>>) -> MediaStreamId {
    let id = MediaStreamId::new();
    stream.lock().unwrap().set_id(id);
    MEDIA_STREAMS_REGISTRY.lock().unwrap().insert(id, stream);
    id
}

pub fn unregister_stream(stream: &MediaStreamId) {
    // A stream's destructor may unregister itself. Drop the removed value only
    // after releasing the registry lock so that cleanup can re-enter safely.
    let removed = MEDIA_STREAMS_REGISTRY.lock().unwrap().remove(stream);
    drop(removed);
}

pub fn get_stream(stream: &MediaStreamId) -> Option<Arc<Mutex<dyn MediaStream>>> {
    MEDIA_STREAMS_REGISTRY.lock().unwrap().get(stream).cloned()
}

pub fn set_stream_enabled(stream: &MediaStreamId, enabled: bool) -> Result<(), String> {
    let stream = get_stream(stream).ok_or_else(|| String::from("メディアストリームが無い"))?;
    stream.lock().unwrap().set_enabled(enabled)
}

pub fn clone_stream(stream: &MediaStreamId) -> Result<MediaStreamId, String> {
    let stream = get_stream(stream).ok_or_else(|| String::from("メディアストリームが無い"))?;
    stream.lock().unwrap().clone_stream()
}

pub fn stop_stream(stream: &MediaStreamId) -> Result<(), String> {
    let stream = get_stream(stream).ok_or_else(|| String::from("メディアストリームが無い"))?;
    stream.lock().unwrap().stop()
}

pub fn stream_settings(stream: &MediaStreamId) -> Option<MediaStreamSettings> {
    get_stream(stream).map(|stream| stream.lock().unwrap().settings())
}
