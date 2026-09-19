//! Machinery shared by the family pipelines: bounded-memory writer adapters
//! ([`engine`]), the stream accessors
//! ([`stream_mut`](stream::stream_mut), [`stream_len`](stream::stream_len),
//! [`seek_past_data_area`](stream::seek_past_data_area)), the format-neutral
//! member entry points in [`write_ops`], the cross-volume split merge
//! ([`split`]), the shared legacy [`checksum`] and the family-neutral read
//! orchestration in [`extract`].

pub(crate) mod checksum;
pub(crate) mod engine;
pub(crate) mod entry_ext;
pub(crate) mod extract;
pub(crate) mod legacy_time;
pub(crate) mod split;
pub(crate) mod stream;
pub(crate) mod write_ops;

pub(crate) use self::stream::{seek_past_data_area, stream_len, stream_mut};
