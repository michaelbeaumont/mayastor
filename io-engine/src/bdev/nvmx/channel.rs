/* I/O channel for NVMe controller, one per core. */

use std::{
    cmp::max,
    mem::size_of,
    os::raw::c_void,
    ptr::NonNull,
    time::Duration,
};

use nix::errno::Errno;

use spdk_rs::{
    cpu_cores::Cores,
    libspdk::{
        nvme_qpair_abort_all_queued_reqs,
        nvme_transport_qpair_abort_reqs,
        spdk_io_channel,
        spdk_nvme_ctrlr_alloc_io_qpair,
        spdk_nvme_ctrlr_connect_io_qpair,
        spdk_nvme_ctrlr_connect_io_qpair_async,
        spdk_nvme_ctrlr_disconnect_io_qpair,
        spdk_nvme_ctrlr_free_io_qpair,
        spdk_nvme_ctrlr_get_default_io_qpair_opts,
        spdk_nvme_ctrlr_io_qpair_connect_poll_async,
        spdk_nvme_io_qpair_connect_ctx,
        spdk_nvme_io_qpair_opts,
        spdk_nvme_poll_group,
        spdk_nvme_poll_group_add,
        spdk_nvme_poll_group_create,
        spdk_nvme_poll_group_destroy,
        spdk_nvme_poll_group_process_completions,
        spdk_nvme_poll_group_remove,
        spdk_nvme_qpair,
        spdk_put_io_channel,
    },
    Poller,
    PollerBuilder,
};

use crate::{
    bdev::{
        device_lookup,
        nvmx::{
            controller_inner::SpdkNvmeController,
            nvme_bdev_running_config,
            IoQpairConnectionStatus,
            NvmeControllerState,
            NVME_CONTROLLERS,
        },
    },
    core::{BlockDevice, BlockDeviceIoStats, CoreError, IoType},
    ffihelper::ErrnoResult,
};

use futures::channel::oneshot;

#[repr(C)]
pub struct NvmeIoChannel<'a> {
    inner: *mut NvmeIoChannelInner<'a>,
}

impl<'a> NvmeIoChannel<'a> {
    #[inline]
    fn from_raw(p: *mut c_void) -> &'a mut NvmeIoChannel<'a> {
        unsafe { &mut *(p as *mut NvmeIoChannel) }
    }

    #[inline]
    fn inner_mut(&mut self) -> &'a mut NvmeIoChannelInner<'a> {
        unsafe { &mut *self.inner }
    }

    #[inline]
    pub fn inner_from_channel(
        io_channel: *mut spdk_io_channel,
    ) -> &'a mut NvmeIoChannelInner<'a> {
        NvmeIoChannel::from_raw(Self::io_channel_ctx(io_channel)).inner_mut()
    }

    #[inline]
    fn io_channel_ctx(ch: *mut spdk_io_channel) -> *mut c_void {
        unsafe {
            (ch as *mut u8).add(size_of::<spdk_io_channel>()) as *mut c_void
        }
    }
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, PartialOrd)]
pub enum QPairState {
    Disconnected,
    Disconnecting,
    Connecting,
    Connected,
    Enabling,
    Enabled,
    Destroying,
}

impl From<u8> for QPairState {
    fn from(u: u8) -> Self {
        match u {
            0 => Self::Disconnected,
            1 => Self::Disconnecting,
            2 => Self::Connecting,
            3 => Self::Connected,
            4 => Self::Enabling,
            5 => Self::Enabled,
            6 => Self::Destroying,
            _ => panic!("qpair in a unknown state"),
        }
    }
}

impl ToString for QPairState {
    fn to_string(&self) -> String {
        match *self {
            QPairState::Disconnected => "Disconnected",
            QPairState::Disconnecting => "Disconnecting",
            QPairState::Connecting => "Connecting",
            QPairState::Connected => "Connected",
            QPairState::Enabling => "Enabling",
            QPairState::Enabled => "Enabled",
            QPairState::Destroying => "Destroying",
        }
        .parse()
        .unwrap()
    }
}

#[derive(Debug)]
pub struct IoQpair<'poller> {
    qpair: NonNull<spdk_nvme_qpair>,
    ctrlr_handle: SpdkNvmeController,
    state: QPairState,
    connect_waiters: Vec<oneshot::Sender<Result<(), CoreError>>>,
    connect_arg: Option<NonNull<IoQpairConnectContext<'poller>>>,
}

impl IoQpair<'_> {
    fn get_default_options(
        ctrlr_handle: SpdkNvmeController,
    ) -> spdk_nvme_io_qpair_opts {
        let mut opts = spdk_nvme_io_qpair_opts::default();
        let default_opts = nvme_bdev_running_config();

        unsafe {
            spdk_nvme_ctrlr_get_default_io_qpair_opts(
                ctrlr_handle.as_ptr(),
                &mut opts,
                size_of::<spdk_nvme_io_qpair_opts>() as u64,
            )
        };

        opts.io_queue_requests =
            max(opts.io_queue_requests, default_opts.io_queue_requests);
        opts.create_only = true;

        // Always assume async_mode is enabled instread of
        // relying on default_opts.async_mode.
        opts.async_mode = true;

        opts
    }

    /// Create a qpair with default options for target NVMe controller.
    fn create(
        ctrlr_handle: SpdkNvmeController,
        ctrlr_name: &str,
    ) -> Result<Self, CoreError> {
        //assert!(!ctrlr_handle.is_null(), "controller handle is null");

        let qpair_opts = IoQpair::get_default_options(ctrlr_handle);

        let qpair: *mut spdk_nvme_qpair = unsafe {
            spdk_nvme_ctrlr_alloc_io_qpair(
                ctrlr_handle.as_ptr(),
                &qpair_opts,
                size_of::<spdk_nvme_io_qpair_opts>() as u64,
            )
        };

        if let Some(q) = NonNull::new(qpair) {
            debug!(?qpair, ?ctrlr_name, "qpair created for controller");
            Ok(Self {
                qpair: q,
                ctrlr_handle,
                state: QPairState::Disconnected,
                connect_waiters: Vec::new(),
                connect_arg: None,
            })
        } else {
            error!(?ctrlr_name, "Failed to allocate I/O qpair for controller",);
            Err(CoreError::GetIoChannel {
                name: ctrlr_name.to_string(),
            })
        }
    }

    /// Get SPDK qpair object.
    pub fn as_ptr(&self) -> *mut spdk_nvme_qpair {
        self.qpair.as_ptr()
    }

    /// TODO:
    pub(crate) fn try_connect(&mut self) -> IoQpairConnectionStatus {
        let r = self.connect();

        if r == 0 {
            IoQpairConnectionStatus::Complete {
                status: Ok(()),
            }
        } else if r == -libc::EAGAIN {
            let (sender, receiver) =
                oneshot::channel::<Result<(), CoreError>>();
            self.connect_waiters.push(sender);
            info!(
                ?self,
                 "Added connection waiter to handle synchronous connect readiness"
            );
            IoQpairConnectionStatus::Pending {
                channel: receiver,
            }
        } else {
            IoQpairConnectionStatus::Complete {
                status: Err(CoreError::GetIoChannel {
                    name: "xxx".to_string(),
                }),
            }
        }
    }

    /// Synchronously connect qpair.
    pub(crate) fn connect(&mut self) -> i32 {
        debug!(?self, "connecting I/O qpair");

        // Check if I/O qpair is already connected to provide idempotency for
        // multiple allocations of the same handle for the same thread, to make
        // sure we don't reconnect every time.
        if self.state == QPairState::Connected {
            return 0;
        }

        // During synchronous connection we shouldn't be preemped by any other
        // SPDK thread, however, there can be asynchronous connection
        // operation already in progress, being executed on this thread.
        // In such a case we proceed with connection synchronously: asynchrnous
        // operation will observe the result of synchronous connect.
        if self.state == QPairState::Connecting {
            info!(
                ?self,
                core=Cores::current(),
                "Asynchronous I/O qpair connection already in progress, bailing out"
            );
            return -libc::EAGAIN;
        } else {
            self.state = QPairState::Connecting;
        }

        // Mark qpair as being connected and try to connect.
        let status = unsafe {
            spdk_nvme_ctrlr_connect_io_qpair(
                self.ctrlr_handle.as_ptr(),
                self.qpair.as_ptr(),
            )
        };

        // Update QPairState according to the connection result.
        self.state = if status == 0 {
            QPairState::Connected
        } else {
            QPairState::Disconnected
        };

        debug!(?self, ?status, state=?self.state,"I/O qpair connected");

        status
    }

    /// Asynchronously connect qpair.
    pub(crate) async fn connect_async(&mut self) -> Result<(), CoreError> {
        // Check if I/O qpair is already connected to provide idempotency for
        // multiple allocations of the same handle for the same thread, to make
        // sure we don't reconnect every time.
        if self.state == QPairState::Connected {
            return Ok(());
        }

        debug!(?self, "Asynchronously connecting I/O qpair");
        // Take into account other connect requests for this I/O qpair to avoid
        // multiple concurrent connections.
        match self.state {
            QPairState::Disconnected => {
                self.state = QPairState::Connecting;
            }
            QPairState::Connecting => {
                let (sender, receiver) =
                    oneshot::channel::<Result<(), CoreError>>();

                self.connect_waiters.push(sender);

                let r = receiver
                    .await
                    .expect("I/O qpair connection sender disappeared");
                return r;
            }
            _ => {
                panic!("QPair is in insufficient state: {:?}", self.state);
            }
        }

        let (sender, receiver) = oneshot::channel::<ErrnoResult<()>>();

        let connect_arg = Box::into_raw(Box::new(IoQpairConnectContext {
            sender: Some(sender),
            poller: None,
            connect_ctx: None,
        }));

        let connect_ctx = unsafe {
            spdk_nvme_ctrlr_connect_io_qpair_async(
                self.ctrlr_handle.as_ptr(),
                self.qpair.as_ptr(),
                Some(qpair_connect_cb),
                connect_arg as *mut c_void,
            )
        };

        if connect_ctx.is_null() {
            error!(qpair=?self, "Failed to initiate asynchronous connection on a qpair");
            return Err(CoreError::OpenBdev {
                source: Errno::ENXIO,
            });
        }

        let qpair = self.qpair.as_ptr();

        let poller = PollerBuilder::new()
            .with_name("io_qpair_connect_poller")
            .with_interval(Duration::from_millis(1))
            .with_data(())
            .with_poll_fn(move |_| unsafe {
                let ctx = &mut *connect_arg;

                let st = spdk_nvme_ctrlr_io_qpair_connect_poll_async(
                    qpair,
                    connect_ctx,
                );

                match st {
                    // Connection complete, callback is called.
                    0 => 1,
                    // Connection still in progress, keep polling.
                    1 => 0,
                    // Error occured during polling.
                    errno => {
                        error!(?qpair, ?errno, "I/O qpair connection failed");

                        // Stop the poller and notify the listener.
                        ctx.poller.take();
                        ctx.sender
                            .take()
                            .expect("No qpair connection sender provided")
                            .send(Err(Errno::from_i32(errno)))
                            .expect("Failed to notify I/O qpair connection listener");
                        1
                    }
                }
            })
            .build();

        unsafe {
            (*connect_arg).poller = Some(poller);
            (*connect_arg).connect_ctx = Some(
                NonNull::new(connect_ctx)
                    .expect("I/O qpair async connection context is null"),
            )
        }

        self.connect_arg = Some(
            NonNull::new(connect_arg)
                .expect("I/O qpair async connection argument is null"),
        );

        let r = receiver
            .await
            .expect("I/O qpair connection sender disappeared")
            .map_err(|e| CoreError::OpenBdev {
                source: e,
            });

        let mut dump_info = false; // TODO: debug only.

        // Clear connect arg straight after connection complete.
        if self.connect_arg.take().is_some() {
            // Update QPairState according to the connection result.
            self.state = if r.is_ok() {
                QPairState::Connected
            } else {
                QPairState::Disconnected
            };
        } else {
            info!(
                ?self,
                core = Cores::current(),
                "Got woken up via synchronous connect !"
            );
            dump_info = true;
        }

        // Wake up all other callers waiting for connection to complete.
        if !self.connect_waiters.is_empty() {
            let waiters: Vec<oneshot::Sender<Result<(), CoreError>>> =
                self.connect_waiters.drain(..).collect();

            if dump_info {
                info!(
                    ?qpair,
                    waiters = waiters.len(),
                    "Notifying connection waiters"
                );
            }
            for w in waiters {
                if w.send(r.clone()).is_err() {
                    error!("Failed to notify a connection waiter");
                }
            }
            if dump_info {
                info!(?qpair, "All connection waiters are notified");
            }
        }

        // Drop context object transformed previously into a raw pointer.
        unsafe {
            Box::from_raw(connect_arg);
        }

        debug!(?self, state=?self.state,"I/O qpair connected asynchronously");
        r
    }
}

#[derive(Debug)]
struct IoQpairConnectContext<'poller> {
    sender: Option<oneshot::Sender<Result<(), Errno>>>,
    poller: Option<Poller<'poller>>,
    connect_ctx: Option<std::ptr::NonNull<spdk_nvme_io_qpair_connect_ctx>>,
}

extern "C" fn qpair_connect_cb(
    qpair: *mut spdk_nvme_qpair,
    cb_ctx: *mut c_void,
) {
    debug!(?qpair, "I/O qpair successfully connected");

    let connect_ctx =
        unsafe { &mut *(cb_ctx as *const _ as *mut IoQpairConnectContext) };

    // Stop the poller.
    connect_ctx.poller.take();

    // Notify the listener.
    connect_ctx
        .sender
        .take()
        .expect("No qpair connection sender provided")
        .send(Ok(()))
        .expect("Failed to notify I/O qpair connection listener");
}

struct PollGroup(NonNull<spdk_nvme_poll_group>);

impl PollGroup {
    /// Create a poll group.
    fn create(ctx: *mut c_void, ctrlr_name: &str) -> Result<Self, CoreError> {
        let poll_group: *mut spdk_nvme_poll_group =
            unsafe { spdk_nvme_poll_group_create(ctx, std::ptr::null_mut()) };

        if poll_group.is_null() {
            Err(CoreError::GetIoChannel {
                name: ctrlr_name.to_string(),
            })
        } else {
            Ok(Self(NonNull::new(poll_group).unwrap()))
        }
    }

    /// Add I/O qpair to poll group.
    fn add_qpair(&mut self, qpair: &IoQpair) -> i32 {
        unsafe { spdk_nvme_poll_group_add(self.0.as_ptr(), qpair.as_ptr()) }
    }

    /// Remove I/O qpair to poll group.
    fn remove_qpair(&mut self, qpair: &IoQpair) -> i32 {
        unsafe { spdk_nvme_poll_group_remove(self.0.as_ptr(), qpair.as_ptr()) }
    }

    /// Get SPDK handle for poll group.
    #[inline]
    fn as_ptr(&self) -> *mut spdk_nvme_poll_group {
        self.0.as_ptr()
    }
}

impl Drop for PollGroup {
    fn drop(&mut self) {
        debug!("dropping poll group {:p}", self.0.as_ptr());
        let rc = unsafe { spdk_nvme_poll_group_destroy(self.0.as_ptr()) };
        if rc < 0 {
            error!("Error on poll group destroy: {}", rc);
        }
        debug!("poll group {:p} successfully dropped", self.0.as_ptr());
    }
}

/// spdk_nvme_ctrlr_free_io_qpair() calls disconnected. So we can either
/// a. NOT call disconnect here
///     and have SPDK disconnect it.
/// b. set the ptr to null, as SPDK checks if the ptr is NULL. However, that
/// breaks    the contract with NonNull<T>
impl Drop for IoQpair<'_> {
    fn drop(&mut self) {
        let qpair = self.qpair.as_ptr();

        unsafe {
            nvme_qpair_abort_all_queued_reqs(qpair, 1);
            debug!(?qpair, "I/O requests successfully aborted,");
            nvme_transport_qpair_abort_reqs(qpair, 1);
            debug!(?qpair, "transport requests successfully aborted,");
            spdk_nvme_ctrlr_disconnect_io_qpair(qpair);
            debug!(?qpair, "qpair successfully disconnected,");
            spdk_nvme_ctrlr_free_io_qpair(qpair);
            debug!(?qpair, "qpair successfully freed,");
        }

        debug!(?qpair, "qpair successfully dropped,");
    }
}

impl std::fmt::Debug for NvmeIoChannelInner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvmeIoChannelInner")
            .field("qpair", &self.qpair)
            .field("pending IO", &self.num_pending_ios)
            .finish()
    }
}

pub struct NvmeIoChannelInner<'a> {
    pub qpair: Option<IoQpair<'a>>,
    poll_group: PollGroup,
    poller: Poller<'a>,
    io_stats_controller: IoStatsController,
    pub device: Box<dyn BlockDevice>,
    /// to prevent the controller from being destroyed before the channel
    ctrl: Option<
        std::sync::Arc<parking_lot::Mutex<crate::bdev::NvmeController<'a>>>,
    >,
    num_pending_ios: u64,

    // Flag to indicate the shutdown state of the channel.
    // We need such a flag to differentiate between channel reset and shutdown.
    // Channel reset is a reversible operation, which is followed by
    // reinitialize(), which 'resurrects' channel (i.e. recreates all its
    // I/O resources): such behaviour is observed during controller reset.
    // Shutdown, in contrary, means 'one-way' ticket for the channel, which
    // doesn't assume any further resurrections: such behaviour is seen
    // upon controller shutdown. Being able to differentiate between these
    // 2 states allows controller reset to behave properly in parallel with
    // shutdown (if case reset is initiated before shutdown), and
    // not to reinitialize channels already processed by shutdown logic.
    is_shutdown: bool,
}

impl NvmeIoChannelInner<'_> {
    /// Reset channel, making it unusable till reinitialize() is called.
    pub fn reset(&mut self) -> i32 {
        if self.qpair.is_some() {
            // Remove qpair and trigger its deallocation via drop().
            let qpair = self.qpair.take().unwrap();
            debug!(
                "dropping qpair {:p} ({}) I/O requests pending)",
                qpair.as_ptr(),
                self.num_pending_ios
            );
        }
        0
    }

    /// Checks whether the I/O channel is shutdown.
    pub fn is_shutdown(&self) -> bool {
        self.is_shutdown
    }

    /// Shutdown I/O channel and make it completely unusable for I/O.
    pub fn shutdown(&mut self) -> i32 {
        if self.is_shutdown {
            return 0;
        }

        let rc = self.reset();
        if rc == 0 {
            self.is_shutdown = true;
            self.ctrl.take();
        }
        rc
    }

    /// Account active I/O for channel.
    #[inline]
    pub fn account_io(&mut self) {
        self.num_pending_ios += 1;
    }

    /// Discard active I/O operation for channel.
    #[inline]
    pub fn discard_io(&mut self) {
        if self.num_pending_ios == 0 {
            warn!("Discarding I/O operation without any active I/O operations")
        } else {
            self.num_pending_ios -= 1;
        }
    }

    /// Reinitialize channel after reset unless the channel is shutdown.
    pub fn reinitialize(
        &mut self,
        ctrlr_name: &str,
        ctrlr_handle: SpdkNvmeController,
    ) -> i32 {
        if self.is_shutdown {
            error!(
                "{} I/O channel is shutdown, channel reinitialization not possible",
                ctrlr_name
            );
            return -libc::ENODEV;
        }

        // We assume that channel is reinitialized after being reset, so we
        // expect to see no I/O qpair.
        let prev = self.qpair.take();
        if prev.is_some() {
            warn!(
                ?ctrlr_name,
                "I/O channel has active I/O qpair while being reinitialized, clearing"
            );
        }

        // Create qpair for target controller.
        let mut qpair = match IoQpair::create(ctrlr_handle, ctrlr_name) {
            Ok(qpair) => qpair,
            Err(e) => {
                error!(?ctrlr_name, ?e, "Failed to allocate qpair,");
                return -libc::ENOMEM;
            }
        };

        // Add qpair to the poll group.
        let mut rc = self.poll_group.add_qpair(&qpair);
        if rc != 0 {
            error!(?ctrlr_name, "failed to add qpair to poll group");
            return rc;
        }

        // Connect qpair.
        rc = qpair.connect();
        if rc != 0 {
            error!("{} failed to connect qpair (errno={})", ctrlr_name, rc);
            self.poll_group.remove_qpair(&qpair);
            return rc;
        }

        debug!("{} I/O channel successfully reinitialized", ctrlr_name);
        self.qpair = Some(qpair);
        0
    }

    /// Get I/O statistics for channel.
    #[inline]
    pub fn get_io_stats_controller(&mut self) -> &mut IoStatsController {
        &mut self.io_stats_controller
    }
}
pub struct IoStatsController {
    // Note that for the sake of optimization, all bytes-related I/O stats
    // (bytes_read, bytes_written and bytes_unmapped) are accounted in
    // sectors. Translation into bytes occurs only when providing the full
    // I/O stats to the caller, inside get_io_stats().
    io_stats: BlockDeviceIoStats,
    block_size: u64,
}

/// Top-level wrapper around device I/O statistics.
impl IoStatsController {
    fn new(block_size: u64) -> Self {
        Self {
            io_stats: BlockDeviceIoStats::default(),
            block_size,
        }
    }

    #[inline]
    /// Account amount of blocks and I/O operations.
    pub fn account_block_io(
        &mut self,
        op: IoType,
        num_ops: u64,
        num_blocks: u64,
    ) {
        match op {
            IoType::Read => {
                self.io_stats.num_read_ops += num_ops;
                self.io_stats.bytes_read += num_blocks;
            }
            IoType::Write => {
                self.io_stats.num_write_ops += num_ops;
                self.io_stats.bytes_written += num_blocks;
            }
            IoType::Unmap => {
                self.io_stats.num_unmap_ops += num_ops;
                self.io_stats.bytes_unmapped += num_blocks;
            }
            IoType::WriteZeros => {}
            _ => {
                warn!("Unsupported I/O type for I/O statistics: {:?}", op);
            }
        }
    }

    /// Get I/O statistics for channel.
    #[inline]
    pub fn get_io_stats(&self) -> BlockDeviceIoStats {
        let mut stats = self.io_stats;

        // Translate sectors into bytes before returning the stats.
        stats.bytes_read *= self.block_size;
        stats.bytes_written *= self.block_size;
        stats.bytes_unmapped *= self.block_size;

        stats
    }
}

pub struct NvmeControllerIoChannel(NonNull<spdk_io_channel>);

extern "C" fn disconnected_qpair_cb(
    _qpair: *mut spdk_nvme_qpair,
    ctx: *mut c_void,
) {
    let inner = NvmeIoChannel::from_raw(ctx).inner_mut();

    if let Some(ref qpair) = inner.qpair {
        unsafe {
            nvme_qpair_abort_all_queued_reqs(qpair.as_ptr(), 1);
            nvme_transport_qpair_abort_reqs(qpair.as_ptr(), 1);
        }
    }

    //warn!(?qpair, "NVMe qpair disconnected");
    // shutdown the channel such that pending IO if any, gets aborted.
    //inner.shutdown();
}

extern "C" fn nvme_poll(ctx: *mut c_void) -> i32 {
    let inner = NvmeIoChannel::from_raw(ctx).inner_mut();

    let num_completions = unsafe {
        spdk_nvme_poll_group_process_completions(
            inner.poll_group.as_ptr(),
            0,
            Some(disconnected_qpair_cb),
        )
    };

    if num_completions > 0 {
        1
    } else {
        0
    }
}

impl NvmeControllerIoChannel {
    pub extern "C" fn create(device: *mut c_void, ctx: *mut c_void) -> i32 {
        let id = device as u64;

        debug!("Creating IO channel for controller ID 0x{:X}", id);

        let carc = match NVME_CONTROLLERS.lookup_by_name(id.to_string()) {
            None => {
                error!("No NVMe controller found for ID 0x{:X}", id);
                return 1;
            }
            Some(c) => c,
        };

        let (cname, controller, block_size) = {
            let controller = carc.lock();
            // Make sure controller is available.
            if controller.get_state() != NvmeControllerState::Running {
                error!(
                    "{} controller is in {:?} state, I/O channel creation not possible",
                    controller.get_name(),
                    controller.get_state()
                );
                return 1;
            }
            // Release controller's lock before proceeding to avoid deadlocks,
            // as qpair-related operations might hang in case of
            // network connection failures. Note that we still hold
            // the reference to the controller instance (carc) which
            // guarantees that the controller exists during I/O channel
            // creation.
            let block_size = controller
                .namespace()
                .expect("No namespaces in active controller")
                .block_len();
            (
                controller.get_name(),
                controller.controller().unwrap(),
                block_size,
            )
        };

        let nvme_channel = NvmeIoChannel::from_raw(ctx);

        // Get a block device that corresponds to the controller.
        let device = match device_lookup(&cname) {
            Some(device) => device,
            None => {
                error!(
                    "{} no block device exists for controller, I/O channel creation not possible",
                    cname,
                );
                return 1;
            }
        };

        // Allocate qpair.
        let qpair = match IoQpair::create(controller, &cname) {
            Ok(qpair) => qpair,
            Err(e) => {
                error!(?cname, ?e, "Failed to allocate qpair");
                return 1;
            }
        };
        debug!(?cname, "I/O qpair successfully created");

        // Create poll group.
        let mut poll_group = match PollGroup::create(ctx, &cname) {
            Ok(poll_group) => poll_group,
            Err(e) => {
                error!(?cname, ?e, "Failed to create a poll group");
                return 1;
            }
        };

        // Add qpair to poll group.
        let rc = poll_group.add_qpair(&qpair);
        if rc != 0 {
            error!(?cname, ?rc, "failed to add qpair to poll group");
            return 1;
        }

        // Create poller.
        let poller = PollerBuilder::new()
            .with_interval(Duration::from_micros(
                nvme_bdev_running_config().nvme_ioq_poll_period_us,
            ))
            .with_poll_fn(move |_| nvme_poll(ctx))
            .build();

        let inner = Box::new(NvmeIoChannelInner {
            qpair: Some(qpair),
            poll_group,
            poller,
            io_stats_controller: IoStatsController::new(block_size),
            is_shutdown: false,
            device,
            ctrl: Some(carc),
            num_pending_ios: 0,
        });

        nvme_channel.inner = Box::into_raw(inner);
        debug!(?cname, ?ctx, "I/O channel successfully initialized");
        0
    }

    /// Callback function to be invoked by SPDK to deinitialize I/O channel for
    /// NVMe controller.
    pub extern "C" fn destroy(device: *mut c_void, ctx: *mut c_void) {
        debug!(
            "Destroying IO channel for controller ID 0x{:X}",
            device as u64
        );

        {
            let ch = NvmeIoChannel::from_raw(ctx);
            let mut inner = unsafe { Box::from_raw(ch.inner) };

            // Stop the poller and do extra handling for I/O qpair, as it needs
            // to be detached from the poller prior poller
            // destruction.
            inner.poller.stop();

            if let Some(qpair) = inner.qpair.take() {
                inner.poll_group.remove_qpair(&qpair);
            }
        }

        debug!(
            "IO channel for controller ID 0x{:X} successfully destroyed",
            device as u64
        );
    }
}

/// Wrapper around SPDK I/O channel.
impl NvmeControllerIoChannel {
    pub fn from_null_checked(
        ch: *mut spdk_io_channel,
    ) -> Option<NvmeControllerIoChannel> {
        if ch.is_null() {
            None
        } else {
            Some(NvmeControllerIoChannel(NonNull::new(ch).unwrap()))
        }
    }

    pub fn as_ptr(&self) -> *mut spdk_io_channel {
        self.0.as_ptr()
    }
}

impl Drop for NvmeControllerIoChannel {
    fn drop(&mut self) {
        trace!("I/O channel {:p} dropped", self.0.as_ptr());
        unsafe { spdk_put_io_channel(self.0.as_ptr()) }
    }
}
