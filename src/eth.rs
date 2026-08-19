//! Ethernet driver for the STM32F429 (ETH v1b, RMII).
//!
//! Copy-pasted from `embassy-stm32`'s `eth::v1` driver and adapted. It serves all three
//! stacks under test — the DMA path, the descriptor rings, the PHY handling and the
//! ring depths are bit-identical between them, so a benchmark difference is a stack
//! difference. What is *not* identical is how each stack wants to own frame memory, and
//! that is the design difference the benchmark is about:
//!
//!  * [`xarxa::iface::Interface`] moves **owned buffers**. On receive the driver hands
//!    the stack the very buffer the DMA filled and refills the descriptor with a freshly
//!    allocated one; on transmit the descriptor points at the payload of the buffer the
//!    stack handed down, which the driver owns until the hardware is done and then drops
//!    (dropping is what frees it). Zero memcpys, one allocation per frame per direction.
//!  * [`smoltcp::phy::Device`] hands out **borrows** via tokens. The slot's buffer stays
//!    put: RX borrows it in place and re-arms the descriptor with it, TX writes into it
//!    and sends it. Zero memcpys and zero allocations — smoltcp pays its copies one
//!    layer up, between the device buffer and the socket buffers.
//!  * **lwIP** passes **refcounted pbufs**. On receive the driver wraps the ring's own
//!    buffer in a custom pbuf ([`RxPbuf`]) that re-arms its descriptor when lwIP frees
//!    it, so nothing is copied and nothing is allocated; on transmit it takes a
//!    reference on the pbuf chain lwIP built and points one descriptor at each pbuf in
//!    it (scatter-gather), dropping the reference once the hardware is done. Zero
//!    memcpys either way.
//!
//! Which stack is built is a cargo feature, and only that stack is compiled in — see
//! [`Frame`] for how the descriptors' buffer type follows. The driver is polled, not
//! interrupt-driven, in every case: the benchmarks spin, so there is no waker to wake
//! and the ETH interrupt is left disabled.
//!
//! Everything else is trimmed to what this one board needs: RMII only, no MII, no PTP,
//! no `Phy` trait (the LAN8742 on the Nucleo is driven directly), and no checksum
//! offload — both stacks checksum in software.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, compiler_fence, fence};

use embassy_stm32::Peri;
use embassy_stm32::gpio::{AfType, Flex, OutputType, Speed};
use embassy_stm32::pac::eth::vals::{
    Apcs, Cr, Dm, DmaomrSr, Fes, Ftf, Ifg, Ipco, MbProgress, Mw, Pbl, Rpd, Rps, Rsf, St, Tsf,
};
use embassy_stm32::pac::{ETH, RCC, SYSCFG};
use embassy_stm32::peripherals;
use embassy_time::{Duration, Instant, block_for};
use vcell::VolatileCell;

/// Alternate function number of the ETH peripheral on STM32F4.
const AF_ETH: u8 = 11;

/// Largest frame the MAC may hand us or send: 1500 bytes of payload plus the 14-byte
/// Ethernet header. The FCS is appended/stripped by the MAC.
pub const MTU: usize = 1514;

/// What a descriptor points at, on the xarxa side.
///
/// This is xarxa's own `PacketBuf`, because that is the whole point: the buffer the DMA
/// fills is the buffer the stack owns, so it is heap-allocated and moves. The other two
/// stacks need no owned type: smoltcp borrows through tokens and lwIP refcounts pbufs
/// over [`Rings`]' static frame storage, which is what a normal application of either
/// does.
#[cfg(feature = "stack-xarxa")]
type Frame = xarxa::PacketBuf;

/// Whether the PHY last reported the link up.
///
/// Like [`stats`], a static because the driver disappears into whichever stack owns it.
pub static LINK_UP: AtomicBool = AtomicBool::new(false);

/// Frame counters, for the benchmarks.
///
/// `TX_FULL` is the one that matters: it counts frames the device had no room for, which
/// both stacks handle by not sending them. A TX benchmark's offered and achieved rates
/// differ by exactly that count.
pub mod stats {
    use super::AtomicU32;

    pub static RX_FRAMES: AtomicU32 = AtomicU32::new(0);
    pub static RX_BAD: AtomicU32 = AtomicU32::new(0);
    pub static TX_FRAMES: AtomicU32 = AtomicU32::new(0);
    pub static TX_FULL: AtomicU32 = AtomicU32::new(0);
}

/// Storage for one frame, in [`Rings`].
///
/// 4-byte aligned, with a size that is `MTU` rounded up to a multiple of 4: this DMA
/// engine requires both. (xarxa's `PacketBuf` is shaped the same way, so the hardware
/// sees the same thing either way.)
#[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
#[repr(C, align(4))]
pub struct FrameStorage([u8; MTU.next_multiple_of(4)]);

#[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
impl FrameStorage {
    const fn new() -> Self {
        Self([0; MTU.next_multiple_of(4)])
    }
}

/// One receive slot's custom pbuf, on the lwIP side.
///
/// lwIP frames are refcounted, so the driver can hand the ring's own buffer up the stack
/// instead of copying into a pool pbuf: this is a pbuf whose payload points into
/// [`Rings`]' receive storage and whose free callback re-arms the descriptor that buffer
/// belongs to. Until then the descriptor stays ours ([`RxPbuf::held`]) and the DMA has
/// nowhere to put a frame in that slot — which is the same backpressure a slow reader
/// applies on the other two stacks.
///
/// `#[repr(C)]` with the pbuf first: lwIP hands the free callback a `*mut pbuf`, which
/// is this struct's address.
#[cfg(feature = "stack-lwip")]
#[repr(C)]
pub struct RxPbuf {
    pb: lwip_sys::pbuf_custom,
    /// The descriptor to re-arm when lwIP is done with the frame.
    desc: *const RDes,
    /// The slot's buffer, i.e. the pbuf's payload.
    buf: *mut u8,
    cap: u16,
    /// Whether lwIP still holds this slot's pbuf.
    held: AtomicBool,
}

#[cfg(feature = "stack-lwip")]
impl RxPbuf {
    const fn new() -> Self {
        Self {
            pb: lwip_sys::pbuf_custom {
                pbuf: lwip_sys::pbuf {
                    next: core::ptr::null_mut(),
                    payload: core::ptr::null_mut(),
                    tot_len: 0,
                    len: 0,
                    type_internal: 0,
                    flags: 0,
                    ref_: 0,
                    if_idx: 0,
                },
                custom_free_function: None,
            },
            desc: core::ptr::null(),
            buf: core::ptr::null_mut(),
            cap: 0,
            held: AtomicBool::new(false),
        }
    }

    /// lwIP is done with the frame: give the buffer back to the DMA.
    ///
    /// This is the `pbuf_free_custom_fn` every receive pbuf carries.
    unsafe extern "C" fn free(p: *mut lwip_sys::pbuf) {
        // SAFETY: every pbuf carrying this callback is the first field of an `RxPbuf`,
        // and both pointers in it were set by `RxRing::take_pbuf`.
        unsafe {
            let this = p as *mut Self;
            let buf = core::slice::from_raw_parts_mut((*this).buf, (*this).cap as usize);
            (*(*this).desc).set_ready(buf);
            (*this).held.store(false, Ordering::Relaxed);
        }
        ETH.ethernet_dma().dmarpdr().write(|w| w.set_rpd(Rpd::Poll));
    }
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

mod rx_consts {
    /// Owned by DMA engine.
    pub const RXDESC_0_OWN: u32 = 1 << 31;
    /// Error summary.
    pub const RXDESC_0_ES: u32 = 1 << 15;
    /// Frame length.
    pub const RXDESC_0_FL_MASK: u32 = 0x3FFF;
    pub const RXDESC_0_FL_SHIFT: usize = 16;
    /// First descriptor of the frame.
    pub const RXDESC_0_FS: u32 = 1 << 9;
    /// Last descriptor of the frame.
    pub const RXDESC_0_LS: u32 = 1 << 8;

    pub const RXDESC_1_RBS1_MASK: u32 = 0x1FFF;
    /// Second address chained.
    pub const RXDESC_1_RCH: u32 = 1 << 14;
    /// End of ring.
    pub const RXDESC_1_RER: u32 = 1 << 15;
}

mod tx_consts {
    pub const TXDESC_0_OWN: u32 = 1 << 31;
    /// Interrupt on completion.
    pub const TXDESC_0_IOC: u32 = 1 << 30;
    /// Last segment of the frame.
    pub const TXDESC_0_LS: u32 = 1 << 29;
    /// First segment of the frame.
    pub const TXDESC_0_FS: u32 = 1 << 28;
    /// Transmit end of ring.
    pub const TXDESC_0_TER: u32 = 1 << 21;
    /// Second address chained.
    pub const TXDESC_0_TCH: u32 = 1 << 20;
    /// Error summary.
    pub const TXDESC_0_ES: u32 = 1 << 15;
    /// Checksum insertion control: none. Both stacks checksum in software.
    pub const TXDESC_0_CIC_NONE: u32 = 0b00 << 22;
    /// The status the DMA writes back, as opposed to the control bits above it.
    pub const TXDESC_0_STATUS_MASK: u32 = 0xFFFF;

    pub const TXDESC_1_TBS1_MASK: u32 = 0x1FFF;
}

use rx_consts::*;
use tx_consts::*;

/// Enhanced receive descriptor (8 words, 32 bytes).
#[repr(C, align(4))]
struct RDes {
    rdes0: VolatileCell<u32>,
    rdes1: VolatileCell<u32>,
    rdes2: VolatileCell<u32>,
    rdes3: VolatileCell<u32>,
    rdes4: VolatileCell<u32>,
    rdes5: VolatileCell<u32>,
    rdes6: VolatileCell<u32>,
    rdes7: VolatileCell<u32>,
}

impl RDes {
    const fn new() -> Self {
        Self {
            rdes0: VolatileCell::new(0),
            rdes1: VolatileCell::new(0),
            rdes2: VolatileCell::new(0),
            rdes3: VolatileCell::new(0),
            rdes4: VolatileCell::new(0),
            rdes5: VolatileCell::new(0),
            rdes6: VolatileCell::new(0),
            rdes7: VolatileCell::new(0),
        }
    }

    /// True if the DMA has handed this descriptor back to us.
    fn available(&self) -> bool {
        self.rdes0.get() & RXDESC_0_OWN == 0
    }

    /// True if the descriptor holds one entire, error-free frame.
    fn valid(&self) -> bool {
        let rdes0 = self.rdes0.get();
        rdes0 & RXDESC_0_ES == 0 && rdes0 & (RXDESC_0_FS | RXDESC_0_LS) == RXDESC_0_FS | RXDESC_0_LS
    }

    fn frame_len(&self) -> usize {
        ((self.rdes0.get() >> RXDESC_0_FL_SHIFT) & RXDESC_0_FL_MASK) as usize
    }

    /// Point the descriptor at `buf` and hand it back to the DMA.
    ///
    /// `buf` must remain valid, and untouched by the CPU, until the DMA gives the
    /// descriptor back (`available()`).
    fn set_ready(&self, buf: &mut [u8]) {
        self.rdes1
            .set((self.rdes1.get() & !RXDESC_1_RBS1_MASK) | (buf.len() as u32 & RXDESC_1_RBS1_MASK));
        self.rdes2.set(buf.as_mut_ptr() as u32);

        // "Preceding reads and writes cannot be moved past subsequent writes."
        fence(Ordering::Release);
        compiler_fence(Ordering::Release);

        self.rdes0.set(RXDESC_0_OWN);

        // Flush the store buffer, to hand the descriptor over as fast as possible.
        fence(Ordering::SeqCst);
    }

    fn setup(&self, next: Option<&Self>, buf: &mut [u8]) {
        self.rdes0.set(0);
        self.rdes1.set(RXDESC_1_RCH);
        self.rdes4.set(0);
        self.rdes5.set(0);
        self.rdes6.set(0);
        self.rdes7.set(0);

        match next {
            // Chained mode: the second buffer address is the next descriptor.
            Some(next) => self.rdes3.set(next as *const Self as u32),
            None => {
                self.rdes3.set(0);
                self.rdes1.set(self.rdes1.get() | RXDESC_1_RER);
            }
        }

        self.set_ready(buf);
    }
}

/// Enhanced transmit descriptor (8 words, 32 bytes).
#[repr(C, align(4))]
struct TDes {
    tdes0: VolatileCell<u32>,
    tdes1: VolatileCell<u32>,
    tdes2: VolatileCell<u32>,
    tdes3: VolatileCell<u32>,
    tdes4: VolatileCell<u32>,
    tdes5: VolatileCell<u32>,
    tdes6: VolatileCell<u32>,
    tdes7: VolatileCell<u32>,
}

impl TDes {
    const fn new() -> Self {
        Self {
            tdes0: VolatileCell::new(0),
            tdes1: VolatileCell::new(0),
            tdes2: VolatileCell::new(0),
            tdes3: VolatileCell::new(0),
            tdes4: VolatileCell::new(0),
            tdes5: VolatileCell::new(0),
            tdes6: VolatileCell::new(0),
            tdes7: VolatileCell::new(0),
        }
    }

    /// True if the DMA is done with this descriptor (and so with its buffer).
    fn available(&self) -> bool {
        self.tdes0.get() & TXDESC_0_OWN == 0
    }

    fn setup(&self, next: Option<&Self>) {
        self.tdes0
            .set(TXDESC_0_TCH | TXDESC_0_IOC | TXDESC_0_FS | TXDESC_0_LS | TXDESC_0_CIC_NONE);
        self.tdes1.set(0);
        self.tdes2.set(0);
        self.tdes4.set(0);
        self.tdes5.set(0);
        self.tdes6.set(0);
        self.tdes7.set(0);

        match next {
            // Chained mode: the second buffer address is the next descriptor.
            Some(next) => self.tdes3.set(next as *const Self as u32),
            None => {
                self.tdes3.set(0);
                self.tdes0.set(self.tdes0.get() | TXDESC_0_TER);
            }
        }
    }

    /// Point the descriptor at one segment of a frame, without handing it to the DMA yet.
    ///
    /// This is the multi-descriptor form of [`transmit`](Self::transmit), for the lwIP
    /// side, where one frame is a chain of pbufs and each pbuf gets a descriptor: the
    /// frame's first and last descriptors carry FS and LS, and the caller sets OWN on
    /// the first one only once every later descriptor is ready (see
    /// [`TxRing::submit_chain`]).
    #[cfg(feature = "stack-lwip")]
    fn prepare(&self, payload: &[u8], first: bool, last: bool) {
        self.tdes2.set(payload.as_ptr() as u32);
        self.tdes1
            .set((self.tdes1.get() & !TXDESC_1_TBS1_MASK) | (payload.len() as u32 & TXDESC_1_TBS1_MASK));

        // Keep the control bits `setup` wrote (TCH/IOC/CIC/TER), clear the status the
        // DMA wrote back for the previous frame, and place this segment in the frame.
        let mut tdes0 = self.tdes0.get() & !(TXDESC_0_STATUS_MASK | TXDESC_0_FS | TXDESC_0_LS);
        if first {
            tdes0 |= TXDESC_0_FS;
        }
        if last {
            tdes0 |= TXDESC_0_LS;
        }
        self.tdes0.set(tdes0);
    }

    /// Hand a descriptor prepared by [`prepare`](Self::prepare) to the DMA.
    #[cfg(feature = "stack-lwip")]
    fn set_own(&self) {
        fence(Ordering::Release);
        compiler_fence(Ordering::Release);
        self.tdes0.set(self.tdes0.get() | TXDESC_0_OWN);
        fence(Ordering::SeqCst);
    }

    /// Point the descriptor at `payload` and hand it to the DMA.
    ///
    /// `payload` must remain valid, and untouched by the CPU, until the DMA gives the
    /// descriptor back (`available()`).
    #[cfg(not(feature = "stack-lwip"))]
    fn transmit(&self, payload: &[u8]) {
        self.tdes2.set(payload.as_ptr() as u32);
        self.tdes1
            .set((self.tdes1.get() & !TXDESC_1_TBS1_MASK) | (payload.len() as u32 & TXDESC_1_TBS1_MASK));

        fence(Ordering::Release);
        compiler_fence(Ordering::Release);

        // Keep the control bits (TCH/IOC/FS/LS/CIC/TER, all above bit 16) and clear the
        // status the DMA wrote back for the previous frame, so it can't be misread as
        // this one's.
        self.tdes0
            .set((self.tdes0.get() & !TXDESC_0_STATUS_MASK) | TXDESC_0_OWN);

        fence(Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Rings
//
// RX and TX are separate structs, each with its own descriptors, because smoltcp's
// `Device::receive` hands out an RX token and a TX token at the same time and they have
// to borrow disjoint fields.
//
// The descriptors — and, for smoltcp, the frame buffers — live in one [`Rings`] value
// that the caller puts in a `static`. That is what embassy-stm32's own driver does, and
// it is what a smoltcp application would do: no allocator on the packet path at all.
// The xarxa build has no static frame buffers, because there the buffers are owned
// values that move between the driver and the stack, which is the design under test.
// ---------------------------------------------------------------------------

/// The DMA rings' backing storage. Put one in a `static`.
///
/// What is in here follows the stack's buffer model: xarxa's ring holds owned buffers
/// that live in the heap, so only the descriptors are static; smoltcp borrows its frame
/// storage in both directions; lwIP receives into static storage and transmits straight
/// out of its own pbufs, so only the receive side is here.
pub struct Rings<const TX: usize, const RX: usize> {
    tx_desc: [TDes; TX],
    rx_desc: [RDes; RX],
    #[cfg(feature = "stack-smoltcp")]
    tx_buf: [FrameStorage; TX],
    #[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
    rx_buf: [FrameStorage; RX],
    #[cfg(feature = "stack-lwip")]
    rx_pbuf: [RxPbuf; RX],
}

impl<const TX: usize, const RX: usize> Rings<TX, RX> {
    pub const fn new() -> Self {
        Self {
            tx_desc: [const { TDes::new() }; TX],
            rx_desc: [const { RDes::new() }; RX],
            #[cfg(feature = "stack-smoltcp")]
            tx_buf: [const { FrameStorage::new() }; TX],
            #[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
            rx_buf: [const { FrameStorage::new() }; RX],
            #[cfg(feature = "stack-lwip")]
            rx_pbuf: [const { RxPbuf::new() }; RX],
        }
    }
}

struct RxRing<const N: usize> {
    desc: &'static mut [RDes; N],
    /// Where the DMA receives into. Owned, heap-allocated buffers that get handed to the
    /// stack and replaced (xarxa), or static storage that is borrowed and reused
    /// (smoltcp, lwIP).
    #[cfg(feature = "stack-xarxa")]
    bufs: [Option<Frame>; N],
    #[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
    bufs: &'static mut [FrameStorage; N],
    /// The custom pbuf wrapping each slot. A raw pointer rather than a reference because
    /// [`RxPbuf::free`] writes through it from inside lwIP, at moments when this ring is
    /// borrowed elsewhere.
    #[cfg(feature = "stack-lwip")]
    pbufs: *mut RxPbuf,
    index: usize,
}

impl<const N: usize> RxRing<N> {
    fn new(
        desc: &'static mut [RDes; N],
        #[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))] bufs: &'static mut [FrameStorage; N],
        #[cfg(feature = "stack-lwip")] pbufs: &'static mut [RxPbuf; N],
    ) -> Self {
        assert!(N > 1);

        #[cfg(feature = "stack-xarxa")]
        let mut this = Self {
            desc,
            bufs: [const { None }; N],
            index: 0,
        };
        #[cfg(feature = "stack-smoltcp")]
        let mut this = Self { desc, bufs, index: 0 };
        #[cfg(feature = "stack-lwip")]
        let mut this = Self {
            desc,
            bufs,
            pbufs: pbufs.as_mut_ptr(),
            index: 0,
        };

        for i in 0..N {
            #[cfg(feature = "stack-xarxa")]
            {
                this.bufs[i] = Some(Frame::new());
            }
            // Chain each descriptor to the next; the last one wraps via the ring bit.
            let next: Option<*const RDes> = (i + 1 < N).then(|| &this.desc[i + 1] as *const RDes);
            let storage = this.slot(i) as *mut [u8];
            // SAFETY: `next` and `storage` point into `this`, which owns both for as long
            // as the DMA can use them; the two borrows are of disjoint fields.
            unsafe {
                this.desc[i].setup(next.map(|p| &*p), &mut *storage);
            }
        }

        ETH.ethernet_dma().dmardlar().write(|w| w.0 = this.desc.as_ptr() as u32);

        this
    }

    /// The storage of slot `i`, whichever kind of buffer this build uses.
    fn slot(&mut self, i: usize) -> &mut [u8] {
        #[cfg(feature = "stack-xarxa")]
        return self.bufs[i].as_mut().unwrap().storage_mut();
        #[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
        return &mut self.bufs[i].0;
    }

    fn demand_poll(&self) {
        ETH.ethernet_dma().dmarpdr().write(|w| w.set_rpd(Rpd::Poll));
    }

    /// True if the receive DMA is running (as opposed to stopped, or suspended because
    /// it ran out of descriptors).
    fn running(&self) -> bool {
        matches!(
            ETH.ethernet_dma().dmasr().read().rps(),
            Rps::RunningFetching | Rps::RunningWaiting | Rps::RunningWriting
        )
    }

    /// Length of the frame waiting at the head of the ring, if any.
    ///
    /// Bad frames are recycled and skipped over, so what this returns is always a whole,
    /// error-free frame that fits in a buffer.
    fn peek(&mut self) -> Option<usize> {
        if !self.running() {
            self.demand_poll();
        }
        // Not strictly needed on a Cortex-M4 without caches, but it costs nothing and
        // documents that the descriptor was written by something other than this core.
        fence(Ordering::SeqCst);

        loop {
            // On lwIP a slot whose pbuf is still out has a descriptor that is neither
            // ours to read nor the DMA's to fill: it was handed up and has not been
            // re-armed. The frames behind it wait, exactly as they would if the
            // application were slow to read on either of the other two stacks.
            #[cfg(feature = "stack-lwip")]
            // SAFETY: `pbufs` points at the ring's `RX` slots, for the life of the ring.
            if unsafe { (*self.pbufs.add(self.index)).held.load(Ordering::Relaxed) } {
                return None;
            }

            let desc = &self.desc[self.index];
            if !desc.available() {
                return None;
            }

            let len = desc.frame_len();
            let ok = desc.valid() && len > 0 && len <= MTU;
            stats::RX_FRAMES.fetch_add(1, Ordering::Relaxed);
            if ok {
                return Some(len);
            }

            stats::RX_BAD.fetch_add(1, Ordering::Relaxed);
            defmt::debug!("eth: dropping bad rx frame, len={} rdes0={:08x}", len, desc.rdes0.get());
            self.recycle();
        }
    }

    /// The frame at the head of the ring. Only valid right after [`peek`](Self::peek).
    #[cfg(feature = "stack-smoltcp")]
    fn frame(&self, len: usize) -> &[u8] {
        &self.bufs[self.index].0[..len]
    }

    /// Re-arm the head descriptor with the buffer it already has, and advance.
    fn recycle(&mut self) {
        let i = self.index;
        let storage = self.slot(i) as *mut [u8];
        // SAFETY: `desc` and `bufs` are disjoint fields of `self`.
        unsafe { self.desc[i].set_ready(&mut *storage) };
        self.index = (i + 1) % N;
        self.demand_poll();
    }

    /// Take the frame at the head of the ring, replacing it with a fresh buffer so the
    /// descriptor still has somewhere to receive into. This is the owned-buffer path.
    #[cfg(feature = "stack-xarxa")]
    fn take(&mut self, len: usize) -> Frame {
        let i = self.index;
        let mut buf = self.bufs[i].take().unwrap();
        let mut fresh = Frame::new();
        self.desc[i].set_ready(fresh.storage_mut());
        self.bufs[i] = Some(fresh);
        self.index = (i + 1) % N;
        self.demand_poll();

        // The DMA wrote the frame at offset 0, so there is no headroom to skip.
        buf.set_len(len);
        buf
    }

    /// Wrap the frame at the head of the ring in a custom pbuf and advance.
    ///
    /// The pbuf's payload *is* the slot's buffer: nothing is copied, and the descriptor
    /// is left un-armed until [`RxPbuf::free`] runs, which is what stops the DMA from
    /// overwriting a frame lwIP still holds.
    #[cfg(feature = "stack-lwip")]
    fn take_pbuf(&mut self, len: usize) -> *mut lwip_sys::pbuf {
        let i = self.index;
        let desc: *const RDes = &self.desc[i];
        let buf = self.bufs[i].0.as_mut_ptr();
        let cap = self.bufs[i].0.len() as u16;

        // SAFETY: `pbufs` points at the ring's `RX` slots, and slot `i` is not held (the
        // caller got here through `peek`).
        let p = unsafe {
            let slot = self.pbufs.add(i);
            (*slot).desc = desc;
            (*slot).buf = buf;
            (*slot).cap = cap;
            (*slot).pb.custom_free_function = Some(RxPbuf::free);
            (*slot).held.store(true, Ordering::Relaxed);
            lwip_sys::pbuf_alloced_custom(
                lwip_sys::pbuf_layer_PBUF_RAW,
                len as u16,
                lwip_sys::pbuf_type_PBUF_REF,
                &raw mut (*slot).pb,
                buf.cast(),
                cap,
            )
        };

        if p.is_null() {
            // Cannot happen — `peek` bounds the length by the buffer — but leaving the
            // slot held would strand it for good.
            unsafe { (*self.pbufs.add(i)).held.store(false, Ordering::Relaxed) };
            self.recycle();
            return p;
        }

        self.index = (i + 1) % N;
        self.demand_poll();
        p
    }
}

struct TxRing<const N: usize> {
    desc: &'static mut [TDes; N],
    /// What the descriptors point at: the buffers the stack handed down, held until the
    /// hardware is done with them (xarxa), static storage written in place (smoltcp), or
    /// a reference on the pbuf chain lwIP built, held on the chain's *last* descriptor.
    #[cfg(feature = "stack-xarxa")]
    bufs: [Option<Frame>; N],
    #[cfg(feature = "stack-smoltcp")]
    bufs: &'static mut [FrameStorage; N],
    #[cfg(feature = "stack-lwip")]
    bufs: [*mut lwip_sys::pbuf; N],
    index: usize,
    /// Oldest descriptor handed to the DMA and not yet reclaimed, and how many there
    /// are. Only the lwIP path needs them: one frame can span several descriptors there,
    /// so "is there room" is a count, not a look at the head.
    #[cfg(feature = "stack-lwip")]
    tail: usize,
    #[cfg(feature = "stack-lwip")]
    used: usize,
}

impl<const N: usize> TxRing<N> {
    fn new(
        desc: &'static mut [TDes; N],
        #[cfg(feature = "stack-smoltcp")] bufs: &'static mut [FrameStorage; N],
    ) -> Self {
        assert!(N > 0);
        for i in 0..N {
            let next = if i + 1 < N { Some(&desc[i + 1]) } else { None };
            desc[i].setup(next);
        }

        ETH.ethernet_dma().dmatdlar().write(|w| w.0 = desc.as_ptr() as u32);

        Self {
            desc,
            #[cfg(feature = "stack-xarxa")]
            bufs: [const { None }; N],
            #[cfg(feature = "stack-smoltcp")]
            bufs,
            #[cfg(feature = "stack-lwip")]
            bufs: [core::ptr::null_mut(); N],
            index: 0,
            #[cfg(feature = "stack-lwip")]
            tail: 0,
            #[cfg(feature = "stack-lwip")]
            used: 0,
        }
    }

    /// True if the head descriptor is ours, i.e. there is room to send.
    #[cfg(not(feature = "stack-lwip"))]
    fn available(&self) -> bool {
        self.desc[self.index].available()
    }

    /// Book-keeping after handing a descriptor to the DMA.
    #[cfg(not(feature = "stack-lwip"))]
    fn advance(&mut self) {
        stats::TX_FRAMES.fetch_add(1, Ordering::Relaxed);
        self.index = (self.index + 1) % N;

        fence(Ordering::Release);
        // Ask the DMA to re-read the descriptor list.
        ETH.ethernet_dma().dmatpdr().write(|w| w.0 = 1);
    }

    fn check_error(&self, i: usize) {
        if self.desc[i].tdes0.get() & TXDESC_0_ES != 0 {
            defmt::debug!("eth: tx error, tdes0={:08x}", self.desc[i].tdes0.get());
        }
    }

    /// Send the buffer the stack built, and hold it until the hardware is done. This is
    /// the owned-buffer path.
    #[cfg(feature = "stack-xarxa")]
    fn submit(&mut self, buf: Frame) {
        let i = self.index;
        self.check_error(i);
        self.desc[i].transmit(&buf);
        // Keep the buffer alive until the DMA hands the descriptor back. This also frees
        // whatever was here before, which the DMA is done with.
        self.bufs[i] = Some(buf);
        self.advance();
    }

    /// Build a frame of `len` bytes in place in the head slot's static buffer, then send
    /// it. This is the borrowed path; nothing is allocated or freed.
    #[cfg(feature = "stack-smoltcp")]
    fn send_with<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let i = self.index;
        debug_assert!(self.desc[i].available());
        self.check_error(i);

        let r = f(&mut self.bufs[i].0[..len]);
        self.desc[i].transmit(&self.bufs[i].0[..len]);
        self.advance();
        r
    }

    /// Free the pbufs of every frame the hardware has finished with.
    ///
    /// The reference taken in [`submit_chain`](Self::submit_chain) is dropped here, which
    /// is what returns a transmitted frame's memory to lwIP's heap.
    #[cfg(feature = "stack-lwip")]
    fn reclaim(&mut self) {
        while self.used > 0 && self.desc[self.tail].available() {
            self.check_error(self.tail);
            let p = core::mem::replace(&mut self.bufs[self.tail], core::ptr::null_mut());
            if !p.is_null() {
                // SAFETY: one reference on this chain, taken when it was submitted.
                unsafe { lwip_sys::pbuf_free(p) };
            }
            self.tail = (self.tail + 1) % N;
            self.used -= 1;
        }
    }

    /// Send the pbuf chain lwIP built, one descriptor per pbuf, and hold a reference on
    /// it until the hardware is done. Returns false if the ring has no room for the whole
    /// chain, in which case nothing was sent and no reference was taken.
    ///
    /// The chain's descriptors are filled back to front as far as ownership goes: every
    /// descriptor but the first is handed to the DMA as it is written, and the first one
    /// last, so the hardware can never start on a frame whose later segments are still
    /// being set up.
    #[cfg(feature = "stack-lwip")]
    fn submit_chain(&mut self, head: *mut lwip_sys::pbuf) -> bool {
        self.reclaim();

        // SAFETY: `head` is a chain lwIP handed to `linkoutput`, valid for this call.
        let segments = unsafe {
            let mut n = 0;
            let mut q = head;
            while !q.is_null() {
                n += 1;
                q = (*q).next;
            }
            n
        };
        if segments > N - self.used {
            return false;
        }

        let first = self.index;
        let mut i = first;
        // SAFETY: as above, plus every descriptor written here is free (`used` says so),
        // and the reference taken below is released in `reclaim`.
        unsafe {
            lwip_sys::pbuf_ref(head);
            let mut q = head;
            while !q.is_null() {
                let next = (*q).next;
                let payload = core::slice::from_raw_parts((*q).payload as *const u8, (*q).len as _);
                self.desc[i].prepare(payload, i == first, next.is_null());
                // The whole chain is freed through its head, so only the last descriptor
                // carries the reference: it is the one that completes last.
                self.bufs[i] = if next.is_null() { head } else { core::ptr::null_mut() };
                if i != first {
                    self.desc[i].set_own();
                }
                i = (i + 1) % N;
                q = next;
            }
        }
        self.desc[first].set_own();

        self.index = i;
        self.used += segments;
        stats::TX_FRAMES.fetch_add(1, Ordering::Relaxed);

        fence(Ordering::Release);
        // Ask the DMA to re-read the descriptor list.
        ETH.ethernet_dma().dmatpdr().write(|w| w.0 = 1);
        true
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// STM32F429 ethernet driver.
///
/// `TX` and `RX` are the ring depths. Each RX slot holds a [`PacketBuf`] for the whole
/// life of the driver (the DMA needs somewhere to put the next frame), so the RX ring
/// permanently costs `RX` buffers out of the heap.
pub struct Ethernet<const TX: usize, const RX: usize> {
    _peri: Peri<'static, peripherals::ETH>,
    _pins: [Flex<'static>; 9],

    rx: RxRing<RX>,
    tx: TxRing<TX>,

    /// Only lwIP asks the driver for it, in the netif init callback.
    #[cfg(feature = "stack-lwip")]
    mac_addr: [u8; 6],
    phy_addr: u8,
    link_up: bool,
    link_poll_at: Instant,
}

impl<const TX: usize, const RX: usize> Ethernet<TX, RX> {
    /// Bring up the ETH peripheral in RMII mode on the Nucleo-F429ZI's pins.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rings: &'static mut Rings<TX, RX>,
        peri: Peri<'static, peripherals::ETH>,
        ref_clk: Peri<'static, peripherals::PA1>,
        crs: Peri<'static, peripherals::PA7>,
        rx_d0: Peri<'static, peripherals::PC4>,
        rx_d1: Peri<'static, peripherals::PC5>,
        tx_d0: Peri<'static, peripherals::PG13>,
        tx_d1: Peri<'static, peripherals::PB13>,
        tx_en: Peri<'static, peripherals::PG11>,
        mdio: Peri<'static, peripherals::PA2>,
        mdc: Peri<'static, peripherals::PC1>,
        mac_addr: [u8; 6],
    ) -> Self {
        let af = AfType::output(OutputType::PushPull, Speed::VeryHigh);
        let mut pins = [
            Flex::new(ref_clk),
            Flex::new(crs),
            Flex::new(rx_d0),
            Flex::new(rx_d1),
            Flex::new(tx_d0),
            Flex::new(tx_d1),
            Flex::new(tx_en),
            Flex::new(mdio),
            Flex::new(mdc),
        ];
        critical_section::with(|_| {
            for pin in &mut pins {
                pin.set_as_af_unchecked(AF_ETH, af);
            }
        });

        critical_section::with(|_| {
            RCC.ahb1enr().modify(|w| {
                w.set_ethen(true);
                w.set_ethtxen(true);
                w.set_ethrxen(true);
            });
            // SYSCFG_PMC holds the (R)MII selection bit, so its clock must be on.
            RCC.apb2enr().modify(|w| w.set_syscfgen(true));
            // Reduced Media Independent Interface. Must be set while the MAC is under
            // reset, i.e. before the DMA soft reset below completes.
            SYSCFG.pmc().modify(|w| w.set_mii_rmii_sel(true));
        });

        let dma = ETH.ethernet_dma();
        let mac = ETH.ethernet_mac();

        // Reset the MAC/DMA and wait for the reset to complete.
        dma.dmabmr().modify(|w| w.set_sr(true));
        while dma.dmabmr().read().sr() {}

        // Enhanced descriptor format: 8 words per descriptor instead of 4. Must be set
        // before the DMA starts fetching descriptors.
        dma.dmabmr().modify(|w| w.set_edfe(true));

        mac.maccr().modify(|w| {
            w.set_ifg(Ifg::Ifg96); // inter frame gap: 96 bit times
            // Strip padding and FCS from received frames. APCS only covers 802.3
            // length-field frames (length <= 1500); CSTF is what strips the FCS off
            // EtherType frames, which is everything we actually care about. Without it
            // the reported frame length includes the 4-byte FCS, and a full 1514-byte
            // frame no longer fits in a `PacketBuf`.
            w.set_apcs(Apcs::Strip);
            w.set_cstf(true);
            w.set_fes(Fes::Fes100); // 100 Mbps
            w.set_dm(Dm::FullDuplex);
            w.set_ipco(Ipco::Disabled); // no RX checksum offload: both stacks do software
        });

        // Pass all multicast frames up; the stack filters them.
        mac.macffr().modify(|w| w.set_pam(true));

        // Note: writing MACA0LR triggers the synchronisation of both halves into the
        // MAC core, so it must come after the MACA0HR write.
        mac.maca0hr()
            .modify(|w| w.set_maca0h(u16::from(mac_addr[4]) | (u16::from(mac_addr[5]) << 8)));
        mac.maca0lr().write(|w| {
            w.set_maca0l(
                u32::from(mac_addr[0])
                    | (u32::from(mac_addr[1]) << 8)
                    | (u32::from(mac_addr[2]) << 16)
                    | (u32::from(mac_addr[3]) << 24),
            )
        });

        mac.macfcr().modify(|w| w.set_pt(0x100)); // pause time

        dma.dmaomr().modify(|w| {
            w.set_tsf(Tsf::StoreForward);
            w.set_rsf(Rsf::StoreForward);
        });
        dma.dmabmr().modify(|w| w.set_pbl(Pbl::Pbl32));

        // Split the storage into the two halves the rings own independently.
        let Rings {
            tx_desc,
            rx_desc,
            #[cfg(feature = "stack-smoltcp")]
            tx_buf,
            #[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
            rx_buf,
            #[cfg(feature = "stack-lwip")]
            rx_pbuf,
        } = rings;
        let tx = TxRing::new(
            tx_desc,
            #[cfg(feature = "stack-smoltcp")]
            tx_buf,
        );
        let rx = RxRing::new(
            rx_desc,
            #[cfg(any(feature = "stack-smoltcp", feature = "stack-lwip"))]
            rx_buf,
            #[cfg(feature = "stack-lwip")]
            rx_pbuf,
        );

        fence(Ordering::SeqCst);

        mac.maccr().modify(|w| {
            w.set_re(true);
            w.set_te(true);
        });
        dma.dmaomr().modify(|w| {
            w.set_ftf(Ftf::Flush); // flush the transmit FIFO
            w.set_st(St::Started);
            w.set_sr(DmaomrSr::Started);
        });

        let mut this = Self {
            _peri: peri,
            _pins: pins,
            rx,
            tx,
            #[cfg(feature = "stack-lwip")]
            mac_addr,
            phy_addr: 0,
            link_up: false,
            link_poll_at: Instant::from_ticks(0),
        };

        this.rx.demand_poll();
        this.phy_reset();
        this.phy_init();

        this
    }

    // -- PHY (LAN8742 on the Nucleo, driven over MDIO) --

    fn smi_read(&mut self, phy_addr: u8, reg: u8) -> u16 {
        let mac = ETH.ethernet_mac();
        mac.macmiiar().modify(|w| {
            w.set_pa(phy_addr);
            w.set_mr(reg);
            w.set_mw(Mw::Read);
            // HCLK is 180 MHz (see `rcc_config`), so divide by 102 for a ~1.8 MHz MDC.
            w.set_cr(Cr::Cr150168);
            w.set_mb(MbProgress::Busy);
        });
        while mac.macmiiar().read().mb() == MbProgress::Busy {}
        mac.macmiidr().read().md()
    }

    fn smi_write(&mut self, phy_addr: u8, reg: u8, val: u16) {
        let mac = ETH.ethernet_mac();
        mac.macmiidr().write(|w| w.set_md(val));
        mac.macmiiar().modify(|w| {
            w.set_pa(phy_addr);
            w.set_mr(reg);
            w.set_mw(Mw::Write);
            w.set_cr(Cr::Cr150168);
            w.set_mb(MbProgress::Busy);
        });
        while mac.macmiiar().read().mb() == MbProgress::Busy {}
    }

    /// Find the PHY's SMI address by resetting every address in turn and seeing which
    /// one clears its reset bit.
    fn phy_reset(&mut self) {
        for addr in 0..32 {
            self.smi_write(addr, PHY_REG_BCR, PHY_REG_BCR_RESET);
            for _ in 0..10 {
                if self.smi_read(addr, PHY_REG_BCR) & PHY_REG_BCR_RESET != PHY_REG_BCR_RESET {
                    defmt::info!("eth: found PHY at SMI address {}", addr);
                    self.phy_addr = addr;
                    return;
                }
                // Give the PHY a total of 100ms to respond.
                block_for(Duration::from_millis(10));
            }
        }
        defmt::panic!("eth: PHY did not respond on any SMI address");
    }

    /// Block until the PHY reports the link up, or `timeout` elapses.
    ///
    /// The benchmarks connect straight away, so they need the cable to be up first —
    /// otherwise the first SYN goes out into a link that is not there yet. Frames that
    /// arrive meanwhile just sit in the RX ring.
    pub fn wait_link_up(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.poll_link();
            if self.link_up {
                return;
            }
            if Instant::now() > deadline {
                defmt::warn!("eth: link still down, carrying on anyway");
                return;
            }
        }
    }

    fn phy_init(&mut self) {
        // Advertise 100BASE-TX full duplex only, and restart auto-negotiation.
        let antx = self.smi_read(self.phy_addr, PHY_REG_ANTX);
        let antx = (antx & !PHY_REG_ANTX_TECH) | PHY_REG_ANTX_100BTX_FD;
        self.smi_write(self.phy_addr, PHY_REG_ANTX, antx);
        self.smi_write(self.phy_addr, PHY_REG_BCR, PHY_REG_BCR_AN | PHY_REG_BCR_ANRST);
    }

    /// Poll the PHY's link status, at most twice a second, logging any change.
    ///
    /// Nothing in either stack depends on this; it is here so you can tell a dead cable
    /// from a dead stack. Called on the receive path, which both stacks poll.
    fn poll_link(&mut self) {
        let now = Instant::now();
        if now < self.link_poll_at {
            return;
        }
        self.link_poll_at = now + Duration::from_millis(500);

        let bsr = self.smi_read(self.phy_addr, PHY_REG_BSR);
        let up = bsr & PHY_REG_BSR_ANDONE != 0 && bsr & PHY_REG_BSR_UP != 0;
        if up != self.link_up {
            self.link_up = up;
            LINK_UP.store(up, Ordering::Relaxed);
            if up {
                defmt::info!("eth: link up");
            } else {
                defmt::warn!("eth: link down");
            }
        }
    }
}

impl<const TX: usize, const RX: usize> Drop for Ethernet<TX, RX> {
    fn drop(&mut self) {
        let dma = ETH.ethernet_dma();
        let mac = ETH.ethernet_mac();

        // Stop the DMA before the descriptors and buffers are freed.
        dma.dmaomr().modify(|w| w.set_st(St::Stopped));
        mac.maccr().modify(|w| {
            w.set_re(false);
            w.set_te(false);
        });
        dma.dmaomr().modify(|w| w.set_sr(DmaomrSr::Stopped));

        fence(Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// xarxa: owned buffers
// ---------------------------------------------------------------------------

#[cfg(feature = "stack-xarxa")]
impl<const TX: usize, const RX: usize> xarxa::iface::Interface for Ethernet<TX, RX> {
    fn capabilities(&self) -> xarxa::iface::IfaceCapabilities {
        let mut caps = xarxa::iface::IfaceCapabilities::default();
        caps.medium = xarxa::iface::Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps
    }

    fn receive(&mut self) -> Option<xarxa::PacketBuf> {
        self.poll_link();
        let len = self.rx.peek()?;
        Some(self.rx.take(len))
    }

    fn transmit(&mut self, buf: xarxa::PacketBuf) -> Result<(), xarxa::PacketBuf> {
        if !self.tx.available() {
            // Ring full: the hardware still owns this descriptor. The stack drops the
            // frame, so count it.
            stats::TX_FULL.fetch_add(1, Ordering::Relaxed);
            return Err(buf);
        }
        if buf.len() > MTU {
            defmt::warn!("eth: dropping oversized tx frame of {} bytes", buf.len());
            return Ok(());
        }

        self.tx.submit(buf);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// smoltcp: borrowed buffers behind tokens
// ---------------------------------------------------------------------------

#[cfg(feature = "stack-smoltcp")]
mod smoltcp_device {
    use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
    use smoltcp::time::Instant as SmolInstant;

    use super::*;

    pub struct EthRxToken<'a, const N: usize> {
        pub(super) rx: &'a mut RxRing<N>,
        pub(super) len: usize,
    }

    impl<const N: usize> RxToken for EthRxToken<'_, N> {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            let r = f(self.rx.frame(self.len));
            self.rx.recycle();
            r
        }
    }

    pub struct EthTxToken<'a, const N: usize> {
        pub(super) tx: &'a mut TxRing<N>,
    }

    impl<const N: usize> TxToken for EthTxToken<'_, N> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            self.tx.send_with(len, f)
        }
    }

    impl<const TX: usize, const RX: usize> Device for Ethernet<TX, RX> {
        type RxToken<'a> = EthRxToken<'a, RX>;
        type TxToken<'a> = EthTxToken<'a, TX>;

        fn receive(&mut self, _now: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            self.poll_link();

            // smoltcp wants both tokens at once, so that a reply can be built from the
            // received frame without allocating. If TX has no room the frame simply
            // stays in the ring — nothing is dropped, so this is not counted.
            let len = self.rx.peek()?;
            if !self.tx.available() {
                return None;
            }

            Some((EthRxToken { rx: &mut self.rx, len }, EthTxToken { tx: &mut self.tx }))
        }

        fn transmit(&mut self, _now: SmolInstant) -> Option<Self::TxToken<'_>> {
            if !self.tx.available() {
                // The stack has something to send and cannot: same drop as xarxa's
                // `Err(buf)` path, counted the same way.
                stats::TX_FULL.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Some(EthTxToken { tx: &mut self.tx })
        }

        fn capabilities(&self) -> DeviceCapabilities {
            let mut caps = DeviceCapabilities::default();
            caps.medium = Medium::Ethernet;
            caps.max_transmission_unit = MTU;
            // smoltcp uses this to clamp the TCP window it advertises to
            // `max_burst_size * MSS`, so that a peer cannot push more into the RX ring
            // than it holds. The ring here is sized to a whole window (see
            // `bench::ETH_RING`), so the clamp does not bind and smoltcp advertises the
            // window its socket buffer actually has — the same one xarxa advertises,
            // which has no such mechanism.
            caps.max_burst_size = Some(TX);
            // Checksums stay in software, exactly as on the xarxa side.
            caps
        }
    }
}

// ---------------------------------------------------------------------------
// lwIP: refcounted pbufs
// ---------------------------------------------------------------------------

/// Everything lwIP needs from a driver is here: the netif init callback that describes
/// the interface, the transmit callback, and a receive pump the poll loop calls.
///
/// All three take the driver as a raw pointer rather than `&mut self`, and none of them
/// keeps a borrow of it alive across a call into lwIP. They have to: lwIP calls straight
/// back into the driver from inside `netif->input` (an arriving segment can be answered
/// with an ACK before `input` returns), so a `&mut Ethernet` held across that call would
/// alias the one `linkoutput` takes.
#[cfg(feature = "stack-lwip")]
impl<const TX: usize, const RX: usize> Ethernet<TX, RX> {
    /// `netif_init_fn`: fill in what lwIP wants to know about the interface.
    ///
    /// # Safety
    /// `netif` must be the netif being added, with its `state` pointing at the driver.
    pub unsafe extern "C" fn netif_init(netif: *mut lwip_sys::netif) -> lwip_sys::err_t {
        unsafe {
            let this = &*((*netif).state as *const Self);

            (*netif).name = [b'e' as _, b'n' as _];
            (*netif).hwaddr = this.mac_addr;
            (*netif).hwaddr_len = 6;
            // The IP MTU, i.e. the frame less its ethernet header — the same 1500 the
            // other two stacks are told, and what lwIP derives the IPv6 MSS from.
            (*netif).mtu = (MTU - 14) as u16;
            (*netif).mtu6 = (MTU - 14) as u16;
            (*netif).flags =
                (lwip_sys::NETIF_FLAG_BROADCAST | lwip_sys::NETIF_FLAG_ETHARP | lwip_sys::NETIF_FLAG_ETHERNET) as u8;
            (*netif).output = Some(lwip_sys::etharp_output);
            (*netif).output_ip6 = Some(lwip_sys::ethip6_output);
            (*netif).linkoutput = Some(Self::linkoutput);
        }
        lwip_sys::err_enum_t_ERR_OK as lwip_sys::err_t
    }

    /// `netif_linkoutput_fn`: put one frame on the wire.
    ///
    /// # Safety
    /// As `netif_init`, and `p` must be the chain lwIP is sending.
    unsafe extern "C" fn linkoutput(netif: *mut lwip_sys::netif, p: *mut lwip_sys::pbuf) -> lwip_sys::err_t {
        // SAFETY: no other borrow of the driver is alive here — see the impl's docs.
        let this = unsafe { &mut *((*netif).state as *mut Self) };

        if unsafe { (*p).tot_len } as usize > MTU {
            defmt::warn!("eth: dropping oversized tx frame of {} bytes", unsafe { (*p).tot_len });
            return lwip_sys::err_enum_t_ERR_IF as lwip_sys::err_t;
        }

        if !this.tx.submit_chain(p) {
            // Ring full: the hardware still owns those descriptors. lwIP drops the frame
            // (and TCP retransmits it), which is what the other two stacks do as well.
            stats::TX_FULL.fetch_add(1, Ordering::Relaxed);
            return lwip_sys::err_enum_t_ERR_MEM as lwip_sys::err_t;
        }
        lwip_sys::err_enum_t_ERR_OK as lwip_sys::err_t
    }

    /// Hand lwIP the frames the DMA has finished with — at most one ring's worth, which
    /// is everything that can have been waiting when the poll started. Call once per
    /// poll.
    ///
    /// The bound is not an optimisation, it is what makes the poll loop a loop. On this
    /// API the application's work happens *inside* ingress (the receive callback counts
    /// the payload and frees the frame there and then), so on a saturated link an
    /// unbounded drain would keep finding another frame and never return — the receive
    /// window never closes to slow the peer down, because the data is consumed as it
    /// arrives. The other two stacks stop by themselves: their ingress only fills a
    /// socket buffer, which the loop below has to be reached to drain.
    ///
    /// # Safety
    /// `this` must point at a live driver, and `netif` at the netif it was added as.
    pub unsafe fn lwip_input(this: *mut Self, netif: *mut lwip_sys::netif) {
        unsafe {
            (*this).poll_link();
            // Each statement takes its own short-lived borrow of the driver, so that none
            // is alive while lwIP runs.
            for _ in 0..RX {
                let Some(len) = (*this).rx.peek() else { break };
                let p = (*this).rx.take_pbuf(len);
                if p.is_null() {
                    break;
                }
                let input = (*netif).input.unwrap_unchecked();
                if input(p, netif) != lwip_sys::err_enum_t_ERR_OK as lwip_sys::err_t {
                    // Not handled (an unknown ethertype, or a full queue somewhere):
                    // freeing it is the caller's job, and is what re-arms the descriptor.
                    lwip_sys::pbuf_free(p);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PHY registers (IEEE 802.3 clause 22)
// ---------------------------------------------------------------------------

const PHY_REG_BCR: u8 = 0x00;
const PHY_REG_BSR: u8 = 0x01;
const PHY_REG_ANTX: u8 = 0x04;

const PHY_REG_BCR_ANRST: u16 = 1 << 9;
const PHY_REG_BCR_AN: u16 = 1 << 12;
const PHY_REG_BCR_RESET: u16 = 1 << 15;

const PHY_REG_ANTX_100BTX_FD: u16 = 0b1 << 8;
const PHY_REG_ANTX_TECH: u16 = 0b11111 << 5;

const PHY_REG_BSR_UP: u16 = 1 << 2;
const PHY_REG_BSR_ANDONE: u16 = 1 << 5;
