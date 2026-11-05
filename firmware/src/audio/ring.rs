//! A DMA receive ring whose descriptors form a circle.
//!
//! esp-hal's `DmaRxStreamBuf` keeps a list that ends in a null link and
//! moves each read descriptor to its end. The DMA prefetches the next
//! descriptor, so a reader more than about one chunk late lets it load the
//! end of the list before the link is there: it then stops with free room
//! left (esp-hal issue 2269). Here the links never change; the DMA runs
//! for good, and a reader a whole ring late loses data instead
//! ([`RingView::overrun`]).

use esp_hal::dma::{BurstConfig, DmaDescriptor, DmaRxBuffer, Owner, Preparation};

pub struct Ring {
    descriptors: &'static mut [DmaDescriptor],
    buffer: &'static mut [u8],
}

impl Ring {
    /// `buffer` split evenly over `descriptors` (at least 2, chunks of at
    /// most 4092 bytes, a multiple of 4).
    pub fn new(descriptors: &'static mut [DmaDescriptor], buffer: &'static mut [u8]) -> Option<Self> {
        let n = descriptors.len();
        let chunk = buffer.len().checked_div(n)?;
        if n < 2 || chunk == 0 || chunk > 4092 || chunk % 4 != 0 || chunk * n != buffer.len() {
            return None;
        }
        for (d, c) in descriptors.iter_mut().zip(buffer.chunks_exact_mut(chunk)) {
            d.buffer = c.as_mut_ptr();
            d.set_size(chunk);
        }
        Some(Self { descriptors, buffer })
    }

    fn chunk(&self) -> usize {
        self.buffer.len() / self.descriptors.len()
    }
}

// SAFETY: the descriptors and the buffer are 'static and owned by the ring;
// the DMA writes only through descriptors handed to it in `prepare`.
unsafe impl DmaRxBuffer for Ring {
    type View = RingView;
    type Final = Ring;

    fn prepare(&mut self) -> Preparation {
        let n = self.descriptors.len();
        let base = self.descriptors.as_mut_ptr();
        for (i, d) in self.descriptors.iter_mut().enumerate() {
            d.reset_for_rx();
            // SAFETY: `(i + 1) % n` is within the slice.
            d.next = unsafe { base.add((i + 1) % n) };
        }
        Preparation {
            start: base,
            burst_transfer: BurstConfig::default(),
            // Never stop at a descriptor not read yet: the DMA would check it
            // when it prefetches, one chunk before the ring is full.
            check_owner: Some(false),
            auto_write_back: true,
        }
    }

    fn into_view(self) -> RingView {
        RingView { ring: self, idx: 0, offset: 0 }
    }

    fn from_view(view: RingView) -> Ring {
        view.ring
    }
}

/// The ring while the DMA writes it: [`peek`](Self::peek) and
/// [`consume`](Self::consume) as with esp-hal's stream buffer.
pub struct RingView {
    ring: Ring,
    /// The descriptor read next and the bytes of it already read.
    idx: usize,
    offset: usize,
}

impl RingView {
    /// The descriptor `k` places after the read position, as the DMA last
    /// wrote it.
    fn desc(&self, k: usize) -> DmaDescriptor {
        let i = (self.idx + k) % self.ring.descriptors.len();
        // SAFETY: a valid descriptor; volatile, since the DMA writes it.
        unsafe { core::ptr::read_volatile(&raw const self.ring.descriptors[i]) }
    }

    /// Bytes ready to read (all filled descriptors from the read position).
    pub fn available_bytes(&self) -> usize {
        let mut n = 0;
        for k in 0..self.ring.descriptors.len() {
            let d = self.desc(k);
            if d.owner() == Owner::Dma {
                break;
            }
            n += d.len();
        }
        n - self.offset
    }

    /// Every descriptor is filled: the DMA has caught up with the reader
    /// and is overwriting what it has not read. Reading on in ring order
    /// gets back behind the DMA by itself (it writes in that order too);
    /// up to a ring of audio is lost or out of order.
    pub fn overrun(&self) -> bool {
        (0..self.ring.descriptors.len()).all(|k| self.desc(k).owner() == Owner::Cpu)
    }

    /// The ready bytes that lie in one piece (up to the end of the buffer or
    /// a part-filled descriptor).
    pub fn peek(&self) -> &[u8] {
        let n = self.ring.descriptors.len();
        let mut len = 0;
        for k in 0..n - self.idx {
            let d = self.desc(k);
            if d.owner() == Owner::Dma {
                break;
            }
            len += d.len();
            if d.len() < d.size() {
                break;
            }
        }
        let start = self.idx * self.ring.chunk() + self.offset;
        &self.ring.buffer[start..][..len.saturating_sub(self.offset)]
    }

    /// Marks the first `n` ready bytes read, handing full descriptors back
    /// to the DMA. Returns how many were ready.
    pub fn consume(&mut self, mut n: usize) -> usize {
        let total = n;
        let count = self.ring.descriptors.len();
        while n > 0 {
            let d = self.desc(0);
            if d.owner() == Owner::Dma {
                break;
            }
            let left = d.len() - self.offset;
            if n < left {
                self.offset += n;
                return total;
            }
            n -= left;
            let desc = &mut self.ring.descriptors[self.idx];
            let mut fresh = d;
            fresh.reset_for_rx();
            // SAFETY: a valid descriptor; volatile, since the DMA reads it.
            unsafe { core::ptr::write_volatile(&raw mut desc.flags, fresh.flags) };
            self.idx = (self.idx + 1) % count;
            self.offset = 0;
        }
        total - n
    }
}
