//! Write machinery shared by the RAR4 and RAR5 pipelines: bounded-memory
//! writer adapters ([`engine`]), the stream accessor
//! ([`stream_mut`](stream::stream_mut)) and the format-neutral member entry
//! points in [`write_ops`].

pub(crate) mod engine;
pub(crate) mod stream;
pub(crate) mod write_ops;

pub(crate) use self::stream::stream_mut;
