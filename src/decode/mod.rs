//! 音频解码模块

pub mod decoder;

pub use decoder::{AudioDecoder, AudioInfo, DecodeError, DecoderIterator};
