// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use epoll;
use libc::EAGAIN;
use std::cmp;
#[cfg(not(test))]
use std::io::Read;
use std::io::{self, Write};
use std::mem;
use std::net::Ipv4Addr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::result;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::vec::Vec;

use super::{
    ActivateError, ActivateResult, EpollHandlerPayload, Queue, VirtioDevice, TYPE_NET,
    VIRTIO_MMIO_INT_VRING,
};
use dumbo::ns::MmdsNetworkStack;
use logger::{Metric, METRICS};
use memory_model::{GuestAddress, GuestMemory};
use net_gen;
use net_util::{MacAddr, Tap, TapError, MAC_ADDR_LEN};
use rate_limiter::{RateLimiter, TokenType};
use sys_util::EventFd;
use virtio_gen::virtio_config::*;
use virtio_gen::virtio_net::*;
use {DeviceEventT, EpollHandler};

/// The maximum buffer size when segmentation offload is enabled. This
/// includes the 12-byte virtio net header.
/// http://docs.oasis-open.org/virtio/virtio/v1.0/virtio-v1.0.html#x1-1740003
const MAX_BUFFER_SIZE: usize = 65562;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE; NUM_QUEUES];

// A frame is available for reading from the tap device to receive in the guest.
const RX_TAP_EVENT: DeviceEventT = 0;
// The guest has made a buffer available to receive a frame into.
const RX_QUEUE_EVENT: DeviceEventT = 1;
// The transmit queue has a frame that is ready to send from the guest.
const TX_QUEUE_EVENT: DeviceEventT = 2;
// rx rate limiter budget is now available.
const RX_RATE_LIMITER_EVENT: DeviceEventT = 3;
// tx rate limiter budget is now available.
const TX_RATE_LIMITER_EVENT: DeviceEventT = 4;
// Number of DeviceEventT events supported by this implementation.
pub const NET_EVENTS_COUNT: usize = 5;

#[derive(Debug)]
pub enum Error {
    /// Open tap device failed.
    TapOpen(TapError),
    /// Setting tap IP failed.
    TapSetIp(TapError),
    /// Setting tap netmask failed.
    TapSetNetmask(TapError),
    /// Setting tap interface offload flags failed.
    TapSetOffload(TapError),
    /// Setting vnet header size failed.
    TapSetVnetHdrSize(TapError),
    /// Enabling tap interface failed.
    TapEnable(TapError),
}

pub type Result<T> = result::Result<T, Error>;

struct TxVirtio {
    queue_evt: EventFd,
    rate_limiter: RateLimiter,
    queue: Queue,
    iovec: Vec<(GuestAddress, usize)>,
    used_desc_heads: [u16; QUEUE_SIZE as usize],
    frame_buf: [u8; MAX_BUFFER_SIZE],
}

impl TxVirtio {
    fn new(queue: Queue, queue_evt: EventFd, rate_limiter: RateLimiter) -> Self {
        let tx_queue_max_size = queue.get_max_size() as usize;
        TxVirtio {
            queue_evt,
            rate_limiter,
            queue,
            iovec: Vec::with_capacity(tx_queue_max_size),
            used_desc_heads: [0u16; QUEUE_SIZE as usize],
            frame_buf: [0u8; MAX_BUFFER_SIZE],
        }
    }
}

struct RxVirtio {
    queue_evt: EventFd,
    rate_limiter: RateLimiter,
    deferred_frame: bool,
    deferred_irqs: bool,
    queue: Queue,
    bytes_read: usize,
    frame_buf: [u8; MAX_BUFFER_SIZE],
}

impl RxVirtio {
    fn new(queue: Queue, queue_evt: EventFd, rate_limiter: RateLimiter) -> Self {
        RxVirtio {
            queue_evt,
            rate_limiter,
            deferred_frame: false,
            deferred_irqs: false,
            queue,
            bytes_read: 0,
            frame_buf: [0u8; MAX_BUFFER_SIZE],
        }
    }
}

fn vnet_hdr_len() -> usize {
    mem::size_of::<virtio_net_hdr_v1>()
}

// Frames being sent/received through the network device model have a VNET header. This
// function returns a slice which holds the L2 frame bytes without this header.
fn frame_bytes_from_buf(buf: &[u8]) -> &[u8] {
    &buf[vnet_hdr_len()..]
}

fn frame_bytes_from_buf_mut(buf: &mut [u8]) -> &mut [u8] {
    &mut buf[vnet_hdr_len()..]
}

// This initializes to all 0 the VNET hdr part of a buf.
fn init_vnet_hdr(buf: &mut [u8]) {
    // The buffer should be larger than vnet_hdr_len.
    // TODO: any better way to set all these bytes to 0? Or is this optimized by the compiler?
    for i in 0..vnet_hdr_len() {
        buf[i] = 0;
    }
}

struct NetEpollHandler {
    rx: RxVirtio,
    tap: Tap,
    mem: GuestMemory,
    tx: TxVirtio,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    // TODO(smbarber): http://crbug.com/753630
    // Remove once MRG_RXBUF is supported and this variable is actually used.
    #[allow(dead_code)]
    acked_features: u64,
    mmds_ns: Option<MmdsNetworkStack>,

    #[cfg(test)]
    test_mutators: tests::TestMutators,
}

impl NetEpollHandler {
    fn signal_used_queue(&self) {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING as usize, Ordering::SeqCst);
        if let Err(e) = self.interrupt_evt.write(1) {
            error!("Failed to signal used queue: {:?}", e);
            METRICS.net.event_fails.inc();
        }
    }

    // Attempts to copy a single frame into the guest if there is enough
    // rate limiting budget.
    // Returns true on successful frame delivery.
    fn rate_limited_rx_single_frame(&mut self) -> bool {
        // If limiter.consume() fails it means there is no more TokenType::Ops
        // budget and rate limiting is in effect.
        if !self.rx.rate_limiter.consume(1, TokenType::Ops) {
            return false;
        }
        // If limiter.consume() fails it means there is no more TokenType::Bytes
        // budget and rate limiting is in effect.
        if !self
            .rx
            .rate_limiter
            .consume(self.rx.bytes_read as u64, TokenType::Bytes)
        {
            // revert the OPS consume()
            self.rx.rate_limiter.manual_replenish(1, TokenType::Ops);
            return false;
        }

        // Attempt frame delivery.
        let success = self.rx_single_frame();

        // Undo the tokens consumption if guest delivery failed.
        if !success {
            // revert the OPS consume()
            self.rx.rate_limiter.manual_replenish(1, TokenType::Ops);
            // revert the BYTES consume()
            self.rx
                .rate_limiter
                .manual_replenish(self.rx.bytes_read as u64, TokenType::Bytes);
        }
        return success;
    }

    // Copies a single frame from `self.rx.frame_buf` into the guest. Returns true
    // if a buffer was used, and false if the frame must be deferred until a buffer
    // is made available by the driver.
    fn rx_single_frame(&mut self) -> bool {
        let mut next_desc = self.rx.queue.iter(&self.mem).next();

        if next_desc.is_none() {
            return false;
        }

        // We just checked that the head descriptor exists.
        let head_index = next_desc.as_ref().unwrap().index;
        let mut write_count = 0;

        // Copy from frame into buffer, which may span multiple descriptors.
        loop {
            match next_desc {
                Some(desc) => {
                    if !desc.is_write_only() {
                        break;
                    }
                    let limit = cmp::min(write_count + desc.len as usize, self.rx.bytes_read);
                    let source_slice = &self.rx.frame_buf[write_count..limit];
                    let write_result = self.mem.write_slice_at_addr(source_slice, desc.addr);

                    match write_result {
                        Ok(sz) => {
                            write_count += sz;
                        }
                        Err(e) => {
                            error!("Failed to write slice: {:?}", e);
                            METRICS.net.rx_fails.inc();
                            break;
                        }
                    };

                    if write_count >= self.rx.bytes_read {
                        break;
                    }
                    next_desc = desc.next_descriptor();
                }
                None => {
                    warn!("Receiving buffer is too small to hold frame of current size");
                    METRICS.net.rx_fails.inc();
                    break;
                }
            }
        }

        self.rx
            .queue
            .add_used(&self.mem, head_index, write_count as u32);

        // Mark that we have at least one pending packet and we need to interrupt the guest.
        self.rx.deferred_irqs = true;

        if write_count >= self.rx.bytes_read {
            METRICS.net.rx_bytes_count.add(write_count);
            METRICS.net.rx_packets_count.inc();
            return true;
        } else {
            return false;
        }
    }

    // Tries to detour the frame to MMDS and if MMDS doesn't accept it, sends it on the host TAP.
    //
    // `frame_buf` should contain the frame bytes in a slice of exact length.
    // Returns whether MMDS consumed the frame.
    fn write_to_mmds_or_tap(
        mmds_ns: Option<&mut MmdsNetworkStack>,
        rate_limiter: &mut RateLimiter,
        frame_buf: &[u8],
        tap: &mut Tap,
    ) -> bool {
        if let Some(ns) = mmds_ns {
            if ns.detour_frame(frame_bytes_from_buf(frame_buf)) {
                METRICS.mmds.rx_accepted.inc();

                // MMDS frames are not accounted by the rate limiter.
                rate_limiter.manual_replenish(frame_buf.len() as u64, TokenType::Bytes);
                rate_limiter.manual_replenish(1, TokenType::Ops);

                // MMDS consumed the frame.
                return true;
            }
        }
        // This frame goes to the TAP.
        let write_result = tap.write(frame_buf);
        match write_result {
            Ok(_) => {
                METRICS.net.tx_bytes_count.add(frame_buf.len());
                METRICS.net.tx_packets_count.inc();
            }
            Err(e) => {
                error!("Failed to write to tap: {:?}", e);
                METRICS.net.tx_fails.inc();
            }
        };
        false
    }

    // We currently prioritize packets from the MMDS over regular network packets.
    fn read_from_mmds_or_tap(&mut self) -> io::Result<usize> {
        if let Some(ns) = self.mmds_ns.as_mut() {
            if let Some(len) = ns.write_next_frame(frame_bytes_from_buf_mut(&mut self.rx.frame_buf))
            {
                let len = len.get();
                METRICS.mmds.tx_frames.inc();
                METRICS.mmds.tx_bytes.add(len);
                init_vnet_hdr(&mut self.rx.frame_buf);
                return Ok(vnet_hdr_len() + len);
            }
        }
        self.read_tap()
    }

    fn process_rx(&mut self) {
        // Read as many frames as possible.
        loop {
            match self.read_from_mmds_or_tap() {
                Ok(count) => {
                    self.rx.bytes_read = count;
                    if !self.rate_limited_rx_single_frame() {
                        self.rx.deferred_frame = true;
                        break;
                    }
                }
                Err(e) => {
                    // The tap device is non-blocking, so any error aside from EAGAIN is
                    // unexpected.
                    match e.raw_os_error() {
                        Some(err) if err == EAGAIN => (),
                        _ => {
                            error!("Failed to read tap: {:?}", e);
                            METRICS.net.rx_fails.inc();
                        }
                    };
                    break;
                }
            }
        }
        if self.rx.deferred_irqs {
            self.rx.deferred_irqs = false;
            self.signal_used_queue();
        }
    }

    fn resume_rx(&mut self) {
        if self.rx.deferred_frame {
            if self.rate_limited_rx_single_frame() {
                self.rx.deferred_frame = false;
                // process_rx() was interrupted possibly before consuming all
                // packets in the tap; try continuing now.
                self.process_rx();
            } else if self.rx.deferred_irqs {
                self.rx.deferred_irqs = false;
                self.signal_used_queue();
            }
        }
    }

    fn process_tx(&mut self) {
        let mut rate_limited = false;
        let mut used_count = 0;

        // The MMDS network stack works like a state machine, based on synchronous calls, and
        // without being added to any event loop. If any frame is accepted by the MMDS, we also
        // trigger a process_rx() which checks if there are any new frames to be sent, starting
        // with the MMDS network stack.
        let mut process_rx_for_mmds = false;

        for avail_desc in self.tx.queue.iter(&self.mem) {
            // If limiter.consume() fails it means there is no more TokenType::Ops
            // budget and rate limiting is in effect.
            if !self.tx.rate_limiter.consume(1, TokenType::Ops) {
                rate_limited = true;
                // Stop processing the queue.
                break;
            }

            let head_index = avail_desc.index;
            let mut read_count = 0;
            let mut next_desc = Some(avail_desc);

            self.tx.iovec.clear();
            loop {
                match next_desc {
                    Some(desc) => {
                        if desc.is_write_only() {
                            break;
                        }
                        self.tx.iovec.push((desc.addr, desc.len as usize));
                        read_count += desc.len as usize;
                        next_desc = desc.next_descriptor();
                    }
                    None => {
                        break;
                    }
                }
            }

            // If limiter.consume() fails it means there is no more TokenType::Bytes
            // budget and rate limiting is in effect.
            if !self
                .tx
                .rate_limiter
                .consume(read_count as u64, TokenType::Bytes)
            {
                rate_limited = true;
                // revert the OPS consume()
                self.tx.rate_limiter.manual_replenish(1, TokenType::Ops);
                // stop processing the queue
                break;
            }

            read_count = 0;
            // Copy buffer from across multiple descriptors.
            // TODO(performance - Issue #420): change this to use `writev()` instead of `write()`
            // and get rid of the intermediate buffer.
            for (desc_addr, desc_len) in self.tx.iovec.drain(..) {
                let limit = cmp::min((read_count + desc_len) as usize, self.tx.frame_buf.len());

                let read_result = self.mem.read_slice_at_addr(
                    &mut self.tx.frame_buf[read_count..limit as usize],
                    desc_addr,
                );
                match read_result {
                    Ok(sz) => {
                        read_count += sz;
                    }
                    Err(e) => {
                        error!("Failed to read slice: {:?}", e);
                        METRICS.net.tx_fails.inc();
                        break;
                    }
                }
            }

            if Self::write_to_mmds_or_tap(
                self.mmds_ns.as_mut(),
                &mut self.tx.rate_limiter,
                &mut self.tx.frame_buf[..read_count],
                &mut self.tap,
            ) && !self.rx.deferred_frame
            {
                // MMDS consumed this frame/request, let's also try to process the response.
                process_rx_for_mmds = true;
            }

            self.tx.used_desc_heads[used_count] = head_index;
            used_count += 1;
        }
        if rate_limited {
            // If rate limiting kicked in, queue had advanced one element that we aborted
            // processing; go back one element so it can be processed next time.
            self.tx.queue.go_to_previous_position();
        }

        if used_count != 0 {
            // TODO(performance - Issue #425): find a way around RUST mutability enforcements to
            // allow calling queue.add_used() inside the loop. This would lead to better distribution
            // of descriptor usage between the firecracker thread and the guest tx thread.
            // One option to do this is to call queue.add_used() from a static function.
            for &desc_index in &self.tx.used_desc_heads[..used_count] {
                self.tx.queue.add_used(&self.mem, desc_index, 0);
            }
        }

        // An incoming frame for the MMDS may trigger the transmission of a new message.
        if process_rx_for_mmds {
            self.process_rx();
        }
    }

    #[cfg(not(test))]
    fn read_tap(&mut self) -> io::Result<usize> {
        self.tap.read(&mut self.rx.frame_buf)
    }
}

impl EpollHandler for NetEpollHandler {
    fn handle_event(&mut self, device_event: DeviceEventT, _: u32, _: EpollHandlerPayload) {
        match device_event {
            RX_TAP_EVENT => {
                METRICS.net.rx_tap_event_count.inc();

                // While limiter is blocked, don't process any more incoming.
                if self.rx.rate_limiter.is_blocked() {
                    return;
                }
                // Process a deferred frame first if available. Don't read from tap again
                // until we manage to receive this deferred frame.
                if self.rx.deferred_frame {
                    if self.rate_limited_rx_single_frame() {
                        self.rx.deferred_frame = false;
                    } else {
                        if self.rx.deferred_irqs {
                            self.rx.deferred_irqs = false;
                            self.signal_used_queue();
                        }
                        return;
                    }
                }
                self.process_rx();
            }
            RX_QUEUE_EVENT => {
                METRICS.net.rx_queue_event_count.inc();
                if let Err(e) = self.rx.queue_evt.read() {
                    error!("Failed to get rx queue event: {:?}", e);
                    METRICS.net.event_fails.inc();
                    // Shouldn't we return here?
                }
                // If the limiter is not blocked, resume the receiving of bytes.
                if !self.rx.rate_limiter.is_blocked() {
                    // There should be a buffer available now to receive the frame into.
                    self.resume_rx();
                }
            }
            TX_QUEUE_EVENT => {
                METRICS.net.tx_queue_event_count.inc();
                if let Err(e) = self.tx.queue_evt.read() {
                    error!("Failed to get tx queue event: {:?}", e);
                    // Shouldn't we return here?
                    METRICS.net.event_fails.inc();
                }
                // If the limiter is not blocked, continue transmitting bytes.
                if !self.tx.rate_limiter.is_blocked() {
                    self.process_tx();
                }
            }
            RX_RATE_LIMITER_EVENT => {
                METRICS.net.rx_event_rate_limiter_count.inc();
                // Upon rate limiter event, call the rate limiter handler
                // and restart processing the queue.
                match self.rx.rate_limiter.event_handler() {
                    Ok(_) => {
                        // There might be enough budget now to receive the frame.
                        self.resume_rx();
                    }
                    Err(e) => {
                        METRICS.net.event_fails.inc();
                        error!("Failed to get rx rate-limiter event: {:?}", e)
                    }
                }
            }
            TX_RATE_LIMITER_EVENT => {
                METRICS.net.tx_rate_limiter_event_count.inc();
                // Upon rate limiter event, call the rate limiter handler
                // and restart processing the queue.
                match self.tx.rate_limiter.event_handler() {
                    Ok(_) => {
                        // There might be enough budget now to send the frame.
                        self.process_tx();
                    }
                    Err(e) => {
                        METRICS.net.event_fails.inc();
                        error!("Failed to get tx rate-limiter event: {:?}", e)
                    }
                }
            }
            _ => panic!("Unknown event type was received."),
        }
    }
}

pub struct EpollConfig {
    rx_tap_token: u64,
    rx_queue_token: u64,
    tx_queue_token: u64,
    rx_rate_limiter_token: u64,
    tx_rate_limiter_token: u64,
    epoll_raw_fd: RawFd,
    sender: mpsc::Sender<Box<EpollHandler>>,
}

impl EpollConfig {
    pub fn new(
        first_token: u64,
        epoll_raw_fd: RawFd,
        sender: mpsc::Sender<Box<EpollHandler>>,
    ) -> Self {
        EpollConfig {
            rx_tap_token: first_token + RX_TAP_EVENT as u64,
            rx_queue_token: first_token + RX_QUEUE_EVENT as u64,
            tx_queue_token: first_token + TX_QUEUE_EVENT as u64,
            rx_rate_limiter_token: first_token + RX_RATE_LIMITER_EVENT as u64,
            tx_rate_limiter_token: first_token + TX_RATE_LIMITER_EVENT as u64,
            epoll_raw_fd,
            sender,
        }
    }
}

pub struct Net {
    tap: Option<Tap>,
    avail_features: u64,
    acked_features: u64,
    // The config space will only consist of the MAC address specified by the user,
    // or nothing, if no such address if provided.
    config_space: Vec<u8>,
    epoll_config: EpollConfig,
    rx_rate_limiter: Option<RateLimiter>,
    tx_rate_limiter: Option<RateLimiter>,
    allow_mmds_requests: bool,
}

impl Net {
    /// Create a new virtio network device with the given TAP interface.
    pub fn new_with_tap(
        tap: Tap,
        guest_mac: Option<&MacAddr>,
        epoll_config: EpollConfig,
        rx_rate_limiter: Option<RateLimiter>,
        tx_rate_limiter: Option<RateLimiter>,
        allow_mmds_requests: bool,
    ) -> Result<Self> {
        // Set offload flags to match the virtio features below.
        tap.set_offload(
            net_gen::TUN_F_CSUM | net_gen::TUN_F_UFO | net_gen::TUN_F_TSO4 | net_gen::TUN_F_TSO6,
        ).map_err(Error::TapSetOffload)?;

        let vnet_hdr_size = vnet_hdr_len() as i32;
        tap.set_vnet_hdr_size(vnet_hdr_size)
            .map_err(Error::TapSetVnetHdrSize)?;

        let mut avail_features = 1 << VIRTIO_NET_F_GUEST_CSUM
            | 1 << VIRTIO_NET_F_CSUM
            | 1 << VIRTIO_NET_F_GUEST_TSO4
            | 1 << VIRTIO_NET_F_GUEST_UFO
            | 1 << VIRTIO_NET_F_HOST_TSO4
            | 1 << VIRTIO_NET_F_HOST_UFO
            | 1 << VIRTIO_F_VERSION_1;

        let mut config_space;
        if let Some(mac) = guest_mac {
            config_space = Vec::with_capacity(MAC_ADDR_LEN);
            // This is safe, because we know the capacity is large enough.
            unsafe { config_space.set_len(MAC_ADDR_LEN) }
            config_space[..].copy_from_slice(mac.get_bytes());
            // When this feature isn't available, the driver generates a random MAC address.
            // Otherwise, it should attempt to read the device MAC address from the config space.
            avail_features |= 1 << VIRTIO_NET_F_MAC;
        } else {
            config_space = Vec::new();
        }

        Ok(Net {
            tap: Some(tap),
            avail_features,
            acked_features: 0u64,
            config_space,
            epoll_config,
            rx_rate_limiter,
            tx_rate_limiter,
            allow_mmds_requests,
        })
    }

    /// Create a new virtio network device with the given IP address and
    /// netmask.
    pub fn new(
        ip_addr: Ipv4Addr,
        netmask: Ipv4Addr,
        guest_mac: Option<&MacAddr>,
        epoll_config: EpollConfig,
        rx_rate_limiter: Option<RateLimiter>,
        tx_rate_limiter: Option<RateLimiter>,
        allow_mmds_requests: bool,
    ) -> Result<Self> {
        let tap = Tap::new().map_err(Error::TapOpen)?;
        tap.set_ip_addr(ip_addr).map_err(Error::TapSetIp)?;
        tap.set_netmask(netmask).map_err(Error::TapSetNetmask)?;
        tap.enable().map_err(Error::TapEnable)?;

        Self::new_with_tap(
            tap,
            guest_mac,
            epoll_config,
            rx_rate_limiter,
            tx_rate_limiter,
            allow_mmds_requests,
        )
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            0 => self.avail_features as u32,
            1 => (self.avail_features >> 32) as u32,
            _ => {
                warn!("Received request for unknown features page: {}", page);
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => value as u64,
            1 => (value as u64) << 32,
            _ => {
                warn!("Cannot acknowledge unknown features page: {}", page);
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("Received acknowledge request for unknown feature: {:x}", v);
            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_len = self.config_space.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            METRICS.net.cfg_fails.inc();
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write(&self.config_space[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let data_len = data.len() as u64;
        let config_len = self.config_space.len() as u64;
        if offset + data_len > config_len {
            error!("Failed to write config space");
            METRICS.net.cfg_fails.inc();
            return;
        }
        let (_, right) = self.config_space.split_at_mut(offset as usize);
        right.copy_from_slice(&data[..]);
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt_evt: EventFd,
        status: Arc<AtomicUsize>,
        mut queues: Vec<Queue>,
        mut queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != NUM_QUEUES || queue_evts.len() != NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                NUM_QUEUES,
                queues.len()
            );
            METRICS.net.activate_fails.inc();

            return Err(ActivateError::BadActivate);
        }

        if let Some(tap) = self.tap.take() {
            let rx_queue = queues.remove(0);
            let tx_queue = queues.remove(0);
            let rx_queue_evt = queue_evts.remove(0);
            let tx_queue_evt = queue_evts.remove(0);
            let mut mmds_ns = None;
            if self.allow_mmds_requests {
                mmds_ns = Some(MmdsNetworkStack::new_with_defaults());
            }
            let handler = NetEpollHandler {
                rx: RxVirtio::new(
                    rx_queue,
                    rx_queue_evt,
                    self.rx_rate_limiter.take().unwrap_or_default(),
                ),
                tap,
                mem,
                tx: TxVirtio::new(
                    tx_queue,
                    tx_queue_evt,
                    self.tx_rate_limiter.take().unwrap_or_default(),
                ),
                interrupt_status: status,
                interrupt_evt,
                acked_features: self.acked_features,
                mmds_ns,

                #[cfg(test)]
                test_mutators: tests::TestMutators::default(),
            };

            let tap_raw_fd = handler.tap.as_raw_fd();
            let rx_queue_raw_fd = handler.rx.queue_evt.as_raw_fd();
            let tx_queue_raw_fd = handler.tx.queue_evt.as_raw_fd();

            let rx_rate_limiter_rawfd = handler.rx.rate_limiter.as_raw_fd();
            let tx_rate_limiter_rawfd = handler.tx.rate_limiter.as_raw_fd();

            //channel should be open and working
            self.epoll_config
                .sender
                .send(Box::new(handler))
                .expect("Failed to send through the channel");

            //TODO: barrier needed here maybe?

            epoll::ctl(
                self.epoll_config.epoll_raw_fd,
                epoll::EPOLL_CTL_ADD,
                tap_raw_fd,
                epoll::Event::new(epoll::EPOLLIN, self.epoll_config.rx_tap_token),
            ).map_err(|e| {
                METRICS.net.activate_fails.inc();
                ActivateError::EpollCtl(e)
            })?;

            epoll::ctl(
                self.epoll_config.epoll_raw_fd,
                epoll::EPOLL_CTL_ADD,
                rx_queue_raw_fd,
                epoll::Event::new(epoll::EPOLLIN, self.epoll_config.rx_queue_token),
            ).map_err(|e| {
                METRICS.net.activate_fails.inc();
                ActivateError::EpollCtl(e)
            })?;

            epoll::ctl(
                self.epoll_config.epoll_raw_fd,
                epoll::EPOLL_CTL_ADD,
                tx_queue_raw_fd,
                epoll::Event::new(epoll::EPOLLIN, self.epoll_config.tx_queue_token),
            ).map_err(|e| {
                METRICS.net.activate_fails.inc();
                ActivateError::EpollCtl(e)
            })?;

            if rx_rate_limiter_rawfd != -1 {
                epoll::ctl(
                    self.epoll_config.epoll_raw_fd,
                    epoll::EPOLL_CTL_ADD,
                    rx_rate_limiter_rawfd,
                    epoll::Event::new(epoll::EPOLLIN, self.epoll_config.rx_rate_limiter_token),
                ).map_err(ActivateError::EpollCtl)?;
            }

            if tx_rate_limiter_rawfd != -1 {
                epoll::ctl(
                    self.epoll_config.epoll_raw_fd,
                    epoll::EPOLL_CTL_ADD,
                    tx_rate_limiter_rawfd,
                    epoll::Event::new(epoll::EPOLLIN, self.epoll_config.tx_rate_limiter_token),
                ).map_err(ActivateError::EpollCtl)?;
            }

            return Ok(());
        }
        METRICS.net.activate_fails.inc();
        Err(ActivateError::BadActivate)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;
    use std::thread;
    use std::time::Duration;
    use std::u32;

    use libc;

    use super::*;
    use memory_model::GuestAddress;
    use virtio::queue::tests::*;

    use dumbo::pdu::{arp, ethernet};

    /// Will read $metric, run the code in $block, then assert metric has increased by $delta.
    macro_rules! check_metric_after_block {
        ($metric:expr, $delta:expr, $block:expr) => {{
            let before = $metric.count();
            $block;
            assert_eq!($metric.count(), before + $delta, "unexpected metric value");
        }};
    }

    pub struct TestMutators {
        pub tap_read_fail: bool,
    }

    impl Default for TestMutators {
        fn default() -> TestMutators {
            TestMutators {
                tap_read_fail: false,
            }
        }
    }

    struct DummyNet {
        net: Net,
        epoll_raw_fd: i32,
        _receiver: Receiver<Box<EpollHandler>>,
    }

    impl DummyNet {
        fn new(guest_mac: Option<&MacAddr>) -> Self {
            let epoll_raw_fd = epoll::create(true).unwrap();
            let (sender, _receiver) = mpsc::channel();
            let epoll_config = EpollConfig::new(0, epoll_raw_fd, sender);

            DummyNet {
                net: Net::new(
                    "192.168.249.1".parse().unwrap(),
                    "255.255.255.0".parse().unwrap(),
                    guest_mac,
                    epoll_config,
                    // rate limiters present but with _very high_ allowed rate
                    Some(
                        RateLimiter::new(
                            u64::max_value(),
                            None,
                            1000,
                            u64::max_value(),
                            None,
                            1000,
                        ).unwrap(),
                    ),
                    Some(
                        RateLimiter::new(
                            u64::max_value(),
                            None,
                            1000,
                            u64::max_value(),
                            None,
                            1000,
                        ).unwrap(),
                    ),
                    true,
                ).unwrap(),
                epoll_raw_fd,
                _receiver,
            }
        }

        fn net(&mut self) -> &mut Net {
            &mut self.net
        }
    }

    impl Drop for DummyNet {
        fn drop(&mut self) {
            unsafe { libc::close(self.epoll_raw_fd) };
        }
    }

    impl NetEpollHandler {
        fn get_rx_rate_limiter(&self) -> &RateLimiter {
            &self.rx.rate_limiter
        }

        fn get_tx_rate_limiter(&self) -> &RateLimiter {
            &self.tx.rate_limiter
        }

        // This needs to be public to be accessible from the non-cfg-test `impl NetEpollHandler`.
        pub fn read_tap(&mut self) -> io::Result<usize> {
            use std::cmp::min;

            let count = min(1234, self.rx.frame_buf.len());

            for i in 0..count {
                self.rx.frame_buf[i] = 5;
            }

            if self.test_mutators.tap_read_fail {
                Err(io::Error::new(io::ErrorKind::Other, "oh no!"))
            } else {
                Ok(count)
            }
        }

        fn rx_single_frame_no_irq_coalescing(&mut self) -> bool {
            let ret = self.rx_single_frame();
            if self.rx.deferred_irqs {
                self.rx.deferred_irqs = false;
                self.signal_used_queue();
            }
            ret
        }

        fn set_rx_rate_limiter(&mut self, rx_rate_limiter: RateLimiter) {
            self.rx.rate_limiter = rx_rate_limiter;
        }

        fn set_tx_rate_limiter(&mut self, tx_rate_limiter: RateLimiter) {
            self.tx.rate_limiter = tx_rate_limiter;
        }
    }

    fn activate_some_net(n: &mut Net, bad_qlen: bool, bad_evtlen: bool) -> ActivateResult {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let interrupt_evt = EventFd::new().unwrap();
        let status = Arc::new(AtomicUsize::new(0));

        let rxq = VirtQueue::new(GuestAddress(0), &mem, 16);
        let txq = VirtQueue::new(GuestAddress(0x1000), &mem, 16);

        assert!(rxq.end().0 < txq.start().0);

        let mut queues = vec![rxq.create_queue(), txq.create_queue()];
        let mut queue_evts = vec![EventFd::new().unwrap(), EventFd::new().unwrap()];

        if bad_qlen {
            queues.pop();
        }

        if bad_evtlen {
            queue_evts.pop();
        }

        n.activate(mem.clone(), interrupt_evt, status, queues, queue_evts)
    }

    fn default_test_netepollhandler<'a>(
        mem: &'a GuestMemory,
        test_mutators: TestMutators,
    ) -> (NetEpollHandler, VirtQueue<'a>, VirtQueue<'a>) {
        let mut dummy = DummyNet::new(None);
        let n = dummy.net();

        let rxq = VirtQueue::new(GuestAddress(0), &mem, 16);
        let txq = VirtQueue::new(GuestAddress(0x1000), &mem, 16);

        assert!(rxq.end().0 < txq.start().0);

        let rx_queue = rxq.create_queue();
        let tx_queue = txq.create_queue();
        let interrupt_status = Arc::new(AtomicUsize::new(0));
        let interrupt_evt = EventFd::new().unwrap();
        let rx_queue_evt = EventFd::new().unwrap();
        let tx_queue_evt = EventFd::new().unwrap();

        (
            NetEpollHandler {
                rx: RxVirtio::new(rx_queue, rx_queue_evt, RateLimiter::default()),
                tap: n.tap.take().unwrap(),
                mem: mem.clone(),
                tx: TxVirtio::new(tx_queue, tx_queue_evt, RateLimiter::default()),
                interrupt_status,
                interrupt_evt,
                acked_features: n.acked_features,
                mmds_ns: Some(MmdsNetworkStack::new_with_defaults()),
                test_mutators,
            },
            txq,
            rxq,
        )
    }

    #[test]
    fn test_vnet_helpers() {
        let mut frame_buf: [u8; MAX_BUFFER_SIZE] = [42u8; MAX_BUFFER_SIZE];

        let vnet_hdr_len_ = mem::size_of::<virtio_net_hdr_v1>();
        assert_eq!(vnet_hdr_len_, vnet_hdr_len());

        init_vnet_hdr(&mut frame_buf);
        let zero_vnet_hdr = vec![0u8; vnet_hdr_len_];
        assert_eq!(zero_vnet_hdr, &frame_buf[..vnet_hdr_len_]);

        let payload = vec![42u8; MAX_BUFFER_SIZE - vnet_hdr_len_];
        assert_eq!(payload, frame_bytes_from_buf(&frame_buf));

        {
            let payload = frame_bytes_from_buf_mut(&mut frame_buf);
            payload[0] = 15;
        }
        assert_eq!(frame_buf[vnet_hdr_len_], 15);
    }

    #[test]
    fn test_virtio_device() {
        let mac = MacAddr::parse_str("11:22:33:44:55:66").unwrap();
        let mut dummy = DummyNet::new(Some(&mac));
        let n = dummy.net();

        // Test `device_type()`.
        {
            assert_eq!(n.device_type(), TYPE_NET);
        }

        // Test `queue_max_sizes()`.
        {
            let x = n.queue_max_sizes();
            assert_eq!(x, QUEUE_SIZES);

            // power of 2?
            for &y in x {
                assert!(y > 0 && y & (y - 1) == 0);
            }
        }

        // Test `features()` and `ack_features()`.
        {
            let features = 1 << VIRTIO_NET_F_GUEST_CSUM
                | 1 << VIRTIO_NET_F_CSUM
                | 1 << VIRTIO_NET_F_GUEST_TSO4
                | 1 << VIRTIO_NET_F_MAC
                | 1 << VIRTIO_NET_F_GUEST_UFO
                | 1 << VIRTIO_NET_F_HOST_TSO4
                | 1 << VIRTIO_NET_F_HOST_UFO
                | 1 << VIRTIO_F_VERSION_1;

            assert_eq!(n.features(0), features as u32);
            assert_eq!(n.features(1), (features >> 32) as u32);
            for i in 2..10 {
                assert_eq!(n.features(i), 0u32);
            }

            for i in 0..10 {
                n.ack_features(i, u32::MAX);
            }

            assert_eq!(n.acked_features, features);
        }

        // Test `read_config()`. This also validates the MAC was properly configured.
        {
            let mut config_mac = [0u8; MAC_ADDR_LEN];
            n.read_config(0, &mut config_mac);
            assert_eq!(config_mac, mac.get_bytes());

            // Invalid read.
            config_mac = [0u8; MAC_ADDR_LEN];
            check_metric_after_block!(
                &METRICS.net.cfg_fails,
                1,
                n.read_config(MAC_ADDR_LEN as u64 + 1, &mut config_mac)
            );
            assert_eq!(config_mac, [0u8, 0u8, 0u8, 0u8, 0u8, 0u8]);
        }

        // Let's test the activate function.
        {
            // It should fail when not enough queues and/or evts are provided.
            check_metric_after_block!(
                &METRICS.net.activate_fails,
                1,
                assert!(match activate_some_net(n, true, false) {
                    Err(ActivateError::BadActivate) => true,
                    _ => false,
                })
            );

            check_metric_after_block!(
                &METRICS.net.activate_fails,
                1,
                assert!(match activate_some_net(n, false, true) {
                    Err(ActivateError::BadActivate) => true,
                    _ => false,
                })
            );

            check_metric_after_block!(
                &METRICS.net.activate_fails,
                1,
                assert!(match activate_some_net(n, true, true) {
                    Err(ActivateError::BadActivate) => true,
                    _ => false,
                })
            );

            // Otherwise, it should be ok.
            check_metric_after_block!(
                &METRICS.net.activate_fails,
                0,
                assert!(activate_some_net(n, false, false).is_ok())
            );

            // Second activate shouldn't be ok anymore.
            check_metric_after_block!(
                &METRICS.net.activate_fails,
                1,
                assert!(match activate_some_net(n, false, false) {
                    Err(ActivateError::BadActivate) => true,
                    _ => false,
                })
            );
        }

        // Test writing another config.
        {
            let new_config: [u8; 6] = [0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
            n.write_config(0, &new_config);
            let mut new_config_read = [0u8; 6];
            n.read_config(0, &mut new_config_read);
            assert_eq!(new_config, new_config_read);

            // Invalid write.
            check_metric_after_block!(&METRICS.net.cfg_fails, 1, n.write_config(5, &new_config));
            // Verify old config was untouched.
            new_config_read = [0u8; 6];
            n.read_config(0, &mut new_config_read);
            assert_eq!(new_config, new_config_read);
        }
    }

    #[test]
    fn test_error_tap_set_ip() {
        let epoll_raw_fd = epoll::create(true).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let epoll_config = EpollConfig::new(0, epoll_raw_fd, sender);

        match Net::new(
            "255.255.255.255".parse().unwrap(),
            "0.0.0.0".parse().unwrap(),
            None,
            epoll_config,
            None,
            None,
            false,
        ) {
            Err(Error::TapSetIp(_)) => (),
            _ => assert!(false),
        };
    }

    #[test]
    fn test_error_tap_set_netmask() {
        let epoll_raw_fd = epoll::create(true).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let epoll_config = EpollConfig::new(0, epoll_raw_fd, sender);

        match Net::new(
            "0.0.0.0".parse().unwrap(),
            "0.0.0.255".parse().unwrap(),
            None,
            epoll_config,
            None,
            None,
            false,
        ) {
            Err(Error::TapSetNetmask(_)) => (),
            _ => assert!(false),
        };
    }

    #[test]
    fn test_mmds_detour_and_injection() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let (mut h, _txq, _rxq) = default_test_netepollhandler(&mem, TestMutators::default());

        let sha = MacAddr::parse_str("11:11:11:11:11:11").unwrap();
        let spa = Ipv4Addr::new(10, 1, 2, 3);
        let tha = MacAddr::parse_str("22:22:22:22:22:22").unwrap();
        let tpa = Ipv4Addr::new(169, 254, 169, 254);

        let packet_len;
        {
            // Create an ethernet frame.
            let eth_frame_i = ethernet::EthernetFrame::write_incomplete(
                frame_bytes_from_buf_mut(&mut h.tx.frame_buf),
                tha,
                sha,
                ethernet::ETHERTYPE_ARP,
            ).ok()
            .unwrap();
            // Set its length to hold an ARP request.
            let mut eth_frame_complete =
                eth_frame_i.with_payload_len_unchecked(arp::ETH_IPV4_FRAME_LEN);

            // Save the total frame length.
            packet_len =
                vnet_hdr_len() + eth_frame_complete.payload_offset() + arp::ETH_IPV4_FRAME_LEN;

            // Create the ARP request.
            let arp_req = arp::EthIPv4ArpFrame::write_request(
                eth_frame_complete.payload_mut(),
                sha,
                spa,
                tha,
                tpa,
            );
            // Validate success.
            assert!(arp_req.is_ok());
        }

        // Call the code which sends the packet to the host or MMDS.
        // Validate the frame was consumed by MMDS and that the metrics reflect that.
        check_metric_after_block!(
            &METRICS.mmds.rx_accepted,
            1,
            assert!(NetEpollHandler::write_to_mmds_or_tap(
                h.mmds_ns.as_mut(),
                &mut h.tx.rate_limiter,
                &h.tx.frame_buf[..packet_len],
                &mut h.tap,
            ))
        );

        // Validate that MMDS has a response and we can retrieve it.
        check_metric_after_block!(
            &METRICS.mmds.tx_frames,
            1,
            h.read_from_mmds_or_tap().unwrap()
        );
    }

    #[test]
    fn test_handler_error_cases() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let (mut h, _txq, _rxq) = default_test_netepollhandler(&mem, TestMutators::default());

        // RX rate limiter events should error since the limiter is not blocked.
        // Validate that the event failed and failure was properly accounted for.
        check_metric_after_block!(
            &METRICS.net.event_fails,
            1,
            h.handle_event(RX_RATE_LIMITER_EVENT, 0, EpollHandlerPayload::Empty)
        );

        // TX rate limiter events should error since the limiter is not blocked.
        // Validate that the event failed and failure was properly accounted for.
        check_metric_after_block!(
            &METRICS.net.event_fails,
            1,
            h.handle_event(TX_RATE_LIMITER_EVENT, 0, EpollHandlerPayload::Empty)
        );
    }

    #[test]
    #[should_panic]
    fn test_invalid_event_handler() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let (mut h, _txq, _rxq) = default_test_netepollhandler(&mem, TestMutators::default());
        // This should panic because the event is invalid.
        h.handle_event(
            NET_EVENTS_COUNT as DeviceEventT,
            0,
            EpollHandlerPayload::Empty,
        );
    }

    #[test]
    fn test_handler() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let (mut h, txq, rxq) = default_test_netepollhandler(&mem, TestMutators::default());

        let daddr = 0x2000;
        assert!(daddr as usize > txq.end().0);

        // Some corner cases for rx_single_frame().
        {
            assert_eq!(h.rx.bytes_read, 0);

            // Let's imagine we received some data.
            h.rx.bytes_read = MAX_BUFFER_SIZE;

            {
                // a read only descriptor
                rxq.avail.ring[0].set(0);
                rxq.avail.idx.set(1);
                rxq.dtable[0].set(daddr, 0x1000, 0, 0);
                assert!(!h.rx_single_frame_no_irq_coalescing());
                assert_eq!(rxq.used.idx.get(), 1);

                // resetting values
                rxq.used.idx.set(0);
                h.rx.queue = rxq.create_queue();
                h.interrupt_evt.write(1).unwrap();
                // The prev rx_single_frame_no_irq_coalescing() call should have written one more.
                assert_eq!(h.interrupt_evt.read(), Ok(2));
            }

            {
                // We make the prev desc write_only (with no other flag) to get a chain which is
                // writable, but too short.
                rxq.dtable[0].flags.set(VIRTQ_DESC_F_WRITE);
                check_metric_after_block!(
                    &METRICS.net.rx_fails,
                    1,
                    assert!(!h.rx_single_frame_no_irq_coalescing())
                );
                assert_eq!(rxq.used.idx.get(), 1);

                rxq.used.idx.set(0);
                h.rx.queue = rxq.create_queue();
                h.interrupt_evt.write(1).unwrap();
                assert_eq!(h.interrupt_evt.read(), Ok(2));
            }

            // set rx_count back to 0
            h.rx.bytes_read = 0;
        }

        // Now let's move on to the actual device events.

        {
            // testing TX_QUEUE_EVENT
            txq.avail.idx.set(1);
            txq.avail.ring[0].set(0);
            txq.dtable[0].set(daddr, 0x1000, 0, 0);

            h.tx.queue_evt.write(1).unwrap();
            h.handle_event(TX_QUEUE_EVENT, 0, EpollHandlerPayload::Empty);
            // Make sure the data queue advanced.
            assert_eq!(txq.used.idx.get(), 1);
        }

        {
            // testing RX_TAP_EVENT

            assert!(!h.rx.deferred_frame);

            // this should work just fine
            rxq.avail.idx.set(1);
            rxq.avail.ring[0].set(0);
            rxq.dtable[0].set(daddr, 0x1000, VIRTQ_DESC_F_WRITE, 0);

            h.interrupt_evt.write(1).unwrap();
            h.handle_event(RX_TAP_EVENT, 0, EpollHandlerPayload::Empty);
            assert!(h.rx.deferred_frame);
            assert_eq!(h.interrupt_evt.read(), Ok(2));
            // The #cfg(test) enabled version of read_tap always returns 1234 bytes (or the len of
            // the buffer, whichever is smaller).
            assert_eq!(rxq.used.ring[0].get().len, 1234);

            // Since deferred_frame is now true, activating the same event again will trigger
            // a different execution path.

            // reset some parts of the queue first
            h.rx.queue = rxq.create_queue();
            rxq.used.idx.set(0);

            // this should also be successful
            h.interrupt_evt.write(1).unwrap();
            h.handle_event(RX_TAP_EVENT, 0, EpollHandlerPayload::Empty);
            assert!(h.rx.deferred_frame);
            assert_eq!(h.interrupt_evt.read(), Ok(2));

            // ... but the following shouldn't, because we emulate receiving much more data than
            // we can fit inside a single descriptor

            h.rx.bytes_read = MAX_BUFFER_SIZE;
            h.rx.queue = rxq.create_queue();
            rxq.used.idx.set(0);

            h.interrupt_evt.write(1).unwrap();
            check_metric_after_block!(
                &METRICS.net.rx_fails,
                1,
                h.handle_event(RX_TAP_EVENT, 0, EpollHandlerPayload::Empty)
            );
            assert!(h.rx.deferred_frame);
            assert_eq!(h.interrupt_evt.read(), Ok(2));

            // A mismatch shows the reception was unsuccessful.
            assert_ne!(rxq.used.ring[0].get().len as usize, h.rx.bytes_read);

            // We set this back to a manageable size, for the following test.
            h.rx.bytes_read = 1234;
        }

        {
            // now also try an RX_QUEUE_EVENT
            rxq.avail.idx.set(2);
            rxq.avail.ring[1].set(1);
            rxq.dtable[1].set(daddr + 0x1000, 0x1000, VIRTQ_DESC_F_WRITE, 0);

            h.rx.queue_evt.write(1).unwrap();
            h.interrupt_evt.write(1).unwrap();
            h.handle_event(RX_QUEUE_EVENT, 0, EpollHandlerPayload::Empty);
            assert_eq!(h.interrupt_evt.read(), Ok(2));
        }

        {
            let test_mutators = TestMutators {
                tap_read_fail: true,
            };
            let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
            let (mut h, _txq, _rxq) = default_test_netepollhandler(&mem, test_mutators);

            check_metric_after_block!(&METRICS.net.rx_fails, 1, h.process_rx());
        }
    }

    #[test]
    fn test_bandwidth_rate_limiter() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let (mut h, txq, rxq) = default_test_netepollhandler(&mem, TestMutators::default());

        let daddr = 0x2000;
        assert!(daddr as usize > txq.end().0);

        // Test TX bandwidth rate limiting
        {
            // create bandwidth rate limiter that allows 40960 bytes/s with bucket size 4096 bytes
            let mut rl = RateLimiter::new(0x1000, None, 100, 0, None, 0).unwrap();
            // use up the budget
            assert!(rl.consume(0x1000, TokenType::Bytes));

            // set this tx rate limiter to be used
            h.set_tx_rate_limiter(rl);

            // try doing TX
            txq.avail.idx.set(1);
            txq.avail.ring[0].set(0);
            txq.dtable[0].set(daddr, 0x1000, 0, 0);

            // following TX procedure should fail because of bandwidth rate limiting
            {
                // trigger the TX handler
                h.tx.queue_evt.write(1).unwrap();
                h.handle_event(TX_QUEUE_EVENT, 0, EpollHandlerPayload::Empty);

                // assert that limiter is blocked
                assert!(h.get_tx_rate_limiter().is_blocked());
                // make sure the data is still queued for processing
                assert_eq!(txq.used.idx.get(), 0);
            }

            // wait for 100ms to give the rate-limiter timer a chance to replenish
            // wait for an extra 50ms to make sure the timerfd event makes its way from the kernel
            thread::sleep(Duration::from_millis(150));

            // following TX procedure should succeed because bandwidth should now be available
            {
                h.handle_event(TX_RATE_LIMITER_EVENT, 0, EpollHandlerPayload::Empty);
                // validate the rate_limiter is no longer blocked
                assert!(!h.get_tx_rate_limiter().is_blocked());
                // make sure the data queue advanced
                assert_eq!(txq.used.idx.get(), 1);
            }
        }

        // Test RX bandwidth rate limiting
        {
            // create bandwidth rate limiter that allows 40960 bytes/s with bucket size 4096 bytes
            let mut rl = RateLimiter::new(0x1000, None, 100, 0, None, 0).unwrap();
            // use up the budget
            assert!(rl.consume(0x1000, TokenType::Bytes));

            // set this rx rate limiter to be used
            h.set_rx_rate_limiter(rl);

            // set up RX
            assert!(!h.rx.deferred_frame);
            rxq.avail.idx.set(1);
            rxq.avail.ring[0].set(0);
            rxq.dtable[0].set(daddr, 0x1000, VIRTQ_DESC_F_WRITE, 0);

            // following RX procedure should fail because of bandwidth rate limiting
            {
                // leave at least one event here so that reading it later won't block
                h.interrupt_evt.write(1).unwrap();
                // trigger the RX handler
                h.handle_event(RX_TAP_EVENT, 0, EpollHandlerPayload::Empty);

                // assert that limiter is blocked
                assert!(h.get_rx_rate_limiter().is_blocked());
                assert!(h.rx.deferred_frame);
                // assert that no operation actually completed (limiter blocked it)
                assert_eq!(h.interrupt_evt.read(), Ok(1));
                // make sure the data is still queued for processing
                assert_eq!(rxq.used.idx.get(), 0);
            }

            // wait for 100ms to give the rate-limiter timer a chance to replenish
            // wait for an extra 50ms to make sure the timerfd event makes its way from the kernel
            thread::sleep(Duration::from_millis(150));

            // following RX procedure should succeed because bandwidth should now be available
            {
                // leave at least one event here so that reading it later won't block
                h.interrupt_evt.write(1).unwrap();
                h.handle_event(RX_RATE_LIMITER_EVENT, 0, EpollHandlerPayload::Empty);
                // validate the rate_limiter is no longer blocked
                assert!(!h.get_rx_rate_limiter().is_blocked());
                // make sure the virtio queue operation completed this time
                assert_eq!(h.interrupt_evt.read(), Ok(2));
                // make sure the data queue advanced
                assert_eq!(rxq.used.idx.get(), 1);
                // The #cfg(test) enabled version of read_tap always returns 1234 bytes
                // (or the len of the buffer, whichever is smaller).
                assert_eq!(rxq.used.ring[0].get().len, 1234);
            }
        }
    }

    #[test]
    fn test_ops_rate_limiter() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let (mut h, txq, rxq) = default_test_netepollhandler(&mem, TestMutators::default());

        let daddr = 0x2000;
        assert!(daddr as usize > txq.end().0);

        // Test TX ops rate limiting
        {
            // create ops rate limiter that allows 10 ops/s with bucket size 1 ops
            let mut rl = RateLimiter::new(0, None, 0, 1, None, 100).unwrap();
            // use up the budget
            assert!(rl.consume(1, TokenType::Ops));

            // set this tx rate limiter to be used
            h.set_tx_rate_limiter(rl);

            // try doing TX
            txq.avail.idx.set(1);
            txq.avail.ring[0].set(0);
            txq.dtable[0].set(daddr, 0x1000, 0, 0);

            // following TX procedure should fail because of ops rate limiting
            {
                // trigger the TX handler
                h.tx.queue_evt.write(1).unwrap();
                h.handle_event(TX_QUEUE_EVENT, 0, EpollHandlerPayload::Empty);

                // assert that limiter is blocked
                assert!(h.get_tx_rate_limiter().is_blocked());
                // make sure the data is still queued for processing
                assert_eq!(txq.used.idx.get(), 0);
            }

            // wait for 100ms to give the rate-limiter timer a chance to replenish
            // wait for an extra 50ms to make sure the timerfd event makes its way from the kernel
            thread::sleep(Duration::from_millis(150));

            // following TX procedure should succeed because ops should now be available
            {
                h.handle_event(TX_RATE_LIMITER_EVENT, 0, EpollHandlerPayload::Empty);
                // validate the rate_limiter is no longer blocked
                assert!(!h.get_tx_rate_limiter().is_blocked());
                // make sure the data queue advanced
                assert_eq!(txq.used.idx.get(), 1);
            }
        }

        // Test RX ops rate limiting
        {
            // create ops rate limiter that allows 10 ops/s with bucket size 1 ops
            let mut rl = RateLimiter::new(0, None, 0, 1, None, 100).unwrap();
            // use up the budget
            assert!(rl.consume(0x800, TokenType::Ops));

            // set this rx rate limiter to be used
            h.set_rx_rate_limiter(rl);

            // set up RX
            assert!(!h.rx.deferred_frame);
            rxq.avail.idx.set(1);
            rxq.avail.ring[0].set(0);
            rxq.dtable[0].set(daddr, 0x1000, VIRTQ_DESC_F_WRITE, 0);

            // following RX procedure should fail because of ops rate limiting
            {
                // leave at least one event here so that reading it later won't block
                h.interrupt_evt.write(1).unwrap();
                // trigger the RX handler
                h.handle_event(RX_TAP_EVENT, 0, EpollHandlerPayload::Empty);

                // assert that limiter is blocked
                assert!(h.get_rx_rate_limiter().is_blocked());
                assert!(h.rx.deferred_frame);
                // assert that no operation actually completed (limiter blocked it)
                assert_eq!(h.interrupt_evt.read(), Ok(1));
                // make sure the data is still queued for processing
                assert_eq!(rxq.used.idx.get(), 0);

                // leave at least one event here so that reading it later won't block
                h.interrupt_evt.write(1).unwrap();
                // trigger the RX handler again, this time it should do the limiter fast path exit
                h.handle_event(RX_TAP_EVENT, 0, EpollHandlerPayload::Empty);
                // assert that no operation actually completed, that the limiter blocked it
                assert_eq!(h.interrupt_evt.read(), Ok(1));
                // make sure the data is still queued for processing
                assert_eq!(rxq.used.idx.get(), 0);
            }

            // wait for 100ms to give the rate-limiter timer a chance to replenish
            // wait for an extra 50ms to make sure the timerfd event makes its way from the kernel
            thread::sleep(Duration::from_millis(150));

            // following RX procedure should succeed because ops should now be available
            {
                // leave at least one event here so that reading it later won't block
                h.interrupt_evt.write(1).unwrap();
                h.handle_event(RX_RATE_LIMITER_EVENT, 0, EpollHandlerPayload::Empty);
                // make sure the virtio queue operation completed this time
                assert_eq!(h.interrupt_evt.read(), Ok(2));
                // make sure the data queue advanced
                assert_eq!(rxq.used.idx.get(), 1);
                // The #cfg(test) enabled version of read_tap always returns 1234 bytes
                // (or the len of the buffer, whichever is smaller).
                assert_eq!(rxq.used.ring[0].get().len, 1234);
            }
        }
    }
}
