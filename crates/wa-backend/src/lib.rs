//! App-facing WhatsApp backend: wraps `whatsapp-rust` and owns the local chat/message store.
//!
//! The UI talks to the backend only through [`BackendHandle`]: it sends [`Command`]s and
//! receives [`Event`]s. The backend runs its own Tokio runtime on a background thread,
//! so the GPUI app never touches Tokio directly.

mod audio;
mod backend;
mod extract;
mod media;
mod media_retry;
mod profile;
mod stickers;
mod store;
mod types;

pub use backend::{new_message_id, spawn};
pub use types::*;
