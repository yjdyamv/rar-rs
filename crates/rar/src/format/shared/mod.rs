//! Write machinery shared by the RAR4 and RAR5 pipelines: bounded-memory
//! writer adapters ([`engine`]), the stream accessors
//! ([`stream_mut`](stream::stream_mut), [`stream_len`](stream::stream_len),
//! [`seek_past_data_area`](stream::seek_past_data_area)) and the
//! format-neutral member entry points in [`write_ops`].

pub(crate) mod engine;
pub(crate) mod stream;
pub(crate) mod write_ops;

pub(crate) use self::stream::{seek_past_data_area, stream_len, stream_mut};
