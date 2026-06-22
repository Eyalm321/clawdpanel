//! Unsupported-platform backend (Go `audio_stub.go`): `new` → `ErrUnsupported`.

use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::event::Event;
use crate::player::Player;

pub fn new_player(_events: mpsc::Sender<Event>) -> Result<Box<dyn Player>> {
    Err(Error::unsupported())
}
