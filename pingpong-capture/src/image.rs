//! A picture captured on Linux: 4 bytes a pixel, shared by the capture and
//! the encoder until both are done with it (the capture reuses its buffers
//! only once no frame holds them).

use std::sync::Arc;

/// Memory holding a picture.
pub trait Pixels: Send + Sync {
    fn bytes(&self) -> &[u8];
}

impl Pixels for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

/// Which way round the colours are in memory (the fourth byte is unused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelOrder {
    Bgrx,
    Rgbx,
}

#[derive(Clone)]
pub struct Frame {
    pub(crate) pixels: Arc<dyn Pixels>,
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub stride: usize,
    pub order: PixelOrder,
}

impl Frame {
    pub fn bytes(&self) -> &[u8] {
        self.pixels.bytes()
    }
}
